use bytes::Bytes;
use futures::Stream;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

use super::{StorageEngine, UpstreamMeta};
use crate::log_info;
use crate::utils::error::Result;

#[derive(Clone)]
pub struct StorageManagerConfig {
    pub max_cache_size: u64,
    pub max_file_count: usize,
    pub cleanup_interval: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::error::ProxyError;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct DeleteTrackingStorage {
        deleted: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl StorageEngine for DeleteTrackingStorage {
        async fn write<S>(&self, _key: &str, mut stream: S, _range: (u64, u64)) -> Result<u64>
        where
            S: Stream<Item = Result<Bytes>> + Send + Unpin + 'static,
        {
            let mut total = 0;
            while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
                total += chunk?.len() as u64;
            }
            Ok(total)
        }

        async fn read(&self, _key: &str, _range: (u64, u64)) -> Result<Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin>> {
            Err(ProxyError::Storage("not implemented in test".to_string()))
        }

        async fn get_size(&self, _key: &str) -> Result<Option<u64>> { Ok(None) }
        async fn check_range(&self, _key: &str, _range: (u64, u64)) -> Result<bool> { Ok(false) }
        async fn record_upstream_meta(&self, _key: &str, _meta: &UpstreamMeta) -> Result<()> { Ok(()) }
        async fn upstream_meta(&self, _key: &str) -> Result<UpstreamMeta> { Ok(UpstreamMeta::default()) }

        async fn delete(&self, _key: &str) -> Result<()> {
            self.deleted.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn cleanup_uses_storage_delete() {
        let deleted = Arc::new(AtomicBool::new(false));
        let manager = StorageManager::new(
            DeleteTrackingStorage { deleted: deleted.clone() },
            StorageManagerConfig {
                max_cache_size: 0,
                max_file_count: 0,
                cleanup_interval: Duration::from_millis(5),
            },
        );
        manager.write("asset", futures::stream::iter([Ok(Bytes::from_static(b"data"))]), (0, 3)).await.unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            while !deleted.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        }).await.expect("cleanup did not delete the cache entry");
    }
}

impl Default for StorageManagerConfig {
    fn default() -> Self {
        Self {
            max_cache_size: 1024 * 1024 * 1024, // 1GB
            max_file_count: 1000,
            cleanup_interval: Duration::from_secs(60),
        }
    }
}

#[derive(Clone)]
struct CacheEntry {
    key: String,
    total_size: u64, // 文件的总大小
    last_access: SystemTime,
}

pub struct StorageManager<E> {
    engine: Arc<E>,
    config: StorageManagerConfig,
    cache_entries: Arc<RwLock<HashMap<String, CacheEntry>>>,
    total_size: Arc<RwLock<u64>>,
}

impl<E: StorageEngine + 'static> StorageManager<E> {
    pub fn new(engine: E, config: StorageManagerConfig) -> Self {
        let manager = Self {
            engine: Arc::new(engine),
            config,
            cache_entries: Arc::new(RwLock::new(HashMap::new())),
            total_size: Arc::new(RwLock::new(0)),
        };

        // 启动清理任务
        manager.start_cleanup();
        manager
    }

    fn start_cleanup(&self) {
        let cache_entries = self.cache_entries.clone();
        let total_size = self.total_size.clone();
        let config = self.config.clone();
        let engine = self.engine.clone();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(config.cleanup_interval).await;

                // 阶段一：持锁挑选淘汰对象。只读取簿记，不做 IO。
                let to_remove = {
                    let entries = cache_entries.read().await;
                    let total = *total_size.read().await;

                    if total <= config.max_cache_size && entries.len() <= config.max_file_count {
                        continue;
                    }

                    // 按最后访问时间排序（LRU）
                    let mut entry_list: Vec<_> = entries.values().cloned().collect();
                    entry_list.sort_by(|a, b| a.last_access.cmp(&b.last_access));

                    let mut current_total = total;
                    let mut current_count = entries.len();
                    let mut victims = Vec::new();

                    for entry in entry_list {
                        if current_total <= config.max_cache_size
                            && current_count <= config.max_file_count
                        {
                            break;
                        }
                        current_total = current_total.saturating_sub(entry.total_size);
                        current_count -= 1;
                        victims.push(entry);
                    }
                    victims
                };

                // 阶段二：不持锁做删除 IO。tokio 的 RwLock 是写优先的，
                // 持写锁跨 await 会让清理期间所有请求路径的读操作排队。
                let mut deleted = Vec::new();
                for entry in to_remove {
                    match engine.delete(&entry.key).await {
                        Ok(()) => deleted.push(entry.key),
                        Err(e) => log_info!("Cache", "清理缓存条目失败 {}: {}", entry.key, e),
                    }
                }

                // 阶段三：重新持锁更新簿记。此时才读取条目的当前大小，
                // 避免用阶段一的快照值去减一个可能已被并发写扩展过的条目。
                if !deleted.is_empty() {
                    let mut entries = cache_entries.write().await;
                    let mut total = total_size.write().await;
                    for key in deleted {
                        if let Some(removed) = entries.remove(&key) {
                            *total = total.saturating_sub(removed.total_size);
                        }
                    }
                }
            }
        });
    }

    pub async fn write<S>(&self, key: &str, stream: S, range: (u64, u64)) -> Result<u64>
    where
        S: Stream<Item = Result<Bytes>> + Send + Unpin + 'static,
    {
        let bytes_written = self.engine.write(key, stream, range).await?;

        // 更新缓存信息
        let mut entries = self.cache_entries.write().await;
        let mut total = self.total_size.write().await;

        let end_pos = range.0.saturating_add(bytes_written);

        if let Some(entry) = entries.get_mut(key) {
            // 更新文件的总大小（如果新写入的范围扩展了文件）
            if end_pos > entry.total_size {
                // saturating：记账漂移时宁可低估，不要在减法上 panic。
                *total = total.saturating_sub(entry.total_size).saturating_add(end_pos);
                entry.total_size = end_pos;
            }
            entry.last_access = SystemTime::now();
        } else {
            entries.insert(
                key.to_string(),
                CacheEntry {
                    key: key.to_string(),
                    total_size: end_pos,
                    last_access: SystemTime::now(),
                },
            );
            // saturating 与上面的分支保持一致：记账漂移不该让写入路径 panic。
            *total = total.saturating_add(end_pos);
        }

        Ok(bytes_written)
    }

    pub async fn read(
        &self,
        key: &str,
        range: (u64, u64),
    ) -> Result<Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin>> {
        // 更新访问时间
        if let Some(entry) = self.cache_entries.write().await.get_mut(key) {
            entry.last_access = SystemTime::now();
        }

        // 读取数据
        self.engine.read(key, range).await
    }

    pub async fn get_size(&self, key: &str) -> Result<Option<u64>> {
        // 从缓存条目中获取大小
        if let Some(entry) = self.cache_entries.read().await.get(key) {
            return Ok(Some(entry.total_size));
        }

        // 如果缓存中没有，从存储引擎获取
        self.engine.get_size(key).await
    }

    pub async fn check_range(&self, key: &str, range: (u64, u64)) -> Result<bool> {
        // 文件长度无法证明稀疏文件中的字节已下载，始终查询持久化区间索引。
        self.engine.check_range(key, range).await
    }

    pub async fn record_upstream_meta(&self, key: &str, meta: &UpstreamMeta) -> Result<()> {
        self.engine.record_upstream_meta(key, meta).await
    }

    /// 资源总长度。持久化于区间元数据，使缓存命中时无需再向上游探测长度。
    pub async fn upstream_meta(&self, key: &str) -> Result<UpstreamMeta> {
        self.engine.upstream_meta(key).await
    }
}
