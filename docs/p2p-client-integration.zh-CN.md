# 可选 P2P 客户端接入

[English](p2p-client-integration.md)

## 范围

`p2p` feature 是由 Host 提供字节的受控接口，不是 BitTorrent 客户端。它不接受
magnet，不实现 DHT、公共 tracker、peer discovery、上传或做种。只有 Host 对目标
媒体拥有明确授权来源时才能启用。

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

## 分片回调

每个分片会调用 `ProxyP2pPieceCallback` 两次：

1. `buffer == NULL`、`capacity == 0`：返回所需长度。
2. Core 提供缓冲区：写入完全相同数量的字节并返回该长度。

回调和 context 可能在 Core 工作线程执行，必须保持有效，直到调用
`proxy_p2p_source_remove` 或 `proxy_server_destroy`。分片 SHA-256 与清单一致后才会被
接受。同一分片使用 single-flight，已验证分片使用每个 source 最大 16 MiB 的内存缓存。
已验证分片还会持久化到 `<缓存目录>/p2p`，Core 重启后可继续复用。每次读取磁盘缓存时
都会重新校验分片；损坏文件会被删除，并重新向 Host 请求。
P2P 磁盘缓存使用 Host 的 `max_cache_bytes` 上限；超过上限时会淘汰最后修改时间最早的
已验证分片。

## 播放

注册成功后返回 opaque ID，播放地址为：

```text
http://127.0.0.1:<实际端口>/p2p/<id>
```

路由支持 GET、HEAD、普通/开区间/后缀 Range，并按已验证子范围流式返回大内容。它不接受
来源 URL、magnet、tracker 或 peer 地址。授权撤销时必须立即删除该 ID。

## 验收

- 未授权或错误清单必须被拒绝；
- 损坏、过短、过长、缺失或超时分片必须失败关闭；
- 并发重叠 Range 对每个分片只调用一次 Provider；
- 目标播放器的 GET、HEAD、Seek、后缀 Range 和 416 行为正常；
- 删除 ID 后不能继续读取；
- 日志和 HTTP 响应不包含授权引用或 peer 信息。
