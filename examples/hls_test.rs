use proxy_server::server::ProxyServer;
use std::error::Error;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("[INFO] 启动代理服务器...");
    
    // 创建并启动代理服务器。
    // 上游主机必须显式加入白名单：默认策略是全部拒绝，用来挡住 SSRF。
    let server = ProxyServer::with_allowed_hosts(
        8080,
        "./cache",
        ["devstreaming-cdn.apple.com", "devimages.apple.com"],
    );
    let _server_handle = tokio::spawn(async move {
        if let Err(e) = server.start().await {
            eprintln!("Server error: {}", e);
        }
    });
    
    // 等待服务器启动
    tokio::time::sleep(Duration::from_secs(1)).await;
    
    // 测试用的 HLS 流
    let test_urls = [
        // Apple 的测试流
        "https://devstreaming-cdn.apple.com/videos/streaming/examples/img_bipbop_adv_example_ts/master.m3u8",
        // 备用测试流
        "http://devimages.apple.com/iphone/samples/bipbop/bipbopall.m3u8",
    ];
    
    let client = reqwest::Client::new();
    
    for &test_url in &test_urls {
        println!("\n[INFO] 开始 HLS 测试");
        let proxy_url = format!("http://127.0.0.1:8080/proxy/{}", test_url);
        
        // 1. 测试主播放列表
        println!("\n[INFO] 1. 获取主播放列表");
        match client.get(&proxy_url).send().await {
            Ok(resp) => {
                println!("[INFO] 状态码: {}", resp.status());
                if resp.status().is_success() {
                    let content = resp.text().await?;
                    println!("[INFO] 播放列表内容:\n{}", content);
                    
                    // 从播放列表中提取第一个码率的 URL
                    let variant_url = content.lines()
                        .find(|line| line.ends_with(".m3u8") && !line.starts_with('#'))
                        .map(|line| {
                            if line.starts_with("/proxy/") {
                                // 如果已经是代理 URL，直接使用
                                format!("http://127.0.0.1:8080{}", line)
                            } else if line.starts_with("http") {
                                format!("http://127.0.0.1:8080/proxy/{}", line)
                            } else {
                                let base = test_url.rsplit_once('/').map(|x| x.0).unwrap_or("");
                                format!("http://127.0.0.1:8080/proxy/{}/{}", base, line)
                            }
                        });
                    
                    // 2. 测试码率播放列表
                    if let Some(url) = variant_url {
                        println!("\n[INFO] 2. 获取码率播放列表");
                        
                        match client.get(&url).send().await {
                            Ok(resp) => {
                                println!("[INFO] 状态码: {}", resp.status());
                                if resp.status().is_success() {
                                    let content = resp.text().await?;
                                    println!("[INFO] 码率列表内容:\n{}", content);
                                    
                                    // 从码率列表中提取第一个分片的 URL
                                    let segment_url = content.lines()
                                        .find(|line| line.ends_with(".ts") && !line.starts_with('#'))
                                        .map(|line| {
                                            if line.starts_with("/proxy/") {
                                                // 如果已经是代理 URL，直接使用
                                                format!("http://127.0.0.1:8080{}", line)
                                            } else if line.starts_with("http") {
                                                format!("http://127.0.0.1:8080/proxy/{}", line)
                                            } else {
                                                let base = url.rsplit_once('/').map(|x| x.0).unwrap_or("");
                                                format!("http://127.0.0.1:8080/proxy/{}/{}", base, line)
                                            }
                                        });
                                    
                                    // 3. 测试分片下载
                                    if let Some(url) = segment_url {
                                        println!("\n[INFO] 3. 下载视频分片");
                                        
                                        // 3.1 完整下载
                                        println!("[INFO] 3.1 完整下载分片");
                                        match client.get(&url).send().await {
                                            Ok(resp) => {
                                                println!("[INFO] 状态码: {}", resp.status());
                                                println!("[INFO] 内容长度: {} bytes", resp.content_length().unwrap_or(0));
                                            }
                                            Err(e) => println!("[ERROR] 请求失败: {}", e),
                                        }
                                        
                                        // 3.2 范围请求
                                        println!("\n[INFO] 3.2 范围请求分片");
                                        match client.get(&url)
                                            .header("Range", "bytes=0-1024")
                                            .send()
                                            .await 
                                        {
                                            Ok(resp) => {
                                                println!("[INFO] 状态码: {}", resp.status());
                                                println!("[INFO] Content-Range: {:?}", resp.headers().get("content-range"));
                                                println!("[INFO] 内容长度: {} bytes", resp.content_length().unwrap_or(0));
                                            }
                                            Err(e) => println!("[ERROR] 请求失败: {}", e),
                                        }
                                        
                                        // 3.3 测试缓存
                                        println!("\n[INFO] 3.3 测试缓存（再次请求相同分片）");
                                        match client.get(&url).send().await {
                                            Ok(resp) => {
                                                println!("[INFO] 状态码: {}", resp.status());
                                                println!("[INFO] 内容长度: {} bytes", resp.content_length().unwrap_or(0));
                                            }
                                            Err(e) => println!("[ERROR] 请求失败: {}", e),
                                        }
                                    }
                                }
                            }
                            Err(e) => println!("[ERROR] 请求失败: {}", e),
                        }
                    }
                }
            }
            Err(e) => println!("[ERROR] 请求失败: {}", e),
        }
    }
    
    println!("\n[INFO] 测试完成！");
    Ok(())
}
