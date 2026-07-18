# Session summary — Stabilize shared operation-deadline test

## Goal

Remove the loaded-host scheduling race from the shared operation-deadline regression while preserving its proof that one absolute budget spans phases, the first phase consumes that budget, and the hung second child is terminated and reaped.

## Bead(s)

- `bd-05bedb` — Fix flaky `one_absolute_deadline_bounds_multiple_phases_and_reaps_the_hung_phase` test.

## Before state

- The test gave the whole two-command operation one second, then required a newly spawned shell running the first 50-millisecond phase to complete inside that budget.
- Under full-suite load, the first child could remain unscheduled until the approximately 988-millisecond remaining budget expired, so the test failed before reaching the behavior it intended to check; an immediate isolated rerun passed.
- Production per-command timeout and shared-deadline code had no observed failure.

## After state

- The test uses a three-second operation budget while retaining a larger five-second per-child timeout, so the operation deadline remains authoritative.
- It still proves the first 50-millisecond phase reduces the second child's reported budget, the total operation finishes before four seconds, the second command times out, and its bounded output is preserved when emitted.
- All eight command tests passed, 32 parallel exact test processes passed, and strict all-target/all-feature Clippy passed.
- Production `ProcessRunner` behavior is unchanged.

## Diff summary

- Code/content receipt: `bb3bc22425e003ef36021a65e42fd5c43dab2166`; final landed squash SHA will come from the reintegration receipt.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `src/command.rs`.
- Tests: no assertions removed; the operation budget changed from one to three seconds, the wall bound from two to four seconds, and the remaining-budget assertion still subtracts the completed 50-millisecond phase.
- Behavioural delta: test infrastructure no longer treats one-second shell scheduling as part of the production contract.

## Operator-takeaway

This was the companion scheduling assumption to bd-dfea55, not a second production timeout bug. The regression remains strict about shared-deadline accounting and reap behavior while becoming robust under parallel suite pressure.
