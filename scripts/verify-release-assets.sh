#!/usr/bin/env bash
set -euo pipefail

expected_assets() {
  local version="${1:-v0.0.0}"
  cat <<'EOF'
media-proxy-cache-linux-x86_64.tar.gz
media-proxy-cache-macos-arm64.tar.gz
media-proxy-cache-macos-x86_64.tar.gz
media-proxy-cache-windows-x86_64.zip
EOF
  printf 'media-proxy-cache-%s-ios-librqbit.tar.gz\n' "$version"
  printf 'media-proxy-cache-%s-android-librqbit.tar.gz\n' "$version"
}

if [[ "${1:-}" == "--self-test" ]]; then
  version="v0.0.0"
  actual="$(expected_assets "$version")"
else
  tag="${1:?usage: verify-release-assets.sh <vX.Y.Z> | --self-test}"
  version="$tag"
  actual="$(curl --fail --silent --show-error \
    "https://api.github.com/repos/chinaxxren/video-proxy-server/releases/tags/${tag}" \
    | jq -r '.assets[].name')"
fi

while IFS= read -r asset; do
  [[ -n "$asset" ]] || continue
  grep -Fqx "$asset" <<<"$actual" || {
    echo "Missing release asset: $asset" >&2
    exit 1
  }
done < <(expected_assets "$version")

echo "Required release assets are present"
