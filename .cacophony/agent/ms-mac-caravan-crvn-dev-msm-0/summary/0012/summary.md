# Session summary — Make registered Nix CI runner self-contained

## Goal

Repair the first scheduled CI run after runner-selector routing landed.

## Bead(s)

- `bd-0ae65f` — Register Caravan release runners and finish v0.0.1 assets.

## Before state

- Runner selectors landed at `151b25b` and workflows began scheduling.
- CI run `29626832528` failed before compilation because `dtolnay/rust-toolchain` tried to bootstrap rustup through `curl`; the persistent NixOS runner intentionally has no ambient curl.

## After state

- CI uses the pinned Nix development shell for fmt, strict clippy, all tests, help, and MCP registry smoke.
- No ambient curl, rustup, apt, or unpinned toolchain is required.
- Existing registered-runner selectors remain unchanged.

## Validation

- CI workflow YAML parses.
- `nix flake check --no-write-lock-file` passes on aarch64-darwin.
- Git diff check passes.

## Diff summary

- Code/content commit: `8a25990`.
- File: `.github/workflows/ci.yml`.

## Operator takeaway

The next push should exercise the actual pinned toolchain on the registered NixOS runner rather than failing during external toolchain bootstrap.
