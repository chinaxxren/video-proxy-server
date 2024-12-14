use hyper::{Client, Request, Body};
use tokio;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 启动代理服务器（在另一个线程中运行）
    tokio::spawn(async {
        println!("启动代理服务器...");
        proxy_server::server::run_server().await.unwrap();
    });

    // 等待服务器启动
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // 创建 HTTP 客户端
    let client = Client::new();
    // let video_url = "https://www.w3school.com.cn/i/movie.mp4";
    // let video_url = "https://media.w3.org/2010/05/sintel/trailer.mp4";
    let video_url = "http://aliuwmp3.changba.com/userdata/video/45F6BD5E445E4C029C33DC5901307461.mp4";
    
    // 第一次请求：获取前 1MB
    println!("\n第一次请求: 0-1MB");
    let req1 = Request::builder()
        .method("GET")
        .uri("http://127.0.0.1:8080")
        .header("X-Original-Url", video_url)
        .body(Body::empty())?;

    let resp1 = client.request(req1).await?;
    println!("第一次响应状态: {}", resp1.status());
    println!("第一次响应头: {:#?}", resp1.headers());

    // 等待一秒
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // 第二次请求：相同的范围
    println!("\n第二次请求: 0-1MB (从缓存获取)");
    let req2 = Request::builder()
        .method("GET")
        .uri("http://127.0.0.1:8080")
        .header("X-Original-Url", video_url)
        .body(Body::empty())?;

    let resp2 = client.request(req2).await?;
    println!("第二次响应状态: {}", resp2.status());
    println!("第二次响应头: {:#?}", resp2.headers());

    Ok(())
}