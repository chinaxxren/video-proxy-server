use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use hyper_util::client::legacy::connect::dns::{GaiResolver as GaiResolverV1, Name as NameV1};
use tokio::time::Instant;
use url::Url;

use crate::utils::error::{ProxyError, Result};

/// 准入检查里 DNS 结果的复用时长。
const ADMISSION_TTL: Duration = Duration::from_secs(30);

/// 准入缓存的条目上限，防止请求大量不同主机把表撑大。
const MAX_ADMISSION_ENTRIES: usize = 1024;

#[derive(Clone, Debug)]
pub struct NetworkPolicy {
    allowed_hosts: HashSet<String>,
    /// 已通过 DNS 准入检查的 `host:port` 及通过时刻。
    ///
    /// 播放器对同一个主机会连着发几十上百个 range 请求，而
    /// [`NetworkPolicy::validate`] 每次都要做一次 `getaddrinfo`——加上连接器
    /// 自己那次解析，等于每个请求两次 DNS。`getaddrinfo` 走的是阻塞线程池，
    /// 量大时既占线程又给每个请求平白加上一次解析延迟。
    ///
    /// 只缓存**通过**的判定，且只省掉 DNS 那一步：scheme、凭据、主机白名单
    /// 每次都仍然重新校验。失败不缓存，避免上游 DNS 抖动被固化 30 秒。
    admitted: Arc<Mutex<HashMap<String, Instant>>>,
}

impl NetworkPolicy {
    pub fn deny_all() -> Self {
        Self {
            allowed_hosts: HashSet::new(),
            admitted: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn allow_hosts<I, S>(hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            allowed_hosts: hosts
                .into_iter()
                .map(|host| host.as_ref().trim().to_ascii_lowercase())
                .filter(|host| !host.is_empty())
                .collect(),
            admitted: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 校验一个上游 URL 是否可以请求。
    ///
    /// 这里的 DNS 解析只是**准入检查**，不能防住 DNS rebinding：解析结果和
    /// 连接时刻之间存在 TOCTOU 窗口，攻击者控制的权威 DNS 可以在这两次解析
    /// 之间把记录翻成 127.0.0.1。域名类 URL 的兜底在 [`PublicOnlyResolverV1`]，
    /// 它在连接建立那一刻再过滤一次地址。
    ///
    /// **但对 IP 字面量的 URL，这一层是唯一的防线。** hyper 的
    /// `HttpConnector` 发现 host 已经是 IP 就跳过解析器直接 connect
    /// Connector 发现 host 已经是 IP 时会跳过解析器，`PublicOnlyResolverV1`
    /// 根本不会被调用。所以 `http://127.0.0.1/` 之所以连不出去，靠的是这里
    /// 的非公网判定，不是那个类型约束。任何新增的回源路径都必须先过
    /// `validate`，绕过它就等于没有 SSRF 防护——不能依赖「解析器兜底」。
    ///
    /// 正因为不可绕过的那层在连接时，这里的 DNS 结果可以按
    /// [`ADMISSION_TTL`] 复用：缓存命中最坏情况只是把一个已经翻成内网地址的
    /// 主机放过这一层，而它在 `connect` 时照样连不出去，只是错误信息换成解析器
    /// 给的那句。省掉的是每个 range 请求一次 `getaddrinfo`。
    pub async fn validate(&self, raw_url: &str) -> Result<()> {
        let url =
            Url::parse(raw_url).map_err(|_| ProxyError::Request("无效的上游 URL".to_string()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ProxyError::Request("仅允许 HTTP(S) 上游".to_string()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ProxyError::Request("上游 URL 不允许包含凭据".to_string()));
        }

        let host = url
            .host_str()
            .ok_or_else(|| ProxyError::Request("上游 URL 缺少主机".to_string()))?
            .to_ascii_lowercase();
        if !self.allowed_hosts.contains(&host) {
            return Err(ProxyError::Request("上游主机不在白名单中".to_string()));
        }

        let port = url
            .port_or_known_default()
            .ok_or_else(|| ProxyError::Request("无效的上游端口".to_string()))?;
        let target = format!("{host}:{port}");
        if self.is_recently_admitted(&target) {
            return Ok(());
        }

        let addresses = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|_| ProxyError::Network("无法解析上游主机".to_string()))?;

        // 「至少有一个公网地址」而不是「全部都是公网地址」。
        //
        // 原先只要撞见一个非公网地址就整体拒绝，与 [`PublicOnlyResolverV1`]
        // 「过滤而非拒绝」的约定直接矛盾：split-horizon DNS 下同时返回公网和
        // 内网 A 记录的主机（CDN 上很常见）会在这一层就被挡掉，解析器那句
        // 「仍然允许连公网那几个」永远不会生效。放宽这一层不放宽安全性——
        // 连接时解析器只会从过滤后的地址里挑，内网那几个到不了 `connect`。
        let mut public = 0usize;
        let mut total = 0usize;
        for address in addresses {
            total += 1;
            if is_allowed_target(address.ip()) {
                public += 1;
            }
        }
        if total == 0 {
            return Err(ProxyError::Network("上游主机没有可用地址".to_string()));
        }
        if public == 0 {
            return Err(ProxyError::Request("上游主机解析到非公网地址".to_string()));
        }

        self.remember_admission(target);
        Ok(())
    }

    fn is_recently_admitted(&self, target: &str) -> bool {
        let Ok(admitted) = self.admitted.lock() else {
            // 锁中毒说明别处已经 panic 过。当作未命中重新解析即可，
            // 不该让一个缓存失效把整条请求路径变成硬失败。
            return false;
        };
        admitted
            .get(target)
            .is_some_and(|at| at.elapsed() < ADMISSION_TTL)
    }

    fn remember_admission(&self, target: String) {
        let Ok(mut admitted) = self.admitted.lock() else {
            return;
        };

        if admitted.len() >= MAX_ADMISSION_ENTRIES && !admitted.contains_key(&target) {
            admitted.retain(|_, at| at.elapsed() < ADMISSION_TTL);
            if admitted.len() >= MAX_ADMISSION_ENTRIES {
                // 清一遍过期条目还是满，说明确实有这么多活跃主机。
                // 这是纯缓存，丢任意一条只会让它下次重新解析。
                if let Some(victim) = admitted.keys().next().cloned() {
                    admitted.remove(&victim);
                }
            }
        }

        admitted.insert(target, Instant::now());
    }
}

/// 只放行公网地址的 DNS 解析器。
///
/// 装在 hyper 的 `HttpConnector` 上，作用于**域名解析后、建立连接前**：
/// 无论 [`NetworkPolicy::validate`] 当时解析到什么，这里都会重新过滤一遍，
/// 因此 DNS rebinding 翻出来的内网地址永远到不了 `connect`。
///
/// 注意作用范围：host 本身就是 IP 字面量时 hyper 不走解析器，这一层看不到
/// 那类请求。IP 字面量由 [`NetworkPolicy::validate`] 负责，见那里的说明。
///
/// 过滤而非整体拒绝：一个域名同时有公网和内网 A 记录时（split-horizon DNS
/// 里很常见），仍然允许连公网那几个。全部地址都不合格才报错。
#[derive(Clone, Debug)]
pub struct PublicOnlyResolverV1 {
    inner: GaiResolverV1,
}

impl PublicOnlyResolverV1 {
    pub fn new() -> Self {
        Self {
            inner: GaiResolverV1::new(),
        }
    }
}

impl Default for PublicOnlyResolverV1 {
    fn default() -> Self {
        Self::new()
    }
}

impl tower_service::Service<NameV1> for PublicOnlyResolverV1 {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        tower_service::Service::poll_ready(&mut self.inner, cx)
    }

    fn call(&mut self, name: NameV1) -> Self::Future {
        let mut inner = self.inner.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addresses = tower_service::Service::call(&mut inner, name).await?;
            let allowed: Vec<SocketAddr> = addresses
                .filter(|address| is_allowed_target(address.ip()))
                .collect();
            if allowed.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{host} 没有解析到任何公网地址，已拒绝连接"),
                ));
            }
            Ok(allowed.into_iter())
        })
    }
}

/// 上游目标准入判定，是 [`NetworkPolicy::validate`] 和 [`PublicOnlyResolverV1`]
/// 共用的唯一决策点。
///
/// 默认等价于 [`is_public_ip`]。只有编译期打开 `allow-private-upstream`
/// 时才额外放行内网/回环地址——这是给端到端测试用的：测试需要把上游指向
/// 本机起的假源站，而生产逻辑必须拒绝这种地址。
///
/// 用编译期 feature 而不是运行期开关，是为了让生产构建**没有**这条代码路径，
/// 而不是「有但没打开」：没有配置项能在运行时误开它。
#[cfg(not(feature = "allow-private-upstream"))]
pub fn is_allowed_target(ip: IpAddr) -> bool {
    is_public_ip(ip)
}

/// 测试专用变体，见上面那条注释。
#[cfg(feature = "allow-private-upstream")]
pub fn is_allowed_target(ip: IpAddr) -> bool {
    // 每个进程只喊一次，别把测试输出刷满。
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        // 直接写 stderr 而不用 log_info!：后者可能被关掉，而这行警告
        // 恰恰是不能被静音的那一行。
        eprintln!(
            "[WARN] 本二进制启用了 allow-private-upstream：\
             回源允许指向内网与回环地址，SSRF 防护已被削弱。\
             仅限本地测试，绝不能用于生产构建。"
        );
    });

    if is_public_ip(ip) {
        return true;
    }
    // 只放开回环和私网这两类——测试假源站只会绑在这上面。
    // link-local（含 169.254.169.254 这类云元数据地址）依旧拒绝：
    // 就算是测试构建，也没有理由需要它。
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
        IpAddr::V6(ip) => ip.is_loopback(),
    }
}

/// 判定一个地址是否是公网单播地址。
///
/// 只要无法确定就返回 `false`：这里的默认值必须偏保守，
/// 漏判一个内网地址就等于开放一次 SSRF。
///
/// 准入判定请调用 [`is_allowed_target`]，不要直接用这个函数——
/// 它不受 `allow-private-upstream` 影响，是纯粹的地址分类谓词。
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        // 0.0.0.0/8「本网络」，以及 240.0.0.0/4 保留段。
        || octets[0] == 0
        || octets[0] >= 240
        // 100.64.0.0/10 运营商级 NAT（RFC 6598）。
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        // 198.18.0.0/15 基准测试网段（RFC 2544）。
        || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        // 192.0.0.0/24 IETF 协议分配段，含 NAT64 的 192.0.0.170 等。
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        // 192.88.99.0/24 已废弃的 6to4 中继任播段（RFC 7526）。
        || (octets[0] == 192 && octets[1] == 88 && octets[2] == 99))
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    // 内嵌 IPv4 的地址必须按内嵌地址判定，否则 `::ffff:127.0.0.1`
    // 会被当成普通公网 v6 地址放行，直接绕过全部 v4 私网检查。
    if let Some(v4) = embedded_ipv4(ip) {
        return is_public_ipv4(v4);
    }

    let segments = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // fc00::/7 唯一本地地址。
        || (segments[0] & 0xfe00) == 0xfc00
        // fe80::/10 链路本地地址。
        || (segments[0] & 0xffc0) == 0xfe80
        // 2001:db8::/32 文档示例段。
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        // 2001::/32 Teredo：其余比特携带可路由到内网的 v4 地址。
        || (segments[0] == 0x2001 && segments[1] == 0x0000)
        // 2001:20::/28 ORCHIDv2，非可路由标识符。
        || (segments[0] == 0x2001 && (segments[1] & 0xfff0) == 0x0020)
        // 100::/64 丢弃前缀（RFC 6666）。
        || (segments[0] == 0x0100 && segments[1..4].iter().all(|s| *s == 0)))
}

/// 提取 IPv6 地址中内嵌的 IPv4 地址（若存在）。
///
/// 覆盖 IPv4-mapped（`::ffff:a.b.c.d`）、IPv4-compatible（`::a.b.c.d`）、
/// NAT64 的 `64:ff9b::/96` 与 6to4 的 `2002::/16`。这几类地址都能把
/// 一个 v4 目标伪装成 v6 字面量。
fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = ip.segments();

    // 2002:V4ADDR::/16 —— v4 地址位于第 2、3 段。
    if segments[0] == 0x2002 {
        return Some(Ipv4Addr::from(
            ((segments[1] as u32) << 16) | segments[2] as u32,
        ));
    }

    // 64:ff9b::/96 与 64:ff9b:1::/48 —— NAT64 的 well-known 前缀。
    if segments[0] == 0x0064 && segments[1] == 0xff9b {
        return Some(Ipv4Addr::from(
            ((segments[6] as u32) << 16) | segments[7] as u32,
        ));
    }

    // ::ffff:0:0/96（mapped）与 ::/96（compatible）。后者已废弃但仍会被解析。
    if segments[0..5].iter().all(|s| *s == 0) && (segments[5] == 0xffff || segments[5] == 0) {
        let v4 = Ipv4Addr::from(((segments[6] as u32) << 16) | segments[7] as u32);
        // `::` 与 `::1` 由调用方的 unspecified/loopback 判定处理。
        if !v4.is_unspecified() {
            return Some(v4);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_private_and_special_addresses() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "192.0.0.170",
            "192.88.99.1",
            "198.18.0.1",
            "0.0.0.0",
            "240.0.0.1",
            "::1",
            "::",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "accepted {ip}");
        }
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        assert!(is_public_ip("2606:4700::1111".parse().unwrap()));
    }

    /// 内嵌 IPv4 的 v6 形式是绕过私网检查最常见的手法，逐类固定住。
    #[test]
    fn rejects_ipv4_embedded_in_ipv6() {
        for ip in [
            // IPv4-mapped
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            // IPv4-compatible（已废弃）
            "::127.0.0.1",
            // NAT64 well-known 前缀
            "64:ff9b::127.0.0.1",
            "64:ff9b::10.0.0.1",
            // 6to4
            "2002:7f00:1::1",
            "2002:a00:1::1",
            // Teredo
            "2001::1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "accepted {ip}");
        }

        // 内嵌的确实是公网地址时不应误伤。
        assert!(is_public_ip("::ffff:8.8.8.8".parse().unwrap()));
        assert!(is_public_ip("64:ff9b::8.8.8.8".parse().unwrap()));
    }

    /// 连接前的最后一道过滤：即使名字解析成功，内网地址也不能交给 connect。
    ///
    /// 注意这只覆盖**域名**形式的上游。IP 字面量走不到解析器，见
    /// [`NetworkPolicy::validate`] 的注释。
    ///
    /// 打开 `allow-private-upstream` 时 localhost 是故意放行的，所以这条
    /// 只在默认构建下成立；公网放行那一半与 feature 无关，单独一条常驻。
    #[cfg(not(feature = "allow-private-upstream"))]
    #[tokio::test]
    async fn hyper_resolver_drops_non_public_addresses() {
        use std::str::FromStr;

        let mut resolver = PublicOnlyResolverV1::new();
        let error =
            tower_service::Service::call(&mut resolver, NameV1::from_str("localhost").unwrap())
                .await
                .expect_err("localhost 应当被拒绝");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    /// 公网地址必须原样带出。这一半不受 `allow-private-upstream` 影响，
    /// 否则打开 feature 后解析器就完全没有测试覆盖了。
    #[tokio::test]
    async fn hyper_resolver_passes_public_addresses_through() {
        use std::str::FromStr;

        let mut resolver = PublicOnlyResolverV1::new();
        let addresses: Vec<SocketAddr> =
            tower_service::Service::call(&mut resolver, NameV1::from_str("8.8.8.8").unwrap())
                .await
                .expect("公网地址应当放行")
                .collect();
        assert!(addresses.iter().all(|address| is_public_ip(address.ip())));
        assert!(!addresses.is_empty());
    }

    #[tokio::test]
    async fn rejects_non_http_credentials_and_unlisted_hosts() {
        let policy = NetworkPolicy::allow_hosts(["media.example"]);
        assert!(policy.validate("file:///etc/passwd").await.is_err());
        assert!(policy
            .validate("https://user:secret@media.example/song")
            .await
            .is_err());
        assert!(policy.validate("https://other.example/song").await.is_err());
    }

    /// IP 字面量的唯一防线就在这里——解析器根本不会被调用。
    #[cfg(not(feature = "allow-private-upstream"))]
    #[tokio::test]
    async fn rejects_allowlisted_private_ip_literal() {
        let policy = NetworkPolicy::allow_hosts(["127.0.0.1"]);
        assert!(policy.validate("http://127.0.0.1/secret").await.is_err());
    }

    /// 打开 `allow-private-upstream` 后回环地址放行——端到端测试要靠它把
    /// 上游指向本机假源站。白名单仍然生效，没列进去的主机照样拒绝。
    #[cfg(feature = "allow-private-upstream")]
    #[tokio::test]
    async fn feature_allows_loopback_but_still_enforces_allowlist() {
        let policy = NetworkPolicy::allow_hosts(["127.0.0.1"]);
        assert!(policy.validate("http://127.0.0.1/media").await.is_ok());
        assert!(policy.validate("http://10.0.0.1/media").await.is_err());

        // 云元数据地址属于 link-local，即使测试构建也不放行。
        let metadata = NetworkPolicy::allow_hosts(["169.254.169.254"]);
        assert!(metadata
            .validate("http://169.254.169.254/latest/meta-data/")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn accepts_allowlisted_public_ip_and_normalizes_host_case() {
        let policy = NetworkPolicy::allow_hosts([" 8.8.8.8 "]);
        assert!(policy.validate("https://8.8.8.8/media").await.is_ok());
    }

    #[cfg(not(feature = "allow-private-upstream"))]
    #[tokio::test]
    async fn allowlisted_name_resolving_to_loopback_is_rejected() {
        let named = NetworkPolicy::allow_hosts(["LOCALHOST"]);
        // 规范化后的名字过了白名单，随后因非公网被拒。
        let error = named.validate("http://localhost/media").await.unwrap_err();
        assert!(matches!(error, ProxyError::Request(message) if message.contains("非公网")));
    }

    /// 命中准入缓存时确实不再解析：预置一个永远解析不出来的主机，validate
    /// 仍然通过，说明这条路径上没有 DNS。
    #[tokio::test]
    async fn cache_hit_skips_dns_resolution() {
        let policy = NetworkPolicy::allow_hosts(["nonexistent.invalid"]);
        // 未预置时必然因解析失败被拒——保证这个测试不是空转的。
        assert!(policy
            .validate("https://nonexistent.invalid/media")
            .await
            .is_err());

        policy.remember_admission("nonexistent.invalid:443".to_string());
        assert!(policy
            .validate("https://nonexistent.invalid/media")
            .await
            .is_ok());
    }

    /// 白名单与 scheme/凭据检查不受缓存影响，每次都要重新走。
    #[tokio::test]
    async fn cache_hit_still_enforces_allowlist_and_scheme() {
        let policy = NetworkPolicy::allow_hosts(["media.example"]);
        policy.remember_admission("other.example:443".to_string());
        assert!(policy.validate("https://other.example/song").await.is_err());

        policy.remember_admission("media.example:443".to_string());
        assert!(policy
            .validate("https://user:secret@media.example/song")
            .await
            .is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn admission_expires_after_ttl() {
        let policy = NetworkPolicy::allow_hosts(["media.example"]);
        policy.remember_admission("media.example:443".to_string());
        assert!(policy.is_recently_admitted("media.example:443"));

        tokio::time::advance(ADMISSION_TTL).await;
        assert!(!policy.is_recently_admitted("media.example:443"));
    }

    #[tokio::test]
    async fn admission_cache_stays_bounded() {
        let policy = NetworkPolicy::allow_hosts(["media.example"]);
        for index in 0..MAX_ADMISSION_ENTRIES + 32 {
            policy.remember_admission(format!("host-{index}.example:443"));
        }
        assert!(policy.admitted.lock().unwrap().len() <= MAX_ADMISSION_ENTRIES);
    }

    #[tokio::test]
    async fn deny_all_rejects_even_public_hosts() {
        assert!(NetworkPolicy::deny_all()
            .validate("https://8.8.8.8/media")
            .await
            .is_err());
    }
}
