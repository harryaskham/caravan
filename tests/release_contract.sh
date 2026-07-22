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
IFS=. read -r major minor patch <<<"$version"
next_version="$major.$minor.$((patch + 1))"
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
  mkdir -p "$workspace/home/.cargo/bin"
  cp "$binary" "$workspace/home/.cargo/bin/cara"
  chmod 0755 "$workspace/home/.cargo/bin/cara"
  status_json="$(HOME="$workspace/home" PATH="$workspace/home/.cargo/bin" "$workspace/home/.cargo/bin/cara" --json self-update status)"
  expected_install_dir="$(cd "$workspace/home/.cargo/bin" && pwd -P)"
  [[ "$status_json" == *'"status":"success"'* ]]
  [[ "$status_json" == *'"tool":"cara"'* ]]
  [[ "$status_json" == *"\"current_version\":\"$version\""* ]]
  [[ "$status_json" == *"\"install_dir\":\"$expected_install_dir\""* ]]
  [[ "$status_json" == *'"next_staged":false'* ]]

  # Reader/config compatibility is an offline release gate. This fixture is the
  # sync policy that broke Cara 0.0.6; no GitHub/provider command is involved.
  config_json="$(
    HOME="$workspace/home" PATH="$workspace/home/.cargo/bin" \
      "$workspace/home/.cargo/bin/cara" --json \
      --config tests/fixtures/config-v0.0.7.yaml config check
  )"
  [[ "$config_json" == *'"status":"success"'* ]]
  [[ "$config_json" == *'"compatible":true'* ]]
  [[ "$config_json" == *'"min_cara_version":"0.0.7"'* ]]
  [[ "$config_json" == *'"provider_mutated":false'* ]]
fi

echo "release contract ok: cara $version"
