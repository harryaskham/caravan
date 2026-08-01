# Remote writer lease protocol (preview, not active)

Caravan's existing `.git/caravan/operation.lock` excludes mutating processes on
one machine. It cannot fence local and hosted workers on different machines.
This document defines the remote compare-and-swap contract required before a
hosted writer may be enabled.

**No non-local writer mode is active yet.** Repository config defaults to:

```yaml
writer:
  mode: local_only
```

`read_only` and `remote_fenced` are reserved schema values. Offline config
validation can review them, but production context startup refuses both until
every mutation entry point consumes and revalidates a remote fence. Remote
policy may set bounded timing only in that mode:

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
included when App policy selects one. A broker path or config value cannot activate
hosted writes by itself.

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

Production context still refuses `remote_fenced`. Membership and sync provider
adapters share their operation guard, including post-rewrite rediscovery and
auto-admission handoff. Their physical rebase budgets now retain the same guard
through temporary worktree preparation, barrier verification, and the marked
force-with-lease push; reshape eviction uses the same path. Lease loss after
preparation stops before push and preserves the remote head. Repair discovery,
workspace materialization, semantic grant/revoke, continuation, and the marked
non-force provider publication push now share their operation guard too. The
remaining activation work is repair-to-sync inherited-guard handoff, durable
lease checkpoints/renewal, read-only policy, and final mode opening.

## Guard lifecycle

`RemoteLeaseGuard` owns one grant, implements the command mutation-fence seam,
supports authoritative revalidation and exact
renewal, and attempts exact best-effort release on drop without claiming that
release succeeded. The remaining operation-integration slice must:

1. acquire the remote lease before the local operation lock;
2. retain the guard and secret-free grant receipt for the whole operation;
3. revalidate/renew immediately before every GitHub or Git write;
4. stop before write on expiry, mismatch, timeout, malformed response, or
   indeterminate backend state;
5. preserve all existing provider preconditions and force-with-lease OIDs.

Until that integration lands, `writer.mode: local_only` plus exactly one
configured deployment writer remains the only supported topology.
