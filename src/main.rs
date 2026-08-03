use proxy_server::server::ProxyServer;
use proxy_server::utils::error::ProxyError;
use std::env;

#[tokio::main]
async fn main() -> Result<(), ProxyError> {
    // 解析命令行参数
    let args: Vec<String> = env::args().collect();

    // 获取端口号，默认为 8080
    let port = if args.len() > 1 {
        args[1].parse().unwrap_or(8080)
    } else {
        8080
    };

    // 获取缓存目录，默认为 cache
    let cache_dir = if args.len() > 2 { &args[2] } else { "cache" };

    let allowed_hosts = args
        .get(3)
        .map(|value| value.split(',').collect::<Vec<_>>())
        .unwrap_or_default();

    if allowed_hosts.is_empty() {
        // 服务器仍然启动，但会拒绝所有上游请求。这比默认放行安全，
        // 只是失败原因不直观，所以这里明确提示一次。
        eprintln!(
            "[WARN] 未配置上游白名单，所有代理请求都会被拒绝。\
             用法: {} <端口> <缓存目录> <host1,host2,...>",
            args.first().map(String::as_str).unwrap_or("proxy-server")
        );
    }

    let server = ProxyServer::with_allowed_hosts(port, cache_dir, allowed_hosts);
    // 不要吞掉这个错误：端口被占用之类的失败必须以非零退出码反映出来，
    // 否则进程管理器会以为服务已经正常起来了。
    server.start().await
}
