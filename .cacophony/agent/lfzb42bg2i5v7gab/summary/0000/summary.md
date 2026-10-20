# Session summary — typed GitHub App probe refusals with a configured deadline

## Goal

Stop explicit GitHub App credential resolution from dead-lettering completions on a fixed 5 s probe bound and an erased error. The broker and exact-repository probe now honour the caller's configured command timeout, retry transient timeouts and gateway failures inside the operation deadline, and report a typed, secret-free stage and category that callers can act on.

## Bead(s)

- `bd-096b1e` — Preserve redacted runner-error categories in GitHub App broker refusals
- (parent: `bd-acf9ca` — Make Cara admission and merge succeed reliably: provider truth over receipt ceremony)

## Before state

- Failing tests: none.
- The App broker and probe ran under a hardcoded 5 s bound regardless of `command_timeout_secs`; the deterministic repro (5.5 s response, `command_timeout_secs=10`) failed at about 5.4 s.
- Every broker runner error became "credential broker could not be executed" and every probe runner error became a generic transport failure; discovery errors carried a Debug dump.

## After state

- Failing tests: none (`cargo test --all`, clippy `-D warnings`, fmt clean).
- Broker and probe use the caller's configured timeout and operation deadline. Ambient multi-account discovery keeps its 5 s per-child bound.
- `GithubAppRefusal` (stage, category, attempts, timeout_ms, retryable) flows through `CommandRunError`, `provider_api` telemetry and `github_discovery_failed` details; the error code is unchanged for existing consumers.
- Started-child timeouts and HTTP 502/503/504/reset probe failures retry up to three attempts with backoff only while another attempt fits the deadline; grant, token and configuration verdicts never retry.
- GitHub's generic GraphQL "Something went wrong while executing your query" joins the narrow one-retry read classifier.

## Diff summary

- Code/content commits: 05e14f5 (final landed squash SHA comes from the reintegration receipt).
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: src/command.rs, src/read.rs, src/github.rs, src/model.rs, docs/github-app.md.
- Tests: +8 (configured-timeout repro, every runner category with leak canaries, broker and probe retry-then-success, exhausted timeout, no-retry verdicts, deadline guard, discovery error mapping); 3 updated.
- Behavioural delta: a slow but healthy App broker or provider no longer fails a completion, and a real refusal names its stage and category without exposing argv, environment, output or tokens.

## Operator-takeaway

`spawn_or_io` deliberately stays non-retryable and is not proof the broker never ran. Historical dead letters keep their erased generic message; this change does not reconstruct or replay them. Cacophony can key retries on `details.github_app_refusal.retryable` once it adopts the release.
