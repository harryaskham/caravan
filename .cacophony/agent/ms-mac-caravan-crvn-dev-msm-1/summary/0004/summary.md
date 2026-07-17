# Session summary — Idempotent sync and live rolling-head advancement

## Goal

Implement `bd-1597d4`: deterministic `cara sync`/`sync --all`, merged-head reconciliation, exact optimistic receipts, structured decision points, interruption recovery, and GitHub-state-only rerun semantics. Prove the transition on the live Caravan fixture chain and restore the operator-directed self-hosted Nix CI selector for the release milestone (`bd-90da08`).

## Bead(s)

- `bd-1597d4` — Implement idempotent caravan sync and rolling head advancement
- `bd-90da08` — Restore self-hosted Nix CI after dogfood bootstrap
- Related acceptance: `bd-322e38`

## Before state

- `cara sync` returned structured `not_implemented`.
- Live state was healthy caravan `#1: [1,2,3]`, with PR #1 targeting `main` and squash auto-merge enabled; #2 targeted #1; #3 targeted #2.
- The first real no-op tick had not been exercised, merged-head advancement did not exist, and all required checks were queued because the intended architecture-labelled runner fleet had not yet registered.

## After state

- `cara sync` and `sync --all` are wired through CLI and MCP with repository operation locking, deterministic caravan selection, graph/fleet preflight, rolling-head detection, non-head-before-head auto-merge repair ordering, fresh post-mutation rediscovery, exact provider receipts, and structured decisions for graph/compatibility/stale/timeout failures.
- Nine focused sync tests cover healthy repeat no-op, merged-head advancement, interrupted resume, deterministic all ordering, stale races, head/link conflicts, non-head auto-merge repair order, and timeout evidence.
- Full gate: 83 library tests + 4 binary tests + 3 CLI integration tests pass; warning-denied all-target Clippy passes; `nix flake check` passes on the local aarch64-darwin system.
- Live PR #1 merged through a real successful `build-test`. Pre-sync status correctly identified `dangling_base [2,1]` and `auto_merge_invariant [2]`. Real `cara sync --all` retargeted #2 to `main@653099e`, enabled squash auto-merge on #2, kept #3 auto-merge off, returned exact before/after receipts, and reported rolling ID `1 -> 2`. Fresh status was healthy `#2: [2,3]`; immediate rerun was `changed=false` with zero provider writes.
- The temporary hosted bootstrap selector is removed; `.github/workflows/ci.yml` again targets `[self-hosted, azure-ephemeral]` for msm-0’s v0.0.1 tag milestone, as directed by the operator.

## Diff summary

- Implementation commit before final reintegration: `8e37307` (canonical landed SHA recorded by reintegration).
- Files: `.github/workflows/ci.yml`, `src/lib.rs`, `src/main.rs`, `src/sync.rs`, `tests/cli_exit.rs`.
- New behaviour: sync converges or stops at the first typed decision with exact revisions, affected PRs, compatibility evidence, completed steps, provider receipts, suggested actions, and `resumable: true`.
- Live evidence is appended to canonical dogfood bead `bd-322e38`.
- Reflection follow-up: draft `bd-6d92bd` tracks splitting the 1,195-line sync module after v1 policy stabilizes.

## Operator-takeaway

Caravan has now advanced its own real queue head from merged PR #1 to PR #2 without rebasing history or using raw GitHub mutation commands, and an immediate repeat proved the operation is idempotent. This is the requested self-hosted CI release milestone for msm-0’s v0.0.1 tag.
