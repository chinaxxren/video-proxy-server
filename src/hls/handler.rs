use super::{playlist_refresh_ttl, HlsHandler, HlsManager, PROXY_PREFIX};
use crate::data_source::net_source::{shared_client_v1, SharedClientV1};
use crate::log_info;
use crate::source_registry::SourceRegistry;
use crate::utils::error::{ProxyError, Result};
use crate::utils::network_policy::NetworkPolicy;
use crate::utils::percent_encoding::decode_component;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Full};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

/// m3u8 播放列表的大小上限。
///
/// 播放列表是要整体读进内存的，没有上限就等于把内存交给上游决定：
/// 一个恶意或故障的上游返回无限长的响应体就能把进程 OOM 掉。
/// 8 MiB 对任何正常播放列表都远远够用（一万个分片约 500 KiB）。
const MAX_PLAYLIST_BYTES: usize = 8 * 1024 * 1024;

/// 下载播放列表的整体超时。响应体也算在内，因为这里不是流式读取。
const PLAYLIST_TIMEOUT: Duration = Duration::from_secs(20);

pub struct DefaultHlsHandler {
    manager: Arc<HlsManager>,
    /// 复用进程级共享客户端。
    ///
    /// 这里**必须**用共享客户端：它的连接器装了 `PublicOnlyResolver`。
    /// 之前这里自建 `HttpsConnector::new()`，走的是默认解析器，
    /// m3u8 这条路径上的 DNS rebinding 防护是完全失效的。
    client: SharedClientV1,
    policy: Arc<NetworkPolicy>,
    source_registry: SourceRegistry,
}

impl DefaultHlsHandler {
    pub fn new(
        cache_dir: PathBuf,
        policy: Arc<NetworkPolicy>,
        source_registry: SourceRegistry,
    ) -> Self {
        Self {
            manager: Arc::new(HlsManager::new(cache_dir)),
            client: shared_client_v1(),
            policy,
            source_registry,
        }
    }

    fn get_base_url(&self, url: &str) -> Result<String> {
        let mut base =
            Url::parse(url).map_err(|e| ProxyError::Parse(format!("无法解析URL: {}", e)))?;

        // 只判断「有没有路径段」，不必把所有段收进 Vec——原先那次 collect
        // 分配了一整个向量，只为了问一句是否为空。
        if base.path_segments().is_some_and(|mut s| s.next().is_some()) {
            base.path_segments_mut()
                .map_err(|_| ProxyError::Parse("无法修改URL路径".to_string()))?
                .pop();
        }

        Ok(base.to_string())
    }

    async fn download_m3u8(&self, url: &str) -> Result<String> {
        self.policy.validate(url).await?;
        log_info!("HLS", "下载 m3u8 文件");

        let request = hyper::Request::builder()
            .method("GET")
            .uri(url)
            .header("Range", "bytes=0-")
            .header("User-Agent", "Mozilla/5.0 MediaProxyCache/1")
            .header("Accept", "*/*")
            .body(Full::new(bytes::Bytes::new()))
            .map_err(|_| ProxyError::Request("无法构造 m3u8 请求".to_string()))?;
        let response = tokio::time::timeout(PLAYLIST_TIMEOUT, self.client.request(request))
            .await
            .map_err(|_| ProxyError::Network("下载 m3u8 超时".to_string()))?
            .map_err(|e| ProxyError::Network(format!("请求失败: {}", e)))?;

        if !response.status().is_success() {
            return Err(ProxyError::Network(format!(
                "请求失败: {}",
                response.status()
            )));
        }

        let body = Self::read_body_capped(response.into_body()).await?;
        String::from_utf8(body).map_err(|e| ProxyError::Parse(format!("解析响应内容失败: {}", e)))
    }

    /// 带上限地读取响应体。
    ///
    /// 不做无上限的 Body 聚合：一直读到上游结束会让长度完全由对端
    /// 决定。这里边读边累计，超过 [`MAX_PLAYLIST_BYTES`] 立刻放弃并丢弃
    /// 响应体（drop 即断连），已读部分不会保留。
    ///
    /// 声明的长度只用来做两件事，都不信任它的真实性：超限就直接拒绝（省掉
    /// 白读一遍再放弃），以及预留容量（原先从零开始 `extend_from_slice`，
    /// 一个 200 KiB 的播放列表要经历十几次翻倍搬迁）。预留值仍要跟上限取
    /// 小，否则上游只要谎报一个巨大的 Content-Length 就能让这里替它分配。
    async fn read_body_capped<B>(body: B) -> Result<Vec<u8>>
    where
        B: hyper::body::Body<Data = bytes::Bytes>,
        B::Error: std::fmt::Display,
    {
        let declared = body.size_hint().lower();
        if declared > MAX_PLAYLIST_BYTES as u64 {
            return Err(ProxyError::Parse(format!(
                "m3u8 声明长度 {} 超过上限 {} 字节",
                declared, MAX_PLAYLIST_BYTES
            )));
        }

        let mut buffer = Vec::with_capacity(declared as usize);
        let mut body = Box::pin(body.into_data_stream());
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| ProxyError::Network(format!("读取响应失败: {}", e)))?;
            checked_playlist_length(buffer.len(), chunk.len())?;
            buffer.extend_from_slice(&chunk);
        }
        Ok(buffer)
    }
}

fn checked_playlist_length(current: usize, chunk: usize) -> Result<usize> {
    let next = current
        .checked_add(chunk)
        .ok_or_else(|| ProxyError::Parse("m3u8 长度累计溢出".to_string()))?;
    if next > MAX_PLAYLIST_BYTES {
        return Err(ProxyError::Parse(format!(
            "m3u8 超过大小上限 {} 字节",
            MAX_PLAYLIST_BYTES
        )));
    }
    Ok(next)
}

impl HlsHandler for DefaultHlsHandler {
    async fn handle_m3u8(&self, url: &str) -> Result<String> {
        log_info!("HLS", "处理 m3u8 请求");

        // 移除可能存在的 /proxy/ 前缀（可能嵌套多层）。
        //
        // 游标是 `&str` 而不是 `String`：原先每剥一层都 `to_string()` 一次，
        // 嵌套 n 层就是 n 次分配加 n 次拷贝，而每次拷贝的都是同一段尾部。
        let clean_url = if let Some(proxy_path) = url.find(PROXY_PREFIX) {
            let mut clean = &url[proxy_path + PROXY_PREFIX.len()..];
            while let Some(index) = clean.find(PROXY_PREFIX) {
                clean = &clean[index + PROXY_PREFIX.len()..];
            }
            decode_component(clean)
                .map_err(|e| ProxyError::Request(format!("URL 解码失败: {}", e)))?
                .into_owned()
        } else {
            url.to_string()
        };

        if let Some(content) = self.manager.cached_playlist_body(&clean_url).await {
            return Ok(content.to_string());
        }

        // 下载 m3u8 内容
        let content = self.download_m3u8(&clean_url).await?;

        // 处理 m3u8 文件
        let info = self.manager.process_m3u8(&clean_url, &content).await?;

        // 获取基础 URL
        let base_url = self.get_base_url(&clean_url)?;

        // 重写 m3u8 内容
        let rewritten = self.manager.rewrite_m3u8(&content, &base_url, "/proxy");
        let rewritten = self.register_rewritten_sources(&rewritten)?;
        self.manager
            .cache_playlist_body(&clean_url, rewritten.clone(), playlist_refresh_ttl(&info))
            .await;

        Ok(rewritten)
    }
}

impl DefaultHlsHandler {
    fn register_rewritten_sources(&self, content: &str) -> Result<String> {
        let mut out = String::with_capacity(content.len());
        let mut rest = content;
        while let Some(start) = rest.find("/proxy/") {
            out.push_str(&rest[..start]);
            let tail = &rest[start + "/proxy/".len()..];
            let end = tail
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                .unwrap_or(tail.len());
            let encoded = &tail[..end];
            let source_url = decode_component(encoded)
                .map_err(|e| ProxyError::Request(format!("URL 解码失败: {}", e)))?;
            let id = self.source_registry.register_or_reuse("hls", &source_url)?;
            out.push_str("/media/");
            out.push_str(&id.to_string());
            rest = &tail[end..];
        }
        out.push_str(rest);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hyper_playlist_body_is_read_without_legacy_bridge() {
        let body = Full::new(bytes::Bytes::from_static(b"#EXTM3U\n"));
        let bytes = DefaultHlsHandler::read_body_capped(body).await.unwrap();
        assert_eq!(bytes, b"#EXTM3U\n");
    }

    #[tokio::test]
    async fn hyper_playlist_body_rejects_declared_length_over_limit() {
        let body = Full::new(bytes::Bytes::from(vec![0; MAX_PLAYLIST_BYTES + 1]));
        let error = DefaultHlsHandler::read_body_capped(body).await.unwrap_err();
        assert!(matches!(error, ProxyError::Parse(_)));
    }

    #[test]
    fn playlist_length_rejects_overflow_and_limit_overrun() {
        assert_eq!(checked_playlist_length(10, 20).unwrap(), 30);
        assert!(checked_playlist_length(usize::MAX, 1).is_err());
        assert!(checked_playlist_length(MAX_PLAYLIST_BYTES, 1).is_err());
    }

    #[test]
    fn rewritten_hls_sources_are_opaque_and_cover_uri_attributes() {
        let policy = Arc::new(NetworkPolicy::allow_hosts(["media.example"]));
        let handler =
            DefaultHlsHandler::new(std::env::temp_dir(), policy, SourceRegistry::default());
        let signed = "https://media.example/live/seg.ts?token=secret";
        let encoded = crate::utils::percent_encoding::encode_component(signed);
        let input = format!(
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"/proxy/{encoded}\"\n/proxy/{encoded}\n"
        );
        let output = handler.register_rewritten_sources(&input).unwrap();
        assert!(!output.contains("secret"));
        assert_eq!(output.matches("/media/").count(), 2);
        assert!(!output.contains("/proxy/"));
    }
}
