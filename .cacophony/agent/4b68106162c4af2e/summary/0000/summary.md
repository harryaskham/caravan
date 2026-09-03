# Session summary — Isolate malformed native stacks

## Goal

Keep `cara sync --all` available when one provider-native Stack has deterministic membership drift: quarantine only that topology, preserve its evidence, and continue unrelated healthy caravans without mutating the ambiguous Stack.

## Bead(s)

- `bd-90d539` — Isolate malformed native stacks during sync-all

## Before state

- A pure `github_stack_member_order_drift` made the repository-wide native backend health gate abort every sync tick.
- Automatic native rebase treated member-order drift as self-healing even when the provider Stack was not a proven active Caravan prefix.
- Independent admitted work could not reach convergence.

## After state

- Pure, fully mapped member-order drift is classified as a per-caravan zero-write quarantine; mixed, orphaned, truncated, or otherwise ambiguous backend failures still fail closed.
- `sync --all` removes only quarantined caravan IDs from selection and continues independent caravans.
- Quarantine operation evidence records provider Stack order and heads, logical member generations, zero-write/non-retryable classification, repair route, and rollback preservation.
- Explicit native rebase preview/apply can seal a representation-only membership repair; applying the exact reviewed plan rebuilds provider representation without rewriting source heads.

## Diff summary

- Code/content commit: `eda292c`
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA
- Files touched: `src/sync.rs`, `src/native_stack_rebase.rs`, `src/sync/tests.rs`
- Tests: added one regression test covering a malformed Stack plus an independent healthy caravan; focused native rebase tests remain green.
- Validation: `cargo clippy --lib --tests -- -D warnings` passed; focused isolation and native rebase test groups passed.
- Behavioural delta: deterministic membership drift no longer returns a repository-wide retry loop or blocks unrelated queue progress.

## Operator-takeaway

A malformed native Stack is now a bounded topology incident rather than a fleet-wide queue outage: Cara leaves it byte-for-byte untouched, emits exact repair evidence, and keeps independent work moving.
