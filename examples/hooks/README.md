# Cara hooks that dispatch Cacophony agents

This is a complete, fast, copy-paste example of running Cara so that real queue
events dispatch real agents, driven by a `caco` cron entry instead of a
long-lived process.

Three pieces:

1. a bounded `cara loop --once` tick that a cron entry runs,
2. Cara hooks that turn canonical events into exactly one bead each,
3. ordinary Cacophony controller dispatch from those beads.

Nothing here needs a webhook, a daemon of its own, or a secret in YAML.

## 1. Repository policy

`.caravan/config.yaml` in the managed repository:

```yaml
version: 1
sync:
  actions:
    join_unlabelled_prs: true
  max_candidates_per_tick: 8
  max_mutations_per_tick: 64
  max_github_requests_per_tick: 256
  max_duration_secs: 120
loop:
  interval_secs: 60
hooks:
  ci_failed:
    command: ./examples/hooks/caco-bead-dispatch.sh
    timeout_secs: 20
    blocking: false
  sync_failed:
    command: ./examples/hooks/caco-bead-dispatch.sh
    timeout_secs: 20
    blocking: false
  join_failed:
    command: ./examples/hooks/caco-bead-dispatch.sh
    timeout_secs: 20
    blocking: false
  eviction_failed:
    command: ./examples/hooks/caco-bead-dispatch.sh
    timeout_secs: 20
    blocking: false
```

Keep hooks `blocking: false`. Hook failure must never roll back completed
GitHub work; Cara returns typed partial receipts instead.

## 2. What Cara gives the hook

Each delivery runs the command from the repository root with the complete event
JSON on stdin and these environment values:

| Variable | Meaning |
|---|---|
| `CARA_EVENT` | canonical event kind, for example `ci_failed` |
| `CARA_EVENT_ID` | exact event identity; the only safe dedupe key |
| `CARA_OPERATION_ID` | operation that produced the event |
| `CARA_REPOSITORY` | `owner/name` |
| `CARA_CARAVAN_ID` | caravan head PR number, when applicable |
| `CARA_PRS` | comma-separated affected PR numbers |

Deliveries can repeat after an interrupted Cara operation, so every hook must be
idempotent. `caco-bead-dispatch.sh` uses the `cara-event:<CARA_EVENT_ID>` label
as its dedupe key and exits cleanly when the bead already exists.

## 3. The cron entry

Add a `caco` cron entry that runs one bounded tick. A one-minute schedule keeps
the queue responsive while every tick stays bounded by config:

```yaml
crons:
  caravan-tick:
    schedule: "* * * * *"
    nodes: [ms-mac]
    command: |
      export PATH="$HOME/.nix-profile/bin:/run/current-system/sw/bin:$PATH"
      cd "${CACOPHONY_DIR:-$HOME/.cacophony}/daemon/checkouts/cacophony"
      cara loop --once --json > /tmp/caravan-tick.json || true
      caco bd list --project cacophony --label cara-hook --status open --count-only --json
```

Use `cara loop --once` for cron. It performs exactly one `sync --all` tick,
dispatches hooks, returns a bounded JSON envelope, and exits, so overlapping
runs cannot stack up. `|| true` keeps a typed decision tick from marking the
cron entry failed; the filed bead is the durable signal.

Run `caco cron run --name caravan-tick` to trigger it immediately.

For an interactive operator terminal, `cara loop` (no `--once`) streams human
progress and never exits on a domain failure: it dispatches hooks, prints
bounded failure evidence, and ticks again until you stop it.

## 4. Dispatch

The hook only files exact, deduplicated work. Normal Cacophony routing then
dispatches an agent for the bead, so queue repair follows the same claim,
ownership, and reintegration rules as every other bead. To point dispatch at a
different project, set `CARA_HOOK_PROJECT`; to change urgency, set
`CARA_HOOK_PRIORITY`.

## 5. Verify the wiring quickly

```sh
# one bounded tick with full evidence
cara loop --once --json | jq '{scheduler: .last_tick.sync.scheduler_status.disposition,
                               hooks: [.last_tick.hook_deliveries[].state]}'

# exactly one bead per canonical event
caco bd list --project cacophony --label cara-hook --status open --json |
  jq -r '.data.beads[] | [.id, .title] | @tsv'

# replay safety: the same event id never files a second bead
CARA_EVENT=ci_failed CARA_EVENT_ID=test-1 CARA_REPOSITORY=owner/name \
  ./examples/hooks/caco-bead-dispatch.sh </dev/null
```

`cara log` shows the bounded event journal and hook delivery receipts if a
delivery fails or times out.
