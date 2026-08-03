use crate::log_info;
use crate::utils::error::{ProxyError, Result};
use hyper::{
    header::{HeaderMap, HeaderValue, RANGE},
    Request,
};
use std::fmt::Write;
use url::Url;
use urlencoding;

/// 代理路径前缀。播放器回请的 URL 会带上这一段。
const PROXY_PREFIX: &str = "/proxy/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestType {
    Normal,
    M3u8,
    Segment,
}

#[derive(Debug, Clone)]
pub struct DataRequest {
    pub url: String,
    cache_key: Option<String>,
    pub range: String,
    pub headers: HeaderMap,
    pub request_type: RequestType,
}

impl DataRequest {
    pub fn new(req: &Request<hyper::Body>) -> Result<Self> {
        let url = if let Some(original_url) = req.headers().get("X-Original-Url") {
            original_url.to_str()?.to_string()
        } else {
            let path = req.uri().path();

            // 检查是否是 /proxy/ 格式
            if let Some(proxy_path) = path.strip_prefix(PROXY_PREFIX) {
                // 处理可能存在的多重 /proxy/ 前缀。
                //
                // 游标是 `&str` 而不是 `String`：原先每剥一层都 `to_string()`
                // 一次，嵌套 n 层就是 n 次分配加 n 次拷贝，拷的还都是同一段尾部。
                let mut clean_url = proxy_path;
                while let Some(index) = clean_url.find(PROXY_PREFIX) {
                    clean_url = &clean_url[index + PROXY_PREFIX.len()..];
                }

                // 解码 URL
                urlencoding::decode(clean_url)
                    .map_err(|e| ProxyError::Request(format!("URL 解码失败: {}", e)))?
                    .into_owned()
            } else {
                // 如果不是 /proxy/ 格式，尝试查询参数
                let uri = req.uri().to_string();
                let parsed_url = Url::parse(&uri)
                    .map_err(|_| ProxyError::Request("无效的请求URL".to_string()))?;

                parsed_url.to_string()
            }
        };

        // 整个构造过程只解析一次 URL。
        //
        // 原先解析两次：`build_cache_key` 里一次（取规范身份），判断请求类型时
        // 又一次。`Url::parse` 要做完整语法校验并分配一个新 `Url`，而两次拿到的
        // 结果完全相同。判断类型那次还额外把 path 复制成 `String`，只为了调一次
        // `ends_with`。
        //
        // 提前解析不改变错误行为：`build_cache_key` 本来就无条件调
        // `canonical_upstream_identity`，URL 不合法时同样是在这个位置返回错误。
        let parsed =
            Url::parse(&url).map_err(|_| ProxyError::Request("无效的上游 URL".to_string()))?;

        let cache_key = Self::build_cache_key(&parsed, req.headers())?;

        // 获取 Range 头
        let range = if let Some(range_header) = req.headers().get(RANGE) {
            range_header.to_str()?.to_string()
        } else {
            "bytes=0-".to_string()
        };

        log_info!("Request", "key: range, value: {}", range);

        // 确定请求类型
        let path = parsed.path();
        let request_type = if path.ends_with(".m3u8") {
            log_info!("Request", "type: M3u8");
            RequestType::M3u8
        } else if path.ends_with(".ts") {
            log_info!("Request", "type: Segment");
            RequestType::Segment
        } else {
            log_info!("Request", "type: Normal");
            RequestType::Normal
        };

        Ok(Self {
            url,
            cache_key,
            range,
            headers: req.headers().clone(),
            request_type,
        })
    }

    /// 构造缓存键。
    ///
    /// 键由两部分组成，顺序固定：**上游规范身份** + 客户端声明的身份头。
    ///
    /// 前者是安全上的关键。只用客户端头做键时，任何人都能伪造
    /// `X-Cache-Asset-Id` 把自己的请求映射到别人的缓存条目上：先让
    /// `asset-1` 缓存 A 资源，再带同一个 `asset-1` 去请求 B 资源，就会
    /// 拿到 A 的内容——既是投毒也是越权读取。把上游 host+path 拼进键以后，
    /// 客户端头只能在同一个上游资源内部再做切分，无法让两个不同资源互相别名。
    ///
    /// 后者仍然保留：签名 URL 的 query 每次都变，不能进键，而同一个 path
    /// 在不同用户/版本下可能需要分开存（见 `X-Cache-Asset-Revision`）。
    fn build_cache_key(url: &Url, headers: &HeaderMap) -> Result<Option<String>> {
        let names = [
            "X-Cache-User-Id",
            "X-Cache-Asset-Id",
            "X-Cache-Asset-Revision",
        ];

        // 直接往一个 String 里追加，不再 `Vec<String>` + `format!` 每段 + `join`。
        // 原先四段各分配一次、长度前缀又各分配一次、join 再分配一次结果，
        // 一共九次分配；现在只有身份串和结果两次。
        let mut key = String::new();
        push_component(&mut key, &canonical_upstream_identity(url)?);
        for name in names {
            match headers.get(name) {
                Some(value) => {
                    let value = value.to_str()?.trim();
                    if value.is_empty() {
                        return Err(ProxyError::Request(format!("{} 不能为空", name)));
                    }
                    key.push('|');
                    push_component(&mut key, value);
                }
                None => return Ok(None),
            }
        }

        Ok(Some(key))
    }

    pub fn new_request_with_range(url: &str, range: &str) -> Request<hyper::Body> {
        let mut builder = Request::builder().method("GET").uri(url);

        // 总是添加 Range 头，因为现在我们总是有一个值
        if let Ok(value) = HeaderValue::from_str(range) {
            builder = builder.header(RANGE, value);
            log_info!("Request", "Range header: {}", range);
        }

        builder = builder
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36")
            .header("Accept", "*/*")
            .header("Connection", "keep-alive");

        builder
            .body(hyper::Body::empty())
            .unwrap_or_else(|_| Request::new(hyper::Body::empty()))
    }

    pub fn get_url(&self) -> &str {
        &self.url
    }

    pub fn get_range(&self) -> &str {
        &self.range
    }

    pub fn get_cache_key(&self) -> Result<&str> {
        self.cache_key.as_deref().ok_or_else(|| {
            ProxyError::Request(
                "缺少稳定缓存身份头: X-Cache-User-Id, X-Cache-Asset-Id, X-Cache-Asset-Revision"
                    .to_string(),
            )
        })
    }

    pub fn get_headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn get_type(&self) -> &RequestType {
        &self.request_type
    }
}

/// 从上游 URL 提取「同一个资源」的规范表示。
///
/// 只取 scheme、host、port、path，**丢掉 query 和 fragment**：签名 URL 的
/// query 每次请求都不同，带进缓存键会让同一个文件被反复重新下载。
///
/// 规范化的点：
/// - host 转小写（DNS 大小写不敏感，`CDN.example` 和 `cdn.example` 是同一台）
/// - 端口只在非默认时写入，避免 `https://h/x` 与 `https://h:443/x` 分成两条
/// - path 保持原样，不做大小写折叠：多数上游的 path 是大小写敏感的
fn canonical_upstream_identity(parsed: &Url) -> Result<String> {
    let host = parsed
        .host_str()
        .ok_or_else(|| ProxyError::Request("上游 URL 缺少主机".to_string()))?;

    let scheme = parsed.scheme();
    let path = parsed.path();
    // 一次性把容量算够，后面几次 push 都不会再搬迁。`+ 8` 留给 "://" 和端口。
    let mut identity = String::with_capacity(scheme.len() + host.len() + path.len() + 8);
    identity.push_str(scheme);
    identity.push_str("://");
    let host_start = identity.len();
    identity.push_str(host);
    // 原地转小写，不再单独 `to_ascii_lowercase()` 分配一个中间 String。
    // 只动 ASCII 字节，不会破坏 IDN 主机名的 UTF-8 编码。
    identity[host_start..].make_ascii_lowercase();
    if let Some(port) = parsed.port() {
        // Url::port() 在端口等于该 scheme 默认值时返回 None，正好是我们要的。
        let _ = write!(identity, ":{}", port);
    }
    identity.push_str(path);
    Ok(identity)
}

/// 追加一个长度前缀的组件。
///
/// 长度前缀是必需的：否则 `("ab","c")` 和 `("a","bc")` 会拼出同一个键。
fn push_component(key: &mut String, value: &str) {
    let _ = write!(key, "{}:{}", value.len(), value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::Body;

    /// 测试用：从字符串取规范身份。`canonical_upstream_identity` 现在收
    /// `&Url`，因为调用方（`DataRequest::new`）已经解析过一次了。
    fn identity_of(url: &str) -> Result<String> {
        canonical_upstream_identity(&Url::parse(url).unwrap())
    }

    #[test]
    fn cache_key_is_stable_when_signed_url_changes() {
        let build = |url: &str| {
            Request::builder()
                .uri("/proxy/placeholder")
                .header("X-Original-Url", url)
                .header("X-Cache-User-Id", "user-1")
                .header("X-Cache-Asset-Id", "song-2")
                .header("X-Cache-Asset-Revision", "7")
                .body(Body::empty())
                .unwrap()
        };

        let first = DataRequest::new(&build("https://media.example/song?token=one")).unwrap();
        let second = DataRequest::new(&build("https://media.example/song?token=two")).unwrap();
        assert_eq!(
            first.get_cache_key().unwrap(),
            second.get_cache_key().unwrap()
        );
    }

    #[test]
    fn cache_key_requires_all_identity_headers() {
        let request = Request::builder()
            .uri("/proxy/placeholder")
            .header("X-Original-Url", "https://media.example/song")
            .body(Body::empty())
            .unwrap();
        let parsed = DataRequest::new(&request).unwrap();
        assert!(parsed.get_cache_key().is_err());
    }

    #[test]
    fn parses_encoded_proxy_url_range_and_media_type() {
        let request = Request::builder()
            .uri("/proxy/https%3A%2F%2Fmedia.example%2Flive%2Findex.m3u8%3Ftoken%3Done")
            .header(RANGE, "bytes=100-199")
            .body(Body::empty())
            .unwrap();

        let parsed = DataRequest::new(&request).unwrap();
        assert_eq!(
            parsed.get_url(),
            "https://media.example/live/index.m3u8?token=one"
        );
        assert_eq!(parsed.get_range(), "bytes=100-199");
        assert_eq!(parsed.get_type(), &RequestType::M3u8);
    }

    /// 嵌套的 `/proxy/` 前缀要全部剥掉，只留最里层的真实 URL。
    #[test]
    fn nested_proxy_prefixes_are_all_stripped() {
        let request = Request::builder()
            .uri("/proxy/https%3A%2F%2Fouter.example%2Fx/proxy/https%3A%2F%2Fmedia.example%2Fa.mp4")
            .body(Body::empty())
            .unwrap();

        let parsed = DataRequest::new(&request).unwrap();
        assert_eq!(parsed.get_url(), "https://media.example/a.mp4");
    }

    /// 解码后不是合法 URL 时必须报错，而不是带着垃圾串继续往上游走。
    #[test]
    fn unparseable_upstream_url_is_rejected() {
        let request = Request::builder()
            .uri("/proxy/not-a-url")
            .body(Body::empty())
            .unwrap();

        assert!(DataRequest::new(&request).is_err());
    }

    #[test]
    fn malformed_signed_url_is_not_exposed_in_error_text() {
        let request = Request::builder()
            .uri("/proxy/not-a-url%3Ftoken%3Dsuper-secret")
            .body(Body::empty())
            .unwrap();

        let error = match DataRequest::new(&request) {
            Ok(_) => panic!("malformed upstream URL was accepted"),
            Err(error) => error,
        };
        let text = error.to_string();
        assert!(!text.contains("super-secret"));
        assert!(!text.contains("token="));
    }

    #[test]
    fn original_url_takes_precedence_and_segment_query_is_recognized() {
        let request = Request::builder()
            .uri("/proxy/https%3A%2F%2Fwrong.example%2Ffile.mp4")
            .header(
                "X-Original-Url",
                "https://media.example/segment.ts?token=one",
            )
            .body(Body::empty())
            .unwrap();

        let parsed = DataRequest::new(&request).unwrap();
        assert_eq!(
            parsed.get_url(),
            "https://media.example/segment.ts?token=one"
        );
        assert_eq!(parsed.get_range(), "bytes=0-");
        assert_eq!(parsed.get_type(), &RequestType::Segment);
    }

    #[test]
    fn length_prefixed_cache_key_avoids_component_ambiguity() {
        let build = |user: &str, asset: &str| {
            Request::builder()
                .uri("/proxy/placeholder")
                .header("X-Original-Url", "https://media.example/song")
                .header("X-Cache-User-Id", user)
                .header("X-Cache-Asset-Id", asset)
                .header("X-Cache-Asset-Revision", "1")
                .body(Body::empty())
                .unwrap()
        };

        let first = DataRequest::new(&build("ab", "c")).unwrap();
        let second = DataRequest::new(&build("a", "bc")).unwrap();
        assert_ne!(
            first.get_cache_key().unwrap(),
            second.get_cache_key().unwrap()
        );
    }

    /// 伪造身份头不能把请求映射到另一个上游资源的缓存条目上。
    #[test]
    fn forged_identity_headers_cannot_alias_across_upstream_resources() {
        let build = |url: &str| {
            Request::builder()
                .uri("/proxy/placeholder")
                .header("X-Original-Url", url)
                .header("X-Cache-User-Id", "user-1")
                .header("X-Cache-Asset-Id", "same-asset-id")
                .header("X-Cache-Asset-Revision", "1")
                .body(Body::empty())
                .unwrap()
        };

        let victim = DataRequest::new(&build("https://media.example/private/a.mp4")).unwrap();
        let attacker = DataRequest::new(&build("https://media.example/public/b.mp4")).unwrap();
        assert_ne!(
            victim.get_cache_key().unwrap(),
            attacker.get_cache_key().unwrap(),
            "不同上游 path 在同一组身份头下仍然必须落到不同缓存键"
        );

        // 换 host 同样要分开。
        let other_host = DataRequest::new(&build("https://evil.example/private/a.mp4")).unwrap();
        assert_ne!(
            victim.get_cache_key().unwrap(),
            other_host.get_cache_key().unwrap()
        );
    }

    #[test]
    fn upstream_identity_normalizes_host_case_and_default_port() {
        assert_eq!(
            identity_of("https://CDN.Example/Song.mp4?token=1").unwrap(),
            identity_of("https://cdn.example:443/Song.mp4?token=2").unwrap()
        );

        // path 大小写必须保留：上游通常大小写敏感。
        assert_ne!(
            identity_of("https://cdn.example/Song.mp4").unwrap(),
            identity_of("https://cdn.example/song.mp4").unwrap()
        );

        // 非默认端口要区分开。
        assert_ne!(
            identity_of("https://cdn.example/song.mp4").unwrap(),
            identity_of("https://cdn.example:8443/song.mp4").unwrap()
        );
    }

    #[test]
    fn rejects_empty_cache_identity_component() {
        let request = Request::builder()
            .uri("/proxy/placeholder")
            .header("X-Original-Url", "https://media.example/song")
            .header("X-Cache-User-Id", " ")
            .header("X-Cache-Asset-Id", "asset")
            .header("X-Cache-Asset-Revision", "1")
            .body(Body::empty())
            .unwrap();

        assert!(DataRequest::new(&request).is_err());
    }

    #[test]
    fn outbound_range_request_contains_required_transport_headers() {
        let request =
            DataRequest::new_request_with_range("https://media.example/song", "bytes=10-20");

        assert_eq!(request.method(), hyper::Method::GET);
        assert_eq!(request.uri(), "https://media.example/song");
        assert_eq!(request.headers()[RANGE], "bytes=10-20");
        assert_eq!(request.headers()[hyper::header::ACCEPT], "*/*");
        assert!(request.headers().contains_key(hyper::header::USER_AGENT));
    }
}
