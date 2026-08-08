mod support;
use support as local_http;

use proxy_server::{log_info, server};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 启动代理服务器
    log_info!("Example", "启动代理服务器...");
    let video_url = "https://media.w3.org/2010/05/sintel/trailer.mp4";
    let server = server::ProxyServer::with_allowed_hosts(8080, "./cache", ["media.w3.org"]);

    // 在新线程中启动服务器
    tokio::spawn(async move {
        if let Err(e) = server.start().await {
            eprintln!("服务器错误: {}", e);
        }
    });

    // 等待服务器启动
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // 创建 HTTP 客户端
    let client = local_http::Client::new();

    // 第一次请求：获取前 5MB
    log_info!("Example", "第一次请求: 0-1MB");
    let resp1 = client
        .get("http://127.0.0.1:8080")
        .header("Range", "bytes=0-10240")
        .header("X-Original-Url", video_url)
        .header("X-Cache-User-Id", "example-user")
        .header("X-Cache-Asset-Id", "sintel-trailer")
        .header("X-Cache-Asset-Revision", "1")
        .send()
        .await?;
    log_info!("Example", "第一次响应状态: {}", resp1.status());
    log_info!("Example", "第一次响应头: {:#?}", resp1.headers());

    // 等待一秒
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // 第二次请求：获取接下来的 5MB
    log_info!("Example", "第二次请求: 1MB-10MB");
    let resp2 = client
        .get("http://127.0.0.1:8080")
        .header("Range", "bytes=10240-102400")
        .header("X-Original-Url", video_url)
        .header("X-Cache-User-Id", "example-user")
        .header("X-Cache-Asset-Id", "sintel-trailer")
        .header("X-Cache-Asset-Revision", "1")
        .send()
        .await?;
    log_info!("Example", "第二次响应状态: {}", resp2.status());
    log_info!("Example", "第二次响应头: {:#?}", resp2.headers());

    Ok(())
}
