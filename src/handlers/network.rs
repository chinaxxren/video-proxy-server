use crate::data_source::net_source::{NetResponse, UpstreamByteStream};
use crate::data_source::NetSource;
use crate::handlers::response::ALLOWED_UPSTREAM_HEADERS;
use crate::log_info;
use crate::storage::UpstreamMeta;
use crate::utils::error::{ProxyError, Result};
use crate::utils::network_policy::NetworkPolicy;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use hyper::header::CONTENT_TYPE;
use hyper::HeaderMap;
use std::sync::Arc;

/// 一次上游取数的结果。
pub struct FetchedUpstream {
    /// 已剔除范围相关头的上游响应头，可安全转发。
    pub headers: HeaderMap,
    /// 上游声明的本次响应体长度。
    pub content_length: u64,
    /// 可持久化的上游元数据，供后续缓存命中重建响应头。
    pub meta: UpstreamMeta,
    body: UpstreamByteStream,
}

impl FetchedUpstream {
    /// 拆成「响应头 + 上游元数据 + 字节流」，三者都按移动交出。
    ///
    /// 调用方之前一律是「先 `headers.clone()`（有的还要 `meta.clone()`），再把
    /// 整个结构消耗掉」。克隆一个 `HeaderMap` 要为每个头名和头值各分配一次，
    /// 而原件紧接着就被丢弃——纯浪费。
    ///
    /// 流保持为字节流，不再反复包装成 HTTP Body。
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
}

impl NetworkHandler {
    pub fn new(policy: Arc<NetworkPolicy>) -> Self {
        Self { policy }
    }

    pub async fn fetch(&self, url: &str, range: &str) -> Result<FetchedUpstream> {
        let net_source = NetSource::new(url, range, self.policy.clone());
        let (resp, content_length) = net_source.download_stream().await?;
        log_info!("Cache", "网络响应成功，内容长度: {}", content_length);

        // 200 响应的总长度只能来自 `Content-Length`。
        //
        // `Content-Range` 是 206 专有的头，200 里根本不会出现，于是
        // `total_size_from_content_range` 必然返回 0（未知）。而「未知总长度」
        // 会让 `resolve_range` 拒绝开区间请求 —— 结果是上游明明把整个文件都给
        // 了我们，代理却以 416 收场。
        //
        // 能这么推断是因为 `try_download` 只在 `start == 0 && end == OPEN_ENDED`
        // 时才接受 200（其余情况一律判为「上游忽略了 Range」并拒绝），所以走到
        // 这里的 200 响应体必然是从 0 开始的完整资源，它的长度就是总长度。
        let total_size = match total_size_from_content_range(&resp) {
            0 if resp.status == hyper::StatusCode::OK => content_length,
            from_content_range => from_content_range,
        };
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
            body: resp.body,
        })
    }

    pub fn extract_headers(&self, resp: &NetResponse) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (key, value) in &resp.headers {
            if ALLOWED_UPSTREAM_HEADERS
                .iter()
                .any(|allowed| allowed.as_str() == key.as_str())
            {
                headers.append(key, value.clone());
            }
        }
        headers
    }
}

/// 从 `Content-Range: bytes a-b/total` 解析 total；`*` 或缺失时返回 0（未知）。
fn total_size_from_content_range(resp: &NetResponse) -> u64 {
    resp.headers
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

    fn response(headers: &[(&str, &str)]) -> NetResponse {
        let mut map = hyper::HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                hyper::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                hyper::header::HeaderValue::from_str(value).unwrap(),
            );
        }
        NetResponse {
            status: hyper::StatusCode::PARTIAL_CONTENT,
            headers: map,
            body: Box::pin(futures_util::stream::empty()),
        }
    }

    #[test]
    fn extracts_upstream_headers_without_range_specific_values() {
        let response = response(&[
            ("content-range", "bytes 0-9/100"),
            ("content-length", "10"),
            ("content-type", "audio/mp4"),
            ("cache-control", "private"),
            ("set-cookie", "session=secret"),
            ("location", "https://example.test/redirect"),
        ]);
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
        let with_range = |value: &str| response(&[("content-range", value)]);

        assert_eq!(
            total_size_from_content_range(&with_range("bytes 0-9/100")),
            100
        );
        // `*` 表示上游不知道总长度，必须当作未知而不是 0 长度资源。
        assert_eq!(total_size_from_content_range(&with_range("bytes 0-9/*")), 0);
        assert_eq!(total_size_from_content_range(&response(&[])), 0);
    }
}
