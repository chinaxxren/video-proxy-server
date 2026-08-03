use hyper::{Client, Request, Body};
use proxy_server::{log_info, server};
use hyper::body::to_bytes;

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

    // 创建 HTTP 客户端
    let client = Client::new();
    let video_url = "https://www.w3school.com.cn/i/movie.mp4";

    // 第一次请求
    log_info!("Example", "发送第一次请求");
    let req = Request::builder()
        .method("GET")
        .uri("http://127.0.0.1:8080")
        // .header("Range", "bytes=0-1023")  // 添加 Range 头
        .header("X-Original-Url", video_url)
        .body(Body::empty())?;

    let resp = client.request(req).await?;
    log_info!("Example", "第一次响应状态: {}", resp.status());
    log_info!("Example", "第一次响应头: {:#?}", resp.headers());
    let body_bytes = to_bytes(resp.into_body()).await?;
    log_info!("Example", "第一次响应体长度: {}", body_bytes.len());

    // 等待一秒，确保缓存写入
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // 第二次请求（完全相同的请求）
    log_info!("Example", "发送第二次请求（相同范围）");
    let req = Request::builder()
        .method("GET")
        .uri("http://127.0.0.1:8080")
        // .header("Range", "bytes=0-1023")  // 相同的 Range
        .header("X-Original-Url", video_url)
        .body(Body::empty())?;

    let resp = client.request(req).await?;
    log_info!("Example", "第二次响应状态: {}", resp.status());
    log_info!("Example", "第二次响应头: {:#?}", resp.headers());
    let body_bytes = to_bytes(resp.into_body()).await?;
    log_info!("Example", "第二次响应体长度: {}", body_bytes.len());

    Ok(())
}
