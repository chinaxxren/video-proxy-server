use crate::data_request::DataRequest;
use crate::handlers::{
    tee_to_cache, CacheHandler, MixedSourceHandler, NetworkHandler, ResponseBuilder,
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
        let mixed_source_handler = MixedSourceHandler::new(cache_handler.clone(), policy);
        let response_builder = ResponseBuilder::new();

        Self {
            cache_handler,
            network_handler,
            mixed_source_handler,
            response_builder,
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

        // 3) 完全走网络。
        self.fetch_from_network(url, &key, req.get_range(), start, requested_end)
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

    async fn fetch_from_network(
        &self,
        url: &str,
        key: &str,
        range: &str,
        start: u64,
        requested_end: u64,
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
        let client_stream = tee_to_cache(
            Box::pin(upstream),
            self.cache_handler.clone(),
            key.to_string(),
            (start, end),
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
