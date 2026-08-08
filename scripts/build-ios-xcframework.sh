#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MOBILE_DIR="${1:-${ROOT_DIR}/dist/mobile}"
OUTPUT_DIR="${2:-${ROOT_DIR}/dist/ios}"
DEVICE_LIBRARY="$MOBILE_DIR/ios/aarch64-apple-ios/libproxy_server.a"
SIMULATOR_LIBRARY="$MOBILE_DIR/ios-sim/aarch64-apple-ios-sim/libproxy_server.a"
FRAMEWORK="$OUTPUT_DIR/MediaProxyCacheCore.xcframework"

if [[ ! -f "$DEVICE_LIBRARY" || ! -f "$SIMULATOR_LIBRARY" ]]; then
  PLATFORM=ios "$ROOT_DIR/scripts/build-mobile.sh" "$MOBILE_DIR"
fi

rm -rf "$OUTPUT_DIR"
mkdir -p "$OUTPUT_DIR/headers" "$OUTPUT_DIR/Sources"
cp "$ROOT_DIR/include/media_proxy_cache.h" "$OUTPUT_DIR/headers/"
cp "$ROOT_DIR/include/module.modulemap" "$OUTPUT_DIR/headers/"
cp "$ROOT_DIR/ios/MediaProxyCache.swift" "$OUTPUT_DIR/Sources/"

xcodebuild -create-xcframework \
  -library "$DEVICE_LIBRARY" -headers "$OUTPUT_DIR/headers" \
  -library "$SIMULATOR_LIBRARY" -headers "$OUTPUT_DIR/headers" \
  -output "$FRAMEWORK"

test -f "$FRAMEWORK/Info.plist"
slice_count="$(/usr/libexec/PlistBuddy -c 'Print :AvailableLibraries' "$FRAMEWORK/Info.plist" | grep -c 'Dict')"
if [[ "$slice_count" -ne 2 ]]; then
  echo "Expected two XCFramework slices, found $slice_count" >&2
  exit 1
fi

echo "iOS XCFramework written to $FRAMEWORK"
