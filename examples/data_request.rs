mod support;
use support as local_http;

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

    let client = local_http::Client::new();

    // 测试视频 URL
    let video_url = "https://www.w3school.com.cn/i/movie.mp4";

    log_info!("Example", "1. 普通请求（无 Range）");
    let resp1 = client
        .get("http://127.0.0.1:8080")
        .header("X-Original-Url", video_url)
        .header("X-Cache-User-Id", "example-user")
        .header("X-Cache-Asset-Id", "w3school-movie")
        .header("X-Cache-Asset-Revision", "1")
        .send()
        .await?;
    log_info!("Example", "响应状态: {}", resp1.status());
    log_info!("Example", "响应头: {:#?}", resp1.headers());

    log_info!("Example", "2. Range 请求（前 10KB）");
    let resp2 = client
        .get("http://127.0.0.1:8080")
        .header("Range", "bytes=0-10240")
        .header("X-Original-Url", video_url)
        .header("X-Cache-User-Id", "example-user")
        .header("X-Cache-Asset-Id", "w3school-movie")
        .header("X-Cache-Asset-Revision", "1")
        .send()
        .await?;
    log_info!("Example", "响应状态: {}", resp2.status());
    log_info!("Example", "响应头: {:#?}", resp2.headers());

    log_info!("Example", "3. 重复请求（相同的 Range）");
    let resp3 = client
        .get("http://127.0.0.1:8080")
        .header("Range", "bytes=0-10240")
        .header("X-Original-Url", video_url)
        .header("X-Cache-User-Id", "example-user")
        .header("X-Cache-Asset-Id", "w3school-movie")
        .header("X-Cache-Asset-Revision", "1")
        .send()
        .await?;
    log_info!("Example", "响应状态: {}", resp3.status());
    log_info!("Example", "响应头: {:#?}", resp3.headers());

    log_info!("Example", "4. 连续 Range 请求");
    let resp4 = client
        .get("http://127.0.0.1:8080")
        .header("Range", "bytes=10241-20480")
        .header("X-Original-Url", video_url)
        .header("X-Cache-User-Id", "example-user")
        .header("X-Cache-Asset-Id", "w3school-movie")
        .header("X-Cache-Asset-Revision", "1")
        .send()
        .await?;
    log_info!("Example", "响应状态: {}", resp4.status());
    log_info!("Example", "响应头: {:#?}", resp4.headers());

    Ok(())
}
