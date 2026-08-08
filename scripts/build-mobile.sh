#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${1:-${ROOT_DIR}/dist/mobile}"
PROFILE="${PROFILE:-release}"

cd "$ROOT_DIR"
mkdir -p "$OUT_DIR/include"
cp include/media_proxy_cache.h "$OUT_DIR/include/"

build_target() {
  local platform="$1" target="$2" crate_type="${3:-}"
  rustup target list --installed | grep -Fxq "$target" || {
    echo "Missing Rust target $target; install it with: rustup target add $target" >&2
    return 1
  }
  if [[ -n "$crate_type" ]]; then
    cargo rustc --locked --$PROFILE --target "$target" --lib -- --crate-type="$crate_type"
  else
    cargo build --locked --$PROFILE --target "$target" --lib
  fi
  mkdir -p "$OUT_DIR/$platform/$target"
  local copied=0
  for artifact in \
    "target/$target/$PROFILE/libproxy_server.a" \
    "target/$target/$PROFILE/libproxy_server.dylib" \
    "target/$target/$PROFILE/libproxy_server.so" \
    "target/$target/$PROFILE/proxy_server.dll" \
    "target/$target/$PROFILE/proxy_server.dll.a" \
    "target/$target/$PROFILE/proxy_server.lib"; do
    if [[ -f "$artifact" ]]; then
      cp "$artifact" "$OUT_DIR/$platform/$target/"
      copied=1
    fi
  done
  if [[ "$copied" -eq 0 ]]; then
    echo "No native library artifact produced for target $target" >&2
    return 1
  fi
}

case "${PLATFORM:-all}" in
  macos) build_target macos aarch64-apple-darwin; build_target macos x86_64-apple-darwin ;;
  windows) build_target windows x86_64-pc-windows-gnu ;;
  ios) build_target ios aarch64-apple-ios staticlib; build_target ios-sim aarch64-apple-ios-sim staticlib ;;
  android) build_target android aarch64-linux-android cdylib; build_target android armv7-linux-androideabi cdylib; build_target android x86_64-linux-android cdylib ;;
  harmony) build_target harmony aarch64-unknown-linux-ohos cdylib ;;
  all) PLATFORM=macos "$0" "$OUT_DIR"; PLATFORM=windows "$0" "$OUT_DIR"; PLATFORM=ios "$0" "$OUT_DIR"; PLATFORM=android "$0" "$OUT_DIR"; PLATFORM=harmony "$0" "$OUT_DIR" ;;
  *) echo "Usage: PLATFORM={macos|windows|ios|android|harmony|all} $0 [output-dir]" >&2; exit 2 ;;
esac

echo "Mobile artifacts written to $OUT_DIR"
