use proxy_server::server::{ProxyConfig, ProxyServer};
use proxy_server::utils::error::ProxyError;
use std::env;
use std::path::PathBuf;
use std::time::Duration;

/// 读一个数值型环境变量，缺失或格式非法时用默认值。
///
/// 非法值不报错退出：这些都是可选调优项，一个拼错的环境变量不该让
/// 服务起不来。真正必需的参数走命令行位置参数。
fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() -> Result<(), ProxyError> {
    // 日志开关：`PROXY_LOG=0` 关掉。放在最前面，后面所有 log_info! 才受它管。
    proxy_server::utils::logger::init_from_env();

    let args: Vec<String> = env::args().collect();

    let port = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(8080);
    let cache_dir = args.get(2).map(PathBuf::from).unwrap_or_else(|| "cache".into());
    let allowed_hosts: Vec<String> = args
        .get(3)
        .map(|value| value.split(',').map(str::to_string).collect())
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

    let defaults = ProxyConfig::default();
    let config = ProxyConfig {
        port,
        cache_dir,
        allowed_hosts,
        // 缓存上限等调优项走环境变量：命令行位置参数已经排到第三个，
        // 再往后加可读性会迅速变差。
        max_cache_bytes: env_or("PROXY_MAX_CACHE_BYTES", defaults.max_cache_bytes),
        max_file_count: env_or("PROXY_MAX_FILES", defaults.max_file_count),
        max_concurrent_requests: env_or("PROXY_MAX_CONCURRENT", defaults.max_concurrent_requests),
        cleanup_interval: Duration::from_secs(env_or(
            "PROXY_CLEANUP_SECS",
            defaults.cleanup_interval.as_secs(),
        )),
    };

    let server = ProxyServer::with_config(config);
    // 不要吞掉这个错误：端口被占用之类的失败必须以非零退出码反映出来，
    // 否则进程管理器会以为服务已经正常起来了。
    server.start().await
}
