# Session summary — explicit admission intent and local release backfill

## Goal / bead

- `bd-c1799b` (P0) — Cacophony workers were wedged because `cara check --pr N` rejected every PR that was not the canonical FIFO first attempt.
- Operator also asked for Cacophony-style local release recipes so GitHub releases can be populated when runners are down.

## Live evidence

- Cacophony fleet had zero caravans, `next_candidate=2113`, and unqueued `2113, 2115, 2117, 2119, 2122, 2141, 2147, 2179...`.
- `cara check --pr 2183` returned `eligible=false`, `next_action=reject`, problem `candidate is not canonical first admission attempt; fail closed on PR #2113`.
- Cacophony `pr_cara_join` refuses any non `new`/`join` action, so one unadmitted older PR starved the whole fleet.
- The operator's assumption that recent Cara already preferred join intent was **incorrect**: 0.0.8 and main both rejected.

## Change

- Priority/FIFO ordering now binds **automatic** selection only.
- An explicit `--pr` request is deliberate admission intent: it is admitted on that candidate's own exact eligibility, compatibility, freshness, and generation integrity.
- Canonical position is reported as non-blocking evidence: `canonical_candidate` plus a new optional `admission_note`.
- Removed the order-based rejection from candidate action selection; every other guard is unchanged.
- Sync-owned automatic admission keeps strict priority-then-FIFO with generation-bound skip receipts, and already used `pr: None` preflight, so it is unaffected.
- Added `just release-backfill`, `release-backfill-target`, and `release-backfill-all`: tagged detached worktree build, CI-identical `package-release.sh` assets, smoke, and upload for `x86_64-linux`, `aarch64-linux`, and `aarch64-darwin`. They never create or move tags.

## Regression proof

- Explicit non-canonical `--pr` now returns `eligible=true`, `next_action=new`, `canonical_candidate=false`, and ordering evidence naming both PRs.
- An ineligible non-canonical candidate (draft) still fails closed.

## Validation

- Library suite at 16 threads: 380/380 green.
- v1 parity and CLI exit suites green.
- Strict all-target/all-feature Clippy and `git diff --check` green.
- `just` parses and lists the new recipes.

## Commit

- `1d91006`.
