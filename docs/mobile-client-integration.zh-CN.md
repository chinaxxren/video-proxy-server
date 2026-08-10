# 移动客户端接入

[English](mobile-client-integration.md) | 简体中文

本文档描述 Media Proxy Cache 接入 iOS、Android 和鸿蒙应用的建议方案。

> 当前状态：设计目标，并非已发布 SDK。仓库目前提供 localhost Rust 代理可执行程序和核心缓存组件。下文描述的 FFI 层、移动端产物及生命周期 API 仍需实现，并在真机上验证。

## 目标

为每个平台编译同一套 Rust 缓存核心，并暴露小型平台原生 API。应用提供媒体身份和当前 signed URL，SDK 返回供平台播放器使用的 localhost 播放 URL。

```text
应用
    |
    | 媒体身份 + 当前来源 URL
    v
平台 Adapter
    |
    | FFI
    v
Media Proxy Cache Core (Rust)
    |
    | http://127.0.0.1:{动态端口}/...
    v
AVPlayer / Media3 / HarmonyOS AVPlayer
```

同一份 Rust 源码会针对各 CPU ABI 分别编译，并不是三端共用同一个二进制文件。

## 目标产物

| 平台 | Rust 产物 | 分发包 | 原生 API |
| --- | --- | --- | --- |
| iOS | 静态库 | XCFramework | Swift |
| Android | 动态库 | AAR | Kotlin/JNI |
| 鸿蒙 | 动态库 | HAR | ArkTS/N-API |

原生压缩包会在 `adapter/` 下包含当前平台模板。Swift 直接封装 C ABI；Kotlin 由 Rust
`android-jni` feature 提供实现，鸿蒙声明由 Rust `harmony-napi` feature 提供实现。
Android AAR 已完成组装验证，但运行时验证仍待完成。鸿蒙 HAR 已使用真实 Rust OHOS 库完成组装验证，设备运行时验证仍待完成。

建议支持的架构：

- iOS 真机：`aarch64-apple-ios`
- iOS 模拟器：`aarch64-apple-ios-sim` 和 `x86_64-apple-ios`
- Android：优先 `arm64-v8a`；仅在产品需要时增加 `armeabi-v7a` 和 `x86_64`
- 鸿蒙：`aarch64-unknown-linux-ohos` 和 `armv7-unknown-linux-ohos`

为了可复现地进行本机构建，Android 设置 `ANDROID_NDK_HOME`（或
`ANDROID_NDK_ROOT`），鸿蒙设置 `OHOS_NDK_HOME`（或 `OHOS_SDK_HOME`）。
`scripts/build-mobile.sh` 会自动配置对应的 clang 和 LLVM 归档工具。

## 建议 Host API

以下名称仅用于说明。最终平台 API 应遵循各平台命名习惯，但保持一致行为。

```text
create(config) -> client
start() -> localEndpoint
makePlaybackUrl(source, identity) -> localUrl
updateSource(identity, source)
stop()
clearCache(scope)
cacheUsage() -> bytes
```

配置：

```text
cacheDirectory       Host 提供的应用可写目录
maxCacheBytes        缓存容量硬限制
maxFileCount         可选的对象数量限制
allowedHosts         精确匹配的上游域名白名单
requestTimeout       上游请求超时
logLevel             任何级别都不能记录 signed URL
```

媒体身份：

```text
userId
assetId
assetRevision
```

来源信息：

```text
url                  当前 signed 或普通 HTTP(S) URL
headers              可选且经过批准的上游请求头
expiresAt            可选的来源过期时间
```

来源 URL 是可变网络信息，绝不能作为缓存身份。

## 生命周期合同

开始移动端打包前，Core 必须提供以下保证：

1. `start` 在 `127.0.0.1` 的端口 `0` 上绑定 HTTP/1.1 端点，并返回系统实际分配的端口。Adapter 不能要求 localhost 播放支持 HTTP/2 或 h2c。
2. 重复调用 `start` 必须幂等，或返回有文档说明的状态错误。
3. `stop` 停止接收请求、取消上游任务、刷新已提交元数据、释放 socket，并在有界超时内完成。
4. 客户端实例拥有自己的运行时资源；销毁实例不能留下脱离管理的服务任务。
5. 缓存目录由 Host 提供，Core 不能假定桌面工作目录。
6. 进程终止后恢复时，未完成写入必须视为缓存未命中，已完成区间必须保留。
7. 多个播放器请求同一缺失区间时，应进行合并或协调，不能损坏缓存元数据。

建议状态：

```text
Created -> Starting -> Running -> Stopping -> Stopped
                   \-> Failed
```

Rust Core 现已通过 `ProxyServerStatus` 暴露这些状态，支持
`ProxyConfig { port: 0, .. }`，通过 `wait_until_ready()`/`bound_port()` 返回实际
端口，拒绝同一实例重复 `start`，并执行有界停止。平台 Adapter 必须持有运行中的
`start` 任务，并在销毁阶段等待该任务结束。

## 播放流程

1. 应用完成鉴权并取得当前媒体来源 URL。
2. 应用为当前进程启动一个共享代理实例。
3. 应用使用稳定身份和来源信息调用 `makePlaybackUrl`。
4. Adapter 向 Core 注册来源，返回包含不透明请求 ID、但不包含 signed URL 的 localhost URL。
5. 平台播放器打开 localhost URL，并正常发送 Range 请求。
6. Core 从磁盘返回已完成区间，从已注册来源拉取缺失区间。
7. 授权过期时，Core 请求 Host 刷新来源，只重试失败的上游请求。
8. 应用在受控退出时停止 Core；意外进程终止由下次启动时的恢复流程处理。

不要把 signed URL 放进 localhost 路径或查询参数。播放器诊断、分析系统、崩溃日志或操作系统网络工具可能暴露该 URL。

## 来源刷新

生产播放必须提供 Host 回调，以处理播放过程中的 signed URL 过期：

```text
refreshSource(identity, reason) -> new Source
```

合同需要明确：

- 哪些上游响应触发刷新，通常为 `401` 或 `403`；
- 每个媒体身份同时只能有一个刷新任务；
- 有界重试次数；
- 播放或 Core 停止时可以取消；
- 回调线程和超时行为；
- 刷新后的 URL 不符合白名单或网络策略时必须拒绝。

回调不能通过日志或错误消息暴露 signed URL。

## iOS Adapter

构建脚本会把 Rust 静态库、C 头文件和 module map 打包为
`MediaProxyCache.xcframework`，并校验设备和模拟器切片。
iOS 压缩包还会在 `adapter/MediaProxyCache.swift` 中附带所有权封装。
模板通过 `MediaProxyCacheCore` 导入 C ABI；每次 iOS 构建都会针对打包的头文件和
module map 执行 `swiftc -typecheck`。模板会串行化 handle 访问，避免并发调用 start、
stop 和 close 产生竞态。
`setSourceRefreshProvider` 将同步 C 回调桥接为 `@Sendable` Swift 闭包；回调上下文
和返回的 UTF-8 缓冲区会保持到 Core 销毁完成，避免并发刷新产生悬空指针。

建议接口形态：

```swift
let cache = MediaProxyCache(configuration: configuration)
let endpoint = try await cache.start()
let playbackURL = try cache.makePlaybackURL(source: source, identity: identity)
let player = AVPlayer(url: playbackURL)
```

iOS 工作项：

- 构建真机和模拟器 slices；
- 提供不会跨边界抛异常的 C ABI、明确错误码和内存所有权；
- 明确 Swift 并发及回调队列；
- 使用应用提供的 App Support 或 Caches 目录；
- 验证 App Transport Security 下的本地 HTTP 播放；
- 验证 `AVPlayer` 的 Range 和 HLS 行为；
- 验证后台音频、中断、音频路由变化及进程恢复；
- 确保 App Store 真机产物不包含模拟器 slice。

## Android Adapter

编译 Rust 动态库，暴露 JNI 接口，并将 Kotlin API 与原生库打包为 AAR。

仓库现在包含基于 Rust `jni 0.22` 的 Bridge 和 AGP `9.3.1` library 工程。
`scripts/package-android-aar.sh` 会暂存 arm64-v8a、armeabi-v7a 和 x86_64 动态库，
构建 Release AAR，并校验 `classes.jar` 与三个 JNI 库。CI 使用 NDK
`29.0.14206865` 和 Gradle `9.7.0`；AGP 9.3.1 会拒绝之前配置的 Gradle 9.3.1。
ARM64 JNI 库、完整三 ABI Release AAR 和 Debug APK 已通过该工具链构建，仍需进行运行时验证。
JNI 暴露进程内不透明 token，而不是原生指针值。未知、已移除和重复销毁的 token 会在
访问原生内存前被拒绝，销毁操作也会与活动 JNI 调用串行化。
`SourceRefreshProvider` 会保存为 JNI 全局引用。Core 工作线程在调用前附加到 JVM，
回调上下文只会在原生服务器销毁并等待活动任务结束后释放。

建议接口形态：

```kotlin
val cache = MediaProxyCache.create(context, configuration)
val endpoint = cache.start()
val playbackUri = cache.makePlaybackUri(source, identity)
val player = ExoPlayer.Builder(context).build()
player.setMediaItem(MediaItem.fromUri(playbackUri))
```

Android 工作项：

- 为每个支持的 ABI 打包一个 `.so`；
- JNI handle 保持不透明，并验证每一个原生 handle；
- 使用应用提供的缓存目录；
- 定义进程终止和 Service 重建行为；
- 在应用 network-security 配置中验证 localhost cleartext 策略；
- 测试 Media3/ExoPlayer 的 Range、Seek、HLS、前台 Service 和后台播放；
- FFI 调用不能阻塞 Binder、主线程或播放器线程；
- 需要时添加 JNI 入口的 R8/ProGuard keep 规则。

## 鸿蒙 Adapter

使用鸿蒙工具链编译 Rust 动态库，通过 N-API 暴露稳定 C ABI，并将 ArkTS API 和原生库打包为 HAR。

建议接口形态：

```typescript
const cache = await MediaProxyCache.create(context, configuration)
const endpoint = await cache.start()
const playbackUrl = await cache.makePlaybackUrl(source, identity)
await avPlayer.setUrl(playbackUrl)
```

鸿蒙工作项：

- 根据支持的鸿蒙 SDK 版本验证 Rust target 和原生构建链；
- 根据产品设备矩阵验证打包的 ARM64 和 ARMv7 库；
- 将可能阻塞的 N-API 生命周期调用迁移到异步任务，并明确回调线程；
- 使用应用 Context 提供的沙箱路径；
- 验证 localhost 网络访问和 cleartext 策略；
- 测试 AVPlayer 的 Range、Seek、HLS、后台播放和应用恢复；
- 验证 debug/release 构建中的 HAR 加载与符号可见性。

`scripts/package-harmony-har.sh` 会暂存两个原生 ABI，启用 ArkTS type check 运行 Hvigor，
构建 `MediaProxyCache.har`，并校验声明文件和原生库条目。ARM64、ARMv7 Rust 库及最终
HAR 已使用 Rust 1.94、DevEco Hvigor 6.24.3 和 OpenHarmony API 24 完成构建验证。

### 鸿蒙 AVPlayer POC

`examples/harmony-player-poc` 中的应用使用 Host 沙箱缓存目录启动 N-API Adapter，
通过 `XComponent` Surface 渲染视频，并向 `AVPlayer` 传入源站地址和稳定缓存身份请求头。
运行 `scripts/build-harmony-player-poc.sh` 会交叉编译两套真实 OHOS 原生库、打包 HAR
并组装 unsigned HAP。该工程已在 macOS 使用 DevEco Hvigor 6.24.3 和 OpenHarmony
API 24 通过 ArkTS 类型检查及组装。验证时没有连接鸿蒙设备或模拟器，因此原生库加载、
播放、Range、Seek、HLS 和缓存行为仍属于真机验收项。

## 安全要求

- 仅绑定 `127.0.0.1`，不能绑定全部网络接口。
- 使用不可猜测的进程级令牌或不透明请求 ID，防止其他本地调用方任意使用代理。
- 强制精确匹配的上游域名白名单。
- 拒绝非 HTTP(S) 协议、URL 凭据、私网/保留地址和不安全重定向。
- 生产环境 HTTPS 使用 Rustls 和内置 WebPKI 根证书。除非 Core 明确提供由 Host 注入信任库的接口，否则不会信任企业私有 CA。
- 将已验证 DNS 结果固定到实际连接，关闭 DNS rebinding 的检查/使用时间窗口。
- 不能记录来源 URL、授权头、Cookie、不透明请求 ID 或稳定缓存身份。
- 只允许转发明确白名单中的上游请求头。
- 缓存文件必须位于应用沙箱内，并遵守平台数据保护要求。
- 根据产品和内容授权要求决定缓存媒体是否必须静态加密。

## 线程与 FFI 规则

- Rust panic 不能跨越 FFI 边界展开。
- 返回的字符串和字节缓冲区必须有明确所有权和释放函数。
- 长时间操作必须异步且可取消。
- 回调 Swift、Kotlin 或 ArkTS 时必须使用有文档说明的线程/队列。
- Host 对象销毁后，后续回调必须被安全失效。
- 错误应包含稳定错误码和脱敏消息，不能包含来源凭据。

## POC 验收标准

### iOS 模拟器播放器 POC

维护中的 AVPlayer 示例位于 `examples/ios-player-poc`。运行
`scripts/build-ios-player-poc.sh` 可生成测试专用 XCFramework 和 Xcode 工程。按照
示例 README 启动支持 Range 的本地源站，并设置 `MEDIA_PROXY_ORIGIN_URL`。

该脚本只对测试产物显式启用 `allow-private-upstream`。正常的
`scripts/build-mobile.sh` 默认不会启用此 feature，并且只允许 iOS 与 Android
POC 显式使用测试开关。不得发布 POC XCFramework。

### Android 模拟器播放器 POC

Media3 1.11.0 示例位于 `examples/android-player-poc`。运行
`scripts/build-android-player-poc.sh` 会为当前模拟器和真机交叉编译 ARM64
原生库并组装测试 APK；生产 Android 构建仍默认覆盖三个受支持 ABI。示例 README
说明了如何通过模拟器的 `10.0.2.2` 宿主机别名连接支持 Range 的本地源站。

已在 ARM64、API 35 模拟器验证：Media3 进入 `STATE_READY`，播放和向前 Seek
10 秒成功，完整的 3,434,642 字节文件及区间 sidecar 已落盘；使用变化后的 signed
URL 查询参数重新启动时命中同一缓存，源站没有新增请求。已连接的 MIUI API 31
真机通过设备安全策略拒绝 USB APK 安装，因此真机播放仍需用户先在设备上明确授权。

每个平台 POC 应证明：

- 在动态 localhost 端口启动并确定性停止；
- 首次播放下载并缓存媒体；
- signed URL 变化后的第二次播放命中同一缓存身份；
- Seek 到已缓存和未缓存区间；
- 两个播放器并发请求重叠区间；
- signed URL 过期后刷新且不中断播放；
- 离线重播已完成区间；
- 写入期间强制终止进程后的恢复；
- 容量限制和物理删除生效；
- 拒绝未授权域名和私网地址；
- 应用日志、崩溃报告和 localhost 请求 URL 中均无 signed URL；
- 符合各平台要求的前后台切换；
- 除模拟器外必须完成真机测试。

## 建议交付顺序

1. 增加托管语言刷新回调 Adapter。Core C ABI 已在上游 401/403 后单飞刷新并有限重试一次；三端 Adapter 已提供不透明来源注册和手动刷新/移除。
2. 将现有 Range、并发请求、损坏恢复、缓存清理、HLS 和网络策略单元及桌面集成测试持续作为发布门禁。
3. 使用 Media3 在真机上验证已打包的 Android AAR。
4. 根据 Android POC 固化共享生命周期和错误合同。
5. 使用真机 AVPlayer 验证已打包的 iOS XCFramework/Swift Adapter。
6. 构建鸿蒙 HAR/N-API Adapter，并验证 AVPlayer。
7. 完成跨平台验收矩阵后，再将 SDK 标记为生产可用。

## 当前仓库差距

当前仓库已经通过 C ABI 和三端 Adapter 提供 create/start/stop/destroy、不透明来源注册、手动刷新/移除、动态端口结果和 Host 缓存目录注入。Core C ABI 刷新回调支持单飞，并在上游 401/403 后只重试一次。仓库还包含构建/发布脚本、所有权封装、XCFramework 结构校验以及通过类型检查的鸿蒙 AVPlayer POC。AAR、Swift Adapter 和 HAR 仍需真机验证，托管语言刷新回调 Adapter 仍未完成。本文档是剩余移动 SDK 工作的实现与验收合同，不代表这些平台 Adapter 已经完成真机验证。
