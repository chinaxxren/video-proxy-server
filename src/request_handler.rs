use crate::data_request::DataRequest;
use crate::data_source_manager::DataSourceManager;
use crate::hls::{DefaultHlsHandler, HlsHandler};
use crate::utils::error::{ProxyError, Result};
use hyper::header::{CACHE_CONTROL, CONTENT_TYPE};
use hyper::{Body, Request, Response};
use std::sync::Arc;

pub struct RequestHandler {
    source_manager: Arc<DataSourceManager>,
    hls_handler: Arc<DefaultHlsHandler>,
}

impl RequestHandler {
    pub fn new(source_manager: Arc<DataSourceManager>, hls_handler: Arc<DefaultHlsHandler>) -> Self {
        Self {
            source_manager,
            hls_handler,
        }
    }
    
    pub async fn handle_request(&self, req: Request<Body>) -> Result<Response<Body>> {
        let data_request = DataRequest::new(&req)?;
        
        match data_request.get_type() {
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
            crate::data_request::RequestType::Segment => {
                // 保留原请求中的稳定缓存身份头，走统一分片缓存路径。
                self.source_manager.process_request(&data_request).await
            }
            _ => {
                // 处理普通请求
                self.source_manager.process_request(&data_request).await
            }
        }
    }
}
