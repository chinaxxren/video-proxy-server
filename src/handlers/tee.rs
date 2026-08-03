use crate::handlers::CacheHandler;
use crate::log_info;
use crate::utils::error::Result;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// 转发通道容量（数据块个数）。上下游之间的缓冲窗口。
pub const FORWARD_CHANNEL_CAPACITY: usize = 32;

/// 把上游响应体分叉：一份返回给客户端，一份在后台写进缓存。
///
/// 缓存写入是 best-effort —— 失败只记日志，绝不影响返回给客户端的流。
/// 写入任务独立于返回的流存活，因此客户端中途断开时缓存仍会写完整段，
/// 这正是「边播边缓存」能在下次命中的前提。
///
/// `range` 是这段数据在整个资源里的闭区间位置，必须与上游实际返回的字节
/// 范围一致，否则缓存索引会把错位的数据标记成已下载。
pub fn tee_to_cache(
    upstream: Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>,
    cache_handler: Arc<CacheHandler>,
    key: String,
    range: (u64, u64),
) -> ReceiverStream<Result<Bytes>> {
    let (client_tx, client_rx) = mpsc::channel::<Result<Bytes>>(FORWARD_CHANNEL_CAPACITY);
    let (cache_tx, cache_rx) = mpsc::channel::<Result<Bytes>>(FORWARD_CHANNEL_CAPACITY);

    tokio::spawn(forward_upstream(upstream, client_tx, cache_tx));

    // 缓存写入独立后台运行。绝不能在返回响应前 await 它：
    // 那样 hyper 还没开始 poll 响应体，转发任务就会被通道容量卡死。
    tokio::spawn(async move {
        let stream = Box::pin(ReceiverStream::new(cache_rx));
        if let Err(error) = cache_handler.write_stream(&key, range, stream).await {
            log_info!("Cache", "缓存写入失败: {}", error);
        }
    });

    ReceiverStream::new(client_rx)
}

/// 把上游响应体同时喂给客户端和缓存写入器。
///
/// 两侧互不影响是这个函数的全部要点，四条约束：
///
/// 1. 用 `send().await` 而非 `try_send`——后者在通道满时失败并丢弃余下全部
///    数据，客户端会收到截断的响应体，而缓存仍被标记为完整。
/// 2. 客户端断开只停发客户端侧，缓存侧继续写完，避免留下半截缓存。
/// 3. **缓存侧失败只停发缓存侧，客户端侧必须继续。** 之前这里是 `break`：
///    磁盘写满、上游响应体超出请求范围等任何让写入任务提前退出的情况，都会
///    连带把客户端的响应体截断在半路——而 `Content-Length` 已经按完整长度
///    发出去了，播放器只会看到一个卡住的流，日志里也只有一行缓存写入失败。
/// 4. 两侧都关了才 `break`。此时再读上游没有任何意义，提前 drop 掉响应体
///    可以立刻释放连接。
pub async fn forward_upstream(
    mut upstream: Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>,
    client_tx: mpsc::Sender<Result<Bytes>>,
    cache_tx: mpsc::Sender<Result<Bytes>>,
) {
    let mut client_open = true;
    let mut cache_open = true;

    while let Some(item) = upstream.next().await {
        match item {
            Ok(chunk) => {
                // Bytes 是引用计数的，clone 只加一次计数，不复制数据。
                if cache_open && cache_tx.send(Ok(chunk.clone())).await.is_err() {
                    log_info!("Cache", "缓存写入端已关闭，继续向客户端转发");
                    cache_open = false;
                }
                if client_open && client_tx.send(Ok(chunk)).await.is_err() {
                    log_info!("Cache", "客户端已断开，继续写入缓存");
                    client_open = false;
                }
                if !client_open && !cache_open {
                    break;
                }
            }
            Err(error) => {
                if cache_open {
                    // 让写入端看到错误，它才不会把这段标记成完整区间。
                    let _ = cache_tx.send(Err(error.clone())).await;
                }
                if client_open {
                    let _ = client_tx.send(Err(error)).await;
                }
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{DiskStorage, StorageConfig, StorageManager, StorageManagerConfig};
    use crate::utils::error::ProxyError;
    use futures::stream;

    /// 收集通道里剩下的全部数据，遇错即停。
    async fn drain(mut rx: mpsc::Receiver<Result<Bytes>>) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        while let Some(item) = rx.recv().await {
            bytes.extend_from_slice(&item?);
        }
        Ok(bytes)
    }

    fn upstream_of(
        chunks: Vec<Result<Bytes>>,
    ) -> Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>> {
        Box::pin(stream::iter(chunks))
    }

    #[tokio::test]
    async fn forwards_the_whole_body_to_both_sides() {
        let (client_tx, client_rx) = mpsc::channel(4);
        let (cache_tx, cache_rx) = mpsc::channel(4);
        let upstream = upstream_of(vec![
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"de")),
        ]);

        let forwarding = tokio::spawn(forward_upstream(upstream, client_tx, cache_tx));
        let (client, cache) = tokio::join!(drain(client_rx), drain(cache_rx));
        forwarding.await.unwrap();

        assert_eq!(client.unwrap(), b"abcde");
        assert_eq!(cache.unwrap(), b"abcde");
    }

    /// 缓存写入器提前退出时，客户端必须仍然收到完整响应体。
    ///
    /// 这是修掉的那个 bug：原实现在 `cache_tx.send` 失败时 `break`，于是磁盘
    /// 写满或上游响应体超出请求范围时，客户端拿到的是一个长度短于
    /// `Content-Length` 的响应体——播放器表现为卡死，日志里只有一行缓存失败。
    #[tokio::test]
    async fn cache_writer_failure_does_not_truncate_the_client_body() {
        let (client_tx, client_rx) = mpsc::channel(4);
        let (cache_tx, cache_rx) = mpsc::channel(4);
        drop(cache_rx);
        let upstream = upstream_of(vec![
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"de")),
        ]);

        let forwarding = tokio::spawn(forward_upstream(upstream, client_tx, cache_tx));
        let client = drain(client_rx).await;
        forwarding.await.unwrap();

        assert_eq!(client.unwrap(), b"abcde");
    }

    /// 客户端断开时缓存侧必须写完，否则会留下半截缓存。
    #[tokio::test]
    async fn client_disconnect_does_not_truncate_the_cached_body() {
        let (client_tx, client_rx) = mpsc::channel(4);
        let (cache_tx, cache_rx) = mpsc::channel(4);
        drop(client_rx);
        let upstream = upstream_of(vec![
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"de")),
        ]);

        let forwarding = tokio::spawn(forward_upstream(upstream, client_tx, cache_tx));
        let cache = drain(cache_rx).await;
        forwarding.await.unwrap();

        assert_eq!(cache.unwrap(), b"abcde");
    }

    /// 上游报错必须同时传达给两侧：缓存侧靠它避免把半截数据标记成完整区间。
    #[tokio::test]
    async fn upstream_error_reaches_both_sides() {
        let (client_tx, client_rx) = mpsc::channel(4);
        let (cache_tx, cache_rx) = mpsc::channel(4);
        let upstream = upstream_of(vec![
            Ok(Bytes::from_static(b"abc")),
            Err(ProxyError::Network("连接中断".to_string())),
        ]);

        let forwarding = tokio::spawn(forward_upstream(upstream, client_tx, cache_tx));
        let (client, cache) = tokio::join!(drain(client_rx), drain(cache_rx));
        forwarding.await.unwrap();

        assert!(client.is_err());
        assert!(cache.is_err());
    }

    /// `tee_to_cache` 必须真的把数据落盘，而且客户端拿到的是同一份字节。
    ///
    /// 这条断言守着混合源的「渐进式缓存」：网络段写不回去的话，
    /// 混合源请求每次都只读旧缓存 + 全量回源，缓存边界永远不前进。
    #[tokio::test]
    async fn tee_writes_the_range_to_cache_and_returns_it_to_the_client() {
        let dir = tempfile::tempdir().unwrap();
        let storage = DiskStorage::new(StorageConfig {
            root_path: dir.path().to_path_buf(),
            chunk_size: 4,
        });
        let cache = Arc::new(CacheHandler::new(Arc::new(StorageManager::new(
            storage,
            StorageManagerConfig::default(),
        ))));

        let upstream = upstream_of(vec![
            Ok(Bytes::from_static(b"abcde")),
            Ok(Bytes::from_static(b"fghij")),
        ]);
        let mut client = tee_to_cache(upstream, cache.clone(), "k".to_string(), (0, 9));

        let mut received = Vec::new();
        while let Some(chunk) = client.next().await {
            received.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(received, b"abcdefghij");

        // 写入是后台任务，等区间索引出现为止（而不是睡固定时长）。
        for _ in 0..200 {
            if cache.check_range("k", (0, 9)).await.unwrap() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            cache.check_range("k", (0, 9)).await.unwrap(),
            "网络段没有写回缓存，渐进式缓存失效"
        );

        let mut cached = cache.read("k", (0, 9)).await.unwrap();
        let mut disk_bytes = Vec::new();
        while let Some(chunk) = cached.next().await {
            disk_bytes.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(disk_bytes, b"abcdefghij");
    }
}
