#!/usr/bin/env bash
# Installs the pinned k6 release into a directory (default ~/.local/bin), after checking its
# SHA-256 against the value recorded here. CI uses it; locally it's optional.
#
#   scripts/install-k6.sh [directory]
set -euo pipefail

readonly VERSION="v2.3.0"
dest="${1:-$HOME/.local/bin}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  asset="k6-$VERSION-linux-amd64.tar.gz"; sha="39c3117b6af817592dcd0ce4242105c0a7af10948c2a425306f0be8f7a8a8ab1" ;;
  Linux-aarch64) asset="k6-$VERSION-linux-arm64.tar.gz"; sha="5ca3433e8201da72a284aaa241a1bb5fb47f4abb4e384d39410ddd8062f49b90" ;;
  Darwin-arm64)  asset="k6-$VERSION-macos-arm64.zip";    sha="b2417a3038edc5fe81dc178a889237724b595c5c9cfed875822008e46e862c7d" ;;
  *) echo "error: no pinned k6 build for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -sSfL -o "$work/$asset" "https://github.com/grafana/k6/releases/download/$VERSION/$asset"
echo "$sha  $work/$asset" | shasum -a 256 -c - >/dev/null
case "$asset" in
  *.zip) unzip -q "$work/$asset" -d "$work" ;;
  *) tar -xzf "$work/$asset" -C "$work" ;;
esac
mkdir -p "$dest"
install -m 0755 "$(find "$work" -type f -name k6 | head -n 1)" "$dest/k6"
"$dest/k6" version
