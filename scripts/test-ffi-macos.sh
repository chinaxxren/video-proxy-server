#!/usr/bin/env bash
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CACHE_DIR="$(mktemp -d)"
trap 'rm -rf "$CACHE_DIR" "$ROOT_DIR/target/ffi-smoke"' EXIT
cargo build --locked --release --manifest-path "$ROOT_DIR/Cargo.toml"
cc "$ROOT_DIR/tests/ffi_smoke.c" -I"$ROOT_DIR/include" \
  -L"$ROOT_DIR/target/release" -lproxy_server \
  -o "$ROOT_DIR/target/ffi-smoke"
DYLD_LIBRARY_PATH="$ROOT_DIR/target/release" "$ROOT_DIR/target/ffi-smoke" "$CACHE_DIR"
