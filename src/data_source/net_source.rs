use std::error::Error as StdError;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures_util::StreamExt;
use http_body_util::{BodyExt, Full};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::HttpConnector as HttpConnectorV1;
use hyper_util::client::legacy::Client as ClientV1;
use hyper_util::rt::TokioExecutor;

use crate::log_info;
use crate::utils::error::ProxyError;
use crate::utils::error::Result;
use crate::utils::network_policy::{NetworkPolicy, PublicOnlyResolverV1};
use crate::utils::range::{parse_range, OPEN_ENDED};

/// 复用的 HTTPS 客户端类型。
///
/// 解析器固定为 [`PublicOnlyResolver`]，让「只连公网地址」成为连接池的类型
/// 约束而非调用方的自觉：任何拿到 `SharedClient` 的代码都无法绕开它。
pub type SharedClientV1 =
    Arc<ClientV1<HttpsConnector<HttpConnectorV1<PublicOnlyResolverV1>>, Full<bytes::Bytes>>>;
pub type UpstreamByteStream =
    Pin<Box<dyn futures_util::Stream<Item = Result<bytes::Bytes>> + Send>>;

fn empty_upstream_stream() -> UpstreamByteStream {
    Box::pin(futures_util::stream::empty())
}

pub struct NetResponse {
    pub status: hyper::StatusCode,
    pub headers: hyper::HeaderMap,
    pub body: UpstreamByteStream,
}

fn v1_data_stream<B>(body: B) -> UpstreamByteStream
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: StdError + Send + Sync + 'static,
{
    Box::pin(body.into_data_stream().map(|chunk| {
        chunk.map_err(|error| ProxyError::Network(format!("读取上游响应失败: {error}")))
    }))
}

/// 响应头到达的超时上限。响应体是流式的，不在此计时。
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
/// TCP 连接建立的超时上限。缺了它，连到黑洞地址的请求会一直挂着。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ATTEMPTS: u32 = 3;
/// 第一次重试前的等待时长，之后逐次加倍。
///
/// 原先是固定 1 秒。固定间隔有两个问题：上游只是瞬时抖动时，白等满 1 秒才重试，
/// 而这段时间播放器那边就是纯延迟；上游真的过载时，所有并发分片又都以同一个固定
/// 节奏回打，正好在它最脆弱的时候维持住压力。改成 200ms → 400ms 之后，
/// 瞬时故障的恢复快了五倍，而两次重试的总退避时间反而更短。
const RETRY_BACKOFF: Duration = Duration::from_millis(200);

static SHARED_CLIENT_V1: OnceLock<SharedClientV1> = OnceLock::new();

pub fn shared_client_v1() -> SharedClientV1 {
    SHARED_CLIENT_V1
        .get_or_init(|| {
            let mut http = HttpConnectorV1::new_with_resolver(PublicOnlyResolverV1::new());
            http.enforce_http(false);
            http.set_connect_timeout(Some(CONNECT_TIMEOUT));
            let https = HttpsConnectorBuilder::new()
                .with_webpki_roots()
                .https_or_http()
                .enable_http1()
                .wrap_connector(http);
            Arc::new(
                ClientV1::builder(TokioExecutor::new())
                    .pool_idle_timeout(Duration::from_secs(90))
                    .build(https),
            )
        })
        .clone()
}

#[derive(Clone, Debug)]
pub struct NetSource {
    pub url: String,
    pub range: String,
    policy: Arc<NetworkPolicy>,
    client_v1: SharedClientV1,
}

impl NetSource {
    pub fn new(url: &str, range: &str, policy: Arc<NetworkPolicy>) -> Self {
        Self {
            url: url.to_string(),
            range: range.to_string(),
            policy,
            client_v1: shared_client_v1(),
        }
    }

    pub async fn download_stream(&self) -> Result<(NetResponse, u64)> {
        self.policy.validate(&self.url).await?;
        let (start, end) = parse_range(&self.range)?;

        let mut last_error = None;
        for attempt in 1..=MAX_ATTEMPTS {
            match self.try_download(start, end).await {
                Ok((resp, content_length)) => {
                    // 先保存需要的部分，再消费 resp。
                    let status = resp.status;
                    let headers = resp.headers;
                    let body = resp.body;

                    // 包装响应体，让它能在流式读取失败时自动重试。
                    let wrapped_body =
                        Self::wrap_with_retry(body, self.clone(), start, end, MAX_ATTEMPTS);

                    // 构造新的 Response。
                    let wrapped_resp = NetResponse {
                        status,
                        headers,
                        body: wrapped_body,
                    };
                    return Ok((wrapped_resp, content_length));
                }
                // 416 是客户端语义错误，重试不会改变结果。
                Err(error @ ProxyError::InvalidRange(_)) => return Err(error),
                Err(error) => {
                    log_info!("Request", "第 {} 次尝试失败: {}", attempt, error);
                    last_error = Some(error);
                    if attempt < MAX_ATTEMPTS {
                        // 指数退避：attempt 从 1 起，依次 200ms、400ms。
                        //
                        // 原先是固定 1 秒。上游偶发抖动时前两次重试白等 2 秒，
                        // 而播放器还在等首字节；上游真的过载时，固定间隔又等于
                        // 稳定地继续加压。退避两头都更合适。
                        //
                        // checked_shl 而非直接 `1 << (attempt - 1)`：MAX_ATTEMPTS
                        // 若被调到 33 以上，移位本身就会溢出（debug 下 panic）。
                        let factor = 1u32.checked_shl(attempt - 1).unwrap_or(u32::MAX);
                        tokio::time::sleep(RETRY_BACKOFF.saturating_mul(factor)).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| ProxyError::Request("Max retries reached".into())))
    }

    /// 包装响应体，让它能在流式读取失败时从断点自动重试。
    ///
    /// 用 `futures::stream::unfold` 创建一个有状态的流：记录已读字节数，
    /// 遇到流错误时发起新请求继续拉取剩余部分，最多重试 `max_attempts` 次。
    fn wrap_with_retry(
        initial_body: UpstreamByteStream,
        source: NetSource,
        range_start: u64,
        range_end: u64,
        max_attempts: u32,
    ) -> UpstreamByteStream {
        // 状态：(当前body, 已读字节数, 已尝试次数)
        type State = (UpstreamByteStream, u64, u32);
        let initial_state: State = (initial_body, 0, 1);

        let stream = futures_util::stream::unfold(
            (initial_state, source, range_start, range_end, max_attempts),
            |(state, source, range_start, range_end, max_attempts)| async move {
                let (mut body, mut bytes_read, mut attempt) = state;

                loop {
                    // 尝试从当前 body 读一块数据。
                    match body.next().await {
                        Some(Ok(chunk)) => {
                            let next_bytes_read = match advance_bytes_read(
                                bytes_read,
                                chunk.len(),
                                range_start,
                                range_end,
                            ) {
                                Ok(value) => value,
                                Err(error) => {
                                    return Some((
                                        Err(error),
                                        (
                                            (empty_upstream_stream(), bytes_read, max_attempts),
                                            source,
                                            range_start,
                                            range_end,
                                            max_attempts,
                                        ),
                                    ));
                                }
                            };
                            bytes_read = next_bytes_read;
                            let new_state = (body, bytes_read, attempt);
                            return Some((
                                Ok(chunk),
                                (new_state, source, range_start, range_end, max_attempts),
                            ));
                        }
                        Some(Err(error)) => {
                            // 流报错，尝试重试。
                            if attempt >= max_attempts {
                                log_info!(
                                    "Request",
                                    "流式读取失败且已达最大重试次数 {}",
                                    max_attempts
                                );
                                return Some((
                                    Err(ProxyError::Network(format!("流式读取失败: {}", error))),
                                    (
                                        (body, bytes_read, attempt),
                                        source,
                                        range_start,
                                        range_end,
                                        max_attempts,
                                    ),
                                ));
                            }

                            // 发起重试。
                            attempt += 1;
                            let Some(resume_start) = range_start.checked_add(bytes_read) else {
                                return Some((
                                    Err(ProxyError::Network("续传位置超出可表示范围".to_string())),
                                    (
                                        (empty_upstream_stream(), bytes_read, max_attempts),
                                        source,
                                        range_start,
                                        range_end,
                                        max_attempts,
                                    ),
                                ));
                            };
                            let resume_range = if range_end == OPEN_ENDED {
                                format!("bytes={}-", resume_start)
                            } else {
                                format!("bytes={}-{}", resume_start, range_end)
                            };

                            log_info!(
                                "Request",
                                "流式读取中断于 {} 字节，第 {} 次重试: {}",
                                bytes_read,
                                attempt,
                                resume_range
                            );

                            // 指数退避。
                            let factor = 1u32.checked_shl(attempt - 1).unwrap_or(u32::MAX);
                            tokio::time::sleep(RETRY_BACKOFF.saturating_mul(factor)).await;

                            let retry_source =
                                NetSource::new(&source.url, &resume_range, source.policy.clone());

                            match retry_source.try_download(resume_start, range_end).await {
                                Ok((resp, _)) => {
                                    body = resp.body;
                                    // 继续循环，从新 body 读取。
                                }
                                Err(retry_error) => {
                                    log_info!("Request", "重试失败: {}", retry_error);
                                    return Some((
                                        Err(retry_error),
                                        (
                                            (body, bytes_read, attempt),
                                            source,
                                            range_start,
                                            range_end,
                                            max_attempts,
                                        ),
                                    ));
                                }
                            }
                        }
                        None => {
                            // 流正常结束。
                            return None;
                        }
                    }
                }
            },
        );

        Box::pin(stream)
    }

    async fn try_download(&self, start: u64, end: u64) -> Result<(NetResponse, u64)> {
        let req = hyper::Request::builder()
            .method("GET")
            .uri(&self.url)
            .header("Range", &self.range)
            .header("User-Agent", "Mozilla/5.0 MediaProxyCache/1")
            .header("Accept", "*/*")
            .body(Full::new(bytes::Bytes::new()))
            .map_err(|_| ProxyError::Request("无法构造上游请求".to_string()))?;
        log_info!("Request", "Range header: {}", self.range);
        let response = tokio::time::timeout(HEADER_TIMEOUT, self.client_v1.request(req))
            .await
            .map_err(|_| ProxyError::Network("等待上游响应头超时".to_string()))?
            .map_err(|error| ProxyError::Network(format!("上游请求失败: {error}")))?;
        let (parts, body) = response.into_parts();
        let resp = NetResponse {
            status: parts.status,
            headers: parts.headers,
            body: v1_data_stream(body),
        };

        let status = resp.status;
        if status == hyper::StatusCode::RANGE_NOT_SATISFIABLE {
            return Err(ProxyError::InvalidRange(format!(
                "上游拒绝范围请求: {}",
                self.range
            )));
        }
        if status == hyper::StatusCode::UNAUTHORIZED || status == hyper::StatusCode::FORBIDDEN {
            return Err(ProxyError::UpstreamAuthorizationExpired);
        }
        if !status.is_success() {
            return Err(ProxyError::Request(format!(
                "Invalid response status: {}",
                status
            )));
        }

        // 上游可以合法地忽略 Range 而返回 200 + 整个文件。
        // - start > 0：缓存偏移会对不上，拒绝。
        // - end 有限：请求了明确的截止位置，200 无法保证只返回该范围，拒绝。
        // - bytes=0-（end == OPEN_ENDED）：整段从头开始，200 等同于完整文件，允许。
        if status != hyper::StatusCode::PARTIAL_CONTENT && (start > 0 || end != OPEN_ENDED) {
            return Err(ProxyError::Network(format!(
                "上游忽略了 Range 请求（状态 {}），要求 206 但收到其他状态码（range: {}-{}）",
                status, start, end
            )));
        }

        let content_length = match resp.headers.get(hyper::header::CONTENT_LENGTH) {
            Some(len) => len
                .to_str()
                .map_err(|_| ProxyError::Request("Invalid content length header".into()))?
                .parse::<u64>()
                .map_err(|_| ProxyError::Request("Invalid content length value".into()))?,
            None => return Err(ProxyError::Request("Missing content length header".into())),
        };

        if status == hyper::StatusCode::PARTIAL_CONTENT {
            verify_content_range(&resp.headers, start, end, content_length)?;
        }

        // 状态、响应头和 Hyper 1 数据帧流直接交给上层。
        Ok((resp, content_length))
    }
}

fn advance_bytes_read(
    bytes_read: u64,
    chunk_length: usize,
    range_start: u64,
    range_end: u64,
) -> Result<u64> {
    let chunk_length = u64::try_from(chunk_length)
        .map_err(|_| ProxyError::Network("上游响应块长度超出可表示范围".to_string()))?;
    let next = bytes_read
        .checked_add(chunk_length)
        .ok_or_else(|| ProxyError::Network("上游响应体累计长度溢出".to_string()))?;

    if range_end != OPEN_ENDED {
        let expected = range_end
            .checked_sub(range_start)
            .and_then(|span| span.checked_add(1))
            .ok_or_else(|| ProxyError::InvalidRange("上游请求范围无效".to_string()))?;
        if next > expected {
            return Err(ProxyError::Network(
                "上游响应体超过请求的 Range 范围".to_string(),
            ));
        }
    }

    Ok(next)
}

/// 校验 `Content-Range` 的起始偏移与我们请求的一致。
///
/// 不校验就落盘等于相信上游返回的是我们要的那一段；偏移错位会静默写坏缓存。
fn verify_content_range(
    headers: &hyper::HeaderMap,
    expected_start: u64,
    expected_end: u64,
    content_length: u64,
) -> Result<()> {
    let value = headers
        .get(hyper::header::CONTENT_RANGE)
        .ok_or_else(|| ProxyError::Network("206 响应缺少 Content-Range".to_string()))?
        .to_str()
        .map_err(|_| ProxyError::Request("Invalid content range header".into()))?;

    let (range, total) = value
        .trim()
        .strip_prefix("bytes ")
        .and_then(|rest| rest.split_once('/'))
        .ok_or_else(|| ProxyError::Network(format!("无法解析 Content-Range: {}", value)))?;
    let (actual_start, actual_end) = range
        .split_once('-')
        .and_then(|(start, end)| Some((start.parse::<u64>().ok()?, end.parse::<u64>().ok()?)))
        .ok_or_else(|| ProxyError::Network(format!("无法解析 Content-Range: {}", value)))?;

    if actual_start != expected_start {
        return Err(ProxyError::Network(format!(
            "上游返回的范围起点 {} 与请求的 {} 不一致",
            actual_start, expected_start
        )));
    }
    if actual_end < actual_start {
        return Err(ProxyError::Network(
            "Content-Range 结束位置早于起点".to_string(),
        ));
    }
    if expected_end != u64::MAX && actual_end > expected_end {
        return Err(ProxyError::Network(format!(
            "上游返回的范围终点 {} 超过请求的 {}",
            actual_end, expected_end
        )));
    }
    let actual_length = actual_end
        .checked_sub(actual_start)
        .and_then(|span| span.checked_add(1))
        .ok_or_else(|| ProxyError::Network("Content-Range 长度溢出".to_string()))?;
    if actual_length != content_length {
        return Err(ProxyError::Network(format!(
            "Content-Range 长度 {} 与 Content-Length {} 不一致",
            actual_length, content_length
        )));
    }
    if total != "*" {
        let total = total
            .parse::<u64>()
            .map_err(|_| ProxyError::Network("Content-Range 总长度无效".to_string()))?;
        if total == 0 || actual_end >= total {
            return Err(ProxyError::Network(
                "Content-Range 超出声明的资源总长度".to_string(),
            ));
        }
    }

    log_info!("Request", "Content-Range: {}", value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(content_range: Option<&str>) -> hyper::HeaderMap {
        let mut headers = hyper::HeaderMap::new();
        if let Some(value) = content_range {
            headers.insert(hyper::header::CONTENT_RANGE, value.parse().unwrap());
        }
        headers
    }

    #[tokio::test]
    async fn hyper_shared_client_reuses_one_connection_pool() {
        let first = shared_client_v1();
        let second = shared_client_v1();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn response_byte_counter_rejects_overflow_and_range_overrun() {
        assert_eq!(advance_bytes_read(10, 5, 100, 199).unwrap(), 15);
        assert!(advance_bytes_read(u64::MAX, 1, 0, OPEN_ENDED).is_err());
        assert!(advance_bytes_read(99, 2, 100, 199).is_err());
        assert!(advance_bytes_read(0, 1, 200, 100).is_err());
    }

    #[test]
    fn verifies_complete_content_range_contract() {
        assert!(verify_content_range(&response(Some("bytes 10-19/100")), 10, 19, 10,).is_ok());
        assert!(verify_content_range(&response(Some("bytes 10-19/*")), 10, u64::MAX, 10,).is_ok());
    }

    #[test]
    fn rejects_missing_malformed_or_misaligned_content_range() {
        let cases = [
            (None, 10, 19, 10),
            (Some("items 10-19/100"), 10, 19, 10),
            (Some("bytes 9-18/100"), 10, 19, 10),
            (Some("bytes 10-20/100"), 10, 19, 11),
            (Some("bytes 19-10/100"), 19, 20, 0),
            (Some("bytes 10-x/100"), 10, 19, 10),
            (Some("bytes 10-19/nope"), 10, 19, 10),
            (Some("bytes 10-19/19"), 10, 19, 10),
            (Some("bytes 10-19/100"), 10, 19, 9),
        ];

        for (value, start, end, length) in cases {
            assert!(
                verify_content_range(&response(value), start, end, length).is_err(),
                "accepted {:?}",
                value
            );
        }
    }
}
