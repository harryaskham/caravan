# Session summary — Make cara dogfood failures shell-trustworthy

## Goal

Start the operator-directed self-host dogfood loop on the Caravan repository and repair the first concrete contract failure found when invoking `cara status` as an agent.

## Bead(s)

- `bd-35c608` — Return nonzero exit status for JSON error envelopes.
- `bd-322e38` — Dogfood cara against the Caravan repository until v1 works end to end (ongoing acceptance track).
- Parent: `bd-caab31` — Implement Caravan v1 agent-in-the-loop merge queue.

## Before state

- `nix develop --command cargo run --quiet -- --json status` emitted a structured `not_implemented` error envelope but exited with status 0.
- Human-mode errors already exited nonzero, so JSON automation could silently treat an unresolved Caravan decision as success.
- `cara status` itself remains a domain stub; implementation is tracked by the now-unblocked graph/read bead `bd-c11c04`.

## After state

- JSON mode records whether the domain result failed, writes the same stable envelope, and then returns exit status 1 for the error path.
- Human error mode remains nonzero and JSON success mode remains zero.
- The same live `cara --json status` dogfood invocation now returns the expected `not_implemented` envelope with process exit 1.

## Diff summary

- Code/content commit: `52c7ee1`.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `src/main.rs`, `tests/cli_exit.rs`.
- Tests: +3 process-level CLI tests for JSON error, human error, and JSON success exits; targeted warning-denied clippy is green.
- Behavioural delta: agents and shell automation can now trust the process status without losing the machine-readable error continuation contract.

## Operator-takeaway

Caravan is not yet functionally complete—live status discovery is still stubbed—but its first self-host failure now fails honestly at the process boundary, so every subsequent dogfood step can use exit status as a reliable automation signal.
