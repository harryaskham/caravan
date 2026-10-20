# Session summary — the queue repairs routine native Stack drift itself (rebased generation)

## Goal

Make diverged native Stacks ordinary queue work instead of an operator decision. Under a repository's reviewed Stack mutation opt-in, sync now repairs one caravan-local drifted Stack per tick with the same sealed, lease-guarded native rebase an owner would review. It then lets fresh CI and the next tick land the result. This generation replaces PR #274, which stacked on squash-merged predecessors and could not merge cleanly. The tree is identical.

## Bead(s)

- `bd-7ff76d` — Repair diverged native stacks safely as normal queue behaviour
- (parent: `bd-acf9ca` — Make Cara admission and merge succeed reliably: provider truth over receipt ceremony)

## Before state

- Failing tests: none.
- Since bd-4170f8, ordinary sync never invoked the native rebase publisher, so ancestry drift always needed an explicit `native-stack rebase-apply`. On 2026-10-20 Cacophony Stack #4567 stalled until Harry merged both PRs by hand.

## After state

- Failing tests: none (`cargo test --all`, clippy `-D warnings`, fmt clean).
- `native_stack_rebase::repair_drift_for_sync` is sync's single gated entry. It requires the reviewed `mutations_opt_in` and `manual_drift_repair` false, and acts only on open, mapped, unpaused, unparked Stacks with only ancestry or PR base/head representation drift. It uses the sealed planner with actor `caravan-scheduler` and force-with-lease against exact observed heads, rebuilds the Stack exactly, checkpoints the receipt, and never replays a plan.
- Sync dispatches no CI; rewritten heads' pushes start fresh CI. Preparation refusals write nothing and leave the caravan quarantined with its owner command.

## Diff summary

- Code/content commits: 19291b5, rebased onto main 02f3691 with a tree identical to the earlier reviewed 5b3b54b (final landed squash SHA comes from the reintegration receipt).
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: src/native_stack_rebase.rs, src/sync.rs, src/config.rs, src/stack_recovery.rs, src/sync/tests/native_quarantine.rs, tests/non_force_repair_contract.rs, SPEC.md, README.md, docs/lifecycle.md, .agents/skills/cara-operator/*.
- Tests: +4; 1 contract test updated.
- Behavioural delta: routine drift on opted-in repositories is repaired by the queue. `stack_rollout.manual_drift_repair: true` restores the owner-only policy.

## Operator-takeaway

This deliberately reverses bd-4170f8 on Harry's 2026-10-20 direction, with one gated entry, a lease on every head, no synthetic CI and no replay. Separately, stacking a PR on a parent that is later squash-merged leaves the child unmergeable until it is rebased. `owned-pr-branch-update` could not push that rebase (receipt ghrec-01a15a99), so this generation supersedes PR #274.
