# Remote writer lease protocol and writer modes

Caravan's existing `.git/caravan/operation.lock` excludes mutating processes on
one machine. It cannot fence local and hosted workers on different machines.
This document defines the remote compare-and-swap contract required before a
hosted writer may be enabled.

Repository config remains backward-compatible and defaults to:

```yaml
writer:
  mode: local_only
```

`read_only` permits read/config/status/check/log/plan/web-refresh surfaces and
refuses every mutation at `WriterOperationGuard` before provider or local write.
Its plan runner also carries a deny-write fence, so a future accidental marked
command fails before spawn.

`remote_fenced` permits reads without lease acquisition and requires every
mutation to acquire the remote lease before the local lock. Remote policy may
set bounded timing only in that mode:

```yaml
writer:
  mode: remote_fenced
  lease_ttl_secs: 60
  heartbeat_secs: 15
```

TTL is 10-3600 seconds; heartbeat is positive and strictly smaller. Deployment
must also provide `CARA_REMOTE_LEASE_COMMAND`, exact
`CARA_REMOTE_LEASE_HOST`, and bounded `CARA_REMOTE_WRITER_OWNER`. Exact
owner/repository comes from required `config.repository`; App installation is
included when App policy selects one. `sync.checkout_on_decision` must remain
false so an error path cannot attempt a second lease while inherited repair
ownership is live. A broker path alone never activates hosted writes.

## Lease identity and ordering

One lease key contains exact provider `host`, `owner`, `repository`, and the App
`installation_id` when App mode is selected. A grant additionally binds:

- deployment writer-owner ID;
- unique operation ID;
- positive, monotonically increasing `fencing_token`;
- heartbeat and hard expiry timestamps in Unix milliseconds;
- bounded, opaque, non-secret backend CAS revision.

Acquire must be atomic create-or-CAS. A live holder causes typed contention. An
expired takeover must receive a token strictly greater than every token
previously issued for that repository key. Renew and release require exact key,
owner, operation, and fencing token. Time alone never proves ownership or
release.

## External broker

The reviewed deployment supplies one executable in
`CARA_REMOTE_LEASE_COMMAND`; the executable is not stored in repository policy.
Cara invokes it without a shell, places the operation name in
`CARA_REMOTE_LEASE_OPERATION`, and sends a strict JSON request on stdin. No
backend credential or lease secret belongs in argv, stdout, repository config,
receipts, or logs. Request and response are independently limited to 16 KiB.

Each request is tagged with `operation` and contains `schema_version: 1`:

```json
{
  "operation": "acquire",
  "schema_version": 1,
  "request": {
    "key": {
      "host": "github.com",
      "owner": "owner",
      "repository": "name",
      "installation_id": 12345
    },
    "writer_owner": "hosted-worker-3",
    "operation_id": "sync-019f...",
    "now_unix_ms": 1893456000000,
    "ttl_ms": 60000,
    "heartbeat_ms": 15000
  }
}
```

Other operations are `inspect`, `renew`, and `release`. A strict response has
`schema_version`, matching `operation`, optional `grant`, and booleans
`released`/`contended`. Unknown fields, schema/operation mismatch, contradictory
field shapes, invalid identity, nonpositive/regressed fence, malformed expiry,
and oversized output fail closed.

If acquire or renew transport is ambiguous, Cara performs one authoritative
`inspect`. It accepts only the same exact live holder/fence (and, for renew, a
strictly advanced expiry). Release ambiguity succeeds only when inspect proves
no holder. A replacement fence is loss, not release success. There is no
unbounded retry.

## Mutation-intent seam

Every production provider or remote-Git command carries an explicit intent:
`read`, `provider_write`, or `git_write`. Known `gh` write forms (PR/label/run
mutations, REST write methods, and GraphQL `mutation`) and non-dry-run `git
push` are also conservatively inferred. `FencedCommandRunner` rejects an
inferred write with a missing marker before fence or child execution, bypasses
reads without a lease call, and revalidates exactly once before each marked
write. It delegates the inner runner's deadlines, request budgets, App
credentials, and telemetry unchanged.

The GitHub adapter, native Stack/async-merge adapters, physical force-with-lease
push, and reviewed repair push are marked. Source/constructor contract tests
keep their write surfaces explicit. Force/force-intent, pause/resume, priority,
reshape provider controls, and navigation now construct their provider/local
runners through the operation guard; reads bypass and marked writes revalidate
when a remote guard is eventually active.

## Operation authority boundary

Every production mutation now acquires `WriterOperationGuard` through
`AppContext::acquire_writer_operation` rather than opening `OperationLock`
directly. Membership, sync/plan, force/intent, reshape, pause/resume, priority,
navigation decisions, and every repair lifecycle path share this boundary.
`local_only` transparently preserves the existing local lock owner,
checkpoint/recovery, explicit release, and Drop semantics. Source contract tests
reject a new direct acquisition outside the lock implementation and test-only
fixtures. Reserved non-local modes still refuse before local lock creation.

The boundary now models remote-first acquisition: it constructs one unique
operation ID, acquires `RemoteLeaseGuard`, then acquires the local lock. Local
acquisition failure drops and exact-releases remote ownership. One guard can
wrap multiple fully configured `ProcessRunner`s, preserving their auth, budgets,
deadlines, and telemetry; each marked write revalidates the shared fence while
reads bypass it. Explicit release drops local ownership before remote.

Every `WriterOperationGuard::checkpoint` automatically binds the latest
secret-free lease grant to the operation evidence. Remote acquisition writes an
initial `writer_authority_acquired` checkpoint; derived repair/sync locks write
`writer_authority_inherited`. Later checkpoints include current key, owner,
operation ID, fence, heartbeat/expiry, and backend revision. Local-only evidence
is forwarded unchanged. Existing bounded checkpoint compaction handles large
operation evidence and preserves `operation_evidence` and
`remote_writer_lease` navigation keys; broker commands, credentials, and raw
output are never persisted.

Production context validates and opens `remote_fenced` only with the exact
policy/environment above. Membership and sync provider adapters share their
operation guard, including post-rewrite rediscovery and auto-admission handoff. Their physical rebase budgets now retain the same guard
through temporary worktree preparation, barrier verification, and the marked
force-with-lease push; reshape eviction uses the same path. Lease loss after
preparation stops before push and preserves the remote head. Repair discovery,
workspace materialization, semantic grant/revoke, continuation, and the marked
non-force provider publication push now share their operation guard too. Repair
sync derives a second workspace-local lock while sharing the parent's exact
remote guard, so it performs no second broker acquire and releases remote only
when the last owner drops. Checkpoints automatically bind the latest grant and
existing compaction limits remain authoritative.

## Guard lifecycle

`RemoteLeaseGuard` owns the latest grant behind synchronized shared state and
implements the command mutation-fence seam. Before each write it authoritatively
inspects when heartbeat is not due, or renews under one critical section when
due. Concurrent writers single-flight that renewal: later writers observe the
new revision/heartbeat and inspect instead. Renewal requires unchanged exact
key/owner/operation/fence and strictly advanced expiry. Release and Drop use the
latest grant, attempt exact best-effort release, and never claim ambiguous
success. Existing provider preconditions, request/mutation budgets, operation
deadlines, result-tree checks, and force-with-lease OIDs remain additional
mandatory gates.

This enables a remote-fenced Cara core, not a hosted service. Deployments still
configure exactly one scheduler/worker topology per repository; automatic
failover and hosted queue/tenancy lifecycle remain separate work.
