# Session summary — typed failure outcomes with process-only refusals flagged

## Goal

Make every Cara queue failure visible as one stable typed outcome, and flag the refusals where provider truth shows the PR published, enrolled or merged anyway, so bookkeeping refusals stop looking like broken work. Operators and Cacophony get counts, recent examples, the exact refusing field or step, and the tracking bug.

## Bead(s)

- `bd-7e0aa1` — Monitor Cara failures and flag receipt/process-only refusals
- (parent: `bd-acf9ca` — Make Cara admission and merge succeed reliably: provider truth over receipt ceremony)

## Before state

- Failing tests: none.
- Journalled SyncFailed and JoinFailed events carried only an error code and free text. Nothing distinguished a receipt refusal on an enrolled PR from a real failure; on 2026-10-20 Caravan PRs #268, #269 and #270 were all dead-lettered by Caco after Cara enrolled them, and all three merged.

## After state

- Failing tests: none (`cargo test --all`, clippy `-D warnings`, fmt clean).
- `cara status --json` adds `failure_outcomes`: counts per class and per `surface/class/code[/step]`, plus the ten newest examples, with `process_only` set from the same status's provider facts.
- `cara failures` (journal only, zero provider cost) and `cara failure-classify --error-file [--pr --expected-head]`, plus MCP tools. `failure-classify` uses `membership-proof` to set `process_only`.
- Classes: provider_error, budget_refusal, timeout, conflict_or_test_failure, process_refusal and unknown. Each outcome gives the refusing step, retryability, the tracking bug for known modes, or `needs_bug` for an unknown process refusal. Only allowlisted identifiers are emitted.

## Diff summary

- Code/content commits: 8a6f0a3 (final landed squash SHA comes from the reintegration receipt).
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: src/failure_outcome.rs (new), src/failure_outcome/tests.rs (new), src/read.rs, src/lib.rs, src/main.rs, docs/receipts/README.md, tests/fixtures/failures/* (10 envelopes).
- Tests: +7 covering every class, including PR #268's real force_intent dead letter (process-only once enrolled), PR #270's native-stack handoff refusal (a new class that needs a bug), the App probe timeout, GraphQL rate limit and internal error, PR4575's readiness refusal, the 10-12 stack recovery starvation, and the membership rollback.
- Behavioural delta: additive status field and two read-only commands.

## Operator-takeaway

A new process-only class surfaced today. Caco refused PR #270's native-stack handoff because its parent PR #269 merged mid-publication, although the PR was retargeted, enrolled and merged. The classifier marks it `needs_bug`; Cacophony bd-a76eaa is tracking it.
