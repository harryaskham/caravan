# Caravan

Caravan is an agent-in-the-loop merge queue for GitHub pull requests. The `cara`
CLI represents a queue as one or more labelled PR chains, performs deterministic
compatibility and CI checks, and returns typed decision points when semantic
judgment or code repair is required.

The normative behavior is in [`SPEC.md`](SPEC.md). Visit the public project site at
[**a.skh.am/caravan/**](https://a.skh.am/caravan/) for a visual guide to the
queue model, agent loop, safety evidence, commands, and installation.

## Website

The GitHub Pages site is deterministic static source in [`site/`](site/), with
no external runtime assets or tracking. All project-local URLs use the
`/caravan/` Pages base path. Run its dependency-free link, metadata, and asset
smoke check with:

```sh
python3 scripts/check-site.py
```

Pushes that touch the site deploy through [the Pages workflow](.github/workflows/pages.yml)
after the same check passes.

## Core model

- A caravan head targets the repository default branch.
- Each later PR targets its predecessor's branch without routinely rewriting
  history.
- Every member has the `caravan` label.
- Only the head has squash auto-merge enabled.
- The current head PR number is the caravan ID.
- `cara sync` is idempotent: pending CI waits, failed CI returns bounded
  run/job/failed-step and exact selected-lineage receipts, and
  `--rerun-failed` reruns only current-generation infrastructure failures.
  Stale synthetic generations require a fresh candidate trigger; raw logs and
  unrelated log text are never retained or exposed.
- `caravan-force` is explicit operator intent to bypass any CI state that is
  not fully successful, including pending, running, failed, mixed, and empty
  checks. A forced head is admin-squashed only when repository policy permits
  it, the exact head/default compatibility proof is still current, and GitHub
  reports admin permission.

## V1 command surface

Every bounded queue operation is implemented by one shared typed library path
used by the human CLI, stable `--json` envelopes, and MCP. The foreground loop
is intentionally CLI-only; MCP coordinators schedule bounded `sync --all`
calls instead. See [`docs/v1-parity.md`](docs/v1-parity.md) for the checked
SPEC-to-surface matrix.

Development surfaces:

```sh
nix develop
cargo test
cargo run -- help
cargo run -- mcp tools
cargo run -- mcp stdio
cargo run -- self-update status
cargo run -- feedback status
```

Domain surface:

```text
cara init
cara status
cara check [--tail-pr N | --head-pr N]
cara new [--create-pr]
cara join [--tail-pr N | --head-pr N] [--create-pr]
cara renew | cara rejoin
cara show | cara next | cara prev
cara sync [--all] [--rerun-failed] | cara loop [--once]
cara evict [--pr N] --reason TEXT
cara split [--pr N]
cara van list | cara van next | cara van prev
cara lock status | cara lock recover --token TOKEN --confirm
cara mcp tools | cara mcp stdio
cara self-update status | check | run
cara feedback status | report
```

First use is always explicit: run `cara status`, then `cara init`. Init atomically
creates `.caravan/config.yaml` only when absent, verifies repository permission,
default-branch protection, and squash auto-merge policy, and creates only the
three canonical labels. It never overwrites an existing config or label and
never mutates a pull request. Repeated init calls are verification-only no-ops.
If label metadata differs, reconcile it manually and retry. The legacy active
label `1D76DB` / `Active member of a Caravan merge chain` is explicitly
compatible and preserved. See [`docs/first-use.md`](docs/first-use.md).

`cara van next` on the default branch enters the first caravan head; ordinary
`cara next` remains chain-local and requires the current branch to map to an
open caravan PR.

Use `cara help` for the agent operating loop and recovery rules. Use `--json`
for stable `mcp-cli` envelopes.

## Releases and self-update

Version tags publish update-compatible `cara` binaries for `x86_64-linux`,
`aarch64-linux`, and `aarch64-darwin`. Each target produces:

```text
cara-<version>-<target>.tar.gz
cara-<version>-<target>.sha256
```

The archive contains `cara-<version>-<target>/cara`, matching
`updatable-cli`'s default asset strategy. The release workflow tests the tagged
source, packages each binary, verifies native archives by executing `--version`,
and uploads both assets to the GitHub Release. Public ecosystem source inputs
are fetched anonymously over HTTPS; the workflow needs no SSH deploy secret.

Use the bounded helper to keep `Cargo.toml`, `Cargo.lock`, and `flake.nix`
aligned and create the tag:

```sh
./scripts/release.sh patch --dry-run   # inspect the next version
./scripts/release.sh patch --no-push   # commit + tag for local review
./scripts/release.sh patch             # atomically push branch + tag
```

An explicit version such as `./scripts/release.sh 0.2.0` is also accepted.
Run `./tests/release_contract.sh target/debug/cara` after building to exercise
the asset/checksum layout and `cara self-update status` with an isolated home;
it never stages or installs an update over the developer binary.

## Ecosystem

The binary uses:

- [`mcp-cli`](https://github.com/harryaskham/mcp-cli) for shared CLI/MCP typed
  tools and structured envelopes;
- [`updatable-cli`](https://github.com/harryaskham/updatable-cli) for GitHub
  release self-update tools;
- [`feedback-cli`](https://github.com/harryaskham/feedback-cli) for configurable
  structured reporting.

The Nix flake pins Harry's current FlakeHub nixpkgs revision. The three public
ecosystem repositories are also pinned as `flake = false` source inputs and
patched into Cargo, so sandbox builds neither evaluate their independent flakes
nor fetch Git dependencies at build time.

## Configuration

Repository policy and hooks live at `.caravan/config.yaml`. The optional
`rebase_on_join: true` mode physically rebases each owned PR's candidate-only,
linear commit range onto its exact predecessor under an exact force-with-lease;
it is disabled by default and never mutates the caller's worktree:

```yaml
version: 1
force_merge: false
rebase_on_join: false
command_timeout_secs: 30
loop:
  interval_secs: 60
journal:
  max_bytes: 8388608
  max_archives: 3
hooks:
  sync_failed:
    command: ./scripts/on-caravan-sync-failed
    timeout_secs: 30
    blocking: false
```

Cumulative mode rejects fork heads, stale leases, empty/ambiguous ranges, and
candidate-only merge commits. It fetches exact remote OIDs, simulates the whole
rebase in a disposable detached worktree, performs a dry-run permission/lease
preflight, and pushes only with
`--force-with-lease=refs/heads/<branch>:<old-oid>`. Errors are resumable and do
not speculatively push a conflicted range. Successful membership output includes
old/new head and base OIDs, the resulting tree, and the exact lease alongside
GitHub mutation receipts.

GitHub Actions must also be configured to run for non-default PR bases.
`pull_request.branches` filters match the PR's base branch; a workflow restricted
to `main` will not run for B targeting A. Cumulative mode therefore requires a global `pull_request` trigger with no
`branches` or `branches-ignore` filter and activity types `opened`,
`synchronize`, `reopened`, `edited`, and `labeled` (usually also `unlabeled`). A
dedicated stack/full job can then skip unless `base_ref == 'main'` or the PR has
the `caravan` label. The `labeled` event closes the race where the base-edit
`edited` event occurs before Caravan adds its label. Physical ancestry cannot
override provider workflow filters; opt-in membership fails with
`rebase_ci_trigger_missing` when this trigger proof is absent.

This proves cumulative *tree content*, not stable GitHub check identity. Because
Caravan heads currently squash-merge, retargeting a child after its parent lands
can change GitHub's merge ref and trigger CI again even when that cumulative tree
was already tested. Instant no-rerun landing requires an ancestry-preserving
merge mode or an audited exact-tree/check receipt policy, neither of which is
currently implemented.

`command_timeout_secs` is both the hard ceiling for each `git` or `gh` child
and the complete operator-safe budget for `cara status` (30 seconds by
default). Status propagates one absolute deadline through discovery,
compatibility, and label inventory; every child receives only the remaining
budget. Timeouts terminate and reap the child process group and return stable
`github_discovery_timeout` evidence with the exact phase, operation
`elapsed_ms`/`deadline_ms`, retryability, bounded output, and a mutation-free
safe next action. Successful JSON status includes `timing` with total and
per-phase milliseconds (`github_discovery`, `compatibility_analysis`, and
`repository_label_inventory`) so repository-size regressions are visible
without an outer shell timeout.

Discovery performs one bounded all-open PR query containing current check
rollups, derives the current PR and caravan-labelled members from that snapshot,
and uses a separate bounded merged-history query that deliberately omits check
rollups. Provider command count therefore remains constant as open PR count
grows; compatibility subprocesses share the same whole-status deadline.

Hooks receive one versioned `CaravanEvent` JSON object on stdin plus non-secret
`CARA_EVENT`, `CARA_EVENT_ID`, `CARA_OPERATION_ID`, `CARA_REPOSITORY`,
`CARA_CARAVAN_ID`, and `CARA_PRS` context. Delivery output reports only bounded
state/exit/byte counts, never hook output content. Best-effort failures remain in
the command output; blocking failures return typed `hook_failure` and never roll
back provider mutations already recorded by the event.

Every canonical event is first appended under the repository's common Git
metadata (`caravan/events-v1.jsonl`), so linked worktrees share one journal.
Secret-free hook delivery receipts are appended afterward; hook stdout/stderr
content is never stored. Appends and reads use a repository lock, exact event IDs
are deduplicated, torn final records recover safely, and the configured size and
archive count bound retention.

```sh
cara log                         # newest 100 event/delivery records
cara log --kind ci_failed --pr 42 --limit 20
cara log --json                  # stable bounded JSON envelope
cara log -f                      # existing tail, then new records until signal
cara log --json -f               # newline-delimited streaming records
```

Only the bounded `log` snapshot is exposed over MCP; follow is deliberately
CLI-only and creates no queue cursor or authority.

Caravan intentionally owns no cross-process hook dedupe. A long-running hook can
use an external lock and return success while that lock exists; repeated ticks
then become visible no-ops rather than duplicate coordination.

## Incident holds

Freeze one caravan without invalidating or blocking independent caravans:

```sh
cara pause --head-pr 42 --actor oncall --reason "incident INC-123"
cara status                    # reports active, expired, or stale hold evidence
cara sync --all                # skips #42 without mutation; continues the fleet
# After CI/operator recovery:
cara resume --head-pr 42 --actor oncall
cara sync
```

Pause changes only the exact head's auto-merge state and preserves labels,
branches, bases, children, and all PR heads. Optional `--expires-unix-secs` and
`--external-reference` metadata are bounded and non-secret. Expiry only changes
the status warning: no background loop can resume a hold. Resume is an explicit
audited action and fails closed if the head, base, labels, topology, compatibility,
or safe terminal check state no longer matches. See `SPEC.md` for recovery and
retry semantics.

```sh
cara loop --once --json     # one bounded sync --all tick for agents/schedulers
cara loop                   # foreground human stream until SIGINT/SIGTERM
```

The unbounded loop is deliberately not an MCP tool. Every tick starts from fresh
GitHub state, and a decision-point/error tick fires its configured hook and stops
instead of inventing an agent decision.
