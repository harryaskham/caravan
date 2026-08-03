#!/usr/bin/env bash
# Bump Caravan's version, commit it, create vX.Y.Z, and optionally push both
# atomically to canonical GitHub. The checkout's origin may be a daemon mirror
# and is never publication authority. The verified tag triggers release.yml.

set -euo pipefail

bump=""
do_push=1
dry_run=0
for argument in "$@"; do
  case "$argument" in
    major | minor | patch) bump="$argument" ;;
    --no-push) do_push=0 ;;
    --dry-run) dry_run=1 ;;
    -h | --help)
      echo "usage: release.sh {major|minor|patch|X.Y.Z} [--no-push] [--dry-run]"
      exit 0
      ;;
    *)
      if [[ "$argument" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        bump="$argument"
      else
        echo "unknown argument: $argument" >&2
        exit 2
      fi
      ;;
  esac
done
[ -n "$bump" ] || {
  echo "usage: release.sh {major|minor|patch|X.Y.Z} [--no-push] [--dry-run]" >&2
  exit 2
}

root="$(git rev-parse --show-toplevel)"
cd "$root"
current="$(awk -F '"' '/^version = "/ { print $2; exit }' Cargo.toml)"
[[ "$current" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "could not read a semantic package version from Cargo.toml" >&2
  exit 1
}

if [[ "$bump" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  next="$bump"
else
  IFS=. read -r major minor patch <<<"$current"
  case "$bump" in
    major) next="$((major + 1)).0.0" ;;
    minor) next="$major.$((minor + 1)).0" ;;
    patch) next="$major.$minor.$((patch + 1))" ;;
  esac
fi
[ "$next" != "$current" ] || {
  echo "requested version is already current: $current" >&2
  exit 2
}

tag="v$next"
echo "==> $current -> $next ($tag)"

if [ "$dry_run" -eq 1 ]; then
  echo "[dry-run] would update Cargo.toml and Cargo.lock; commit; tag; push=$do_push"
  exit 0
fi

[ -z "$(git status --porcelain)" ] || {
  echo "working tree is dirty; commit or clean it before releasing" >&2
  exit 1
}

RELEASE_REPO="$(./scripts/release-remote.sh repo)"
RELEASE_REMOTE="$(./scripts/release-remote.sh url)"
branch="$(git branch --show-current)"
[ -n "$branch" ] || {
  echo "cannot release from detached HEAD" >&2
  exit 1
}
if [ "$do_push" -eq 1 ]; then
  [ "$branch" = "main" ] || {
    echo "refusing to publish from branch '$branch'; release.sh pushes canonical GitHub main only" >&2
    exit 1
  }
  CANONICAL_MAIN_REF="refs/remotes/cara-release/main"
  git fetch "$RELEASE_REMOTE" "+refs/heads/main:$CANONICAL_MAIN_REF" --quiet
  local_head="$(git rev-parse HEAD)"
  canonical_head="$(git rev-parse "$CANONICAL_MAIN_REF")"
  [ "$local_head" = "$canonical_head" ] || {
    echo "local main is not exact canonical GitHub main" >&2
    echo "  local:     $local_head" >&2
    echo "  canonical: $canonical_head" >&2
    exit 1
  }
fi

if git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null; then
  echo "tag already exists locally: $tag" >&2
  exit 1
fi
set +e
git ls-remote --exit-code --tags "$RELEASE_REMOTE" "refs/tags/$tag" >/dev/null 2>&1
remote_tag_status=$?
set -e
case "$remote_tag_status" in
  0)
    echo "tag already exists on canonical GitHub: $tag" >&2
    exit 1
    ;;
  2) ;;
  *)
    echo "could not verify whether $tag exists on canonical GitHub" >&2
    exit 1
    ;;
esac

replace_exact_once() {
  local file="$1" old="$2" new="$3" content suffix
  content="$(<"$file")"
  case "$content" in
    *"$old"*) ;;
    *)
      echo "expected version marker not found in $file" >&2
      return 1
      ;;
  esac
  suffix="${content#*"$old"}"
  case "$suffix" in
    *"$old"*)
      echo "version marker is ambiguous in $file" >&2
      return 1
      ;;
  esac
  printf '%s%s%s\n' "${content%%"$old"*}" "$new" "$suffix" >"$file"
}

replace_exact_once Cargo.toml \
  "version = \"$current\"" \
  "version = \"$next\""
replace_exact_once Cargo.lock \
  "name = \"caravan\"
version = \"$current\"" \
  "name = \"caravan\"
version = \"$next\""
# flake.nix is deliberately NOT patched here: both packages read the version
# from Cargo.toml, so there is no literal to bump. Patching it used to be a
# single-match replacement, which silently updated only one of the two package
# versions once they had drifted apart, and kept reporting success (bd-e48912).
for file in Cargo.toml Cargo.lock; do
  grep -q "$next" "$file" || {
    echo "version bump did not take in $file" >&2
    exit 1
  }
done

# Prove the flake actually reports the version we just committed, rather than
# trusting that it tracks Cargo.toml.
if command -v nix >/dev/null 2>&1; then
  for attr in caravan caravan-static; do
    # --no-write-lock-file: verifying must not mutate the tree it is verifying.
    # Without it, `nix eval` materialises flake.lock and leaves the release
    # commit dirty, which the release contract fixture caught.
    sys="$(nix eval --raw --impure --no-write-lock-file --expr 'builtins.currentSystem' 2>/dev/null || true)"
    [ -n "$sys" ] || continue
    reported="$(nix eval --raw --no-write-lock-file ".#packages.${sys}.${attr}.version" 2>/dev/null || true)"
    if [ -n "$reported" ] && [ "$reported" != "$next" ]; then
      echo "flake package $attr reports version $reported, expected $next" >&2
      exit 1
    fi
  done
fi

git add Cargo.toml Cargo.lock
git commit -m "release: $tag"
git tag -a "$tag" -m "$tag"
echo "==> committed and tagged $tag"

if [ "$do_push" -eq 1 ]; then
  release_commit="$(git rev-parse HEAD)"
  git push --atomic "$RELEASE_REMOTE" \
    "HEAD:refs/heads/main" \
    "refs/tags/$tag:refs/tags/$tag"

  remote_main="$(git ls-remote --heads "$RELEASE_REMOTE" refs/heads/main | awk 'NR == 1 { print $1 }')"
  remote_tag_commit="$(git ls-remote --tags "$RELEASE_REMOTE" "refs/tags/$tag^{}" | awk 'NR == 1 { print $1 }')"
  [ "$remote_main" = "$release_commit" ] || {
    echo "canonical GitHub main verification failed: expected $release_commit, found ${remote_main:-missing}" >&2
    exit 1
  }
  [ "$remote_tag_commit" = "$release_commit" ] || {
    echo "canonical GitHub tag verification failed: expected $release_commit, found ${remote_tag_commit:-missing}" >&2
    exit 1
  }
  echo "==> verified main and $tag at $release_commit on canonical GitHub $RELEASE_REPO"
  echo "==> release workflow will publish cara assets for verified tag $tag"
else
  echo "==> --no-push: inspect the commit; after landing it, delete the provisional local tag and use scripts/release-tag.sh"
fi
