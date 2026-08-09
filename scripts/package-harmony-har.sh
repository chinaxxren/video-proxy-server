#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INPUT_DIR="${1:-${ROOT_DIR}/dist/mobile/harmony}"
OUTPUT_DIR="${2:-${ROOT_DIR}/dist/mobile/harmony}"
MODULE_DIR="$ROOT_DIR/platform/harmony/library"
LIBS_DIR="$MODULE_DIR/libs"

rm -rf "$LIBS_DIR"
stage_abi() {
  local abi="$1" target="$2" library
  library="$INPUT_DIR/$target/libproxy_server.so"
  test -f "$library" || { echo "Missing HarmonyOS library: $library" >&2; exit 1; }
  mkdir -p "$LIBS_DIR/$abi"
  cp "$library" "$LIBS_DIR/$abi/"
}
stage_abi arm64-v8a aarch64-unknown-linux-ohos
stage_abi armeabi-v7a armv7-unknown-linux-ohos

hvigor="${HVIGORW:-}"
if [[ -z "$hvigor" && -n "${OHOS_NDK_HOME:-}" ]]; then
  hvigor="$(find "$OHOS_NDK_HOME" -type f -name hvigorw -print -quit)"
fi
test -n "$hvigor" && test -x "$hvigor" || {
  echo "Set HVIGORW to an executable HarmonyOS hvigorw" >&2
  exit 1
}

(cd "$ROOT_DIR/platform/harmony" && "$hvigor" --no-daemon --mode module -p module=media_proxy_cache@default assembleHar)
har="$(find "$MODULE_DIR/build" -type f -name '*.har' -print -quit)"
test -n "$har" && test -f "$har" || { echo "HarmonyOS HAR was not produced" >&2; exit 1; }
contents="$(tar -tzf "$har")"
for required in \
  package/Index.d.ts \
  package/libs/arm64-v8a/libproxy_server.so \
  package/libs/armeabi-v7a/libproxy_server.so; do
  grep -Fqx "$required" <<<"$contents" || {
    echo "HarmonyOS HAR is missing $required" >&2
    exit 1
  }
done
mkdir -p "$OUTPUT_DIR"
cp "$har" "$OUTPUT_DIR/MediaProxyCache.har"
echo "$OUTPUT_DIR/MediaProxyCache.har"
