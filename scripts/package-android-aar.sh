#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INPUT_DIR="${1:-${ROOT_DIR}/dist/mobile/android}"
OUTPUT_DIR="${2:-${ROOT_DIR}/dist/mobile/android}"
JNI_DIR="$ROOT_DIR/platform/android/library/src/main/jniLibs"

rm -rf "$JNI_DIR"
stage_abi() {
  local abi="$1" target="$2" library
  library="$INPUT_DIR/$target/libproxy_server.so"
  test -f "$library" || { echo "Missing Android library: $library" >&2; exit 1; }
  mkdir -p "$JNI_DIR/$abi"
  cp "$library" "$JNI_DIR/$abi/"
}
stage_abi arm64-v8a aarch64-linux-android
stage_abi armeabi-v7a armv7-linux-androideabi
stage_abi x86_64 x86_64-linux-android

GRADLE_BIN="${GRADLE_BIN:-}"
if [[ -z "$GRADLE_BIN" ]]; then
  GRADLE_BIN="$(command -v gradle || true)"
fi
if [[ -z "$GRADLE_BIN" ]]; then
  GRADLE_BIN="$(find "${GRADLE_USER_HOME:-$HOME/.gradle}/wrapper/dists" -type f -path '*/bin/gradle' -perm -111 2>/dev/null | sort -V | tail -1 || true)"
fi
[[ -x "$GRADLE_BIN" ]] || {
  echo "Gradle is required to package the Android AAR; set GRADLE_BIN to Gradle 9.5+" >&2
  exit 1
}
"$GRADLE_BIN" --no-daemon -p "$ROOT_DIR/platform/android" :library:assembleRelease

aar="$ROOT_DIR/platform/android/library/build/outputs/aar/library-release.aar"
test -f "$aar" || { echo "Android AAR was not produced" >&2; exit 1; }
contents="$(unzip -Z1 "$aar")"
for required in \
  classes.jar \
  jni/arm64-v8a/libproxy_server.so \
  jni/armeabi-v7a/libproxy_server.so \
  jni/x86_64/libproxy_server.so; do
  grep -Fqx "$required" <<<"$contents" || {
    echo "Android AAR is missing $required" >&2
    exit 1
  }
done
mkdir -p "$OUTPUT_DIR"
cp "$aar" "$OUTPUT_DIR/MediaProxyCache.aar"
echo "$OUTPUT_DIR/MediaProxyCache.aar"
