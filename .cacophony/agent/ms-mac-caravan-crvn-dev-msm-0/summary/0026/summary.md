# Session summary — GitHub API budget/auth/cache slice for v0.0.5

## Goal

Reduce authenticated GitHub API pressure and make provider budget visible before the dashboard and continuous sync optimiser widen Cara polling.

## Bead

- `bd-c51f62` — Budget and reduce authenticated GitHub API usage.

## Before state

- Cara already authenticated every `gh` subprocess via ambient tokens or a repository-accessible `gh auth` account, but each short process spent a redundant REST access probe for an explicit ambient token.
- Status/check/sync receipts did not expose provider call counts or GraphQL budget.
- The new dashboard performed full status refreshes without coalescing duplicate reads, and stable label inventory was fetched on every status pass.
- Post-v0.0.4 auditing exhausted the authenticated shared user quota; one live Cacophony status pass took roughly 12.8 seconds.

## After state

- ProcessRunner clones share secret-free GitHub telemetry: authenticated source class, total/GraphQL/REST/gh-CLI calls, cache hits/age, and latest GraphQL cost/remaining/reset.
- The existing merge-candidate GraphQL request collects `rateLimit` in-band without another request.
- Explicit ambient `GH_TOKEN`/`GITHUB_TOKEN` is trusted as the caller's choice and validated by the first real provider request, removing one redundant REST probe per short Cara process; gh-account fallback still probes to choose safely.
- Status, check, sync/loop status, JSON, MCP, human output, and timeout/error evidence carry provider telemetry. Remote candidate refetch telemetry is merged into the original status receipt.
- Long-lived polling surfaces have a bounded exact repository+config status cache; dashboard duplicate refreshes coalesce for five seconds and explicit manual refresh invalidates first. Stable repository label inventory is cached for ten minutes. Mutating domain operations retain fresh provider preflight and never consume the read cache as authority.
- Documentation covers authentication, GitHub App installation tokens, non-secret `CARA_GITHUB_AUTH_KIND`, event-driven single-controller operation, query budgeting, and the requirement that cache never be mutation authority.
- Live smoke against Cacophony succeeded and reported authenticated `gh_auth_account`, six provider calls (one GraphQL, one REST, four gh-CLI), GraphQL cost 1, remaining 4972, and reset timestamp.

## Validation

- 241 library tests, 7 binary tests, 7 CLI exit tests, and 3 parity tests passed.
- Strict all-target/all-feature Clippy and canonical rustfmt passed.
- `nix flake check --no-write-lock-file` passed; packaged `nix run . -- --version` reports 0.0.4 before the planned v0.0.5 cut.
- New tests cover call/rate extraction, exact status-cache hit/invalidation, and repository label-cache coalescing.

## Diff summary

- Implementation commit: `b171a3b`.
- Main base: dashboard foundation `7bd3a6392d27a06e612e51d88b89ca4b261d18c0`.
- Main surfaces: `src/command.rs`, `src/model.rs`, `src/read.rs`, `src/github.rs`, `src/web.rs`, typed output fixtures, README/SPEC/parity docs.

## Operator takeaway

A GitHub App is useful for a separate least-privilege installation bucket, but query reduction comes first. Cara now proves which authenticated path it used, how many provider calls a tick spent, what GraphQL budget remained, and when long-lived read polling reused bounded cache. v0.0.5 remains gated on the dashboard UI/mutation slices and greedy sync auto-admission.
