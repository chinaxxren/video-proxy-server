use hyper::{Body, Client, Request};
use proxy_server::{log_info, server};
use std::env;
use tokio::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 解析命令行参数
    let args: Vec<String> = env::args().collect();
    let target_url = if args.len() > 1 {
        &args[1]
    } else {
        "https://media.w3.org/2010/05/sintel/trailer.mp4"
    };
    let allowed_host = url::Url::parse(target_url)?
        .host_str()
        .ok_or("target URL has no host")?
        .to_string();

    // 启动代理服务器
    tokio::spawn(async move {
        log_info!("Server", "启动代理服务器...");
        let server = server::ProxyServer::with_allowed_hosts(8080, "./cache", [allowed_host]);
        let _ = server.start().await;
    });

    // 等待服务器启动
    tokio::time::sleep(Duration::from_secs(1)).await;

    let proxy_host = "127.0.0.1:8080";

    // 构建完整的代理URL
    let proxy_url = format!("http://{}/proxy/{}", proxy_host, target_url);

    // 创建 HTTP 客户端
    let client = Client::new();

    // 构建请求
    let req = Request::builder()
        .method("GET")
        .uri(&proxy_url)
        .header("X-Cache-User-Id", "example-user")
        .header("X-Cache-Asset-Id", "sintel-trailer")
        .header("X-Cache-Asset-Revision", "1")
        .body(Body::empty())?;

    // 发送请求
    log_info!("Client", "发送请求...");
    let resp = client.request(req).await?;

    // 输出响应信息
    log_info!("Client", "响应状态: {}", resp.status());
    log_info!("Client", "响应头: {:#?}", resp.headers());

    Ok(())
}
