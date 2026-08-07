# Media Proxy Cache

[English](README.md) | 简体中文

一个使用 Rust 实现的 HTTP 媒体代理缓存。服务监听 `127.0.0.1`，从明确允许的上游域名流式读取媒体，并在磁盘上持久化真实完成的字节区间。

> 当前状态：原型。核心安全和缓存正确性问题已有第一轮修复及回归测试，但仍不建议直接作为生产依赖。移动端 FFI 和 localhost 调用方认证边界尚未完成。

## 功能

- HTTP/HTTPS 上游数据源
- HTTP `Range` 请求和部分内容响应
- 基于磁盘的分片缓存
- 独立于稀疏文件长度的持久化完成区间
- 对已验证连续缓存前缀进行缓存/网络混合响应
- HLS 播放列表重写和分片代理
- 基于容量和文件数量的缓存清理，并真实删除磁盘文件
- 不受 signed URL 变化影响的稳定缓存身份
- 上游域名白名单和私网地址拦截
- 仅监听 localhost

## 环境要求

- Rust 1.70 或更高版本
- Cargo
- Linux、macOS 或 Windows

## 构建和测试

```bash
cargo build --locked
cargo test --locked
```

当前测试覆盖稳定缓存键、稀疏区间正确性、区间元数据持久化、物理删除、缓存清理、区块锁回归以及核心网络策略拒绝场景。

### 浏览器真实播放测试

项目根目录的 `aa.mp4` 可通过本地 Web 测试页进行真实播放、拖动和 Range 验证：

```bash
cargo run --locked --features allow-private-upstream --example local_playground -- ./aa.mp4
```

命令会同时启动本地 Range 源站、代理和 Web 测试页。打开终端输出的 Web 地址，
可以播放视频、输入自定义字节范围，并运行重复及重叠 Range 检查。同源开发网关
只负责补充 `<video>` 无法发送的缓存身份头，媒体数据仍会经过真实代理核心。

本机安装 `ffmpeg` 时，启动过程还会把 `aa.mp4` 自动封装为 fMP4 HLS。页面的 HLS
检查会验证 m3u8 MIME、初始化段与分片 URI 改写、全部分片响应、浏览器
MediaSource 解码，以及重复请求首分片时的缓存命中。

`allow-private-upstream` 会放宽本机回环地址限制，只能用于本地测试，不能用于生产构建。

## 运行

可执行程序参数：

```text
proxy-server [端口] [缓存目录] [逗号分隔的上游域名白名单]
```

示例：

```bash
cargo run -- 8080 ./cache media.example.com,cdn.example.com
```

未提供白名单时，服务可以启动，但会拒绝所有上游请求。服务始终绑定到 `127.0.0.1`。

可通过环境变量调整限制。`PROXY_SHUTDOWN_TIMEOUT_MS` 控制有界的优雅排空时间（默认
`5000` 毫秒）；超时后会取消当前实例登记的回源和缓存转发任务。其他可调项包括
`PROXY_MAX_CACHE_BYTES`、`PROXY_MAX_FILES`、`PROXY_MAX_CONCURRENT` 和
`PROXY_CLEANUP_SECS`。

也可以运行已维护的客户端示例：

```bash
cargo run --example proxy_client -- \
  https://media.w3.org/2010/05/sintel/trailer.mp4
```

## 嵌入服务

必须显式配置允许访问的上游域名：

```rust
use proxy_server::server::ProxyServer;

#[tokio::main]
async fn main() {
    let server = ProxyServer::with_allowed_hosts(
        8080,
        "./cache",
        ["media.example.com", "cdn.example.com"],
    );

    server.start().await.unwrap();
}
```

`ProxyServer::new` 使用默认拒绝全部上游的网络策略。需要访问上游时应使用 `with_allowed_hosts`。

## 代理请求合同

通过 `X-Original-Url` 传入当前上游 URL，并提供当前 Core 支持的两个缓存身份头：

```bash
curl 'http://127.0.0.1:8080/proxy/media' \
  -H 'X-Original-Url: https://media.example.com/audio/song.m4a?token=short-lived' \
  -H 'Range: bytes=0-65535' \
  -H 'X-Cache-Asset-Id: song-456' \
  -H 'X-Cache-Asset-Revision: 7'
```

当前缓存身份由上游 scheme/host/port/path 加上以下字段生成：

```text
assetId + assetRevision
```

signed URL 的 query 只作为当前网络来源。令牌变化不会产生新的缓存条目。当前 Core 尚未将 `userId` 纳入缓存键；多用户生产集成必须先由 Host 提供可信的用户/租户隔离边界。

## 网络安全

发送上游请求前，代理会：

- 仅接受 `http` 和 `https` URL；
- 拒绝包含凭据的 URL；
- 要求域名精确命中白名单；
- 解析域名并拒绝回环、私网、链路本地、文档、组播及其他保留地址；
- 拒绝重定向，因为当前 Hyper 客户端不会自动跟随重定向。

不要在 Core 之外记录或持久化 `X-Original-Url`，其中可能包含短期凭据。

## 缓存布局

缓存键在作为路径前会先进行哈希。每个缓存对象包含：

- 一个按源文件偏移写入字节的数据文件；
- 一个记录已成功完成闭区间的 JSON sidecar 文件。

文件长度不会被当作区间已缓存的证明。数据完成刷新后才会更新 sidecar，并通过临时文件重命名进行提交。

## 已知限制

- 尚无 Android JNI、iOS XCFramework 或 HarmonyOS N-API Adapter
- 尚无适用于移动端 Host 的公开启动、停止和生命周期 API
- 同一缺失区间已通过 single-flight 合并；缓存侧背压超过 1 秒后会放弃缓存写入，不阻塞播放
- Range、HLS、损坏恢复和进程重启已有聚焦的单元/E2E 测试，但仍需补充移动端播放器覆盖
- DNS 策略校验与连接器后续解析尚未固定到同一解析地址，仍存在 DNS rebinding 的检查/使用时间窗口
- 依赖图仍包含重叠的 HTTP 客户端和较宽泛的 Tokio features

## 客户端接入

参见[移动客户端接入文档](docs/mobile-client-integration.zh-CN.md)，其中包含 iOS、Android 和鸿蒙 SDK 的建议架构、生命周期合同、产物形式及 POC 验收标准。

## 项目结构

```text
src/
├── data_source/          # 网络和文件数据源
├── handlers/             # 缓存、网络、混合源和响应处理器
├── hls/                  # HLS 解析、重写和分片处理
├── storage/              # 磁盘引擎、完成区间、清理和区块状态
├── utils/                # Range 解析、错误、日志和网络策略
├── data_request.rs       # 代理请求和稳定缓存身份
├── data_source_manager.rs
├── request_handler.rs
└── server.rs
```

## 贡献

请保持改动聚焦，并为影响 Range、缓存完整性、网络策略、清理或并发行为的修改添加回归测试。提交前运行：

```bash
cargo test --locked
git diff --check
```

## 许可证

本项目采用 [Apache License 2.0](LICENSE) 开源协议。在遵守协议条款的前提下，
可自由使用、修改和分发，包括商业用途。
