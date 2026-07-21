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
  Every successful tick includes a versioned `scheduler_status` with exact
  default/root/tail/member generations and a `healthy`, `waiting_ci`, or `held`
  disposition. Failed ticks classify `wake_class` as `retry_tick`,
  `external_decision`, or `operator_action`; only an external decision emits a
  repair-wake failure event. Stale provider preconditions are routine retry
  ticks, not merger work. Deterministic unsupported range shapes such as
  `rebase_nonlinear_range`, ambiguous/empty ranges, or rewritten target history
  are non-retryable external decisions with a stable evidence fingerprint and
  explicit repair/reshape/strategy continuations. Stale synthetic generations
  require a fresh candidate trigger; raw logs and unrelated log text are never
  retained or exposed.
- `caravan-force` is explicit operator intent to bypass any CI state that is
  not fully successful, including pending, running, failed, mixed, and empty
  checks. A forced head is admin-squashed only when repository policy permits
  it, the exact head/default compatibility proof is still current, and GitHub
  reports admin permission. Cara-owned physical rewrites consume and audit any
  force label bound to the old head; the new generation requires a fresh
  operator label, while routine membership never carries force intent.
- Optional sync-owned auto-admission is strictly opt-in. After the existing
  fleet converges, `sync --all` considers unlabelled PRs in configured-priority
  then immutable-FIFO order and greedily joins the first compatible live tail.
  Incompatible generations receive `caravan-join-skipped` plus a durable exact
  evidence receipt; unchanged generations are not retried, while candidate,
  default, tail, config, or heuristic changes invalidate the skip automatically.

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
cara web --repo PATH [--repo PATH ...] [--read-only]
cara check [--pr N] [--tail-pr N | --head-pr N]
cara new [--create-pr]
cara join [--tail-pr N | --head-pr N] [--create-pr]
cara renew | cara rejoin
cara show | cara next | cara prev
cara sync [--all] [--rerun-failed] | cara loop [--once]
cara repair start --pr N [--target-pr T]
cara repair grant --session ID --path P --source-revision SHA --actor A --reason R
cara repair revoke-grant --session ID --path P --actor A --reason R
cara repair status --session ID | cara repair continue --session ID
cara repair abort --session ID --confirm
cara evict [--pr N] --reason TEXT
cara split [--pr N]
cara van list | cara van next | cara van prev
cara lock status | cara lock recover --token TOKEN --confirm
cara mcp tools | cara mcp stdio
cara self-update status | check | run
cara feedback status | report
```

`cara next-candidate` selects the canonical priority/FIFO attempt. Preflight that exact provider candidate with `cara check --pr N` (and optionally `--tail-pr T` or `--head-pr H`) without checking out or mutating a branch, base, label, or auto-merge state. The receipt includes the canonical provider candidate identity/freshness evidence, exact head/base repository and OIDs, draft/labels/auto-merge facts, enrollment and canonical-order state, pairwise conflict paths, and a mechanical `new`, `join`, `repair`, `wait`, or `reject` continuation. Provider/ref races fail closed, and a rejected first attempt is never silently leapfrogged.

Human CLI output is terminal-aware: concise sectioned layouts use color when
stdout is a TTY, honor `NO_COLOR`, and render PR numbers/titles as OSC-8 GitHub
links where supported. JSON/MCP remains unstyled and stable for `jq` and agents.

For `cara new` and local `cara join`, `--create-pr` remains the deterministic
noninteractive contract. In an interactive terminal, a missing PR now offers to
publish the current topic branch and create a commit-derived PR automatically.
If invoked from the default branch with changes, Cara shows the exact changed
paths and, after confirmation, creates a named topic branch, stages and commits
the listed changes, publishes it, creates the PR, and resumes membership. On a
clean default branch it creates the requested topic branch and tells the user to
make a commit before rerunning; no empty PR is invented. Advanced same-repository
branches with one merged historical PR are safe fresh-generation ancestry for
an explicit create, while ambiguous reuse, unchanged old heads, unpushed heads,
forks, and provider races still fail closed.

First use is always explicit: run `cara status`, then `cara init`. Init atomically
creates `.caravan/config.yaml` only when absent, verifies repository permission,
default-branch protection, and squash auto-merge policy, and creates only the
three fixed control labels (`caravan`, `caravan-evicted`, and `caravan-force`),
the opt-in `caravan-join-skipped` label when greedy admission is enabled, and
configured priority labels. It never overwrites an existing config or label and
never mutates a pull request. Repeated init calls are verification-only no-ops.
If label metadata differs, reconcile it manually and retry. The legacy active
label `1D76DB` / `Active member of a Caravan merge chain` is explicitly
compatible and preserved. See [`docs/first-use.md`](docs/first-use.md).

`cara van next` on the default branch enters the first caravan head; ordinary
`cara next` remains chain-local and requires the current branch to map to an
open caravan PR.

Use `cara help` for the agent operating loop and recovery rules. Use `--json`
for stable `mcp-cli` envelopes.

## Built-in web dashboard

`cara web` serves the primary visual operations surface directly from the Cara
binary; HTML, CSS, and JavaScript are embedded at compile time, with no CDN or
separate deployment:

```sh
cara web --repo /path/to/repository
cara web --repo ~/src/a --repo ~/src/b --poll-seconds 30 --read-only
cara web --repo . --listen 127.0.0.1:4774 --open
```

Repository inputs are explicit filesystem paths rather than slugs because every
read and mutation remains bound to that exact worktree, config, operation lock,
and provider identity. The initial server refuses non-loopback listeners,
canonicalizes and deduplicates paths, periodically returns the same typed status
used by CLI/JSON/MCP, and applies strict CSP/anti-frame/no-store headers. The
responsive one-page dashboard renders trail-linked caravans, linked GitHub PRs,
exact generations and check failures, unenrolled PR reasons, and current
problems/decisions. Bounded inner lists keep fleet topology and attention queues
visible without an unbounded page. The Evidence drawer retains the latest typed
action receipt, CI lineage diagnostics, canonical events, and hook delivery
outcomes; the Config drawer shows the effective parsed policy with hook commands
redacted. `--read-only` leaves read-only preflight available but disables every
mutating control. Interactive actions use same-origin CSRF, exact snapshot
sequences, and existing typed Cara operations and receipts; they never execute
arbitrary shell input.

## Managed sync repair

When sync returns a typed head or link conflict, do not create a nested
worktree, update a local PR ref, or hand-push a guessed generation. Start an
exact Cara-owned session instead:

```sh
cara repair start --pr 1962                 # merge current default
cara repair start --pr 1959 --target-pr 1962 # merge exact predecessor
cara repair status --session pr-1962-<generation>
# Optional semantic restoration from one reviewed source commit; repeated --path
cara repair grant --session pr-1962-<generation> --path README.md \
  --source-revision <full-sha> --actor <actor> --reason <reviewed-contract>
# Resolve/stage typed conflicts only; grant applies its reviewed patch itself
cara repair continue --session pr-1962-<generation>
```

A semantic grant is distinct from a mechanical conflict: it is bounded, expiring,
and bound to session/repository/head/target, actor/reason, exact one-parent source
commit, source/base blobs, source patch fingerprint, original index blob, and
expected merged result blob. Cara three-way applies and stages that reviewed
source change; continue rejects any later byte drift or ungranted path. Before
continue, the same granting actor may revoke exact paths; Cara restores and
stages each pre-grant blob and records the revocation reason without provider
mutation.

Repair uses an independent clone below Git's common Caravan metadata. It seeds
content-addressed objects from the current canonical checkout, binds a separate
explicit provider `origin`, and minimally fetches the exact recorded head and
target with blob filtering. A dirty caller worktree, daemon-internal `origin`,
and locally diverged PR refs are irrelevant and remain untouched; transport
resume reuses a valid partial repository instead of retransferring it. The manifest persists the
exact provider head and target, allowed conflict paths, mechanically staged
baseline, config path, lifecycle state, and publication receipt. Continue
rejects unstaged/untracked files, unresolved markers, edits outside the typed
conflict scope, changed parents, and moved provider heads. It creates the exact
merge commit itself, publishes only by ordinary non-force fast-forward, verifies
the provider ref, and resumes `sync --all` from the clean managed workspace.
Interrupted committed or published sessions resume idempotently; nonterminal
workspaces are preserved rather than deleted. After inspecting status, an
operator can explicitly remove abandoned local state with `repair abort
--session ID --confirm`; abort never changes provider state.

## GitHub authentication and API budgets

Cara's `gh` subprocesses are authenticated. An explicit `GH_TOKEN` or
`GITHUB_TOKEN` is used directly; otherwise Cara selects a repository-accessible
account from `gh auth` and injects its token without printing it. Explicit
ambient tokens are validated by the first real provider request rather than a
redundant per-process REST probe.

Status, check, sync, loop, JSON, and MCP receipts expose secret-free provider
telemetry: authenticated source class, total/GraphQL/REST/gh-CLI call counts,
and the latest GraphQL cost/remaining/reset evidence. The merge-candidate query
collects `rateLimit` in-band, so observing budget costs no extra request. Set
`CARA_GITHUB_AUTH_KIND=github_app_installation` beside an installation token to
make that non-secret identity explicit in receipts.

For automation, prefer one event-driven Cara loop/controller over independent
agent polling. A GitHub App installation token gives a separate least-privilege
installation bucket and webhook identity, but does not replace query batching,
generation-aware caching, adaptive backoff, or exact provider re-reads before
mutation. In GitHub Actions, mint the short-lived token with the repository's
approved GitHub App token action, export it as `GH_TOKEN`, and run bounded
`cara loop --once` ticks from PR/check/default-branch events plus a schedule.
Never store app private keys or access tokens in `.caravan/config.yaml`.

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
repair:
  materialization_timeout_secs: 180
sync:
  actions:
    join_unlabelled_prs: false
  max_candidates_per_tick: 8
  max_mutations_per_tick: 64
  max_github_requests_per_tick: 256
  max_duration_secs: 120
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
candidate-only merge commits. Membership rewrites one candidate; `cara sync
--all` builds every selected caravan head-to-tail, feeding each retained planned
head into its child. It materializes each generation exactly once in a retained
detached worktree, then verifies every conflict, PR precondition, remote head,
dry-run permission, and exact lease across the complete plan before the first
write. Only independent, disjoint caravans apply concurrently (at most two);
each caravan remains strictly parent-to-descendant. Pushes use only
`--force-with-lease=refs/heads/<branch>:<old-oid>`. A mandatory midpoint GitHub
rediscovery verifies every new head and replaces stale CI facts before normal
sync convergence. Errors preserve exact plan, completed-prefix, provider, and
lease receipts and never force-rollback rewritten branches.

GitHub Actions must also be configured to run for non-default PR bases.
`pull_request.branches` filters match the PR's base branch; a workflow restricted
to `main` will not run for B targeting A. Cumulative mode therefore requires a global `pull_request` trigger with no
`branches` or `branches-ignore` filter and activity types `opened`,
`synchronize`, `reopened`, `edited`, and `labeled` (usually also `unlabeled`). A
dedicated stack/full job can then skip unless `base_ref == 'main'` or the PR has
the `caravan` label. The `labeled` event closes the race where the base-edit
`edited` event occurs before Caravan adds its label. Physical ancestry cannot
override provider workflow filters; opt-in membership and whole-chain sync fail
with `rebase_ci_trigger_missing` when this trigger proof is absent.

For an existing repository, enable this first on a disposable or paused
caravan: commit `rebase_on_join: true`, run `cara status`, `cara check`, and then
one `cara sync --all`; verify the returned plans, leases, rewritten heads, and
fresh pending CI before widening the rollout. Roll back by reverting the config
to `rebase_on_join: false`. Do **not** force-push branches back: any successfully
applied prefix is authoritative and the resumable recovery is rediscovery plus
the same idempotent sync.

With `sync.actions.join_unlabelled_prs: true`, only `sync --all` and the
identical `loop`/`loop --once` path grow the fleet. Existing caravans always
reconcile first; a real graph, provider, compatibility, CI, or operator decision
stops before admission. The `priority_fifo_greedy_v1` heuristic considers
caravans in their existing deterministic order and candidates in canonical
priority/FIFO order. It is best-effort rather than globally optimal: the first
compatible tail wins. When no tail works (or the empty fleet candidate cannot
form a head), Cara records exact candidate head/base, default, all tested tail
generations, compatibility reasons, config fingerprint, actor/time, and
heuristic version in a GitHub comment and adds `caravan-join-skipped`. Manual
`new`/`join`/`rejoin` consumes that advisory label. One tick is bounded by the
configured wall-clock, authenticated `gh` request, candidate, and mutation
limits and returns exact joins, skips, remaining candidates, and continuation.
Hosted automation should run bounded `cara loop --once` ticks from PR/check,
workflow, default-branch, and scheduled events rather than one unbounded job.

This proves cumulative *tree content*, not stable GitHub check identity. Because
Caravan heads currently squash-merge, retargeting a child after its parent lands
can change GitHub's merge ref and trigger CI again even when that cumulative tree
was already tested. Instant no-rerun landing requires an ancestry-preserving
merge mode or an audited exact-tree/check receipt policy, neither of which is
currently implemented.

`command_timeout_secs` is the hard ceiling for lightweight `git` or `gh`
children and the complete operator-safe budget for `cara status` (30 seconds by
default). Network-heavy repair clone/fetch/checkout uses the separately bounded
`repair.materialization_timeout_secs` (180 seconds by default), and persisted
repair status reports its exact phase, budget, process group, partial path, last
error, and resume/abort path. Exact retries reuse verified partial objects after
a sideband disconnect; changed repository/head/target/provider/config facts
still refuse rather than trusting the cache.
Status propagates one absolute deadline through discovery,
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
