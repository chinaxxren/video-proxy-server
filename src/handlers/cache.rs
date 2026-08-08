use crate::log_info;
use crate::storage::{DiskStorage, StorageManager, UpstreamMeta};
use crate::utils::error::Result;
use bytes::Bytes;
use futures_util::Stream;
use std::pin::Pin;
use std::sync::Arc;

pub struct CacheHandler {
    storage_manager: Arc<StorageManager<DiskStorage>>,
}

impl CacheHandler {
    pub fn new(storage_manager: Arc<StorageManager<DiskStorage>>) -> Self {
        Self { storage_manager }
    }

    pub async fn check_range(&self, key: &str, range: (u64, u64)) -> Result<bool> {
        self.storage_manager.check_range(key, range).await
    }

    pub async fn get_size(&self, key: &str) -> Result<Option<u64>> {
        self.storage_manager.get_size(key).await
    }

    pub async fn read(
        &self,
        key: &str,
        range: (u64, u64),
    ) -> Result<Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin>> {
        self.storage_manager.read(key, range).await
    }

    pub async fn record_upstream_meta(&self, key: &str, meta: &UpstreamMeta) -> Result<()> {
        self.storage_manager.record_upstream_meta(key, meta).await
    }

    pub async fn upstream_meta(&self, key: &str) -> Result<UpstreamMeta> {
        self.storage_manager.upstream_meta(key).await
    }

    /// 把上游数据流写入缓存。
    ///
    /// 直接把流交给存储引擎：引擎内部边收边写，结束时只记一次区间。
    /// 旧实现在这里按 64KB 重新分块，每块都调一次 `write`，于是每 64KB
    /// 就触发一次「打开文件 + fsync + 全量重写 ranges.json」——1GB 视频
    /// 约 16000 次 fsync，且中途失败会留下一串已标记完成的碎片区间。
    pub fn write_stream(
        self: Arc<Self>,
        key: String,
        range: (u64, u64),
        stream: Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>> {
        Box::pin(async move {
            let written = self.storage_manager.write(&key, stream, range).await?;
            log_info!(
                "Cache",
                "缓存写入完成: {} 字节 (起始偏移 {})",
                written,
                range.0
            );
            Ok(())
        })
    }
}
