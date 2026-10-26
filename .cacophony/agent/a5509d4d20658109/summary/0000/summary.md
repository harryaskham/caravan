# Session summary — bounded native-root CI waits

## Goal

Stop a native Stack with unqualified root CI from consuming the entire sync request budget through repeated member analysis and then entering a membership mutation it cannot finish or roll back. Keep actual required checks authoritative, preserve existing native transactions, and separate the CI-trigger and cron-log defects from the queue's bounded-wait fix.

## Bead(s)

- `bd-4eccb7` — native root CI wait exhausts the sync request budget.
- Related Cacophony `bd-ba6c82` — current-base PR-event recovery; its owner is implementing the materialized live base-ref/SHA binding fix.
- Related Cacophony `bd-a1a5ae` and `bd-626170` — cron-log rotation and terminal-receipt preservation.

## Before state

- A zero-write native prefix wait was discarded, allowing ordinary convergence to repeat the same member reads and proceed into automatic admission.
- Supplied incident evidence showed 256/256 requests consumed and membership rollback indeterminate at Cacophony root PR #4766.
- CI reruns retained an obsolete event base ref even after the workflow materialized the live base SHA. This workflow defect is owned separately by Cacophony.
- No implementation was present at session hydration; the conversation before the controller request contained only non-authoritative heartbeat receipts.

## After state

- Native convergence stops expensive descendant CI at the first unqualified member, skips a redundant full Stack read when the root is unqualified, and retains the wait as a terminal tick result before automatic admission.
- Typed native wait evidence identifies the Stack, blocking member, blockers and deferred suffix. Pending CI remains `waiting_ci`, not claimed progress. Independent ready caravans still progress.
- Existing landing checkpoints remain on their original reconciliation path. No required checks, source-ref fences, request limits or provider preconditions were weakened.
- Validation: all 83 focused native tests passed, including four new regression tests. Touched-file rustfmt and Git whitespace checks passed.
- Strict local Clippy remains blocked by five pre-existing ambient-toolchain diagnostics outside this diff. Existing draft `bd-8095c7` records that toolchain-lint issue; hosted configured CI is the final gate.
- No live Cacophony source, PR state, Stack topology, CI run, cron settings or logs were changed. The CI recovery and log-rotation tracks remain separately open.

## Diff summary

- Code/content commit: `56b5e29b7d5e0f2f8116fe26525657a4fa622305`; canonical rebase retained the tested head. Final landed squash SHA comes from the queue receipt.
- `src/sync.rs`: bounded native-prefix completion, root-only wait fast path, typed wait receipts, retained prior receipts, and independent singleton progress.
- `src/sync/decision.rs`, `src/loop_runner.rs`: additive scheduler receipt field and fixture initialization.
- `src/sync/tests/native_root_wait.rs`: four regressions covering pending/cancelled/failed roots at 2/5/25 members, terminal wait/admission disposition, preserved checkpoints, and independent root progress.
- `src/sync/tests.rs`, `src/sync/tests/native_quarantine/join_race.rs`: revised bounded-read assertions and call signatures.
- `README.md`, `docs/native-root-ci-wait.md`: behavior, recovery boundaries and ownership of remaining incident slices.
- Reflection: draft `bd-d60ca8` for initial task-delivery evidence; draft `bd-cb3569` for unbounded connected inbox notes. Existing threaded-reply issue `bd-869df3` and Clippy draft received a controller addendum rather than duplicates.

## Operator-takeaway

An unchanged CI wait must end a bounded queue tick, not be discarded and rediscovered until a later membership transaction runs out of budget. This patch fixes that failure mode without pretending to repair CI: the separately owned workflow base-ref binding and cron-log rotation fixes are still needed for full incident recovery.
