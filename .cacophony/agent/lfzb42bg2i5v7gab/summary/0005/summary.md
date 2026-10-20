# Session summary — the queue repairs routine native Stack drift itself

## Goal

Make diverged native Stacks ordinary queue work instead of an operator decision. Under a repository's reviewed Stack mutation opt-in, sync now repairs one caravan-local drifted Stack per tick with the same sealed, lease-guarded native rebase an owner would review. It then lets fresh CI and the next tick land the result.

## Bead(s)

- `bd-7ff76d` — Repair diverged native stacks safely as normal queue behaviour
- (parent: `bd-acf9ca` — Make Cara admission and merge succeed reliably: provider truth over receipt ceremony)

## Before state

- Failing tests: none.
- Since bd-4170f8, ordinary sync never invoked the native rebase publisher, so ancestry drift always needed an explicit `native-stack rebase-apply`. On 2026-10-20 Cacophony Stack #4567 (child PR4566 no longer containing root PR4565) stalled until Harry merged both PRs by hand.

## After state

- Failing tests: none (`cargo test --all`, clippy `-D warnings`, fmt clean).
- `native_stack_rebase::repair_drift_for_sync` is sync's single gated entry. It requires `stack_type: github`, `stack_rollout.mutations_opt_in`, and `manual_drift_repair` false. It acts only on an open, mapped, unpaused, unparked Stack whose problems are only ancestry or PR base/head representation drift. It uses the sealed planner (actor `caravan-scheduler`) and force-with-lease against each exact observed head, rebuilds the Stack exactly, checkpoints the receipt, and never replays an applied plan hash.
- Sync dispatches no CI; the rewritten heads' pushes start fresh CI. A root merged out of band is handled by replaying the suffix onto the default branch.
- Preparation refusals such as real conflicts write nothing and leave the caravan quarantined with the owner command from bd-4d4621.
- `stack_rollout.manual_drift_repair: true` restores the owner-only policy. The field is omitted when false, so fingerprints are unchanged.

## Diff summary

- Code/content commits: 5b3b54b (final landed squash SHA comes from the reintegration receipt).
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: src/native_stack_rebase.rs, src/sync.rs, src/config.rs, src/stack_recovery.rs, src/sync/tests/native_quarantine.rs, tests/non_force_repair_contract.rs, SPEC.md, README.md, docs/lifecycle.md, .agents/skills/cara-operator/{SKILL.md,references/*.md}.
- Tests: +4 (plan selection and gating, stale-root suffix replay, preparation-refusal marking, a replaced force-boundary contract that pins the single gated entry); 1 contract test updated.
- Behavioural delta: routine drift on an opted-in repository is repaired by the queue; Cacophony needs no config change, because its `mutations_opt_in` is already true.

## Operator-takeaway

This deliberately reverses bd-4170f8's "sync never rewrites" policy, on Harry's direction of 2026-10-20, but keeps its lessons. There is one gated entry, a lease on every head, an exact sealed plan, no synthetic CI, and no replay. Any repository can opt out with `stack_rollout.manual_drift_repair: true`. Watch the first live repairs on Cacophony after the release is adopted.
