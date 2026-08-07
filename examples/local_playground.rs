//! 本地联调场：一条命令同时起假源站和代理，然后打印可直接粘贴的请求命令。
//!
//! 解决的是「怎么真实地测一把」这个问题。自动化测试在
//! `tests/end_to_end.rs`，这个例子是给手动验证用的：想拿真实播放器
//! （ffplay / mpv / VLC）看一眼实际播放效果时用它。
//!
//! ```sh
//! # 合成字节，只验协议正确性
//! cargo run --features allow-private-upstream --example local_playground
//!
//! # 用真实媒体文件，可以真的播
//! cargo run --features allow-private-upstream --example local_playground -- /path/to/video.mp4
//! ```
//!
//! 需要 `allow-private-upstream`：假源站在 127.0.0.1 上，生产逻辑会拒绝
//! 回环地址的上游。

#[cfg(feature = "allow-private-upstream")]
mod playground {
    use std::collections::HashMap;
    use std::convert::Infallible;
    use std::net::{SocketAddr, TcpListener};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;

    use hyper::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE};
    use hyper::service::{make_service_fn, service_fn};
    use hyper::{Body, Client, Request, Response, Server, StatusCode};
    use proxy_server::server::{ProxyConfig, ProxyServer};

    /// 没给真实文件时合成多大。5MB 足够让播放器发出好几轮 range 请求。
    const SYNTHETIC_SIZE: usize = 5 * 1024 * 1024;

    struct HlsAsset {
        path: PathBuf,
        content_type: &'static str,
    }

    pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
        proxy_server::utils::logger::init_from_env();

        let path = std::env::args().nth(1).map(PathBuf::from);
        let (body, content_type, label) = match &path {
            Some(path) => {
                // 整个读进内存。联调工具，几百 MB 也无所谓，换来的是
                // range 切片这段代码足够短、不会自己成为怀疑对象。
                let bytes = std::fs::read(path)?;
                let content_type = guess_content_type(path);
                let label = format!("{}（{} 字节）", path.display(), bytes.len());
                (bytes, content_type, label)
            }
            None => {
                let bytes: Vec<u8> = (0..SYNTHETIC_SIZE).map(|i| (i % 251) as u8).collect();
                (
                    bytes,
                    "application/octet-stream",
                    format!("合成字节（{} 字节，第 n 字节 = n % 251）", SYNTHETIC_SIZE),
                )
            }
        };

        let (hls_temp_dir, hls_assets) = match path.as_deref() {
            Some(path) => {
                let (dir, assets) = generate_hls(path)?;
                (Some(dir), assets)
            }
            None => (None, Arc::new(HashMap::new())),
        };
        let port = 8099;
        let origin = spawn_origin(Arc::new(body), content_type, hls_assets, port);
        let origin_url = format!("{origin}/media.mp4");

        let cache_dir = std::env::temp_dir().join("video-proxy-playground");
        let server = Arc::new(ProxyServer::with_config(ProxyConfig {
            port,
            cache_dir: cache_dir.clone(),
            allowed_hosts: vec!["127.0.0.1".to_string()],
            ..Default::default()
        }));

        print_usage(&label, &origin_url, port, &cache_dir);

        if let Err(e) = server.start().await {
            eprintln!("代理退出: {e}");
        }
        drop(hls_temp_dir);
        Ok(())
    }

    fn guess_content_type(path: &Path) -> &'static str {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "mp4" | "m4v" | "m4a" => "video/mp4",
            "webm" => "video/webm",
            "mp3" => "audio/mpeg",
            "flac" => "audio/flac",
            "ts" => "video/mp2t",
            "m3u8" => "application/vnd.apple.mpegurl",
            _ => "application/octet-stream",
        }
    }

    fn generate_hls(
        path: &Path,
    ) -> Result<(tempfile::TempDir, Arc<HashMap<String, HlsAsset>>), Box<dyn std::error::Error>>
    {
        let output_dir = tempfile::Builder::new()
            .prefix("video-proxy-hls-playground-")
            .tempdir()?;

        let status = Command::new("ffmpeg")
            .arg("-y")
            .arg("-v")
            .arg("error")
            .arg("-i")
            .arg(path)
            .args(["-map", "0:v:0", "-map", "0:a:0?", "-c", "copy"])
            .args([
                "-hls_time",
                "4",
                "-hls_playlist_type",
                "vod",
                "-hls_segment_type",
                "fmp4",
                "-hls_flags",
                "independent_segments",
                "-hls_fmp4_init_filename",
                "init.mp4",
            ])
            .arg("-hls_segment_filename")
            .arg(output_dir.path().join("segment-%03d.m4s"))
            .arg(output_dir.path().join("master.m3u8"))
            .status()
            .map_err(|e| format!("无法启动 ffmpeg 生成 HLS: {e}"))?;
        if !status.success() {
            return Err("ffmpeg 生成 HLS 失败".into());
        }

        let mut assets = HashMap::new();
        for entry in std::fs::read_dir(output_dir.path())? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let content_type = match path.extension().and_then(|ext| ext.to_str()) {
                Some("m3u8") => "application/vnd.apple.mpegurl",
                Some("mp4") | Some("m4s") => "video/mp4",
                _ => continue,
            };
            assets.insert(name.to_string(), HlsAsset { path, content_type });
        }
        println!("[HLS] 已生成 {} 个播放列表/分片文件", assets.len());
        Ok((output_dir, Arc::new(assets)))
    }

    fn spawn_origin(
        body: Arc<Vec<u8>>,
        content_type: &'static str,
        hls_assets: Arc<HashMap<String, HlsAsset>>,
        proxy_port: u16,
    ) -> String {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("假源站无法绑定端口");
        let addr: SocketAddr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let origin_url = format!("{base_url}/media.mp4");
        let hls_url = format!("{base_url}/hls/master.m3u8");
        let proxy_url = format!("http://127.0.0.1:{proxy_port}/playback");
        let client = Client::new();

        let make_svc = make_service_fn(move |_conn| {
            let body = body.clone();
            let client = client.clone();
            let origin_url = origin_url.clone();
            let hls_url = hls_url.clone();
            let hls_assets = hls_assets.clone();
            let proxy_url = proxy_url.clone();
            async move {
                Ok::<_, Infallible>(service_fn(move |req: Request<Body>| {
                    let body = body.clone();
                    let client = client.clone();
                    let origin_url = origin_url.clone();
                    let hls_url = hls_url.clone();
                    let hls_assets = hls_assets.clone();
                    let proxy_url = proxy_url.clone();
                    async move {
                        Ok::<_, Infallible>(
                            serve_web_or_media(
                                req,
                                &body,
                                content_type,
                                &hls_assets,
                                &client,
                                &origin_url,
                                &hls_url,
                                &proxy_url,
                            )
                            .await,
                        )
                    }
                }))
            }
        });

        tokio::spawn(async move {
            let _ = Server::from_tcp(listener)
                .expect("假源站无法接管 listener")
                .serve(make_svc)
                .await;
        });

        base_url
    }

    async fn serve_web_or_media(
        req: Request<Body>,
        body: &[u8],
        content_type: &'static str,
        hls_assets: &HashMap<String, HlsAsset>,
        client: &Client<hyper::client::HttpConnector>,
        origin_url: &str,
        hls_url: &str,
        proxy_url: &str,
    ) -> Response<Body> {
        match req.uri().path() {
            "/" | "/index.html" => static_response(
                "text/html; charset=utf-8",
                include_str!("../web-test/index.html"),
            ),
            "/styles.css" => static_response(
                "text/css; charset=utf-8",
                include_str!("../web-test/styles.css"),
            ),
            "/app.js" => static_response(
                "text/javascript; charset=utf-8",
                include_str!("../web-test/app.js"),
            ),
            "/media.mp4" => serve(&req, body, content_type),
            "/playback" => {
                forward_to_proxy(req, client, proxy_url, Some(origin_url), "aa-video").await
            }
            "/hls-playback" => {
                forward_to_proxy(req, client, proxy_url, Some(hls_url), "aa-hls-playlist").await
            }
            path if path.starts_with("/proxy/") => {
                let target = format!("{}{path}", proxy_url.trim_end_matches("/playback"));
                forward_to_proxy(req, client, &target, None, "aa-hls-resource").await
            }
            path if path.starts_with("/hls/") => {
                let name = path.trim_start_matches("/hls/");
                match hls_assets.get(name) {
                    Some(asset) => match tokio::fs::read(&asset.path).await {
                        Ok(bytes) => serve_named(&req, &bytes, asset.content_type, name),
                        Err(_) => Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .body(Body::from("Unable to read HLS asset"))
                            .unwrap(),
                    },
                    None => Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Body::from("HLS asset not found"))
                        .unwrap(),
                }
            }
            _ => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Not found"))
                .unwrap(),
        }
    }

    fn static_response(content_type: &'static str, content: &'static str) -> Response<Body> {
        Response::builder()
            .header(CONTENT_TYPE, content_type)
            .header("Cache-Control", "no-store")
            .body(Body::from(content))
            .unwrap()
    }

    async fn forward_to_proxy(
        browser_request: Request<Body>,
        client: &Client<hyper::client::HttpConnector>,
        proxy_url: &str,
        original_url: Option<&str>,
        asset_id: &str,
    ) -> Response<Body> {
        let mut builder = Request::builder()
            .method(browser_request.method())
            .uri(proxy_url)
            .header("X-Cache-Asset-Id", asset_id)
            .header("X-Cache-Asset-Revision", "1");
        if let Some(original_url) = original_url {
            builder = builder.header("X-Original-Url", original_url);
        }
        if let Some(range) = browser_request.headers().get(RANGE) {
            builder = builder.header(RANGE, range);
        }

        let request = match builder.body(Body::empty()) {
            Ok(request) => request,
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Body::from("Invalid test request"))
                    .unwrap()
            }
        };

        match client.request(request).await {
            Ok(response) => response,
            Err(error) => Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("Proxy unavailable: {error}")))
                .unwrap(),
        }
    }

    fn serve(req: &Request<Body>, body: &[u8], content_type: &'static str) -> Response<Body> {
        serve_named(req, body, content_type, "media.mp4")
    }

    fn serve_named(
        req: &Request<Body>,
        body: &[u8],
        content_type: &'static str,
        name: &str,
    ) -> Response<Body> {
        let total = body.len() as u64;
        let range = req
            .headers()
            .get(RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| parse_range(value, total));

        // 打印每一次回源，这样能直接看出哪些区间命中了缓存、哪些穿透了。
        match range {
            Some((start, end)) => println!("[源站] {name} range {start}-{end}"),
            None => println!("[源站] {name} 整个文件"),
        }

        match range {
            Some((start, end)) => Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(CONTENT_TYPE, content_type)
                .header(ACCEPT_RANGES, "bytes")
                .header(CONTENT_LENGTH, end - start + 1)
                .header(CONTENT_RANGE, format!("bytes {start}-{end}/{total}"))
                .body(Body::from(body[start as usize..=end as usize].to_vec()))
                .unwrap(),
            None => Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, content_type)
                .header(ACCEPT_RANGES, "bytes")
                .header(CONTENT_LENGTH, total)
                .body(Body::from(body.to_vec()))
                .unwrap(),
        }
    }

    fn parse_range(value: &str, total: u64) -> Option<(u64, u64)> {
        let spec = value.strip_prefix("bytes=")?;
        let (start, end) = spec.split_once('-')?;
        let start: u64 = start.trim().parse().ok()?;
        let end = match end.trim() {
            "" => total - 1,
            text => text.parse::<u64>().ok()?.min(total - 1),
        };
        (start <= end && start < total).then_some((start, end))
    }

    fn print_usage(label: &str, origin_url: &str, port: u16, cache_dir: &std::path::Path) {
        let headers = format!(
            "-H 'X-Original-Url: {origin_url}' \\\n     -H 'X-Cache-Asset-Id: demo' \\\n     -H 'X-Cache-Asset-Revision: 1'"
        );
        let proxy_url = format!("http://127.0.0.1:{port}/playback");

        println!(
            "
================ 本地联调场 ================
源站内容 : {label}
源站地址 : {origin_url}
代理地址 : {proxy_url}
缓存目录 : {}

注意：三个头都是必需的。X-Original-Url 指定真实上游，
另两个构成缓存身份；缺任何一个都会得到 400。
Web 测试页 : {}/
页面通过同源测试网关补齐缓存身份头，视频播放和拖动都会进入真实代理。

# 1. 取前 100KB。看 Content-Range 和 Content-Length 对不对
curl -si {proxy_url} \\
     -H 'Range: bytes=0-99999' \\
     {headers} | head -n 20

# 2. 再取一次同一段。源站这边不该再打印新的一行，说明命中了缓存
curl -s -o /dev/null {proxy_url} \\
     -H 'Range: bytes=0-99999' \\
     {headers}

# 3. 取一个更大的区间。前一半来自缓存、后一半回源（混合源路径）
curl -s -o /dev/null {proxy_url} \\
     -H 'Range: bytes=0-199999' \\
     {headers}

# 4. 验字节正确性：代理取的和源站直取的必须一模一样
diff <(curl -s {proxy_url} -H 'Range: bytes=50000-149999' {headers}) \\
     <(curl -s '{origin_url}' -H 'Range: bytes=50000-149999') \\
  && echo '字节一致'

# 5. 用真实播放器播（需要给了真实媒体文件才有意义）
ffplay -headers $'X-Original-Url: {origin_url}\\r\\nX-Cache-Asset-Id: demo\\r\\nX-Cache-Asset-Revision: 1\\r\\n' '{proxy_url}'

mpv --http-header-fields='X-Original-Url: {origin_url},X-Cache-Asset-Id: demo,X-Cache-Asset-Revision: 1' '{proxy_url}'

播放时在进度条上来回拖动，观察源站打印的 range：
已经缓存过的区间不应该再出现。

Ctrl-C 结束。
===========================================
",
            cache_dir.display(),
            origin_url.trim_end_matches("/media.mp4")
        );
    }
}

#[cfg(feature = "allow-private-upstream")]
#[tokio::main]
async fn main() {
    if let Err(error) = playground::run().await {
        eprintln!("本地联调场启动失败: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(feature = "allow-private-upstream"))]
fn main() {
    eprintln!(
        "这个例子需要 allow-private-upstream feature：假源站绑在 127.0.0.1 上，\n\
         而默认构建会拒绝回环地址的上游（SSRF 防护）。\n\n\
         请改用：\n\
         \x20 cargo run --features allow-private-upstream --example local_playground"
    );
    std::process::exit(1);
}
