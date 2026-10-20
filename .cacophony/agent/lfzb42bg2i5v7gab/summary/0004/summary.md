# Session summary — one stuck native Stack quarantines only its caravan

## Goal

Stop a sole caravan whose native Stack needs owner repair from starving the whole queue. Caravan-local Stack drift now quarantines only that caravan: the caravan releases capacity and escalates with the exact recovery command, while independent caravans and admission keep moving.

## Bead(s)

- `bd-4d4621` — Unrecoverable sole caravan starves the queue for hours: retry_tick forever on github_stack_recovery_* with no escalation or capacity release
- (parent: `bd-acf9ca` — Make Cara admission and merge succeed reliably: provider truth over receipt ceremony)

## Before state

- Failing tests: none.
- On 2026-10-12 the sole Cacophony caravan (#4444 -> #4445) failed every tick for 4.5 hours on `github_stack_recovery_*` refusals, each classed `retry_tick`. Any open-Stack problem other than pure member-order drift failed every caravan's tick, quarantined caravans still consumed `max_caravans`, and nothing escalated.

## After state

- Failing tests: none (`cargo test --all`, clippy `-D warnings`, fmt clean).
- Ancestry divergence, member order, and PR base/head representation drift on an open mapped Stack quarantine only that caravan: it is excluded from convergence and capacity, like a parked caravan. Provider-unknown ancestry and inventory problems still fail closed.
- `stack_backend.quarantined_caravans` is derived from provider truth on every read and includes the exact recovery command. A zero-write automatic recovery refusal for such a caravan is contained; indeterminate or written outcomes still fail the tick.
- Scheduler status escalates quarantine as `external_decision` with `quarantined_caravans`. Non-retryable stack recovery codes that still abort a tick escalate instead of retrying.

## Diff summary

- Code/content commits: aecd108 (final landed squash SHA comes from the reintegration receipt).
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: src/read.rs, src/sync.rs, src/sync/decision.rs, src/stack_recovery.rs, src/lib.rs, src/loop_runner.rs, src/reshape.rs, src/stack_policy.rs, src/sync/tests.rs, src/sync/tests/native_quarantine.rs (new).
- Tests: +6 (an independent root lands beside a diverged Stack, unknown ancestry still fails closed, capacity release at max_caravans=1, escalation with the recovery command, containment rules, derivation).
- Behavioural delta: one caravan's Stack drift no longer stops the queue; it is escalated to its owner.

## Operator-takeaway

Quarantine is containment, not repair. bd-7ff76d makes the queue repair routine drift itself, so quarantine should become rare. When it does happen, the scheduler's `quarantined_caravans` names the exact command to run.
