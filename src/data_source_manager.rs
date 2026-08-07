use crate::data_request::DataRequest;
use crate::handlers::{
    tee_to_cache, BackgroundTasks, CacheHandler, Follower, Join, LeaderGuard, MixedSourceHandler, NetworkHandler,
    ResponseBuilder, SingleFlight,
};
use crate::log_info;
use crate::storage::{
    DiskStorage, StorageConfig, StorageManager, StorageManagerConfig, UpstreamMeta,
};
use crate::utils::error::Result;
use crate::utils::network_policy::NetworkPolicy;
use crate::utils::range::{parse_range, resolve_range};
use hyper::header::{HeaderMap, CONTENT_TYPE};
use hyper::{Body, Response};
use std::path::PathBuf;
use std::sync::Arc;

pub struct DataSourceManager {
    cache_handler: Arc<CacheHandler>,
    network_handler: NetworkHandler,
    mixed_source_handler: MixedSourceHandler,
    response_builder: ResponseBuilder,
    /// 同一 `(key, 区间)` 的并发回源合并表，见 [`SingleFlight`]。
    single_flight: Arc<SingleFlight>,
    tasks: Arc<BackgroundTasks>,
}

impl DataSourceManager {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self::new_with_policy(cache_dir, Arc::new(NetworkPolicy::deny_all()))
    }

    pub fn new_with_policy(cache_dir: PathBuf, policy: Arc<NetworkPolicy>) -> Self {
        Self::with_config(cache_dir, policy, StorageManagerConfig::default())
    }

    /// 完整构造入口：缓存容量与清理周期由调用方决定。
    ///
    /// 宿主进程对「这个缓存目录最多能占多少盘」的要求各不相同，写死在
    /// [`StorageManagerConfig::default`] 里的 1GB 只是一个能跑起来的默认值。
    pub fn with_config(
        cache_dir: PathBuf,
        policy: Arc<NetworkPolicy>,
        manager_config: StorageManagerConfig,
    ) -> Self {
        Self::with_tasks(cache_dir, policy, manager_config, BackgroundTasks::new())
    }

    pub fn with_tasks(
        cache_dir: PathBuf,
        policy: Arc<NetworkPolicy>,
        manager_config: StorageManagerConfig,
        tasks: Arc<BackgroundTasks>,
    ) -> Self {
        log_info!("Cache", "初始化数据源管理器，缓存目录: {:?}", cache_dir);

        let storage_config = StorageConfig {
            root_path: cache_dir.clone(),
            // 64KB。缓存读的每个分块都要单独 allocate 一个 buffer、走一次
            // read syscall、再过一遍 channel；8KB 时读 1GB 就是 13 万次，
            // 而视频播放本来就是顺序大块读，分块小到 8KB 毫无收益。
            chunk_size: 64 * 1024,
        };

        let storage_engine = DiskStorage::new(storage_config);
        let storage_manager = Arc::new(StorageManager::new(storage_engine, manager_config));

        let cache_handler = Arc::new(CacheHandler::new(storage_manager));
        let network_handler = NetworkHandler::new(policy.clone());
        let mixed_source_handler = MixedSourceHandler::with_tasks(cache_handler.clone(), policy, tasks.clone());
        let response_builder = ResponseBuilder::new();

        Self {
            cache_handler,
            network_handler,
            mixed_source_handler,
            response_builder,
            single_flight: Arc::new(SingleFlight::new()),
            tasks,
        }
    }

    pub async fn process_request(&self, req: &DataRequest) -> Result<Response<Body>> {
        let url = req.get_url();
        let key = req.get_cache_key()?.to_string();
        let (start, requested_end) = parse_range(req.get_range())?;

        log_info!("Cache", "开始处理请求范围: {}", req.get_range());

        // 元数据只读一次，供以下两条缓存路径共用。
        let meta = self.cache_handler.upstream_meta(&key).await?;

        // 1) 完整缓存命中。总长度来自持久化元数据，因此上游不可达时依然可服务。
        if let Some(response) = self
            .try_serve_from_cache(&key, &meta, start, requested_end)
            .await?
        {
            return Ok(response);
        }

        // 2) 部分缓存命中 → 混合源。仅在已知总长度时可安全收敛开区间。
        if let Some(response) = self
            .try_serve_mixed(url, &key, &meta, start, requested_end)
            .await?
        {
            return Ok(response);
        }

        // 3) 完全走网络。同一区间的并发请求在这里合并：只有 leader 真的回源，
        //    其余等它写完缓存后走上面那两条缓存路径。
        match self.single_flight.join(&key, start, requested_end) {
            Join::Leader(guard) => {
                self.fetch_from_network(
                    url,
                    &key,
                    req.get_range(),
                    start,
                    requested_end,
                    Some(guard),
                )
                .await
            }
            Join::Follower(follower) => {
                self.serve_as_follower(follower, url, &key, req.get_range(), start, requested_end)
                    .await
            }
        }
    }

    /// follower 路径：等 leader 写完缓存，然后自己去读。
    ///
    /// 三级兜底，缺一不可——**等到 leader 结束不等于缓存里就有完整数据**。
    /// leader 可能回源失败、可能只写了个前缀（客户端提前断开、宽限期放弃缓存），
    /// 也可能超时还没写完。所以这里每一级都可能落空，最后必须有一条无条件
    /// 能走通的路：
    ///
    /// 1. 缓存完整命中 → 直接服务，这是合并生效时的正常结果；
    /// 2. 缓存部分命中 → 混合源，把 leader 写下的那段用上，只补剩下的；
    /// 3. 都不行 → 自己回源。
    ///
    /// 第 3 步**不再登记 single-flight**。此刻表里可能已经有下一轮的新 leader，
    /// 再 join 一次就又变成 follower，最坏情况是一路等下去。兜底必须是无条件的。
    async fn serve_as_follower(
        &self,
        follower: Follower,
        url: &str,
        key: &str,
        range: &str,
        start: u64,
        requested_end: u64,
    ) -> Result<Response<Body>> {
        if follower.wait().await {
            log_info!("Cache", "合并等待结束，改从缓存读取: {}", range);
        } else {
            log_info!("Cache", "合并等待超时，自行回源: {}", range);
        }

        // 必须重读元数据：leader 刚写进去的 total_size 是下面两条缓存路径
        // 收敛开区间的前提，用等待之前那份就还是「未知总长度」。
        let meta = self.cache_handler.upstream_meta(key).await?;

        if let Some(response) = self
            .try_serve_from_cache(key, &meta, start, requested_end)
            .await?
        {
            return Ok(response);
        }

        if let Some(response) = self
            .try_serve_mixed(url, key, &meta, start, requested_end)
            .await?
        {
            return Ok(response);
        }

        log_info!("Cache", "合并后缓存仍未命中，自行回源: {}", range);
        self.fetch_from_network(url, key, range, start, requested_end, None)
            .await
    }

    /// 请求区间已完整落盘时直接返回，不触碰网络。
    ///
    /// 旧实现即使全部命中也要向上游发一次 `bytes=0-0` 探针来取总长度，
    /// 导致上游故障时已缓存的内容也无法播放。现在总长度取自元数据。
    async fn try_serve_from_cache(
        &self,
        key: &str,
        meta: &UpstreamMeta,
        start: u64,
        requested_end: u64,
    ) -> Result<Option<Response<Body>>> {
        let Some(total_size) = meta.total_size else {
            return Ok(None);
        };

        // start 越界属于客户端错误，直接上抛（映射为 416），不要退化成网络请求。
        let (start, end) = resolve_range(start, requested_end, Some(total_size))?;

        if !self.cache_handler.check_range(key, (start, end)).await? {
            return Ok(None);
        }

        log_info!("Cache", "完全从缓存读取: {}-{}", start, end);
        let stream = self.cache_handler.read(key, (start, end)).await?;
        self.response_builder
            .build_partial_content_response(stream, headers_from_meta(meta), start, end, total_size)
            .map(Some)
    }

    /// 前半段在缓存、后半段需要网络时，走混合源拼接。
    async fn try_serve_mixed(
        &self,
        url: &str,
        key: &str,
        meta: &UpstreamMeta,
        start: u64,
        requested_end: u64,
    ) -> Result<Option<Response<Body>>> {
        let Some(total_size) = meta.total_size else {
            return Ok(None);
        };
        let (start, end) = resolve_range(start, requested_end, Some(total_size))?;

        let cached_size = self.cache_handler.get_size(key).await?.unwrap_or(0);
        if cached_size <= start {
            return Ok(None);
        }

        // cached_end 是缓存段的开区间右边界，与 MixedSourceHandler 的约定一致。
        let cached_end = cached_size.min(end.saturating_add(1));
        if cached_end <= start {
            return Ok(None);
        }

        // 稀疏文件里「文件长度」不代表字节已下载，必须查区间索引。
        if !self
            .cache_handler
            .check_range(key, (start, cached_end - 1))
            .await?
        {
            return Ok(None);
        }

        self.mixed_source_handler
            .handle(url, key, start, end, cached_end)
            .await
            .map(Some)
    }

    /// 从网络取整段。
    ///
    /// `guard` 是 single-flight 的 leader 凭证。它被移进 tee 的缓存写入任务，
    /// 活到 `write_stream` 返回为止——follower 被唤醒时缓存里才真的有数据。
    /// `None` 表示这一路不参与合并（follower 的兜底回源、混合源路径）。
    async fn fetch_from_network(
        &self,
        url: &str,
        key: &str,
        range: &str,
        start: u64,
        requested_end: u64,
        guard: Option<LeaderGuard>,
    ) -> Result<Response<Body>> {
        log_info!("Cache", "开始从网络获取: {}", range);
        let fetched = self.network_handler.fetch(url, range).await?;
        // 一次拆解全部按移动取出，不再克隆 HeaderMap 和 UpstreamMeta。
        // 流是惰性的（只是给 Body 套了一层错误映射），提前取出不会开始拉取
        // 上游数据，下面那次 await 期间它只是躺着。
        let (headers, meta, upstream) = fetched.into_parts();

        // 收敛开区间：u64::MAX 哨兵绝不能流入后续算术或响应头。
        let (start, end) = resolve_range(start, requested_end, meta.total_size)?;
        let total_size = meta.total_size.unwrap_or(0);

        // 持久化总长度与 Content-Type，让后续命中无需再探测上游。
        if let Err(error) = self.cache_handler.record_upstream_meta(key, &meta).await {
            log_info!("Cache", "记录上游元数据失败: {}", error);
        }

        // 上游响应体 tee 给客户端和缓存写入器，两侧互不影响。
        // guard 跟着进缓存写入任务：写完才算 leader 完成。
        let client_stream = tee_to_cache(
            Box::pin(upstream),
            self.cache_handler.clone(),
            key.to_string(),
            (start, end),
            guard,
            self.tasks.clone(),
        );

        self.response_builder.build_partial_content_response(
            Box::new(client_stream),
            headers,
            start,
            end,
            total_size,
        )
    }
}

/// 由持久化元数据重建回放给客户端的响应头。
fn headers_from_meta(meta: &UpstreamMeta) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(content_type) = meta
        .content_type
        .as_deref()
        .and_then(|value| value.parse().ok())
    {
        headers.insert(CONTENT_TYPE, content_type);
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::StorageEngine;
    use bytes::Bytes;
    use futures::stream;
    use hyper::header::{CONTENT_LENGTH, CONTENT_RANGE};

    fn request(url: &str, range: &str, asset: &str) -> DataRequest {
        let request = hyper::Request::builder()
            .uri("/proxy/media")
            .header("X-Original-Url", url)
            .header("X-Cache-Asset-Id", asset)
            .header("X-Cache-Asset-Revision", "1")
            .header(hyper::header::RANGE, range)
            .body(Body::empty())
            .unwrap();
        DataRequest::new(&request).unwrap()
    }

    fn disk(root: &std::path::Path) -> DiskStorage {
        DiskStorage::new(StorageConfig {
            root_path: root.to_path_buf(),
            chunk_size: 4,
        })
    }

    #[test]
    fn cached_response_headers_restore_only_valid_content_type() {
        let valid = headers_from_meta(&UpstreamMeta {
            total_size: Some(100),
            content_type: Some("audio/mp4".to_string()),
        });
        assert_eq!(valid[CONTENT_TYPE], "audio/mp4");

        let invalid = headers_from_meta(&UpstreamMeta {
            total_size: Some(100),
            content_type: Some("bad\r\nheader".to_string()),
        });
        assert!(!invalid.contains_key(CONTENT_TYPE));
    }

    #[tokio::test]
    async fn serves_closed_and_open_ranges_fully_offline_from_persisted_cache() {
        let dir = tempfile::tempdir().unwrap();
        let closed = request(
            "https://media.example/song.m4a?token=expired",
            "bytes=2-5",
            "song",
        );
        let key = closed.get_cache_key().unwrap();
        let storage = disk(dir.path());
        storage
            .write(
                key,
                stream::iter([Ok(Bytes::from_static(b"abcdefghij"))]),
                (0, 9),
            )
            .await
            .unwrap();
        storage
            .record_upstream_meta(
                key,
                &UpstreamMeta {
                    total_size: Some(10),
                    content_type: Some("audio/mp4".to_string()),
                },
            )
            .await
            .unwrap();

        // deny-all proves these responses cannot have fallen back to the upstream.
        let manager = DataSourceManager::new(dir.path().to_path_buf());
        let response = manager.process_request(&closed).await.unwrap();
        assert_eq!(response.headers()[CONTENT_RANGE], "bytes 2-5/10");
        assert_eq!(response.headers()[CONTENT_LENGTH], "4");
        assert_eq!(response.headers()[CONTENT_TYPE], "audio/mp4");
        assert_eq!(
            hyper::body::to_bytes(response.into_body()).await.unwrap(),
            "cdef"
        );

        let open = request(
            "https://media.example/song.m4a?token=refreshed",
            "bytes=6-",
            "song",
        );
        assert_eq!(open.get_cache_key().unwrap(), key);
        let response = manager.process_request(&open).await.unwrap();
        assert_eq!(response.headers()[CONTENT_RANGE], "bytes 6-9/10");
        assert_eq!(
            hyper::body::to_bytes(response.into_body()).await.unwrap(),
            "ghij"
        );
    }

    #[tokio::test]
    async fn sparse_file_never_becomes_an_offline_full_cache_hit() {
        let dir = tempfile::tempdir().unwrap();
        let request = request(
            "https://media.example/sparse.m4a?token=expired",
            "bytes=0-9",
            "sparse",
        );
        let key = request.get_cache_key().unwrap();
        let storage = disk(dir.path());
        storage
            .write(key, stream::iter([Ok(Bytes::from_static(b"ij"))]), (8, 9))
            .await
            .unwrap();
        storage
            .record_upstream_meta(
                key,
                &UpstreamMeta {
                    total_size: Some(10),
                    content_type: None,
                },
            )
            .await
            .unwrap();

        let manager = DataSourceManager::new(dir.path().to_path_buf());
        let error = manager.process_request(&request).await.unwrap_err();
        assert!(matches!(error, crate::utils::error::ProxyError::Request(_)));
    }
}
