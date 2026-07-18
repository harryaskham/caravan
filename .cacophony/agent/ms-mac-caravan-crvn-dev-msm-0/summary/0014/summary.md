# Session summary — Supply pinned Python to legacy release contract

## Goal

Fix v0.0.1 release run `29628391848` after runner scheduling and resolver fixes succeeded.

## Bead(s)

- `bd-0ae65f` — Register Caravan release runners and finish v0.0.1 assets.

## Before state

- Resolve job passed.
- Release contract failed because the v0.0.1-tagged development shell predates Python inclusion, while `tests/release_contract.sh` requires Python for semantic version and updater-envelope checks.
- The NixOS runner correctly had no ambient Python.

## After state

- Workflow builds Python from the exact pinned FlakeHub nixpkgs URL already used by the tagged flake.
- The pinned Python bin path is injected only for the release contract inside the tagged Nix development shell.
- No ambient package install or unpinned registry input is used.

## Validation

- Release YAML parses.
- The exact URL resolves to `/nix/store/fcdr...-python3-3.13.12`; `bin/python3` is executable.
- Git diff check passes.

## Diff summary

- Code/content commit: `f04fea8`.
- File: `.github/workflows/release.yml`.

## Operator takeaway

Re-dispatch v0.0.1 after this workflow lands; observe the next actual stage rather than modifying runner hosts.
