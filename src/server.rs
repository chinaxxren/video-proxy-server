use crate::data_source_manager::DataSourceManager;
use crate::hls::DefaultHlsHandler;
use crate::log_info;
use crate::request_handler::RequestHandler;
use crate::storage::StorageManagerConfig;
use crate::utils::error::{ProxyError, Result};
use crate::utils::network_policy::NetworkPolicy;
use hyper::service::{make_service_fn, service_fn};
use hyper::Server;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

/// 代理服务器的全部可调参数。
///
/// 这些值原先散在各模块里当私有常量：宿主想改缓存上限就得改源码重编译。
/// 集中到一处并从 [`ProxyServer::with_config`] 往下贯穿，宿主只需构造一个
/// 结构体。用 `..Default::default()` 只改关心的字段即可。
#[derive(Clone, Debug)]
pub struct ProxyConfig {
    /// 监听端口。只绑 127.0.0.1，不接受外部连接。
    pub port: u16,
    pub cache_dir: PathBuf,
    /// 缓存目录的字节上限，超出后按 LRU 淘汰。
    pub max_cache_bytes: u64,
    /// 缓存条目数上限，与字节上限同时生效（任一超出即触发淘汰）。
    pub max_file_count: usize,
    /// 两次淘汰检查之间的间隔。
    pub cleanup_interval: Duration,
    /// 并发处理的请求数上限，超出的请求排队等待而不是被拒绝。
    pub max_concurrent_requests: usize,
    /// 允许回源的主机白名单。**留空表示拒绝一切上游请求**——
    /// 默认拒绝而不是默认放行，是这一层 SSRF 防护的基本前提。
    pub allowed_hosts: Vec<String>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            port: 8080,
            cache_dir: PathBuf::from("cache"),
            max_cache_bytes: 1024 * 1024 * 1024, // 1GB
            max_file_count: 1000,
            cleanup_interval: Duration::from_secs(60),
            max_concurrent_requests: 64,
            allowed_hosts: Vec::new(),
        }
    }
}

pub struct ProxyServer {
    port: u16,
    handler: Arc<RequestHandler>,
    shutdown: Arc<Notify>,
}

impl ProxyServer {
    pub fn new(port: u16, cache_dir: &str) -> Self {
        Self::with_config(ProxyConfig {
            port,
            cache_dir: PathBuf::from(cache_dir),
            ..Default::default()
        })
    }

    pub fn with_allowed_hosts<I, S>(port: u16, cache_dir: &str, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::with_config(ProxyConfig {
            port,
            cache_dir: PathBuf::from(cache_dir),
            allowed_hosts: hosts
                .into_iter()
                .map(|host| host.as_ref().to_string())
                .collect(),
            ..Default::default()
        })
    }

    pub fn with_config(config: ProxyConfig) -> Self {
        let policy = Arc::new(NetworkPolicy::allow_hosts(&config.allowed_hosts));
        let cache_dir = config.cache_dir.clone();

        // 创建数据源管理器
        let source_manager = Arc::new(DataSourceManager::with_config(
            cache_dir.clone(),
            policy.clone(),
            StorageManagerConfig {
                max_cache_size: config.max_cache_bytes,
                max_file_count: config.max_file_count,
                cleanup_interval: config.cleanup_interval,
            },
        ));

        // 创建 HLS 处理器
        let hls_handler = Arc::new(DefaultHlsHandler::new(cache_dir, policy));

        // 创建请求处理器
        let handler = Arc::new(RequestHandler::with_limit(
            source_manager,
            hls_handler,
            config.max_concurrent_requests,
        ));

        Self {
            port: config.port,
            handler,
            shutdown: Arc::new(Notify::new()),
        }
    }

    /// 发送优雅停止信号。`start()` 会完成所有进行中的请求后关闭监听器。
    /// 可从任意线程安全调用；多次调用幂等。
    pub fn stop(&self) {
        self.shutdown.notify_one();
    }

    pub async fn start(&self) -> Result<()> {
        let addr = SocketAddr::from(([127, 0, 0, 1], self.port));

        let handler = self.handler.clone();
        let make_svc = make_service_fn(move |_conn| {
            let handler = handler.clone();
            async move {
                Ok::<_, Infallible>(service_fn(move |req| {
                    let handler = handler.clone();
                    async move {
                        match handler.handle_request(req).await {
                            Ok(response) => Ok::<_, Infallible>(response),
                            Err(e) => {
                                // 详细原因只进日志；响应体只给类别，避免回显
                                // 缓存路径、上游 URL 等内部信息。
                                log_info!("Server", "请求失败: {}", e);
                                let mut builder =
                                    hyper::Response::builder().status(e.status_code());
                                if matches!(e, ProxyError::MethodNotAllowed) {
                                    builder = builder.header(hyper::header::ALLOW, "GET");
                                }
                                Ok(builder
                                    .body(hyper::Body::from(e.public_message()))
                                    .unwrap_or_else(|_| {
                                        let mut fallback =
                                            hyper::Response::new(hyper::Body::empty());
                                        *fallback.status_mut() =
                                            hyper::StatusCode::INTERNAL_SERVER_ERROR;
                                        fallback
                                    }))
                            }
                        }
                    }
                }))
            }
        });

        // try_bind 而非 bind：后者在端口被占用时直接 panic，调用方
        // 无论怎么处理返回值都拦不住，进程会带着 panic 栈退出。
        let shutdown = self.shutdown.clone();
        let server = Server::try_bind(&addr)
            .map_err(|e| ProxyError::IO(format!("无法绑定 {}: {}", addr, e)))?
            .serve(make_svc)
            .with_graceful_shutdown(async move { shutdown.notified().await });
        log_info!("Server", "代理服务器正在运行在 http://{}", addr);

        // 错误必须往上传：原先只打一行 stderr 就 return Ok(())，
        // 调用方看到成功、进程静默退出，排查时毫无线索。
        server
            .await
            .map_err(|e| ProxyError::IO(format!("服务器异常终止: {}", e)))
    }
}

pub async fn run_server(port: u16, cache_dir: &str) -> Result<()> {
    let server = ProxyServer::new(port, cache_dir);
    server.start().await
}

/// 按完整配置启动并运行到停止。返回的 [`ProxyServer`] 句柄由调用方持有，
/// 需要停止时调用 [`ProxyServer::stop`]。
pub async fn run_server_with_config(config: ProxyConfig) -> Result<()> {
    ProxyServer::with_config(config).start().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[tokio::test]
    async fn occupied_port_returns_error_instead_of_panicking() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let cache = tempfile::tempdir().unwrap();
        let server = ProxyServer::new(port, cache.path().to_str().unwrap());

        let error = server.start().await.unwrap_err();
        assert!(matches!(error, ProxyError::IO(_)));
        drop(listener);
    }

    /// 借用一个当前空闲的端口号。绑定后立刻释放，理论上存在被别人抢占的
    /// 竞争窗口，但测试环境里足够稳定，而且比硬编码端口可靠得多。
    fn free_port() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    }

    /// 轮询到端口可连接为止。`start()` 是异步的，直接往下走会撞上
    /// 「bind 还没完成」的窗口——stop() 早于 bind 就会被 with_graceful_shutdown
    /// 吃掉，测试随机挂起。
    async fn wait_until_listening(port: u16) {
        for _ in 0..50 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("服务器未在预期时间内开始监听");
    }

    #[tokio::test]
    async fn stop_releases_the_port_and_returns_ok() {
        let port = free_port();
        let cache = tempfile::tempdir().unwrap();
        let server = Arc::new(ProxyServer::new(port, cache.path().to_str().unwrap()));

        let running = tokio::spawn({
            let server = server.clone();
            async move { server.start().await }
        });

        // 等监听器真正就绪，否则 stop() 可能早于 bind 完成。
        wait_until_listening(port).await;

        server.stop();

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), running)
            .await
            .expect("stop() 后 start() 未能在超时前返回")
            .expect("服务器任务 panic");
        assert!(result.is_ok(), "优雅停止应返回 Ok，实际: {:?}", result);

        // 关键断言：端口必须真的还给系统，否则固定端口重启会失败。
        TcpListener::bind(("127.0.0.1", port)).expect("stop() 之后端口仍被占用");
    }

    /// `ProxyConfig` 必须真的贯穿下去，而不是被内部常量悄悄覆盖。
    ///
    /// 用一次真实 HTTP 往返验证三件事：配置里的端口生效、请求走完了
    /// 完整链路（RequestHandler → DataSourceManager → NetworkPolicy）、
    /// 以及 `allowed_hosts` 留空时默认拒绝一切上游。
    #[tokio::test]
    async fn config_port_takes_effect_and_empty_allowlist_denies_upstream() {
        let port = free_port();
        let cache = tempfile::tempdir().unwrap();
        let server = Arc::new(ProxyServer::with_config(ProxyConfig {
            port,
            cache_dir: cache.path().to_path_buf(),
            max_cache_bytes: 4096,
            max_concurrent_requests: 4,
            // 留空 = 拒绝一切上游，这是默认拒绝的 SSRF 前提。
            allowed_hosts: Vec::new(),
            ..Default::default()
        }));

        let running = tokio::spawn({
            let server = server.clone();
            async move { server.start().await }
        });
        wait_until_listening(port).await;

        let client = hyper::Client::new();
        let request = hyper::Request::builder()
            .uri(format!("http://127.0.0.1:{}/proxy/media", port))
            .header("X-Original-Url", "https://example.com/video.mp4")
            .header("X-Cache-Asset-Id", "asset-1")
            .header("X-Cache-Asset-Revision", "1")
            .header(hyper::header::RANGE, "bytes=0-1023")
            .body(hyper::Body::empty())
            .unwrap();

        let response = tokio::time::timeout(std::time::Duration::from_secs(5), client.request(request))
            .await
            .expect("请求超时")
            .expect("请求失败");

        // 白名单为空 → 上游被拒。具体状态码由 NetworkPolicy 决定，
        // 关键是不能是 2xx：那意味着回源真的发生了。
        assert!(
            !response.status().is_success(),
            "白名单为空时不应成功回源，实际状态: {}",
            response.status()
        );

        let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        // 响应体不能回显上游 URL 或缓存路径。
        assert!(
            !text.contains("example.com") && !text.contains(cache.path().to_str().unwrap()),
            "响应体泄露了内部信息: {}",
            text
        );

        server.stop();
        tokio::time::timeout(std::time::Duration::from_secs(5), running)
            .await
            .expect("stop() 后未能退出")
            .expect("服务器任务 panic")
            .expect("优雅停止应返回 Ok");
    }

    #[tokio::test]
    async fn stop_before_start_is_not_lost() {
        let port = free_port();
        let cache = tempfile::tempdir().unwrap();
        let server = ProxyServer::new(port, cache.path().to_str().unwrap());

        // Notify 会记住一个许可，所以先发的停止信号不会丢。
        server.stop();

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), server.start())
            .await
            .expect("start() 应当立刻因已有停止信号而返回");
        assert!(result.is_ok());
    }
}
