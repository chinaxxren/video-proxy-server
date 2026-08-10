#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${1:-${ROOT_DIR}/dist/mobile}"
PROFILE="${PROFILE:-release}"
P2P_ENABLED="${P2P_ENABLED:-0}"
LIBRQBIT_ENABLED="${LIBRQBIT_ENABLED:-0}"
TEST_ALLOW_PRIVATE_UPSTREAM="${TEST_ALLOW_PRIVATE_UPSTREAM:-0}"
IOS_DEPLOYMENT_TARGET="${IOS_DEPLOYMENT_TARGET:-13.0}"
ANDROID_ARM64_ONLY="${ANDROID_ARM64_ONLY:-0}"
ANDROID_API_LEVEL="${ANDROID_API_LEVEL:-21}"

if [[ "$P2P_ENABLED" != "0" && "$P2P_ENABLED" != "1" ]]; then
  echo "P2P_ENABLED must be 0 or 1" >&2
  exit 2
fi
if [[ "$LIBRQBIT_ENABLED" != "0" && "$LIBRQBIT_ENABLED" != "1" ]]; then
  echo "LIBRQBIT_ENABLED must be 0 or 1" >&2
  exit 2
fi
if [[ "$TEST_ALLOW_PRIVATE_UPSTREAM" != "0" && "$TEST_ALLOW_PRIVATE_UPSTREAM" != "1" ]]; then
  echo "TEST_ALLOW_PRIVATE_UPSTREAM must be 0 or 1" >&2
  exit 2
fi
if [[ "$ANDROID_ARM64_ONLY" != "0" && "$ANDROID_ARM64_ONLY" != "1" ]]; then
  echo "ANDROID_ARM64_ONLY must be 0 or 1" >&2
  exit 2
fi
if ! [[ "$ANDROID_API_LEVEL" =~ ^[0-9]+$ ]] || (( ANDROID_API_LEVEL < 21 )); then
  echo "ANDROID_API_LEVEL must be an integer greater than or equal to 21" >&2
  exit 2
fi

configure_android_toolchain() {
  local ndk_root="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}" host_tag toolchain
  [[ -n "$ndk_root" ]] || return 0
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64|Darwin-x86_64) host_tag=darwin-x86_64 ;;
    Linux-x86_64) host_tag=linux-x86_64 ;;
    *) echo "Unsupported Android NDK host: $(uname -s)-$(uname -m)" >&2; return 1 ;;
  esac
  toolchain="$ndk_root/toolchains/llvm/prebuilt/$host_tag/bin"
  [[ -x "$toolchain/llvm-ar" ]] || {
    echo "Invalid Android NDK toolchain: $toolchain" >&2
    return 1
  }
  export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$toolchain/aarch64-linux-android${ANDROID_API_LEVEL}-clang"
  export CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER="$toolchain/armv7a-linux-androideabi${ANDROID_API_LEVEL}-clang"
  export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$toolchain/x86_64-linux-android${ANDROID_API_LEVEL}-clang"
  export CC_aarch64_linux_android="$CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER"
  export CC_armv7_linux_androideabi="$CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER"
  export CC_x86_64_linux_android="$CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER"
  export AR_aarch64_linux_android="$toolchain/llvm-ar"
  export AR_armv7_linux_androideabi="$toolchain/llvm-ar"
  export AR_x86_64_linux_android="$toolchain/llvm-ar"
}

configure_harmony_toolchain() {
  local sdk_root="${OHOS_NDK_HOME:-${OHOS_SDK_HOME:-}}" toolchain aarch64_clang armv7_clang
  [[ -n "$sdk_root" ]] || return 0
  toolchain="$sdk_root/native/llvm/bin"
  aarch64_clang="$(find "$toolchain" -maxdepth 1 -type f \( -name aarch64-unknown-linux-ohos-clang -o -name aarch64-linux-ohos-clang \) -print -quit 2>/dev/null)"
  armv7_clang="$(find "$toolchain" -maxdepth 1 -type f -name armv7-unknown-linux-ohos-clang -print -quit 2>/dev/null)"
  [[ -x "$aarch64_clang" && -x "$armv7_clang" && -x "$toolchain/llvm-ar" ]] || {
    echo "Invalid OpenHarmony native LLVM toolchain: $toolchain" >&2
    return 1
  }
  export OHOS_NDK_HOME="$sdk_root"
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_OHOS_LINKER="$aarch64_clang"
  export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_OHOS_LINKER="$armv7_clang"
  export CC_aarch64_unknown_linux_ohos="$aarch64_clang"
  export CC_armv7_unknown_linux_ohos="$armv7_clang"
  export AR_aarch64_unknown_linux_ohos="$toolchain/llvm-ar"
  export AR_armv7_unknown_linux_ohos="$toolchain/llvm-ar"
  if [[ -x "$toolchain/llvm-nm" ]]; then
    export NM="$toolchain/llvm-nm"
  fi
}

if [[ "${PLATFORM:-all}" == "android" ]]; then
  configure_android_toolchain
fi
if [[ "${PLATFORM:-all}" == "harmony" ]]; then
  configure_harmony_toolchain
fi

CARGO_FEATURES=()
if [[ "$P2P_ENABLED" == "1" ]]; then
  CARGO_FEATURES+=(p2p)
fi
if [[ "$LIBRQBIT_ENABLED" == "1" ]]; then
  CARGO_FEATURES+=(p2p-librqbit)
fi
if [[ "$TEST_ALLOW_PRIVATE_UPSTREAM" == "1" ]]; then
  [[ "${PLATFORM:-all}" == "ios" || "${PLATFORM:-all}" == "android" || "${PLATFORM:-all}" == "harmony" ]] || {
    echo "TEST_ALLOW_PRIVATE_UPSTREAM is restricted to explicit mobile POC builds" >&2
    exit 2
  }
  CARGO_FEATURES+=(allow-private-upstream)
fi
if [[ "${PLATFORM:-all}" == "android" ]]; then
  CARGO_FEATURES+=(android-jni)
fi
if [[ "${PLATFORM:-all}" == "harmony" ]]; then
  CARGO_FEATURES+=(harmony-napi)
fi
CARGO_FEATURE_ARGS=()
if [[ "${#CARGO_FEATURES[@]}" -gt 0 ]]; then
  CARGO_FEATURE_ARGS=(--features "$(IFS=,; echo "${CARGO_FEATURES[*]}")")
fi

cd "$ROOT_DIR"
mkdir -p "$OUT_DIR/include"
cp include/media_proxy_cache.h "$OUT_DIR/include/"
cp include/module.modulemap "$OUT_DIR/include/"
printf 'p2p_enabled=%s\n' "$P2P_ENABLED" > "$OUT_DIR/build-features.txt"
printf 'librqbit_enabled=%s\n' "$LIBRQBIT_ENABLED" >> "$OUT_DIR/build-features.txt"
printf 'test_allow_private_upstream=%s\n' "$TEST_ALLOW_PRIVATE_UPSTREAM" >> "$OUT_DIR/build-features.txt"
printf 'android_arm64_only=%s\n' "$ANDROID_ARM64_ONLY" >> "$OUT_DIR/build-features.txt"

verify_native_symbols() {
  local artifact="$1" platform="$2" nm_tool
  [[ "$P2P_ENABLED" == "1" || "$LIBRQBIT_ENABLED" == "1" || "$platform" == "android" || "$platform" == "harmony" ]] || return 0
  if [[ -n "${NM:-}" ]]; then
    nm_tool="$NM"
  elif command -v llvm-nm >/dev/null 2>&1; then
    nm_tool="$(command -v llvm-nm)"
  else
    nm_tool="$(command -v nm || true)"
  fi
  [[ -n "$nm_tool" ]] || { echo "No nm tool available to verify P2P ABI" >&2; return 1; }
  local symbols
  symbols="$("$nm_tool" -g "$artifact" 2>/dev/null)" || {
    echo "Unable to inspect native symbols in $artifact" >&2
    return 1
  }
  if [[ "$P2P_ENABLED" == "1" ]]; then
    for symbol in \
      proxy_p2p_source_register \
      proxy_p2p_source_register_directory \
      proxy_p2p_source_verify_complete \
      proxy_p2p_source_remove; do
      rg -q "[[:space:]]_?${symbol}$" <<<"$symbols" || {
        echo "Missing P2P ABI symbol $symbol in $artifact" >&2
        return 1
      }
    done
  fi
  if [[ "$LIBRQBIT_ENABLED" == "1" ]]; then
    for symbol in \
      proxy_torrent_add_authorized \
      proxy_torrent_add_file_authorized \
      proxy_torrent_remove \
      proxy_torrent_files_json \
      proxy_torrent_status_json \
      proxy_torrent_set_paused \
      proxy_torrent_set_download_limit \
      proxy_torrent_select_files; do
      rg -q "[[:space:]]_?${symbol}$" <<<"$symbols" || {
        echo "Missing librqbit ABI symbol $symbol in $artifact" >&2
        return 1
      }
    done
  fi
  if [[ "$platform" == "android" ]]; then
    for symbol in \
      Java_com_example_mediaproxy_MediaProxyCache_nativeCreate \
      Java_com_example_mediaproxy_MediaProxyCache_nativeStart \
      Java_com_example_mediaproxy_MediaProxyCache_nativeStop \
      Java_com_example_mediaproxy_MediaProxyCache_nativeDestroy; do
      rg -q "[[:space:]]${symbol}$" <<<"$symbols" || {
        echo "Missing Android JNI symbol $symbol in $artifact" >&2
        return 1
      }
    done
    if [[ "$P2P_ENABLED" == "1" ]]; then
      for symbol in \
        Java_com_example_mediaproxy_MediaProxyCache_nativeRegisterP2PDirectory \
        Java_com_example_mediaproxy_MediaProxyCache_nativeVerifyP2PSource \
        Java_com_example_mediaproxy_MediaProxyCache_nativeRemoveP2PSource; do
        rg -q "[[:space:]]${symbol}$" <<<"$symbols" || {
          echo "Missing Android P2P JNI symbol $symbol in $artifact" >&2
          return 1
        }
      done
    fi
    if [[ "$LIBRQBIT_ENABLED" == "1" ]]; then
      for symbol in \
        Java_com_example_mediaproxy_MediaProxyCache_nativeAddAuthorizedTorrent \
        Java_com_example_mediaproxy_MediaProxyCache_nativeAddAuthorizedTorrentFile \
        Java_com_example_mediaproxy_MediaProxyCache_nativeRemoveTorrent \
        Java_com_example_mediaproxy_MediaProxyCache_nativeTorrentFilesJson \
        Java_com_example_mediaproxy_MediaProxyCache_nativeTorrentStatusJson \
        Java_com_example_mediaproxy_MediaProxyCache_nativeSetTorrentPaused \
        Java_com_example_mediaproxy_MediaProxyCache_nativeSetTorrentDownloadLimit; do
        rg -q "[[:space:]]${symbol}$" <<<"$symbols" || {
          echo "Missing Android librqbit JNI symbol $symbol in $artifact" >&2
          return 1
        }
      done
    fi
  fi
  if [[ "$platform" == "harmony" ]]; then
    rg -q "[[:space:]]napi_register_module_v1$" <<<"$symbols" || {
      echo "Missing HarmonyOS N-API registration symbol in $artifact" >&2
      return 1
    }
  fi
}

build_target() {
  local platform="$1" target="$2" crate_type="${3:-}"
  rustup target list --installed | rg -qx "$target" || {
    echo "Missing Rust target $target; install it with: rustup target add $target" >&2
    return 1
  }
  if [[ "$platform" == "ios" || "$platform" == "ios-sim" ]]; then
    export IPHONEOS_DEPLOYMENT_TARGET="$IOS_DEPLOYMENT_TARGET"
  fi
  if [[ -n "$crate_type" ]]; then
    cargo rustc --locked --"$PROFILE" --target "$target" --lib "${CARGO_FEATURE_ARGS[@]}" -- --crate-type="$crate_type"
  else
    cargo build --locked --"$PROFILE" --target "$target" --lib "${CARGO_FEATURE_ARGS[@]}"
  fi
  mkdir -p "$OUT_DIR/$platform/$target"
  local copied=0
  local artifacts=()
  case "$crate_type" in
    staticlib)
      artifacts=("target/$target/$PROFILE/libproxy_server.a")
      ;;
    cdylib)
      artifacts=(
        "target/$target/$PROFILE/libproxy_server.dylib"
        "target/$target/$PROFILE/libproxy_server.so"
        "target/$target/$PROFILE/proxy_server.dll"
      )
      ;;
    *)
      artifacts=(
        "target/$target/$PROFILE/libproxy_server.a"
        "target/$target/$PROFILE/libproxy_server.dylib"
        "target/$target/$PROFILE/libproxy_server.so"
        "target/$target/$PROFILE/proxy_server.dll"
        "target/$target/$PROFILE/proxy_server.dll.a"
        "target/$target/$PROFILE/proxy_server.lib"
      )
      ;;
  esac
  for artifact in "${artifacts[@]}"; do
    if [[ -f "$artifact" ]]; then
      verify_native_symbols "$artifact" "$platform"
      cp "$artifact" "$OUT_DIR/$platform/$target/"
      copied=1
    fi
  done
  if [[ "$copied" -eq 0 ]]; then
    echo "No native library artifact produced for target $target" >&2
    return 1
  fi
}

build_ios_xcframework() {
  local simulator_dir="$OUT_DIR/ios-sim/universal"
  local framework="$OUT_DIR/ios/MediaProxyCache.xcframework"
  mkdir -p "$simulator_dir"
  xcrun lipo -create \
    "$OUT_DIR/ios-sim/aarch64-apple-ios-sim/libproxy_server.a" \
    "$OUT_DIR/ios-sim/x86_64-apple-ios/libproxy_server.a" \
    -output "$simulator_dir/libproxy_server.a"
  rm -rf "$framework"
  xcodebuild -create-xcframework \
    -library "$OUT_DIR/ios/aarch64-apple-ios/libproxy_server.a" \
    -headers "$OUT_DIR/include" \
    -library "$simulator_dir/libproxy_server.a" \
    -headers "$OUT_DIR/include" \
    -output "$framework"
  local swift_flags=()
  if [[ "$P2P_ENABLED" == "1" ]]; then
    swift_flags=(-D MEDIA_PROXY_CACHE_ENABLE_P2P -Xcc -DMEDIA_PROXY_CACHE_ENABLE_P2P)
  fi
  if [[ "$LIBRQBIT_ENABLED" == "1" ]]; then
    swift_flags+=(-D MEDIA_PROXY_CACHE_ENABLE_LIBRQBIT -Xcc -DMEDIA_PROXY_CACHE_ENABLE_LIBRQBIT)
  fi
  swiftc -typecheck -I "$OUT_DIR/include" "${swift_flags[@]}" platform/ios/MediaProxyCache.swift
}

install_adapter_template() {
  local platform="$1" source="$2"
  mkdir -p "$OUT_DIR/$platform/adapter"
  cp "$source" "$OUT_DIR/$platform/adapter/"
}

case "${PLATFORM:-all}" in
  macos) build_target macos aarch64-apple-darwin; build_target macos x86_64-apple-darwin ;;
  windows) build_target windows x86_64-pc-windows-gnu ;;
  ios) build_target ios aarch64-apple-ios staticlib; build_target ios-sim aarch64-apple-ios-sim staticlib; build_target ios-sim x86_64-apple-ios staticlib; build_ios_xcframework; install_adapter_template ios platform/ios/MediaProxyCache.swift ;;
  android)
    build_target android aarch64-linux-android cdylib
    if [[ "$ANDROID_ARM64_ONLY" == "0" ]]; then
      build_target android armv7-linux-androideabi cdylib
      build_target android x86_64-linux-android cdylib
    fi
    install_adapter_template android platform/android/library/src/main/kotlin/com/example/mediaproxy/MediaProxyCache.kt
    ;;
  harmony) build_target harmony aarch64-unknown-linux-ohos cdylib; build_target harmony armv7-unknown-linux-ohos cdylib; install_adapter_template harmony platform/harmony/MediaProxyCache.d.ts ;;
  all) PLATFORM=macos "$0" "$OUT_DIR"; PLATFORM=windows "$0" "$OUT_DIR"; PLATFORM=ios "$0" "$OUT_DIR"; PLATFORM=android "$0" "$OUT_DIR"; PLATFORM=harmony "$0" "$OUT_DIR" ;;
  *) echo "Usage: PLATFORM={macos|windows|ios|android|harmony|all} $0 [output-dir]" >&2; exit 2 ;;
esac

echo "Mobile artifacts written to $OUT_DIR"
