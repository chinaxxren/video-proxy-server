use crate::utils::error::{ProxyError, Result};
use crate::utils::range::{parse_range, range_length, resolve_range};
use bytes::Bytes;
use futures::Stream;
use std::io::SeekFrom;
use std::path::PathBuf;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// 单次读取的分块大小。
const CHUNK_SIZE: usize = 8192;

/// [`FileSource::read_data`] 一次性读入内存的上限。
///
/// 没有这个上限时，`bytes=0-` 打在一个大文件上会按文件大小分配一整块内存，
/// 单个请求就能把进程 OOM 掉。需要读大范围的调用方应该用
/// [`FileSource::read_stream`]。
const MAX_IN_MEMORY_READ: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct FileSource {
    pub path: String,
    pub range: String,
}

impl FileSource {
    pub fn new(path: &str, range: &str) -> Self {
        Self {
            path: path.to_string(),
            range: range.to_string(),
        }
    }

    pub fn from_path_buf(path: Result<PathBuf>, range: &str) -> Result<Self> {
        let path_str = path?.to_string_lossy().into_owned();
        Ok(Self {
            path: path_str,
            range: range.to_string(),
        })
    }

    /// 打开文件并把请求范围收敛成真实的闭区间。
    ///
    /// 收敛必须在这里做：`bytes=N-` 经 [`parse_range`] 出来的 end 是
    /// `u64::MAX` 哨兵，直接参与 `end - start + 1` 会溢出。
    async fn open_resolved(&self) -> Result<(File, u64, u64)> {
        let file = File::open(&self.path).await?;
        let (start, end) = parse_range(&self.range)?;
        let file_size = file.metadata().await?.len();
        let (start, end) = resolve_range(start, end, Some(file_size))?;
        Ok((file, start, end))
    }

    pub async fn read_stream(&self) -> Result<impl Stream<Item = Result<Bytes>>> {
        let (mut file, start, end) = self.open_resolved().await?;
        let total_bytes = range_length(start, end)?;

        // 旧实现只把 start 记进游标却从未 seek，于是从偏移 0 开始读，
        // 却按 start 汇报进度——返回的内容整体错位。
        file.seek(SeekFrom::Start(start)).await?;

        Ok(futures::stream::try_unfold(
            (file, 0u64, total_bytes),
            |(mut file, mut bytes_read, total_bytes)| async move {
                if bytes_read >= total_bytes {
                    return Ok(None);
                }

                let remaining = total_bytes - bytes_read;
                let to_read = (CHUNK_SIZE as u64).min(remaining) as usize;
                let mut buffer = vec![0u8; to_read];

                // read 可能短读，按实际字节数推进游标。
                let n = file.read(&mut buffer).await?;
                if n == 0 {
                    return Err(ProxyError::IO(format!(
                        "文件在读满范围前结束: 已读 {}/{} 字节",
                        bytes_read, total_bytes
                    )));
                }

                buffer.truncate(n);
                bytes_read += n as u64;
                Ok(Some((Bytes::from(buffer), (file, bytes_read, total_bytes))))
            },
        ))
    }

    pub async fn read_data(&self) -> Result<Vec<u8>> {
        let (mut file, start, end) = self.open_resolved().await?;
        let length = range_length(start, end)?;
        if length > MAX_IN_MEMORY_READ {
            return Err(ProxyError::Request(format!(
                "范围 {} 字节超过单次读取上限 {} 字节，请改用流式读取",
                length, MAX_IN_MEMORY_READ
            )));
        }

        file.seek(SeekFrom::Start(start)).await?;
        // length 已由 resolve_range 收敛在文件长度内，read_exact 不会读到 EOF。
        let mut buffer = vec![0u8; length as usize];
        file.read_exact(&mut buffer).await?;
        Ok(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::io::Write;

    fn fixture(contents: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(contents).unwrap();
        file.flush().unwrap();
        file
    }

    async fn collect(source: &FileSource) -> Result<Vec<u8>> {
        let stream = source.read_stream().await?;
        let mut out = Vec::new();
        let mut stream = Box::pin(stream);
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(out)
    }

    /// 开区间请求过去会带着 u64::MAX 进算术，debug 下直接 panic。
    #[tokio::test]
    async fn open_ended_range_is_clamped_to_file_size() {
        let file = fixture(b"0123456789");
        let path = file.path().to_str().unwrap();

        let source = FileSource::new(path, "bytes=0-");
        assert_eq!(collect(&source).await.unwrap(), b"0123456789");
        assert_eq!(source.read_data().await.unwrap(), b"0123456789");
    }

    /// read_stream 过去不 seek，从偏移 0 读起，返回的数据整体错位。
    #[tokio::test]
    async fn stream_starts_at_requested_offset() {
        let file = fixture(b"0123456789");
        let path = file.path().to_str().unwrap();

        let source = FileSource::new(path, "bytes=4-7");
        assert_eq!(collect(&source).await.unwrap(), b"4567");
        assert_eq!(source.read_data().await.unwrap(), b"4567");
    }

    #[tokio::test]
    async fn range_beyond_file_is_rejected() {
        let file = fixture(b"0123456789");
        let path = file.path().to_str().unwrap();

        let source = FileSource::new(path, "bytes=10-20");
        assert!(collect(&source).await.is_err());
        assert!(source.read_data().await.is_err());
    }

    /// 超出文件末尾的 end 收敛到最后一个字节，而不是报错。
    #[tokio::test]
    async fn end_past_eof_is_clamped() {
        let file = fixture(b"0123456789");
        let source = FileSource::new(file.path().to_str().unwrap(), "bytes=8-99");
        assert_eq!(collect(&source).await.unwrap(), b"89");
    }

    /// 跨多个分块时不能丢字节，也不能重复。
    #[tokio::test]
    async fn multi_chunk_read_is_exact() {
        let contents: Vec<u8> = (0..(CHUNK_SIZE * 2 + 123)).map(|i| (i % 251) as u8).collect();
        let file = fixture(&contents);
        let source = FileSource::new(file.path().to_str().unwrap(), "bytes=0-");
        assert_eq!(collect(&source).await.unwrap(), contents);
    }
}
