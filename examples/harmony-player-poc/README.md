# HarmonyOS AVPlayer POC

This application exercises the HarmonyOS HAR adapter with `AVPlayer`, HTTP
Range requests, seeking, and stable cache identity headers. It is a test
application, not a production client SDK.

Build it with:

```bash
scripts/build-harmony-player-poc.sh
```

The script uses the DevEco OpenHarmony SDK by default, cross-compiles real ARM64
and ARMv7 Rust libraries, packages the HAR, and assembles an unsigned HAP. Set
`OHOS_NDK_HOME`, `HVIGORW`, and `OHPM` when the tools are installed elsewhere.
The two Rust OHOS targets must already be installed. This POC build explicitly
enables private upstreams for local testing; production builds do not.

Enter a Range-capable HTTP media URL or an HLS playlist URL in the application.
The POC parses that URL and starts the proxy with its exact hostname allowlisted.
For local development, run the repository's `local_playground` example and use
its printed source URL. A signed HAP or a connected HarmonyOS device is required
for real playback validation.

## 中文

此应用使用鸿蒙 `AVPlayer` 验证 HAR Adapter、HTTP Range、Seek 和稳定缓存身份请求头。
它是测试应用，不是生产客户端 SDK。

运行 `scripts/build-harmony-player-poc.sh` 会使用 DevEco OpenHarmony SDK 交叉编译真实
ARM64/ARMv7 Rust 库、打包 HAR 并组装 unsigned HAP。工具安装在其他位置时请设置
`OHOS_NDK_HOME`、`HVIGORW` 和 `OHPM`；两个 Rust OHOS target 需要预先安装。该 POC
会显式启用私网源站测试能力，生产构建默认不会启用。在应用中输入支持 Range 的
HTTP 媒体地址或 HLS 播放列表地址。本地测试可运行仓库的 `local_playground` 示例，
应用会解析该 URL，并只将其精确 hostname 加入代理白名单。使用其输出的源站地址时，
测试原生库还必须显式启用私网源站 feature。真实播放验证需要签名后的 HAP 和已连接的鸿蒙设备。
