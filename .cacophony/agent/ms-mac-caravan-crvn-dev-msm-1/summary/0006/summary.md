# Session summary — SPEC / CLI / JSON / MCP v1 parity

## Goal

Execute `bd-af5c3d`: independently enumerate the normative `SPEC.md` command matrix against the human CLI, stable `--json` envelopes, and MCP registry/handlers; implement bounded non-live gaps and leave live fixtures exclusively to `ms-mac-caravan-crvn-dev-msm-2`.

## Land-ready generation

- Rebases cleanly over `origin/main@d34d063` after preserving the singular CI-event fix and main's rustix-based process probe.
- Code commit: `30fd8e2` before reintegration.

## Changes

- Added `docs/v1-parity.md`, a checked SPEC-to-human/JSON/MCP matrix with explicit live-only acceptance ownership.
- Added `tests/v1_parity.rs`:
  - invokes every bounded v1 CLI domain command in an isolated non-repository and requires a versioned JSON success/error envelope;
  - invokes every bounded MCP domain handler directly and rejects any `not_implemented` route;
  - requires every MCP tool to expose a description, input schema, and output envelope schema;
  - asserts the foreground `loop` remains intentionally absent from MCP.
- Removed all dead foundation `not_implemented`, `OperationOutput`, and scaffold code.
- Fixed config read/parse/validation failures so `--json` returns a structured config-error envelope on stdout rather than human-only stderr.
- Replaced ecosystem registrar calls locally with output-schema-aware registrations for self-update and feedback tools; aligned CLI feedback status/report with the same typed `FeedbackStatus` / `ReportReceipt` outputs.
- Expanded MCP descriptions with preconditions, side effects, decision behavior, and safe recovery.
- Added canonical `join_failed` and `eviction_failed` events to typed failures after live discovery, including operation identity, fleet/PR context, error code, and configured hook delivery evidence.
- Implemented SPEC's safe affected-PR checkout behavior for `head_conflict`, `link_conflict`, and `ci_failure`: rediscover exact facts, require clean Git state, and check out exact OID. If unsafe, preserve the original decision and attach typed `checkout=skipped` evidence instead of replacing it. Cross-caravan/ambiguous decisions never guess a checkout.
- Updated README from transitional implementation language to the checked v1 surface.

## Validation

Post-rebase gates on `d34d063`:

- 115 library tests passed.
- 4 binary tests passed.
- 5 CLI exit/envelope tests passed.
- 3 v1 parity integration tests passed.
- Doc tests passed.
- `cargo clippy --all-targets -- -D warnings` passed.
- `nix flake check` passed in the sandbox.

## Coordination / live boundary

- `msm-2` owns `bd-322e38` and all live PR fixtures. It has live-green receipts for status/check, membership, navigation, reshape, renew/rejoin/split, fleet navigation, sync/head advancement, loop/hooks, and exact CI event IDs.
- Live PR2 force and affected-checkout acceptance is waiting only on this generation landing; send the canonical SHA immediately after reintegration.
- No live fixture was mutated by this audit.

## Follow-up drafts

- `updatable-cli:bd-29bbfa`: make `register_update_tool` publish output schemas.
- `feedback-cli:bd-35fab5`: make `register_feedback_tools` publish output schemas.

These avoid leaving Caravan's local registrar duplication unexplained.
