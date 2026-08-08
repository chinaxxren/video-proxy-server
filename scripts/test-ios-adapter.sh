#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_DIR="$(mktemp -d)"
trap 'rm -rf "$TEST_DIR"' EXIT

cd "$ROOT_DIR"
cargo build --locked --release --lib
swiftc \
  -I "$ROOT_DIR/include" \
  -L "$ROOT_DIR/target/release" \
  -lproxy_server \
  "$ROOT_DIR/ios/MediaProxyCache.swift" \
  "$ROOT_DIR/ios/MediaProxyCacheSmokeTest.swift" \
  -o "$TEST_DIR/ios-adapter-smoke"
DYLD_LIBRARY_PATH="$ROOT_DIR/target/release" "$TEST_DIR/ios-adapter-smoke"

echo "iOS Swift adapter smoke test passed"
