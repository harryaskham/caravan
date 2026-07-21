# Session summary — independent exact-candidate execution budgets

## Goal / bead

- `bd-33bb85` — prevent large fleet discovery and prior automatic-admission scans from leaving only milliseconds for a selected PR's required provider/Git preflight.
- Canonical ownership confirmed by Cacophony controller; duplicate `bd-818a25` stopped with no code changes.
- Live Cacophony PRs were not mutated.

## After state

- `status_for_remote_candidate_with_deadline` uses the caller deadline only for complete fleet discovery, then creates a fresh `command_timeout_secs` deadline for the selected provider PR.
- The candidate binder can consume an already-discovered `StatusOutput`; sync-owned automatic admission no longer repeats unrelated fleet/cross-caravan compatibility before selected-PR refetch.
- Candidate binding re-reads the exact PR, compares every operation-shaping fact, resolves exact merge-candidate identity, and preserves stale head/base/repository/labels/auto-merge refusal.
- Direct `check/new/join/rejoin --pr` use the fresh candidate deadline for provider refetch, compatibility checker, physical rebase, provider mutations, and exact post-rewrite rediscovery.
- After a rewrite, membership performs provider rediscovery under another bounded budget and rebuilds checker/provider adapters against that new deadline, preventing final commands from inheriting an exhausted pre-rewrite clock.
- Sync auto-admission reserves up to 30 seconds of nonzero exact-Git budget before beginning another candidate. Below the reserve it returns `deadline_exhausted` continuation without a doomed fetch or mutation.
- Once auto-admission selects a candidate, it passes the existing fresh fleet snapshot to membership and permits bounded post-membership rediscovery even when the outer scan deadline expired.
- `AutoAdmissionOutput` and lock checkpoints expose `candidate_budget_reserved_ms` and `candidate_budget_remaining_ms`.
- Status timing adds `exact_candidate_provider_refetch` and `exact_candidate_merge_identity`; candidate refetch timeout is typed as `candidate_refetch_timeout`, with exact phase/deadline and `mutated=false`.
- README, SPEC, and embedded help document the split discovery/exact-candidate budgets and continuation contract.

## Validation

- Added a 40-candidate fixture with only five seconds remaining: auto-admission starts no candidate, records 30,000ms reserve, preserves all 40 candidates, performs no provider mutation, and returns resumable deadline continuation.
- Full post-membership-refactor composition: 302 library + 12 binary + 10 CLI + 3 parity tests green.
- Strict all-target/all-feature Clippy and rustfmt green.
- Nix flake checks green before final main rebase; full Rust suite rerun green after `bd-cdc14e` membership split landed.

## Commit

- Rebasing implementation commit: `34e418e`.
