use crate::data_source::net_source::{shared_client, SharedClient};
use crate::data_source::NetSource;
use crate::handlers::response::ALLOWED_UPSTREAM_HEADERS;
use crate::log_info;
use crate::storage::UpstreamMeta;
use crate::utils::error::{ProxyError, Result};
use crate::utils::network_policy::NetworkPolicy;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use hyper::header::CONTENT_TYPE;
use hyper::{Body, HeaderMap, Response};
use std::sync::Arc;

/// 一次上游取数的结果。
pub struct FetchedUpstream {
    /// 已剔除范围相关头的上游响应头，可安全转发。
    pub headers: HeaderMap,
    /// 上游声明的本次响应体长度。
    pub content_length: u64,
    /// 可持久化的上游元数据，供后续缓存命中重建响应头。
    pub meta: UpstreamMeta,
    body: Body,
}

impl FetchedUpstream {
    /// 拆成「响应头 + 上游元数据 + 字节流」，三者都按移动交出。
    ///
    /// 调用方之前一律是「先 `headers.clone()`（有的还要 `meta.clone()`），再把
    /// 整个结构消耗掉」。克隆一个 `HeaderMap` 要为每个头名和头值各分配一次，
    /// 而原件紧接着就被丢弃——纯浪费。
    ///
    /// 流本身不再套 `Body::wrap_stream`：`hyper::Body` 自己就是
    /// `Stream<Item = Result<Bytes, hyper::Error>>`，那层包装只是把它装箱成
    /// `dyn Stream` 再包回一个 `Body`，每个数据块都要多走一次动态分发。
    pub fn into_parts(
        self,
    ) -> (
        HeaderMap,
        UpstreamMeta,
        impl Stream<Item = Result<Bytes>> + Send,
    ) {
        let stream = self
            .body
            .map(|result| result.map_err(|error| ProxyError::Network(error.to_string())));
        (self.headers, self.meta, stream)
    }
}

pub struct NetworkHandler {
    policy: Arc<NetworkPolicy>,
    /// 长期存活的连接池。旧实现每次请求都新建 Client 且把
    /// pool_max_idle_per_host 设为 0，等于每个分片重做一次 TLS 握手。
    client: SharedClient,
}

impl NetworkHandler {
    pub fn new(policy: Arc<NetworkPolicy>) -> Self {
        Self {
            policy,
            client: shared_client(),
        }
    }

    pub async fn fetch(&self, url: &str, range: &str) -> Result<FetchedUpstream> {
        let net_source = NetSource::new(url, range, self.policy.clone(), self.client.clone());
        let (resp, content_length) = net_source.download_stream().await?;
        log_info!("Cache", "网络响应成功，内容长度: {}", content_length);

        let total_size = total_size_from_content_range(&resp);
        let headers = self.extract_headers(&resp);
        let meta = UpstreamMeta {
            total_size: (total_size > 0).then_some(total_size),
            content_type: headers
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
        };

        Ok(FetchedUpstream {
            headers,
            content_length,
            meta,
            body: resp.into_body(),
        })
    }

    pub fn extract_headers(&self, resp: &Response<Body>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (key, value) in resp.headers().iter() {
            if ALLOWED_UPSTREAM_HEADERS
                .iter()
                .any(|allowed| allowed == key)
            {
                headers.insert(key, value.clone());
            }
        }
        headers
    }
}

/// 从 `Content-Range: bytes a-b/total` 解析 total；`*` 或缺失时返回 0（未知）。
fn total_size_from_content_range(resp: &Response<Body>) -> u64 {
    resp.headers()
        .get(hyper::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit('/').next())
        .and_then(|total| total.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, LOCATION, SET_COOKIE};

    #[test]
    fn extracts_upstream_headers_without_range_specific_values() {
        let response = Response::builder()
            .header(CONTENT_RANGE, "bytes 0-9/100")
            .header(CONTENT_LENGTH, "10")
            .header(CONTENT_TYPE, "audio/mp4")
            .header(CACHE_CONTROL, "private")
            .header(SET_COOKIE, "session=secret")
            .header(LOCATION, "https://example.test/redirect")
            .body(Body::empty())
            .unwrap();
        let handler = NetworkHandler::new(Arc::new(NetworkPolicy::deny_all()));

        let headers = handler.extract_headers(&response);
        assert!(!headers.contains_key(CONTENT_RANGE));
        assert!(!headers.contains_key(CONTENT_LENGTH));
        assert!(!headers.contains_key(SET_COOKIE));
        assert!(!headers.contains_key(LOCATION));
        assert_eq!(headers[CONTENT_TYPE], "audio/mp4");
        assert_eq!(headers[CACHE_CONTROL], "private");
    }

    #[test]
    fn parses_total_size_and_treats_unknown_as_zero() {
        let with_range = |value: &str| {
            Response::builder()
                .header(CONTENT_RANGE, value)
                .body(Body::empty())
                .unwrap()
        };

        assert_eq!(
            total_size_from_content_range(&with_range("bytes 0-9/100")),
            100
        );
        // `*` 表示上游不知道总长度，必须当作未知而不是 0 长度资源。
        assert_eq!(total_size_from_content_range(&with_range("bytes 0-9/*")), 0);
        assert_eq!(
            total_size_from_content_range(&Response::builder().body(Body::empty()).unwrap()),
            0
        );
    }
}
