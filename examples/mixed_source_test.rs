use proxy_server::server::ProxyServer;
use std::error::Error;
use std::time::Duration;
use proxy_server::log_info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    log_info!("Example", "启动代理服务器...");
    
    // 创建并启动代理服务器
    // 上游主机必须显式列入白名单，否则 SSRF 策略会拒绝全部请求。
    let server = ProxyServer::with_allowed_hosts(8080, "./cache", ["www.w3school.com.cn"]);
    let _server_handle = tokio::spawn(async move {
        if let Err(e) = server.start().await {
            eprintln!("Server error: {}", e);
        }
    });
    
    // 等待服务器启动
    tokio::time::sleep(Duration::from_secs(1)).await;
    
    // 创建 HTTP 客户端
    let client = reqwest::Client::new();
    let url = "https://www.w3school.com.cn/i/movie.mp4";
    
    // 第一步：请求前 100KB 数据
    log_info!("Example", "第一步：请求前 100KB 数据");
    let resp = client.get(format!("http://127.0.0.1:8080/proxy/{}", url))
        .header("Range", "bytes=0-102399")
        .send()
        .await?;
    
    log_info!("Example", "第一次响应状态: {}", resp.status());
    log_info!("Example", "第一次响应头: {:#?}", resp.headers());
    let body = resp.bytes().await?;
    log_info!("Example", "第一次响应体长度: {}", body.len());
    
    // 等待一会儿
    tokio::time::sleep(Duration::from_secs(2)).await;
    
    // 第二步：请求 50KB-150KB 数据（混合源）
    log_info!("Example", "第二步：请求 50KB-150KB 数据（混合源）");
    let resp = client.get(format!("http://127.0.0.1:8080/proxy/{}", url))
        .header("Range", "bytes=51200-153599")
        .send()
        .await?;
    
    log_info!("Example", "第二次响应状态: {}", resp.status());
    log_info!("Example", "第二次响应头: {:#?}", resp.headers());
    let body = resp.bytes().await?;
    log_info!("Example", "第二次响应体长度: {}", body.len());
    
    // 第三步：再次请求相同范围（验证缓存）
    log_info!("Example", "第三步：再次请求相同范围（验证缓存）");
    let resp = client.get(format!("http://127.0.0.1:8080/proxy/{}", url))
        .header("Range", "bytes=51200-153599")
        .send()
        .await?;
    
    log_info!("Example", "第三次响应状态: {}", resp.status());
    log_info!("Example", "第三次响应头: {:#?}", resp.headers());
    let body = resp.bytes().await?;
    log_info!("Example", "第三次响应体长度: {}", body.len());
    
    Ok(())
}
