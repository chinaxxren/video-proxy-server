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
    /// 该元数据所属的缓存键。
    ///
    /// 文件路径是 key 的 MD5，不可逆，所以启动扫描无法从路径反推 key，
    /// 而淘汰时要拿 key 去调 `delete`。`serde(default)` 让本字段出现之前
    /// 写下的元数据仍能解析——它们只是不会进入启动簿记，下次写入时补上。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
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

        // 每次落盘都盖上 key。这是 `enumerate` 唯一的反查依据——路径里只有
        // MD5，摘要不可逆。旧元数据文件没有这个字段，重启枚举时会被跳过，
        // 等它下次被写入时补上。
        if metadata.key.as_deref() != Some(key) {
            metadata.key = Some(key.to_string());
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
            touching
                .iter()
                .fold(end, |widest, range| widest.max(range.1)),
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

        // 提前中止的原因，留到 flush + 记账之后再上抛。
        //
        // 之所以不直接 `return Err(..)`：`BufWriter` 每攒满 256KB 就下发一次
        // 真实 write，所以中止时前面的字节**已经在文件里了**。直接返回会跳过
        // `record_completed_range`，那些字节就成了幽灵——占着磁盘、计入容量
        // 簿记（`enumerate` 按文件长度统计）、却不在 ranges.json 里，于是永远
        // 不会被任何读命中，只能等整条缓存被淘汰时一起消失。
        let mut failure = None;

        // 中止时 `written` 这个前缀能不能记账。三条中止路径的答案并不相同，
        // 所以逐条判断，不做统一处理：
        //
        // - **超出声明范围**：能记。检查发生在写这一块**之前**，`written` 是
        //   精确值；而这些字节正是上游 `Content-Range` 承诺的那一段（该头已由
        //   `net_source` 校验过），tee 也已经把它们发给客户端了。记下来既准确，
        //   又让缓存和客户端拿到的内容保持一致。
        // - **`write_all` 失败**：不能记。`write_all` 出错时不会告诉你写进去了
        //   多少字节，`written` 此刻是个高估值。记账就等于把可能并不存在的
        //   字节标记成已缓存，后续读会拿到空洞。
        // - **上游流报错**：保留前缀。此处 `written` 是精确的，已拿到的数据都是
        //   有效的，配合网络层重试可以让缓存边界逐步前进。分段下载场景下，丢弃
        //   已下载的部分既浪费带宽也让用户体验变差。
        let mut prefix_is_recordable = true;

        while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    failure = Some(error);
                    // 流错误时 written 是准确的，保留前缀以便下次从断点继续。
                    break;
                }
            };
            let Some(next_written) = written.checked_add(chunk.len() as u64) else {
                failure = Some(ProxyError::Storage("写入字节数溢出".to_string()));
                prefix_is_recordable = false;
                break;
            };
            if max_length.is_some_and(|maximum| next_written > maximum) {
                failure = Some(ProxyError::Storage(
                    "上游响应体超过请求的缓存范围".to_string(),
                ));
                break;
            }
            if let Err(error) = writer.write_all(&chunk).await {
                failure = Some(error.into());
                prefix_is_recordable = false;
                break;
            }
            written = next_written;
        }

        let mut sync_failure = None;

        if written > 0 && prefix_is_recordable {
            // flush 把缓冲交给内核，sync_data 才是让内核落盘。两步都不能少，
            // 且必须都在 record_completed_range 之前 —— 否则元数据可能先于数据
            // 落盘，崩溃后区间被标记完整而内容还是空洞。
            //
            // `?` 换成显式处理，是为了不让 flush 自己的错误盖掉 `failure` 里那个
            // 更能说明问题的原因。
            match writer.flush().await {
                Ok(()) => {
                    if let Err(error) = writer.into_inner().sync_data().await {
                        sync_failure = Some(ProxyError::from(error));
                    }
                }
                Err(error) => sync_failure = Some(ProxyError::from(error)),
            }

            // 落盘失败时绝不能记账：那会把可能并不在磁盘上的字节标记成已缓存。
            if sync_failure.is_none() {
                let end = range
                    .0
                    .checked_add(written - 1)
                    .ok_or_else(|| ProxyError::Storage("写入范围溢出".to_string()))?;
                self.record_completed_range(key, range.0, end).await?;
            }
        }
        // 不记账的中止路径上连 flush 都不做，直接把 writer 丢掉。
        //
        // tokio 的 `BufWriter` 在 drop 时不会（也无法）异步 flush，所以缓冲里
        // 那最多 256KB 还没下发的字节就随之丢弃了——既然不会记账，写进文件
        // 只是白占磁盘。循环中途按 256KB 已经下发过的部分收不回来，那是
        // 缓冲写入固有的代价；它们会在下一次对同一区间的成功写入中被覆盖并
        // 正确记账，不会永久留着。

        if let Some(error) = failure.or(sync_failure) {
            log_info!(
                "Storage",
                "写入中止: {:?}, 已落盘并记账 {} 字节, 原因: {}",
                file_path,
                written,
                error
            );
            return Err(error);
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
                let n = (&mut file)
                    .take(to_read as u64)
                    .read_buf(&mut buffer)
                    .await?;
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
            let known_total = self
                .with_metadata(key, |metadata| metadata.total_size)
                .await;
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
        let _write_guard = self.write_lock_for(key).lock().await;
        let _metadata_guard = self.metadata_lock_for(key).lock().await;

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

    /// 扫两层哈希目录，从每个 `*.ranges.json` 里取回 key 和数据文件长度。
    ///
    /// 目录结构是 `<root>/<hash[0..2]>/<hash[2..4]>/<hash>`，哈希不可逆，
    /// 所以 key 只能从元数据文件内部读回来（`RangeMetadata::key`）。
    /// 该字段是后加的，旧缓存文件里没有——这类条目无法归属到某个 key，
    /// 直接跳过：宁可少算一点用量，也不能编一个错的 key 出来，那会让
    /// 清理任务去删一个不存在的条目、真正的旧文件却永远留在盘上。
    async fn enumerate(&self) -> Result<Vec<(String, u64)>> {
        let mut entries = Vec::new();

        // 根目录不存在（冷启动、还没写过任何缓存）不是错误。
        let mut level1 = match tokio_fs::read_dir(&self.config.root_path).await {
            Ok(dir) => dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(entries),
            Err(error) => return Err(error.into()),
        };

        while let Some(first) = level1.next_entry().await? {
            if !first.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let mut level2 = match tokio_fs::read_dir(first.path()).await {
                Ok(dir) => dir,
                Err(_) => continue,
            };

            while let Some(second) = level2.next_entry().await? {
                if !second
                    .file_type()
                    .await
                    .map(|t| t.is_dir())
                    .unwrap_or(false)
                {
                    continue;
                }
                let mut files = match tokio_fs::read_dir(second.path()).await {
                    Ok(dir) => dir,
                    Err(_) => continue,
                };

                while let Some(file) = files.next_entry().await? {
                    let path = file.path();
                    // 只认已 rename 到位的元数据文件，.tmp 是写入中途的残留。
                    if !path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(|name| name.ends_with(".ranges.json"))
                        .unwrap_or(false)
                    {
                        continue;
                    }

                    let Some(metadata) = tokio_fs::read(&path)
                        .await
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<RangeMetadata>(&bytes).ok())
                    else {
                        continue;
                    };
                    let Some(key) = metadata.key else { continue };

                    // 用数据文件长度而不是 completed 的最大右端点：管理器的
                    // 簿记记的就是「最高写入偏移」，稀疏文件下两者一致，
                    // 而元数据可能记着尚未落盘的区间。
                    let size = self.data_file_size(&key).await.unwrap_or(0);
                    entries.push((key, size));
                }
            }
        }

        Ok(entries)
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

    /// 上游流报错时，已写入的前缀应当被保留并记账。
    ///
    /// 配合网络层重试，让缓存边界逐步前进：第一次写了 3MB 然后断了，第二次
    /// 从 3MB 继续、又写了 2MB 断了，第三次从 5MB 继续写完。每次都丢弃的话，
    /// 三次重试每次都从 0 开始，既浪费带宽也让响应时间变长。
    #[tokio::test]
    async fn failed_stream_still_records_the_prefix_received_before_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage(dir.path());
        let stream = stream::iter([
            Ok(Bytes::from_static(b"partial")),
            Err(ProxyError::Network("connection lost".to_string())),
        ]);

        assert!(storage.write("asset", stream, (0, 99)).await.is_err());
        // 前 7 字节（"partial"）已落盘并记账。
        assert!(storage.check_range("asset", (0, 6)).await.unwrap());
        let mut cached = storage.read("asset", (0, 6)).await.unwrap();
        let mut bytes = Vec::new();
        while let Some(chunk) = cached.next().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(bytes, b"partial");
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

    /// 超范围中止时，中止**之前**已经收下的那些块必须记账。
    ///
    /// 上面那条测试用的是单独一个超长块：越界在写第一块时就发现了，`written`
    /// 还是 0，于是「有没有记前缀」这件事根本没被走到。多块才走得到——而这条
    /// 路径上原先是直接 `return Err`，跳过了 flush 和 `record_completed_range`。
    /// 后果不只是少缓存一段：`BufWriter` 每攒满 256KB 就真的下发一次 write，
    /// 所以大响应体里那些字节**已经在磁盘上**，占着空间、被 `enumerate` 计入
    /// 容量簿记，却不在 ranges.json 里，任何读都命不中，只能等整条缓存被淘汰
    /// 时一起消失。
    #[tokio::test]
    async fn overrun_still_records_the_prefix_received_before_the_abort() {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage(dir.path());

        // 声明范围 10-14（5 字节），分两块共给 6 字节：第二块才把总数推到越界。
        let result = storage
            .write(
                "asset",
                stream::iter([
                    Ok(Bytes::from_static(b"abc")),
                    Ok(Bytes::from_static(b"def")),
                ]),
                (10, 14),
            )
            .await;

        // 中止依然作为错误上抛：上游违反了自己声明的范围，这个信号不能吞掉。
        assert!(result.is_err());

        // 但先收到的 3 字节是这段范围货真价实的前缀，必须可读、可命中。
        assert!(
            storage.check_range("asset", (10, 12)).await.unwrap(),
            "中止前已落盘的前缀没有被记账"
        );
        let mut stream = storage.read("asset", (10, 12)).await.unwrap();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(bytes, b"abc");

        // 记的只能是前缀，绝不能把没拿到的字节也标记成已缓存。
        assert!(!storage.check_range("asset", (10, 14)).await.unwrap());
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
    /// `enumerate` 必须能把落盘的条目连同 key 一起报回来，
    /// 这是重启后重建磁盘用量簿记的唯一依据。
    #[tokio::test]
    async fn enumerate_reports_keys_and_sizes_of_cached_entries() {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage(dir.path());
        storage
            .write(
                "first",
                stream::iter([Ok(Bytes::from_static(b"abcdefghij"))]),
                (0, 9),
            )
            .await
            .unwrap();
        storage
            .write(
                "second",
                stream::iter([Ok(Bytes::from_static(b"xy"))]),
                (0, 1),
            )
            .await
            .unwrap();

        let mut found = storage.enumerate().await.unwrap();
        found.sort();
        assert_eq!(
            found,
            vec![("first".to_string(), 10), ("second".to_string(), 2)]
        );
    }

    /// 冷启动（缓存目录还不存在）不是错误，只是没有条目。
    #[tokio::test]
    async fn enumerate_on_a_missing_cache_dir_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage(&dir.path().join("not-created-yet"));
        assert!(storage.enumerate().await.unwrap().is_empty());
    }

    /// 加 `key` 字段之前写下的元数据没有 key，无法归属，必须跳过而不是猜。
    #[tokio::test]
    async fn enumerate_skips_legacy_metadata_without_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage(dir.path());
        storage
            .write(
                "asset",
                stream::iter([Ok(Bytes::from_static(b"ab"))]),
                (0, 1),
            )
            .await
            .unwrap();

        // 模拟旧格式：抹掉 key 字段后原样写回。
        let path = storage.get_metadata_path("asset");
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&tokio_fs::read(&path).await.unwrap()).unwrap();
        metadata.as_object_mut().unwrap().remove("key");
        tokio_fs::write(&path, serde_json::to_vec(&metadata).unwrap())
            .await
            .unwrap();

        assert!(storage.enumerate().await.unwrap().is_empty());
    }
}
