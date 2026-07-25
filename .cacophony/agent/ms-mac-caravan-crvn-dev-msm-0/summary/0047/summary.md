# Session summary — retire merged caravan heads and bound post-merge checkpoints

## Goal / bead

- `bd-a5197c` (P0) — a provider-merged head must never keep presenting as an active caravan requiring auto-merge repair, and a checkpoint must never fail an operation whose provider merge already succeeded.

## Live recurrence

- Cacophony PR #2101 merged at `3ad8d2843fe2f05e4617081d14837482f51b448c`, yet Cara stayed degraded with a stale sole-member caravan and `auto_merge_invariant`, blocking the fresh `pr_cara_join` root so PR #2167 published unjoined.
- Original instance: PR #2053/#2056 merged, then `operation_lock_checkpoint_too_large` left merged heads shown as actionable.
- No provider mutation was performed on any merged PR during this work.

## Change

- Provider truth now dominates durable hold records. A recorded head observed merged or closed becomes `PauseState::Retired` with an exact `retired_state`, is never `auto_merge_suspended`, and carries history-only guidance.
- Added `PauseState::is_effective()`; only active/expired holds constrain operations. Sync, plan, force, force-intent, pause, and resume now use it, so retired records can neither suppress invariants nor be resumed.
- Oversized operation-lock checkpoints compact instead of failing: evidence is replaced by a bounded receipt with original byte length, exact SHA-256 digest, top-level keys, and per-array counts, with a phase-only last resort. Phase and provider-indeterminacy survive compaction.
- The dashboard treats stale and retired holds as historical diagnostics only.
- Boxed the large dead-owner recovery variant to satisfy strict Clippy.

## Regression proof

- A merged force-labelled head with a long check history yields zero caravans, zero unqueued, zero problems, healthy status, and a preserved merge receipt.
- A merged recorded head classifies as retired, keeps `auto_merge_suspended=false`, is not effective, and refuses resume.
- A ~4,000-check post-merge checkpoint persists within the owner-file bound and reports `checks: 4000`, the original size, and a `sha256:` digest.

## Validation

- Full library suite at 16 threads: 379/379 green.
- Strict all-target/all-feature Clippy: green.
- `git diff --check`: green.
- README/SPEC document retirement and bounded checkpoint semantics.

## Commit

- `6a97709`.
