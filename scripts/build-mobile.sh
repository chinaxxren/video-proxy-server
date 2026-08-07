#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${1:-${ROOT_DIR}/dist/mobile}"
PROFILE="${PROFILE:-release}"

cd "$ROOT_DIR"
mkdir -p "$OUT_DIR/include"
cp include/media_proxy_cache.h "$OUT_DIR/include/"

build_target() {
  local platform="$1" target="$2"
  rustup target list --installed | rg -qx "$target" || {
    echo "Missing Rust target $target; install it with: rustup target add $target" >&2
    return 1
  }
  cargo build --locked --$PROFILE --target "$target"
  mkdir -p "$OUT_DIR/$platform/$target"
  cp "target/$target/$PROFILE/libproxy_server.a" "$OUT_DIR/$platform/$target/" 2>/dev/null || true
  cp "target/$target/$PROFILE/libproxy_server.dylib" "$OUT_DIR/$platform/$target/" 2>/dev/null || true
  cp "target/$target/$PROFILE/libproxy_server.so" "$OUT_DIR/$platform/$target/" 2>/dev/null || true
}

case "${PLATFORM:-all}" in
  macos) build_target macos aarch64-apple-darwin; build_target macos x86_64-apple-darwin ;;
  windows) build_target windows x86_64-pc-windows-gnu ;;
  ios) build_target ios aarch64-apple-ios; build_target ios-sim aarch64-apple-ios-sim ;;
  android) build_target android aarch64-linux-android; build_target android armv7-linux-androideabi; build_target android x86_64-linux-android ;;
  harmony) build_target harmony aarch64-unknown-linux-ohos ;;
  all) PLATFORM=macos "$0" "$OUT_DIR"; PLATFORM=windows "$0" "$OUT_DIR"; PLATFORM=ios "$0" "$OUT_DIR"; PLATFORM=android "$0" "$OUT_DIR"; PLATFORM=harmony "$0" "$OUT_DIR" ;;
  *) echo "Usage: PLATFORM={macos|windows|ios|android|harmony|all} $0 [output-dir]" >&2; exit 2 ;;
esac

echo "Mobile artifacts written to $OUT_DIR"
