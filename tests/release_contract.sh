#!/usr/bin/env bash
# Offline contract smoke for release naming, checksums, versions, and updater status.

set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"
binary="${1:-}"
version="$(awk -F '"' '/^version = "/ { print $2; exit }' Cargo.toml)"
lock_version="$(awk '/^name = "caravan"$/ { getline; if ($1 == "version") { gsub(/"/, "", $3); print $3; exit } }' Cargo.lock)"
# flake.nix carries no version literal: both packages derive it from Cargo.toml
# so the two can never drift (bd-e48912). Prove the derivation still resolves,
# rather than grepping for a literal whose absence is the point.
flake_version="$(nix eval --raw --no-write-lock-file ".#packages.$(nix eval --raw --impure --no-write-lock-file --expr 'builtins.currentSystem').caravan.version" 2>/dev/null || echo "")"

[ "$version" = "$lock_version" ] || {
  echo "Cargo.toml ($version) and Cargo.lock ($lock_version) disagree" >&2
  exit 1
}
# An empty result means nix could not evaluate here, not that the flake is
# wrong; only a resolved-and-different version is a contract violation.
[ -z "$flake_version" ] || [ "$version" = "$flake_version" ] || {
  echo "Cargo.toml ($version) and the flake ($flake_version) disagree" >&2
  exit 1
}

release_plan="$(./scripts/release.sh patch --dry-run)"
grep -q "\[dry-run\] would update Cargo.toml and Cargo.lock" <<<"$release_plan"

workspace="$(mktemp -d "${TMPDIR:-/tmp}/cara-release-contract.XXXXXX")"
trap 'rm -rf "$workspace"' EXIT

# Exercise the mutating helper only in an isolated throwaway repository.
IFS=. read -r major minor patch <<<"$version"
next_version="$major.$minor.$((patch + 1))"
git init --quiet --bare "$workspace/mirror.git"
git init --quiet --bare "$workspace/canonical.git"
mkdir -p "$workspace/release-repo/scripts"
cp Cargo.toml Cargo.lock flake.nix "$workspace/release-repo/"
cp scripts/release.sh scripts/release-remote.sh scripts/release-tag.sh "$workspace/release-repo/scripts/"
git -C "$workspace/release-repo" init --quiet --initial-branch=main
git -C "$workspace/release-repo" config user.name "Caravan Release Test"
git -C "$workspace/release-repo" config user.email "caravan-release@example.invalid"
git -C "$workspace/release-repo" add .
git -C "$workspace/release-repo" commit --quiet --message initial
git -C "$workspace/release-repo" remote add origin "$workspace/mirror.git"
git -C "$workspace/release-repo" push --quiet --set-upstream origin main
git -C "$workspace/release-repo" push --quiet "$workspace/canonical.git" main
(
  cd "$workspace/release-repo"
  if CARA_RELEASE_REMOTE="$workspace/canonical.git" ./scripts/release-remote.sh url >/dev/null 2>&1; then
    echo "release remote resolver accepted a local path without the explicit fixture guard" >&2
    exit 1
  fi
  CARA_RELEASE_REPO=harryaskham/caravan \
    CARA_RELEASE_REMOTE="$workspace/canonical.git" \
    CARA_RELEASE_REMOTE_ALLOW_LOCAL_FIXTURE=1 \
    ./scripts/release.sh "$next_version" --no-push >/dev/null
  git rev-parse --verify "refs/tags/v$next_version^{commit}" >/dev/null
  [ -z "$(git status --porcelain)" ]
  grep -q "version = \"$next_version\"" Cargo.toml
  grep -q "version = \"$next_version\"" Cargo.lock
  # flake.nix must remain untouched by the bump: it has no literal to update.
  ! grep -q 'version = "[0-9]' flake.nix
)

# A managed checkout's origin can be a daemon mirror. Publication must update
# only the separately configured canonical GitHub fixture and must verify both
# remote main and the peeled annotated tag before claiming success.
git init --quiet --bare "$workspace/push-mirror.git"
git init --quiet --bare "$workspace/push-canonical.git"
cp -R "$workspace/release-repo" "$workspace/push-repo"
(
  cd "$workspace/push-repo"
  git tag -d "v$next_version" >/dev/null
  git reset --hard HEAD^ >/dev/null
  git remote set-url origin "$workspace/push-mirror.git"
  git push --quiet --set-upstream origin main
  git push --quiet "$workspace/push-canonical.git" main
  initial="$(git rev-parse HEAD)"
  output="$(
    CARA_RELEASE_REPO=harryaskham/caravan \
      CARA_RELEASE_REMOTE="$workspace/push-canonical.git" \
      CARA_RELEASE_REMOTE_ALLOW_LOCAL_FIXTURE=1 \
      ./scripts/release.sh "$next_version"
  )"
  release_commit="$(git rev-parse HEAD)"
  [ "$(git --git-dir="$workspace/push-mirror.git" rev-parse refs/heads/main)" = "$initial" ]
  if git --git-dir="$workspace/push-mirror.git" rev-parse --verify "refs/tags/v$next_version" >/dev/null 2>&1; then
    echo "release.sh leaked the release tag into daemon-mirror origin" >&2
    exit 1
  fi
  [ "$(git --git-dir="$workspace/push-canonical.git" rev-parse refs/heads/main)" = "$release_commit" ]
  [ "$(git --git-dir="$workspace/push-canonical.git" rev-parse "refs/tags/v$next_version^{}")" = "$release_commit" ]
  grep -q "verified main and v$next_version" <<<"$output"
)

# The reviewed post-landing tag-only path obeys the same remote boundary.
git init --quiet --bare "$workspace/tag-mirror.git"
git init --quiet --bare "$workspace/tag-canonical.git"
cp -R "$workspace/release-repo" "$workspace/tag-repo"
(
  cd "$workspace/tag-repo"
  git tag -d "v$next_version" >/dev/null
  git remote set-url origin "$workspace/tag-mirror.git"
  git push --quiet --set-upstream origin main
  git push --quiet "$workspace/tag-canonical.git" main
  commit="$(git rev-parse HEAD)"
  output="$(
    CARA_RELEASE_REPO=harryaskham/caravan \
      CARA_RELEASE_REMOTE="$workspace/tag-canonical.git" \
      CARA_RELEASE_REMOTE_ALLOW_LOCAL_FIXTURE=1 \
      ./scripts/release-tag.sh "v$next_version"
  )"
  if git --git-dir="$workspace/tag-mirror.git" rev-parse --verify "refs/tags/v$next_version" >/dev/null 2>&1; then
    echo "release-tag leaked the release tag into daemon-mirror origin" >&2
    exit 1
  fi
  [ "$(git --git-dir="$workspace/tag-canonical.git" rev-parse "refs/tags/v$next_version^{}")" = "$commit" ]
  grep -q "verified v$next_version" <<<"$output"
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

# The CI matrix carries exactly the targets a runner can actually pick up.
# A leg no runner can accept, or one pinned to a permanently busy host, sits
# queued and leaves the whole release non-terminal: that is bd-8b6d28 for darwin
# and bd-893ff0 for aarch64-linux. Darwin is back because caravan-ms-mac now
# advertises the label; aarch64-linux is out until ms-dev-2 has spare capacity
# or a second aarch64-capable runner exists.
for target in x86_64-linux aarch64-darwin; do
  grep -q -- "\"target\":\"$target\"" .github/workflows/release.yml
done
# shellcheck disable=SC2016 # workflow expressions are matched literally.
grep -Fq 'matrix: ${{ fromJSON(needs.resolve.outputs.matrix) }}' .github/workflows/release.yml
# shellcheck disable=SC2016 # runner environment reference is matched literally.
grep -Fq 'git_config="$RUNNER_TEMP/' .github/workflows/release.yml
grep -Fq 'unsupported release target' .github/workflows/release.yml
if grep -Fq 'git config --global --unset-all' .github/workflows/release.yml; then
  echo "release workflow must not mutate persistent-runner global Git config" >&2
  exit 1
fi
if grep -q -- '"target":"aarch64-linux"' .github/workflows/release.yml; then
  echo "release.yml schedules aarch64-linux; it is starved on ms-dev-2, so keep it backfilled until another aarch64 runner exists" >&2
  exit 1
fi
# The backfill path is the contract for that target, so it must stay available.
grep -q 'release-backfill-target' justfile
grep -q 'softprops/action-gh-release@v2' .github/workflows/release.yml
grep -q 'scripts/package-release.sh' .github/workflows/release.yml

# Downstream pin rows are derived from published assets, never hand-typed. This
# fixture is fully offline: it packages throwaway per-target binaries and checks
# the exact TSV contract downstream consumers read.
pins_dist="$workspace/pins-dist"
mkdir -p "$pins_dist"
for target in x86_64-linux aarch64-linux aarch64-darwin; do
  printf '#!/usr/bin/env sh\nprintf "cara %s\\n"\n' "$target" > "$workspace/bin/cara-$target"
  chmod 0755 "$workspace/bin/cara-$target"
  ./scripts/package-release.sh "$version" "$target" "$workspace/bin/cara-$target" "$pins_dist" >/dev/null
done

pin_rows="$(./scripts/release-pin-rows.sh "$version" --dist "$pins_dist" --context 'contract fixture')"
[ "$(grep -c "^cara $version\b" <<<"$pin_rows")" = 3 ]
grep -q "^# v$version assets: https://github.com/" <<<"$pin_rows"
grep -q '^# archive sha256: aarch64-darwin=.* aarch64-linux=.* x86_64-linux=' <<<"$pin_rows"
for target in x86_64-linux aarch64-linux aarch64-darwin; do
  pin_row="$(grep -F "	$target	" <<<"$pin_rows")"
  [ "$(awk -F '\t' '{print NF}' <<<"$pin_row")" = 5 ]
  [ "$(awk -F '\t' '{print $5}' <<<"$pin_row")" = 'contract fixture' ]
  pinned_archive="$(awk -F '\t' '{print $4}' <<<"$pin_row")"
  [ "$pinned_archive" = "$(awk '{print $1; exit}' "$pins_dist/cara-$version-$target.sha256")" ]
  mkdir -p "$workspace/pins-extract-$target"
  tar -xzf "$pins_dist/cara-$version-$target.tar.gz" -C "$workspace/pins-extract-$target"
  if command -v shasum >/dev/null 2>&1; then
    pinned_expected="$(shasum -a 256 "$workspace/pins-extract-$target/cara-$version-$target/cara" | awk '{print $1}')"
  else
    pinned_expected="$(sha256sum "$workspace/pins-extract-$target/cara-$version-$target/cara" | awk '{print $1}')"
  fi
  [ "$(awk -F '\t' '{print $2}' <<<"$pin_row")" = "$pinned_expected" ]
done

# A partially published release must never look pinnable.
rm -f "$pins_dist/cara-$version-aarch64-darwin.tar.gz" "$pins_dist/cara-$version-aarch64-darwin.sha256"
if ./scripts/release-pin-rows.sh "$version" --dist "$pins_dist" >/dev/null 2>&1; then
  echo "release-pin-rows.sh accepted a partially published release" >&2
  exit 1
fi
partial_rows="$(./scripts/release-pin-rows.sh "$version" --dist "$pins_dist" --allow-partial 2>/dev/null)"
grep -q "^# missing aarch64-darwin: " <<<"$partial_rows"
[ "$(grep -c "^cara $version\b" <<<"$partial_rows")" = 2 ]

# A tampered published checksum is a hard failure, never a pinned row.
bad_dist="$workspace/pins-tampered"
mkdir -p "$bad_dist"
cp "$pins_dist/cara-$version-x86_64-linux.tar.gz" "$bad_dist/"
printf '%s  cara-%s-x86_64-linux.tar.gz\n' "$(printf '0%.0s' $(seq 64))" "$version" \
  > "$bad_dist/cara-$version-x86_64-linux.sha256"
if ./scripts/release-pin-rows.sh "$version" --dist "$bad_dist" --allow-partial >/dev/null 2>&1; then
  echo "release-pin-rows.sh accepted a tampered published checksum" >&2
  exit 1
fi

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

  # Canonical downstream deployments pin no Cara version and rely on the neutral
  # sentinel. Rejecting it would fail closed on every queue tick, so it is a
  # release gate, not an incidental case.
  sentinel_json="$(
    HOME="$workspace/home" PATH="$workspace/home/.cargo/bin" \
      "$workspace/home/.cargo/bin/cara" --json \
      --config tests/fixtures/config-rolling-sentinel.yaml config check
  )"
  [[ "$sentinel_json" == *'"status":"success"'* ]]
  [[ "$sentinel_json" == *'"compatible":true'* ]]
  [[ "$sentinel_json" == *'"min_cara_version":"0.0.0"'* ]]
  [[ "$sentinel_json" == *'"provider_mutated":false'* ]]
  [[ "$sentinel_json" == *"\"reader_version\":\"$version\""* ]]
fi

echo "release contract ok: cara $version"
