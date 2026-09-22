# Rebase failure-generation evidence

`rebase_conflict`, `rebase_merge_replay_conflict`, and
`rebase_merge_tree_conflict` errors from physical preparation include
`details.failure_generation` (`RebaseFailureGeneration` in
`src/physical_rebase.rs`). See the serializable consumer fixture
[`tests/fixtures/rebase-failure-generation.json`](../tests/fixtures/rebase-failure-generation.json).

Version 1 captures the repository, PR, original head and provider base snapshots,
and attempted target before preparation. Target `kind: simulated` means a local
planned parent object, **not** an observed provider base. `kind: remote` identifies
the selected remote target snapshot. These historical facts must not be replaced
with later status or recovery-request observations.

`operation_id`, when supplied, is the existing Cara caller operation identity.
Physical sync supplies its progress operation ID. It is not the enclosing
scheduler's operation or event ID. Consumers bind the snapshot to the existing
authenticated configured-actor event; they must not fabricate an actor ID for
standalone errors. Existing manual decision IDs likewise remain in their enclosing
records. This object is evidence, not a signed receipt or a new identity service.

`mutation_scope: prepare_candidate` and
`provider_mutation_outcome: not_attempted` describe **only this preparation**,
which performs no remote/provider writes. They say nothing about earlier writes
in a sync or scheduler operation. Preserve enclosing receipts and partial or
indeterminate outcomes; an absent enclosing outcome is unknown, not no mutation.
Do not apply this preparation type to an apply/push failure.

A consumer may deduplicate the authenticated event plus failed generation and
resolve the same owner using its durable submission/source mapping, then wake that
owner to inspect fresh state. Provider and preserved source heads need not match.
This evidence never grants write authority, custody, a release token, or a
scheduler pause. Cara and the owner may continue concurrently. Every later write
still uses fresh exact leases and adapts to changed provider state. Replaying the
same error preserves historical evidence; it does not make that evidence current.
