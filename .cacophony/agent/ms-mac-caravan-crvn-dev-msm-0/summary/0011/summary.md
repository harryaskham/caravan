# Session summary — Route Caravan workflows to registered Nix runners

## Goal

Unblock CI, Pages, and v0.0.1 release workflows after the operator registered the real self-hosted architecture fleet.

## Bead(s)

- `bd-0ae65f` — Register Caravan release runners and finish v0.0.1 assets.

## Before state

- GitHub had three online runners, but CI/Pages/release required the nonexistent `azure-ephemeral` label; all new runs remained queued/cancelled by superseding pushes.
- Release Linux steps assumed apt/rustup cross toolchains despite both Linux runners being NixOS.

## After state

- CI and Pages select `[self-hosted, nix, x86_64-linux]`.
- Release resolve/test/x86 select the registered Nix Linux pool.
- aarch64-linux is pinned to `[self-hosted, nix, ms-dev-2]`; that host advertises `extra-platforms = aarch64-linux i686-linux`.
- aarch64-darwin selects `[self-hosted, nix, aarch64-darwin]`.
- Linux release binaries build from pinned flake packages rather than apt/rustup; Darwin retains the native pinned build script.

## Validation

- YAML parses for CI, Pages, and release workflows.
- No active workflow selector still requires `azure-ephemeral`.
- `nix flake check --no-write-lock-file` passes on aarch64-darwin.
- GitHub runner inventory: caravan-aurora, caravan-ms-dev-2, caravan-ms-mac all online/idle.
- Long-running aarch64 Linux build receipt `bj-0dc57a5b` is still running at summary time and remains the final architecture proof before closing the bead.

## Diff summary

- Code/content commit: `40dc62e`.
- Files: `.github/workflows/ci.yml`, `pages.yml`, `release.yml`.

## Operator takeaway

Landing this commit makes new workflow runs schedulable on the fleet that was actually registered. The existing v0.0.1 release is re-dispatched only after the workflow commit reaches main.
