use crate::data_request::DataRequest;
use crate::data_source_manager::DataSourceManager;
use crate::hls::{DefaultHlsHandler, HlsHandler};
use crate::utils::error::{ProxyError, Result};
use hyper::header::{CACHE_CONTROL, CONTENT_TYPE};
use hyper::{Body, Method, Request, Response};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MAX_CONCURRENT_REQUESTS: usize = 64;

pub struct RequestHandler {
    source_manager: Arc<DataSourceManager>,
    hls_handler: Arc<DefaultHlsHandler>,
    request_limit: Arc<Semaphore>,
}

impl RequestHandler {
    pub fn new(
        source_manager: Arc<DataSourceManager>,
        hls_handler: Arc<DefaultHlsHandler>,
    ) -> Self {
        Self::with_limit(source_manager, hls_handler, MAX_CONCURRENT_REQUESTS)
    }

    /// 指定并发上限。0 会被抬到 1：`Semaphore::new(0)` 会让所有请求永久
    /// 挂起，看起来是服务器卡死而不是配置写错。
    pub fn with_limit(
        source_manager: Arc<DataSourceManager>,
        hls_handler: Arc<DefaultHlsHandler>,
        max_concurrent_requests: usize,
    ) -> Self {
        Self {
            source_manager,
            hls_handler,
            request_limit: Arc::new(Semaphore::new(max_concurrent_requests.max(1))),
        }
    }

    pub async fn handle_request(&self, req: Request<Body>) -> Result<Response<Body>> {
        validate_method(req.method())?;
        let permit = self.request_limit.clone().acquire_owned().await?;
        let data_request = DataRequest::new(&req)?;

        let response = match data_request.get_type() {
            crate::data_request::RequestType::M3u8 => {
                // 处理 m3u8 请求
                let content = self.hls_handler.handle_m3u8(data_request.get_url()).await?;
                // 必须带 Content-Type：缺了它 hyper 不会补，播放器普遍会拒绝
                // 一个没有类型的播放列表，或按 text/plain 处理而不去解析。
                Response::builder()
                    .header(CONTENT_TYPE, "application/vnd.apple.mpegurl")
                    .header(CACHE_CONTROL, "no-cache")
                    .body(Body::from(content))
                    .map_err(|e| ProxyError::Request(format!("构建 m3u8 响应失败: {}", e)))
            }
            // Segment 和 Normal 走同一缓存路径；DataRequest::new 已完成缓存身份键构造。
            _ => self.source_manager.process_request(&data_request).await,
        }?;

        // 并发许可跟随响应体，而不是在本方法返回时释放。流式媒体响应可能持续
        // 很久，只限制响应构造阶段无法阻止大量上游连接和文件句柄同时存活。
        Ok(guard_response(response, permit))
    }
}

fn guard_response(response: Response<Body>, permit: OwnedSemaphorePermit) -> Response<Body> {
    let (parts, body) = response.into_parts();
    let guarded = futures::stream::unfold((body, permit), |(mut body, permit)| async move {
        use futures::StreamExt;
        body.next().await.map(|item| (item, (body, permit)))
    });
    Response::from_parts(parts, Body::wrap_stream(guarded))
}

fn validate_method(method: &Method) -> Result<()> {
    if method == Method::GET {
        Ok(())
    } else {
        Err(ProxyError::MethodNotAllowed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_get_is_accepted() {
        assert!(validate_method(&Method::GET).is_ok());
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::HEAD] {
            let error = validate_method(&method).unwrap_err();
            assert!(matches!(error, ProxyError::MethodNotAllowed));
            assert_eq!(error.status_code(), hyper::StatusCode::METHOD_NOT_ALLOWED);
        }
    }

    #[tokio::test]
    async fn concurrency_permit_lives_until_response_body_finishes() {
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let response = guard_response(Response::new(Body::from("media")), permit);

        assert!(semaphore.clone().try_acquire_owned().is_err());
        assert_eq!(
            hyper::body::to_bytes(response.into_body()).await.unwrap(),
            "media"
        );
        assert!(semaphore.try_acquire_owned().is_ok());
    }
}
