#!/usr/bin/env bash
# Resolve Caravan's canonical GitHub repository and push URL without trusting
# the checkout's `origin`, which is a daemon-local mirror in managed worktrees.

set -euo pipefail

mode="${1:-url}"
root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$root"

manifest_repository="$(awk '
  /^\[package\]$/ { in_package=1; next }
  /^\[/ { in_package=0 }
  in_package && /^repository = "/ {
    sub(/^repository = "/, ""); sub(/"$/, ""); print; exit
  }
' Cargo.toml)"
configured_repo="${CARA_RELEASE_REPO:-$manifest_repository}"

github_slug() {
  local value="$1" slug
  case "$value" in
    https://github.com/*)
      slug="${value#https://github.com/}"
      ;;
    ssh://git@github.com/*)
      slug="${value#ssh://git@github.com/}"
      ;;
    ssh://github.com/*)
      slug="${value#ssh://github.com/}"
      ;;
    git@github.com:*)
      slug="${value#git@github.com:}"
      ;;
    */*)
      slug="$value"
      ;;
    *)
      return 1
      ;;
  esac
  slug="${slug%.git}"
  slug="${slug%/}"
  [[ "$slug" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || return 1
  printf '%s\n' "$slug"
}

repo="$(github_slug "$configured_repo")" || {
  echo "error: Caravan release repository must identify one github.com owner/repo (got: $configured_repo)" >&2
  exit 2
}

remote="${CARA_RELEASE_REMOTE:-ssh://git@github.com/${repo}.git}"
if [[ "${CARA_RELEASE_REMOTE_ALLOW_LOCAL_FIXTURE:-0}" == "1" ]]; then
  # Offline contract tests use two local bare repositories to prove that a
  # daemon-mirror origin is not mistaken for the canonical publication remote.
  # Production release environments must never set this fixture-only switch.
  [[ "$remote" = /* || "$remote" == file://* ]] || {
    remote_repo="$(github_slug "$remote")" || {
      echo "error: invalid CARA_RELEASE_REMOTE: $remote" >&2
      exit 2
    }
    [[ "$remote_repo" == "$repo" ]] || {
      echo "error: release remote $remote_repo does not match configured repository $repo" >&2
      exit 2
    }
  }
else
  remote_repo="$(github_slug "$remote")" || {
    echo "error: release remote must be an explicit github.com URL, never a local path: $remote" >&2
    exit 2
  }
  [[ "$remote_repo" == "$repo" ]] || {
    echo "error: release remote $remote_repo does not match configured repository $repo" >&2
    exit 2
  }
fi

case "$mode" in
  repo) printf '%s\n' "$repo" ;;
  url) printf '%s\n' "$remote" ;;
  *)
    echo "usage: release-remote.sh {repo|url}" >&2
    exit 2
    ;;
esac
