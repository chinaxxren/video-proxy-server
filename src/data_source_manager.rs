use crate::data_request::DataRequest;
use crate::handlers::{
    tee_to_cache, BackgroundTasks, CacheHandler, Follower, Join, LeaderGuard, MixedSourceHandler,
    NetworkHandler, ResponseBuilder, SingleFlight,
};
use crate::http_types::{empty_body, AppBody};
use crate::log_info;
use crate::storage::{
    DiskStorage, StorageConfig, StorageManager, StorageManagerConfig, UpstreamMeta,
};
use crate::utils::error::Result;
use crate::utils::network_policy::NetworkPolicy;
use crate::utils::range::{
    clamp_end_to_upstream_length, format_range, parse_range_spec, range_length, resolve_range,
    RangeSpec,
};
use hyper::header::{HeaderMap, CONTENT_TYPE};
use hyper::{Response, StatusCode};
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
        let mixed_source_handler =
            MixedSourceHandler::with_tasks(cache_handler.clone(), policy, tasks.clone());
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

    pub async fn process_request(&self, req: &DataRequest) -> Result<Response<AppBody>> {
        let url = req.get_url();
        let key = req.get_cache_key().to_string();
        let spec = parse_range_spec(req.get_range())?;

        log_info!("Cache", "开始处理请求范围: {}", req.get_range());

        // 元数据只读一次，供以下两条缓存路径共用。
        let mut meta = self.cache_handler.upstream_meta(&key).await?;

        // 后缀范围（`bytes=-N`）的起点由总长度算出来，缺了总长度无从定位。这是
        // 唯一在解析阶段就需要总长度的形态，因此只在这一种情况下多探一次上游，
        // 其余请求的路径开销不变。
        if matches!(spec, RangeSpec::Suffix { .. }) && meta.total_size.is_none() {
            meta = self.probe_upstream_meta(url, &key).await?;
        }

        let (start, requested_end) = spec.endpoints(meta.total_size)?;

        // 归一化后的范围串。后缀形态到此已被消解，下游（含 `NetSource`）只会
        // 看到 `bytes=start-end` 或 `bytes=start-`，不必再懂后缀语义。
        let normalized = format_range(start, requested_end);
        let range = normalized.as_str();

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
                self.fetch_from_network(url, &key, range, start, requested_end, Some(guard))
                    .await
            }
            Join::Follower(follower) => {
                self.serve_as_follower(follower, url, &key, range, start, requested_end)
                    .await
            }
        }
    }

    /// Build a HEAD response without starting the normal tee/cache body pipeline.
    pub async fn process_head(&self, req: &DataRequest) -> Result<Response<AppBody>> {
        let key = req.get_cache_key().to_string();
        let mut meta = self.cache_handler.upstream_meta(&key).await?;

        if meta.total_size.is_none() {
            meta = self.probe_upstream_meta(req.get_url(), &key).await?;
        }

        let total = meta.total_size.ok_or_else(|| {
            crate::utils::error::ProxyError::InvalidRange("上游未提供资源总长度".to_string())
        })?;
        let (status, content_length, content_range) = if req.client_sent_range() {
            // 后缀范围在这里也要先借总长度定位；HEAD 已经拿到了 total，
            // 直接复用，不必再探一次上游。
            let (start, end) = parse_range_spec(req.get_range())?.endpoints(Some(total))?;
            let (start, end) = resolve_range(start, end, Some(total))?;
            (
                StatusCode::PARTIAL_CONTENT,
                range_length(start, end)?,
                Some(format!("bytes {}-{}/{}", start, end, total)),
            )
        } else {
            (StatusCode::OK, total, None)
        };

        let mut builder = Response::builder()
            .status(status)
            .header(hyper::header::ACCEPT_RANGES, "bytes")
            .header(hyper::header::CONTENT_LENGTH, content_length);
        if let Some(content_type) = meta.content_type {
            builder = builder.header(hyper::header::CONTENT_TYPE, content_type);
        }
        if let Some(content_range) = content_range {
            builder = builder.header(hyper::header::CONTENT_RANGE, content_range);
        }
        builder
            .body(empty_body())
            .map_err(|error| crate::utils::error::ProxyError::Request(error.to_string()))
    }

    /// 用一个最小范围请求探出上游的总长度与内容类型，并落盘。
    ///
    /// `bytes=0-0` 只取一个字节，代价接近一次 HEAD，但比 HEAD 可靠：不少上游
    /// （尤其是签名 URL 的 CDN）对 HEAD 回 403 或 405，却能正常响应范围 GET。
    /// 总长度从 206 的 `Content-Range` 尾段取得。
    ///
    /// 探测结果写回缓存元数据，因此同一资源后续请求不会重复探测。
    async fn probe_upstream_meta(&self, url: &str, key: &str) -> Result<UpstreamMeta> {
        let fetched = self.network_handler.fetch(url, "bytes=0-0").await?;
        let meta = fetched.meta.clone();
        // 显式丢弃：那一个字节的响应体没有用处，但必须先释放连接。
        drop(fetched);
        self.cache_handler.record_upstream_meta(key, &meta).await?;
        Ok(meta)
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
    ) -> Result<Response<AppBody>> {
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
    ) -> Result<Option<Response<AppBody>>> {
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
    ) -> Result<Option<Response<AppBody>>> {
        let Some(total_size) = meta.total_size else {
            return Ok(None);
        };
        let (start, end) = resolve_range(start, requested_end, Some(total_size))?;

        let cached_size = self.cache_handler.get_size(key).await?.unwrap_or(0);

        // cached_end 是缓存段的开区间右边界，与 MixedSourceHandler 的约定一致。
        let cached_end = match plan_mixed(start, end, cached_size) {
            MixedPlan::NoCache => return Ok(None),
            // 整段都已落盘，这是纯缓存路径的活。它在本次请求里刚跑过一次并
            // 落空，说明那时区间索引还缺字节——但之后后台的缓存写入可能正好
            // 落盘了。重试一次即可，它自己会再查一遍区间索引。
            MixedPlan::WholeRangeCached => {
                return self
                    .try_serve_from_cache(key, meta, start, requested_end)
                    .await
            }
            MixedPlan::Stitch { cached_end } => cached_end,
        };

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
    ) -> Result<Response<AppBody>> {
        log_info!("Cache", "开始从网络获取: {}", range);
        let fetched = self.network_handler.fetch(url, range).await?;
        // 上游实际声明的本次响应体长度，必须在拆解之前取出。
        let upstream_length = fetched.content_length;
        // 一次拆解全部按移动取出，不再克隆 HeaderMap 和 UpstreamMeta。
        // 流是惰性的（只是给 Body 套了一层错误映射），提前取出不会开始拉取
        // 上游数据，下面那次 await 期间它只是躺着。
        let (headers, meta, upstream) = fetched.into_parts();

        // 收敛开区间：u64::MAX 哨兵绝不能流入后续算术或响应头。
        let (start, end) = resolve_range(start, requested_end, meta.total_size)?;

        // 上游只给了请求区间的一个前缀时按实际长度收窄，否则 `Content-Length`
        // 会按请求长度发出而响应体更短，客户端读到一半撞 EOF。详见该函数注释。
        // 缓存侧也用收窄后的区间记账，不会把没拿到的字节标记成已缓存。
        let end = clamp_end_to_upstream_length(start, end, upstream_length);
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

/// 缓存前缀与请求区间的三种位置关系。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MixedPlan {
    /// 缓存没盖到 `start`，混合源无从下手。
    NoCache,
    /// 缓存前缀盖满了整个请求区间，属于纯缓存路径。
    WholeRangeCached,
    /// 前半段在缓存、后半段要回源。`cached_end` 是缓存段的**开区间**右边界。
    Stitch { cached_end: u64 },
}

/// 决定混合源怎么切这一刀。
///
/// 抽成纯函数是为了让下面那个不变量能被确定性地测到：它原先只在一个竞态
/// 窗口里被违反，而竞态没法稳定复现。
///
/// **不变量**：`Stitch` 的 `cached_end` 必须满足 `start < cached_end <= end`，
/// 这是 [`MixedSourceHandler::handle`] 的前置条件，破了它就直接 416。
///
/// 原先这里写的是 `cached_size.min(end + 1)`，当磁盘上的文件长度 ≥ 请求终点
/// 时它正好取到 `end + 1`，越界一个字节。触发路径很窄但真实存在：
/// `process_request` 先试纯缓存路径，那时区间索引还缺字节所以落空；紧接着
/// 走到这里时，后台的缓存写入刚好落盘，于是「整段都在缓存里」——本该由纯
/// 缓存路径处理的情形漏到了混合源，然后撞上守卫。表现是一个本该 206 的
/// 请求返回 416，且只在调度更容易撞上这个窗口的机器上出现。
fn plan_mixed(start: u64, end: u64, cached_size: u64) -> MixedPlan {
    if cached_size <= start {
        return MixedPlan::NoCache;
    }
    // cached_size > end 即 cached_size >= end + 1：字节 0..=end 全在缓存里，
    // 没有留给网络段的余地，再往下切就会得出 cached_end > end。
    if cached_size > end {
        return MixedPlan::WholeRangeCached;
    }
    MixedPlan::Stitch {
        cached_end: cached_size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::StorageEngine;
    use bytes::Bytes;
    use futures_util::stream;
    use http_body_util::BodyExt;

    fn request(url: &str, range: &str, asset: &str) -> DataRequest {
        let request = hyper::Request::builder()
            .uri("/proxy/media")
            .header("X-Original-Url", url)
            .header("X-Cache-Asset-Id", asset)
            .header("X-Cache-Asset-Revision", "1")
            .header("Range", range)
            .body(())
            .unwrap();
        DataRequest::new(&request).unwrap()
    }

    fn disk(root: &std::path::Path) -> DiskStorage {
        DiskStorage::new(StorageConfig {
            root_path: root.to_path_buf(),
            chunk_size: 4,
        })
    }

    /// 回归：`cached_end` 绝不能越过 `end`。
    ///
    /// 旧实现是 `cached_size.min(end + 1)`，整段都已落盘时它正好等于
    /// `end + 1`，而 `MixedSourceHandler::handle` 的守卫要求
    /// `cached_end <= end`，于是一个本该完全命中缓存的请求以 416 收场。
    ///
    /// 这条 bug 只在竞态下可达（详见 [`plan_mixed`] 的注释），压测复现不了，
    /// 所以把判定抽成纯函数、在这里确定性地压住不变量。
    #[test]
    fn mixed_plan_never_lets_cached_end_exceed_end() {
        // 整段已落盘：交给纯缓存路径，不能构造出越界的 cached_end。
        assert_eq!(plan_mixed(0, 199_999, 200_000), MixedPlan::WholeRangeCached);
        // 磁盘比请求区间还长，同样属于「整段已落盘」。
        assert_eq!(plan_mixed(0, 199_999, 300_007), MixedPlan::WholeRangeCached);
        // 恰好差最后一个字节：这才是真正需要拼接的形态。
        assert_eq!(
            plan_mixed(0, 199_999, 199_999),
            MixedPlan::Stitch {
                cached_end: 199_999
            }
        );

        // 穷举一遍：任何 Stitch 的 cached_end 都必须落在 (start, end] 内。
        for cached_size in 0..40u64 {
            for start in 0..12u64 {
                for end in start..20u64 {
                    if let MixedPlan::Stitch { cached_end } = plan_mixed(start, end, cached_size) {
                        assert!(
                            cached_end > start && cached_end <= end,
                            "start={start} end={end} cached_size={cached_size} \
                             产出越界的 cached_end={cached_end}"
                        );
                    }
                }
            }
        }
    }

    /// 缓存没覆盖到起点时不该走混合源。
    #[test]
    fn mixed_plan_skips_when_cache_does_not_reach_start() {
        assert_eq!(plan_mixed(100, 199, 0), MixedPlan::NoCache);
        assert_eq!(plan_mixed(100, 199, 100), MixedPlan::NoCache);
        assert_eq!(
            plan_mixed(100, 199, 101),
            MixedPlan::Stitch { cached_end: 101 }
        );
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
        let key = closed.get_cache_key();
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
        assert_eq!(response.headers()["content-range"], "bytes 2-5/10");
        assert_eq!(response.headers()["content-length"], "4");
        assert_eq!(response.headers()["content-type"], "audio/mp4");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "cdef"
        );

        let open = request(
            "https://media.example/song.m4a?token=refreshed",
            "bytes=6-",
            "song",
        );
        assert_eq!(open.get_cache_key(), key);
        let response = manager.process_request(&open).await.unwrap();
        assert_eq!(response.headers()["content-range"], "bytes 6-9/10");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
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
        let key = request.get_cache_key();
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
