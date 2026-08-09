# BitTorrent 客户端接入

[English](torrent-client-integration.md)

## 能力范围

可选 feature `p2p-librqbit` 提供 Magnet 解析、BitTorrent wire、HTTP/UDP
Tracker、DHT、Peer 自动发现、断点数据、分片校验和面向 Range 的边下边播。该能力默认
关闭。生产后端关闭上传、监听端口和 UPnP，因此不会做种。

只能添加应用已获得合法授权的内容。Core 要求显式授权，并在创建 P2P 会话前严格校验
Magnet URI。仅启动普通 localhost 代理不会产生 BitTorrent 网络活动。

## 构建

```bash
cargo build --locked --features p2p-librqbit
LIBRQBIT_ENABLED=1 PLATFORM=android ./scripts/build-mobile.sh dist/mobile
LIBRQBIT_ENABLED=1 PLATFORM=ios ./scripts/build-mobile.sh dist/mobile
LIBRQBIT_ENABLED=1 PLATFORM=harmony ./scripts/build-mobile.sh dist/mobile
```

手动运行 `mobile-native` 时选择 `include_librqbit`。产物名称包含 `-librqbit`，
`build-features.txt` 记录 `librqbit_enabled=1`。iOS 打包时会自动定义
`MEDIA_PROXY_CACHE_ENABLE_LIBRQBIT`。

## 生命周期

1. 创建并启动 `MediaProxyCache`。
2. 调用 `addAuthorizedTorrent(magnetUri)`。解析元数据可能阻塞，应离开 UI 线程执行。
3. 从 torrent 元数据选择文件 ID。当前移动端 ABI 已支持播放路径，但文件列表和状态 DTO
   尚未暴露，这是下一阶段 Adapter 工作。
4. 播放 `http://127.0.0.1:<端口>/torrent/<torrentId>/<fileId>`。
5. `removeTorrent(id, false)` 仅移除会话并保留数据；传 `true` 会删除下载文件。
6. 停止并关闭代理，关闭时会取消 librqbit 会话。

HTTP 端点支持 `GET`、`HEAD`、开放 Range、有限 Range 和后缀 Range。每次最多流式
读取 8 MiB，每个分块的超时时间为 30 秒。

## 三端接口

- Android/Kotlin：`addAuthorizedTorrent`、`removeTorrent`、`torrentPlaybackUrl`
- iOS/Swift：`addAuthorizedTorrent`、`removeTorrent`、`torrentPlaybackURL`
- HarmonyOS/ArkTS：`addAuthorizedTorrent`、`removeTorrent`、`torrentPlaybackUrl`

torrent ID 允许为 `0`。HarmonyOS 使用十进制字符串传递 ID，避免 JavaScript 整数精度
丢失。若默认原生包未启用 `p2p-librqbit`，调用这些接口会明确失败。
