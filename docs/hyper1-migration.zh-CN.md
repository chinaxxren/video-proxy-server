# Hyper 1.x 迁移

生产请求、回源和响应链路现已使用 Hyper 1.x。由于 Body 和 Service API 不兼容，
本次迁移按阶段完成。

## 顺序

1. 将测试和示例客户端替换为共享的 Hyper 1 localhost 客户端。已完成，`reqwest` 已删除；测试源站
   Server 仍使用 Hyper 0.14。
2. 使用 `http-body-util` 引入流式 `AppBody`。已完成。
3. 迁移上游客户端和 TLS Connector。已完成。
4. 使用 `hyper-util::server::conn` 迁移 localhost Server。已完成。
5. 迁移测试源站和本地 playground，再删除 Hyper 0.14。已完成。

上游 Connector 必须保留 `PublicOnlyResolver`。如果直接替换成默认 reqwest Client，
会重新引入 DNS rebinding SSRF 路径。

`ResponseBuilder`、`DataSourceManager`、`MixedSourceHandler`、`RequestHandler` 与
localhost Server 现在直接传递 `Response<AppBody>`，旧 Server 响应桥接已经删除。
响应 Body 包装器会持有并发许可，直到 Body 被读取完毕或丢弃。

Hyper 0.14 已从生产和开发依赖中全部删除。测试源站和本地 playground 现在使用
Tokio listener 与 `hyper-util` 连接驱动，整个仓库只保留一个 Hyper 主版本。

## 完成标准

- Range、HEAD、开放结尾 Range 和 416 响应保持不变。
- HLS 播放列表、Key、Map、字幕和分片请求保持不变。
- 相同并发 Range 仍只产生一次回源请求。
- 客户端断开和优雅停止测试通过。
- macOS C ABI 冒烟测试通过。
