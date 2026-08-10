# Media Proxy Cache

[English](README.md) | 简体中文

一个使用 Rust 实现的 HTTP 媒体代理缓存。服务监听 `127.0.0.1`，从明确允许的上游域名流式读取媒体，并在磁盘上持久化真实完成的字节区间。

> 当前状态：原型。核心安全和缓存正确性问题已有第一轮修复及回归测试，但仍不建议直接作为生产依赖。C ABI、Android/Kotlin、iOS/Swift、HarmonyOS/ArkTS Adapter 和打包能力已经提供。localhost 调用方认证明确不在本项目当前范围内。

## 功能

- HTTP/HTTPS 上游数据源
- HTTP `Range` 请求和部分内容响应
- `GET` 和仅返回元数据的 `HEAD` 请求
- 基于磁盘的分片缓存
- 独立于稀疏文件长度的持久化完成区间
- 对已验证连续缓存前缀进行缓存/网络混合响应
- HLS 播放列表重写和分片代理
- 基于容量和文件数量的缓存清理，并真实删除磁盘文件
- 不受 signed URL 变化影响的稳定缓存身份
- 上游域名白名单和私网地址拦截
- 使用内置 WebPKI 根证书的纯 Rust TLS，保证移动端构建一致性
- 仅监听 localhost 的 HTTP/1.1 服务（支持 HTTP 和 HTTPS 上游）
- 面向移动端 Adapter 的 C ABI 生命周期入口（`include/media_proxy_cache.h`）
- 默认关闭、带合规授权门的可选 P2P 字节提供接口
- 默认关闭、基于 librqbit 的可选 Magnet/BitTorrent 边下边播后端

## 环境要求

- Rust 1.85 或更高版本
- Cargo
- Linux、macOS 或 Windows

## 构建和测试

```bash
cargo build --locked
cargo test --locked
```

### 可选 P2P 接口

使用以下命令构建和测试可选模块：

```bash
cargo test --locked --features p2p
```

该 feature 不是 BitTorrent 客户端，不接受 magnet，不实现 DHT、公共 tracker 或自动
peer discovery。Host 必须明确确认内容授权，并提供稳定 content ID、总长度、完整内容
SHA-256 和逐片 SHA-256 清单。Core 会先验证每个分片，再返回其中的字节。项目没有合法
P2P 来源时应保持该 feature 关闭。

清单、C 回调、生命周期、播放 URL 和验收合同参见
[可选 P2P 客户端接入](docs/p2p-client-integration.zh-CN.md)。

### P2P 边下边播

Host 将已授权分片写入 `<分片目录>/<index>.piece`。播放器使用本地 HTTP Range 地址；Core
读取并校验请求分片，将校验后的字节写入共享缓存并立即返回。播放期间 Host 可以继续填充后续
分片。Core 不负责发现 Peer 或从 P2P 网络下载，分片获取和授权由 Host 负责。

真实 Magnet、Tracker、DHT、Peer 自动发现和分片下载由独立开关
`p2p-librqbit` 提供，参见 [BitTorrent 客户端接入](docs/torrent-client-integration.zh-CN.md)。
librqbit 生产后端明确关闭上传和做种。

### 依赖安全

CI 会在每次 push 和 pull request 时运行 RustSec 审计。本地可用以下命令复现：

```bash
cargo install cargo-audit --locked
cargo audit
cargo install cargo-license --locked
cargo license --avoid-dev-deps --avoid-build-deps
```

当前锁文件审计覆盖 116 个包，没有 RustSec 安全公告。生产依赖使用宽松的
Apache-2.0、MIT、ISC、BSD-3-Clause、Unicode-3.0、Unlicense、
CDLA-Permissive-2.0，或包含宽松许可选项的多许可证表达式；不存在强制 GPL、
AGPL 或 SSPL 依赖。

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

### 移动端 FFI 预览

该 crate 现在同时构建 `staticlib` 和 `cdylib` 产物。移动端 Adapter 可包含
[`include/media_proxy_cache.h`](include/media_proxy_cache.h)，传入由 Host 管理的缓存目录，
在固定端口或端口 `0` 上启动服务，并通过 `stop`/`destroy` 释放资源。这仍是预览 ABI。
项目已提供构建和 Releases 打包脚本，以及 Android JNI、iOS Swift 和鸿蒙 N-API Adapter 模板；宿主工程仍需按平台链接产物、配置播放器并完成真机验收。

访问真实上游必须调用 `proxy_server_create_with_hosts` 并传入逗号分隔的域名白名单。
简化版 `proxy_server_create` 会有意使用拒绝全部上游的策略。

统一原生库构建脚本位于 `scripts/build-mobile.sh`：

```bash
PLATFORM=ios ./scripts/build-mobile.sh dist/mobile
PLATFORM=android ./scripts/build-mobile.sh dist/mobile
PLATFORM=harmony ./scripts/build-mobile.sh dist/mobile
PLATFORM=macos ./scripts/build-mobile.sh dist/desktop
PLATFORM=windows ./scripts/build-mobile.sh dist/desktop
```

脚本要求先安装对应的 Rust target，并把 C 头文件复制到各平台产物目录。Android
Kotlin 工程应将生成的 `.so` 放入 Android Library 模块使用。

三端 Adapter 的所有权接口模板位于 `platform/android`、`platform/ios` 和
`platform/harmony`。这些文件提供生命周期和所有权封装，宿主工程仍需链接原生库并配置对应的
JNI、Swift module map 或 N-API 工程设置。

同一个 Core 也支持桌面端构建。macOS 会构建 Apple Silicon 和 Intel 目标；Windows
默认使用 `x86_64-pc-windows-gnu`，构建机需要安装 MinGW linker。桌面程序可以直接
使用生成的 `cdylib` 或 `staticlib`。

推送匹配 `v*` 的 tag 后，`.github/workflows/release.yml` 会自动发布 macOS ARM64/Intel、
Windows x86_64 和 Linux x86_64 压缩包。也可以手动运行工作流，只生成可下载的
Actions Artifacts 而不创建 GitHub Release。每个归档都会附带对应的 `.sha256` 文件；
macOS/Linux 可运行 `shasum -a 256 -c <归档>.sha256` 校验，Windows 可运行
`Get-FileHash <归档> -Algorithm SHA256` 校验。

独立的 `.github/workflows/mobile.yml` 会在 GitHub runner 上构建 iOS 和 Android
原生库，并在 tag 推送时作为 Release 资产发布。鸿蒙构建默认不启用；需要设置仓库变量
`ENABLE_HARMONY_BUILD=true`，通过 Secret `OHOS_NDK_URL` 提供可下载的 OHOS NDK，
并通过 `OHOS_HVIGOR_URL` 提供包含可执行 `hvigorw` 的工具归档。还必须将两个不可变归档
各自的 SHA-256 配置为 `OHOS_NDK_SHA256` 和 `OHOS_HVIGOR_SHA256` Secret。未启用时
鸿蒙任务会跳过；缺少 URL 或摘要时会在解压工具前失败。

在 macOS 上运行 `./scripts/test-ffi-macos.sh`，会构建一个链接 Release dylib 的小型
C 程序，并真实执行 create/start/stop/destroy 完整生命周期。

### HLS 播放列表刷新策略

重写后的播放列表正文会保存在有上限的内存缓存中。VOD 播放列表（包含
`EXT-X-ENDLIST`）每 5 分钟刷新一次，Master 播放列表每 30 秒刷新一次，直播媒体
播放列表按目标时长的一半刷新，并限制在 1 到 10 秒之间。最多保留 128 个播放列表，
播放列表正文和键的总大小上限为 8 MiB，超限时优先淘汰最早写入的条目。分片字节仍
使用持久化磁盘缓存，与播放列表正文的 TTL 相互独立。

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
请求头超时和数量限制可通过 `PROXY_REQUEST_HEADER_TIMEOUT_MS`（默认 `10000` 毫秒）以及
`PROXY_MAX_REQUEST_HEADERS`（默认 `64`）配置。其他可调项包括
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
use proxy_server::server::{ProxyConfig, ProxyServer};

#[tokio::main]
async fn main() {
    let server = std::sync::Arc::new(ProxyServer::with_config(ProxyConfig {
        port: 0,
        cache_dir: "./cache".into(),
        allowed_hosts: vec!["media.example.com".into(), "cdn.example.com".into()],
        ..Default::default()
    }));
    let running = tokio::spawn({
        let server = server.clone();
        async move { server.start().await }
    });
    let port = server.wait_until_ready().await.unwrap();
    println!("proxy ready at http://127.0.0.1:{port}");
    server.stop();
    running.await.unwrap().unwrap();
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

`HEAD` 使用相同的请求合同。冷缓存 HEAD 只通过上游 `bytes=0-0` 探测总长度和
Content-Type，不会把媒体字节标记为已缓存；元数据持久化后，后续 HEAD 不再回源。
当前支持单个闭区间和开区间 Range，多 Range 会明确返回 `416`。

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
- 一个带 schema 版本、缓存键、上游元数据和已完成闭区间的 JSON sidecar 文件。

文件长度不会被当作区间已缓存的证明。数据完成刷新后才会更新 sidecar，并通过临时文件重命名进行提交。

启动恢复会删除上次异常中断遗留的 sidecar 临时文件，并校验缓存键对应的哈希路径、数据文件长度以及区间顺序。损坏 JSON、未知的未来 schema、重叠或乱序区间、超过数据文件长度的区间都视为不可相信，对应数据文件和 sidecar 会一并删除，不能成为缓存命中。旧版 v0 元数据仍可读取，并会在下次写入时升级到当前格式。

## 已知限制

- Android AAR、iOS XCFramework 和 HarmonyOS HAR 已完成组装验证；真机播放器和后台生命周期仍需宿主应用验收
- Core 已提供动态端口、readiness 等待和生命周期状态；仍需在三端 Adapter 中验证前后台切换时的实例所有权
- 同一缺失区间已通过 single-flight 合并；缓存侧背压超过 1 秒后会放弃缓存写入，不阻塞播放
- Range、HLS、损坏恢复和进程重启已有聚焦的单元/E2E 测试，但仍需补充移动端播放器覆盖
- DNS 策略校验与连接器后续解析尚未固定到同一解析地址，仍存在 DNS rebinding 的检查/使用时间窗口
- `cargo audit` 报告的 `backoff`、`bincode` 和 `instant` unmaintained 警告仅来自可选 `librqbit` 依赖链。安全门禁仍保持启用；两个 `quick-xml` 公告只作精确豁免，因为 UPnP 端口转发已强制关闭。

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
