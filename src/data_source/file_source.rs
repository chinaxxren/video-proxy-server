use crate::config::CONFIG;
use crate::utils::parse_range;
use crate::utils::error::{Result, ProxyError};
use hyper::Body;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use std::io::SeekFrom;
use bytes::Bytes;
use futures_util::stream::{Stream, StreamExt};
use std::pin::Pin;
use futures_util::Future;
use std::task::{Context, Poll};
use crate::{log_info, log_error};

pub struct FileSource {
    path: String,
    range: String,
}

impl FileSource {
    pub fn new(url: &str, range: &str) -> Self {
        let path = CONFIG.get_cache_file(url);
        Self { path, range: range.to_string() }
    }

    pub async fn read_stream(&self) -> Result<impl Stream<Item = Result<Bytes>> + Send + 'static> {
        let (start, end) = parse_range(&self.range)?;
        let mut file = File::open(&self.path).await?;
        
        // 获取文件大小
        let file_size = file.metadata().await?.len();
        
        // 确保开始位置不超过文件大小
        if start >= file_size {
            return Err(ProxyError::Cache("请求范围超出文件大小".to_string()));
        }
        
        // 设置实际的结束位置
        let end_pos = std::cmp::min(end + 1, file_size);
        
        // 移动到起始位置
        file.seek(SeekFrom::Start(start)).await?;
        
        Ok(FileStream {
            file: Some(file),
            buffer_size: 64 * 1024, // 64KB 缓冲区
            current_pos: start,
            end_pos,
        })
    }

    pub async fn read_data(&self) -> Result<Vec<u8>> {
        let mut file = File::open(&self.path).await?;
        let (start, end) = parse_range(&self.range)?;
        
        // 获取文件大小
        let file_size = file.metadata().await?.len();
        
        // 确保开始位置不超过文件大小
        if start >= file_size {
            return Err(ProxyError::Cache("请求范围超出文件大小".to_string()));
        }
        
        // 设置实际的结束位置
        let end_pos = std::cmp::min(end + 1, file_size);
        
        // 移动到起始位置
        file.seek(SeekFrom::Start(start)).await?;
        
        let mut buffer = vec![0; (end_pos - start) as usize];
        file.read_exact(&mut buffer).await?;
        Ok(buffer)
    }
}

pub struct FileStream {
    file: Option<File>,
    buffer_size: usize,
    current_pos: u64,
    end_pos: u64,
}

impl Stream for FileStream {
    type Item = Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.current_pos >= self.end_pos {
            return Poll::Ready(None);
        }

        let remaining = self.end_pos - self.current_pos;
        let to_read = self.buffer_size.min(remaining as usize);
        let mut buffer = vec![0; to_read];

        let file = if let Some(file) = self.file.as_mut() {
            file
        } else {
            return Poll::Ready(None);
        };

        let read_future = file.read(&mut buffer);
        futures_util::pin_mut!(read_future);

        match read_future.poll(cx) {
            Poll::Ready(Ok(n)) if n > 0 => {
                self.current_pos += n as u64;
                buffer.truncate(n);
                log_info!("FileSource", "读取缓存: {} bytes at position {}", n, self.current_pos - n as u64);
                Poll::Ready(Some(Ok(Bytes::from(buffer))))
            }
            Poll::Ready(Ok(_)) => {
                self.file.take();
                Poll::Ready(None)
            }
            Poll::Ready(Err(e)) => {
                log_error!("FileSource", "读取文件失败: {}", e);
                self.file.take();
                Poll::Ready(Some(Err(ProxyError::Io(e))))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
