use crate::data_source_manager::DataSourceManager;
use crate::handlers::BackgroundTasks;
use crate::hls::DefaultHlsHandler;
use crate::http_types::{empty_body, full_body, AppBody};
use crate::log_info;
use crate::request_handler::RequestHandler;
use crate::source_registry::SourceRegistry;
use crate::storage::StorageManagerConfig;
use crate::utils::error::{ProxyError, Result};

#[cfg(test)]
#[path = "../examples/support/mod.rs"]
mod test_http_client;
use crate::utils::network_policy::NetworkPolicy;
use hyper::server::conn::http1::Builder as ConnectionBuilder;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{watch, Notify};
use tokio::task::JoinSet;

const STATE_CREATED: u8 = 0;
const STATE_STARTING: u8 = 1;
const STATE_RUNNING: u8 = 2;
const STATE_STOPPING: u8 = 3;
const STATE_STOPPED: u8 = 4;
const STATE_FAILED: u8 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyServerStatus {
    Created,
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
}

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
    /// 收到停止信号后等待连接自然排空的最长时间。
    pub shutdown_timeout: Duration,
    /// 客户端必须在此时间内发送完整 HTTP 请求头，防止半开连接长期占用任务。
    pub request_header_timeout: Duration,
    /// 单个 HTTP 请求允许的最大头字段数量，超出时 Hyper 返回 431。
    pub max_request_headers: usize,
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
            shutdown_timeout: Duration::from_secs(5),
            request_header_timeout: Duration::from_secs(10),
            max_request_headers: 64,
            allowed_hosts: Vec::new(),
        }
    }
}

pub struct ProxyServer {
    port: u16,
    bound_port: AtomicU16,
    state: AtomicU8,
    handler: Arc<RequestHandler>,
    shutdown: Arc<Notify>,
    ready: watch::Sender<u8>,
    shutdown_timeout: Duration,
    request_header_timeout: Duration,
    max_request_headers: usize,
    background_tasks: Arc<BackgroundTasks>,
    source_registry: SourceRegistry,
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
        Self::with_config_and_registry(config, SourceRegistry::default())
    }

    pub fn with_config_and_registry(config: ProxyConfig, source_registry: SourceRegistry) -> Self {
        let policy = Arc::new(NetworkPolicy::allow_hosts(&config.allowed_hosts));
        let cache_dir = config.cache_dir.clone();

        // 创建数据源管理器
        let background_tasks = BackgroundTasks::new();
        let source_manager = Arc::new(DataSourceManager::with_tasks(
            cache_dir.clone(),
            policy.clone(),
            StorageManagerConfig {
                max_cache_size: config.max_cache_bytes,
                max_file_count: config.max_file_count,
                cleanup_interval: config.cleanup_interval,
            },
            background_tasks.clone(),
        ));

        // 创建 HLS 处理器
        let hls_handler = Arc::new(DefaultHlsHandler::new(
            cache_dir,
            policy,
            source_registry.clone(),
        ));

        // 创建请求处理器
        let handler = Arc::new(RequestHandler::with_limit(
            source_manager,
            hls_handler,
            config.max_concurrent_requests,
            source_registry.clone(),
        ));

        let (ready, _) = watch::channel(0);
        Self {
            port: config.port,
            bound_port: AtomicU16::new(0),
            state: AtomicU8::new(STATE_CREATED),
            handler,
            shutdown: Arc::new(Notify::new()),
            ready,
            shutdown_timeout: config.shutdown_timeout,
            request_header_timeout: config.request_header_timeout,
            max_request_headers: config.max_request_headers,
            background_tasks,
            source_registry,
        }
    }

    pub fn source_registry(&self) -> SourceRegistry {
        self.source_registry.clone()
    }

    /// 发送优雅停止信号。`start()` 会完成所有进行中的请求后关闭监听器。
    /// 可从任意线程安全调用；多次调用幂等。
    pub fn stop(&self) {
        let _ = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| match state {
                STATE_CREATED | STATE_STARTING | STATE_RUNNING => Some(STATE_STOPPING),
                _ => None,
            });
        self.shutdown.notify_one();
    }

    pub fn status(&self) -> ProxyServerStatus {
        match self.state.load(Ordering::Acquire) {
            STATE_STARTING => ProxyServerStatus::Starting,
            STATE_RUNNING => ProxyServerStatus::Running,
            STATE_STOPPING => ProxyServerStatus::Stopping,
            STATE_STOPPED => ProxyServerStatus::Stopped,
            STATE_FAILED => ProxyServerStatus::Failed,
            _ => ProxyServerStatus::Created,
        }
    }

    /// Returns the actual listening port after startup. This is especially
    /// useful when `ProxyConfig::port` is zero and the OS chooses a free port.
    pub fn bound_port(&self) -> Option<u16> {
        match self.bound_port.load(Ordering::Acquire) {
            0 => None,
            port => Some(port),
        }
    }

    /// Wait until the socket has been bound and return its actual port.
    pub async fn wait_until_ready(&self) -> Result<u16> {
        let mut ready = self.ready.subscribe();
        loop {
            if let Some(port) = self.bound_port() {
                return Ok(port);
            }
            if self.status() == ProxyServerStatus::Failed {
                return Err(ProxyError::IO("代理服务器启动失败".to_string()));
            }
            ready
                .changed()
                .await
                .map_err(|_| ProxyError::IO("代理服务器启动状态不可用".to_string()))?;
        }
    }

    pub async fn start(&self) -> Result<()> {
        match self.state.compare_exchange(
            STATE_CREATED,
            STATE_STARTING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(STATE_STOPPING) => {}
            Err(_) => return Err(ProxyError::Request("代理服务器不能重复启动".to_string())),
        }
        let addr = SocketAddr::from(([127, 0, 0, 1], self.port));

        let listener = match TcpListener::bind(addr).await {
            Ok(listener) => listener,
            Err(error) => {
                self.state.store(STATE_FAILED, Ordering::Release);
                self.ready.send_replace(1);
                return Err(ProxyError::IO(format!("无法绑定 {}: {}", addr, error)));
            }
        };
        let actual_addr = listener
            .local_addr()
            .map_err(|error| ProxyError::IO(format!("无法读取监听地址: {error}")))?;
        self.bound_port.store(actual_addr.port(), Ordering::Release);
        if self.state.load(Ordering::Acquire) != STATE_STOPPING {
            self.state.store(STATE_RUNNING, Ordering::Release);
        }
        self.ready.send_replace(1);

        log_info!("Server", "代理服务器正在运行在 http://{}", actual_addr);

        let graceful = GracefulShutdown::new();
        let mut connections = JoinSet::new();
        let request_header_timeout = self.request_header_timeout;
        let max_request_headers = self.max_request_headers;
        loop {
            tokio::select! {
                _ = self.shutdown.notified() => break,
                accepted = listener.accept() => {
                    let (stream, _) = accepted
                        .map_err(|error| ProxyError::IO(format!("接受连接失败: {error}")))?;
                    let handler = self.handler.clone();
                    let watcher = graceful.watcher();
                    connections.spawn(async move {
                        let service = service_fn(move |request| {
                            let handler = handler.clone();
                            async move {
                                let response = match handler.handle_request(request).await {
                                    Ok(response) => response,
                                    Err(error) => error_response(error),
                                };
                                Ok::<_, Infallible>(response)
                            }
                        });
                        let mut builder = ConnectionBuilder::new();
                        builder
                            .timer(TokioTimer::new())
                            .header_read_timeout(request_header_timeout)
                            .max_headers(max_request_headers);
                        let connection = builder.serve_connection(TokioIo::new(stream), service);
                        let _ = watcher.watch(connection).await;
                    });
                }
            }
        }
        if tokio::time::timeout(self.shutdown_timeout, graceful.shutdown())
            .await
            .is_err()
        {
            log_info!(
                "Server",
                "等待连接排空超过 {:?}，强制停止",
                self.shutdown_timeout
            );
            connections.abort_all();
        }
        while connections.join_next().await.is_some() {}
        let result = Ok(());
        self.background_tasks.abort_all();
        self.state.store(
            if result.is_ok() {
                STATE_STOPPED
            } else {
                STATE_FAILED
            },
            Ordering::Release,
        );
        result
    }
}

fn error_response(error: ProxyError) -> hyper::Response<AppBody> {
    log_info!("Server", "请求失败: {}", error);
    let mut builder = hyper::Response::builder().status(error.status_code());
    if matches!(error, ProxyError::MethodNotAllowed) {
        builder = builder.header(hyper::header::ALLOW, "GET, HEAD");
    }
    builder
        .body(full_body(error.public_message()))
        .unwrap_or_else(|_| {
            let mut response = hyper::Response::new(empty_body());
            *response.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
            response
        })
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
    use super::test_http_client as local_http;
    use super::*;
    use std::net::TcpListener;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn incomplete_request_headers_are_closed_after_timeout() {
        let cache = tempfile::tempdir().unwrap();
        let server = Arc::new(ProxyServer::with_config(ProxyConfig {
            port: 0,
            cache_dir: cache.path().to_path_buf(),
            request_header_timeout: std::time::Duration::from_millis(100),
            ..Default::default()
        }));
        let running = tokio::spawn({
            let server = Arc::clone(&server);
            async move { server.start().await }
        });
        let port = server.wait_until_ready().await.unwrap();

        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nX-Incomplete:")
            .await
            .unwrap();

        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut byte))
            .await
            .expect("半开请求头没有在配置时间内关闭")
            .expect("读取连接关闭状态失败");
        assert_eq!(read, 0, "超时后连接应以 EOF 关闭");

        server.stop();
        running.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn excessive_request_headers_return_431() {
        let cache = tempfile::tempdir().unwrap();
        let server = Arc::new(ProxyServer::with_config(ProxyConfig {
            port: 0,
            cache_dir: cache.path().to_path_buf(),
            max_request_headers: 2,
            ..Default::default()
        }));
        let running = tokio::spawn({
            let server = Arc::clone(&server);
            async move { server.start().await }
        });
        let port = server.wait_until_ready().await.unwrap();

        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nX-One: 1\r\nX-Two: 2\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stream.read_to_end(&mut response),
        )
        .await
        .expect("超出头数量的请求没有及时结束")
        .expect("读取 431 响应失败");
        assert!(
            response.starts_with(b"HTTP/1.1 431"),
            "响应不是 431: {response:?}"
        );

        server.stop();
        running.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn occupied_port_returns_error_instead_of_panicking() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let cache = tempfile::tempdir().unwrap();
        let server = ProxyServer::new(port, cache.path().to_str().unwrap());

        let error = server.start().await.unwrap_err();
        assert!(matches!(error, ProxyError::IO(_)));
        assert_eq!(server.status(), ProxyServerStatus::Failed);
        assert!(server.wait_until_ready().await.is_err());
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

        let request = local_http::Client::new()
            .get(format!("http://127.0.0.1:{}/proxy/media", port))
            .header("X-Original-Url", "https://example.com/video.mp4")
            .header("X-Cache-Asset-Id", "asset-1")
            .header("X-Cache-Asset-Revision", "1")
            .header("Range", "bytes=0-1023")
            .send();

        let response = tokio::time::timeout(std::time::Duration::from_secs(5), request)
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

        let body = response.bytes().await.unwrap();
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

    #[tokio::test]
    async fn stop_forces_a_hanging_connection_closed_after_timeout() {
        let port = free_port();
        let cache = tempfile::tempdir().unwrap();
        let server = Arc::new(ProxyServer::with_config(ProxyConfig {
            port,
            cache_dir: cache.path().to_path_buf(),
            shutdown_timeout: Duration::from_millis(50),
            ..Default::default()
        }));
        let running = tokio::spawn({
            let server = server.clone();
            async move { server.start().await }
        });
        wait_until_listening(port).await;

        let mut connection = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        server.stop();

        let result = tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .expect("停止操作超过外层测试时限")
            .expect("服务器任务 panic");
        assert!(result.is_ok());

        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(1), connection.read(&mut byte))
            .await
            .expect("停止后连接仍未关闭");
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "停止后 socket 仍可读取: {read:?}"
        );
    }

    #[tokio::test]
    async fn port_zero_reports_the_assigned_port_and_lifecycle_state() {
        let cache = tempfile::tempdir().unwrap();
        let server = Arc::new(ProxyServer::with_config(ProxyConfig {
            port: 0,
            cache_dir: cache.path().to_path_buf(),
            ..Default::default()
        }));
        assert_eq!(server.status(), ProxyServerStatus::Created);
        assert_eq!(server.bound_port(), None);

        let running = tokio::spawn({
            let server = server.clone();
            async move { server.start().await }
        });
        let port = tokio::time::timeout(Duration::from_secs(1), server.wait_until_ready())
            .await
            .expect("等待动态端口超时")
            .unwrap();

        assert_ne!(port, 0);
        assert_eq!(server.bound_port(), Some(port));
        assert_eq!(server.status(), ProxyServerStatus::Running);
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("动态端口不可连接");

        server.stop();
        assert_eq!(server.status(), ProxyServerStatus::Stopping);
        running.await.unwrap().unwrap();
        assert_eq!(server.status(), ProxyServerStatus::Stopped);
    }

    #[tokio::test]
    async fn running_server_rejects_a_second_start() {
        let cache = tempfile::tempdir().unwrap();
        let server = Arc::new(ProxyServer::with_config(ProxyConfig {
            port: 0,
            cache_dir: cache.path().to_path_buf(),
            ..Default::default()
        }));
        let running = tokio::spawn({
            let server = server.clone();
            async move { server.start().await }
        });
        server.wait_until_ready().await.unwrap();

        let error = server.start().await.unwrap_err();
        assert!(matches!(error, ProxyError::Request(_)));

        server.stop();
        running.await.unwrap().unwrap();
    }
}
