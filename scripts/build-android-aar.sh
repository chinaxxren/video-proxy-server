#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MOBILE_DIR="${1:-${ROOT_DIR}/dist/mobile}"
OUTPUT_DIR="${2:-${ROOT_DIR}/dist/android-sdk}"
MODULE_DIR="$ROOT_DIR/android/media-proxy-cache"
JNI_DIR="$MODULE_DIR/src/main/jniLibs"
GRADLE_COMMAND="${GRADLE_COMMAND:-gradle}"

copy_library() {
  local target="$1" abi="$2"
  local source="$MOBILE_DIR/android/$target/libproxy_server.so"
  test -f "$source" || {
    echo "Missing Android library: $source" >&2
    exit 1
  }
  mkdir -p "$JNI_DIR/$abi"
  cp "$source" "$JNI_DIR/$abi/libproxy_server.so"
}

copy_library aarch64-linux-android arm64-v8a
copy_library armv7-linux-androideabi armeabi-v7a
copy_library x86_64-linux-android x86_64

"$GRADLE_COMMAND" -p "$ROOT_DIR/android" --no-daemon \
  :media-proxy-cache:testReleaseUnitTest \
  :media-proxy-cache:lintRelease \
  :media-proxy-cache:assembleRelease

mkdir -p "$OUTPUT_DIR"
cp "$MODULE_DIR/build/outputs/aar/media-proxy-cache-release.aar" \
  "$OUTPUT_DIR/media-proxy-cache.aar"

echo "Android AAR written to $OUTPUT_DIR/media-proxy-cache.aar"
