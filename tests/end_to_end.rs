//! 端到端 HTTP 测试：真源站 + 真代理 + 真 TCP。
//!
//! 这一层补的是单元测试拿不到的东西。单元测试全都在进程内直接调
//! `DataSourceManager` / `DiskStorage`，绕开了 hyper 的请求解析、响应头构造
//! 和真实的 socket 行为；「播放器发一串 range 请求」这个最重要的场景，
//! 在单元测试里根本没有被覆盖过。
//!
//! 整个文件需要 `allow-private-upstream`：假源站绑在 127.0.0.1 上，而生产
//! 逻辑必须拒绝回环地址的上游。
//!
//! ```sh
//! cargo test --features allow-private-upstream --test end_to_end
//! ```
#![cfg(feature = "allow-private-upstream")]

#[path = "../examples/support/mod.rs"]
mod local_http;

use std::convert::Infallible;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, StreamBody};
use hyper::header::{ACCEPT_RANGES, ALLOW, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use hyper::server::conn::http1::Builder as ConnectionBuilder;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use proxy_server::server::{ProxyConfig, ProxyServer};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 假源站上那个文件的大小。
///
/// 故意取一个既不是 2 的幂、也不是任何块大小整数倍的值：末尾会剩下一个
/// 不完整的块，能顺带压住「最后一块少算一字节」这类 off-by-one。
const ORIGIN_SIZE: u64 = 300_007;

/// 假源站的响应方式。
#[derive(Clone, Copy, PartialEq, Eq)]
enum OriginMode {
    /// 正常实现 range：带 Range 就回 206。
    Honest,
    /// 无视 Range，永远回 200 + 整个文件。
    ///
    /// 这正是问题 2 里那种上游。代理必须识别出来并拒绝，而不是把整个文件
    /// 当成请求的那一小段收下。
    IgnoresRange,
    /// 正常实现 range，但把响应体切成小块、块间留间隔慢慢吐。
    ///
    /// 「客户端中途断开」的测试必须用它。`Body::from(Vec<u8>)` 是一整块，
    /// `forward_upstream` 的循环转一轮就把整段都喂完了，于是测试里那个 drop
    /// 永远发生在转发**结束之后**——「断开后缓存仍会写完」这个断言即使代码
    /// 在客户端断开时直接中止整个循环也照样成立。切成小块才能让 drop 真的
    /// 落在转发过程中间。这一点是被变异测试抓出来的：把正确实现改回
    /// 「断开就 break」，用整块 body 的版本测试依然是绿的。
    Trickle,
    /// 正常实现 range，但每次响应前先睡 [`ORIGIN_DELAY`]。
    ///
    /// 并发合并的测试必须用它。源站瞬间返回时，几路「并发」请求很可能是
    /// 依次跑完的：第一路早已写完缓存，后面几路直接缓存命中，于是
    /// 「只回源一次」这个断言即使合并根本没生效也照样成立——测试通过，
    /// 但什么都没证明。加一段延迟才能保证它们真的在合并窗口里重叠。
    Slow,
}

/// [`OriginMode::Slow`] 每次响应前的延迟。
///
/// 只要比「发起 8 个请求 + 代理走完缓存判定」长就够了，取 300ms 是为了在
/// 慢速 CI 上也稳。整个测试仍然是亚秒级的。
const ORIGIN_DELAY: Duration = Duration::from_millis(300);

/// 第 `offset` 个字节应该是什么。
///
/// 用 `offset % 251`（质数，且小于 256）而不是全零或递增字节：错位一个字节
/// 就会对不上，而质数周期保证任何小于 251 的偏移量错误都不会碰巧自洽。
fn byte_at(offset: u64) -> u8 {
    (offset % 251) as u8
}

fn expected_bytes(start: u64, end: u64) -> Vec<u8> {
    (start..=end).map(byte_at).collect()
}

struct Origin {
    base_url: String,
    /// 收到的请求数。用它判断某次代理请求到底有没有真的回源。
    hits: Arc<AtomicUsize>,
    /// 置位后源站进入「离线」：见 [`Origin::go_offline`]。
    offline: Arc<std::sync::atomic::AtomicBool>,
}

impl Origin {
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn url(&self) -> String {
        format!("{}/media.mp4", self.base_url)
    }

    /// 让源站进入离线：此后任何到达的请求都只记数、然后直接断连接。
    ///
    /// 这里**不能**用 `JoinHandle::abort()` 掐掉源站任务。hyper 的 `Server`
    /// 会把每条 accept 到的连接 spawn 成独立任务，abort 只终止 accept 循环，
    /// 而代理的 Client 带连接池，那条已经建好的 keep-alive 连接由另一个任务
    /// 伺候着，照样正常回数据。这是实测出来的：abort 之后未缓存区间依然
    /// 取到了完整数据（日志里明明白白一行 `网络响应成功，内容长度: 10000`）。
    /// 换 graceful shutdown 也一样，它连 accept 都会等现存连接自己结束。
    ///
    /// 记数放在断连接之前，是为了让「离线后到底有没有试图回源」可被观测：
    /// 只看「请求失败了」分不清是没去连还是连了没连上。
    ///
    /// 也不能改用「把上游地址换成一个不可达地址」来模拟离线：缓存键里含
    /// 上游 host+path，换地址等于换了一条缓存条目，测的就不是同一份缓存了。
    fn go_offline(&self) {
        self.offline.store(true, Ordering::SeqCst);
    }
}

fn spawn_origin(mode: OriginMode) -> Origin {
    // 先用 std 的 listener 占住端口再交给 hyper：先 bind 后交接，
    // 中间没有「端口已释放但还没被 hyper 接手」的竞争窗口。
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("假源站无法绑定端口");
    let addr: SocketAddr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let offline = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_hits = hits.clone();
    let server_offline = offline.clone();

    tokio::spawn(async move {
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        while let Ok((stream, _)) = listener.accept().await {
            let hits = server_hits.clone();
            let offline = server_offline.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let hits = hits.clone();
                    let offline = offline.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        if offline.load(Ordering::SeqCst) {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::ConnectionAborted,
                                "origin is offline",
                            ));
                        }
                        Ok(serve_origin(&req, mode).await)
                    }
                });
                let builder = ConnectionBuilder::new();
                let _ = builder
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });

    Origin {
        base_url: format!("http://{}", addr),
        hits,
        offline,
    }
}

type OriginBody = UnsyncBoxBody<Bytes, Infallible>;

async fn serve_origin<B>(req: &Request<B>, mode: OriginMode) -> Response<OriginBody> {
    if mode == OriginMode::Slow {
        tokio::time::sleep(ORIGIN_DELAY).await;
    }

    let range = req
        .headers()
        .get(RANGE)
        .and_then(|value| value.to_str().ok());

    let full = || {
        Response::builder()
            .status(StatusCode::OK)
            .header(ACCEPT_RANGES, "bytes")
            .header(CONTENT_LENGTH, ORIGIN_SIZE)
            .body(Full::new(Bytes::from(expected_bytes(0, ORIGIN_SIZE - 1))).boxed_unsync())
            .unwrap()
    };

    let Some(range) = range else { return full() };
    if mode == OriginMode::IgnoresRange {
        return full();
    }

    let Some((start, end)) = parse_range(range) else {
        return Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .body(Full::new(Bytes::new()).boxed_unsync())
            .unwrap();
    };

    let body = if mode == OriginMode::Trickle {
        trickle_body(start, end)
    } else {
        Full::new(Bytes::from(expected_bytes(start, end))).boxed_unsync()
    };

    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_LENGTH, end - start + 1)
        .header(
            CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, end, ORIGIN_SIZE),
        )
        .body(body)
        .unwrap()
}

/// 一块 trickle 响应体的大小。
const TRICKLE_CHUNK: u64 = 4 * 1024;

/// trickle 模式下每块之间的间隔。
///
/// 乘上块数就是整段响应的时长（300KB 约 74 块，合计约 370ms）。要比
/// 「客户端建连 + 拿到响应头」长得多，才能保证 drop 落在转发中间。
const TRICKLE_GAP: Duration = Duration::from_millis(5);

/// 把 `[start, end]` 切成小块，块间留间隔，做成一个流式 body。
fn trickle_body(start: u64, end: u64) -> OriginBody {
    let chunks = (start..=end)
        .step_by(TRICKLE_CHUNK as usize)
        .map(move |from| {
            let to = (from + TRICKLE_CHUNK - 1).min(end);
            expected_bytes(from, to)
        })
        .collect::<Vec<_>>();

    StreamBody::new(futures_util::stream::iter(chunks).then(|chunk| async move {
        tokio::time::sleep(TRICKLE_GAP).await;
        Ok::<_, Infallible>(hyper::body::Frame::data(Bytes::from(chunk)))
    }))
    .boxed_unsync()
}

/// 解析 `bytes=a-b` / `bytes=a-`，返回闭区间。
fn parse_range(value: &str) -> Option<(u64, u64)> {
    let spec = value.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end = match end.trim() {
        "" => ORIGIN_SIZE - 1,
        text => text.parse::<u64>().ok()?.min(ORIGIN_SIZE - 1),
    };
    (start <= end).then_some((start, end))
}

struct Proxy {
    port: u16,
    server: Arc<ProxyServer>,
    /// 缓存目录。不带下划线前缀是因为 [`Proxy::cached_ranges`] 要读它。
    cache_dir: std::path::PathBuf,
    _cache: Option<TempDir>,
}

impl Proxy {
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/playback", self.port)
    }

    fn stop(&self) {
        self.server.stop();
    }

    /// 直接从磁盘元数据读出「已缓存的闭区间」，不经过代理。
    ///
    /// 为什么要绕开 HTTP：判断「缓存写入有没有完成」不能靠再发一个请求——
    /// 那个请求自己就会把缺失的部分补上，于是无论后台任务是否写完，断言
    /// 都会通过。这类测试看着是绿的，实际什么都没测。读磁盘是唯一不会
    /// 干扰被测状态的观测方式。
    ///
    /// 汇总所有 `*.ranges.json`：单个测试里只有一个缓存条目，不必从路径
    /// 反推 key（路径是 key 的 MD5，本来也反推不回来）。
    fn cached_ranges(&self) -> Vec<(u64, u64)> {
        let mut ranges = Vec::new();
        let Ok(level1) = std::fs::read_dir(&self.cache_dir) else {
            return ranges;
        };

        for entry in level1.flatten() {
            let Ok(level2) = std::fs::read_dir(entry.path()) else {
                continue;
            };
            for entry in level2.flatten() {
                let Ok(files) = std::fs::read_dir(entry.path()) else {
                    continue;
                };
                for file in files.flatten() {
                    let path = file.path();
                    if !path.to_string_lossy().ends_with(".ranges.json") {
                        continue;
                    }
                    let Ok(bytes) = std::fs::read(&path) else {
                        continue;
                    };
                    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                        continue;
                    };
                    let Some(completed) = json.get("completed").and_then(|v| v.as_array()) else {
                        continue;
                    };
                    for pair in completed {
                        if let (Some(start), Some(end)) = (
                            pair.get(0).and_then(|v| v.as_u64()),
                            pair.get(1).and_then(|v| v.as_u64()),
                        ) {
                            ranges.push((start, end));
                        }
                    }
                }
            }
        }
        ranges
    }

    /// 磁盘元数据里是否有区间完整覆盖 `[start, end]`。
    fn has_cached(&self, start: u64, end: u64) -> bool {
        self.cached_ranges()
            .iter()
            .any(|&(from, to)| from <= start && to >= end)
    }
}

/// 起一个代理，白名单只放 127.0.0.1。
///
/// `cache` 传 `None` 表示新建临时目录；传 `Some` 可以让两个代理实例共用
/// 同一个缓存目录，用来测重启后的持久化。
fn spawn_proxy(cache: Option<&TempDir>) -> Proxy {
    let (cache_dir, owned) = match cache {
        Some(dir) => (dir.path().to_path_buf(), None),
        None => {
            let dir = tempfile::tempdir().unwrap();
            (dir.path().to_path_buf(), Some(dir))
        }
    };

    let port = free_port();
    let server = Arc::new(ProxyServer::with_config(ProxyConfig {
        port,
        cache_dir: cache_dir.clone(),
        allowed_hosts: vec!["127.0.0.1".to_string()],
        ..Default::default()
    }));

    tokio::spawn({
        let server = server.clone();
        async move { server.start().await }
    });

    Proxy {
        port,
        server,
        cache_dir,
        _cache: owned,
    }
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn wait_until_listening(port: u16) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("代理未在预期时间内开始监听");
}

/// 一次代理请求的参数。字段多且大多有合理默认值，用结构体比长参数列表清楚。
#[derive(Clone)]
struct Ask {
    range: Option<String>,
    asset: Option<(&'static str, &'static str)>,
    method: Method,
    /// 覆盖 `X-Original-Url`。默认用源站的真实地址。
    origin_url: Option<String>,
}

impl Default for Ask {
    fn default() -> Self {
        Self {
            range: None,
            asset: Some(("asset-1", "1")),
            method: Method::GET,
            origin_url: None,
        }
    }
}

impl Ask {
    fn range(mut self, range: &str) -> Self {
        self.range = Some(range.to_string());
        self
    }

    fn head(mut self) -> Self {
        self.method = Method::HEAD;
        self
    }
}

async fn fetch(
    proxy: &Proxy,
    origin: &Origin,
    ask: Ask,
) -> (StatusCode, hyper::HeaderMap, Vec<u8>) {
    wait_until_listening(proxy.port).await;

    let upstream = ask.origin_url.unwrap_or_else(|| origin.url());
    let client = local_http::Client::new();
    let method = ask.method.as_str().parse().expect("无效请求方法");
    let mut builder = client
        .request(method, proxy.url())
        .header("X-Original-Url", upstream);

    if let Some(range) = ask.range {
        builder = builder.header("Range", range);
    }
    if let Some((id, revision)) = ask.asset {
        builder = builder
            .header("X-Cache-Asset-Id", id)
            .header("X-Cache-Asset-Revision", revision);
    }

    let response = builder.send().await.expect("代理请求失败");

    let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
    let mut headers = hyper::HeaderMap::new();
    for (name, value) in response.headers() {
        if let (Ok(name), Ok(value)) = (
            hyper::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            hyper::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            headers.append(name, value);
        }
    }
    let body = response.bytes().await.expect("读取响应体失败").to_vec();

    (status, headers, body)
}

/// 轮询到某个区间确实能命中缓存为止。
///
/// 缓存写入是背景任务，客户端拿完响应体它可能还没落盘。直接 sleep 一个
/// 固定时长既慢又不稳，这里改成「重复请求直到源站计数不再增长」。
async fn wait_until_cached(proxy: &Proxy, origin: &Origin, range: &str) {
    for _ in 0..50 {
        let before = origin.hits();
        let (status, _, _) = fetch(proxy, origin, Ask::default().range(range)).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        if origin.hits() == before {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("区间 {range} 始终没有进入缓存");
}

#[tokio::test]
async fn range_request_is_served_then_cached() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    let (status, headers, body) =
        fetch(&proxy, &origin, Ask::default().range("bytes=0-99999")).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, expected_bytes(0, 99_999));
    // Content-Length 必须等于实际区间长度。问题 2 里报的就是这里announce
    // 了一个和响应体不符的长度。
    assert_eq!(
        headers.get(CONTENT_LENGTH).unwrap().to_str().unwrap(),
        "100000"
    );
    assert!(headers
        .get(CONTENT_RANGE)
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("bytes 0-99999/"));

    // 同一区间再来一次不应该回源。
    wait_until_cached(&proxy, &origin, "bytes=0-99999").await;
    let before = origin.hits();
    let (status, _, body) = fetch(&proxy, &origin, Ask::default().range("bytes=0-99999")).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, expected_bytes(0, 99_999));
    assert_eq!(origin.hits(), before, "命中缓存的请求不应该回源");
}

#[tokio::test]
async fn subrange_of_cached_range_does_not_hit_origin() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    fetch(&proxy, &origin, Ask::default().range("bytes=0-99999")).await;
    wait_until_cached(&proxy, &origin, "bytes=0-99999").await;

    let before = origin.hits();
    let (status, _, body) = fetch(&proxy, &origin, Ask::default().range("bytes=1000-4999")).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, expected_bytes(1_000, 4_999));
    assert_eq!(origin.hits(), before, "子区间应当完全由缓存提供");
}

/// 混合源：一半在缓存里，一半要回源，拼出来必须是连续正确的字节。
///
/// 这条同时是问题 D（混合源不写回缓存）的回归测试：第三次请求如果还回源，
/// 说明网络那一半没有被写进缓存。
#[tokio::test]
async fn mixed_source_stitches_cache_and_network_then_caches_the_new_part() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    fetch(&proxy, &origin, Ask::default().range("bytes=0-99999")).await;
    wait_until_cached(&proxy, &origin, "bytes=0-99999").await;

    let (status, headers, body) =
        fetch(&proxy, &origin, Ask::default().range("bytes=0-199999")).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, expected_bytes(0, 199_999), "缓存段与网络段拼接错误");
    assert_eq!(
        headers.get(CONTENT_LENGTH).unwrap().to_str().unwrap(),
        "200000"
    );

    // 扩展出来的那一段必须也进了缓存。
    wait_until_cached(&proxy, &origin, "bytes=0-199999").await;
    let before = origin.hits();
    let (_, _, body) = fetch(&proxy, &origin, Ask::default().range("bytes=100000-199999")).await;
    assert_eq!(body, expected_bytes(100_000, 199_999));
    assert_eq!(origin.hits(), before, "混合源的网络段没有被写回缓存");
}

/// 缓存段小于 MIN_CACHE_SIZE（8KB）时走的是另一条快路径，它同样要写回缓存。
#[tokio::test]
async fn tiny_cached_prefix_fast_path_still_writes_back() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    fetch(&proxy, &origin, Ask::default().range("bytes=0-4095")).await;
    wait_until_cached(&proxy, &origin, "bytes=0-4095").await;

    let (status, _, body) = fetch(&proxy, &origin, Ask::default().range("bytes=0-49999")).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, expected_bytes(0, 49_999));

    wait_until_cached(&proxy, &origin, "bytes=0-49999").await;
    let before = origin.hits();
    let (_, _, body) = fetch(&proxy, &origin, Ask::default().range("bytes=10000-49999")).await;
    assert_eq!(body, expected_bytes(10_000, 49_999));
    assert_eq!(origin.hits(), before, "快路径没有把网络段写回缓存");
}

/// 并发同区间请求只回源一次。这是问题 7（请求去重）的验收条件。
///
/// 合并之前这里是 8 路各开一条上游连接，其中 7 路纯浪费，而且它们的缓存
/// 写入任务全都卡在抢同一把 key 写锁上，最后被 tee 的宽限期判定为
/// 「缓存侧阻塞」而放弃——下载了却没存下来。
#[tokio::test]
async fn concurrent_identical_ranges_are_merged_into_one_upstream_fetch() {
    // 必须用 Slow：源站瞬间返回时这 8 路很可能是依次跑完的，后面几路直接
    // 缓存命中，「只回源一次」即使合并没生效也成立。延迟保证它们真的重叠。
    let origin = spawn_origin(OriginMode::Slow);
    let proxy = spawn_proxy(None);
    wait_until_listening(proxy.port).await;

    let requests = (0..8).map(|_| fetch(&proxy, &origin, Ask::default().range("bytes=0-99999")));

    let results = tokio::time::timeout(
        Duration::from_secs(30),
        futures_util::future::join_all(requests),
    )
    .await
    .expect("并发同区间请求卡死");

    // 正确性优先：合并做错最容易表现为某几路拿到截断或错位的字节。
    for (status, _, body) in results {
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(body, expected_bytes(0, 99_999));
    }

    // 这才是合并本身的断言。没有合并时 8 路各开一条上游连接。
    assert_eq!(
        origin.hits(),
        1,
        "同一区间的 8 路并发请求应当只回源一次，实际 {} 次",
        origin.hits()
    );
}

/// 模拟播放器 seek：几段互不相邻的区间，每段都必须是自己那一段。
#[tokio::test]
async fn disjoint_seeks_each_return_their_own_bytes() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    for (start, end) in [
        (0u64, 8_191u64),
        (200_000, 209_999),
        (100_000, 104_999),
        (295_000, ORIGIN_SIZE - 1),
    ] {
        let range = format!("bytes={}-{}", start, end);
        let (status, headers, body) = fetch(&proxy, &origin, Ask::default().range(&range)).await;

        assert_eq!(status, StatusCode::PARTIAL_CONTENT, "range {range}");
        assert_eq!(body, expected_bytes(start, end), "range {range} 字节错位");
        assert_eq!(
            headers.get(CONTENT_LENGTH).unwrap().to_str().unwrap(),
            (end - start + 1).to_string(),
            "range {range} 长度不符"
        );
    }
}

/// 不带 `Range` 的普通 GET 必须得到 200，而不是 206。
///
/// RFC 7233：206 只用于回应带 `Range` 的请求。内部管线一律按范围处理（缺
/// `Range` 时合成 `bytes=0-`），所以这条很容易退化成 206——退化之后 Safari
/// 和 AVPlayer 的首个探路请求就会拿到一个不合规的响应。
///
/// `Accept-Ranges` 在 200 里必须存在：206 自身隐含了范围支持，而一个不带这个
/// 头的 200 会被播放器当成不可 seek 的资源，后续 seek 请求根本不会发出来。
#[tokio::test]
async fn plain_get_without_range_gets_200_and_the_whole_file() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    // Ask::default() 的 range 是 None，fetch 不会加 Range 头。
    let (status, headers, body) = fetch(&proxy, &origin, Ask::default()).await;

    assert_eq!(status, StatusCode::OK, "不带 Range 的 GET 应当返回 200");
    assert!(
        !headers.contains_key(CONTENT_RANGE),
        "200 响应不该带 Content-Range，实际: {:?}",
        headers.get(CONTENT_RANGE)
    );
    assert_eq!(
        headers.get(CONTENT_LENGTH).unwrap().to_str().unwrap(),
        ORIGIN_SIZE.to_string(),
        "Content-Length 应当是整个资源的长度"
    );
    assert_eq!(
        headers.get(ACCEPT_RANGES).unwrap().to_str().unwrap(),
        "bytes",
        "200 响应必须声明 Accept-Ranges，否则播放器不会去 seek"
    );
    assert_eq!(body, expected_bytes(0, ORIGIN_SIZE - 1), "整文件字节不对");
}

#[tokio::test]
async fn head_reports_metadata_without_downloading_or_caching_the_media_body() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    let (status, headers, body) = fetch(&proxy, &origin, Ask::default().head()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
    assert_eq!(headers[CONTENT_LENGTH], ORIGIN_SIZE.to_string());
    assert_eq!(origin.hits(), 1, "冷 HEAD 只应发起一次 0-0 探测");

    let (_, _, second_body) = fetch(&proxy, &origin, Ask::default().head()).await;
    assert!(second_body.is_empty());
    assert_eq!(origin.hits(), 1, "已有元数据时 HEAD 不应再次回源");

    let (_, _, get_body) = fetch(&proxy, &origin, Ask::default().range("bytes=0-1023")).await;
    assert_eq!(get_body.len(), 1024);
    assert_eq!(origin.hits(), 2, "HEAD 不应把媒体字节误标记为已缓存");
}

#[tokio::test]
async fn ranged_head_returns_206_headers_and_no_body() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    let (status, headers, body) = fetch(
        &proxy,
        &origin,
        Ask::default().head().range("bytes=100-199"),
    )
    .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert!(body.is_empty());
    assert_eq!(headers[CONTENT_LENGTH], "100");
    assert_eq!(
        headers[CONTENT_RANGE],
        format!("bytes 100-199/{ORIGIN_SIZE}")
    );
}

#[tokio::test]
async fn unsupported_multiple_ranges_return_416() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);
    let (status, _, body) = fetch(&proxy, &origin, Ask::default().range("bytes=0-9,20-29")).await;

    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(body, b"Requested range not satisfiable");
    assert_eq!(origin.hits(), 0, "无效多 Range 不能到达上游");
}

/// 与上一条成对：客户端**明确**发了 `Range` 就必须拿到 206，即使那个区间
/// 恰好覆盖整个资源。`bytes=0-` 和「没有 Range 头」在内部是同一个字符串，
/// 改写逻辑要是照字符串判断就会把这一条也错改成 200。
#[tokio::test]
async fn explicit_full_range_still_gets_206() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    let (status, headers, body) = fetch(&proxy, &origin, Ask::default().range("bytes=0-")).await;

    assert_eq!(
        status,
        StatusCode::PARTIAL_CONTENT,
        "客户端明确要了范围，必须回 206"
    );
    assert_eq!(
        headers.get(CONTENT_RANGE).unwrap().to_str().unwrap(),
        format!("bytes 0-{}/{}", ORIGIN_SIZE - 1, ORIGIN_SIZE)
    );
    assert_eq!(body, expected_bytes(0, ORIGIN_SIZE - 1));
}

/// 开区间 `bytes=N-`：要一直给到文件末尾。
#[tokio::test]
async fn open_ended_range_reaches_end_of_file() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    let (status, _, body) = fetch(&proxy, &origin, Ask::default().range("bytes=250000-")).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, expected_bytes(250_000, ORIGIN_SIZE - 1));
}

/// 问题 2：上游无视 Range 回 200 时必须拒绝，不能把整个文件当成那一段。
#[tokio::test]
async fn origin_ignoring_range_is_rejected_without_leaking_details() {
    let origin = spawn_origin(OriginMode::IgnoresRange);
    let proxy = spawn_proxy(None);

    let (status, _, body) = fetch(&proxy, &origin, Ask::default().range("bytes=1000-1999")).await;

    assert!(
        status.is_client_error() || status.is_server_error(),
        "上游无视 Range 却返回了 {status}"
    );

    // 响应体不能回显上游地址。
    let text = String::from_utf8_lossy(&body);
    assert!(!text.contains("127.0.0.1"), "响应体泄露了上游地址: {text}");
    assert!(!text.contains("media.mp4"), "响应体泄露了上游路径: {text}");
}

/// 完全不带身份头的请求必须能正常服务。
///
/// 这一条压着 HLS 链路的命门。播放器拿到重写后的 `/proxy/<encoded>` 分片地址
/// 后是**自己**去请求的，不会捎上任何自定义头。一旦把身份头设成硬性要求，
/// 每个 `.ts` 都以 400 收场，整条 HLS 链断在第一个分片上。
///
/// 缺头不影响防别名：缓存键的第一段仍是上游 host+path，见
/// `forged_identity_headers_cannot_alias_across_upstream_resources`。
#[tokio::test]
async fn requests_without_identity_headers_are_served() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    let (status, headers, body) = fetch(
        &proxy,
        &origin,
        Ask {
            asset: None,
            ..Ask::default().range("bytes=0-1023")
        },
    )
    .await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        headers[CONTENT_RANGE],
        format!("bytes 0-1023/{ORIGIN_SIZE}")
    );
    assert_eq!(body, expected_bytes(0, 1_023));
}

/// 只发其中一个身份头是配置错误，必须拒绝。
///
/// 静默忽略已发的那一个会把不同 revision 折叠进同一个缓存条目，正好是身份头
/// 想避免的情况。所以「两个都有」和「两个都没有」都合法，「只有一个」不合法。
#[tokio::test]
async fn exactly_one_identity_header_is_rejected() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);
    wait_until_listening(proxy.port).await;

    for (name, value) in [
        ("X-Cache-Asset-Id", "asset-1"),
        ("X-Cache-Asset-Revision", "1"),
    ] {
        let response = local_http::Client::new()
            .get(proxy.url())
            .header("X-Original-Url", origin.url())
            .header("Range", "bytes=0-1023")
            .header(name, value)
            .send()
            .await
            .expect("代理请求失败");
        assert_eq!(
            response.status().as_u16(),
            StatusCode::BAD_REQUEST.as_u16(),
            "只发 {name} 应被拒绝"
        );
    }

    assert_eq!(origin.hits(), 0, "身份头不完整的请求不该回源");
}

/// 白名单只放了 127.0.0.1，`localhost` 是另一个字符串，必须拒绝。
#[tokio::test]
async fn host_outside_allowlist_is_denied() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    let disguised = origin.url().replace("127.0.0.1", "localhost");
    let (status, _, _) = fetch(
        &proxy,
        &origin,
        Ask {
            origin_url: Some(disguised),
            ..Ask::default().range("bytes=0-1023")
        },
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(origin.hits(), 0, "白名单外的主机不该被访问");
}

#[tokio::test]
async fn non_get_methods_are_rejected_with_allow_header() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    let (status, headers, _) = fetch(
        &proxy,
        &origin,
        Ask {
            method: Method::POST,
            ..Ask::default()
        },
    )
    .await;

    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers.get(ALLOW).unwrap().to_str().unwrap(), "GET, HEAD");
}

/// 重启后缓存还在：这是问题 E（启动索引预热）的端到端形态。
/// 换一个代理实例、同一个缓存目录，命中的区间不应该再回源。
#[tokio::test]
async fn cache_survives_a_proxy_restart() {
    let origin = spawn_origin(OriginMode::Honest);
    let cache = tempfile::tempdir().unwrap();

    let first = spawn_proxy(Some(&cache));
    fetch(&first, &origin, Ask::default().range("bytes=0-99999")).await;
    wait_until_cached(&first, &origin, "bytes=0-99999").await;
    first.stop();

    let second = spawn_proxy(Some(&cache));
    let before = origin.hits();
    let (status, _, body) = fetch(&second, &origin, Ask::default().range("bytes=0-99999")).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, expected_bytes(0, 99_999));
    assert_eq!(origin.hits(), before, "重启后缓存没有被复用");
}

/// 改 revision 必须重新回源，不能拿旧字节糊弄过去。
#[tokio::test]
async fn new_revision_refetches_instead_of_serving_stale_bytes() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    fetch(&proxy, &origin, Ask::default().range("bytes=0-9999")).await;
    wait_until_cached(&proxy, &origin, "bytes=0-9999").await;

    let before = origin.hits();
    let (status, _, body) = fetch(
        &proxy,
        &origin,
        Ask {
            asset: Some(("asset-1", "2")),
            ..Ask::default().range("bytes=0-9999")
        },
    )
    .await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, expected_bytes(0, 9_999));
    assert!(origin.hits() > before, "换了 revision 却没有重新回源");
}

/// 轮询磁盘元数据，等 `[start, end]` 真的落盘。返回是否等到了。
///
/// 和 [`wait_until_cached`] 的差别是关键：那个靠「再请求一次、看源站计数
/// 有没有涨」来判断，这个只读磁盘。验证「客户端断开后缓存仍然写完」只能
/// 用这个——发请求这个动作本身就会把缺失的部分补上，于是不管后台任务是否
/// 写完，断言都会通过。
async fn wait_until_on_disk(proxy: &Proxy, start: u64, end: u64) -> bool {
    for _ in 0..100 {
        if proxy.has_cached(start, end) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// 播放器 seek 时会直接掐掉当前连接。断开的那一段必须继续写完缓存。
///
/// 这是 `forward_upstream` 明确承诺的行为（「客户端断开只停发客户端侧，
/// 缓存侧继续写完，避免留下半截缓存」），但此前没有任何测试压着它。对
/// Media3 来说这条尤其重要：拖动进度条会连续产生若干个「开了又立刻关」的
/// 请求，如果每次都留下半截缓存，缓存永远攒不起来。
///
/// 必须用 `OriginMode::Trickle`：源站把整段一次性返回时，`forward_upstream`
/// 的循环转一轮就喂完了，断开只可能发生在转发**结束之后**，测的就不是
/// 「传输中途断开」。切成小块慢慢吐，断开才落在循环中间。
#[tokio::test]
async fn client_disconnect_midstream_still_finishes_the_cache_write() {
    let origin = spawn_origin(OriginMode::Trickle);
    let proxy = spawn_proxy(None);
    wait_until_listening(proxy.port).await;

    // 必须用裸 socket，不能用 hyper 的 Client。
    //
    // `Client` 带连接池：把 `Response` drop 掉时它会为了复用这条连接而把剩下
    // 的响应体**读干**，代理那边看到的是「客户端老老实实收完了」，根本没有
    // 断开可言。这也是变异测试抓出来的——用 Client 版本时，把实现改成
    // 「客户端断开就中止整个循环」，测试照样绿。
    //
    // 裸 socket 直接 drop，且接收缓冲里还有没读走的数据，内核会发 RST，
    // 这才是播放器 seek 时掐断连接的真实形态。
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", proxy.port))
        .await
        .expect("无法连接代理");

    let request = format!(
        "GET /playback HTTP/1.1\r\n\
         Host: 127.0.0.1:{}\r\n\
         X-Original-Url: {}\r\n\
         X-Cache-Asset-Id: asset-1\r\n\
         X-Cache-Asset-Revision: 1\r\n\
         Range: bytes=0-299999\r\n\
         \r\n",
        proxy.port,
        origin.url()
    );
    socket
        .write_all(request.as_bytes())
        .await
        .expect("请求写入失败");

    // 只读到响应头结束就停手，剩下的响应体一个字节都不读。
    let mut seen = Vec::new();
    let mut buffer = [0u8; 256];
    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
        let read = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buffer))
            .await
            .expect("等响应头超时")
            .expect("读响应头失败");
        assert_ne!(read, 0, "代理在发完响应头前就关闭了连接");
        seen.extend_from_slice(&buffer[..read]);
    }
    let head = String::from_utf8_lossy(&seen);
    assert!(head.contains("206"), "预期 206，实际响应头: {head}");

    // 此刻源站还在一块块地吐，转发循环正跑在中间。掐断。
    drop(socket);

    assert!(
        wait_until_on_disk(&proxy, 0, 299_999).await,
        "客户端中途断开后，缓存写入没有跑完；已落盘的区间: {:?}",
        proxy.cached_ranges()
    );

    // 缓存里既然是完整的，再请求同一区间就不该回源，且字节要对。
    let before = origin.hits();
    let (status, _, body) = fetch(&proxy, &origin, Ask::default().range("bytes=0-299999")).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, expected_bytes(0, 299_999));
    assert_eq!(origin.hits(), before, "断开前写下的缓存没有被复用");
}

/// POC 验收项：「离线重播已完成区间」。
///
/// 源站下线之后，已缓存的区间必须还能播。这条能抓住一类很隐蔽的退化：
/// 命中缓存的路径上仍然去连了一次上游（比如为了拿 Content-Type 或校验
/// 总长度）。平时看不出来，一断网就全线失败。
#[tokio::test]
async fn cached_range_replays_after_origin_goes_offline() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    fetch(&proxy, &origin, Ask::default().range("bytes=0-199999")).await;
    assert!(
        wait_until_on_disk(&proxy, 0, 199_999).await,
        "预热阶段没能把区间写进缓存"
    );

    origin.go_offline();

    // 先证明源站真的下线了。
    //
    // 少了这一步，整个测试是空的：下线万一没生效，后面那些断言会照样通过
    // ——它们只是在对着一个活着的源站做普通请求。这一步是实打实抓到过问题的：
    // 最初这里用 `JoinHandle::abort()`，探测请求依然取回了完整的 10000 字节。
    //
    // 请求一个**没缓存**的区间必须失败，这才说明后面「缓存命中成功」是缓存
    // 的功劳，不是上游还活着的功劳。
    let probe = local_http::Client::new()
        .get(proxy.url())
        .header("X-Original-Url", origin.url())
        .header("X-Cache-Asset-Id", "asset-1")
        .header("X-Cache-Asset-Revision", "1")
        .header("Range", "bytes=250000-259999")
        .send();
    let probe_failed = match tokio::time::timeout(Duration::from_secs(10), probe).await {
        // 连接直接断掉也算「失败」，这是源站消失后最常见的形态。
        Err(_) | Ok(Err(_)) => true,
        Ok(Ok(response)) => !response.status().is_success(),
    };
    assert!(
        probe_failed,
        "源站已下线，未缓存区间却仍然取到了数据——下线没有生效，本测试无效"
    );

    // 基准点取在探测**之后**：离线处理器会先记数再断连接，那一次尝试算在
    // 探测头上。从这里往后，命中缓存的请求一次都不该再碰源站。
    let hits_before = origin.hits();

    // 整段重播。
    let (status, _, body) = fetch(&proxy, &origin, Ask::default().range("bytes=0-199999")).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT, "离线后整段重播失败");
    assert_eq!(body, expected_bytes(0, 199_999));

    // 段内 seek。
    let (status, _, body) =
        fetch(&proxy, &origin, Ask::default().range("bytes=50000-149999")).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT, "离线后段内 seek 失败");
    assert_eq!(body, expected_bytes(50_000, 149_999));

    assert_eq!(
        origin.hits(),
        hits_before,
        "源站已下线却还收到了请求，说明缓存命中路径仍在连上游"
    );
}
/// POC 验收项：「signed URL 变化后的第二次播放命中同一缓存身份」。
///
/// 签名 URL 每次下发的 query 都不一样（token、过期时间、签名）。缓存键只取
/// scheme+host+port+path，query 被丢掉，所以换了签名不该重新下载整个文件。
///
/// `data_request.rs` 里有对应的单元测试，但那只验证了「两个 key 字符串相等」。
/// 这里走完整链路，确认真的没有回源——中间任何一层把 query 带进路径、或者
/// 按完整 URL 建目录，单元测试都照样绿，只有端到端能抓到。
#[tokio::test]
async fn rotated_signed_url_hits_the_same_cache_entry() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    let first_url = format!("{}?token=aaaa&expires=1000", origin.url());
    fetch(
        &proxy,
        &origin,
        Ask {
            origin_url: Some(first_url),
            ..Ask::default().range("bytes=0-99999")
        },
    )
    .await;
    assert!(
        wait_until_on_disk(&proxy, 0, 99_999).await,
        "预热阶段没能把区间写进缓存"
    );

    // 换一个完全不同的签名，path 不变。
    let rotated_url = format!("{}?token=zzzz&expires=9999&sig=deadbeef", origin.url());
    let before = origin.hits();
    let (status, _, body) = fetch(
        &proxy,
        &origin,
        Ask {
            origin_url: Some(rotated_url),
            ..Ask::default().range("bytes=0-99999")
        },
    )
    .await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, expected_bytes(0, 99_999));
    assert_eq!(
        origin.hits(),
        before,
        "换了 signed URL 就重新回源了，说明 query 被算进了缓存身份"
    );
}

/// POC 验收项：「两个播放器并发请求重叠区间」。
///
/// 注意这里**不**断言只回源一次：single-flight 只合并完全相同的区间，部分
/// 重叠是各自回源的（见 `single_flight.rs` 里为什么不做区间树）。这条测试
/// 要压住的是另一件事——两个写入任务在同一个缓存文件上交错写时，返回给
/// 各自客户端的字节必须都是对的，不能串味。
#[tokio::test]
async fn two_players_requesting_overlapping_ranges_both_get_correct_bytes() {
    let origin = spawn_origin(OriginMode::Slow);
    let proxy = spawn_proxy(None);
    wait_until_listening(proxy.port).await;

    let first = fetch(&proxy, &origin, Ask::default().range("bytes=0-149999"));
    let second = fetch(&proxy, &origin, Ask::default().range("bytes=100000-249999"));

    let (first, second) = tokio::time::timeout(
        Duration::from_secs(30),
        futures_util::future::join(first, second),
    )
    .await
    .expect("并发重叠区间请求卡死");

    assert_eq!(first.0, StatusCode::PARTIAL_CONTENT);
    assert_eq!(first.2, expected_bytes(0, 149_999), "第一路拿到的字节不对");

    assert_eq!(second.0, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        second.2,
        expected_bytes(100_000, 249_999),
        "第二路拿到的字节不对"
    );
}

/// 模拟 Media3/ExoPlayer 的实际请求形态：一串开区间 seek。
///
/// ExoPlayer 打开媒体后发的是 `bytes=N-`（要到文件末尾，靠关闭连接来停），
/// 每次 seek 就换一个 N 重新发。这里按「开头 → 往后跳 → 跳回已缓存区 →
/// 跳到未缓存区」走一遍，每一步都校验字节。
///
/// 前面那些测试大多是闭区间 `bytes=a-b`，而开区间走的是另一条长度收敛路径
/// （`OPEN_ENDED` 哨兵 + `resolve_range`），这条链路上的 off-by-one 只有用
/// 开区间才碰得到。
#[tokio::test]
async fn media3_style_open_ended_seek_sequence_returns_correct_bytes() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    // 起播，然后依次 seek。最后一个 offset 落在第二次 seek 已经缓存过的区域。
    for offset in [0u64, 200_000, 120_000, 260_000, 200_000] {
        let (status, headers, body) = fetch(
            &proxy,
            &origin,
            Ask::default().range(&format!("bytes={}-", offset)),
        )
        .await;

        assert_eq!(
            status,
            StatusCode::PARTIAL_CONTENT,
            "seek 到 {offset} 时状态码不对"
        );
        assert_eq!(
            body,
            expected_bytes(offset, ORIGIN_SIZE - 1),
            "seek 到 {offset} 时字节不对"
        );

        // 开区间也必须报出准确的总长度和结束位置，播放器靠它算时长。
        let content_range = headers
            .get(CONTENT_RANGE)
            .unwrap_or_else(|| panic!("seek 到 {offset} 时缺少 Content-Range"))
            .to_str()
            .unwrap();
        assert_eq!(
            content_range,
            format!("bytes {}-{}/{}", offset, ORIGIN_SIZE - 1, ORIGIN_SIZE),
            "seek 到 {offset} 时 Content-Range 不对"
        );
    }
}

/// 百分号编码一个上游 URL，够用就好：只处理会出现在测试地址里的字符。
///
/// 不引 `urlencoding` 是为了让这个测试只依赖被测 crate 的公开行为。
fn percent_encode(url: &str) -> String {
    url.chars()
        .map(|c| match c {
            ':' => "%3A".to_string(),
            '/' => "%2F".to_string(),
            '?' => "%3F".to_string(),
            '=' => "%3D".to_string(),
            '&' => "%26".to_string(),
            other => other.to_string(),
        })
        .collect()
}

/// HLS 链路的回归测试：播放器请求重写后的分片地址时**不会带任何自定义头**。
///
/// `rewrite_m3u8` 把分片改写成 `/proxy/<encoded-url>`，播放器随后自己去请求
/// 这个地址。它不知道 `X-Cache-Asset-Id` / `X-Cache-Asset-Revision` 的存在，
/// 也没有任何机制让它捎上——所以只要缓存键把这两个头当成硬性要求，每一个
/// 分片都会以 400 收场，整条 HLS 链路断在第一个 `.ts` 上。m3u8 本身还是好的
/// （那条路径不取缓存键），于是故障表现为「播放列表能拿到，一个分片都放不出来」。
///
/// 这条测试直接打 `/proxy/<encoded>`，一个自定义头都不发，压住缓存键必须能
/// 只靠上游 URL 构造出来。
#[tokio::test]
async fn proxy_path_without_identity_headers_serves_bytes() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);
    wait_until_listening(proxy.port).await;

    let uri = format!(
        "http://127.0.0.1:{}/proxy/{}",
        proxy.port,
        percent_encode(&origin.url())
    );

    let response = local_http::Client::new()
        .get(&uri)
        .header("Range", "bytes=0-4095")
        .send()
        .await
        .expect("代理请求失败");
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
    let body = response.bytes().await.expect("读取响应体失败").to_vec();

    assert_eq!(
        status,
        StatusCode::PARTIAL_CONTENT,
        "播放器风格的分片请求（无自定义头）被拒绝了"
    );
    assert_eq!(body, expected_bytes(0, 4_095));
}

/// 同一个上游 URL，带身份头与不带身份头必须落在**不同**的缓存条目上。
///
/// 不带头时缓存键只有上游身份，带头时在其后追加了两段。两者不相等是设计
/// 意图：调用方发身份头就是为了按 revision 隔离，绝不能因为「省略即等价」
/// 而让新旧 revision 共用一条缓存。
#[tokio::test]
async fn identity_headers_still_partition_the_cache() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);
    wait_until_listening(proxy.port).await;

    // 先用带身份头的请求把区间缓存起来。
    fetch(&proxy, &origin, Ask::default().range("bytes=0-4095")).await;
    wait_until_cached(&proxy, &origin, "bytes=0-4095").await;

    // 同一区间、同一上游，但走不带头的 /proxy/ 路径：必须重新回源。
    let before = origin.hits();
    let response = local_http::Client::new()
        .get(format!(
            "http://127.0.0.1:{}/proxy/{}",
            proxy.port,
            percent_encode(&origin.url())
        ))
        .header("Range", "bytes=0-4095")
        .send()
        .await
        .expect("代理请求失败");
    assert_eq!(
        response.status().as_u16(),
        StatusCode::PARTIAL_CONTENT.as_u16()
    );
    let body = response.bytes().await.expect("读取响应体失败").to_vec();

    assert_eq!(body, expected_bytes(0, 4_095));
    assert!(
        origin.hits() > before,
        "无身份头的请求命中了带身份头写下的缓存，键没有真正隔离"
    );
}

/// `bytes=-N`：RFC 7233 的后缀范围，取资源末尾 N 字节。
///
/// 这是播放器探测 MP4 尾部 moov box 的标准手段——索引在文件尾时，播放器
/// 开场第一个请求就是 `bytes=-N`。原先的解析器把它判为非法（起点为空串
/// → "Invalid start position"），这类资源直接起播失败。
#[tokio::test]
async fn suffix_range_serves_the_last_bytes() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    let (status, headers, body) = fetch(&proxy, &origin, Ask::default().range("bytes=-1024")).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT, "后缀范围被拒绝了");
    assert_eq!(
        body,
        expected_bytes(ORIGIN_SIZE - 1024, ORIGIN_SIZE - 1),
        "后缀范围返回的不是末尾 1024 字节"
    );
    assert_eq!(
        headers[CONTENT_RANGE].to_str().unwrap(),
        format!(
            "bytes {}-{}/{}",
            ORIGIN_SIZE - 1024,
            ORIGIN_SIZE - 1,
            ORIGIN_SIZE
        )
    );
    assert_eq!(headers[CONTENT_LENGTH].to_str().unwrap(), "1024");
}

/// 后缀长度超过资源大小时，RFC 7233 要求返回整个资源，而不是 416。
#[tokio::test]
async fn oversized_suffix_range_serves_the_whole_resource() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    let (status, headers, body) = fetch(
        &proxy,
        &origin,
        Ask::default().range(&format!("bytes=-{}", ORIGIN_SIZE * 2)),
    )
    .await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, expected_bytes(0, ORIGIN_SIZE - 1));
    assert_eq!(
        headers[CONTENT_RANGE].to_str().unwrap(),
        format!("bytes 0-{}/{}", ORIGIN_SIZE - 1, ORIGIN_SIZE)
    );
}

/// HEAD 也要懂后缀范围：走的是 `process_head`，与 GET 是两条独立的代码路径。
#[tokio::test]
async fn head_with_suffix_range_reports_the_tail() {
    let origin = spawn_origin(OriginMode::Honest);
    let proxy = spawn_proxy(None);

    let (status, headers, body) =
        fetch(&proxy, &origin, Ask::default().range("bytes=-2048").head()).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert!(body.is_empty(), "HEAD 不该带响应体");
    assert_eq!(headers[CONTENT_LENGTH].to_str().unwrap(), "2048");
    assert_eq!(
        headers[CONTENT_RANGE].to_str().unwrap(),
        format!(
            "bytes {}-{}/{}",
            ORIGIN_SIZE - 2048,
            ORIGIN_SIZE - 1,
            ORIGIN_SIZE
        )
    );
}
