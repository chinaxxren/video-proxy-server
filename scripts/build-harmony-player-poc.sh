#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
POC_DIR="$ROOT_DIR/examples/harmony-player-poc"
HVIGOR_BIN="${HVIGORW:-/Applications/DevEco-Studio.app/Contents/tools/hvigor/bin/hvigorw}"
OHPM_BIN="${OHPM:-/Applications/DevEco-Studio.app/Contents/tools/ohpm/bin/ohpm}"
OHOS_SDK="${OHOS_NDK_HOME:-/Applications/DevEco-Studio.app/Contents/sdk/default/openharmony}"
LLVM_BIN="$OHOS_SDK/native/llvm/bin"
BUILD_DIR="$(mktemp -d)"
trap 'rm -rf "$BUILD_DIR"' EXIT

test -x "$HVIGOR_BIN" || { echo "Set HVIGORW to an executable hvigorw" >&2; exit 1; }
test -x "$OHPM_BIN" || { echo "Set OHPM to an executable ohpm" >&2; exit 1; }
test -x "$LLVM_BIN/aarch64-unknown-linux-ohos-clang" || {
  echo "Set OHOS_NDK_HOME to an OpenHarmony SDK containing the native LLVM toolchain" >&2
  exit 1
}
for target in aarch64-unknown-linux-ohos armv7-unknown-linux-ohos; do
  rustup target list --installed | grep -Fqx "$target" || {
    echo "Missing Rust target $target; install it with: rustup target add $target" >&2
    exit 1
  }
done

export OHOS_NDK_HOME="$OHOS_SDK"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_OHOS_LINKER="$LLVM_BIN/aarch64-unknown-linux-ohos-clang"
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_OHOS_LINKER="$LLVM_BIN/armv7-unknown-linux-ohos-clang"
export CC_aarch64_unknown_linux_ohos="$CARGO_TARGET_AARCH64_UNKNOWN_LINUX_OHOS_LINKER"
export CC_armv7_unknown_linux_ohos="$CARGO_TARGET_ARMV7_UNKNOWN_LINUX_OHOS_LINKER"
export AR_aarch64_unknown_linux_ohos="$LLVM_BIN/llvm-ar"
export AR_armv7_unknown_linux_ohos="$LLVM_BIN/llvm-ar"
export NM="$LLVM_BIN/llvm-nm"

PLATFORM=harmony TEST_ALLOW_PRIVATE_UPSTREAM=1 \
  "$ROOT_DIR/scripts/build-mobile.sh" "$BUILD_DIR/mobile"
HVIGORW="$HVIGOR_BIN" "$ROOT_DIR/scripts/package-harmony-har.sh" \
  "$BUILD_DIR/mobile/harmony" "$BUILD_DIR/mobile/harmony"

(cd "$POC_DIR" && "$OHPM_BIN" install --all)
(cd "$POC_DIR" && "$HVIGOR_BIN" --no-daemon --mode module -p module=entry@default assembleHap)
find "$POC_DIR/entry/build" -type f -name '*.hap' -print
