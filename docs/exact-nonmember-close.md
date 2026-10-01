# Exact non-member close transaction

This source contract follows immutable v0.0.140; that release does not contain
these tools. Consumers must require `pr_close_apply` and `pr_close_status` with
schema version 1 from a containing build, never fall back to check-then-raw-close.

`cara pr-close apply` and MCP `pr_close_apply` close at most one exact,
already-represented, non-member pull request. They do not evict, dequeue, merge,
reshape a Stack, change labels or refs, delete a branch, or publish a replacement.
Cacophony remains responsible for authenticating the caller's owner, assignment,
generation and explicit close intent. `actor` and `custody_reference` are bounded,
non-secret audit references, **not credentials or grants**.

## Inputs and evidence

Supply the exact remote repository (`--expected-repository`; JSON `repository`),
PR, head ref/OID, base ref/OID, default-branch
ref/OID, representation commit, proof kind, actor, custody reference, reason,
operation key and `--confirmed`. All OIDs are full SHA-1 identities. The operation
re-reads these facts; caller evidence never substitutes for provider truth.

Representation has two explicit forms:

- `ancestor`: the source head is an ancestor of, or identical to, `represented_at`.
- `same-tree`: source and `represented_at` have identical complete Git trees.

In both cases `represented_at` must be contained in the exact expected default
branch. An earlier landed squash can therefore prove representation even after
main advances. A title, patch-id, similar diff, or caller assertion cannot prove
it. Unproven representation refuses without closing anything.

The transaction uses `AppContext::acquire_writer_operation`. Read-only policy
refuses; local-only policy serializes that Git common directory; remote-fenced
policy additionally uses the configured remote lease and fenced subprocess
runner. It rechecks policy, head/base/default identities and complete native
Stack non-membership immediately before the close. Closed native Stack records
are audit history, not current membership; open mappings refuse. Active or parked
labels, native membership, unresolved inventory, draft/fork state, provider auto-merge,
identity drift or an insufficient repository permission refuse. No unrelated CI
run or queue convergence is requested.

## Receipt and retry contract

The operation key is bound to the entire request and policy fingerprint in a
bounded, private journal at
`<git-common-dir>/caravan/native-stack/pr-close-v1-<key-sha256>.json`, reusing the
existing common-directory checkpoint infrastructure. On Unix the receipt is
owner-only; intent is flushed before the write. Intent is persisted
before the provider write. Once intent exists, **the same key never sends another
close**, including after a crash before the HTTP result was recorded. It instead
re-reads the provider. Changed request or policy identity refuses; never reuse a
key for another generation.

Outcomes distinguish a confirmed close, a close reconciled after intent,
externally closed/merged state, refusal, unavailable pre-write evidence and
indeterminacy. Phase-specific error codes are retained, never provider stderr or
credential-helper output. `provider_mutated` concerns the current call; the
historical write result and pre-intent fence fingerprint remain separately
available. A write error followed
by matching closed state is reconciliation, not proof of who closed it. Open,
changed or unreadable state after intent remains indeterminate; it never causes
automatic reopening, another close, or a different generation's action.
`cara pr-close status --operation-key KEY` / MCP `pr_close_status` reads the
retained journal without provider access or mutation. It is historical evidence,
not a fresh provider observation.

Successful envelopes distinguish `closed`, `reconciled_closed`,
`externally_closed` and `externally_merged`. Nonzero errors carry the typed
receipt in `error.details`: `pr_close_refused` is a policy/identity refusal,
`pr_close_unavailable` is unavailable pre-write evidence, and
`pr_close_indeterminate` requires reconciliation after recorded intent.
`close_attempted_this_call` means the close adapter was invoked; its runner may
still have refused before HTTP execution. An old successful HTTP result is not
proof of a new mutation on replay.

The journal must survive retries. Linked worktrees sharing a Git common directory
share it. A remote writer lease does **not** replicate this journal: do not reroute
an uncertain operation to a fresh clone without its retained custody/receipt.
This is not a cross-host exactly-once service.

GitHub close does not expose a documented head-CAS transaction. The fence
serializes cooperating writers; it does not freeze arbitrary external GitHub
actors. Immediate preflight and post-write drift detection do not create provider
atomicity. The receipt explicitly reports this limit and preserves indeterminate
outcomes. Native-member retirement remains a separate, supported topology
operation, never a shortcut through this command.

## Example

```sh
cara pr-close apply --expected-repository OWNER/REPO --pr 123 \
  --head-ref feature --head FULL_HEAD_OID \
  --base-ref main --base FULL_BASE_OID \
  --main-ref main --main FULL_MAIN_OID \
  --represented-at FULL_LANDED_OID --representation same-tree \
  --actor OWNER --custody-reference OWNER_GENERATION_RECEIPT \
  --reason 'Source represented by the recorded landed commit' \
  --operation-key UNIQUE_CLOSE_KEY --confirmed --json
cara pr-close status --operation-key UNIQUE_CLOSE_KEY --json
```

See `exact-nonmember-close-acceptance.json` for the source-only regression inventory.
No example authorizes a live close.
