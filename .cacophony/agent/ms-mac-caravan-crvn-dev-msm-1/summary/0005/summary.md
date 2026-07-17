# Session summary — CI decisions, exact reruns, and force merge

## Goal

Implement `bd-66cb11`: extend landed sync with deterministic CI policy, typed `ci_failure` decisions, exact failed-run reruns, `caravan-force`, permission-gated one-shot admin squash, and canonical audit event surfaces for the parallel hooks lane.

## Before state

- Sync repaired graph/base/auto-merge invariants but did not interpret checks.
- `GitHubMutationAdapter` had low-level failed-run, rerun, and admin-squash primitives, but sync did not select or gate them.
- Live caravan #2 `[2,3]` had queued self-hosted `build-test` checks and no CI disposition in sync output.

## After state

- `SyncOutput` reports deterministic head-to-tail `CiObservation` rows and serde-default `events: Vec<CaravanEvent>`.
- Pending/expected/running or absent checks report `waiting`; success/neutral/skipped pass; failure/cancelled/timed-out/action-required/unknown stop at the first unforced typed `ci_failure`.
- CI decisions preserve exact checks, provider values, all exact-head failed workflow runs, only URL-correlated rerunnable run IDs, PR/default/fleet facts, completed steps, and one canonical `ci_failed` event.
- `cara sync --rerun-failed` reruns only listed failed runs, then returns the still-unresolved decision with exact provider receipts. Before mutation, the adapter verifies fresh PR preconditions, current PR association from the workflow-run API, exact head SHA, and failure conclusion.
- `caravan-force` failures remain in-chain. A forced head admin-squashes only when `force_merge=true`, it remains open/non-draft/labelled, exact head→current-default compatibility is clean, the default branch still has the proven OID immediately before mutation, and GitHub reports ADMIN permission.
- Force merge is one-shot per tick. It emits `force_merge_attempted` and `force_merge_completed` with the operation ID, then normally advances at most one child; a forced child waits for the next fresh tick. Textual conflicts, pending mixed checks, stale PR/default facts, missing config, or denied permissions fail closed.
- Normal and exceptional head advancement now emits canonical `head_advanced` events for hook consumption.

## Validation

- Final implementation commit before reintegration: `1c65004`.
- Full gate: 97 library tests, 4 binary tests, 3 CLI integration tests, strict all-target Clippy, and `nix flake check` pass.
- Hermetic coverage includes pending, unforced/unknown failures, spurious old runs, exact rerun selection, wrong-PR run rejection, downstream force, mixed pending+failure, config/permission denial, stale PR/default revisions, forced textual conflict, successful one-shot admin squash, child advancement, and event identity.
- Live `sync --all` and `sync --all --rerun-failed` on caravan #2 both returned `changed=false`, zero provider receipts, exact queued check URLs, CI `waiting` for #2/#3, no audit events, #2 sole auto-merge head, and #3 auto-merge off.
- Live terminal failure/rerun/admin-force mutation remains gated by repository infrastructure: GitHub currently reports zero registered self-hosted runners. This is an operator-known environment condition, not a code blocker; evidence is appended to `bd-322e38`.

## Coordination and reflection

- Parallel `bd-7691a8` owner agreed to consume `SyncOutput.events` through a policy-free dispatcher and did not edit `sync.rs`; send the canonical landed SHA so they can rebase.
- Existing draft `bd-6d92bd` was updated to cover splitting the now-larger sync CI/policy/fixture module; no duplicate refactor bead was filed.
