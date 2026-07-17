# Session summary — Safe eviction and caravan splitting

## Goal

Implement fully preflighted, resumable eviction and split operations over the landed membership/provider seams without disrupting the live three-PR chain before sync can exercise real head advancement.

## Bead(s)

- `bd-4ba31f` — Implement safe eviction and caravan splitting.
- (parent: `bd-caab31` — Implement Caravan v1 agent-in-the-loop merge queue.)
- (live acceptance: `bd-322e38` — Dogfood cara against the Caravan repository.)

## Before state

- Failing tests: none.
- Relevant metrics: `evict` and `split` returned `not_implemented`; live chain `#1 -> #2 -> #3` was healthy and reserved for sync/head advancement.
- Context: membership supplied exact optimistic mutation receipts and graph supplied compatibility validation, but reshape ordering/recovery did not exist.

## After state

- Failing tests: none.
- Relevant metrics: 75 tests pass; warning-denied clippy and `nix flake check` pass; head/middle/tail eviction, split, conflict refusal, and existing-head rejection are covered.
- Context: reshape first builds and mechanically validates the complete virtual final fleet. It then applies exact-precondition, resumable steps for auto-merge, active/force/evicted labels, child retargeting, and new-head promotion. Partial split/eviction failures return operation IDs, completed steps, and provider receipts.

## Diff summary

- Code/content commits: `eed1227`.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `src/reshape.rs`, `src/lib.rs`, `src/main.rs`.
- Tests: +6 / -0 / flipped 0.
- Behavioural delta: CLI and MCP `evict`/`split` now execute safe policy instead of stubs; live mutation is deliberately deferred until sync no longer needs the intact fixture chain.

## Operator-takeaway

Reshaping is implemented and sandbox-green, while live self-hosting also exposed unbounded Git/GitHub waits. That separate P1 is tracked at `bd-ccffdf`; no reshape mutation occurred during the timed-out preflight.
