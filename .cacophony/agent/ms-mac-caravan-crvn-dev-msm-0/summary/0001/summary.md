# Session summary — Canonical Caravan model and config seam

## Goal

Define a stable, GitHub-I/O-free contract for the three parallel v1 lanes: exact repository/PR facts, graph and compatibility evidence, optimistic mutation receipts, agent decision points, hook events, and strict repository policy.

## Bead(s)

- `bd-304e50` — Define Caravan domain model and strict repository config.
- (parent: `bd-caab31` — Implement Caravan v1 agent-in-the-loop merge queue.)

## Before state

- Failing tests: none.
- Relevant metrics: 9 foundation tests; domain commands intentionally returned `not_implemented`; `AppContext` carried only an optional config path.
- Context: discovery and compatibility workers needed canonical types before integrating their independently developed adapters.

## After state

- Failing tests: none.
- Relevant metrics: 21 tests pass; clippy is warning-free; `nix flake check` builds and tests the pinned sandbox package.
- Context: `src/model.rs` now carries exact repository/branch/PR/check snapshots, rolling chains/fleets, compatibility reports, preconditions, UUID operation/event IDs, mutation receipts, decision points, and hook metadata. `src/config.rs` strictly parses and validates versioned loop/force/hook policy, and MCP `AppContext` holds the resolved policy.

## Diff summary

- Code/content commits: `f91b9a2`.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `src/model.rs`, `src/config.rs`, `src/lib.rs`, `src/main.rs`, `Cargo.toml`, `Cargo.lock`.
- Tests: +12 / -0 / flipped 0 (21 total).
- Behavioural delta: the command stubs remain honest, but every downstream layer now has stable serialized types and MCP sessions reject malformed/unknown config before serving tools.

## Operator-takeaway

The three agents can now implement discovery, compatibility, and later graph/mutation policy against one explicit fact/receipt vocabulary, reducing merge conflicts and preventing provider-null or unknown states from being silently treated as success.
