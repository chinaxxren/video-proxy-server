# Android Media3 Player POC

This example plays `aa.mp4` through MediaProxyCache and Media3 1.11.0. It uses a
test-only native build that permits the Android Emulator to reach the host origin.
Never distribute this APK or its staged native libraries.

Prerequisites: Android SDK 37.0, NDK 29.0.14206865, Gradle 9.5 or newer, and an
ARM64 Emulator or device. The validated local build used Gradle 9.7.0.

```bash
cargo run --locked --features allow-private-upstream \
  --example local_playground -- aa.mp4
./scripts/build-android-player-poc.sh
adb install -r examples/android-player-poc/app/build/outputs/apk/debug/app-debug.apk
adb shell am start -n com.example.mediaproxy.poc/.MainActivity \
  --es origin_url http://10.0.2.2:ORIGIN_PORT/media.mp4
```

Replace `ORIGIN_PORT` with the port printed in the playground source URL. The
emulator alias `10.0.2.2` reaches the Mac host.

For a USB device, run `adb -s DEVICE_SERIAL reverse tcp:ORIGIN_PORT
tcp:ORIGIN_PORT` and launch with `http://127.0.0.1:ORIGIN_PORT/media.mp4`.

## Android Media3 播放器 POC

该示例通过 MediaProxyCache 和 Media3 1.11.0 播放 `aa.mp4`。原生测试构建允许
Android 模拟器访问宿主机源站，该 APK 及其暂存原生库不得分发。

环境要求：Android SDK 37.0、NDK 29.0.14206865、Gradle 9.5 或更高版本，以及
ARM64 模拟器或真机。本地验证使用 Gradle 9.7.0。

```bash
cargo run --locked --features allow-private-upstream \
  --example local_playground -- aa.mp4
./scripts/build-android-player-poc.sh
adb install -r examples/android-player-poc/app/build/outputs/apk/debug/app-debug.apk
adb shell am start -n com.example.mediaproxy.poc/.MainActivity \
  --es origin_url http://10.0.2.2:ORIGIN_PORT/media.mp4
```

将 `ORIGIN_PORT` 替换为联调场输出的源站端口。模拟器通过 `10.0.2.2` 访问 Mac
宿主机。

USB 真机可先执行 `adb -s DEVICE_SERIAL reverse tcp:ORIGIN_PORT
tcp:ORIGIN_PORT`，再使用 `http://127.0.0.1:ORIGIN_PORT/media.mp4` 启动应用。
