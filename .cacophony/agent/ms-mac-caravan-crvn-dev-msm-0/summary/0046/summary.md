# Session summary — command deadline fixture load resilience

## Goal / bead

- `bd-dc4c56` — stop the Nix build from failing on host contention instead of real timeout regressions.

## Live failure

- `command::tests::one_absolute_deadline_bounds_multiple_phases_and_reaps_the_hung_phase` panicked with `first phase fits the budget: Timeout`.
- The fixture allowed the whole operation only 3 seconds and required a trivial `sh -c "sleep 0.05"` phase inside it, so parallel Nix scheduling alone could exhaust the budget before any real defect appeared.

## Change

- The deadline fixture now uses a 10s shared operation budget and an intentionally large 120s per-child timeout.
- The first phase is a pure `exit 0`, so only scheduling, not sleeping, consumes budget.
- Assertions now prove the intended contract directly: the hung phase inherits only the remaining shared budget, never the larger child timeout, and cannot regain the first phase's spent time.
- Reaping, typed timeout classification, and partial-stdout evidence remain asserted.
- The hanging-child fixture keeps a 5s child timeout with a 30s outer bound.
- The parallel-timeout stress fixture uses a 200 ms timeout so termination, not startup latency, is what is measured.

## Validation

- Exact reported fixture: green in 10.05s.
- Whole `command::` module at 16 threads: 15/15 green.
- Whole library suite at 16 threads: 373/373 green in 57s.
- Strict library Clippy and `git diff --check`: green.
- Broad parallel reruns were used deliberately as safety reproduction for a recurring load-sensitivity class.

## Commit

- `2e39401`.
