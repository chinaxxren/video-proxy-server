mod handler;

pub use handler::DefaultHlsHandler;

use crate::log_info;
use crate::utils::error::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use url::Url;

/// HLS 分片信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    /// 分片 URL
    pub url: String,
    /// 分片时长（秒）
    pub duration: f32,
    /// 分片序号
    pub sequence: u64,
    /// 分片大小（字节）
    pub size: Option<u64>,
    /// 是否已缓存
    pub cached: bool,
}

/// HLS 播放列表信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistInfo {
    /// 原始 URL
    pub url: String,
    /// 目标持续时间
    pub target_duration: f32,
    /// 媒体序列号
    pub media_sequence: u64,
    /// 是否是直播流
    pub is_endlist: bool,
    /// 分片列表
    pub segments: Vec<Segment>,
    /// 变体流信息（仅用于主播放列表）
    pub variants: Vec<VariantStream>,
    /// 最后更新时间
    pub last_updated: chrono::DateTime<chrono::Utc>,
}

/// 变体流信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariantStream {
    /// 播放列表 URL
    pub url: String,
    /// 带宽（比特/秒）
    pub bandwidth: u64,
    /// 分辨率（可���）
    pub resolution: Option<String>,
}

/// 播放列表缓存的条目上限。
///
/// 这张表以 URL 为键，且原先只插入、从不删除。直播场景里播放列表 URL 常带
/// 轮转的签名参数，每次轮转都是一个新键，内存会无界增长——远端只要不断换
/// query 就能把进程撑爆。超过上限时按 `last_updated` 淘汰最旧的条目。
const MAX_CACHED_PLAYLISTS: usize = 512;

/// 代理路径前缀。播放列表被重写后，播放器回请的 URL 会带上这一段。
pub(crate) const PROXY_PREFIX: &str = "/proxy/";

/// HLS 缓存管理器
pub struct HlsManager {
    /// 缓存根目录
    cache_dir: PathBuf,
    /// 播放列表缓存。
    ///
    /// 值是 `Arc<PlaylistInfo>` 而不是 `PlaylistInfo`：这个结构里装着整张
    /// 分片表，每个分片又各带一个 URL `String`。之前「存一份、再返回一份」
    /// 要深拷贝一次，一万个分片的播放列表就是一万次字符串分配——而唯一的
    /// 生产调用方 `handle_m3u8` 拿到返回值后直接丢弃。
    playlists: Arc<RwLock<HashMap<String, Arc<PlaylistInfo>>>>,
}

impl HlsManager {
    /// 创建新的 HLS 管理器实例
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            playlists: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 处理 m3u8 文件
    ///
    /// 返回 `Arc` 而不是值：缓存里存的和返回的是同一份，调用方要不要用都
    /// 只是一次引用计数加一。
    pub async fn process_m3u8(&self, url: &str, content: &str) -> Result<Arc<PlaylistInfo>> {
        log_info!("HLS", "开始处理 m3u8 文件");

        // 解析 m3u8 内容
        let playlist = m3u8_rs::parse_playlist(content.as_bytes())
            .map_err(|e| crate::utils::error::ProxyError::Parse(e.to_string()))?
            .1; // 获取解析结果的第二个元素

        match playlist {
            m3u8_rs::Playlist::MasterPlaylist(master) => {
                log_info!(
                    "HLS",
                    "处理主播放列表，包含 {} 个变体流",
                    master.variants.len()
                );

                // 处理主播放列表
                let variants = master
                    .variants
                    .iter()
                    .map(|v| VariantStream {
                        url: v.uri.clone(),
                        bandwidth: v.bandwidth,
                        resolution: v
                            .resolution
                            .as_ref()
                            .map(|r| format!("{}x{}", r.width, r.height)),
                    })
                    .collect();

                let info = Arc::new(PlaylistInfo {
                    url: url.to_string(),
                    target_duration: 0.0,
                    media_sequence: 0,
                    is_endlist: false,
                    segments: vec![],
                    variants,
                    last_updated: chrono::Utc::now(),
                });

                // 缓存播放列表信息
                self.cache_playlist(url, Arc::clone(&info)).await;
                Ok(info)
            }
            m3u8_rs::Playlist::MediaPlaylist(media) => {
                log_info!(
                    "HLS",
                    "处理媒体播放列表，包含 {} 个分片",
                    media.segments.len()
                );

                // 处理媒体播放列表
                let segments = media
                    .segments
                    .iter()
                    .enumerate()
                    .map(|(i, s)| Segment {
                        url: s.uri.clone(),
                        duration: s.duration,
                        // media_sequence 来自上游播放列表，可以是接近 u64::MAX
                        // 的任意值；直接相加在 debug 构建下会 panic。
                        sequence: media.media_sequence.saturating_add(i as u64),
                        size: None,
                        cached: false,
                    })
                    .collect();

                let info = PlaylistInfo {
                    url: url.to_string(),
                    target_duration: media.target_duration,
                    media_sequence: media.media_sequence,
                    is_endlist: media.end_list,
                    segments,
                    variants: vec![],
                    last_updated: chrono::Utc::now(),
                };

                let info = Arc::new(info);
                self.cache_playlist(url, Arc::clone(&info)).await;
                Ok(info)
            }
        }
    }

    /// 写入播放列表缓存，必要时先腾出空间。
    ///
    /// 直播场景下 URL 常带轮换的签名 query，每次轮换都是一个新 key；
    /// 无上限的 map 会随播放时长单调增长，最终 OOM。这里按
    /// `last_updated` 淘汰最旧的条目，把内存占用钉在常量级。
    async fn cache_playlist(&self, url: &str, info: Arc<PlaylistInfo>) {
        let mut playlists = self.playlists.write().await;

        if !playlists.contains_key(url) && playlists.len() >= MAX_CACHED_PLAYLISTS {
            let evict = playlists
                .iter()
                .min_by_key(|(_, entry)| entry.last_updated)
                .map(|(key, _)| key.clone());
            if let Some(key) = evict {
                log_info!("HLS", "播放列表缓存已满，淘汰最旧条目");
                playlists.remove(&key);
            }
        }

        playlists.insert(url.to_string(), info);
    }

    /// 重写 m3u8 内容，把其中所有资源引用指向本代理。
    ///
    /// 需要处理两类引用：
    /// 1. 独占一行的 URL —— 分片、变体播放列表。
    /// 2. 标签里的 `URI="..."` 属性 —— `#EXT-X-KEY`、`#EXT-X-MAP`、
    ///    `#EXT-X-MEDIA`、`#EXT-X-I-FRAME-STREAM-INF` 等。
    ///
    /// 第 2 类此前完全没处理，后果不止是泄露上游地址：播放列表本身的 URL
    /// 已经变成 `/proxy/<encoded>`，客户端按它解析相对 URI 会得到
    /// `/proxy/key.bin` 这种无意义路径。也就是说加密流（`#EXT-X-KEY`）和
    /// fMP4（`#EXT-X-MAP`）在重写之后是直接播不出来的。
    pub fn rewrite_m3u8(&self, content: &str, base_url: &str, proxy_prefix: &str) -> String {
        log_info!("HLS", "重写 m3u8 内容");

        // base 只解析一次。原先每一行都要重新 `trim_end_matches` + 拼一个带
        // 尾斜杠的 String + `Url::parse`，而 base 在整个重写过程中是不变的：
        // 一条几千个分片的直播播放列表就是几千次完整的 URL 语法解析。
        let base = Base::new(base_url);
        let prefix = proxy_prefix.trim_end_matches('/');

        // 每一行都会变长：多出 `/proxy/` 前缀，且整个 URL 要百分号编码
        // （`:` `/` 各膨胀成三字节）。按原长预留必然要重新分配几次。
        let mut result = String::with_capacity(content.len() * 2);
        for line in content.lines() {
            if line.starts_with('#') {
                push_rewritten_tag(&mut result, line, &base, prefix);
                result.push('\n');
            } else if !line.is_empty() {
                push_proxied(&mut result, &absolutize(line, &base), prefix);
                result.push('\n');
            }
        }
        result
    }

    /// 获取播放列表信息。
    ///
    /// 返回 `Arc` 而不是克隆：调用方通常只读几个字段，没有理由为此复制
    /// 整张分片表。
    pub async fn get_playlist(&self, url: &str) -> Option<Arc<PlaylistInfo>> {
        self.playlists.read().await.get(url).cloned()
    }

    /// 更新分片缓存状态。
    ///
    /// 缓存里存的是 `Arc<PlaylistInfo>`，就地改需要独占访问。这里用
    /// `Arc::make_mut`：引用计数为 1 时直接改，否则先克隆一份再改
    /// （写时复制）。读侧只在锁内 clone 出 `Arc` 就立刻释放锁，所以
    /// 绝大多数情况下计数已经回到 1，不会真的复制。
    pub async fn update_segment_cache(&self, url: &str, sequence: u64, size: u64) -> Result<()> {
        log_info!("HLS", "更新分片缓存状态: sequence={}", sequence);

        if let Some(playlist) = self.playlists.write().await.get_mut(url) {
            // 分片按 sequence 递增排列（`process_m3u8` 就是这么生成的），
            // 可以二分而不必线性扫。一条直播播放列表几千个分片时，线性
            // 查找每次都要遍历整表。
            let found = playlist
                .segments
                .binary_search_by_key(&sequence, |segment| segment.sequence);
            if let Ok(index) = found {
                let segment = &mut Arc::make_mut(playlist).segments[index];
                segment.size = Some(size);
                segment.cached = true;
            }
        }
        Ok(())
    }

    /// 获取分片的缓存路径
    pub fn get_segment_cache_path(&self, url: &str, sequence: u64) -> PathBuf {
        // 一次 format 直接拼出文件名，不再先把摘要格式化成一个中间 String。
        self.cache_dir
            .join(format!("{:x}_seg_{}.ts", md5::compute(url), sequence))
    }
}

/// 预解析好的播放列表 base，供整轮重写复用。
///
/// 相对引用按 RFC 8216 相对于**播放列表所在目录**解析，所以 base 必须以
/// `/` 结尾再交给 `Url::join`——否则 `join` 会把最后一段当文件名替换掉，
/// `https://h/live` + `seg.ts` 会错解成 `https://h/seg.ts`。
struct Base<'a> {
    /// 去掉尾斜杠的原串，只在 `parsed` 为 `None` 的退路上用到。
    trimmed: &'a str,
    /// 带尾斜杠解析出的 base。base 本身不合法时为 `None`。
    parsed: Option<Url>,
}

impl<'a> Base<'a> {
    fn new(base_url: &'a str) -> Self {
        let trimmed = base_url.trim_end_matches('/');
        let mut with_slash = String::with_capacity(trimmed.len() + 1);
        with_slash.push_str(trimmed);
        with_slash.push('/');
        Self {
            trimmed,
            parsed: Url::parse(&with_slash).ok(),
        }
    }
}

/// 把播放列表里的引用解析成绝对 URL。
///
/// 用 `Url::join` 而不是拼字符串，是为了正确处理三件事：根绝对路径
/// （`/v/seg.ts` 应替换整个 path 而非追加）、query 的保留、以及 `../`
/// 的规范化。
///
/// 返回 `Cow`：引用本来就是绝对 URL 时（主播放列表里很常见）原样借出，
/// 不必为了统一返回类型而复制一遍。
fn absolutize<'a>(reference: &'a str, base: &Base<'_>) -> Cow<'a, str> {
    let reference = reference.trim();

    // 已经是代理 URL 时先剥掉前缀，避免重写叠加成 /proxy/%2Fproxy%2F...
    let reference: Cow<'a, str> = match reference.strip_prefix(PROXY_PREFIX) {
        // decode 返回 Cow：没有转义序列时借用，不分配。原先无条件
        // `into_owned()`，等于每个已代理的引用都白复制一次。
        Some(inner) => urlencoding::decode(inner).unwrap_or(Cow::Borrowed(inner)),
        None => Cow::Borrowed(reference),
    };

    if is_absolute_http(&reference) {
        return reference;
    }

    match base.parsed.as_ref().and_then(|url| url.join(&reference).ok()) {
        Some(joined) => Cow::Owned(joined.to_string()),
        // base 不可解析（或 join 失败）时退回字符串拼接，至少保持旧行为
        // 而不是丢掉这条引用。
        None => Cow::Owned(format!(
            "{}/{}",
            base.trimmed,
            reference.trim_start_matches('/')
        )),
    }
}

/// 引用是否已经是绝对 http(s) URL。
fn is_absolute_http(reference: &str) -> bool {
    reference.starts_with("http://") || reference.starts_with("https://")
}

/// 追加一个套好代理前缀的 URL。整个 URL 做百分号编码后作为单个路径段。
///
/// 直接写进目标缓冲，不再 `format!` 出一个中间 String 再拷进去。
fn push_proxied(output: &mut String, absolute_url: &str, proxy_prefix: &str) {
    output.push_str(proxy_prefix);
    output.push('/');
    output.push_str(&urlencoding::encode(absolute_url));
}

/// 重写标签行里所有 `URI="..."` 属性的值。
///
/// 只认大写的 `URI=`（RFC 8216 规定属性名大写）和双引号形式（规范要求
/// URI 属性必须是 quoted-string），因此不会误伤 `#EXT-X-KEY:METHOD=NONE`
/// 这类不含 URI 的标签。同一行出现多个 URI 属性时全部处理。
/// 直接写进目标缓冲。原先返回 `String`，调用方再 `push_str` 进结果——
/// 每个标签行都要多分配一个中间 String 并整段拷贝一次，而绝大多数标签行
/// （`#EXTINF`、`#EXT-X-TARGETDURATION` 等）根本不含 URI 属性，那次分配
/// 连内容都没改。
fn push_rewritten_tag(output: &mut String, line: &str, base: &Base<'_>, proxy_prefix: &str) {
    const NEEDLE: &str = "URI=\"";

    let mut rest = line;
    while let Some(found) = rest.find(NEEDLE) {
        let value_start = found + NEEDLE.len();
        // 找不到闭合引号说明这行是坏的，原样保留剩余部分。
        let Some(value_length) = rest[value_start..].find('"') else {
            break;
        };

        let value = &rest[value_start..value_start + value_length];
        output.push_str(&rest[..value_start]);
        // 空 URI 无法解析，保持原样（什么都不写）而不是生成一个指向 base
        // 的假地址。
        if !value.is_empty() {
            push_proxied(output, &absolutize(value, base), proxy_prefix);
        }
        output.push('"');

        rest = &rest[value_start + value_length + 1..];
    }

    output.push_str(rest);
}

#[async_trait]
pub trait HlsHandler {
    /// 处理 m3u8 请求
    async fn handle_m3u8(&self, url: &str) -> Result<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> (tempfile::TempDir, HlsManager) {
        let dir = tempfile::tempdir().unwrap();
        let manager = HlsManager::new(dir.path().to_path_buf());
        (dir, manager)
    }

    #[tokio::test]
    async fn parses_and_caches_media_playlist_metadata() {
        let (_dir, manager) = manager();
        let content = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n#EXT-X-MEDIA-SEQUENCE:42\n#EXTINF:9.5,\nsegment-42.ts\n#EXTINF:10.0,\nsegment-43.ts\n#EXT-X-ENDLIST\n";
        let url = "https://media.example/live/index.m3u8";

        let info = manager.process_m3u8(url, content).await.unwrap();
        assert_eq!(info.target_duration, 10.0);
        assert_eq!(info.media_sequence, 42);
        assert!(info.is_endlist);
        assert_eq!(info.segments.len(), 2);
        assert_eq!(info.segments[0].sequence, 42);
        assert_eq!(info.segments[0].url, "segment-42.ts");
        assert_eq!(info.segments[0].duration, 9.5);

        let cached = manager.get_playlist(url).await.unwrap();
        assert_eq!(cached.segments.len(), 2);
    }

    #[tokio::test]
    async fn parses_master_playlist_variants() {
        let (_dir, manager) = manager();
        let content = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1280000,RESOLUTION=1280x720\n720p/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2560000\n1080p/index.m3u8\n";

        let info = manager
            .process_m3u8("https://media.example/master.m3u8", content)
            .await
            .unwrap();
        assert!(info.segments.is_empty());
        assert_eq!(info.variants.len(), 2);
        assert_eq!(info.variants[0].bandwidth, 1_280_000);
        assert_eq!(info.variants[0].resolution.as_deref(), Some("1280x720"));
        assert_eq!(info.variants[1].url, "1080p/index.m3u8");
    }

    /// 播放列表缓存必须有界：直播的签名 URL 每次轮换都是一个新 key。
    /// 加密流的密钥和 fMP4 的 init segment 都藏在标签属性里，漏掉就播不了。
    #[test]
    fn rewrites_uri_attributes_in_tags() {
        let (_dir, manager) = manager();
        let content = concat!(
            "#EXTM3U\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x0f\n",
            "#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n",
            "#EXTINF:5.0,\n",
            "segment-1.m4s\n",
        );

        let rewritten = manager.rewrite_m3u8(content, "https://media.example/live/", "/proxy");

        let key = urlencoding::encode("https://media.example/live/key.bin");
        let map = urlencoding::encode("https://media.example/live/init.mp4");
        assert!(
            rewritten.contains(&format!("URI=\"/proxy/{}\"", key)),
            "key URI not rewritten: {rewritten}"
        );
        assert!(
            rewritten.contains(&format!("URI=\"/proxy/{}\"", map)),
            "map URI not rewritten: {rewritten}"
        );

        // 同一行的其他属性必须原样保留。
        assert!(rewritten.contains("METHOD=AES-128"));
        assert!(rewritten.contains("IV=0x0f"));
        assert!(rewritten.contains("BYTERANGE=\"720@0\""));
    }

    #[test]
    fn tags_without_uri_attribute_pass_through_unchanged() {
        let (_dir, manager) = manager();
        let content = "#EXT-X-KEY:METHOD=NONE\n#EXT-X-TARGETDURATION:6\n#EXT-X-ENDLIST\n";

        let rewritten = manager.rewrite_m3u8(content, "https://media.example/live/", "/proxy");
        assert_eq!(rewritten, content);
    }

    /// 根绝对路径应替换整个 path，而不是追加到播放列表目录后面。
    #[test]
    fn root_absolute_reference_replaces_path() {
        let (_dir, manager) = manager();
        let rewritten = manager.rewrite_m3u8(
            "/v2/segment.ts\n",
            "https://media.example/live/",
            "/proxy",
        );

        let expected = urlencoding::encode("https://media.example/v2/segment.ts");
        assert!(
            rewritten.contains(expected.as_ref()),
            "unexpected: {rewritten}"
        );
    }

    #[test]
    fn malformed_uri_attribute_is_left_intact() {
        let (_dir, manager) = manager();
        // 引号没闭合：不能吞掉这一行，也不能 panic。
        let content = "#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\n";

        let rewritten = manager.rewrite_m3u8(content, "https://media.example/live/", "/proxy");
        assert_eq!(rewritten, content);
    }

    #[tokio::test]
    async fn playlist_cache_is_bounded_and_keeps_newest() {
        let (_dir, manager) = manager();
        let content =
            "#EXTM3U\n#EXT-X-TARGETDURATION:5\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:5.0,\na.ts\n";

        let overflow = MAX_CACHED_PLAYLISTS + 16;
        for i in 0..overflow {
            let url = format!("https://media.example/live.m3u8?token={i}");
            manager.process_m3u8(&url, content).await.unwrap();
        }

        assert_eq!(manager.playlists.read().await.len(), MAX_CACHED_PLAYLISTS);
        // 最后写入的一定还在；被淘汰的是最旧的。
        let newest = format!("https://media.example/live.m3u8?token={}", overflow - 1);
        assert!(manager.get_playlist(&newest).await.is_some());
    }

    /// `update_segment_cache` 改用二分查找，前提是分片按 sequence 递增。
    /// 顺便钉住两件事：不存在的 sequence 不能误伤别的分片，中间位置的
    /// 分片也要能命中（线性扫和二分在只有一个分片时无从区分）。
    #[tokio::test]
    async fn segment_update_finds_the_right_entry_among_many() {
        let (_dir, manager) = manager();
        let mut content = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:5\n#EXT-X-MEDIA-SEQUENCE:100\n");
        for i in 0..64 {
            content.push_str(&format!("#EXTINF:5.0,\nsegment-{i}.ts\n"));
        }

        let url = "https://media.example/live.m3u8";
        let info = manager.process_m3u8(url, &content).await.unwrap();
        assert_eq!(info.segments.len(), 64);
        assert_eq!(info.segments[0].sequence, 100);
        assert_eq!(info.segments[63].sequence, 163);

        manager.update_segment_cache(url, 137, 2048).await.unwrap();
        // 表里没有的 sequence：不能命中，也不能 panic。
        manager.update_segment_cache(url, 9999, 1).await.unwrap();

        let playlist = manager.get_playlist(url).await.unwrap();
        let hit = playlist
            .segments
            .iter()
            .filter(|segment| segment.cached)
            .collect::<Vec<_>>();
        assert_eq!(hit.len(), 1, "只应有一个分片被标记为已缓存");
        assert_eq!(hit[0].sequence, 137);
        assert_eq!(hit[0].size, Some(2048));
    }

    /// 缓存存的是 `Arc`，就地更新走 `Arc::make_mut` 的写时复制。
    /// 已经拿到句柄的读者看到的必须是当时的快照，不能被后续更新改掉。
    #[tokio::test]
    async fn existing_handle_is_not_mutated_by_a_later_update() {
        let (_dir, manager) = manager();
        let content =
            "#EXTM3U\n#EXT-X-TARGETDURATION:5\n#EXT-X-MEDIA-SEQUENCE:3\n#EXTINF:5.0,\na.ts\n";
        let url = "https://media.example/live.m3u8";
        manager.process_m3u8(url, content).await.unwrap();

        let snapshot = manager.get_playlist(url).await.unwrap();
        assert!(!snapshot.segments[0].cached);

        manager.update_segment_cache(url, 3, 512).await.unwrap();

        assert!(!snapshot.segments[0].cached, "旧句柄被就地改写了");
        let fresh = manager.get_playlist(url).await.unwrap();
        assert!(fresh.segments[0].cached);
        assert_eq!(fresh.segments[0].size, Some(512));
    }

    #[tokio::test]
    async fn rejects_invalid_playlist_and_updates_known_segment() {
        let (_dir, manager) = manager();
        assert!(manager
            .process_m3u8("https://media.example/bad", "not-m3u8")
            .await
            .is_err());

        let content =
            "#EXTM3U\n#EXT-X-TARGETDURATION:5\n#EXT-X-MEDIA-SEQUENCE:8\n#EXTINF:5.0,\nsegment.ts\n";
        let url = "https://media.example/live.m3u8";
        manager.process_m3u8(url, content).await.unwrap();
        manager.update_segment_cache(url, 8, 4096).await.unwrap();

        let playlist = manager.get_playlist(url).await.unwrap();
        assert!(playlist.segments[0].cached);
        assert_eq!(playlist.segments[0].size, Some(4096));
    }

    #[test]
    fn rewrites_relative_absolute_and_already_proxied_urls() {
        let (_dir, manager) = manager();
        let absolute = "https://cdn.example/segment-2.ts";
        let content = format!(
            "#EXTM3U\n#EXTINF:5.0,\nsegment-1.ts\n#EXTINF:5.0,\n{}\n/proxy/segment-3.ts\n",
            absolute
        );

        let rewritten = manager.rewrite_m3u8(&content, "https://media.example/live/", "/proxy");
        assert!(rewritten.contains(&format!(
            "/proxy/{}",
            urlencoding::encode("https://media.example/live/segment-1.ts")
        )));
        assert!(rewritten.contains(&format!("/proxy/{}", urlencoding::encode(absolute))));
        assert!(rewritten.contains(&format!(
            "/proxy/{}",
            urlencoding::encode("https://media.example/live/segment-3.ts")
        )));
        assert!(rewritten.starts_with("#EXTM3U\n"));
    }

    #[test]
    fn segment_cache_path_is_stable_and_sequence_specific() {
        let (dir, manager) = manager();
        let first = manager.get_segment_cache_path("https://media.example/segment.ts", 1);
        let same = manager.get_segment_cache_path("https://media.example/segment.ts", 1);
        let next = manager.get_segment_cache_path("https://media.example/segment.ts", 2);

        assert_eq!(first, same);
        assert_ne!(first, next);
        assert_eq!(first.parent(), Some(dir.path()));
    }
}
