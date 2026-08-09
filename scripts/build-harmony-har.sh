#!/usr/bin/env bash
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MOBILE_DIR="${1:-${ROOT_DIR}/dist/mobile}"
OUTPUT_DIR="${2:-${ROOT_DIR}/dist/harmony-sdk}"
OHOS_NDK_HOME="${OHOS_NDK_HOME:-}"
test -n "$OHOS_NDK_HOME" || { echo "Set OHOS_NDK_HOME to the HarmonyOS native SDK" >&2; exit 1; }
test -f "$OHOS_NDK_HOME/native/llvm/include/node_api.h" || { echo "Missing node_api.h" >&2; exit 1; }
SOURCE="$MOBILE_DIR/harmony/aarch64-unknown-linux-ohos/libproxy_server.so"
test -f "$SOURCE" || { echo "Missing HarmonyOS Rust library: $SOURCE" >&2; exit 1; }
rm -rf "$OUTPUT_DIR"
mkdir -p "$OUTPUT_DIR/libs/arm64-v8a" "$OUTPUT_DIR/include" "$OUTPUT_DIR/native"
cp "$SOURCE" "$OUTPUT_DIR/libs/arm64-v8a/libproxy_server.so"
cp "$ROOT_DIR/include/media_proxy_cache.h" "$OUTPUT_DIR/include/"
cp "$ROOT_DIR/harmony/Index.ets" "$OUTPUT_DIR/"
cp "$ROOT_DIR/harmony/oh-package.json5" "$OUTPUT_DIR/"
FEATURE_FLAGS=()
if [[ "${LIBRQBIT_ENABLED:-0}" == "1" ]]; then
  FEATURE_FLAGS+=(-DMEDIA_PROXY_CACHE_ENABLE_LIBRQBIT)
fi
clang -fPIC -shared -Werror "${FEATURE_FLAGS[@]}" -I "$OHOS_NDK_HOME/native/llvm/include" -I "$ROOT_DIR/include" \
  "$ROOT_DIR/harmony/native/media_proxy_cache_napi.c" -L "$OUTPUT_DIR/libs/arm64-v8a" \
  -lproxy_server -o "$OUTPUT_DIR/native/libmedia_proxy_cache_napi.so"
tar -czf "$OUTPUT_DIR/media-proxy-cache-harmony.har.tar.gz" -C "$OUTPUT_DIR" Index.ets oh-package.json5 include libs native
echo "HarmonyOS HAR archive written to $OUTPUT_DIR/media-proxy-cache-harmony.har.tar.gz"
