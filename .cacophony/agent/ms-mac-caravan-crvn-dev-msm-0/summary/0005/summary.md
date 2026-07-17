# Session summary — Restore live Caravan CI capacity

## Goal

Unblock the required `build-test` check so the live Caravan head can auto-merge and exercise rolling-head sync naturally.

## Bead(s)

- `bd-9d816d` — Run Caravan CI on an available GitHub-hosted runner.
- (live acceptance: `bd-322e38` — Dogfood cara against the Caravan repository.)

## Before state

- Failing tests: none; CI never started.
- Relevant metrics: zero registered repository Actions runners; run `29600185649` queued unchanged since `2026-07-17T17:28:35Z`.
- Context: workflow required `[self-hosted, azure-ephemeral]` despite public dependencies and an explicit Rust toolchain install.

## After state

- Failing tests: none locally.
- Relevant metrics: workflow targets `ubuntu-latest`; format, clippy, test, and agent-surface smoke steps are unchanged.
- Context: once landed, a fresh PR run can produce the protected `build-test` context and allow PR #1's configured squash auto-merge to progress.

## Diff summary

- Code/content commits: `ef96cf2`.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `.github/workflows/ci.yml`.
- Tests: workflow-only change; source tests unchanged.
- Behavioural delta: required CI uses an available GitHub-hosted runner instead of an empty self-hosted label pool.

## Operator-takeaway

Caravan's own queue was correctly blocked by required CI, but the CI had no possible runner. This repair restores the intended check rather than bypassing protection.
