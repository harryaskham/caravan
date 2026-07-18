# Session summary — Replace raw Caco repair surgery with a Cara-owned workflow

## Goal

Audit whether Caco was using Cara correctly during the live Cacophony caravan recovery, then provide the missing first-party repair continuation so typed head/link conflicts no longer force controllers into raw nested worktrees, manual ref updates, or hand-pushed merge commits.

## Bead(s)

- `bd-cf44d1` — Provide a managed clean repair workspace for Cara sync decision recovery.
- Related completed orchestration: `bd-cacf7c` — Make sync rebuild a real rebased chain after parent head changes.
- Routed broken-main follow-up: `bd-4dabc9` — Restore canonical rustfmt cleanliness after remote-preflight landing.

## Before state

- The live caco-ctrl replay proved Caco called `cara status` and `cara sync --all` first and honored typed initialization, dirty-worktree, local-divergence, and head-conflict decisions.
- Recovery was not Cara-owned: caco-ctrl created `.cara-clean`, manually fetched and updated refs, merged, edited conflict files, committed, pushed, and then resumed Cara.
- A dirty controller checkout and a first-party checkout with a daemon/internal `origin` had no safe mechanical continuation.
- Interrupted repair work had no durable exact-head/target/config/scope/publication state machine.

## After state

- `cara repair start --pr N [--target-pr T]` creates a durable independent provider-owned clone below the caller repository's common Caravan metadata without touching caller HEAD, refs, config, index, or files.
- Atomic manifests retain repository/PR identity, exact head, old base, target, explicit provider URL, config fingerprint, workspace, typed conflicts, mechanically staged baseline, lifecycle state, timestamps, and committed/published head.
- `repair continue` verifies session/config/workspace identity, exact merge target, no unresolved/unstaged/untracked/marker state, conflict-only patch scope, exact merge parents, and unchanged head and target refs. It then creates the merge commit, publishes by ordinary non-force fast-forward, verifies the provider ref, and resumes `sync --all` while holding caller mutation ownership.
- Commit-before-manifest and push-before-manifest interruptions recover idempotently. `repair status` is read-only; confirmed `repair abort` removes local state only and never mutates the provider.
- CLI, JSON, MCP metadata, README, SPEC, and parity docs expose the same bounded start/status/continue/abort contract.

## Diff summary

- Code/content commits: `b6dee89`, `1b343dc`, `fadabee`, `9c43ae9`; final landed squash SHA will come from the reintegration receipt.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `src/repair.rs`, `src/lib.rs`, `src/main.rs`, `tests/v1_parity.rs`, `README.md`, `SPEC.md`, `docs/v1-parity.md`.
- Tests: 6 focused repair tests, 6 binary tests, 3 CLI/MCP parity tests, full 213-library/6-binary/7-CLI/3-parity suite, and strict all-target/all-feature Clippy all pass on main `ff66a89`.
- Safety cases cover dirty caller plus unusable internal origin, caller-local branch divergence, exact two-file-style conflict scope, unrelated-path rejection, remote head movement without overwrite, exact merge parents and non-force publication, interruption recovery without duplicate commit/push, and confirmed local abort with unchanged provider state.
- Pre-existing `cargo fmt --all -- --check` drift from `eb77ec7` remains outside this functional patch and is routed to `bd-4dabc9` with msm2.

## Operator-takeaway

Caco's graph discovery and typed stopping were correct; the unsafe behavior began only because Cara had no owned post-decision repair path. This change makes that continuation explicit, durable, exact-generation, non-force, and independent of the controller's dirty or internally-remoted checkout, while leaving merged-head promotion and whole-chain rebasing in `bd-cacf7c` rather than conflating the two safety boundaries.
