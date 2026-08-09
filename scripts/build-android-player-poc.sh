#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="$ROOT_DIR/dist/android-player-poc"
ANDROID_SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
NDK_VERSION="${NDK_VERSION:-29.0.14206865}"
NDK_HOME="${NDK_HOME:-$ANDROID_SDK/ndk/$NDK_VERSION}"
GRADLE_BIN="${GRADLE_BIN:-gradle}"

test -n "$ANDROID_SDK" || { echo "Set ANDROID_HOME or ANDROID_SDK_ROOT" >&2; exit 1; }
test -d "$NDK_HOME/toolchains/llvm/prebuilt" || { echo "Missing Android NDK: $NDK_HOME" >&2; exit 1; }
command -v "$GRADLE_BIN" >/dev/null 2>&1 || { echo "Gradle 9.5 or newer is required" >&2; exit 1; }
grep -Fqx "Pkg.Revision = $NDK_VERSION" "$NDK_HOME/source.properties" || {
  echo "NDK at $NDK_HOME is not version $NDK_VERSION" >&2
  exit 1
}
GRADLE_VERSION="$("$GRADLE_BIN" --version | awk '/^Gradle / { print $2; exit }')"
awk -v version="$GRADLE_VERSION" 'BEGIN {
  split(version, parts, ".")
  exit !((parts[1] > 9) || (parts[1] == 9 && parts[2] >= 5))
}' || {
  echo "Gradle 9.5 or newer is required; found ${GRADLE_VERSION:-unknown}" >&2
  exit 1
}

HOST_TAG="$(find "$NDK_HOME/toolchains/llvm/prebuilt" -mindepth 1 -maxdepth 1 -type d -exec basename {} \; | head -1)"
TOOLCHAIN="$NDK_HOME/toolchains/llvm/prebuilt/$HOST_TAG/bin"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$TOOLCHAIN/aarch64-linux-android23-clang"
export CC_aarch64_linux_android="$CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER"
export AR_aarch64_linux_android="$TOOLCHAIN/llvm-ar"

PLATFORM=android ANDROID_ARM64_ONLY=1 TEST_ALLOW_PRIVATE_UPSTREAM=1 \
  "$ROOT_DIR/scripts/build-mobile.sh" "$DIST_DIR"

rm -rf "$ROOT_DIR/platform/android/library/src/main/jniLibs"
mkdir -p "$ROOT_DIR/platform/android/library/src/main/jniLibs/arm64-v8a"
cp "$DIST_DIR/android/aarch64-linux-android/libproxy_server.so" \
  "$ROOT_DIR/platform/android/library/src/main/jniLibs/arm64-v8a/"

"$GRADLE_BIN" --no-daemon -p "$ROOT_DIR/examples/android-player-poc" :app:assembleDebug
echo "$ROOT_DIR/examples/android-player-poc/app/build/outputs/apk/debug/app-debug.apk"
echo "This POC artifact allows private upstreams and must not be distributed."
