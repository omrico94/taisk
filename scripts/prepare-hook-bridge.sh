#!/usr/bin/env bash
# Builds hook-bridge and places it where Tauri's `externalBin` expects it
# (src-tauri/binaries/hook-bridge-<target-triple>). Release builds only.
#   scripts/prepare-hook-bridge.sh                      # host arch
#   scripts/prepare-hook-bridge.sh universal-apple-darwin
set -euo pipefail
cd "$(dirname "$0")/../src-tauri"
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
mkdir -p binaries

build() { cargo build --release -p hook-bridge --target "$1" >&2; echo "target/$1/release/hook-bridge"; }

if [ "$target" = "universal-apple-darwin" ]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >&2
  a=$(build aarch64-apple-darwin)
  x=$(build x86_64-apple-darwin)
  # Tauri builds each arch separately (needs per-arch names) and then merges
  # them (needs the universal name), so provide all three.
  cp "$a" binaries/hook-bridge-aarch64-apple-darwin
  cp "$x" binaries/hook-bridge-x86_64-apple-darwin
  lipo -create "$a" "$x" -output "binaries/hook-bridge-$target"
else
  cp "$(build "$target")" "binaries/hook-bridge-$target"
fi
ls binaries | sed "s|^|wrote src-tauri/binaries/|"
