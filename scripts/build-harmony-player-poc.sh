#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
POC_DIR="$ROOT_DIR/examples/harmony-player-poc"
HVIGOR_BIN="${HVIGORW:-/Applications/DevEco-Studio.app/Contents/tools/hvigor/bin/hvigorw}"
OHPM_BIN="${OHPM:-/Applications/DevEco-Studio.app/Contents/tools/ohpm/bin/ohpm}"

test -x "$HVIGOR_BIN" || { echo "Set HVIGORW to an executable hvigorw" >&2; exit 1; }
test -x "$OHPM_BIN" || { echo "Set OHPM to an executable ohpm" >&2; exit 1; }
test -f "$ROOT_DIR/platform/harmony/library/libs/arm64-v8a/libproxy_server.so" || {
  echo "Build or stage the HarmonyOS ARM64 Rust library before building the POC" >&2
  exit 1
}

(cd "$POC_DIR" && "$OHPM_BIN" install --all)
(cd "$POC_DIR" && "$HVIGOR_BIN" --no-daemon --mode module -p module=entry@default assembleHap)
find "$POC_DIR/entry/build" -type f -name '*.hap' -print
