# Durable queue telemetry

Cara's local event journal remains the write-ahead source for queue events. An
optional, independent state branch makes those records durable and shareable
without adding provider reads to a queue tick.

```yaml
log:
  flush:
    branch: caravan-log
    after: [sync, join, new, evict]
    interval: 60m
    retries: 3
```

When enabled, successful configured commands detach `cara log flush`. The
sidecar reads the local journal, refreshes the state branch, and writes
`actors/<actor>.jsonl`. Each actor owns one file. A push race refreshes and
retries under an exact force-with-lease. Flush failure is never queue failure:
the completed command does not wait for, roll back for, or inherit the exit
status of the sidecar.

Run a synchronous explicit flush when an exact receipt is needed:

```console
cara --repository /path/to/repo --json log flush --force --actor helsinki
```

The branch is orphaned: its first commit has no parent in product history.
Subsequent commits form only the telemetry history. Existing local rotation and
retention remain unchanged.

## Record format

Each line is a versioned `TelemetryEnvelope`:

```json
{"schema_version":1,"actor":"helsinki","recorded_unix_ms":1700000000000,"record_type":"cara_journal","journal":{"record_type":"event","version":1,"event":{}}}
```

The schema is deliberately open to external collectors. A GitHub Actions
watcher writes the same actor-partitioned files using this payload:

```json
{"schema_version":1,"actor":"actions-watcher","recorded_unix_ms":1700000000000,"record_type":"github_actions","repository":"owner/repo","run_id":123,"head_oid":"abc123","event":"pull_request","conclusion":"cancelled","duration_ms":45000,"cancelled_job_ms":42000}
```

The watcher owns Actions API polling, cursoring, and provider cost. It must use
the same fetch, append-only actor file, commit, and exact force-with-lease retry
contract as `cara log flush`. Cara core does not query Actions. Records contain
no credentials, prompts, logs, URLs with credentials, or provider tokens.

## Dashboard

`cara web` reads the configured state branch independently of queue status. The
**Metrics** button in the Caravan header toggles a section between the repository
hero and active topology. The browser renders rollups over raw branch data:

- record and actor counts;
- median and maximum observed caravan size;
- Actions run count, cancellation share, and wall minutes;
- bounded admission-refusal counts.

A missing branch or transient read error never changes repository health or
blocks dashboard queue controls.
