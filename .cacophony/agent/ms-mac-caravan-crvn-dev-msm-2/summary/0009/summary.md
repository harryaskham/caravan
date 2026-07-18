# Session summary — Remove scheduler race from timeout evidence test

## Goal

Stabilize the reopened Linux ProcessRunner timeout-evidence failure without weakening output assertions or changing production timeout, capture, or process-group termination behavior, then prove the narrow test-contract repair repeatedly on native Linux.

## Bead(s)

- `bd-dfea55` — Stabilize hook subprocess capture under parallel Linux Nix tests.

## Before state

- A full Linux suite under parallel load intermittently failed `hanging_child_is_terminated_reaped_and_reported_with_evidence`: the 100-millisecond deadline expired before the newly spawned shell was scheduled to emit `started`, so timeout stdout was legitimately empty while an immediate isolated rerun passed.
- The prior closed-stdin production fix remained green; the live recurrence did not modify or implicate stdin ownership, stream capture, process-group signalling, or hook code.
- Local macOS could not build a Linux flake target, and Helsinki's Cacophony API remained degraded after an expired restart even though direct SSH stayed healthy.

## After state

- The evidence test gives a fresh shell one second of startup budget, while retaining exact `started` stdout, diagnostic stderr, typed timeout duration, process-group reap, and total wall-clock assertions.
- Production `ProcessRunner`, timeout defaults, polling, capture limits, and termination logic are unchanged.
- The focused evidence test, the existing parallel timeout/closed-stdin stress test, and 128 parallel exact evidence-test processes passed on macOS.
- A fresh ephemeral clone on Helsinki Linux 6.18.38 x86_64 at main `0e2c0f0` applied only this patch and passed one normal Nix build plus two forced rebuilds. Both rebuilds ran 198 library, 4 binary, 7 CLI process, and 3 parity tests with zero failures.

## Diff summary

- Code/content receipt: original validated commit `11f27e18e1302cbb18901874ded45cd32643a342`; the final coherent landed squash SHA will come from the reintegration receipt.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `src/command.rs`.
- Tests: no assertions removed or relaxed; the timeout evidence test's shell-start budget changed from 100 milliseconds to one second with an explanatory contract comment.
- Native Linux validation: normal build passed; rebuild 1 passed; rebuild 2 passed; each executed from a disposable clone removed on completion.
- Behavioural delta: test infrastructure no longer assumes a loaded host must schedule a fresh shell within 100 milliseconds; shipping command behavior is byte-for-byte unchanged.

## Operator-takeaway

The reopened signal was not another ProcessRunner output-loss bug. It was a scheduler assumption in the test itself: a timeout cannot preserve bytes a child never got CPU time to emit. The repair preserves the strict evidence contract and proves it repeatedly on the exact Linux platform that exposed the flake.
