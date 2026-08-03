use crate::utils::error::Result;
use bytes::Bytes;
use futures::Stream;
use std::path::PathBuf;

pub mod disk;
pub mod manager;

pub use disk::DiskStorage;
pub use manager::{StorageManager, StorageManagerConfig};

#[derive(Clone)]
pub struct StorageConfig {
    pub root_path: PathBuf,
    pub chunk_size: usize,
}

#[async_trait::async_trait]
pub trait StorageEngine: Send + Sync {
    async fn write<S>(&self, key: &str, stream: S, range: (u64, u64)) -> Result<u64>
    where
        S: Stream<Item = Result<Bytes>> + Send + Unpin + 'static;

    async fn read(
        &self,
        key: &str,
        range: (u64, u64),
    ) -> Result<Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin>>;

    async fn get_size(&self, key: &str) -> Result<Option<u64>>;

    async fn check_range(&self, key: &str, range: (u64, u64)) -> Result<bool>;

    async fn delete(&self, key: &str) -> Result<()>;

    /// 持久化上游元数据（总长度、Content-Type）。
    ///
    /// 缓存命中时用它替代「再向上游发一次 `bytes=0-0` 探针」取这些值，
    /// 否则上游不可达会让已完整缓存的请求整体失败。
    async fn record_upstream_meta(&self, key: &str, meta: &UpstreamMeta) -> Result<()>;

    /// 读取已持久化的上游元数据；字段未知时为 `None`。
    async fn upstream_meta(&self, key: &str) -> Result<UpstreamMeta>;

    /// 枚举已落盘的缓存条目，返回 `(key, 已占用字节数)`。
    ///
    /// [`StorageManager`] 启动时用它重建磁盘用量簿记。缺了这一步，重启后
    /// 管理器认为磁盘是空的，LRU 清理永远不触发，缓存目录无上界增长。
    ///
    /// 默认返回空表：不支持枚举的引擎退化成「重启后从零开始记账」，
    /// 行为与加这个方法之前一致。
    async fn enumerate(&self) -> Result<Vec<(String, u64)>> {
        Ok(Vec::new())
    }
}

/// 缓存命中时重建响应头所需的上游元数据。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpstreamMeta {
    /// 资源总长度，用于收敛开区间 range 和构建 `Content-Range`。
    pub total_size: Option<u64>,
    pub content_type: Option<String>,
}

impl UpstreamMeta {
    pub fn is_empty(&self) -> bool {
        self.total_size.is_none() && self.content_type.is_none()
    }
}
