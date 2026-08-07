use crate::handlers::network::FetchedUpstream;
use crate::handlers::tee::tee_to_cache;
use crate::handlers::{BackgroundTasks, CacheHandler, NetworkHandler, ResponseBuilder};
use crate::log_info;
use crate::utils::error::{ProxyError, Result};
use crate::utils::network_policy::NetworkPolicy;
use crate::utils::range::{range_length, OPEN_ENDED};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use hyper::{Body, Response};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

const NETWORK_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_CACHE_SIZE: usize = 8192; // 最小缓存处理大小

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

    pub fn with_tasks(cache_handler: Arc<CacheHandler>, policy: Arc<NetworkPolicy>, tasks: Arc<BackgroundTasks>) -> Self {
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
    ) -> Result<Response<Body>> {
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

        // 如果缓存部分太小，直接从网络获取整个范围
        if cache_size < MIN_CACHE_SIZE {
            log_info!(
                "Cache",
                "缓存范围过小 ({} 字节), 直接从网络获取整个范围: {}-{}",
                cache_size,
                start,
                end
            );

            let range = format!("bytes={}-{}", start, end);
            let fetched = self.fetch_with_timeout(url, &range).await?;
            let (headers, meta, network_stream) = fetched.into_parts();
            let total_file_size = meta.total_size.unwrap_or(0);

            // 这条快路径同样要写回缓存。否则「缓存段太小」的请求永远只走网络，
            // 缓存一直停在那不足 8KB 的开头，下一次请求还是全量回源。
            let client_stream = tee_to_cache(
                Box::pin(network_stream),
                self.cache_handler.clone(),
                key.to_string(),
                (start, end),
                // 混合源路径不参与去重：进到这里说明缓存里已有可用前缀，
                // 每个请求要补的尾段各不相同，没有「同一个区间被重复拉取」
                // 可言。去重只装在完全走网络那条路径上。
                None,
                self.tasks.clone(),
            );

            log_info!(
                "Cache",
                "创建响应 - 范围: {}-{}, 总大小: {}",
                start,
                end,
                total_file_size
            );
            return self.response_builder.build_partial_content_response(
                Box::new(client_stream),
                headers,
                start,
                end,
                total_file_size,
            );
        }

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

        // 预先发起网络请求
        let range = format!("bytes={}-{}", cached_end, end);
        log_info!("Cache", "发起网络范围请求: {}", range);

        let fetched = self.fetch_with_timeout(url, &range).await?;

        // 验证网络响应大小
        if fetched.content_length != network_size as u64 {
            log_info!(
                "Cache",
                "警告：网络响应大小不匹配 - 期望: {} 字节, 实际: {} 字节",
                network_size,
                fetched.content_length
            );
        }

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
        struct StreamState {
            cached_stream: Option<Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin>>,
            network_stream: Option<Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>>,
            using_cache: bool,
            cache_received: usize,
            network_received: usize,
            cache_size: usize,
            network_size: usize,
            error_occurred: bool,
            chunk_count: usize,
        }

        let state = StreamState {
            cached_stream: Some(cached_stream),
            network_stream: Some(network_stream),
            using_cache: true,
            cache_received: 0,
            network_received: 0,
            cache_size,
            network_size,
            error_occurred: false,
            chunk_count: 0,
        };

        Box::pin(futures::stream::unfold(
            state,
            move |mut state| async move {
                if state.error_occurred {
                    return None;
                }

                if state.using_cache && state.cache_received < state.cache_size {
                    if let Some(ref mut stream) = state.cached_stream {
                        match stream.next().await {
                            Some(Ok(chunk)) => {
                                if chunk.is_empty() {
                                    return Some((Ok(Bytes::new()), state));
                                }
                                let remaining = state.cache_size - state.cache_received;
                                let chunk_size = chunk.len().min(remaining);

                                if chunk_size > 0 {
                                    // slice 而非 to_vec：Bytes 的切片只是同一块
                                    // 内存上的新视图，加一次引用计数即可。to_vec
                                    // 会把整条响应体在这里再复制一遍。
                                    let data = chunk.slice(..chunk_size);
                                    state.cache_received += chunk_size;
                                    state.chunk_count += 1;

                                    log_info!("Cache", "发送缓存数据 #{} - 大小: {} 字节, 已发送: {}/{} 字节 ({:.1}%)",
                                    state.chunk_count,
                                    chunk_size,
                                    state.cache_received,
                                    state.cache_size,
                                    (state.cache_received as f64 / state.cache_size as f64 * 100.0));

                                    if state.cache_received >= state.cache_size {
                                        state.using_cache = false;
                                        state.cached_stream = None;
                                        state.chunk_count = 0;
                                        log_info!("Cache", "缓存数据发送完毕，切换到网络数据");
                                    }

                                    return Some((Ok(data), state));
                                }
                            }
                            Some(Err(e)) => {
                                log_info!("Cache", "读取缓存数据错误: {}", e);
                                state.error_occurred = true;
                                state.using_cache = false;
                                state.cached_stream = None;
                                return Some((Err(e), state));
                            }
                            None => {
                                if state.cache_received < state.cache_size {
                                    log_info!(
                                        "Cache",
                                        "警告：缓存数据不足 - 已接收: {} 字节, 期望: {} 字节",
                                        state.cache_received,
                                        state.cache_size
                                    );
                                    state.error_occurred = true;
                                    return Some((
                                        Err(ProxyError::Network("缓存数据不足".to_string())),
                                        state,
                                    ));
                                }

                                state.using_cache = false;
                                state.cached_stream = None;
                                state.chunk_count = 0;
                                log_info!("Cache", "缓存数据发送完毕，切换到网络数据");
                            }
                        }
                    }
                }

                if !state.using_cache && state.network_received < state.network_size {
                    if let Some(ref mut stream) = state.network_stream {
                        match stream.as_mut().next().await {
                            Some(Ok(chunk)) => {
                                if chunk.is_empty() {
                                    return Some((Ok(Bytes::new()), state));
                                }
                                let remaining = state.network_size - state.network_received;
                                let chunk_size = chunk.len().min(remaining);

                                if chunk_size > 0 {
                                    let data = chunk.slice(..chunk_size);
                                    state.network_received += chunk_size;
                                    state.chunk_count += 1;

                                    log_info!("Cache", "发送网络数据 #{} - 大小: {} 字节, 已发送: {}/{} 字节 ({:.1}%)",
                                    state.chunk_count,
                                    chunk_size,
                                    state.network_received,
                                    state.network_size,
                                    (state.network_received as f64 / state.network_size as f64 * 100.0));

                                    if state.network_received >= state.network_size {
                                        state.network_stream = None;
                                        log_info!(
                                            "Cache",
                                            "网络数据发送完毕 - 总计发送: {} 字节",
                                            state.network_received
                                        );
                                    }

                                    return Some((Ok(data), state));
                                }
                            }
                            Some(Err(e)) => {
                                log_info!("Cache", "读取网络数据错误: {}", e);
                                state.error_occurred = true;
                                state.network_stream = None;
                                return Some((Err(e), state));
                            }
                            None => {
                                if state.network_received < state.network_size {
                                    log_info!(
                                        "Cache",
                                        "警告：网络数据不足 - 已接收: {} 字节, 期望: {} 字节",
                                        state.network_received,
                                        state.network_size
                                    );
                                    state.error_occurred = true;
                                    return Some((
                                        Err(ProxyError::Network("网络数据不足".to_string())),
                                        state,
                                    ));
                                }

                                state.network_stream = None;
                                log_info!(
                                    "Cache",
                                    "网络数据发送完毕 - 总计发送: {} 字节",
                                    state.network_received
                                );
                                return None;
                            }
                        }
                    }
                }

                if state.cache_received >= state.cache_size
                    && state.network_received >= state.network_size
                {
                    log_info!(
                        "Cache",
                        "数据传输完成 - 缓存: {} 字节, 网络: {} 字节, 总计: {} 字节",
                        state.cache_received,
                        state.network_received,
                        state.cache_received + state.network_received
                    );
                    return None;
                }

                // 走到这里说明两侧都没有可读数据，且计数没达到预期。返回 None 结束
                // 流即可 —— 再改 state 也没有意义，它随即被丢弃。
                if state.using_cache {
                    log_info!("Cache", "缓存数据发送完毕，切换到网络数据");
                }

                None
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{DiskStorage, StorageConfig, StorageManager, StorageManagerConfig};
    use futures::stream;

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
