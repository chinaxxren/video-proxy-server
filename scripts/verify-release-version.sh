#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/Cargo.toml" | head -n 1)"

if [[ -z "$VERSION" ]]; then
  echo "Unable to read package version from Cargo.toml" >&2
  exit 1
fi

TAG="${1:-${GITHUB_REF_NAME:-}}"
if [[ -n "$TAG" && "$TAG" == v* && "${TAG#v}" != "$VERSION" ]]; then
  echo "Release tag $TAG does not match Cargo version $VERSION" >&2
  exit 1
fi

echo "Release version verified: $VERSION${TAG:+ ($TAG)}"
