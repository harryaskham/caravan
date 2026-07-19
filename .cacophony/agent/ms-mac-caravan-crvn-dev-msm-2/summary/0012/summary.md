# Session summary — Make Cara sync a deterministic scheduler boundary

## Goal

Turn Cara's already-idempotent whole-chain synchronization into a stable Cacophony scheduler contract: every successful tick must expose exact final main/root/tail/member generations and distinguish healthy, waiting-CI, and held states without waking a repair agent, while only real CI, graph, semantic, or provider-generation decisions wake `caco-merger`. Enable physical cumulative mode only after atomic join and stale force-intent safety were canonical.

## Bead(s)

- `bd-d38dbf` — Expose deterministic scheduler tick and wake contract.
- Related dependency: `bd-6e7179` — Add atomic remote join receipt contract (landed at `780ac5d`).
- Related safety dependency: `bd-4342ee` — Bind `caravan-force` intent to the exact head generation (landed at `35c08e6`).
- Reflection draft: `bd-94a009` — Make Cara self-update replace the active PATH-visible binary.

## Before state

- `cara sync` returned detailed mutation, CI, and final graph facts but no versioned scheduler projection.
- Expected provider/head/tail precondition races could be surfaced through the same generic `sync_failed` path used for repair-worthy decisions.
- Successful empty, expected, queued, or running CI was represented internally as waiting, but external schedulers had to infer that no merger wake was appropriate.
- `.caravan/config.yaml` still had `rebase_on_join: false`; atomic JoinReceipt v1 and generation-bound force invalidation had not yet landed.

## After state

- Successful `SyncOutput` includes `scheduler_status` schema v1 with exact default branch and ordered caravan generations: caravan ID, root, tail, and every member's PR/head/base/synthetic candidate/CI disposition.
- Successful dispositions are `healthy`, `waiting_ci`, or `held`, always with `wake_class=none`; empty checks remain fail-closed waiting rather than passing or waking repair.
- Failed ticks attach typed scheduler status with `wake_class=retry_tick`, `external_decision`, or `operator_action`. Stale provider preconditions are retry-only and emit no repair hook.
- Terminal CI, graph/semantic decisions, and proven provider-generation invariants emit one canonical `ci_failed` or `sync_failed` repair event even when ordinary topology events were also completed in the same tick.
- `.caravan/config.yaml` now explicitly enables `rebase_on_join: true`, rebased over atomic join `780ac5d` and force-generation safety `35c08e6`.
- Focused atomic join, JoinReceipt, force invalidation/preservation/reapplication, waiting-CI schema, stale retry, provider invariant wake, and terminal-CI wake tests all pass. Canonical rustfmt and `git diff --check` pass.

## Diff summary

- Code/content commit: `06dc82e`; final landed squash SHA will come from the reintegration receipt.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `.caravan/config.yaml`, `src/sync.rs`, `src/loop_runner.rs`, `src/main.rs`, `README.md`, `SPEC.md`, `docs/v1-parity.md`.
- Tests: extended three scheduler tests with exact serialized schema/wake assertions and added one provider-generation invariant wake-event test; focused cross-contract tests from `bd-6e7179` and `bd-4342ee` were rerun after rebase.
- Behavioural delta: routine topology/lease drift remains deterministic retry work; waiting/held states remain quiet; only actionable external decisions wake repair, with exact provider generation evidence for deduplication.

## Operator-takeaway

Cara now exposes the boundary Cacophony needs to replace heuristic merge polling: a single bounded tick either proves the exact cumulative generation is healthy/waiting/held without waking anyone, or returns a typed retry/operator/external-decision class. Physical rebasing is enabled only after stale force intent became generation-bound, preventing a rewritten empty-check head from inheriting old bypass authority.
