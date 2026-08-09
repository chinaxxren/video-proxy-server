# 可选 P2P 客户端接入

[English](p2p-client-integration.md)

## 范围

`p2p` feature 是由 Host 提供字节的受控接口，不是 BitTorrent 客户端。它不接受
magnet，不实现 DHT、公共 tracker、peer discovery、上传或做种。只有 Host 对目标
媒体拥有明确授权来源时才能启用。

独立的 `p2p-network` feature 当前仅解析 Magnet 元数据，不会发起网络请求，也不实现 Tracker、
DHT、Peer wire 会话、自动下载、上传或做种。

## 构建

```bash
cargo build --locked --features p2p
cargo test --locked --features p2p
```

原生 Adapter 引用 `include/media_proxy_cache.h` 时，需要定义
`MEDIA_PROXY_CACHE_ENABLE_P2P`。

移动端打包默认不包含 P2P。使用以下命令生成带明确 `-p2p` 标记的可选包：

```bash
P2P_ENABLED=1 PLATFORM=android ./scripts/build-mobile.sh
P2P_ENABLED=1 ./scripts/package-mobile.sh
```

启用 P2P 的原生构建会校验回调/目录注册、完整校验和 remove ABI 导出符号；任何符号
缺失都会使构建失败。
打包时还要求 `build-features.txt` 与 `P2P_ENABLED` 完全一致，防止默认库被错误标记为
可选 P2P Release，反向混用也会被拒绝。
缺少公共 C 头文件或完全没有原生代理库产物的输入目录也会被拒绝。

在 GitHub Actions 中手动运行 `mobile-native`，并将 `include_p2p` 设为 true。Tag 自动
发布始终构建默认核心包；可选产物名称会包含 `-p2p`。

## 授权清单

向 `proxy_p2p_source_register` 传入 UTF-8 JSON：

```json
{
  "content_id": "movie-42-revision-7",
  "content_length": 10485760,
  "content_sha256": "64 位十六进制 SHA-256",
  "piece_length": 1048576,
  "piece_sha256": ["每个分片一个 64 位 SHA-256"],
  "authorization_reference": "许可证或权益引用",
  "explicitly_authorized": true
}
```

Core 会拒绝未知字段、错误长度或摘要、未明确授权、分片数量不一致，以及超过 1 MiB
的清单。授权引用仅保留在 Core 内部，不会通过 HTTP 返回。
每个分片最大 8 MiB，每个清单最多 16,000 个分片；通过 Rust API 直接构造时也执行
相同限制。

## 分片回调

每个分片会调用 `ProxyP2pPieceCallback` 两次：

1. `buffer == NULL`、`capacity == 0`：返回所需长度。
2. Core 提供缓冲区：写入完全相同数量的字节并返回该长度。

回调和 context 可能在 Core 工作线程执行，必须保持有效，直到调用
`proxy_p2p_source_remove` 或 `proxy_server_destroy`。分片 SHA-256 与清单一致后才会被
接受。同一分片使用 single-flight，已验证分片使用每个 source 最大 16 MiB 的内存缓存。
已验证分片还会持久化到 `<缓存目录>/p2p`，Core 重启后可继续复用。每次读取磁盘缓存时
都会重新校验分片；损坏文件会被删除，并重新向 Host 请求。
P2P 磁盘缓存使用 Host 的 `max_cache_bytes` 上限；超过上限时会淘汰最久未使用的
已验证分片。
Core 初始化时也会执行一次 P2P 淘汰，确保前一个进程留下的超限分片不会在新进程空闲时
持续占用空间。
普通 HTTP 缓存的清理周期也会将 `<缓存目录>/p2p` 计入同一个总预算，因此两类缓存不会
各自获得一份完整上限。
版本化缓存身份同时包含整文件摘要和分片清单，因此相同内容的不同分片布局不会冲突。

`proxy_p2p_source_remove` 会先阻止新读取，再等待已经开始的回调结束；返回后 Host 才可
释放 callback context。回调内部不得重入调用 P2P Core 函数或
`proxy_server_destroy`，因为这些操作可能需要等待当前回调结束。
移除引用某个 manifest 的最后一个注册时，也会清理该 manifest 的持久化分片目录。
若未显式 remove，而只是销毁并重新创建 Core，则保留已验证分片用于重启恢复。
Adapter 可在播放前调用 `proxy_p2p_source_verify_complete` 校验全部分片和完整内容摘要。
成功返回 `1`，Provider 或完整性校验失败返回 `0`；该操作可能会获取整个资源。
如果完整摘要不匹配，Core 会永久使共享该 manifest 缓存身份的全部已注册 source 失效，
后续 Range 请求会失败；Host 必须注册修正后的授权 source。该失效标记会跨 Core 重启保留；
显式移除其最后一个注册会清理目录及标记。

## 托管客户端 Adapter

Swift、Kotlin 和 ArkTS 使用目录 Provider API，避免托管对象必须跨任意 Rust 工作线程
承受同步回调。Host 在自己的应用沙箱内使用绝对路径，并按以下格式写入已授权分片：

```text
<分片目录>/0.piece
<分片目录>/1.piece
...
```

文件名由 Core 使用整数索引生成；每次读取限制为 8 MiB，并且字节通过 Manifest 校验后
才能对播放器提供。该接口不会下载分片、发现 peer 或授予内容权限。Host 必须保持目录
可用，直到显式移除 source。

```kotlin
val sourceId = cache.registerP2PDirectory(manifestJson, pieceDirectory.absolutePath)
check(cache.verifyP2PSource(sourceId))
player.setMediaItem(MediaItem.fromUri(cache.p2pPlaybackUrl(sourceId)))
cache.removeP2PSource(sourceId) // 授权撤销
```

iOS 需要同时为 Swift 与 C 定义 `MEDIA_PROXY_CACHE_ENABLE_P2P`：

```swift
let sourceID = try cache.registerP2PDirectory(
    manifestJSON: manifestData,
    pieceDirectory: pieceDirectoryURL
)
guard cache.verifyP2PSource(sourceID) else {
    throw NSError(domain: "MediaProxyCache", code: 6)
}
let player = AVPlayer(url: try cache.p2pPlaybackURL(sourceID: sourceID))
cache.removeP2PSource(sourceID)
```

鸿蒙使用十进制字符串传递 opaque ID，避免 JavaScript number 的精度损失：

```typescript
const sourceId = cache.registerP2PDirectory(manifestJson, pieceDirectory)
if (!cache.verifyP2PSource(sourceId)) throw new Error('P2P verification failed')
avPlayer.url = cache.p2pPlaybackUrl(sourceId)
cache.removeP2PSource(sourceId)
```

默认 Android 和鸿蒙库保留托管方法，但由于没有编译 P2P，注册时会明确失败。iOS 在没有
编译期 P2P 定义时不会暴露这些方法。

## 播放

注册成功后返回 opaque ID，播放地址为：

```text
http://127.0.0.1:<实际端口>/p2p/<id>
```

路由支持 GET、HEAD、普通/开区间/后缀 Range，并按已验证子范围流式返回大内容。它不接受
来源 URL、magnet、tracker 或 peer 地址。授权撤销时必须立即删除该 ID。

## 边下边播流程

```text
Host 获取已授权分片 -> 写入 <index>.piece
播放器请求 Range   -> Core 读取并校验该分片
Core 持久化校验结果 -> 播放器收到 Range 响应
```

完整文件尚未到齐时即可开始播放，Host 可以并发写入后续分片。缺失分片会返回临时 source
失败；Host 写完后，播放器可以重试对应 Range。Core 不负责 Peer 发现或网络下载。

## 验收

- 未授权或错误清单必须被拒绝；
- 损坏、过短、过长、缺失或超时分片必须失败关闭；
- 并发重叠 Range 对每个分片只调用一次 Provider；
- 目标播放器的 GET、HEAD、Seek、后缀 Range 和 416 行为正常；
- 删除 ID 后不能继续读取；
- 日志和 HTTP 响应不包含授权引用或 peer 信息。
