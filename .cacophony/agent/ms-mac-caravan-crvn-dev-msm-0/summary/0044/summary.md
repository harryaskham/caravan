# Session summary — check-progress-safe sync mutation identity

## Goal / bead

- `bd-24840c` — stop ordinary CI/check progression from producing false `stale_precondition: checks` during unrelated sync writes.
- Live reproduction affected PRs #2086/#2102 with zero completed steps while a check moved queued→running.

## After state

- `PullRequestPrecondition::mutation_identity_eq` defines the exact authority for topology/base/label/comment/auto-merge writes: PR number/state, head OID, base ref/OID, labels, and auto-merge.
- Checks remain in snapshots and receipts but are excluded from ordinary GitHub mutation stale-field detection.
- Base edits, label changes, control/audit comments, and auto-merge changes can proceed when only checks changed.
- Real state/head/base/label/auto-merge drift remains typed `stale_precondition` and zero-write.
- `verify_precondition_with_checks` preserves strict observed-check identity for CI-specific operations.
- Failed-run selection, diagnostics and rerun paths use strict verification and continue to bind PR, exact head, checks and run identity.
- Pause resume performs an explicit strict check precondition immediately before re-enabling auto-merge, then uses ordinary mutation identity for the write.
- Sync and membership fake providers now mirror production mutation identity, preventing fixtures from hiding or inventing check-churn failures.

## Regression proof

- Provider base edit succeeds when expected SUCCESS changed to IN_PROGRESS, while exact head/base/labels/auto-merge remain; receipt captures current checks.
- Strict CI verifier rejects the same transition with `changed_fields=[checks]`.
- Sync auto-merge repair succeeds across queued→in-progress check churn and preserves exact head/base/labels.
- Existing real label-drift sync fixture still returns resumable stale precondition.

## Documentation and validation

- README/SPEC distinguish observation checks from mutation authority and strict CI/run operations.
- Focused provider/sync fixtures green.
- Strict all-target/all-feature Clippy/rustfmt green.
- Complete CLI-exit and v1 parity suites green.
- Hosted required CI remains the broad delivery gate.

## Commit

- `0f0e459`.
