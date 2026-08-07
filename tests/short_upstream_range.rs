//! 上游没有按请求区间返回时，代理必须仍然发出一个自洽的响应。
//!
//! 这里压的两个场景都不是构造出来的边缘情况，而是真实上游的常见行为：
//!
//! 1. **按块上限截断 range**。CDN 普遍配置「单次 range 响应最多 N 字节」，
//!    请求 2MB 只给 64KB。RFC 7233 允许这么做，代理自己的
//!    `verify_content_range` 也放行 `actual_end < expected_end`。
//! 2. **完全不支持 range**，任何请求都回 200 + 整个文件。
//!
//! 两条路径此前都会产出一个客户端无法消费的响应，症状分别是「读到一半撞
//! EOF」和「明明拿到了整个文件却回 416」。
//!
//! 需要 `allow-private-upstream`：假源站绑在 127.0.0.1 上。
//!
//! ```sh
//! cargo test --features allow-private-upstream --test short_upstream_range
//! ```
#![cfg(feature = "allow-private-upstream")]

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use hyper::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Client, Request, Response, Server, StatusCode};
use proxy_server::server::{ProxyConfig, ProxyServer};

const ORIGIN_SIZE: u64 = 300_007;

/// 假源站单次 range 响应的字节上限，模拟 CDN 的分块配置。
const CAP: u64 = 64 * 1024;

fn byte_at(offset: u64) -> u8 {
    (offset % 251) as u8
}

fn expected_bytes(start: u64, end: u64) -> Vec<u8> {
    (start..=end).map(byte_at).collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OriginMode {
    /// 实现 range，但每次最多返回 [`CAP`] 字节。
    CapsRangeLength,
    /// 完全不认 Range，永远回 200 + 整个文件。
    NoRangeSupport,
}

fn spawn_origin(mode: OriginMode) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let service = make_service_fn(move |_| async move {
        Ok::<_, std::convert::Infallible>(service_fn(move |req: Request<Body>| async move {
            Ok::<_, std::convert::Infallible>(serve(&req, mode))
        }))
    });

    tokio::spawn(async move {
        let _ = Server::from_tcp(listener).unwrap().serve(service).await;
    });

    format!("http://{}/media.mp4", addr)
}

fn serve(req: &Request<Body>, mode: OriginMode) -> Response<Body> {
    let full = || {
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_LENGTH, ORIGIN_SIZE)
            .body(Body::from(expected_bytes(0, ORIGIN_SIZE - 1)))
            .unwrap()
    };

    if mode == OriginMode::NoRangeSupport {
        return full();
    }

    let Some((start, requested_end)) = req
        .headers()
        .get(RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_range)
    else {
        return full();
    };

    // 这就是被测行为：把区间截断到 CAP 字节，并如实声明自己给了多少。
    let end = requested_end.min(start + CAP - 1);

    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_LENGTH, end - start + 1)
        .header(
            CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, end, ORIGIN_SIZE),
        )
        .body(Body::from(expected_bytes(start, end)))
        .unwrap()
}

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

fn spawn_proxy() -> (u16, Arc<ProxyServer>, tempfile::TempDir) {
    let cache = tempfile::tempdir().unwrap();
    let port = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let server = Arc::new(ProxyServer::with_config(ProxyConfig {
        port,
        cache_dir: cache.path().to_path_buf(),
        allowed_hosts: vec!["127.0.0.1".to_string()],
        ..Default::default()
    }));
    tokio::spawn({
        let server = server.clone();
        async move { server.start().await }
    });
    (port, server, cache)
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

async fn fetch(
    port: u16,
    origin_url: &str,
    range: Option<&str>,
) -> (StatusCode, hyper::HeaderMap, Vec<u8>) {
    wait_until_listening(port).await;

    let mut builder = Request::builder()
        .uri(format!("http://127.0.0.1:{}/playback", port))
        .header("X-Original-Url", origin_url)
        .header("X-Cache-Asset-Id", "asset-1")
        .header("X-Cache-Asset-Revision", "1");
    if let Some(range) = range {
        builder = builder.header(RANGE, range);
    }

    let response = Client::new()
        .request(builder.body(Body::empty()).unwrap())
        .await
        .expect("代理请求失败");

    let status = response.status();
    let headers = response.headers().clone();
    // 这一步就是「客户端挂死」的观测点：声明长度大于真实字节数时，
    // 读响应体会以 `end of file before message length reached` 失败。
    let body = tokio::time::timeout(
        Duration::from_secs(10),
        hyper::body::to_bytes(response.into_body()),
    )
    .await
    .expect("读响应体超时")
    .expect("响应体不可读");

    (status, headers, body.to_vec())
}

/// 上游把 range 截短时，`Content-Length` 不能超过真实响应体长度。
#[tokio::test]
async fn capped_upstream_range_is_served_as_an_honest_short_206() {
    let origin = spawn_origin(OriginMode::CapsRangeLength);
    let (port, server, _cache) = spawn_proxy();

    // 请求 200000 字节，上游只会给 CAP（65536）字节。
    let (status, headers, body) = fetch(port, &origin, Some("bytes=0-199999")).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);

    let declared: u64 = headers[CONTENT_LENGTH].to_str().unwrap().parse().unwrap();
    assert_eq!(
        declared,
        body.len() as u64,
        "Content-Length ({}) 必须等于真实响应体长度 ({})",
        declared,
        body.len()
    );
    assert_eq!(body.len() as u64, CAP, "应当如实返回上游给的那一段");
    assert_eq!(body, expected_bytes(0, CAP - 1), "返回的字节内容不对");
    assert_eq!(
        headers[CONTENT_RANGE],
        format!("bytes 0-{}/{}", CAP - 1, ORIGIN_SIZE),
        "Content-Range 必须描述实际返回的区间，播放器据此续请求剩余部分"
    );

    // 续请求剩下的部分必须能正常拿到，否则播放器无法走完整个文件。
    let (status, _, rest) = fetch(port, &origin, Some(&format!("bytes={}-{}", CAP, CAP * 2 - 1))).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(rest, expected_bytes(CAP, CAP * 2 - 1), "续传区间字节不对");

    server.stop();
}

/// 上游完全不支持 range 时，普通 GET 仍应拿到整个文件。
#[tokio::test]
async fn origin_without_range_support_still_serves_a_plain_get() {
    let origin = spawn_origin(OriginMode::NoRangeSupport);
    let (port, server, _cache) = spawn_proxy();

    // 不带 Range 头：内部会合成 `bytes=0-`，而 200 响应没有 Content-Range，
    // 总长度只能从 Content-Length 推。此前这里以 416 收场。
    let (status, headers, body) = fetch(port, &origin, None).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "上游不支持 Range 时，普通 GET 仍应返回整个文件"
    );
    assert_eq!(
        headers[CONTENT_LENGTH].to_str().unwrap(),
        ORIGIN_SIZE.to_string()
    );
    assert_eq!(body.len() as u64, ORIGIN_SIZE);
    assert_eq!(body, expected_bytes(0, ORIGIN_SIZE - 1));

    server.stop();
}
