# Session summary — Native Stack prefix priority

## Goal

Prevent a blocked descendant's physical replay from starving an independently ready native GitHub Stack prefix during Cara sync.

## Bead(s)

- `bd-bbf2a3` — Land ready Stack prefix before repairing blocked descendants

## Before state

- A sync entered physical candidate replay before native Stack landing.
- A conflict in a later descendant could abort the tick before a green, mergeable prefix reached the existing locked Stack transaction.

## After state

- Native Stack convergence gets a bounded priority pass before physical descendant replay.
- Any landing checkpoint or preparatory provider mutation returns immediately with durable receipts; descendant replay resumes on a later tick.
- The regression proves a two-member ready prefix remains selectable while a conflicting suffix is preserved with exact blocker evidence.

## Diff summary

- Code/content commit: `7236b30`
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA
- Files touched: `src/sync.rs`, `src/github/stack_merge.rs`
- Tests: +1 regression
- Validation: targeted native Stack tests passed; `cargo check --tests` and `cargo clippy --lib --tests -- -D warnings` passed; `git diff --check` clean.
- Behavioural delta: ready native Stack prefixes now advance before repair planning can encounter a later conflict.

## Operator-takeaway

Stack landing and descendant repair are now separate bounded phases: a bad suffix can remain blocked with evidence, but it cannot hold a green prefix hostage.
