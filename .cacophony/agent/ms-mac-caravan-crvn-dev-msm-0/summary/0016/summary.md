# Session summary — Make release helper portable without Python

## Goal

Fix the final Python dependency exposed by v0.0.1 release run `29629725231`.

## Bead(s)

- `bd-0ae65f` — Register Caravan release runners and finish v0.0.1 assets.

## Before state

- Resolver and tagged tests passed.
- The portable current contract started successfully, but the tagged `scripts/release.sh` still called Python and failed on the minimal NixOS runner.

## After state

- `scripts/release.sh` performs exact, unique version-marker replacement using Bash only for Cargo.toml, Cargo.lock, and flake.nix.
- The workflow extracts both the reviewed current contract and release helper through `GITHUB_SHA` before testing tagged inputs.
- No Python, second nixpkgs fetch, or ambient package is needed.

## Validation

- Release workflow YAML parses.
- `nix develop --command cargo build --bin cara` passes.
- Full `nix develop --command ./tests/release_contract.sh target/debug/cara` passes, including isolated mutating release-helper exercise and self-update envelope checks.
- Git diff check passes.

## Diff summary

- Code/content commit: `0a3e4d2`.
- Files: `.github/workflows/release.yml`, `scripts/release.sh`.

## Operator takeaway

Re-dispatch v0.0.1 after this commit lands; the release contract is now self-contained in the tagged Nix shell.
