# Inspecting interrupted native Stack syncs

Every mutating sync allocates and durably saves a Cara operation ID before any
provider write. It emits one complete JSON startup record to **stderr**, including
`phase: sync_started`, `operation_id`, and `receipt_command`. Stdout retains its
single CLI JSON result envelope. A scheduler must spool stderr as well as stdout;
Cacophony's merged cron spool and structured `/operation_id` extractor already
support this record. The startup record is identity, not proof of success.

Normal JSON success and failure envelopes also expose `operation_id` and
`receipt_command` at the top level. Failure summaries include both the ID and
inspection command. A killed process cannot emit a final result: use its startup
record, not a later successful tick, to identify that interrupted invocation.

## Read the receipt without changing the queue

From the same repository (or any of its linked worktrees):

```sh
cara operation --json
cara operation --operation-id <sync-id> --json
cara operation --operation-id '<sync-id>:stack:42' --json
```

The first command lists saved operations. Use it if the caller lost all output.
`--repository /path/to/checkout` selects a different checkout explicitly. The MCP
tool is `operation`, with optional `operation_id`. These reads acquire no writer
lock, contact no provider, and perform no recovery. The CLI read remains available
with invalid repository policy or an unreadable/dead writer lock.

Receipts live in the repository's **Git common directory**, under
`caravan/native-stack/`, not in source control or an authoritative temporary
worktree. They survive ordinary failure, SIGKILL, dead-owner lock cleanup, and
retirement of the active landing cursor. Atomic rename, file sync, and directory
sync establish durability before the next provider step. Preserve this Git
metadata when moving or removing a checkout; an independent clone does not share
these local receipts. No cron log truncation can truncate the stored receipt.

## What the receipt establishes

- `operations[].operation_id` is Cara's durable sync identity, **not** the
  optional GitHub asynchronous merge UUID.
- `state: unfinished` means the invocation did not save its final result. It
  may still be running; do not infer owner death or acquire another writer.
- `writes[].state: may_have_run` is saved immediately before spawning a mutating
  subprocess. A missing return receipt is uncertain, not permission to replay.
  `not_started` proves spawn failed. `returned` records the subprocess exit code,
  not the provider postcondition. Credentials, stdin, raw output, and argument
  bodies are not stored in this generic ledger; arguments have a digest.
- `failed_before_provider_write` means this invocation failed without launching
  any provider/remote-ref write. `failed` alone does not establish whether writes
  applied. `completed` means the tick returned, not that every pending merge landed.
- `native_merges[]` retains the exact sealed plan, repository/Stack identity,
  ordered PRs and heads, selected prefix and complete suffix, merge method/action,
  intended request method/path/top-SHA lease, branch-lock evidence, available
  GitHub request ID/UUID, and terminal observations. Request intent can precede
  submission; inspect the checkpoint phase and write markers too.
- `native_merges[].outcome` is `not_attempted`, `applied`, `not_applied`, or
  `unknown`. It describes the merge, not preceding lock/comment writes. Exact
  terminal observations remain in `checkpoint.terminal_receipt`; no later sync's
  success substitutes for them.

## Reconciliation belongs to the existing writer

The existing authorised scheduler's next normal sync reconciles the saved
checkpoint before new native landing work. Do **not** launch an extra sync, reset
cron, delete checkpoints/locks, or issue another merge to investigate an unknown
result. Reading a receipt grants no mutation authority.

A saved `submitting` marker without a response is never blindly resubmitted.
The ordinary landing state machine preserves the indeterminate outcome and its
existing lock-release contract. A saved UUID is polled read-only. Once release
has completed, reconciliation does **not** require the deliberately deleted lock
again; active, unreleased polling still verifies its lock.

- Exact merged head/commit/order/suffix provenance resolves to `applied`.
- A terminal failed UUID plus provider observations proving no selected entry
  merged resolves to `not_applied`.
- Pending status, incomplete or changed generations, partial results, and read
  failures remain `unknown` with available evidence.
- Without a UUID, exact merged provenance may prove application. Merely seeing
  open PRs cannot prove rejection: the unidentified request could still complete.
  Keep it unknown and do not replay it.

Later reconciliation updates the receipt under the **original native operation
ID**. A new sync ID may also reference it through `native_operations`. Retiring
`land-<stack>.json` from scheduling never deletes its archived operation receipt.

## Regression coverage

`sync::tests::interrupted_receipt` kills a real child after durable pre-submit
and write-attempt markers, then reads the receipt across dead-owner replacement
and subsequent reconciliation. It also exercises the production subprocess
write-ahead seam, preflight/spawn failures, and persistence failure before spawn.
The native adapter fixtures prove applied/not-applied/pending UUID reconciliation
without a deleted-lock read, and unidentified-response reconciliation without a
second PUT. CLI tests exercise inspection with invalid policy and a dead lock;
JSON summary tests bind failure output to the durable operation ID.
