#!/usr/bin/env bash
# Bump Caravan's version, commit it, create vX.Y.Z, and optionally push both.
# The tag triggers .github/workflows/release.yml.

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
  echo "[dry-run] would update Cargo.toml, Cargo.lock, and flake.nix; commit; tag; push=$do_push"
  exit 0
fi

[ -z "$(git status --porcelain)" ] || {
  echo "working tree is dirty; commit or clean it before releasing" >&2
  exit 1
}
if git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null; then
  echo "tag already exists locally: $tag" >&2
  exit 1
fi
set +e
git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1
remote_tag_status=$?
set -e
case "$remote_tag_status" in
  0)
    echo "tag already exists on origin: $tag" >&2
    exit 1
    ;;
  2) ;;
  *)
    echo "could not verify whether $tag exists on origin" >&2
    exit 1
    ;;
esac

python3 - "$current" "$next" <<'PY'
from pathlib import Path
import re
import sys

old, new = sys.argv[1:]

def replace(path: str, pattern: str, replacement: str) -> None:
    file = Path(path)
    text = file.read_text()
    updated, count = re.subn(pattern, replacement, text, count=1, flags=re.MULTILINE)
    if count != 1:
        raise SystemExit(f"expected exactly one version match in {path}, found {count}")
    file.write_text(updated)

replace(
    "Cargo.toml",
    rf'(^\[package\][\s\S]*?^version = "){re.escape(old)}("$)',
    rf'\g<1>{new}\2',
)
replace(
    "Cargo.lock",
    rf'(\[\[package\]\]\nname = "caravan"\nversion = "){re.escape(old)}("\n)',
    rf'\g<1>{new}\2',
)
replace(
    "flake.nix",
    rf'(version = "){re.escape(old)}(";)',
    rf'\g<1>{new}\2',
)
PY

for file in Cargo.toml Cargo.lock flake.nix; do
  grep -q "$next" "$file" || {
    echo "version bump did not take in $file" >&2
    exit 1
  }
done

git add Cargo.toml Cargo.lock flake.nix
git commit -m "release: $tag"
git tag -a "$tag" -m "$tag"
echo "==> committed and tagged $tag"

if [ "$do_push" -eq 1 ]; then
  branch="$(git branch --show-current)"
  [ -n "$branch" ] || {
    echo "cannot push a release from detached HEAD" >&2
    exit 1
  }
  git push --atomic origin "$branch" "refs/tags/$tag"
  echo "==> pushed $branch and $tag; release workflow will publish cara assets"
else
  echo "==> --no-push: inspect the commit, then push branch and $tag atomically"
fi
