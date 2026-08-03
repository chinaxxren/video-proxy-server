use hyper::{Client, Request, Body};
use proxy_server::{log_info, server};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 启动代理服务器
    log_info!("Example", "启动代理服务器...");
    let server = server::ProxyServer::new(8080, "./cache");
    
    // 在新线程中启动服务器
    tokio::spawn(async move {
        if let Err(e) = server.start().await {
            eprintln!("服务器错误: {}", e);
        }
    });

    // 等待服务器启动
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // 创建 HTTP 客户端
    let client = Client::new();
    let video_url = "https://media.w3.org/2010/05/sintel/trailer.mp4";

    // 第一次请求：获取前 5MB
    log_info!("Example", "第一次请求: 0-1MB");
    let req1 = Request::builder()
        .method("GET")
        .uri("http://127.0.0.1:8080")
        .header("Range", "bytes=0-10240")
        .header("X-Original-Url", video_url)
        .body(Body::empty())?;

    let resp1 = client.request(req1).await?;
    log_info!("Example", "第一次响应状态: {}", resp1.status());
    log_info!("Example", "第一次响应头: {:#?}", resp1.headers());

    // 等待一秒
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // 第二次请求：获取接下来的 5MB
    log_info!("Example", "第二次请求: 1MB-10MB");
    let req2 = Request::builder()
        .method("GET")
        .uri("http://127.0.0.1:8080")
        .header("Range", "bytes=10240-102400")
        .header("X-Original-Url", video_url)
        .body(Body::empty())?;

    let resp2 = client.request(req2).await?;
    log_info!("Example", "第二次响应状态: {}", resp2.status());
    log_info!("Example", "第二次响应头: {:#?}", resp2.headers());

    Ok(())
}
