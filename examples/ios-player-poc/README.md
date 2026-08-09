# iOS Simulator Player POC

This example plays the repository's `aa.mp4` through MediaProxyCache and AVPlayer.
It intentionally builds the core with `allow-private-upstream` so the Simulator can
reach a local test origin. Never distribute this test build.

```bash
./scripts/build-ios-player-poc.sh
cargo run --locked --features allow-private-upstream \
  --example local_playground -- aa.mp4
```

The playground prints a Range-capable source URL such as
`http://127.0.0.1:59442/media.mp4`. Set it as the scheme launch environment variable
`MEDIA_PROXY_ORIGIN_URL`, then build and launch
`examples/ios-player-poc/MediaProxyCachePlayerPOC.xcodeproj` on an iOS Simulator.
A plain `python -m http.server` is not suitable because it does not provide the
Range behavior required by AVPlayer in this test.

## iOS 模拟器播放器 POC

该示例通过 MediaProxyCache 和 AVPlayer 播放仓库中的 `aa.mp4`。为了让模拟器
访问本机测试源，构建时会明确启用 `allow-private-upstream`。该测试产物不得分发。

```bash
./scripts/build-ios-player-poc.sh
cargo run --locked --features allow-private-upstream \
  --example local_playground -- aa.mp4
```

联调场会输出一个支持 Range 的源站地址，例如
`http://127.0.0.1:59442/media.mp4`。将它设置为 Scheme 启动环境变量
`MEDIA_PROXY_ORIGIN_URL`，然后在 iOS 模拟器中构建并启动
`examples/ios-player-poc/MediaProxyCachePlayerPOC.xcodeproj`。普通的
`python -m http.server` 不提供本测试中 AVPlayer 所需的 Range 行为，不能用作源站。
