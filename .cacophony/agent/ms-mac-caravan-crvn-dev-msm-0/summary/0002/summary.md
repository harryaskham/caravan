# Session summary — Live graph and read-command self-hosting

## Goal

Replace Caravan's read-only skeleton with real GitHub discovery, graph derivation, compatibility validation, and agent-friendly status/show/check behavior, then prove it against the Caravan repository's live fixture PRs.

## Bead(s)

- `bd-c11c04` — Derive and validate caravan graphs; implement read-only commands.
- (parent: `bd-caab31` — Implement Caravan v1 agent-in-the-loop merge queue.)
- (live acceptance: `bd-322e38` — Dogfood cara against the Caravan repository.)

## Before state

- Failing tests: none in the dev shell; the package sandbox later exposed owned blocker `bd-7bddc3` because Git was absent from Nix test inputs.
- Relevant metrics: `cara status`, `show`, and `check` returned `not_implemented`; the live repository had three ready fixture PRs but cara could not see them.
- Context: discovery and compatibility adapters were landed independently, while graph policy and command wiring remained absent.

## After state

- Failing tests: none.
- Relevant metrics: 58 tests pass; clippy is warning-free; `nix flake check` passes; live human/JSON/MCP status reports healthy; fixture PRs `#1`, `#2`, and `#3` are discovered as ready and unqueued.
- Context: graph analysis derives rolling head-to-tail chains, diagnoses cycles/branching/dangling/evicted/fork/auto-merge violations, and checks head-to-main, adjacent, and ordered cross-caravan compatibility. `show` resolves position; `check` evaluates active health, new-caravan eligibility, or explicit tail/head joining without mutation.

## Diff summary

- Code/content commits: `9e23d37`, `9e207c5`, `574a60e`.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `src/graph.rs`, `src/read.rs`, `src/lib.rs`, `src/main.rs`, `src/model.rs`, `src/github.rs`, `tests/cli_exit.rs`.
- Tests: +14 / -0 / flipped 2 stale status-stub exit tests to the still-unimplemented sync surface.
- Behavioural delta: read operations now inspect real GitHub state and return concise human output plus stable JSON/MCP envelopes; unresolved check/show conditions exit nonzero.

## Operator-takeaway

Caravan is now genuinely self-observing rather than a skeleton: the live repository and its three fixture PRs are visible through cara. Membership remains the next required layer before those PRs can be formed into and advanced as a real caravan.
