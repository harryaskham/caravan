set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

run-web-dev *repos:
  #!/usr/bin/env bash
  set -euo pipefail
  [[ -d ~/caravan-test ]] || git worktree add -b caravan-test ~/caravan-test
  cd ~/caravan-test
  git fetch origin main
  git reset --hard origin/main
  nix develop --command cargo install --path .
  flags=()
  for repo in {{repos}}; do
    flags+=("--repo" "$repo")
  done
  bash -c "~/.cargo/bin/cara web ${flags[@]}"
