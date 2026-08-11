use crate::handlers::network::FetchedUpstream;
use crate::handlers::tee::tee_to_cache;
use crate::handlers::{BackgroundTasks, CacheHandler, NetworkHandler, ResponseBuilder};
use crate::http_types::AppBody;
use crate::log_info;
use crate::utils::error::{ProxyError, Result};
use crate::utils::network_policy::NetworkPolicy;
use crate::utils::range::{clamp_end_to_upstream_length, range_length, OPEN_ENDED};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use hyper::Response;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

const NETWORK_TIMEOUT: Duration = Duration::from_secs(30);

pub struct MixedSourceHandler {
    cache_handler: Arc<CacheHandler>,
    network_handler: NetworkHandler,
    response_builder: ResponseBuilder,
    tasks: Arc<BackgroundTasks>,
}

impl MixedSourceHandler {
    pub fn new(cache_handler: Arc<CacheHandler>, policy: Arc<NetworkPolicy>) -> Self {
        Self::with_tasks(cache_handler, policy, BackgroundTasks::new())
    }

    pub fn with_tasks(
        cache_handler: Arc<CacheHandler>,
        policy: Arc<NetworkPolicy>,
        tasks: Arc<BackgroundTasks>,
    ) -> Self {
        Self {
            cache_handler,
            network_handler: NetworkHandler::new(policy),
            response_builder: ResponseBuilder::new(),
            tasks,
        }
    }

    /// 拼接「缓存前半段 + 网络后半段」。
    ///
    /// `end` 必须已由 [`resolve_range`] 收敛，不能是 [`OPEN_ENDED`]。
    /// `cached_end` 是缓存段的开区间右边界，约定 `start < cached_end <= end`，
    /// 即两段都非空；整段都已缓存的情况由调用方的纯缓存路径处理。
    pub async fn handle(
        &self,
        url: &str,
        key: &str,
        start: u64,
        end: u64,
        cached_end: u64,
    ) -> Result<Response<AppBody>> {
        // 先校验、再打日志。旧实现在校验之前就打印 `cached_end - 1`，
        // cached_end == 0 时直接下溢 panic。
        if end == OPEN_ENDED {
            return Err(ProxyError::InvalidRange(
                "混合源范围未收敛：end 仍为开区间哨兵值".to_string(),
            ));
        }
        if start > end || cached_end <= start || cached_end > end {
            log_info!(
                "Cache",
                "请求范围无效: start={}, end={}, cached_end={}",
                start,
                end,
                cached_end
            );
            return Err(ProxyError::InvalidRange("无效的请求范围".to_string()));
        }

        log_info!(
            "Cache",
            "混合源请求开始 - 缓存范围: {}-{}, 网络范围: {}-{}",
            start,
            cached_end - 1,
            cached_end,
            end
        );

        // 全程 checked 运算。旧实现的 `cache_size + network_size` 在 end 为
        // 开区间哨兵时会溢出 usize 并 panic。
        let cache_size = usize::try_from(cached_end - start)
            .map_err(|_| ProxyError::InvalidRange("缓存段长度超出可表示范围".to_string()))?;

        // 预先发起网络请求
        let range = format!("bytes={}-{}", cached_end, end);
        log_info!("Cache", "发起网络范围请求: {}", range);

        let fetched = self.fetch_with_timeout(url, &range).await?;

        // 网络段的长度由**上游实际给的**决定，不是由我们请求的决定。
        //
        // 这里原先只在两者不一致时打一行警告，然后照旧按期望长度建响应和
        // 合并流。后果分两层：`Content-Length` 超出真实字节数让客户端读到
        // EOF，而 `create_mixed_stream` 还会因为 `network_received <
        // network_size` 在流中途注入一个「网络数据不足」错误 —— 此时响应头
        // 早已发出，客户端只能看到一个断掉的响应体。
        //
        // 按实际长度收窄 `end`，再据此重算网络段长度，响应就变成一个诚实的
        // 短 206，播放器会自己接着请求剩下的部分。
        let end = clamp_end_to_upstream_length(cached_end, end, fetched.content_length);

        // checked 运算：end 已收敛，但缓存段与网络段之和仍可能超出 usize。
        let network_size = usize::try_from(range_length(cached_end, end)?)
            .map_err(|_| ProxyError::InvalidRange("网络段长度超出可表示范围".to_string()))?;
        let total_size = cache_size
            .checked_add(network_size)
            .ok_or_else(|| ProxyError::InvalidRange("请求范围长度超出可表示范围".to_string()))?;

        log_info!(
            "Cache",
            "数据大小计算 - 缓存: {} 字节, 网络: {} 字节, 总计: {} 字节",
            cache_size,
            network_size,
            total_size
        );

        let (headers, meta, network_stream) = fetched.into_parts();
        let total_file_size = meta.total_size.unwrap_or(0);

        // 从缓存读取数据
        log_info!("Cache", "开始读取缓存范围: {}-{}", start, cached_end - 1);
        let cache_stream = match self.cache_handler.read(key, (start, cached_end - 1)).await {
            Ok(stream) => stream,
            Err(e) => {
                log_info!("Cache", "读取缓存失败: {}", e);
                return Err(e);
            }
        };

        // 网络段先 tee 一份写回缓存，再喂给合并流。少了这一步，混合源请求
        // 每次都只读旧缓存 + 回源拉新数据，缓存永远停在原来的边界上，
        // 「边播边缓存」实际只在纯网络路径生效。
        let network_stream = tee_to_cache(
            Box::pin(network_stream),
            self.cache_handler.clone(),
            key.to_string(),
            (cached_end, end),
            // 同上：混合源不参与去重。
            None,
            self.tasks.clone(),
        );

        // 创建合并的流
        let combined_stream = self.create_mixed_stream(
            cache_stream,
            Box::pin(network_stream),
            cache_size,
            network_size,
        );

        log_info!(
            "Cache",
            "创建响应 - 范围: {}-{}, 总大小: {}",
            start,
            end,
            total_file_size
        );
        self.response_builder.build_partial_content_response(
            Box::new(combined_stream),
            headers,
            start,
            end,
            total_file_size,
        )
    }

    /// 带超时的上游取数。超时只覆盖「响应头到达」，不限制响应体流式传输时长。
    async fn fetch_with_timeout(&self, url: &str, range: &str) -> Result<FetchedUpstream> {
        match timeout(NETWORK_TIMEOUT, self.network_handler.fetch(url, range)).await {
            Ok(Ok(fetched)) => Ok(fetched),
            Ok(Err(error)) => {
                log_info!("Cache", "网络请求失败: {}", error);
                Err(error)
            }
            Err(_) => {
                log_info!("Cache", "网络请求超时 ({}秒)", NETWORK_TIMEOUT.as_secs());
                Err(ProxyError::Network("网络请求超时".to_string()))
            }
        }
    }

    fn create_mixed_stream(
        &self,
        cached_stream: Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin>,
        network_stream: Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>,
        cache_size: usize,
        network_size: usize,
    ) -> impl Stream<Item = Result<Bytes>> + Send + Unpin {
        use futures_util::StreamExt;

        let cache_limited = take_bytes(cached_stream, cache_size);
        let network_limited = take_bytes(Box::pin(network_stream), network_size);

        Box::pin(cache_limited.chain(network_limited))
    }
}

/// 从流中精确读取 `limit` 字节,切掉多余数据,检测不足。
fn take_bytes<S>(
    stream: S,
    limit: usize,
) -> impl Stream<Item = Result<Bytes>> + Send + Unpin
where
    S: Stream<Item = Result<Bytes>> + Send + Unpin + 'static,
{
    Box::pin(futures_util::stream::unfold(
        (stream, 0usize, limit),
        |(mut stream, mut taken, limit)| async move {
            if taken >= limit {
                return None;
            }

            match stream.next().await {
                Some(Ok(chunk)) if chunk.is_empty() => {
                    // 跳过空块,继续读取。
                    Some((Ok(Bytes::new()), (stream, taken, limit)))
                }
                Some(Ok(chunk)) => {
                    let remaining = limit.saturating_sub(taken);
                    let chunk_size = chunk.len().min(remaining);
                    taken = taken.saturating_add(chunk_size);
                    let data = chunk.slice(..chunk_size);
                    Some((Ok(data), (stream, taken, limit)))
                }
                Some(Err(e)) => Some((Err(e), (stream, limit, limit))),
                None if taken < limit => Some((
                    Err(ProxyError::Network(format!(
                        "流提前结束：期望 {} 字节,实际 {} 字节",
                        limit, taken
                    ))),
                    (stream, limit, limit),
                )),
                None => None,
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{DiskStorage, StorageConfig, StorageManager, StorageManagerConfig};
    use futures_util::stream;

    fn handler() -> (tempfile::TempDir, MixedSourceHandler) {
        let dir = tempfile::tempdir().unwrap();
        let storage = DiskStorage::new(StorageConfig {
            root_path: dir.path().to_path_buf(),
            chunk_size: 4,
        });
        let cache = Arc::new(CacheHandler::new(Arc::new(StorageManager::new(
            storage,
            StorageManagerConfig::default(),
        ))));
        (
            dir,
            MixedSourceHandler::new(cache, Arc::new(NetworkPolicy::deny_all())),
        )
    }

    async fn collect<S>(mut stream: S) -> (Vec<u8>, Option<ProxyError>)
    where
        S: Stream<Item = Result<Bytes>> + Unpin,
    {
        let mut output = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => output.extend_from_slice(&bytes),
                Err(error) => return (output, Some(error)),
            }
        }
        (output, None)
    }

    #[tokio::test]
    async fn mixed_stream_concatenates_exact_sizes_and_trims_excess() {
        let (_dir, handler) = handler();
        let cached = Box::new(stream::iter([Ok(Bytes::from_static(b"cache-extra"))]));
        let network = Box::pin(stream::iter([Ok(Bytes::from_static(b"net-extra"))]));
        let mixed = handler.create_mixed_stream(cached, network, 5, 3);

        let (bytes, error) = collect(mixed).await;
        assert!(error.is_none());
        assert_eq!(bytes, b"cachenet");
    }

    #[tokio::test]
    async fn mixed_stream_skips_empty_chunks_without_ending_early() {
        let (_dir, handler) = handler();
        let cached = Box::new(stream::iter([
            Ok(Bytes::new()),
            Ok(Bytes::from_static(b"abc")),
        ]));
        let network = Box::pin(stream::iter([
            Ok(Bytes::new()),
            Ok(Bytes::from_static(b"def")),
        ]));
        let mixed = handler.create_mixed_stream(cached, network, 3, 3);

        let (bytes, error) = collect(mixed).await;
        assert!(error.is_none());
        assert_eq!(bytes, b"abcdef");
    }

    #[tokio::test]
    async fn mixed_stream_reports_short_cache_and_network_inputs() {
        let (_dir, handler) = handler();
        let short_cache = handler.create_mixed_stream(
            Box::new(stream::iter([Ok(Bytes::from_static(b"ab"))])),
            Box::pin(stream::iter([Ok(Bytes::from_static(b"def"))])),
            3,
            3,
        );
        let (_, cache_error) = collect(short_cache).await;
        assert!(matches!(cache_error, Some(ProxyError::Network(_))));

        let short_network = handler.create_mixed_stream(
            Box::new(stream::iter([Ok(Bytes::from_static(b"abc"))])),
            Box::pin(stream::iter([Ok(Bytes::from_static(b"de"))])),
            3,
            3,
        );
        let (bytes, network_error) = collect(short_network).await;
        assert_eq!(bytes, b"abcde");
        assert!(matches!(network_error, Some(ProxyError::Network(_))));
    }
}
