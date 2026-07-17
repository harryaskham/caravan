#!/usr/bin/env bash
# Package one cara binary using updatable-cli's TendrilStyle asset contract.

set -euo pipefail

if [ "$#" -ne 4 ]; then
  echo "usage: package-release.sh VERSION TARGET BINARY DIST_DIR" >&2
  exit 2
fi

version="$1"
target="$2"
binary="$3"
dist="$4"

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "version must be X.Y.Z, got: $version" >&2
  exit 2
}
case "$target" in
  x86_64-linux | aarch64-linux | aarch64-darwin) ;;
  *)
    echo "unsupported cara release target: $target" >&2
    exit 2
    ;;
esac
[ -f "$binary" ] || {
  echo "release binary does not exist: $binary" >&2
  exit 1
}

mkdir -p "$dist"
dist="$(cd "$dist" && pwd)"
stage="$(mktemp -d "${TMPDIR:-/tmp}/cara-release.XXXXXX")"
trap 'rm -rf "$stage"' EXIT

root="cara-$version-$target"
archive="$root.tar.gz"
checksum="$root.sha256"
mkdir "$stage/$root"
cp "$binary" "$stage/$root/cara"
chmod 0755 "$stage/$root/cara"

# Suppress AppleDouble metadata when packaging on macOS.
COPYFILE_DISABLE=1 tar -czf "$dist/$archive" -C "$stage" "$root"
if command -v shasum >/dev/null 2>&1; then
  digest="$(shasum -a 256 "$dist/$archive" | awk '{print $1}')"
elif command -v sha256sum >/dev/null 2>&1; then
  digest="$(sha256sum "$dist/$archive" | awk '{print $1}')"
else
  echo "neither shasum nor sha256sum is available" >&2
  exit 1
fi
printf '%s  %s\n' "$digest" "$archive" > "$dist/$checksum"

printf '%s\n' "$dist/$archive" "$dist/$checksum"
