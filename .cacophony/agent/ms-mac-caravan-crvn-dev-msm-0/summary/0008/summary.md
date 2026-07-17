# Session summary — Hermetic macOS stale-lock recovery

## Goal

Fix the landed stale-lock recovery so Caravan builds and tests in the hermetic macOS Nix sandbox, where external `ps` is intentionally unavailable.

## Bead(s)

- `bd-b52180` — Recover verified-stale Caravan operation locks safely (reopened for Nix portability failure).

## Before state

- Failing tests: `operation_lock::tests::status_reports_absent_and_live_owner_without_mutation` and `recovery_requires_dead_old_owner_and_exact_token` failed in `nix flake check` with `git_spawn_failed` because `ps` was absent.
- Relevant metrics: package derivation `/nix/store/23zp...-caravan-0.0.1.drv` failed 2/107 library tests.
- Context: the live recovery behavior had landed, but its process-liveness probe depended on PATH and was not hermetic.

## After state

- Failing tests: none.
- Relevant metrics: `nix flake check --no-write-lock-file` passes on `aarch64-darwin`; 107 library + 4 binary + 3 integration tests pass; strict all-target clippy passes.
- Context: Unix liveness uses Rustix `kill(pid, 0)` directly. ESRCH means dead; EPERM conservatively means alive. No subprocess, PATH, shell, or unsafe application code is involved.

## Diff summary

- Code/content commit: `c3baa02`.
- Files touched: `Cargo.toml`, `Cargo.lock`, `src/operation_lock.rs`.
- Tests: existing live/dead recovery tests now execute in the Nix sandbox unchanged.
- Behavioural delta: guarded recovery is hermetic and buildable on macOS while preserving fail-closed live-owner detection.

## Operator-takeaway

The exact failure Harry observed is fixed and proved by the same `nix flake check` command that previously failed.
