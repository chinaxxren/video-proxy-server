use bytes::Bytes;
use futures_util::Stream;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{Mutex, Notify, RwLock};

use super::{StorageEngine, UpstreamMeta};
use crate::log_info;
use crate::utils::error::Result;

const MUTATION_LOCK_SHARDS: usize = 64;

#[derive(Clone)]
pub struct StorageManagerConfig {
    pub max_cache_size: u64,
    pub max_file_count: usize,
    pub cleanup_interval: Duration,
    pub external_cache_dirs: Vec<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::error::ProxyError;
    use tokio::sync::Notify;

    struct DeleteTrackingStorage {
        deleted: Arc<Notify>,
    }

    impl StorageEngine for DeleteTrackingStorage {
        async fn write<S>(&self, _key: &str, mut stream: S, _range: (u64, u64)) -> Result<u64>
        where
            S: Stream<Item = Result<Bytes>> + Send + Unpin + 'static,
        {
            let mut total = 0;
            while let Some(chunk) = futures_util::StreamExt::next(&mut stream).await {
                total += chunk?.len() as u64;
            }
            Ok(total)
        }

        async fn read(
            &self,
            _key: &str,
            _range: (u64, u64),
        ) -> Result<Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin>> {
            Err(ProxyError::Storage("not implemented in test".to_string()))
        }

        async fn get_size(&self, _key: &str) -> Result<Option<u64>> {
            Ok(None)
        }
        async fn check_range(&self, _key: &str, _range: (u64, u64)) -> Result<bool> {
            Ok(false)
        }
        async fn record_upstream_meta(&self, _key: &str, _meta: &UpstreamMeta) -> Result<()> {
            Ok(())
        }
        async fn upstream_meta(&self, _key: &str) -> Result<UpstreamMeta> {
            Ok(UpstreamMeta::default())
        }

        async fn delete(&self, _key: &str) -> Result<()> {
            self.deleted.notify_one();
            Ok(())
        }
    }

    #[tokio::test]
    async fn cleanup_uses_storage_delete() {
        let deleted = Arc::new(Notify::new());
        let manager = StorageManager::new(
            DeleteTrackingStorage {
                deleted: deleted.clone(),
            },
            StorageManagerConfig {
                max_cache_size: 0,
                max_file_count: 0,
                cleanup_interval: Duration::from_millis(5),
                external_cache_dirs: Vec::new(),
            },
        );
        manager
            .write(
                "asset",
                futures_util::stream::iter([Ok(Bytes::from_static(b"data"))]),
                (0, 3),
            )
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), deleted.notified())
            .await
            .expect("cleanup did not delete the cache entry");
    }

    #[tokio::test]
    async fn cleanup_counts_external_p2p_cache_toward_the_shared_budget() {
        let directory = tempfile::tempdir().unwrap();
        let p2p_piece = directory.path().join("manifest").join("0.piece");
        std::fs::create_dir_all(p2p_piece.parent().unwrap()).unwrap();
        std::fs::write(&p2p_piece, b"p2p").unwrap();
        let deleted = Arc::new(Notify::new());
        let manager = StorageManager::new(
            DeleteTrackingStorage {
                deleted: deleted.clone(),
            },
            StorageManagerConfig {
                max_cache_size: 4,
                max_file_count: 100,
                cleanup_interval: Duration::from_millis(5),
                external_cache_dirs: vec![directory.path().to_path_buf()],
            },
        );
        manager
            .write(
                "asset",
                futures_util::stream::iter([Ok(Bytes::from_static(b"http"))]),
                (0, 3),
            )
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), deleted.notified())
            .await
            .expect("cleanup did not count external P2P bytes");
    }

    #[tokio::test]
    async fn cleanup_does_not_evict_http_entries_when_p2p_alone_exceeds_budget() {
        let directory = tempfile::tempdir().unwrap();
        let p2p_piece = directory.path().join("manifest").join("0.piece");
        std::fs::create_dir_all(p2p_piece.parent().unwrap()).unwrap();
        std::fs::write(&p2p_piece, b"p2p-too-large").unwrap();
        let deleted = Arc::new(Notify::new());
        let manager = StorageManager::new(
            DeleteTrackingStorage {
                deleted: deleted.clone(),
            },
            StorageManagerConfig {
                max_cache_size: 4,
                max_file_count: 100,
                cleanup_interval: Duration::from_millis(5),
                external_cache_dirs: vec![directory.path().to_path_buf()],
            },
        );
        manager
            .write(
                "asset",
                futures_util::stream::iter([Ok(Bytes::from_static(b"http"))]),
                (0, 3),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), deleted.notified())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cleanup_evicts_http_entries_when_p2p_exactly_fills_budget() {
        let directory = tempfile::tempdir().unwrap();
        let p2p_piece = directory.path().join("manifest").join("0.piece");
        std::fs::create_dir_all(p2p_piece.parent().unwrap()).unwrap();
        std::fs::write(&p2p_piece, b"full").unwrap();
        let deleted = Arc::new(Notify::new());
        let manager = StorageManager::new(
            DeleteTrackingStorage {
                deleted: deleted.clone(),
            },
            StorageManagerConfig {
                max_cache_size: 4,
                max_file_count: 100,
                cleanup_interval: Duration::from_millis(5),
                external_cache_dirs: vec![directory.path().to_path_buf()],
            },
        );
        manager
            .write(
                "asset",
                futures_util::stream::iter([Ok(Bytes::from_static(b"http"))]),
                (0, 3),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), deleted.notified())
            .await
            .expect("cleanup did not enforce the combined budget");
    }

    struct CoordinatedStorage {
        data: Arc<Mutex<Vec<u8>>>,
        delete_started: Arc<Notify>,
        allow_delete: Arc<Notify>,
    }

    impl StorageEngine for CoordinatedStorage {
        async fn write<S>(&self, _key: &str, mut stream: S, _range: (u64, u64)) -> Result<u64>
        where
            S: Stream<Item = Result<Bytes>> + Send + Unpin + 'static,
        {
            let mut bytes = Vec::new();
            while let Some(chunk) = futures_util::StreamExt::next(&mut stream).await {
                bytes.extend_from_slice(&chunk?);
            }
            let len = bytes.len() as u64;
            *self.data.lock().await = bytes;
            Ok(len)
        }

        async fn read(
            &self,
            _key: &str,
            _range: (u64, u64),
        ) -> Result<Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin>> {
            let bytes = self.data.lock().await.clone();
            Ok(Box::new(futures_util::stream::iter([Ok(Bytes::from(
                bytes,
            ))])))
        }

        async fn get_size(&self, _key: &str) -> Result<Option<u64>> {
            Ok(None)
        }
        async fn check_range(&self, _key: &str, _range: (u64, u64)) -> Result<bool> {
            Ok(true)
        }
        async fn record_upstream_meta(&self, _key: &str, _meta: &UpstreamMeta) -> Result<()> {
            Ok(())
        }
        async fn upstream_meta(&self, _key: &str) -> Result<UpstreamMeta> {
            Ok(UpstreamMeta::default())
        }

        async fn delete(&self, _key: &str) -> Result<()> {
            self.delete_started.notify_one();
            self.allow_delete.notified().await;
            self.data.lock().await.clear();
            Ok(())
        }
    }

    #[tokio::test]
    async fn rewrite_waits_for_cleanup_delete_and_survives() {
        let data = Arc::new(Mutex::new(Vec::new()));
        let delete_started = Arc::new(Notify::new());
        let allow_delete = Arc::new(Notify::new());
        let manager = Arc::new(StorageManager::new(
            CoordinatedStorage {
                data: data.clone(),
                delete_started: delete_started.clone(),
                allow_delete: allow_delete.clone(),
            },
            StorageManagerConfig {
                max_cache_size: 0,
                max_file_count: 0,
                cleanup_interval: Duration::from_millis(5),
                external_cache_dirs: Vec::new(),
            },
        ));
        manager
            .write(
                "asset",
                futures_util::stream::iter([Ok(Bytes::from_static(b"old"))]),
                (0, 2),
            )
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), delete_started.notified())
            .await
            .expect("cleanup did not begin deletion");
        let writer = {
            let manager = manager.clone();
            tokio::spawn(async move {
                manager
                    .write(
                        "asset",
                        futures_util::stream::iter([Ok(Bytes::from_static(b"new"))]),
                        (0, 2),
                    )
                    .await
            })
        };
        tokio::task::yield_now().await;
        allow_delete.notify_one();
        writer.await.unwrap().unwrap();

        assert_eq!(&*data.lock().await, b"new");
    }

    /// 上一次运行留在盘上的条目，必须能被本次运行的 LRU 清理看到。
    struct PreexistingStorage {
        deleted: Arc<Mutex<Vec<String>>>,
        notify: Arc<Notify>,
    }

    impl StorageEngine for PreexistingStorage {
        async fn write<S>(&self, _key: &str, _stream: S, _range: (u64, u64)) -> Result<u64>
        where
            S: Stream<Item = Result<Bytes>> + Send + Unpin + 'static,
        {
            Ok(0)
        }

        async fn read(
            &self,
            _key: &str,
            _range: (u64, u64),
        ) -> Result<Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin>> {
            Err(ProxyError::Storage("not implemented in test".to_string()))
        }

        async fn get_size(&self, _key: &str) -> Result<Option<u64>> {
            Ok(None)
        }
        async fn check_range(&self, _key: &str, _range: (u64, u64)) -> Result<bool> {
            Ok(false)
        }
        async fn record_upstream_meta(&self, _key: &str, _meta: &UpstreamMeta) -> Result<()> {
            Ok(())
        }
        async fn upstream_meta(&self, _key: &str) -> Result<UpstreamMeta> {
            Ok(UpstreamMeta::default())
        }

        async fn delete(&self, key: &str) -> Result<()> {
            self.deleted.lock().await.push(key.to_string());
            self.notify.notify_one();
            Ok(())
        }

        async fn enumerate(&self) -> Result<Vec<(String, u64)>> {
            Ok(vec![("from-last-run".to_string(), 4096)])
        }
    }

    /// 重启后磁盘用量簿记必须重建，否则旧数据永不淘汰、缓存目录无上界。
    ///
    /// 断言方式：清理任务只认簿记里的条目。它能删掉 `from-last-run`，
    /// 就证明预热确实把这个磁盘上已存在的 key 记了进来。
    #[tokio::test]
    async fn warm_up_lets_cleanup_evict_entries_left_by_a_previous_run() {
        let deleted = Arc::new(Mutex::new(Vec::new()));
        let notify = Arc::new(Notify::new());
        let _manager = StorageManager::new(
            PreexistingStorage {
                deleted: deleted.clone(),
                notify: notify.clone(),
            },
            StorageManagerConfig {
                max_cache_size: 0,
                max_file_count: 0,
                cleanup_interval: Duration::from_millis(5),
                external_cache_dirs: Vec::new(),
            },
        );

        tokio::time::timeout(Duration::from_secs(1), notify.notified())
            .await
            .expect("预热没有把上次运行的条目记入簿记，清理任务看不到它");
        assert_eq!(&*deleted.lock().await, &["from-last-run".to_string()]);
    }
}

impl Default for StorageManagerConfig {
    fn default() -> Self {
        Self {
            max_cache_size: 1024 * 1024 * 1024, // 1GB
            max_file_count: 1000,
            cleanup_interval: Duration::from_secs(60),
            external_cache_dirs: Vec::new(),
        }
    }
}

async fn external_cache_size(directories: &[PathBuf]) -> u64 {
    let mut total = 0u64;
    let mut pending = directories.to_vec();
    while let Some(directory) = pending.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
            continue;
        };
        loop {
            let Ok(Some(entry)) = entries.next_entry().await else {
                break;
            };
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                if let Ok(metadata) = entry.metadata().await {
                    total = total.saturating_add(metadata.len());
                }
            }
        }
    }
    total
}

#[derive(Clone)]
struct CacheEntry {
    key: String,
    /// 该条目占用的磁盘空间（字节数），用于 LRU 淘汰时的容量记账。
    /// 注意：这是缓存引擎报告的实际磁盘占用，不是上游资源的 `Content-Length`。
    disk_usage: u64,
    last_access: SystemTime,
}

pub struct StorageManager<E> {
    engine: Arc<E>,
    config: StorageManagerConfig,
    cache_entries: Arc<RwLock<HashMap<String, CacheEntry>>>,
    total_size: Arc<RwLock<u64>>,
    mutation_locks: Arc<Vec<Mutex<()>>>,
    cleanup_shutdown: Arc<Notify>,
}

impl<E> Drop for StorageManager<E> {
    fn drop(&mut self) {
        // notify_one stores a permit if the task has not reached notified() yet,
        // so shutdown cannot be lost during construction or between timer ticks.
        self.cleanup_shutdown.notify_one();
    }
}

impl<E: StorageEngine + 'static> StorageManager<E> {
    pub fn new(engine: E, config: StorageManagerConfig) -> Self {
        let manager = Self {
            engine: Arc::new(engine),
            config,
            cache_entries: Arc::new(RwLock::new(HashMap::new())),
            total_size: Arc::new(RwLock::new(0)),
            mutation_locks: Arc::new((0..MUTATION_LOCK_SHARDS).map(|_| Mutex::new(())).collect()),
            cleanup_shutdown: Arc::new(Notify::new()),
        };

        // 先把磁盘上已有的条目读回簿记，再启动清理任务。
        manager.start_index_warm_up();
        manager.start_cleanup();
        manager
    }

    /// 后台重建磁盘用量簿记。
    ///
    /// 不这样做的话，重启后 `total_size` 从 0 开始，而磁盘上可能已经躺着
    /// 上一次运行留下的几个 GB：LRU 清理要等到本次运行**新写入**的量超过
    /// 上限才触发，旧数据永远不会被淘汰，缓存目录实际没有上界。
    ///
    /// 放后台而不是在 `new` 里 await：构造函数是同步的，而枚举要扫整个
    /// 缓存目录。预热期间的写入正常记账，两边都走同一把 `total_size` 写锁，
    /// 预热只补表里还没有的 key，不会重复计数。
    ///
    /// 取锁顺序与 `write` 和清理任务一致（先 `cache_entries` 后 `total_size`），
    /// 不能颠倒，否则与并发写入互相等待。
    fn start_index_warm_up(&self) {
        let engine = self.engine.clone();
        let cache_entries = self.cache_entries.clone();
        let total_size = self.total_size.clone();

        tokio::spawn(async move {
            let entries = match engine.enumerate().await {
                Ok(entries) => entries,
                Err(error) => {
                    log_info!("Cache", "重建缓存索引失败: {}", error);
                    return;
                }
            };
            if entries.is_empty() {
                return;
            }

            let count = entries.len();
            let mut table = cache_entries.write().await;
            let mut total = total_size.write().await;
            let mut recovered = 0u64;

            for (key, size) in entries {
                // 预热开始后写入的 key 已经记过账，不能再加一遍。
                if table.contains_key(&key) {
                    continue;
                }
                table.insert(
                    key.clone(),
                    CacheEntry {
                        key,
                        disk_usage: size,
                        // UNIX_EPOCH 让上一次运行留下的条目在 LRU 里排最前，
                        // 优先于本次运行访问过的条目被淘汰。
                        last_access: SystemTime::UNIX_EPOCH,
                    },
                );
                recovered = recovered.saturating_add(size);
            }
            *total = total.saturating_add(recovered);

            log_info!(
                "Cache",
                "缓存索引重建完成: {} 个条目, {} 字节",
                count,
                recovered
            );
        });
    }

    fn start_cleanup(&self) {
        let cache_entries = self.cache_entries.clone();
        let total_size = self.total_size.clone();
        let config = self.config.clone();
        let engine = self.engine.clone();
        let mutation_locks = self.mutation_locks.clone();
        let cleanup_shutdown = self.cleanup_shutdown.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(config.cleanup_interval) => {}
                    _ = cleanup_shutdown.notified() => break,
                }

                let external_size = external_cache_size(&config.external_cache_dirs).await;

                // 阶段一：持锁挑选淘汰对象。只读取簿记，不做 IO。
                let to_remove = {
                    let entries = cache_entries.read().await;
                    let total = *total_size.read().await;
                    let combined_total = total.saturating_add(external_size);

                    // P2P owns eviction of its files. If it alone exceeds the
                    // byte budget, do not destroy every HTTP entry trying to
                    // correct a condition this manager cannot fix.
                    if (external_size > config.max_cache_size
                        && entries.len() <= config.max_file_count)
                        || (combined_total <= config.max_cache_size
                            && entries.len() <= config.max_file_count)
                    {
                        continue;
                    }

                    // 按最后访问时间排序（LRU）
                    let mut entry_list: Vec<_> = entries.values().cloned().collect();
                    entry_list.sort_by_key(|entry| entry.last_access);

                    let mut current_total = combined_total;
                    let mut current_count = entries.len();
                    let mut victims = Vec::new();

                    for entry in entry_list {
                        if current_total <= config.max_cache_size
                            && current_count <= config.max_file_count
                        {
                            break;
                        }
                        current_total = current_total.saturating_sub(entry.disk_usage);
                        current_count -= 1;
                        victims.push(entry);
                    }
                    victims
                };

                // 阶段二：批量持锁验证并移除簿记，磁盘 IO 在锁外执行。
                //
                // 将所有淘汰操作合并到一次全局写锁获取，避免每个条目都单独获取锁。
                // mutation_locks 仍按条目单独持有，因为它保护的是该 key 的并发写入
                // 竞争（写入任务与清理任务之间），而全局锁保护的是簿记数据结构本身。
                let verified_victims = {
                    let mut entries = cache_entries.write().await;
                    let mut total = total_size.write().await;
                    let mut to_delete = Vec::new();

                    for entry in to_remove {
                        let shard = mutation_shard(&entry.key);
                        let _mutation = mutation_locks[shard].lock().await;

                        // 选中后若被读取或重写，last_access 会变化；该快照已经过期，
                        // 不能删除刚写入的新内容。
                        let unchanged = entries
                            .get(&entry.key)
                            .map(|current| current.last_access == entry.last_access)
                            .unwrap_or(false);

                        if unchanged {
                            // 提前移除簿记：后续磁盘操作无论成败，该条目都不应再被读取。
                            if let Some(removed) = entries.remove(&entry.key) {
                                *total = total.saturating_sub(removed.disk_usage);
                                to_delete.push(entry.key.clone());
                            }
                        }
                    }
                    to_delete
                };

                // 锁已释放，磁盘 IO 不阻塞并发读写。
                for key in verified_victims {
                    if let Err(e) = engine.delete(&key).await {
                        log_info!("Cache", "清理缓存条目失败 {}: {}", key, e);
                    }
                }
            }
        });
    }

    // Keeping the Send bound explicit lets callers move this future into spawned tasks.
    #[allow(clippy::manual_async_fn)]
    pub fn write<'a, S>(
        &'a self,
        key: &'a str,
        stream: S,
        range: (u64, u64),
    ) -> impl std::future::Future<Output = Result<u64>> + Send + 'a
    where
        S: Stream<Item = Result<Bytes>> + Send + Unpin + 'static,
    {
        async move {
            let _mutation = self.mutation_locks[mutation_shard(key)].lock().await;
            let bytes_written = self.engine.write(key, stream, range).await?;

            // 更新缓存信息
            let mut entries = self.cache_entries.write().await;
            let mut total = self.total_size.write().await;

            let end_pos = range.0.saturating_add(bytes_written);

            if let Some(entry) = entries.get_mut(key) {
                // 更新文件的总大小（如果新写入的范围扩展了文件）
                if end_pos > entry.disk_usage {
                    // saturating：记账漂移时宁可低估，不要在减法上 panic。
                    *total = total
                        .saturating_sub(entry.disk_usage)
                        .saturating_add(end_pos);
                    entry.disk_usage = end_pos;
                }
                entry.last_access = SystemTime::now();
            } else {
                entries.insert(
                    key.to_string(),
                    CacheEntry {
                        key: key.to_string(),
                        disk_usage: end_pos,
                        last_access: SystemTime::now(),
                    },
                );
                // saturating 与上面的分支保持一致：记账漂移不该让写入路径 panic。
                *total = total.saturating_add(end_pos);
            }

            Ok(bytes_written)
        }
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
            return Ok(Some(entry.disk_usage));
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

fn mutation_shard(key: &str) -> usize {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() as usize) % MUTATION_LOCK_SHARDS
}
