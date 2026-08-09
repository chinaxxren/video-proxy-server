#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
POC_DIR="$ROOT_DIR/examples/ios-player-poc"
DIST_DIR="$ROOT_DIR/dist/ios-player-poc"

command -v xcodegen >/dev/null 2>&1 || {
  echo "xcodegen is required (brew install xcodegen)" >&2
  exit 1
}

PLATFORM=ios TEST_ALLOW_PRIVATE_UPSTREAM=1 \
  "$ROOT_DIR/scripts/build-mobile.sh" "$DIST_DIR"

cd "$POC_DIR"
xcodegen generate

echo "Generated $POC_DIR/MediaProxyCachePlayerPOC.xcodeproj"
echo "This POC artifact allows private upstreams and must not be distributed."
