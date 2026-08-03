use crate::data_source_manager::DataSourceManager;
use crate::hls::DefaultHlsHandler;
use crate::log_info;
use crate::request_handler::RequestHandler;
use crate::utils::error::{ProxyError, Result};
use crate::utils::network_policy::NetworkPolicy;
use hyper::service::{make_service_fn, service_fn};
use hyper::Server;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

pub struct ProxyServer {
    port: u16,
    handler: Arc<RequestHandler>,
}

impl ProxyServer {
    pub fn new(port: u16, cache_dir: &str) -> Self {
        Self::with_policy(port, cache_dir, Arc::new(NetworkPolicy::deny_all()))
    }

    pub fn with_allowed_hosts<I, S>(port: u16, cache_dir: &str, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::with_policy(port, cache_dir, Arc::new(NetworkPolicy::allow_hosts(hosts)))
    }

    fn with_policy(port: u16, cache_dir: &str, policy: Arc<NetworkPolicy>) -> Self {
        let cache_dir = PathBuf::from(cache_dir);

        // 创建数据源管理器
        let source_manager = Arc::new(DataSourceManager::new_with_policy(
            cache_dir.clone(),
            policy.clone(),
        ));

        // 创建 HLS 处理器
        let hls_handler = Arc::new(DefaultHlsHandler::new(cache_dir.clone(), policy));

        // 创建请求处理器
        let handler = Arc::new(RequestHandler::new(source_manager, hls_handler));

        Self { port, handler }
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
        let server = Server::try_bind(&addr)
            .map_err(|e| ProxyError::IO(format!("无法绑定 {}: {}", addr, e)))?
            .serve(make_svc);
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
}
