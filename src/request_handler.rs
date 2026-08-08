use crate::data_request::DataRequest;
use crate::data_source_manager::DataSourceManager;
use crate::hls::{DefaultHlsHandler, HlsHandler};
use crate::http_types::{empty_body, full_body, AppBody};
use crate::source_registry::SourceRegistry;
use crate::utils::error::{ProxyError, Result};
use http_body_util::BodyExt;
use hyper::header::{HeaderValue, ACCEPT_RANGES, CACHE_CONTROL, CONTENT_RANGE, CONTENT_TYPE};
use hyper::{Method, Request, Response, StatusCode};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub struct RequestHandler {
    source_manager: Arc<DataSourceManager>,
    hls_handler: Arc<DefaultHlsHandler>,
    request_limit: Arc<Semaphore>,
    source_registry: SourceRegistry,
}

impl RequestHandler {
    /// 指定并发上限。0 会被抬到 1：`Semaphore::new(0)` 会让所有请求永久
    /// 挂起，看起来是服务器卡死而不是配置写错。
    ///
    /// 这里不再提供一个「默认上限」的构造函数：默认值只应有一处来源，就是
    /// `ProxyConfig::default()`，它已经带了 64。再放一个同义常量在这一层，
    /// 只会让以后改默认值时漏掉一处。
    pub fn with_limit(
        source_manager: Arc<DataSourceManager>,
        hls_handler: Arc<DefaultHlsHandler>,
        max_concurrent_requests: usize,
        source_registry: SourceRegistry,
    ) -> Self {
        Self {
            source_manager,
            hls_handler,
            request_limit: Arc::new(Semaphore::new(max_concurrent_requests.max(1))),
            source_registry,
        }
    }

    pub async fn handle_request<B>(&self, req: Request<B>) -> Result<Response<AppBody>> {
        validate_method(req.method())?;
        let is_head = req.method() == Method::HEAD;
        let permit = self.request_limit.clone().acquire_owned().await?;
        let (req, source_id) = resolve_media_route(req, &self.source_registry)?;
        let data_request = DataRequest::with_source_id(&req, source_id)?;

        let response = match (is_head, data_request.get_type()) {
            (true, crate::data_request::RequestType::M3u8) => {
                let content = self.hls_handler.handle_m3u8(data_request.get_url()).await?;
                Response::builder()
                    .header(CONTENT_TYPE, "application/vnd.apple.mpegurl")
                    .header(CACHE_CONTROL, "no-cache")
                    .header(hyper::header::CONTENT_LENGTH, content.len())
                    .body(empty_body())
                    .map_err(|e| ProxyError::Request(format!("构建 m3u8 HEAD 响应失败: {}", e)))
            }
            (true, _) => self.source_manager.process_head(&data_request).await,
            (false, crate::data_request::RequestType::M3u8) => {
                // 处理 m3u8 请求
                let content = self.hls_handler.handle_m3u8(data_request.get_url()).await?;
                // 必须带 Content-Type：缺了它 hyper 不会补，播放器普遍会拒绝
                // 一个没有类型的播放列表，或按 text/plain 处理而不去解析。
                Response::builder()
                    .header(CONTENT_TYPE, "application/vnd.apple.mpegurl")
                    .header(CACHE_CONTROL, "no-cache")
                    .body(full_body(content))
                    .map_err(|e| ProxyError::Request(format!("构建 m3u8 响应失败: {}", e)))
            }
            // 非播放列表一律走字节范围缓存管线（HLS 分片也在内）。
            (false, _) => self.source_manager.process_request(&data_request).await,
        }?;

        let response = full_content_if_no_range_requested(response, &data_request);

        // 并发许可跟随响应体，而不是在本方法返回时释放。流式媒体响应可能持续
        // 很久，只限制响应构造阶段无法阻止大量上游连接和文件句柄同时存活。
        Ok(guard_response(response, permit))
    }
}

/// Resolve the opaque `/media/<id>` route without exposing the signed source URL
/// in the client-visible URI. Legacy `/proxy` and header routes remain supported.
fn resolve_media_route<B>(
    req: Request<B>,
    registry: &SourceRegistry,
) -> Result<(Request<B>, Option<u64>)> {
    let path = req.uri().path();
    let Some(raw_id) = path.strip_prefix("/media/") else {
        return Ok((req, None));
    };
    if raw_id.is_empty() || raw_id.contains('/') || !raw_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ProxyError::Request("媒体来源 ID 无效".to_string()));
    }
    let id = raw_id
        .parse::<u64>()
        .map_err(|_| ProxyError::Request("媒体来源 ID 无效".to_string()))?;
    let source = registry
        .resolve(id)
        .ok_or_else(|| ProxyError::Request("媒体来源不存在".to_string()))?;
    let mut builder = Request::builder()
        .method(req.method())
        .uri(req.uri().clone());
    for (name, value) in req.headers() {
        if name != "X-Original-Url"
            && name != "X-Cache-Asset-Id"
            && name != "X-Cache-Asset-Revision"
        {
            builder = builder.header(name, value);
        }
    }
    builder = builder.header("X-Original-Url", source.url);
    builder = builder
        .header("X-Cache-Asset-Id", source.identity)
        .header("X-Cache-Asset-Revision", "1");
    let body = req.into_body();
    let request = builder
        .body(body)
        .map_err(|_| ProxyError::Request("请求构造失败".to_string()))?;
    Ok((request, Some(id)))
}

/// 客户端没发 `Range` 时把 206 改写成 200。
///
/// 内部管线一律按范围请求处理：缺 `Range` 头时 `DataRequest` 会合成一个
/// `bytes=0-`，于是所有路径产出的都是 206。但 RFC 7233 讲得很明确，206 只能
/// 用来回应带 `Range` 的请求，普通 GET 必须得到 200。
///
/// 这不是洁癖。Safari 和 AVPlayer 打开资源时先发一个不带 `Range` 的 GET 探路，
/// 拿到 206 会认为服务器不守规矩，有的播放器就此不再发起后续的 seek。
///
/// 改写在这一层而不是往下传一个 bool：产出 206 的地方有四处（缓存命中、
/// 网络回源、混合源的快路径和拼接路径），而「客户端到底发没发 Range」是纯
/// HTTP 边界的信息，四条路径本身都不关心。放在这里也保证以后新增的响应
/// 路径自动被覆盖。
///
/// 改写是安全的：缺 `Range` 时合成的是 `bytes=0-`，`resolve_range` 会把它
/// 收敛成整个资源（总长度未知时它直接报错，根本走不到这里），所以此处的 206
/// 必然覆盖全资源——正是 200 该有的语义。因此无需再去检查区间是否真的完整。
fn full_content_if_no_range_requested(
    mut response: Response<AppBody>,
    request: &DataRequest,
) -> Response<AppBody> {
    // 状态码判断把 m3u8 那条本来就返回 200 的路径自动排除在外。
    if request.client_sent_range() || response.status() != StatusCode::PARTIAL_CONTENT {
        return response;
    }

    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    // `Content-Range` 在 200 里没有意义，留着反而自相矛盾。
    // `Content-Length` 此刻已等于整个资源长度，原样保留。
    headers.remove(CONTENT_RANGE);
    // 200 响应里 `Accept-Ranges` 是客户端唯一能看到的「可以 seek」信号——206
    // 本身就隐含了范围支持，而一个不带这个头的 200 会被当成不可 seek 的资源。
    // 代理对「自己支不支持范围请求」是权威的，这里直接写死。
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response
}

fn guard_response(response: Response<AppBody>, permit: OwnedSemaphorePermit) -> Response<AppBody> {
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        PermitBody {
            body,
            _permit: permit,
        }
        .boxed_unsync(),
    )
}

struct PermitBody {
    body: AppBody,
    _permit: OwnedSemaphorePermit,
}

impl hyper::body::Body for PermitBody {
    type Data = bytes::Bytes;
    type Error = ProxyError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}

fn validate_method(method: &Method) -> Result<()> {
    if method == Method::GET || method == Method::HEAD {
        Ok(())
    } else {
        Err(ProxyError::MethodNotAllowed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_registry::SourceRegistry;

    #[test]
    fn get_and_head_are_accepted() {
        assert!(validate_method(&Method::GET).is_ok());
        assert!(validate_method(&Method::HEAD).is_ok());
        for method in [Method::POST, Method::PUT, Method::DELETE] {
            let error = validate_method(&method).unwrap_err();
            assert!(matches!(error, ProxyError::MethodNotAllowed));
            assert_eq!(error.status_code(), hyper::StatusCode::METHOD_NOT_ALLOWED);
        }
    }

    #[test]
    fn media_route_resolves_opaque_id_without_client_url() {
        let registry = SourceRegistry::default();
        let id = registry
            .register("asset", "https://media.example/a.mp4?token=secret")
            .unwrap();
        let request = Request::builder()
            .uri(format!("/media/{id}"))
            .body(())
            .unwrap();
        let (resolved, source_id) = resolve_media_route(request, &registry).unwrap();
        assert_eq!(source_id, Some(id));
        assert_eq!(resolved.uri().path(), format!("/media/{id}"));
        assert_eq!(
            resolved.headers().get("X-Original-Url").unwrap(),
            "https://media.example/a.mp4?token=secret"
        );
        assert_eq!(resolved.headers().get("X-Cache-Asset-Id").unwrap(), "asset");
        assert_eq!(
            resolved.headers().get("X-Cache-Asset-Revision").unwrap(),
            "1"
        );
    }

    #[test]
    fn media_route_rejects_unknown_and_malformed_ids() {
        let registry = SourceRegistry::default();
        for path in ["/media/0", "/media/abc", "/media/1/child", "/media/"] {
            let request = Request::builder().uri(path).body(()).unwrap();
            assert!(resolve_media_route(request, &registry).is_err());
        }
    }

    #[test]
    fn media_route_replaces_forged_cache_identity_headers() {
        let registry = SourceRegistry::default();
        let id = registry
            .register("trusted-asset", "https://media.example/a.mp4")
            .unwrap();
        let request = Request::builder()
            .uri(format!("/media/{id}"))
            .header("X-Cache-Asset-Id", "attacker")
            .header("X-Cache-Asset-Revision", "999")
            .body(())
            .unwrap();
        let (resolved, source_id) = resolve_media_route(request, &registry).unwrap();
        assert_eq!(source_id, Some(id));
        assert_eq!(
            resolved.headers().get("X-Cache-Asset-Id").unwrap(),
            "trusted-asset"
        );
        assert_eq!(
            resolved.headers().get("X-Cache-Asset-Revision").unwrap(),
            "1"
        );
    }

    /// 造一个 `DataRequest`，`range` 传 `None` 表示客户端没发 Range 头。
    fn request(range: Option<&str>) -> DataRequest {
        let mut builder = Request::builder()
            .method(Method::GET)
            .uri("http://127.0.0.1/playback")
            .header("X-Original-Url", "https://media.example/song.mp4")
            .header("X-Cache-Asset-Id", "asset-1")
            .header("X-Cache-Asset-Revision", "1");
        if let Some(range) = range {
            builder = builder.header("Range", range);
        }
        DataRequest::new(&builder.body(()).unwrap()).unwrap()
    }

    fn partial_response() -> Response<AppBody> {
        let mut response = Response::new(full_body("media"));
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
        response
            .headers_mut()
            .insert(CONTENT_RANGE, HeaderValue::from_static("bytes 0-4999/5000"));
        response
            .headers_mut()
            .insert(hyper::header::CONTENT_LENGTH, HeaderValue::from(5000));
        response
    }

    #[test]
    fn no_range_header_gets_200_without_content_range() {
        let response = full_content_if_no_range_requested(partial_response(), &request(None));

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !response.headers().contains_key(CONTENT_RANGE),
            "200 响应不该带 Content-Range"
        );
        // 长度必须留着，否则客户端不知道要读多少。
        assert_eq!(response.headers()[hyper::header::CONTENT_LENGTH], "5000");
        // 这是 200 里唯一能告诉播放器「可以 seek」的头。
        assert_eq!(response.headers()[ACCEPT_RANGES], "bytes");
    }

    #[test]
    fn explicit_range_header_keeps_206() {
        // 客户端明确发了 `bytes=0-`，和「没发 Range」在内部表示上完全一样
        // （都被规范成 `bytes=0-`），必须靠标记区分。这一条就是压着那个标记的。
        for range in ["bytes=0-", "bytes=0-4999", "bytes=100-199"] {
            let response =
                full_content_if_no_range_requested(partial_response(), &request(Some(range)));
            assert_eq!(
                response.status(),
                StatusCode::PARTIAL_CONTENT,
                "客户端发了 {range}，必须保持 206"
            );
            assert_eq!(response.headers()[CONTENT_RANGE], "bytes 0-4999/5000");
        }
    }

    #[test]
    fn non_partial_responses_are_left_alone() {
        // m3u8 那条路径本来就返回 200，不能被这里再动一次（尤其不能被塞上
        // Accept-Ranges——播放列表不是可 seek 的字节流）。
        let mut original = Response::new(full_body("#EXTM3U"));
        original.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-mpegURL"),
        );

        let response = full_content_if_no_range_requested(original, &request(None));

        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(ACCEPT_RANGES));
    }

    #[tokio::test]
    async fn concurrency_permit_lives_until_response_body_finishes() {
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let response = guard_response(Response::new(full_body("media")), permit);

        assert!(semaphore.clone().try_acquire_owned().is_err());
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "media"
        );
        assert!(semaphore.try_acquire_owned().is_ok());
    }
}
