#!/usr/bin/env bash
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INPUT_DIR="${1:-${ROOT_DIR}/dist/mobile}"
OUTPUT_DIR="${2:-${ROOT_DIR}/dist/releases}"
VERSION="${VERSION:-$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/Cargo.toml" | head -1)}"
test -d "$INPUT_DIR" || { echo "Missing artifact directory: $INPUT_DIR" >&2; exit 1; }
mkdir -p "$OUTPUT_DIR"
ARCHIVE="$OUTPUT_DIR/media-proxy-cache-v${VERSION}-mobile.tar.gz"
tar -czf "$ARCHIVE" -C "$INPUT_DIR" .
echo "$ARCHIVE"
