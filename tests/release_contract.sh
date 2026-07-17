#!/usr/bin/env bash
# Offline contract smoke for release naming, checksums, versions, and updater status.

set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"
binary="${1:-}"
version="$(awk -F '"' '/^version = "/ { print $2; exit }' Cargo.toml)"
lock_version="$(awk '/^name = "caravan"$/ { getline; if ($1 == "version") { gsub(/"/, "", $3); print $3; exit } }' Cargo.lock)"
flake_version="$(awk -F '"' '/^[[:space:]]*version = "/ { print $2; exit }' flake.nix)"

[ "$version" = "$lock_version" ] || {
  echo "Cargo.toml ($version) and Cargo.lock ($lock_version) disagree" >&2
  exit 1
}
[ "$version" = "$flake_version" ] || {
  echo "Cargo.toml ($version) and flake.nix ($flake_version) disagree" >&2
  exit 1
}

release_plan="$(./scripts/release.sh patch --dry-run)"
grep -q "\[dry-run\] would update Cargo.toml, Cargo.lock, and flake.nix" <<<"$release_plan"

workspace="$(mktemp -d "${TMPDIR:-/tmp}/cara-release-contract.XXXXXX")"
trap 'rm -rf "$workspace"' EXIT

# Exercise the mutating helper only in an isolated throwaway repository.
next_version="$(python3 - "$version" <<'PY'
import sys
major, minor, patch = map(int, sys.argv[1].split("."))
print(f"{major}.{minor}.{patch + 1}")
PY
)"
git init --quiet --bare "$workspace/origin.git"
mkdir -p "$workspace/release-repo/scripts"
cp Cargo.toml Cargo.lock flake.nix "$workspace/release-repo/"
cp scripts/release.sh "$workspace/release-repo/scripts/"
git -C "$workspace/release-repo" init --quiet --initial-branch=main
git -C "$workspace/release-repo" config user.name "Caravan Release Test"
git -C "$workspace/release-repo" config user.email "caravan-release@example.invalid"
git -C "$workspace/release-repo" add .
git -C "$workspace/release-repo" commit --quiet --message initial
git -C "$workspace/release-repo" remote add origin "$workspace/origin.git"
git -C "$workspace/release-repo" push --quiet --set-upstream origin main
(
  cd "$workspace/release-repo"
  ./scripts/release.sh "$next_version" --no-push >/dev/null
  git rev-parse --verify "refs/tags/v$next_version^{commit}" >/dev/null
  [ -z "$(git status --porcelain)" ]
  grep -q "version = \"$next_version\"" Cargo.toml
  grep -q "version = \"$next_version\"" Cargo.lock
  grep -q "version = \"$next_version\";" flake.nix
)

mkdir -p "$workspace/bin" "$workspace/dist"
printf '#!/usr/bin/env sh\nprintf "cara fixture\\n"\n' > "$workspace/bin/cara"
chmod 0755 "$workspace/bin/cara"

target="x86_64-linux"
asset_root="cara-$version-$target"
./scripts/package-release.sh "$version" "$target" "$workspace/bin/cara" "$workspace/dist" >/dev/null
archive="$workspace/dist/$asset_root.tar.gz"
checksum="$workspace/dist/$asset_root.sha256"
[ -f "$archive" ]
[ -f "$checksum" ]
[ "$(tar -tzf "$archive")" = "$asset_root/
$asset_root/cara" ]
expected="$(awk '{print $1; exit}' "$checksum")"
if command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$archive" | awk '{print $1}')"
else
  actual="$(sha256sum "$archive" | awk '{print $1}')"
fi
[ "$expected" = "$actual" ] || {
  echo "release checksum mismatch" >&2
  exit 1
}

tar -xzf "$archive" -C "$workspace"
[ "$("$workspace/$asset_root/cara")" = "cara fixture" ]

for target in x86_64-linux aarch64-linux aarch64-darwin; do
  grep -q -- "- target: $target" .github/workflows/release.yml
done
grep -q 'softprops/action-gh-release@v2' .github/workflows/release.yml
grep -q 'scripts/package-release.sh' .github/workflows/release.yml

if [ -n "$binary" ]; then
  [ -x "$binary" ] || {
    echo "cara binary is not executable: $binary" >&2
    exit 1
  }
  mkdir -p "$workspace/home"
  status_json="$(HOME="$workspace/home" "$binary" --json self-update status)"
  python3 - "$version" "$workspace/home" "$status_json" <<'PY'
import json
import sys

version, home, raw = sys.argv[1:]
envelope = json.loads(raw)
assert envelope["status"] == "success", envelope
status = envelope["data"]
assert status["tool"] == "cara", status
assert status["current_version"] == version, status
assert status["install_dir"] == f"{home}/.local/bin", status
assert status["next_staged"] is False, status
PY
fi

echo "release contract ok: cara $version"
