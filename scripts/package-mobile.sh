#!/usr/bin/env bash
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INPUT_DIR="${1:-${ROOT_DIR}/dist/mobile}"
OUTPUT_DIR="${2:-${ROOT_DIR}/dist/releases}"
VERSION="${VERSION:-$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/Cargo.toml" | head -1)}"
P2P_ENABLED="${P2P_ENABLED:-0}"
if [[ "$P2P_ENABLED" != "0" && "$P2P_ENABLED" != "1" ]]; then
  echo "P2P_ENABLED must be 0 or 1" >&2
  exit 2
fi
test -d "$INPUT_DIR" || { echo "Missing artifact directory: $INPUT_DIR" >&2; exit 1; }
FEATURE_MARKER="$INPUT_DIR/build-features.txt"
test -f "$FEATURE_MARKER" || { echo "Missing build feature marker: $FEATURE_MARKER" >&2; exit 1; }
EXPECTED_FEATURE="p2p_enabled=$P2P_ENABLED"
if ! grep -Fqx "$EXPECTED_FEATURE" "$FEATURE_MARKER"; then
  echo "Build feature marker does not match requested package: expected $EXPECTED_FEATURE" >&2
  exit 1
fi
HEADER="$INPUT_DIR/include/media_proxy_cache.h"
test -f "$HEADER" || { echo "Missing public C header: $HEADER" >&2; exit 1; }
NATIVE_ARTIFACT="$(find "$INPUT_DIR" -type f \( \
  -name 'libproxy_server.a' -o \
  -name 'libproxy_server.dylib' -o \
  -name 'libproxy_server.so' -o \
  -name 'proxy_server.dll' -o \
  -name 'proxy_server.dll.a' -o \
  -name 'proxy_server.lib' \
\) -print -quit)"
test -n "$NATIVE_ARTIFACT" || {
  echo "No native proxy library found in $INPUT_DIR" >&2
  exit 1
}
mkdir -p "$OUTPUT_DIR"
P2P_SUFFIX=""
if [[ "$P2P_ENABLED" == "1" ]]; then
  P2P_SUFFIX="-p2p"
fi
ARCHIVE="$OUTPUT_DIR/media-proxy-cache-v${VERSION}-mobile${P2P_SUFFIX}.tar.gz"
tar -czf "$ARCHIVE" -C "$INPUT_DIR" .
CHECKSUMS="$OUTPUT_DIR/SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$OUTPUT_DIR" && sha256sum "$(basename "$ARCHIVE")") > "$CHECKSUMS"
elif command -v shasum >/dev/null 2>&1; then
  (cd "$OUTPUT_DIR" && shasum -a 256 "$(basename "$ARCHIVE")") > "$CHECKSUMS"
else
  echo "Neither sha256sum nor shasum is available; cannot write $CHECKSUMS" >&2
  exit 1
fi
echo "$ARCHIVE"
echo "$CHECKSUMS"
