use crate::utils::error::{ProxyError, Result};
use crate::utils::range::{range_length, OPEN_ENDED};
use bytes::Bytes;
use futures::Stream;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Body, HeaderMap, Response};

/// 逐跳头（RFC 7230 6.1）与由本层重新计算的头，均不得从上游透传给客户端。
const BLOCKED_HEADERS: [HeaderName; 10] = [
    hyper::header::CONNECTION,
    hyper::header::TRANSFER_ENCODING,
    hyper::header::CONTENT_ENCODING,
    hyper::header::CONTENT_LENGTH,
    hyper::header::CONTENT_RANGE,
    hyper::header::TRAILER,
    hyper::header::TE,
    hyper::header::UPGRADE,
    hyper::header::PROXY_AUTHENTICATE,
    hyper::header::PROXY_AUTHORIZATION,
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
    ) -> Result<Response<Body>> {
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
        let content_range = HeaderValue::from_str(&content_range)
            .map_err(|_| ProxyError::Request("无法构建 Content-Range".to_string()))?;

        let mut response = Response::new(Body::wrap_stream(stream));
        *response.status_mut() = hyper::StatusCode::PARTIAL_CONTENT;

        // 先透传上游头（白名单之外的逐跳头被剔除），再写入本层权威的范围头，
        // 避免上游值覆盖我们计算的结果。
        {
            let out = response.headers_mut();
            for (key, value) in headers.iter() {
                if BLOCKED_HEADERS.iter().any(|blocked| blocked == key) {
                    continue;
                }
                out.insert(key, value.clone());
            }
            out.insert(hyper::header::CONTENT_RANGE, content_range);
            out.insert(hyper::header::CONTENT_LENGTH, HeaderValue::from(length));
        }

        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::{
        ACCEPT_RANGES, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE,
        TRANSFER_ENCODING,
    };

    fn body() -> Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin> {
        Box::new(futures::stream::iter([
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
        assert_eq!(response.headers()[CONTENT_RANGE], "bytes 10-15/100");
        assert_eq!(response.headers()[CONTENT_LENGTH], "6");
        assert_eq!(response.headers()[CONTENT_TYPE], "audio/mp4");
        assert_eq!(response.headers()[ACCEPT_RANGES], "bytes");
        assert_eq!(
            hyper::body::to_bytes(response.into_body()).await.unwrap(),
            "abcdef"
        );
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
        assert_eq!(response.headers()[CONTENT_RANGE], "bytes 0-5/*");
        assert_eq!(response.headers()[CONTENT_LENGTH], "6");
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

        assert!(!response.headers().contains_key(TRANSFER_ENCODING));
        assert!(!response.headers().contains_key(CONTENT_ENCODING));
        // 上游的范围头没有覆盖本层计算结果。
        assert_eq!(response.headers()[CONTENT_RANGE], "bytes 10-15/100");
        assert_eq!(response.headers()[CONTENT_LENGTH], "6");
    }
}
