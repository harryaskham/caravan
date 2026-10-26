# Session summary — Stale-base refresh, rebased for fresh queue submission

## Goal

Deliver automatic refresh of a native Stack whose topology is healthy but whose root missed current default-branch content. Preserve the existing branch leases, conflict handoff and fresh-CI requirements, and submit the corrected source on current main after the original queue jam cleared.

## Bead(s)

- `bd-c299b8` — Auto-refresh a stale-base caravan root and propagate down the chain.

## Before state

- Automatic native repair selected topology drift or retained closed history, leaving an otherwise exact stale-base Stack idle.
- Initial source was published as PR #285, but it conflicted with the old queue tail. Compatibility corrections were preserved locally at `821ea76`.
- Same-PR checkout tooling refused the managed checkout. An earlier audited branch update was not verified; the controller subsequently reported it not applied and prohibited replay. That receipt remains historical, not reused.
- After PRs #280/#281 merged, the controller authorized closing unjoined #285 and publishing a fresh generation. The typed close verified non-membership and closed #285. Supported rebase onto `7799268` stopped on the fixture-header conflict while replaying the first two commits.

## After state

- The supported rebase is complete. Both conflict resolutions retain main's `join_race` tests and the new standalone stale-base tests. Main's native-quarantine, join-race, merged-predecessor and post-join repair files remain unchanged by the final source diff.
- Sync and dry-run select a stale-base refresh only under the existing native Stack mutation, manual-repair, pause and parking gates, with exact cumulative-tree or conflict evidence.
- Selected root and descendants use the existing sealed atomic lease-guarded replay and typed conflict handoff. Sync reports “refreshing stale base” and returns before landing, admission or explicit CI dispatch.
- Historical base metadata alone, unknown evidence and already-current heads do not authorize repeated rewrites. The repair-result interface remains compatible with #281.
- **47 focused tests pass under the project's pinned Nix toolchain:** 7 stale-base, 13 native-rebase, 12 native-quarantine/join-race, 7 closed-history, 2 local-Git atomic-publication, 5 merged-predecessor discovery and 1 native-join repair.
- **`nix develop --command cargo clippy --tests -- -D warnings` passes.** Seven semicolon-style findings in the new regressions were fixed. The initial ambient Rust 1.97 invocation also exposed unrelated baseline lints; the repository/CI pin is Cargo 1.91, and existing draft `bd-8095c7` tracks that newer-toolchain baseline issue. No lint suppressions or unrelated source edits were added.
- The final checkout is clean, based on main `7799268ce04e2d7b95bd4e7d1621fa980b97a989`. The original `821ea76` remains on `preserve/bd-c299b8-821ea76`, retained until the new PR merges.
- No installed runtime, live Stack or other member's branch was modified by this worker. Required hosted CI remains the final broad gate after source submission.

## Diff summary

- Rebased code/content commits: `44b4b2d` (implementation), `59c3ac5` (isolated fixtures), `61339e1` (repair-output compatibility), `ac46114` (strict Clippy corrections). Final landed squash SHA comes from the queue receipt.
- `src/native_stack_rebase.rs`: exact stale-base selection using the existing sealed planner and authorization gate; descriptive refresh reason is bound into the plan.
- `src/sync.rs`: bounded refresh invocation and explicit refresh/fresh-CI reporting.
- `src/sync/plan.rs`: matching zero-write preview tagged `repair: stale_base`.
- `src/sync/tests/stale_base_refresh.rs` and `src/sync/tests.rs`: seven new regression cases and registration. Main's existing tests are preserved.
- `README.md`: document stale-base refresh and correct the contradictory statement that ordinary sync never uses native rebase.
- Reflection was completed earlier in this claimed-bead lifecycle: Caravan `bd-a743fa` covers the missing full-transaction provider seam; Cacophony `bd-b908d7` and `bd-ff952d` capture the observed same-PR tooling and recovery-diagnostic gaps. No additional beads were filed during this continuation.

## Operator-takeaway

Healthy Stack topology does not prove that its root has seen current main. This change repairs that exact stale-content case through the existing fenced transaction, without treating stale metadata or old green CI as landing authority. The source is now rebased and validated with the repository's pinned toolchain; fresh-generation queue submission replaces the closed, unjoined PR #285 without replaying the old recovery attempt.
