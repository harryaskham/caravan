# Session summary — phase-aware physical sync commitment

## Goal

Prevent whole-chain physical sync from consuming its absolute deadline during planning and then partially mutating provider control state without enough time to apply and reconcile exact branch generations.

## Bead(s)

- `bd-7f1f39` — Reserve physical-sync apply budget before control-label mutation.
- Coordinated boundaries: `bd-4e4615` remains the separate membership post-rebase reserve; `bd-e89b4b` remains the separate exact force-intent restoration contract after proven failed publication.

## Before state

- Physical planning and repeated dry-run verification shared the complete `sync.max_duration_secs` deadline with mutation/apply.
- A two-member plan could spend roughly 110 of 120 seconds materializing retained generations, then reach a final dry-run with under ten seconds, or remove labels/disable auto-merge before a later apply timeout.
- `apply_prepared` repeated remote-head and permission dry-run checks after the global barrier and after irreversible control mutations.
- Crash recovery had no exact checkpoint between confirmed control mutations and branch apply.
- A child provider `BaseRefOid` lagging its already-advanced parent branch was treated as a stale remote branch rather than a retained historical ancestor boundary.

## After state

- Planning runs only to a precommit phase deadline which preserves one child timeout; complete plans must then fit a conservative N-member budget for serial controls, exact source/range/target revalidation, lease pushes, bounded parallel chain rounds, midpoint/final discovery, and base/CI reconciliation.
- Insufficient time returns `physical_sync_budget_insufficient` with required/remaining/additional milliseconds, command and mutation reserve, complete-or-partial plan count/hash, zero provider/branch mutations, and concrete configuration guidance. It is non-retryable operator action, so unchanged deadline exhaustion cannot hot-loop.
- The repository opts into a 900-second physical-sync bound while retaining the one absolute deadline.
- Confirmed force/control receipts are durably checkpointed; the immediately following branch-apply phase is explicitly provider-state-indeterminate for crash recovery.
- Global no-write verification still checks retained objects, provider PRs, remote sources/default, permission, and dry-run leases under the precommit boundary. Apply revalidates exact source/range/target generations but removes only the redundant permission dry-run; exact force-with-lease remains the candidate writer-race gate.
- A new `historical_parent_branch` range binds the provider-retained old child base to the exact current parent head and same-batch simulated rewrite. Only ancestor lag converges; mismatched topology and real head/default movement still fail closed.

## Diff summary

- Code/content commit: `0802585` (`bd-7f1f39: reserve physical apply phase budget`); the final landed squash SHA will come from reintegration.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files changed: `.caravan/config.yaml`, `README.md`, `SPEC.md`, `src/physical_rebase.rs`, `src/sync.rs`, `src/sync/decision.rs`, and `src/sync/tests.rs`.
- New/strengthened focused fixtures cover N-member reserve arithmetic and zero-write typed evidence, command timeout exceeding the whole tick, historical parent BaseRefOid lag with parent→child rebuild, post-barrier default movement, post-barrier child writer race, nonlinear parent/linear child replay, and stable scheduler classification.
- Validation passed: `cargo fmt --check`, `cargo check --quiet`, `git diff --check`, and each named exact safety fixture. Required hosted CI remains authoritative for full workspace test/lint/Nix validation.

## Operator-takeaway

Physical sync now has an explicit commit-admission boundary: it either retains enough bounded time and mutation capacity for the complete apply/reconcile phase, or it returns a zero-write plan receipt telling the operator exactly how much configuration budget is missing. Provider base-ref lag is recoverable only when ancestry and current-parent identity are proven; true writers still lose safely to exact lease checks.
