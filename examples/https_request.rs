mod support;
use support as local_http;

use proxy_server::{log_info, server};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 启动代理服务器
    tokio::spawn(async {
        log_info!("Example", "启动代理服务器...");
        // 默认策略是 deny-all，必须显式放行本例要访问的上游主机。
        let server = server::ProxyServer::with_allowed_hosts(8080, "./cache", ["media.w3.org"]);
        if let Err(e) = server.start().await {
            eprintln!("代理服务器退出: {}", e);
        }
    });

    // 等待服务器启动
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let client = local_http::Client::new();

    // 测试 HTTPS URL
    let https_url = "https://media.w3.org/2010/05/sintel/trailer.mp4";

    log_info!("Example", "发送 HTTPS 请求");
    let resp = client
        .get("http://127.0.0.1:8080")
        .header("X-Original-Url", https_url)
        .header("X-Cache-User-Id", "example-user")
        .header("X-Cache-Asset-Id", "sintel-trailer")
        .header("X-Cache-Asset-Revision", "1")
        .send()
        .await?;
    log_info!("Example", "响应状态: {}", resp.status());
    log_info!("Example", "响应头: {:#?}", resp.headers());

    Ok(())
}
