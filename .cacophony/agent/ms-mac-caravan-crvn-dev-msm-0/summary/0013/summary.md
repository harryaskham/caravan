# Session summary — Remove ambient GNU assumptions from release workflow

## Goal

Fix the first v0.0.1 workflow-dispatch failure on the registered NixOS runner.

## Bead(s)

- `bd-0ae65f` — Register Caravan release runners and finish v0.0.1 assets.

## Before state

- Re-dispatched release run `29627492437` scheduled successfully but failed in `resolve release context` because the NixOS runner has no ambient `awk`.
- Package/smoke steps likewise assumed ambient tar/checksum tools.

## After state

- Cargo.toml version extraction uses Bash builtins and `BASH_REMATCH`; no awk.
- Packaging and archive extraction execute through the pinned Nix development shell.
- Tag/version validation remains strict and was exercised locally for v0.0.1.

## Validation

- Release workflow YAML parses.
- Local resolver obtains `v0.0.1 = 0.0.1` from the tagged Cargo.toml.
- Git diff check passes.
- Prior architecture proof `bj-0dc57a5b` confirms the pinned aarch64-linux package builds and executes on ms-dev-2.

## Diff summary

- Code/content commit: `5a647c7`.
- File: `.github/workflows/release.yml`.

## Operator takeaway

The next release dispatch no longer assumes GNU userland tools outside Nix. Continue observing each actual runner stage rather than adding packages globally.
