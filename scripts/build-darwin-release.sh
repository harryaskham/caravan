#!/usr/bin/env bash
# Build a portable aarch64-darwin cara binary inside the pinned Nix dev shell.

set -euo pipefail

cargo build --release --bin cara
binary="${CARGO_TARGET_DIR:-target}/release/cara"

for reference in $(otool -L "$binary" | grep -oE '/nix/store/[^ ]*libiconv[^ ]*\.dylib' || true); do
  install_name_tool -change "$reference" /usr/lib/libiconv.2.dylib "$binary"
done
remaining="$(otool -L "$binary" | grep /nix/store || true)"
if [ -n "$remaining" ]; then
  echo "non-portable Nix store linkage remains:" >&2
  echo "$remaining" >&2
  exit 1
fi

otool -L "$binary"
