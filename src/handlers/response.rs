use crate::http_types::{stream_body, AppBody};
use crate::utils::error::{ProxyError, Result};
use crate::utils::range::{range_length, OPEN_ENDED};
use bytes::Bytes;
use futures_util::Stream;
use hyper::header::HeaderName;
use hyper::HeaderMap;

/// 允许透传给客户端的安全响应头。范围和长度由代理重新计算，不在此列表中。
pub(crate) const ALLOWED_UPSTREAM_HEADERS: [HeaderName; 5] = [
    hyper::header::CONTENT_TYPE,
    hyper::header::CACHE_CONTROL,
    hyper::header::ACCEPT_RANGES,
    hyper::header::ETAG,
    hyper::header::LAST_MODIFIED,
];

#[derive(Debug, Default, Clone, Copy)]
pub struct ResponseBuilder;

impl ResponseBuilder {
    pub fn new() -> Self {
        Self
    }

    /// 构建 206 响应。
    ///
    /// `end` 必须已经由 [`crate::utils::range::resolve_range`] 收敛过，不能是
    /// [`OPEN_ENDED`] 哨兵值；`total_size` 为 0 表示上游未给出总长度，此时
    /// `Content-Range` 用 `*` 表示未知。
    pub fn build_partial_content_response(
        &self,
        stream: Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin>,
        headers: HeaderMap,
        start: u64,
        end: u64,
        total_size: u64,
    ) -> Result<hyper::Response<AppBody>> {
        if end == OPEN_ENDED {
            return Err(ProxyError::InvalidRange(
                "响应范围未收敛：end 仍为开区间哨兵值".to_string(),
            ));
        }
        let length = range_length(start, end)?;
        let content_range = if total_size > 0 {
            format!("bytes {}-{}/{}", start, end, total_size)
        } else {
            format!("bytes {}-{}/*", start, end)
        };

        let mut response = hyper::Response::new(stream_body(stream));
        *response.status_mut() = hyper::StatusCode::PARTIAL_CONTENT;
        for (key, value) in &headers {
            if !ALLOWED_UPSTREAM_HEADERS
                .iter()
                .any(|allowed| allowed.as_str() == key.as_str())
            {
                continue;
            }
            response.headers_mut().append(key, value.clone());
        }
        response.headers_mut().insert(
            hyper::header::CONTENT_RANGE,
            hyper::header::HeaderValue::from_str(&content_range)
                .map_err(|_| ProxyError::Request("无法构建 Content-Range".to_string()))?,
        );
        response.headers_mut().insert(
            hyper::header::CONTENT_LENGTH,
            hyper::header::HeaderValue::from(length),
        );
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use hyper::header::{
        ACCEPT_RANGES, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE,
        CONTENT_TYPE, LOCATION, SET_COOKIE, TRANSFER_ENCODING,
    };

    fn body() -> Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin> {
        Box::new(futures_util::stream::iter([
            Ok(Bytes::from_static(b"abcd")),
            Ok(Bytes::from_static(b"ef")),
        ]))
    }

    #[tokio::test]
    async fn builds_partial_content_headers_and_streams_body() {
        let mut upstream_headers = HeaderMap::new();
        upstream_headers.insert(CONTENT_TYPE, "audio/mp4".parse().unwrap());
        upstream_headers.insert(ACCEPT_RANGES, "bytes".parse().unwrap());

        let response = ResponseBuilder::new()
            .build_partial_content_response(body(), upstream_headers, 10, 15, 100)
            .unwrap();

        assert_eq!(response.status(), hyper::StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()["content-range"], "bytes 10-15/100");
        assert_eq!(response.headers()["content-length"], "6");
        assert_eq!(response.headers()["content-type"], "audio/mp4");
        assert_eq!(response.headers()["accept-ranges"], "bytes");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "abcdef"
        );
    }

    #[test]
    fn preserves_duplicate_allowed_upstream_headers() {
        let mut headers = HeaderMap::new();
        headers.append(CACHE_CONTROL, "private".parse().unwrap());
        headers.append(CACHE_CONTROL, "no-store".parse().unwrap());

        let response = ResponseBuilder::new()
            .build_partial_content_response(body(), headers, 0, 5, 6)
            .unwrap();
        let values: Vec<_> = response
            .headers()
            .get_all(CACHE_CONTROL)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect();

        assert_eq!(values, ["private", "no-store"]);
    }

    #[test]
    fn open_ended_end_is_rejected_instead_of_overflowing() {
        // 回归：默认 `bytes=0-` 请求曾在此处 `end - start + 1` 溢出 panic。
        let error = ResponseBuilder::new()
            .build_partial_content_response(body(), HeaderMap::new(), 0, OPEN_ENDED, 0)
            .unwrap_err();
        assert!(matches!(error, ProxyError::InvalidRange(_)));
    }

    #[test]
    fn unknown_total_size_is_rendered_as_star() {
        let response = ResponseBuilder::new()
            .build_partial_content_response(body(), HeaderMap::new(), 0, 5, 0)
            .unwrap();
        assert_eq!(response.headers()["content-range"], "bytes 0-5/*");
        assert_eq!(response.headers()["content-length"], "6");
    }

    #[test]
    fn drops_sensitive_and_redirect_upstream_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(SET_COOKIE, "session=secret".parse().unwrap());
        headers.insert(
            LOCATION,
            "https://signed.example/file?token=secret".parse().unwrap(),
        );
        headers.insert("x-upstream-token", "secret".parse().unwrap());
        headers.insert(CONTENT_TYPE, "audio/mpeg".parse().unwrap());

        let response = ResponseBuilder::new()
            .build_partial_content_response(body(), headers, 0, 5, 6)
            .unwrap();

        assert!(!response.headers().contains_key("set-cookie"));
        assert!(!response.headers().contains_key("location"));
        assert!(!response.headers().contains_key("x-upstream-token"));
        assert_eq!(response.headers()["content-type"], "audio/mpeg");
    }

    #[test]
    fn hop_by_hop_and_encoding_headers_are_not_forwarded() {
        let mut upstream_headers = HeaderMap::new();
        upstream_headers.insert(TRANSFER_ENCODING, "chunked".parse().unwrap());
        upstream_headers.insert(CONTENT_ENCODING, "gzip".parse().unwrap());
        upstream_headers.insert(CONTENT_RANGE, "bytes 0-99/100".parse().unwrap());
        upstream_headers.insert(CONTENT_LENGTH, "100".parse().unwrap());

        let response = ResponseBuilder::new()
            .build_partial_content_response(body(), upstream_headers, 10, 15, 100)
            .unwrap();

        assert!(!response.headers().contains_key("transfer-encoding"));
        assert!(!response.headers().contains_key("content-encoding"));
        // 上游的范围头没有覆盖本层计算结果。
        assert_eq!(response.headers()["content-range"], "bytes 10-15/100");
        assert_eq!(response.headers()["content-length"], "6");
    }
}
