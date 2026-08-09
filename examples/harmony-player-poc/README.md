# HarmonyOS AVPlayer POC

This application exercises the HarmonyOS HAR adapter with `AVPlayer`, HTTP
Range requests, seeking, and stable cache identity headers. It is a test
application, not a production client SDK.

Build it with:

```bash
scripts/build-harmony-player-poc.sh
```

Enter a Range-capable HTTP media URL or an HLS playlist URL in the application.
The POC parses that URL and starts the proxy with its exact hostname allowlisted.
For local development, run the repository's `local_playground` example and use
its printed source URL. A signed HAP or a connected HarmonyOS device is required
for real playback validation.

## 中文

此应用使用鸿蒙 `AVPlayer` 验证 HAR Adapter、HTTP Range、Seek 和稳定缓存身份请求头。
它是测试应用，不是生产客户端 SDK。

运行 `scripts/build-harmony-player-poc.sh` 进行构建。在应用中输入支持 Range 的
HTTP 媒体地址或 HLS 播放列表地址。本地测试可运行仓库的 `local_playground` 示例，
应用会解析该 URL，并只将其精确 hostname 加入代理白名单。使用其输出的源站地址时，
测试原生库还必须显式启用私网源站 feature。真实播放验证需要签名后的 HAP 和已连接的鸿蒙设备。
