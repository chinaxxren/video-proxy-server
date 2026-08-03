use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::Stream;
use md5;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::SeekFrom;
use std::io::{self};
use std::path::{Path, PathBuf};
use tokio::fs as tokio_fs;
use tokio::io as tokio_io;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, RwLock};

use super::{StorageConfig, StorageEngine, UpstreamMeta};
use crate::log_info;
use crate::utils::error::{ProxyError, Result};
use crate::utils::range::{range_length, OPEN_ENDED};

/// 按 key 哈希分片的写锁数量。固定容量避免锁表随 key 无界增长。
const WRITE_LOCK_SHARDS: usize = 64;

/// 写入缓冲大小。上游块通常远小于此值，攒满一批再下发一次 write syscall。
const WRITE_BUFFER_BYTES: usize = 256 * 1024;

/// 元数据读缓存的条目上限。
///
/// 这是纯读缓存，淘汰任何条目都不影响正确性——下次访问重新读盘即可。
/// 加上限只是防止「请求大量互不相同的 URL」把索引撑大。
const MAX_INDEXED_KEYS: usize = 4096;

pub struct DiskStorage {
    config: StorageConfig,
    /// 按 key 分片的元数据锁，保护 ranges.json 的读-改-写。
    ///
    /// 原先是一把全局 `Mutex<()>`：不同资源之间毫无关系，却要互相排队。
    /// 高并发下每次分片下载结束都要抢这把锁，而锁内还有两次文件 IO
    /// （读 + 原子写），排队长度直接等于并发分片数。
    metadata_locks: Vec<Mutex<()>>,
    /// 序列化同一 key 的并发写。元数据锁只保护 ranges.json，
    /// 不保护数据文件本身；缺了这层，两个请求会在同一偏移交错写入。
    write_locks: Vec<Mutex<()>>,
    /// 已解析元数据的内存缓存。
    ///
    /// 一次缓存命中原先要读、解析 ranges.json 三到四次：`upstream_meta`
    /// 一次、`check_range` 一次、`read` 内部再 `check_range` 一次，混合源
    /// 路径上还要多一次。每次都是一趟 syscall 加一次 JSON 反序列化，而内容
    /// 完全相同。
    ///
    /// 所有写路径都是「先落盘、再更新内存」，所以内存永远不会比磁盘新。
    /// 前提是**本进程是该缓存目录的唯一写者**——这个前提本来就已经成立，
    /// 因为 `write_locks` 也只在进程内生效。外部改动 ranges.json 要等到
    /// 条目被淘汰或进程重启才会被看见。
    metadata_index: RwLock<HashMap<String, RangeMetadata>>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct RangeMetadata {
    completed: Vec<(u64, u64)>,
    /// 上游声明的资源总长度。`serde(default)` 保证旧元数据文件仍可读。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total_size: Option<u64>,
    /// 上游声明的 Content-Type，缓存命中时回放给客户端。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
}

impl DiskStorage {
    pub fn new(config: StorageConfig) -> Self {
        Self {
            config,
            metadata_locks: (0..WRITE_LOCK_SHARDS).map(|_| Mutex::new(())).collect(),
            write_locks: (0..WRITE_LOCK_SHARDS).map(|_| Mutex::new(())).collect(),
            metadata_index: RwLock::new(HashMap::new()),
        }
    }

    fn shard_of(key: &str) -> usize {
        let digest = md5::compute(key.as_bytes());
        usize::from(digest.0[0]) % WRITE_LOCK_SHARDS
    }

    fn write_lock_for(&self, key: &str) -> &Mutex<()> {
        &self.write_locks[Self::shard_of(key)]
    }

    /// 元数据锁与写锁是两套独立的锁表，不能合并。
    ///
    /// 写锁在整个流式写入期间持有（可能是几分钟），而 `record_completed_range`
    /// 是在写锁内部调用的。tokio 的 Mutex 不可重入，共用一张表必然自锁。
    fn metadata_lock_for(&self, key: &str) -> &Mutex<()> {
        &self.metadata_locks[Self::shard_of(key)]
    }

    fn get_metadata_path(&self, key: &str) -> PathBuf {
        self.get_file_path(key).with_extension("ranges.json")
    }

    /// 在元数据上求值，命中内存索引就不碰磁盘。
    ///
    /// 传闭包而不是返回 `RangeMetadata`：绝大多数调用方只要判断一个谓词或
    /// 取一个字段，克隆整个 `completed` 向量纯属浪费。
    async fn with_metadata<T>(&self, key: &str, view: impl FnOnce(&RangeMetadata) -> T) -> T {
        if let Some(metadata) = self.metadata_index.read().await.get(key) {
            return view(metadata);
        }

        let Some(metadata) = self.load_metadata(key).await else {
            // 没有元数据文件的 key 不进索引：否则请求大量不存在的 URL
            // 就能用空条目把索引填满，把真正有用的条目挤出去。
            return view(&RangeMetadata::default());
        };

        let result = view(&metadata);
        self.index_insert(key.to_string(), metadata).await;
        result
    }

    /// 读并解析元数据文件。文件不存在或内容损坏都返回 `None`。
    async fn load_metadata(&self, key: &str) -> Option<RangeMetadata> {
        let data = tokio_fs::read(self.get_metadata_path(key)).await.ok()?;
        serde_json::from_slice(&data).ok()
    }

    /// 读-改-写元数据的唯一入口，持分片锁串行执行。
    ///
    /// 落盘成功后才更新内存索引：写盘失败时内存不能领先于磁盘，否则
    /// 后续请求会认为某段已缓存，而磁盘上的 ranges.json 并不这么记。
    async fn update_metadata(
        &self,
        key: &str,
        edit: impl FnOnce(&mut RangeMetadata) -> bool,
    ) -> Result<()> {
        let _guard = self.metadata_lock_for(key).lock().await;

        let mut metadata = match self.metadata_index.read().await.get(key) {
            Some(cached) => cached.clone(),
            None => self.load_metadata(key).await.unwrap_or_default(),
        };

        if !edit(&mut metadata) {
            return Ok(());
        }

        self.write_metadata(key, &metadata).await?;
        self.index_insert(key.to_string(), metadata).await;
        Ok(())
    }

    async fn index_insert(&self, key: String, metadata: RangeMetadata) {
        let mut index = self.metadata_index.write().await;
        if index.len() >= MAX_INDEXED_KEYS && !index.contains_key(&key) {
            // 纯读缓存，淘汰谁都不影响正确性，所以不必维护 LRU 时间戳——
            // 取任意一个条目丢掉即可。
            if let Some(victim) = index.keys().next().cloned() {
                index.remove(&victim);
            }
        }
        index.insert(key, metadata);
    }

    async fn record_completed_range(&self, key: &str, start: u64, end: u64) -> Result<()> {
        self.update_metadata(key, |metadata| {
            merge_range(&mut metadata.completed, start, end);
            true
        })
        .await
    }

    /// 原子写入元数据：先写 .tmp 再 rename。rename 失败时清掉 .tmp，避免残留。
    async fn write_metadata(&self, key: &str, metadata: &RangeMetadata) -> Result<()> {
        let path = self.get_metadata_path(key);

        // 必须自己建目录。元数据文件和数据文件同属一个二级哈希目录，而这个
        // 目录原先只由 `write` 建：冷缓存下 `record_upstream_meta` 先于任何
        // `write` 执行，写 .tmp 直接 ENOENT，错误被调用方记一行日志吞掉。
        // 结果是 total_size 要等到该 URL 的第二次请求才落盘，而「上游不可达时
        // 仍能服务已缓存内容」这条路径恰好依赖它——冷取一次之后并不生效。
        self.ensure_dir_exists(&path).await?;

        let temporary = path.with_extension("ranges.json.tmp");
        let bytes = serde_json::to_vec(metadata)?;

        if let Err(error) = tokio_fs::write(&temporary, bytes).await {
            let _ = tokio_fs::remove_file(&temporary).await;
            return Err(error.into());
        }
        if let Err(error) = tokio_fs::rename(&temporary, &path).await {
            let _ = tokio_fs::remove_file(&temporary).await;
            return Err(error.into());
        }
        Ok(())
    }

    /// 数据文件长度，不存在时为 `None`。用异步 stat，不用 `Path::exists`。
    ///
    /// `Path::exists` 是同步阻塞 syscall，出现在 async 函数里会占住 worker
    /// 线程；这个文件里其余 IO 早就全换成 tokio::fs 了。而且它只回答有无，
    /// 调用方往往紧接着还要长度，等于白跑一次 syscall。
    async fn data_file_size(&self, key: &str) -> Option<u64> {
        tokio_fs::metadata(self.get_file_path(key))
            .await
            .ok()
            .map(|metadata| metadata.len())
    }

    fn get_file_path(&self, key: &str) -> PathBuf {
        // 使用MD5生成URL的哈希值
        let hash = format!("{:x}", md5::compute(key.as_bytes()));

        // 创建二级目录结构，使用哈希的前两个字符
        let dir1 = &hash[0..2];
        let dir2 = &hash[2..4];

        // 构建完整的文件路径
        self.config.root_path.join(dir1).join(dir2).join(hash)
    }

    /// 建好数据文件的父目录。
    ///
    /// 不再先 `parent.exists()` 再 `create_dir_all`：前者是同步阻塞 syscall，
    /// 而 `create_dir_all` 本身对已存在的目录就是无操作，那次探测纯属多余。
    async fn ensure_dir_exists(&self, path: &Path) -> io::Result<()> {
        match path.parent() {
            Some(parent) => tokio_fs::create_dir_all(parent).await,
            None => Ok(()),
        }
    }
}

/// 把 `[start, end]` 并入一张已排序、已合并、互不相邻的区间表。
///
/// 原实现是「push + 整表排序 + 从头重新合并」，每记录一次区间就是
/// O(n log n) 加一次全表重建。这里利用「表已有序」这个不变式，二分找到
/// 插入位置后只做一次线性 splice。
fn merge_range(ranges: &mut Vec<(u64, u64)>, start: u64, end: u64) {
    // 第一个右端点够不到 start 的区间就是可能与新区间接壤的起点。
    // 用 saturating_add 是因为 `[0,9]` 与 `[10,19]` 相邻，应当合并成 `[0,19]`。
    let first = ranges.partition_point(|range| range.1.saturating_add(1) < start);
    // 第一个左端点越过 end 的区间之后的都碰不到了。
    let last = ranges.partition_point(|range| range.0 <= end.saturating_add(1));

    // first == last 表示没有区间与新区间接壤，splice 退化为纯插入。
    let (merged_start, merged_end) = match ranges.get(first..last).filter(|slice| !slice.is_empty())
    {
        Some(touching) => (
            touching[0].0.min(start),
            touching.iter().fold(end, |widest, range| widest.max(range.1)),
        ),
        None => (start, end),
    };

    ranges.splice(first..last, [(merged_start, merged_end)]);
}

#[async_trait]
impl StorageEngine for DiskStorage {
    async fn write<S>(&self, key: &str, mut stream: S, range: (u64, u64)) -> Result<u64>
    where
        S: Stream<Item = Result<Bytes>> + Send + Unpin + 'static,
    {
        let max_length = if range.1 == OPEN_ENDED {
            None
        } else {
            Some(range_length(range.0, range.1)?)
        };
        let file_path = self.get_file_path(key);
        self.ensure_dir_exists(&file_path).await?;

        // 同一 key 的写必须串行：否则两个请求会在重叠偏移上交错写入，
        // 而 record_completed_range 只按字节数记账，会把交错结果标记为完整。
        let _write_guard = self.write_lock_for(key).lock().await;

        log_info!(
            "Storage",
            "写入文件: {:?}, 范围: {}-{}",
            file_path,
            range.0,
            range.1
        );

        let mut file = tokio_fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // 绝不能 truncate：这是一个稀疏文件，不同 range 会在不同偏移上
            // 陆续写入。截断会抹掉此前已缓存的区间，而 ranges.json 仍然记着
            // 它们「已完整」，后续读取就会拿到一片零字节。
            .truncate(false)
            .open(&file_path)
            .await?;

        // 设置文件写入位置。必须在包 BufWriter 之前 seek：BufWriter 自己不
        // 转发 seek，包上之后再 seek 会把已缓冲但未落盘的字节写到错误偏移。
        file.seek(SeekFrom::Start(range.0)).await?;

        // 攒够 WRITE_BUFFER_BYTES 再下一次 write syscall。
        //
        // 上游给的块由 TLS 记录和 socket 读缓冲决定，通常十几 KB，此前每块
        // 直接 write_all 一次 —— 1GB 视频就是六万多次 write syscall，而每次
        // 只搬十几 KB。缓冲之后按 256KB 成批下发，syscall 数量降一个量级。
        let mut writer = tokio_io::BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);

        let mut written = 0u64;
        while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
            let chunk = chunk?;
            let next_written = written
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| ProxyError::Storage("写入字节数溢出".to_string()))?;
            if max_length.is_some_and(|maximum| next_written > maximum) {
                return Err(ProxyError::Storage(
                    "上游响应体超过请求的缓存范围".to_string(),
                ));
            }
            writer.write_all(&chunk).await?;
            written = next_written;
        }

        // flush 把缓冲交给内核，sync_data 才是让内核落盘。两步都不能少，
        // 且必须都在 record_completed_range 之前 —— 否则元数据可能先于数据
        // 落盘，崩溃后区间被标记完整而内容还是空洞。
        writer.flush().await?;
        writer.into_inner().sync_data().await?;
        if written > 0 {
            let end = range
                .0
                .checked_add(written - 1)
                .ok_or_else(|| ProxyError::Storage("写入范围溢出".to_string()))?;
            self.record_completed_range(key, range.0, end).await?;
        }
        log_info!(
            "Storage",
            "写入完成: {:?}, 写入字节数: {}",
            file_path,
            written
        );

        Ok(written)
    }

    async fn read(
        &self,
        key: &str,
        range: (u64, u64),
    ) -> Result<Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin>> {
        let file_path = self.get_file_path(key);

        if !self.check_range(key, range).await? {
            return Err(ProxyError::Storage("请求范围尚未完整缓存".to_string()));
        }

        log_info!(
            "Storage",
            "读取文件: {:?}, 范围: {}-{}",
            file_path,
            range.0,
            range.1
        );

        // 全程使用 tokio::fs：此前用的是 std::fs，同步 seek/read 会阻塞
        // async worker 线程，每个缓存读都在饿死 runtime。
        //
        // 开文件前不再单独 `exists()` 探一次：open 本身就会因文件不存在而
        // 失败，那次同步 stat 既阻塞线程又是纯冗余的。
        let mut file = tokio_fs::File::open(&file_path).await?;
        let file_size = file.metadata().await?.len();

        if range.0 >= file_size {
            return Err(ProxyError::Storage("请求范围超出文件大小".to_string()));
        }

        // file_size >= 1，上面的越界检查已排除空文件，故减一不会下溢。
        let last_byte = file_size - 1;
        let end = if range.1 == OPEN_ENDED {
            last_byte
        } else {
            std::cmp::min(range.1, last_byte)
        };

        let total_bytes = range_length(range.0, end)?;
        log_info!(
            "Storage",
            "需要读取的总字节数: {} (范围: {}-{})",
            total_bytes,
            range.0,
            end
        );

        let chunk_size = self.config.chunk_size;
        file.seek(SeekFrom::Start(range.0)).await?;

        // 只在开头 seek 一次，之后顺序读取。
        let stream = Box::pin(futures::stream::try_unfold(
            (file, chunk_size, 0u64, total_bytes),
            |(mut file, chunk_size, mut bytes_read, total_bytes)| async move {
                if bytes_read >= total_bytes {
                    return Ok(None);
                }

                let remaining = total_bytes - bytes_read;
                let to_read = std::cmp::min(chunk_size as u64, remaining) as usize;

                // BytesMut + read_buf，而不是 `vec![0; to_read]` + read。
                //
                // `vec![0; n]` 会先把整块内存写一遍零，紧接着被 read 完全覆盖，
                // 这遍 memset 纯属浪费；块越大越明显。read_buf 直接写进未初始化
                // 的备用容量，最后 freeze 成 Bytes 也不再拷贝一次。
                let mut buffer = BytesMut::with_capacity(to_read);
                // take 限制这一轮最多读 to_read 字节：read_buf 会尽量填满备用
                // 容量，而 with_capacity 给出的容量可能大于 to_read，不限制就会
                // 越过请求区间的右边界。
                let n = (&mut file).take(to_read as u64).read_buf(&mut buffer).await?;
                if n == 0 {
                    // check_range 已承诺该区间完整，读到 EOF 说明数据文件被
                    // 外部截断。静默返回 None 会让响应体短于 Content-Length，
                    // 因此显式报错。
                    return Err(ProxyError::Storage(format!(
                        "缓存文件意外截断: 已读 {}/{} 字节",
                        bytes_read, total_bytes
                    )));
                }

                bytes_read += n as u64;

                Ok(Some((
                    buffer.freeze(),
                    (file, chunk_size, bytes_read, total_bytes),
                )))
            },
        ));

        Ok(Box::new(stream))
    }

    async fn get_size(&self, key: &str) -> Result<Option<u64>> {
        // 一次 stat 直接给出长度，不必先 `exists()` 再 `metadata()`——
        // 后者失败本身就说明文件不在。
        Ok(self.data_file_size(key).await)
    }

    async fn check_range(&self, key: &str, range: (u64, u64)) -> Result<bool> {
        let requested_end = if range.1 == OPEN_ENDED {
            // 开区间：优先用已知总长度收敛，否则退回文件长度。
            let known_total = self.with_metadata(key, |metadata| metadata.total_size).await;
            let known_end = match known_total {
                Some(total) => total.checked_sub(1),
                None => match self.data_file_size(key).await {
                    Some(size) => size.checked_sub(1),
                    None => return Ok(false),
                },
            };
            match known_end {
                Some(end) => end,
                None => return Ok(false),
            }
        } else {
            range.1
        };

        let covered = self
            .with_metadata(key, |metadata| {
                metadata
                    .completed
                    .iter()
                    .any(|completed| completed.0 <= range.0 && completed.1 >= requested_end)
            })
            .await;

        if !covered {
            return Ok(false);
        }

        // 只在「元数据说已缓存」时才确认数据文件还在。元数据存在而数据文件
        // 被外部删掉时，返回 true 会让上层走缓存读并以 500 结束，而不是退回
        // 网络。放在这里而不是函数开头，是为了让缓存未命中的冷路径完全不碰
        // 这次 stat——冷路径才是量最大的那条。
        Ok(self.data_file_size(key).await.is_some())
    }

    async fn record_upstream_meta(&self, key: &str, meta: &UpstreamMeta) -> Result<()> {
        if meta.total_size.is_none() && meta.content_type.is_none() {
            return Ok(());
        }

        // 返回值即「是否有变化」：没变化时 update_metadata 会跳过落盘。
        // 上游元数据每个分片请求都会记一次，而其中绝大多数是重复值，
        // 这个短路省掉的是同一份 JSON 的反复重写。
        self.update_metadata(key, |metadata| {
            let mut changed = false;
            if let Some(total_size) = meta.total_size {
                if total_size > 0 && metadata.total_size != Some(total_size) {
                    metadata.total_size = Some(total_size);
                    changed = true;
                }
            }
            if let Some(content_type) = meta.content_type.as_deref() {
                if metadata.content_type.as_deref() != Some(content_type) {
                    metadata.content_type = Some(content_type.to_string());
                    changed = true;
                }
            }
            changed
        })
        .await
    }

    async fn upstream_meta(&self, key: &str) -> Result<UpstreamMeta> {
        Ok(self
            .with_metadata(key, |metadata| UpstreamMeta {
                total_size: metadata.total_size,
                content_type: metadata.content_type.clone(),
            })
            .await)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let _guard = self.metadata_lock_for(key).lock().await;

        // 先摘掉内存索引再删文件。反过来的话，两者之间的窗口里有请求进来，
        // 会从索引里读到「已缓存」，而文件已经没了。
        self.metadata_index.write().await.remove(key);

        for path in [self.get_file_path(key), self.get_metadata_path(key)] {
            match tokio_fs::remove_file(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{stream, StreamExt};

    fn storage(root: &Path) -> DiskStorage {
        DiskStorage::new(StorageConfig {
            root_path: root.to_path_buf(),
            chunk_size: 4,
        })
    }

    #[tokio::test]
    async fn sparse_file_holes_are_not_reported_as_cached() {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage(dir.path());
        storage
            .write(
                "asset",
                stream::iter([Ok(Bytes::from_static(b"tail"))]),
                (8, 11),
            )
            .await
            .unwrap();

        assert!(!storage.check_range("asset", (0, 3)).await.unwrap());
        assert!(storage.check_range("asset", (8, 11)).await.unwrap());
    }

    #[tokio::test]
    async fn completed_ranges_survive_reopening_and_merge() {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            root_path: dir.path().to_path_buf(),
            chunk_size: 4,
        };
        let storage = DiskStorage::new(config.clone());
        storage
            .write(
                "asset",
                stream::iter([Ok(Bytes::from_static(b"abcd"))]),
                (0, 3),
            )
            .await
            .unwrap();
        storage
            .write(
                "asset",
                stream::iter([Ok(Bytes::from_static(b"efgh"))]),
                (4, 7),
            )
            .await
            .unwrap();

        let reopened = DiskStorage::new(config);
        assert!(reopened.check_range("asset", (0, 7)).await.unwrap());
    }

    #[tokio::test]
    async fn delete_removes_data_and_range_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage(dir.path());
        storage
            .write(
                "asset",
                stream::iter([Ok(Bytes::from_static(b"data"))]),
                (0, 3),
            )
            .await
            .unwrap();
        storage.delete("asset").await.unwrap();
        storage.delete("asset").await.unwrap();

        assert_eq!(storage.get_size("asset").await.unwrap(), None);
        assert!(!storage.check_range("asset", (0, 3)).await.unwrap());
    }

    #[tokio::test]
    async fn reads_exact_cached_bytes_across_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage(dir.path());
        storage
            .write(
                "asset",
                stream::iter([Ok(Bytes::from_static(b"abcdefghij"))]),
                (0, 9),
            )
            .await
            .unwrap();

        let mut stream = storage.read("asset", (2, 8)).await.unwrap();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(bytes, b"cdefghi");
        assert!(storage.check_range("asset", (0, u64::MAX)).await.unwrap());
    }

    #[tokio::test]
    async fn failed_stream_does_not_mark_partial_write_complete() {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage(dir.path());
        let stream = stream::iter([
            Ok(Bytes::from_static(b"partial")),
            Err(ProxyError::Network("connection lost".to_string())),
        ]);

        assert!(storage.write("asset", stream, (0, 99)).await.is_err());
        assert!(!storage.check_range("asset", (0, 6)).await.unwrap());
        assert!(storage.read("asset", (0, 6)).await.is_err());
    }

    #[tokio::test]
    async fn corrupt_metadata_is_treated_as_a_cache_miss() {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            root_path: dir.path().to_path_buf(),
            chunk_size: 4,
        };
        let storage = DiskStorage::new(config.clone());
        storage
            .write(
                "asset",
                stream::iter([Ok(Bytes::from_static(b"data"))]),
                (0, 3),
            )
            .await
            .unwrap();
        tokio_fs::write(storage.get_metadata_path("asset"), b"not-json")
            .await
            .unwrap();

        // 必须换一个实例来查。损坏是外部改动，而内存索引以「本进程是唯一
        // 写者」为前提，只在条目未被缓存时才回读磁盘——也就是进程重启后。
        // 用原实例查等于在检验它会不会自我怀疑刚写成功的东西，那不是这里
        // 要保证的性质。
        let reopened = DiskStorage::new(config);
        assert!(!reopened.check_range("asset", (0, 3)).await.unwrap());
        assert!(reopened.read("asset", (0, 3)).await.is_err());
    }

    #[tokio::test]
    async fn merge_range_keeps_the_table_sorted_disjoint_and_non_adjacent() {
        let mut ranges = Vec::new();

        // 相邻区间合并成一段。
        merge_range(&mut ranges, 0, 9);
        merge_range(&mut ranges, 10, 19);
        assert_eq!(ranges, vec![(0, 19)]);

        // 有空洞时保持两段，且按 offset 有序。
        merge_range(&mut ranges, 100, 149);
        assert_eq!(ranges, vec![(0, 19), (100, 149)]);

        // 乱序插入落到正确位置。
        merge_range(&mut ranges, 40, 49);
        assert_eq!(ranges, vec![(0, 19), (40, 49), (100, 149)]);

        // 跨越多段的区间把它们全并掉。
        merge_range(&mut ranges, 15, 120);
        assert_eq!(ranges, vec![(0, 149)]);

        // 被完全包含的区间不改变结果。
        merge_range(&mut ranges, 50, 60);
        assert_eq!(ranges, vec![(0, 149)]);

        // OPEN_ENDED 参与合并时 end+1 不会溢出。
        merge_range(&mut ranges, 200, u64::MAX);
        assert_eq!(ranges, vec![(0, 149), (200, u64::MAX)]);
    }

    #[tokio::test]
    async fn metadata_index_serves_repeat_lookups_after_the_file_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage(dir.path());
        storage
            .record_upstream_meta(
                "asset",
                &UpstreamMeta {
                    total_size: Some(100),
                    content_type: Some("video/mp4".to_string()),
                },
            )
            .await
            .unwrap();

        // 删掉 ranges.json 后仍能拿到元数据，说明这次查询没有落盘 IO。
        tokio_fs::remove_file(storage.get_metadata_path("asset"))
            .await
            .unwrap();
        assert_eq!(
            storage.upstream_meta("asset").await.unwrap(),
            UpstreamMeta {
                total_size: Some(100),
                content_type: Some("video/mp4".to_string()),
            }
        );
    }

    #[tokio::test]
    async fn upstream_meta_persists_before_any_data_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            root_path: dir.path().to_path_buf(),
            chunk_size: 4,
        };
        let storage = DiskStorage::new(config.clone());

        // 冷缓存下先记元数据、后写数据是正常顺序：DataSourceManager 拿到上游
        // 响应头就先 record_upstream_meta，数据要等流式转发完才落盘。此前
        // write_metadata 不建父目录，这一步必然 ENOENT 失败，总长度和
        // Content-Type 就永远存不下来——上游一旦不可达，已缓存内容也放不了。
        storage
            .record_upstream_meta(
                "asset",
                &UpstreamMeta {
                    total_size: Some(100),
                    content_type: Some("video/mp4".to_string()),
                },
            )
            .await
            .unwrap();

        // 换一个实例读，确认是真落了盘而不是只在内存索引里。
        let reopened = DiskStorage::new(config);
        assert_eq!(
            reopened.upstream_meta("asset").await.unwrap(),
            UpstreamMeta {
                total_size: Some(100),
                content_type: Some("video/mp4".to_string()),
            }
        );
    }

    #[tokio::test]
    async fn delete_drops_the_cached_metadata_too() {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage(dir.path());
        storage
            .write(
                "asset",
                stream::iter([Ok(Bytes::from_static(b"data"))]),
                (0, 3),
            )
            .await
            .unwrap();
        storage.delete("asset").await.unwrap();

        // 内存索引若没跟着清掉，这里会拿旧的 completed 表报告已缓存。
        assert!(!storage.check_range("asset", (0, 3)).await.unwrap());
        assert!(storage.upstream_meta("asset").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn externally_deleted_data_file_is_not_reported_as_cached() {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage(dir.path());
        storage
            .write(
                "asset",
                stream::iter([Ok(Bytes::from_static(b"data"))]),
                (0, 3),
            )
            .await
            .unwrap();

        // 只删数据文件、留下元数据：check_range 必须报未命中，让上层退回
        // 网络，而不是走缓存读然后以 500 结束。
        tokio_fs::remove_file(storage.get_file_path("asset"))
            .await
            .unwrap();
        assert!(!storage.check_range("asset", (0, 3)).await.unwrap());
    }

    #[tokio::test]
    async fn concurrent_disjoint_writes_preserve_both_completed_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let storage = std::sync::Arc::new(storage(dir.path()));
        let first = {
            let storage = storage.clone();
            tokio::spawn(async move {
                storage
                    .write(
                        "asset",
                        stream::iter([Ok(Bytes::from_static(b"abcd"))]),
                        (0, 3),
                    )
                    .await
            })
        };
        let second = {
            let storage = storage.clone();
            tokio::spawn(async move {
                storage
                    .write(
                        "asset",
                        stream::iter([Ok(Bytes::from_static(b"efgh"))]),
                        (4, 7),
                    )
                    .await
            })
        };

        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert!(storage.check_range("asset", (0, 7)).await.unwrap());

        let mut stream = storage.read("asset", (0, 7)).await.unwrap();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(bytes, b"abcdefgh");
    }

    #[tokio::test]
    async fn write_rejects_bytes_beyond_declared_closed_range() {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage(dir.path());

        let result = storage
            .write(
                "asset",
                stream::iter([Ok(Bytes::from_static(b"sixsix"))]),
                (10, 14),
            )
            .await;

        assert!(result.is_err());
        assert!(!storage.check_range("asset", (10, 14)).await.unwrap());
        assert_eq!(storage.get_size("asset").await.unwrap(), Some(0));
    }

    #[tokio::test]
    async fn write_accepts_short_body_as_only_the_completed_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage(dir.path());
        storage
            .write(
                "asset",
                stream::iter([Ok(Bytes::from_static(b"abc"))]),
                (10, 19),
            )
            .await
            .unwrap();

        assert!(storage.check_range("asset", (10, 12)).await.unwrap());
        assert!(!storage.check_range("asset", (10, 19)).await.unwrap());
    }
}
