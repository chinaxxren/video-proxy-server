# 变更日志

[English](CHANGELOG.md)

## 0.4.1 - 2026-08-10

### 修复

- 单独启用 `harmony-napi` 时自动包含完整的 librqbit N-API 方法集合。
- Android CI 在需要时通过 SDK 预览 channel 安装固定版本 NDK。

### 新增

- 将可重复的 localhost BitTorrent Peer Wire 传输与损坏分片拒绝测试加入 CI。
- 增加 Criterion 解析性能基准和 Release 资产自动校验。
- BitTorrent 分片失败后启用后续候补 Peer，不再忽略并发上限之外的节点。

## 0.4.0 - 2026-08-10

### 新增

- 不透明媒体来源注册、稳定缓存身份，以及由 Host 提供的 signed URL 自动刷新回调。
- Android JNI、iOS Swift、鸿蒙 N-API Adapter 和 Release 打包流程。
- 基于 `librqbit` 的可选 BitTorrent 能力，以及协议级 Magnet、Tracker、DHT、Peer Wire、分片存储、上传和做种组件。
- Range 播放、缓存恢复、HLS、并发请求和授权刷新的真实 TCP 测试。
- Cargo、平台 feature、FFI、打包和 RustSec 自动质量门禁。

### 变更

- 迁移到 Hyper 1 和 Rustls，删除 Core 中重复的 Reqwest 客户端，并收窄 Tokio features。
- 来源刷新回调改为写入 Core 管理的有界缓冲区，不再把每次返回的 URL 保留到关闭。
- C、Swift、Kotlin/JNI 和鸿蒙 N-API Adapter 增加脱敏聚合运行指标。
- Range、百分号解码、Magnet、Tracker 和 Peer Wire 解析器增加属性测试。
- 移动端和桌面端打包使用可复现的 locked 构建，并为 Release 产物生成校验和。

### 安全

- 强制精确上游白名单、仅允许 HTTP(S)、拒绝 URL 凭据、重定向复检和公网地址过滤。
- 从播放器 URL 和日志中移除 signed URL。
- 持久化真实完成区间，避免稀疏文件空洞被误判为缓存命中。
- 将共享上游 HTTP Client 收紧为 crate 内部接口，防止调用方绕过网络策略。

### 兼容性

- 0.4.0 修改了来源刷新回调 ABI，三端 Adapter 必须与 Core 一起重新构建。
- 真机播放和后台生命周期验收仍由各 Host 应用完成。
