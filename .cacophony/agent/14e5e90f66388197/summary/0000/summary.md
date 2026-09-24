# Session summary — Immutable conflict evidence

## Goal

Capture the generation that actually failed physical preparation, without inventing exclusive custody or restricting concurrent scheduler and owner repairs.

## Bead(s)

- `bd-647b33` — Emit immutable failure-generation evidence for owner conflict inspection.
- `bd-55886b` — Separate independent-admission policy review filed and coordinated; no policy implementation in this patch.

## Before state

Conflict errors omitted the original head/base/target tuple. Later status enrichment could describe a different generation. Earlier custody proposals were superseded by the operator's concurrent exact-lease repair policy. Prior bd-74353f work was already merged; its original branch was safely reanchored to its identical-tree squash with a first-party containment receipt and backup, then rebased.

## After state

Preparation conflict errors contain versioned immutable snapshots and explicit preparation-scoped no-write evidence. Physical sync supplies its existing operation ID; enclosing authenticated scheduler event identity remains separate. Simulated targets stay explicitly tagged. No queue gates, scheduler suppression, custody backend, wake implementation or live PR mutations were added.

Two focused tests passed: actual Git rebase-conflict snapshot retention and outer partial/indeterminate outcome preservation with replay serialization. Formatting and diff whitespace checks passed. Broad final validation belongs to hosted CI.

## Diff summary

- Code commit: `a59b661` (final landed SHA comes from the lifecycle receipt).
- `src/physical_rebase.rs`: typed snapshot captured at preparation entry and attached to three conflict errors; extended real-Git regression.
- `src/sync.rs`: existing progress identity passed into preparation.
- `src/sync/tests.rs`: outer-outcome preservation and duplicate serialization regression.
- `tests/fixtures/rebase-failure-generation.json`: consumer schema fixture including simulated target.
- `docs/failure-generation-evidence.md`: identity, outcome scope, replay and authority boundaries.

## Operator-takeaway

A historical failure can prompt the same owner to inspect fresh state, but it is never a write lease. Preparation did not write; the surrounding operation may already have done so. The consumer must retain that distinction while Cara and the owner continue concurrently.
