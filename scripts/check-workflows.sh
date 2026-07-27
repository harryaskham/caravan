#!/usr/bin/env bash
# Validate GitHub Actions and repository shell scripts with the pinned Nix tools.

set -euo pipefail

root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$root"

command -v actionlint >/dev/null || {
  echo "actionlint is required; run through 'nix develop'" >&2
  exit 1
}
command -v shellcheck >/dev/null || {
  echo "shellcheck is required; run through 'nix develop'" >&2
  exit 1
}

actionlint -config-file .github/actionlint.yaml .github/workflows/*.yml
shellcheck scripts/*.sh tests/*.sh examples/hooks/*.sh
