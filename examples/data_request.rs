use hyper::{Client, Request, Body};
use hyper_tls::HttpsConnector;
use proxy_server::{log_info, server};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 启动代理服务器
    tokio::spawn(async {
        log_info!("Example", "启动代理服务器...");
        // 默认策略是 deny-all，必须显式放行本例要访问的上游主机。
        let server =
            server::ProxyServer::with_allowed_hosts(8080, "./cache", ["www.w3school.com.cn"]);
        if let Err(e) = server.start().await {
            eprintln!("代理服务器退出: {}", e);
        }
    });

    // 等待服务器启动
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let https = HttpsConnector::new();
    let client = Client::builder().build::<_, hyper::Body>(https);

    // 测试视频 URL
    let video_url = "https://www.w3school.com.cn/i/movie.mp4";

    log_info!("Example", "1. 普通请求（无 Range）");
    let req1 = Request::builder()
        .method("GET")
        .uri("http://127.0.0.1:8080")
        .header("X-Original-Url", video_url)
        .body(Body::empty())?;

    let resp1 = client.request(req1).await?;
    log_info!("Example", "响应状态: {}", resp1.status());
    log_info!("Example", "响应头: {:#?}", resp1.headers());

    log_info!("Example", "2. Range 请求（前 10KB）");
    let req2 = Request::builder()
        .method("GET")
        .uri("http://127.0.0.1:8080")
        .header("Range", "bytes=0-10240")
        .header("X-Original-Url", video_url)
        .body(Body::empty())?;

    let resp2 = client.request(req2).await?;
    log_info!("Example", "响应状态: {}", resp2.status());
    log_info!("Example", "响应头: {:#?}", resp2.headers());

    log_info!("Example", "3. 重复请求（相同的 Range）");
    let req3 = Request::builder()
        .method("GET")
        .uri("http://127.0.0.1:8080")
        .header("Range", "bytes=0-10240")
        .header("X-Original-Url", video_url)
        .body(Body::empty())?;

    let resp3 = client.request(req3).await?;
    log_info!("Example", "响应状态: {}", resp3.status());
    log_info!("Example", "响应头: {:#?}", resp3.headers());

    log_info!("Example", "4. 连续 Range 请求");
    let req4 = Request::builder()
        .method("GET")
        .uri("http://127.0.0.1:8080")
        .header("Range", "bytes=10241-20480")
        .header("X-Original-Url", video_url)
        .body(Body::empty())?;

    let resp4 = client.request(req4).await?;
    log_info!("Example", "响应状态: {}", resp4.status());
    log_info!("Example", "响应头: {:#?}", resp4.headers());

    Ok(())
}
