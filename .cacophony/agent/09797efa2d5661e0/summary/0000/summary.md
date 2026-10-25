# Session summary — Repair native Stack joins in the sync tick

## Goal

Keep GitHub Stack joins from turning routine, cleanly replayable divergence into an external repair request. Preserve the native-mode single physical writer, exact source leases, repository opt-in, and fresh-CI requirements while covering joins that become visible during discovery or auto-admission.

## Bead(s)

- `bd-28ae57` — GitHub-Stack join behind an active caravan head creates a diverged native Stack; sync refuses instead of repairing.
- Reflection draft `bd-40d3c7` — Dispatch native drift-repair hooks once at the sync boundary.
- Reflection draft `bd-17b606` — Add a hermetic full native-repair transaction fixture for sync join races.

## Before state

- Native join intentionally left source heads unchanged, but repair selection happened before a later fresh native routing read could expose the completed join.
- Strict-prefix Stack inventory was excluded as membership-only drift, even though the new logical suffix had no provider ancestry edges yet and could need replay.
- Auto-admission proceeded directly to ordinary convergence without selecting native repair again.
- No existing regression combined ordinary join, real Stack-status derivation, sealed repair selection, and atomic local Git replay for this shape.

## After state

- A zero-write native generation mismatch permits one fresh discovery and policy revalidation, then the existing gated repair or one routing retry. Repeated mismatch, ambiguous mappings, and unknown/partial writes remain refusals.
- Exact active-fleet prefixes qualify for sealed replay/rebuild rather than hiding a newly joined suffix as membership-only drift.
- Post-admission repair runs before ordinary convergence and preserves previous tick receipts and events. Rewritten heads wait for fresh CI; this path does not land them immediately.
- All 37 distinct focused tests pass: `native_join` (9), `native_quarantine` (12, including 4 overlapping race tests), `closed_history_repair` (7), and `native_stack_rebase` (13).
- The real local-Git regression covers immediate and lagging provider inventory, verifies child ancestry and preserved file contents, and confirms the active root remains unchanged and the repaired Stack has no quarantine.
- Ambient Rust 1.97 library Clippy reported an unrelated existing `manual_assert_eq` in `src/force_intent.rs:432`; the new module's wildcard import was fixed. Hosted CI uses the verified pinned Rust 1.91 toolchain and owns the final broad validation. No full local gate was rerun.
- No live queue, provider Stack, or consumer deployment was mutated for acceptance. Full provider unstack/create plus sync in one hermetic transaction remains a test-infrastructure follow-up, not a claimed test result.

## Diff summary

- Code/content commit: `c7ddd21ff9ae99324b190224425e7163600d7cd3`. Supported rebase completed without changing the head.
- `src/native_stack_rebase.rs`: include proven joined prefixes and replay their active members.
- `src/sync.rs` and new `src/sync/native_repair.rs`: bounded generation-race rediscovery, post-admission repair selection, and prior-progress retention.
- `src/read.rs`: expose the existing inventory trait/status seam within the crate for the hermetic cross-module regression.
- New membership/Git regression and four sync race/policy/receipt regressions; five tests added, none removed or weakened.
- `SPEC.md`: document the same-tick repair cases and their safety boundaries.
- Product changes span nine files. The summary is runtime-local metadata and is not staged or committed to the product branch.

## Operator-takeaway

A clean native join should be repaired by the queue in the same tick even when provider membership arrives after discovery. The fix retains one physical writer and fresh-CI gating; consumer rollout is separate from source submission. Reflection also identified an existing duplicate hook-dispatch path and a gap in full-transaction testability, recorded as drafts rather than mixed into this repair.
