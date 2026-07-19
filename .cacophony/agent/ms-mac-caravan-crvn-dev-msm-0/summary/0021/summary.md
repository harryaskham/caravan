# Session summary — Make repair materialization timeout-safe and resumable

## Goal

Unblock the live PR #1962 managed repair after its authenticated cold SSH clone exceeded the generic 30-second subprocess budget, while preserving exact-generation safety, process-group cleanup, durable recovery, and the prohibition on raw Git fallback.

## Bead(s)

- `bd-9264d0` — Cara repair start cannot recover from 30-second SSH clone timeout.
- Parent capability: `bd-cf44d1` — Managed clean repair workspace for Cara sync recovery.

## Before state

- Wrapper-sanitized Cara status was healthy and showed caravan `1962 -> 1959 -> 1958 -> 1946`.
- `repair start --pr 1962` timed out twice during the first-party SSH clone at the generic fixed 30-second bound.
- After a confirmed abort of the first attempt, the second exact session remained intentionally preserved as `preparing`, with a manifest but no workspace and no provider mutation.
- The old manifest could not be inspected or resumed without a complete workspace; clone/fetch phases lacked dedicated budget and durable phase/error/process-group evidence.

## After state

- Strict config adds `repair.materialization_timeout_secs`, defaulting to 180 seconds and independently bounded from lightweight `command_timeout_secs`.
- Repair manifests persist exact `RepairPhase`, materialization budget, bounded last-error evidence, elapsed/budget milliseconds, process-group ID when a child started, partial path, and resume/abort guidance before every clone/fetch/checkout/merge phase.
- Process-runner timeout evidence now preserves the owned child process-group ID while retaining full group terminate/reap behavior.
- Exact `preparing` sessions with missing or verified partial workspaces are inspectable. Re-running the same `repair start` revalidates repository, PR head/base, target, provider URL, and config, removes only the canonical incomplete clone, and resumes materialization. Head/target/provider drift remains fail-closed.
- `repair continue` returns a typed exact start command for incomplete preparation; human/JSON status expose phase, timeout, error, partial path, and safe next action. Confirmed abort remains local-only.

## Diff summary

- Code/content commit: `bfc9302`; final landed squash SHA will come from the reintegration receipt.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `.caravan/config.yaml`, `src/config.rs`, `src/initialization.rs`, `src/repair.rs`, `src/command.rs`, timeout-pattern consumers, `src/main.rs`, `README.md`, and `SPEC.md`.
- Tests: full 215-library/6-binary/7-CLI/3-parity suite passed; strict all-target/all-feature Clippy and canonical rustfmt passed; canonical Nix flake completed 34 checks.
- Focused recovery tests prove durable clone timeout phase/budget/process-group evidence and exact preparing-without-workspace resume; existing stale-head, scope, interruption, non-force publication, abort, and dirty/internal-origin cases remain green.
- Packaged smoke: `cara 0.0.2`, repair help lists all four operations, and missing-session status returns typed `repair_session_not_found` rather than crashing.

## Operator-takeaway

Cold authenticated repository materialization is fundamentally different from a lightweight status probe. Giving it a separate bounded policy and making each phase durable turns timeout from a dead-end/abort loop into an exact resumable state machine, without extending mutation authority or relaxing any provider/head/target/config check.
