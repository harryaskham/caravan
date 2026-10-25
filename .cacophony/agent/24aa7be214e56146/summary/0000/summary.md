# Session summary — Refresh stale-base native Stack roots

## Goal

Make the queue refresh a native Stack whose topology is healthy but whose root missed default-branch content, rather than waiting indefinitely with no changes. Reuse the existing reviewed repair transaction instead of introducing another branch writer or weakening landing CI.

## Bead(s)

- `bd-c299b8` — Auto-refresh a stale-base caravan root and propagate down the chain.

## Before state

- Automatic native repair selected topology drift or retained closed history, not an otherwise exact Stack with a stale-base root.
- The sealed rebase planner already understood stale roots and propagated rewritten parents through the suffix, but ordinary sync never selected that healthy-topology case.
- Current assignment was confirmed through authoritative agent and bead state; no ownership claim was repeated.
- Reported live case: Caravan Stack #284 containing PRs #280 and #281 after main advanced. No live queue mutation was performed during development.

## After state

- Sync and dry-run share a new stale-base repair selection under the existing GitHub Stack opt-in, manual-repair, pause and parking gates.
- Exact cumulative-tree evidence must show unvalidated default content, or exact compatibility evidence must prove a conflict. Historical base metadata alone, unknown comparisons and already-current heads do not authorize another rewrite.
- Selected root and descendants use the existing sealed atomic lease-guarded replay and typed conflict handoff. Sync reports “refreshing stale base” and returns before landing, admission or explicit CI dispatch.
- Validation passed: 7 new stale-base tests, 13 native-rebase tests, 8 native-quarantine tests, 7 closed-history repair tests and 2 real local-Git atomic-publication tests (37 total). Formatting was applied and `git diff --check` passed. Required hosted CI remains the final broad gate.
- Runtime rollout and acceptance against the real provider remain separate; this session did not deploy a binary or alter live Stack #284.

## Diff summary

- Code/content commit: `0cb2d3f` — `bd-c299b8: refresh healthy native Stacks with stale-base roots`.
- `src/native_stack_rebase.rs`: exact stale-base selection, repair-kind propagation and existing authorization-contract coverage.
- `src/sync.rs`: bounded repair invocation and explicit refresh/fresh-CI reporting.
- `src/sync/plan.rs`: matching sealed preview, tagged `repair: stale_base`.
- `src/sync/tests/stale_base_refresh.rs`: seven regression tests for selection, identity/policy refusals, conflicts, historical-base no-op behavior, checkpoint deduplication and the post-repair landing boundary.
- `src/sync/tests.rs` and `src/sync/tests/native_quarantine.rs`: test registration and shared fixture visibility.
- `README.md`: document stale-base refresh and correct a contradictory legacy statement that ordinary sync never uses native rebase.
- Reflection: existing draft `bd-a743fa` already covers the missing injectable full-transaction provider seam; an addendum was sent to the controller rather than filing a duplicate.

## Operator-takeaway

Healthy provider topology does not prove that the root has seen current main. The queue now repairs that precise stale-content case through its existing fenced transaction, without treating stale metadata as rewrite authority or old green CI as permission to land rewritten heads.
