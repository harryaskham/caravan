#!/usr/bin/env bash
# Create and publish one immutable Caravan tag at an already-landed canonical
# GitHub main commit. The checkout's origin is deliberately ignored.

set -euo pipefail

TAG="${1:-}"
REF="${2:-canonical/main}"

[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "error: tag must be vX.Y.Z (got: $TAG)" >&2
  exit 2
}

root="$(git rev-parse --show-toplevel)"
cd "$root"
[ -z "$(git status --porcelain)" ] || {
  echo "error: working tree is dirty; clean it before tagging" >&2
  exit 1
}

RELEASE_REPO="$(./scripts/release-remote.sh repo)"
RELEASE_REMOTE="$(./scripts/release-remote.sh url)"
CANONICAL_MAIN_REF="refs/remotes/cara-release/main"
git fetch "$RELEASE_REMOTE" "+refs/heads/main:$CANONICAL_MAIN_REF" --tags --quiet

case "$REF" in
  canonical/main|origin/main) REF="$CANONICAL_MAIN_REF" ;;
esac
COMMIT="$(git rev-parse --verify "$REF^{commit}")"
git merge-base --is-ancestor "$COMMIT" "$CANONICAL_MAIN_REF" || {
  echo "error: $COMMIT is not contained in canonical GitHub main; tags are cut from landed main only" >&2
  exit 1
}

VERSION="${TAG#v}"
CARGO_VERSION="$(git show "$COMMIT:Cargo.toml" | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)"
LOCK_VERSION="$(git show "$COMMIT:Cargo.lock" | awk '/^name = "caravan"$/ { getline; sub(/^version = "/, ""); sub(/"$/, ""); print; exit }')"
FLAKE_SOURCE="$(git show "$COMMIT:flake.nix")"
if printf '%s\n' "$FLAKE_SOURCE" | grep -Fq 'caravanVersion = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;'; then
  FLAKE_VERSION="$CARGO_VERSION"
else
  FLAKE_VERSION="$(printf '%s\n' "$FLAKE_SOURCE" | sed -n 's/^ *version = "\(.*\)";$/\1/p' | head -1)"
fi
for pair in "Cargo.toml:$CARGO_VERSION" "Cargo.lock:$LOCK_VERSION" "flake.nix:$FLAKE_VERSION"; do
  file="${pair%%:*}"
  found="${pair#*:}"
  [ "$found" = "$VERSION" ] || {
    echo "error: $file at $COMMIT declares version '$found', not '$VERSION'" >&2
    exit 1
  }
done

if git rev-parse --quiet --verify "refs/tags/$TAG" >/dev/null; then
  echo "error: tag already exists locally: $TAG" >&2
  exit 1
fi
set +e
git ls-remote --exit-code --tags "$RELEASE_REMOTE" "refs/tags/$TAG" >/dev/null 2>&1
remote_tag_status=$?
set -e
case "$remote_tag_status" in
  0) echo "error: tag already exists on canonical GitHub: $TAG" >&2; exit 1 ;;
  2) ;;
  *) echo "error: could not verify whether $TAG exists on canonical GitHub" >&2; exit 1 ;;
esac

echo "==> tagging $TAG at $COMMIT for $RELEASE_REPO (version $VERSION verified)"
git tag -a "$TAG" -m "$TAG" "$COMMIT"
git push "$RELEASE_REMOTE" "refs/tags/$TAG:refs/tags/$TAG"

remote_tag_commit="$(git ls-remote --tags "$RELEASE_REMOTE" "refs/tags/$TAG^{}" | awk 'NR == 1 { print $1 }')"
[ "$remote_tag_commit" = "$COMMIT" ] || {
  echo "error: canonical GitHub tag verification failed: expected $COMMIT, found ${remote_tag_commit:-missing}" >&2
  exit 1
}
echo "==> verified $TAG at $COMMIT on canonical GitHub $RELEASE_REPO; release.yml will publish cara assets"
echo "    next: just release-pin-rows $TAG \"reviewed release binary\""
