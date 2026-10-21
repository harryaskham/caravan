# Session summary — queue-owned repair of native Stacks holding closed history

## Goal

Stop the Cacophony queue wedging on `github_stack_closed_history_requires_owner`.
Since 10:40Z on 2026-10-21 every `cara-sync-cacophony` tick exited 1 on native
Stack #4597 (merged #4595/#4596/#4598/#4601, then a stale flake bump #4600 that
was later closed unmerged, then #4603, which diverged and conflicts with main),
and with `max_caravans: 1` that caravan held the only slot so #4604–#4606
waited. Harry's direction: agents should do what he does by hand — prune the
merged rows, rebase the stack, and get conflicting work out of the way.

## Bead(s)

- `bd-7efe6a` — sync: autonomously repair drifted stacks with closed history
  (prune merged, rebase linear, evict conflicting) instead of requires_owner

## Before state

- `require_supported_history` refused any mapped Stack whose closed rows were not
  one clean merged prefix ahead of a linear open suffix, failing the whole tick
  (not contained, not quarantinable because of `github_stack_merged_prefix_invalid`).
- bd-7ff76d routine drift repair (v0.0.144) only handled quarantinable Stacks
  with a proven merged prefix, and a real conflict left the caravan quarantined
  in its slot.
- Rebuilds never retargeted a root whose predecessor closed without merging, so
  a multi-member rebuild would have failed GitHub's base-chain validation.
- `cara sync --dry-run` failed `github_stack_backend_unhealthy` on the same Stack.

## After state

- Under the reviewed `stack_rollout.mutations_opt_in` (and unless
  `manual_drift_repair: true`), sync runs a closed-history rebuild before
  automatic recovery: exact eligibility (open drifted Stack, one mapped
  unpaused unparked caravan, provider-closed retained rows, open rows all
  members, only rebuild-resolvable problems, and exactly the shapes recovery
  refuses). Same sealed planner/publisher as bd-7ff76d (`PlanScope::ClosedHistory`
  replays every active member linearly from the default branch).
- Apply: force-with-lease rebase of the replayable prefix, clear the drifted
  Stack, evict the unreplayable suffix tail-first (labels only, heads/bases
  preserved, refetched generation guard), retarget survivors' bases onto their
  linear predecessor, rebuild survivors, checkpoint the receipt. A root based on
  a closed row binds its immutable `refs/pull/N/head`.
- Eviction triggers only on deterministic replay refusals (conflict codes and
  unreplayable history shapes); environment failures never evict. Applies to
  routine drift repair too.
- The tick emits `evicted` (with raw `provider_before`) and the existing
  `sync_failed` event with `metadata.repair_request` (PR, head, base, target,
  refusal, conflicting paths, evicted PRs, suggested `cara repair start … --non-force`
  and `cara rejoin`). `cara sync --dry-run` previews the same sealed plan hash.
- Version bumped to 0.0.146 with `docs/release/v0.0.146-rollout.md`; tag and
  pin rows follow after merge.
- Validation: fmt and clippy (`--all-targets -D warnings`) clean; full lib suite
  green locally; required hosted CI owns the final gate.

## Diff summary

- Code/content commits: pending final squash SHA from the reintegration receipt.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `src/native_stack_rebase.rs` (plan scope, eligibility,
  pending-plan selection, eviction-on-conflict apply, survivor retarget,
  closed-row range binding, repair request types), `src/reshape.rs`
  (`queue_evict_suffix`), `src/stack_recovery.rs` and
  `src/stack_recovery/closed_prefix.rs` (shared `unsupported_history`,
  `recovery_caravan_id`), `src/sync.rs` (closed-history call site, eviction
  receipts/events/hooks), `src/sync/plan.rs` (dry-run `native_stack_repair`),
  `src/lib.rs` help, SPEC/README/lifecycle/cara-operator skill docs,
  Cargo version, rollout doc.
- Tests: +13 (closed-history prune, Cacophony #4597 singleton, gating, replay
  idempotence, dry-run parity, eviction events + freed capacity, survivor
  projection, evictable classification, repair request, output round trip,
  closed-row range binding, tail-first labels-only eviction with moved-generation
  refusal); contract test updated for two gated sync call sites.
- Behavioural delta: a drifted Stack with closed history is rebuilt by the queue
  instead of failing every tick, and an unreplayable member is evicted with a
  typed repair request instead of holding the slot.

## Operator-takeaway

The queue now does Harry's manual unwedge itself, but only for exact,
provider-proven shapes and only behind the existing reviewed Stack opt-in; a
real conflict becomes an eviction plus a `sync_failed` repair request rather than
a stall. Two things remain: the release tag/pin for v0.0.146 after merge (then
Cacophony's cron pin must advance before #4597 is repaired), and a closed-history
Stack the queue may *not* repair (paused caravan, `manual_drift_repair`, unknown
ancestry) still fails every tick rather than quarantining just its caravan — filed
as a follow-up draft.
