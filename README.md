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

For the full decision tree — how a PR becomes a candidate, what blocks the
admission front and what does not, when a root is flattened rather than
replayed, the four conditions for merging a root, and how failures are typed by
who can resolve them — see [docs/lifecycle.md](docs/lifecycle.md). It also lists
the currently known gaps between that intended behaviour and what Cara does
today.

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
  default/root/tail/member generations and a `healthy`, `waiting_ci`, `held`,
  `retry_tick`, or `operator_action` disposition. A successful tick is never
  `healthy` while a member's required contexts have no reporting run on its
  exact current head: those members appear in
  `scheduler_status.missing_required_runs` with the exact PR, head, and
  contexts. A successful tick also names any `head_of_line` stall: the exact
  blocking PR, its one-based queue position, the members waiting behind it, the
  block class (conflict, CI failure, missing required runs, invalid graph, or a
  blocking admission rejection), ordered repair/reshape/evict remedies, and a
  stable fingerprint for counting no-progress passes. A stalled front is an
  external decision, never healthy or idle: syncing more often cannot resolve a
  conflict, and work must be selected by queue position rather than by whichever
  member is cheapest to fix. Failed ticks classify `wake_class` as `retry_tick`,
  `external_decision`, or `operator_action`; only an external decision emits a
  repair-wake failure event. Stale provider preconditions are routine retry
  ticks, not merger work. Deterministic unsupported range shapes such as
  `rebase_nonlinear_range`, ambiguous/empty ranges, or rewritten target history
  are non-retryable external decisions with a stable evidence fingerprint and
  explicit repair/reshape/strategy continuations. Stale synthetic generations
  require a fresh candidate trigger; raw logs and unrelated log text are never
  retained or exposed.
- `caravan-force` is durable PR-scoped operator intent: when that PR reaches
  caravan root, if it is mechanically mergeable, Cara ignores every CI state
  (successful, pending, running, failed, mixed, unknown, or empty) and performs
  the administrator squash immediately. `cara force --pr N --actor A --reason R`
  may arm any active member after exact current-edge, selected-Caravan hold/graph,
  permission, branch, and PR preflight. The label follows the PR through
  Cara-owned history rewrites, membership/base changes, and position changes;
  unrelated admission candidates cannot block it. At root, Cara freshly proves
  the exact root/default compatibility, provider head/base, repository policy,
  ADMIN permission, and lease before merging. Explicit revoke, eviction, or a
  successful merge consumes intent. Controller-reviewed transitions additionally
  use `cara --json force-intent preview|apply|revoke` with exact transition
  evidence; that authorization is generation-bound, but the resulting PR intent
  is durable. Matching MCP tools expose the same receipts.
- Optional sync-owned auto-admission is strictly opt-in. After the existing
  fleet converges, `sync --all` considers unlabelled PRs in configured-priority
  then immutable-FIFO order and greedily joins the first compatible live tail.
  Incompatible generations receive `caravan-join-skipped` plus a durable exact
  evidence receipt; unchanged generations are not retried, while candidate,
  default, tail, config, or heuristic changes invalidate the skip automatically.
- Every non-clean attachment check also returns exact squash-equivalence
  evidence. A landed member reaches the default branch as one squash commit
  whose content matches that member's cumulative content but whose commit
  identity is unrelated to the pre-squash commits later members still carry, so
  replaying that stacked history can conflict against content identical to what
  it introduces. Cara reports whether an ancestor-closed linear prefix of the
  candidate-only range is already held by the target byte for byte — identical
  blob objects *and* file modes on every path that prefix's cumulative diff
  changes — and whether replaying only the retained commits from that proven
  boundary is independently clean. Commit messages, subjects, and patch text are
  never proof, an identical patch with a different resulting blob is not
  equivalence, and an untouched file that happens to match proves nothing. A
  represented prefix whose retained commits still diverge reports
  `residual_conflict`; ordinary three-way divergence after the equality point
  reports `no_equivalence`. Neither drops a commit, and nothing is ever resolved
  by taking either side. Detection is evidence, not authority: reconciliation
  applies only under an explicitly authorized rewrite, which reverifies the
  replayed head tree against the proven cumulative tree and the rebuilt commit
  count against the proven retained set before any push. Receipts list dropped
  and retained commits, the proven boundary and its tree, the represented paths
  with blobs and modes, and the cumulative tree before and after reconciliation.

## Dogfooding

Caravan develops Caravan through its own checked-in physical-chain, automatic
admission, force, CI, and repair policies. Cacophony worker reintegration is
being moved to the transactional `pr_cara_join` handoff so every feature lands
through the same path operators use. See [`docs/dogfood.md`](docs/dogfood.md)
for rollout gates, daily operation, evidence capture, and rollback.

## V1 command surface

Every bounded queue operation is implemented by one shared typed library path
used by the human CLI, stable `--json` envelopes, and MCP. The foreground loop
is intentionally CLI-only; MCP coordinators schedule bounded `sync --all`
calls instead. See [`docs/v1-parity.md`](docs/v1-parity.md) for the checked
SPEC-to-surface matrix.

Development surfaces:

```sh
nix develop
./scripts/check-workflows.sh  # pinned actionlint + shellcheck
cargo test
cargo run -- help
cargo run -- mcp tools
cargo run -- mcp stdio
cargo run -- self-update status
cargo run -- feedback status
# Explicit only: runs the process/load-sensitive hook contract.
cargo test --features environmental-hook-acceptance --test hook_example
```

The default development shell pins `actionlint` and `shellcheck`; the same
`scripts/check-workflows.sh` command is a Nix flake check. Custom self-hosted
runner labels live in `.github/actionlint.yaml`, so workflow validation needs no
ad hoc nixpkgs fetch or machine-global linter installation.

`tests/hook_example.rs` is deliberately guarded by the
`environmental-hook-acceptance` feature. Ordinary `cargo test`, Nix package
checks/installations, pull-request CI, and release contract builds retain every
deterministic unit/build/schema/version gate but do not build or execute that
shell/process integration target. `.github/workflows/hook-acceptance.yml` is the
only hosted owner: it runs daily or by `workflow_dispatch`, never on
`push`/`pull_request`, and executes all assertions in the target unchanged.

The environmental lane requires the repository secrets
`CARAVAN_FEEDBACK_WEBHOOK_URL` and `CARAVAN_FEEDBACK_WEBHOOK_TOKEN`.
`scripts/run-hook-acceptance.sh` always runs the assertions; on failure it
refuses a silent stderr fallback and requires an authenticated webhook before
emitting exactly one bounded, credential-redacted feedback event.
The event names both load-bearing tests, architecture, runner environment,
source revision, failure phase, output digest, and a bounded output tail. Its
stable `caravan-hook-acceptance-v1:...` fingerprint lets an identical rerun
refresh one receiver-side record; canonical webhook fingerprint idempotency is
owned by Cacophony `bd-52cf42`, not by a Caravan-local dedupe workaround. A
passing run emits no feedback event. Manual runs may set `force_failure=true`
to exercise the same visible failure/reporting path without weakening either
test assertion.

Domain surface:

```text
cara init
cara config check [--config PATH]  # strict, read-only pin/config compatibility
cara status
cara web --repo PATH [--repo PATH ...] [--read-only]
cara check [--pr N] [--tail-pr N | --head-pr N]
cara new [--pr N | --create-pr]
cara join [--pr N] [--tail-pr N | --head-pr N] [--create-pr]
cara renew [--pr N | --create-pr] | cara rejoin [--pr N]
cara priority set --pr N --label caravan-priority:high --actor A --reason R
cara priority clear --pr N --actor A --reason R
cara show | cara next | cara prev
cara force --pr N --actor A --reason R
cara force revoke --pr N --actor A --reason R
cara --json force-intent preview|apply|revoke --pr N --head OID \
  --membership-generation G --failure-fingerprint F --reason R \
  --expires-at-ms T --auto-merge squash
cara plan sync [--all] [--rerun-failed]
cara plan concat --source-head-pr S --target-tail-pr T --actor A --reason R
cara concat --source-head-pr S --target-tail-pr T --actor A --reason R --expected-plan-hash H
cara sync [--all] [--rerun-failed] | cara loop [--once] [--manual --shell COMMAND]
cara repair start --pr N [--target-pr T]
cara repair authorize-agent-edits --session ID --actor A --reason R
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

`cara concat` is the atomic recovery operation for appending one complete live caravan after another. Always run `cara plan concat` first and review its immutable plan hash, exact old/new ordering, complete source rewrite scope, and rollback heads. Execution atomically rewrites every source branch under exact leases, then commits one source-root base edge; it never sequences client-side evict+rejoin. A failed commit restores every original source head, final verification failure restores the membership edge and physical heads, and exact retry returns the durable original journal receipt. Cycles, forks, holds, incomplete topology, stale plan hashes, conflicts, and ambiguous rollback all fail closed.

`cara next-candidate` selects the canonical priority/FIFO attempt. Preflight that exact provider candidate with `cara check --pr N` (and optionally `--tail-pr T` or `--head-pr H`) without checking out or mutating a branch, base, label, or auto-merge state. A targetless check first offers the one visible unheld caravan: when that attachment is clean, the complete receipt coherently reports `join` mode, caravan, target, compatibility, and intent; an ineligible attachment falls back to `new`. Zero or multiple visible caravans retain `new` evaluation because a later targetless join could not resolve the same target unambiguously. This inference belongs only to check recommendation; an explicit `cara new` still preflights the requested new-caravan operation. The receipt includes the canonical provider candidate identity/freshness evidence, exact head/base repository and OIDs, draft/labels/auto-merge facts, enrollment and canonical-order state, pairwise conflict paths, and a mechanical `new`, `join`, `repair`, `wait`, or `reject` continuation. Provider/ref races fail closed, and automatic selection never silently leapfrogs a rejected first *eligible* attempt. Every remote `--pr` receipt records non-blocking `admission_note` ordering evidence plus a typed `admission_intent` decision, and an explicit `--pr` request proceeds on the recommended or explicitly targeted action using that candidate's own eligibility rather than its queue position, so an unadmitted earlier PR cannot wedge other owners. Structurally ineligible PRs (draft, fork-only, externally enabled auto-merge, superseded/ambiguous/invalid generation) are reported with exact reasons and excluded from ordering instead of wedging the queue, so a fleet at zero caravans still forms a new root from the first eligible candidate. Cacophony-generated PRs also expose structured generation integrity: Cara groups only the same agent, overlapping bead stream, and stack slot; exact containment or a current reviewed canonical-link audit excludes older `superseded_generation` PRs without blocking their successor, while divergent/invalid siblings stop for owner choice. Membership revalidates the complete open generation set immediately before mutation and never auto-closes stale PRs. Use audited `cara priority set|clear` to change one unenrolled PR's persistent automatic-order metadata; priority never authorizes membership or bypasses compatibility.

Priority/FIFO order is the *automatic selection* contract, so Cara resolves who
selected a candidate and what it asked for before rejecting a non-canonical
attempt. These are separate axes:

- **Automatic** selection (`cara next-candidate`, sync auto-admission) is bound
  by priority/FIFO for `new` and `join` intent alike, without exception.
- **Explicit** owner selection — naming one exact remote PR with `cara check
  --pr N`, with or without `--tail-pr`/`--head-pr` — is deliberate admission
  intent for `new` *and* `join`. It may attach ahead of earlier ordered rows
  while every bypassed row is an unrelated *unjoined* first-admission attempt;
  those rows keep their canonical order and are still admitted in turn.
- **Checked-out** owner operations (local `cara check`, every membership
  operation including `renew`/`rejoin`) report canonical position as evidence
  only, exactly as they always have.

The relaxation never passes an active caravan member, a PR on the candidate's
exact base chain, or a rank-indeterminate row; it never applies to a candidate
that is not itself a current ordered admission attempt; it never guesses an
ambiguous or missing target; and it never substitutes for compatibility,
freshness, generation, policy, or provider success. Every check and membership
receipt carries a typed `admission_intent` decision naming the selection, the
intent, the resolved target, each ordered row ahead with its disposition,
compatibility, provider mutation, and idempotency. The human `admission_note` is
derived from that same decision, so the CLI note, the decision, and the mutation
behaviour cannot disagree.

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

With `rebase_on_join: true`, all successful membership operations expose one exact `join_receipt`. For root `new`/`renew`, predecessor PR `0` denotes the live default branch and its exact branch/OID; root ancestry, physical rebase, durable membership, provider receipts, force absence, configuration fingerprint, and receipt hash are validated just like tail `join`/`rejoin` (bd-d15ba3).

Cara first resolves the current Git worktree root, so every command behaves the
same from the root or any nested directory. Default `.caravan/config.yaml`,
locks, journal, repair state, and init writes are rooted there. A relative
explicit `--config` remains relative to the invocation directory and is stored
as an absolute identity; outside a non-bare worktree fails without writing.

First use is always explicit: run `cara status`, then `cara init`. Init atomically
creates `.caravan/config.yaml` only when absent, verifies repository permission,
default-branch protection, and squash auto-merge policy, and creates only the
three fixed control labels (`caravan`, `caravan-evicted`, and `caravan-force`),
the opt-in `caravan-join-skipped` label when greedy admission is enabled, and
configured priority labels. It never overwrites an existing config or label and
never mutates a pull request. Repeated init calls are verification-only no-ops. Before creating any missing
label, init reads the REST core rate-limit resource and requires a conservative
budget for every bounded create/reread step. An exhausted or near-threshold
budget returns `github_rest_rate_limit_wait` with exact core/GraphQL evidence,
reset time/delay, completed/pending labels, and `mutation=false`; wait for reset
rather than hot-looping. A fully initialized repository performs no rate probe.
If label metadata differs, reconcile it manually and retry. The legacy active
label `1D76DB` / `Active member of a Caravan merge chain` is explicitly
compatible and preserved. See [`docs/first-use.md`](docs/first-use.md).

`cara van next` on the default branch enters the first caravan head; ordinary
`cara next` remains chain-local and requires the current branch to map to an
open caravan PR. Navigation follows the exact provider generation after a Cara
physical rewrite. If a clean, non-current destination branch is stale locally,
Cara atomically retains its old OID under `refs/cara-backup/navigation/*`,
advances the named branch to the reverified provider OID, and reports the backup
ref before checkout. It never resets the current branch or a branch checked out
in another worktree, and never discards a local generation.

For human flow testing, `cara loop --manual [--shell 'zsh -i']` runs normal
sync-all ticks and opens a real inherited-TTY shell only at an
`external_decision`. Cara writes the complete bounded decision to a private
file and exports `CARA_DECISION_FILE`, `CARA_DECISION_CODE`, and
`CARA_REPOSITORY_PATH`; the shell starts in a safe affected/repair workspace
when available. Exiting zero never claims success—it causes exact provider
rediscovery and another tick. Nonzero exits preserve evidence and stop. Manual
mode is refused for JSON/MCP/non-TTY use; production hooks remain noninteractive
machine orchestration.

Use `cara help` for the agent operating loop and recovery rules. Use `--json`
for stable `mcp-cli` envelopes. Operation-lock owner files stay below 16 KiB:
large syncs retain schema-versioned counts, deterministic hashes, and bounded
first/last samples instead of copying full plan/receipt/event histories into the
lock; GitHub rediscovery remains recovery authority.

## Built-in web dashboard

`cara web` serves the primary visual operations surface directly from the Cara
binary; HTML, CSS, and JavaScript are embedded at compile time, with no CDN or
separate deployment:

```sh
cara web --repo /path/to/repository
cara web --repo ~/src/a --repo ~/src/b --poll-seconds 30 --read-only
cara web --repo . --listen 127.0.0.1:4774 --open
CARA_GITHUB_WEBHOOK_SECRET=... cara web --repo . \
  --github-webhook-secret-env CARA_GITHUB_WEBHOOK_SECRET \
  --github-installation-id 12345 --webhook-sync
CARA_GITHUB_WEBHOOK_SECRET=... cara web --repo /srv/a --repo /srv/b \
  --github-webhook-secret-env CARA_GITHUB_WEBHOOK_SECRET \
  --github-installation-id 12345 --webhook-sync --hosted
```

`--hosted` is the optional deployment contract for pre-provisioned checkouts: it
requires signed webhooks, one exact installation, `--webhook-sync`, and per-repo
`app_installation` auth plus `remote_fenced` writer with an exact slug. Ambient
auth, `local_only`, mixed installations, or `--read-only` fail closed at startup.
Hosted workers mutate only from verified webhook deliveries; interactive
dashboard mutations are refused, because the same-origin CSRF token is not
authentication. It provisions no clones and performs no failover; see
[`docs/github-app.md`](docs/github-app.md). Default `cara web` is unchanged.

Repository inputs are explicit filesystem paths rather than slugs because every
read and mutation remains bound to that exact worktree, config, operation lock,
and provider identity. The initial server refuses non-loopback listeners,
canonicalizes and deduplicates paths, periodically returns the same typed status
used by CLI/JSON/MCP, and applies strict CSP/anti-frame/no-store headers. The
responsive one-page dashboard renders trail-linked caravans, linked GitHub PRs,
exact generations and check failures. Repositories and attention decisions live
in independently collapsible left/right sidebars, leaving the center for repo
metadata, compact active topology, and the **Saloon**. The Saloon
deterministically groups unenrolled PRs as Ready to Roll, Saddling Up, Other,
then Bounty List; each group is independently collapsible and its per-repository
open/closed state survives polling renders. It evaluates a bounded set of exact
current destinations for each candidate and shows `Ready (main, PR #tail...)`,
`Conflicting (main, PR #tail...)`, or checking/unknown evidence independently
from priority/FIFO position. Mixed PRs show both compatible and conflicting
targets; stale, truncated, or unevaluated targets are never called Ready. Fresh
candidate evidence moves fixed unjoined PRs back to Ready, while draft/incomplete
and skipped/evicted PRs remain Saddling Up and Bounty List. `Plan sync` runs the
same fresh physical/conflict/lease and
first auto-admission selection preflight without provider writes, then renders
ordered exact actions and rediscovery barriers before Apply. Bounded inner lists keep fleet topology and attention queues
visible without an unbounded page. The Evidence drawer retains the latest typed
action receipt, CI lineage diagnostics, canonical events, and hook delivery
outcomes; the Config drawer shows the effective parsed policy with hook commands
redacted. Eligible active heads expose audited Force/Unforce controls, while
unenrolled Saloon PRs expose configured priority/FIFO controls; both require an
actor and reason and call the same typed force/priority APIs as CLI/MCP rather
than editing labels directly. Mutating requests are accepted as bounded per-repository action jobs. Acceptance
binds both the displayed refresh sequence and a deterministic mutation-authority
fingerprint over exact config/default/PR/check/topology/pause facts. Poll or
webhook refreshes coalesce behind queued/running actions; harmless sequence drift
with the same fingerprint may proceed after locks, while real provider/config
drift returns expected/actual fingerprints and `mutated=false`. The browser can
reconnect and poll durable operation-lock checkpoints while a long sync, split,
evict, join, or repair continues. Evidence includes a bounded
Cara event/hook journal and terminal typed receipt; concurrent actions against
the same repository are refused. `--read-only` leaves read-only preflight available but disables every
mutating control. Interactive actions use same-origin CSRF, exact snapshot
sequences, and existing typed Cara operations and receipts; they never execute
arbitrary shell input.

An optional `POST /api/v1/webhooks/github` endpoint accepts GitHub App webhooks
only when an HMAC secret environment variable and exact installation ID are
configured. It verifies `X-Hub-Signature-256`, installation, explicit repository,
and bounded delivery/event IDs; delivery IDs are durably deduplicated under
common Git state. Default-branch pushes, PR lifecycle changes, and check/workflow
updates coalesce into a fresh status refresh or one bounded `sync --all` action
with `--webhook-sync`. Payloads are wake hints only: Cara always rediscovers
provider truth under its normal lock/budgets. Polling remains a low-frequency
reconciliation fallback; webhook counters are secret-free dashboard state. Use a
TLS reverse proxy/tunnel to the loopback listener and never store the webhook
secret in `.caravan/config.yaml`. Configure the GitHub App for JSON delivery and
subscribe to **Push**, **Pull request**, **Pull request review**, **Check run**,
**Check suite**, **Status**, and **Workflow run** events; read-only Metadata,
Contents, Pull requests, Checks, Commit statuses, and Actions permissions are
sufficient for delivery. The ordinary Cara/GitHub credential—not the webhook
payload—still authorizes any `--webhook-sync` provider mutation.

## Managed sync repair

When sync returns a typed head or link conflict, do not create a nested
worktree, update a local PR ref, or hand-push a guessed generation. Start an
exact Cara-owned session instead:

```sh
cara repair start --pr 1962                 # merge current default
cara repair start --pr 1959 --target-pr 1962 # merge exact predecessor
cara repair status --session pr-1962-<generation>
# For a typed semantic/CI decision, authorize one exact agent identity
cara repair authorize-agent-edits --session pr-1962-<generation> \
  --actor caco-merger --reason <reviewed-decision>
# Optional narrower deterministic restoration from one reviewed source commit
cara repair grant --session pr-1962-<generation> --path README.md \
  --source-revision <full-sha> --actor <actor> --reason <reviewed-contract>
# Resolve/stage reviewed edits; broad edits must use the authorized actor
cara repair continue --session pr-1962-<generation> --actor caco-merger
```

A session-level agent-edit authorization is bound to exact repository, PR,
provider head, target, config, session manifest, actor, reason, and expiry. It
lets that agent add, modify, rename, or delete ordinary repository files in the
isolated workspace after a typed semantic/CI decision. Continue still rejects
unstaged/untracked files, unresolved conflicts, Git internals, symlinks/gitlinks,
secret-like paths, identity drift, excess path/diff bounds, and actor mismatch.
Before commit it records the complete bounded path list plus path, staged-index,
and binary-diff fingerprints; publication is non-force and explicitly requires
fresh CI.

A semantic grant is distinct from a mechanical conflict: it is bounded, expiring,
and bound to session/repository/head/target, actor/reason, exact one-parent source
commit, source/base blobs, source patch fingerprint, original index blob, and
expected merged result blob. Cara three-way applies and stages that reviewed
source change; continue rejects any later byte drift or ungranted path. Before
continue, the same granting actor may revoke exact paths; Cara preflights the
whole set, restores and stages each pre-grant blob, and records durable bounded
revocation receipts without provider mutation. Grant and revoke reconcile exact
receipt/index states after interruption, including a staged result or restored
baseline whose final manifest publication did not complete.

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

The reviewed least-privilege App permissions, broker schema, branch rules,
webhook setup, attribution limits, and single-writer contract are in
[`docs/github-app.md`](docs/github-app.md). The cross-host CAS lease protocol,
activated by `writer.mode: remote_fenced`, is documented in
[`docs/remote-writer-lease.md`](docs/remote-writer-lease.md); the exact machine-checked baseline is
[`docs/github-app-policy.json`](docs/github-app-policy.json).

Cara's `gh` subprocesses are authenticated. An explicit `GH_TOKEN` or
`GITHUB_TOKEN` is used directly; otherwise Cara selects a repository-accessible
account from `gh auth` and injects its token without printing it. Explicit
ambient tokens are validated by the first real provider request rather than a
redundant per-process REST probe.

Opt-in local GitHub App mode first requires repository policy
`github_auth.mode: app_installation` with exact non-secret `app_slug` and
`installation_id`. Missing policy defaults to ambient. Production startup then
requires matching `CARA_GITHUB_AUTH_MODE=app_installation`, one executable
`CARA_GITHUB_APP_CREDENTIAL_COMMAND`, `CARA_GITHUB_APP_SLUG`, and
`CARA_GITHUB_APP_INSTALLATION_ID`. A broker path alone never activates App mode;
policy/runtime mismatch, unknown mode, or incomplete settings fail closed
without falling back. Cara passes
exact repository/host to the broker in secret-free environment and accepts one
JSON object containing `token`, `app_slug`, `installation_id`, `repository`, and
`expires_unix_secs`. Repository and expected identity must match; expired or
near-expiry responses fail closed. Valid credentials are process-cached with a
60-second refresh margin and concurrent refresh is single-flight. Broker stdout
is parsed but never rendered.

The same cached installation principal authenticates remote Git operations over
an exact HTTPS repository. Cara installs a process-local Git credential helper
through secret environment configuration: the helper program text contains no
secret and reads the token only from environment; token-bearing scripts, argv,
remote URLs, and persisted Git config are never created. Existing credential
helpers and interactive prompting are disabled for that child. SSH, plaintext
HTTP, non-GitHub/local remotes, explicit repository mismatch, and
credential-bearing URLs fail before the remote command. One authentication
failure refreshes and retries under the same deadline; a second failure stops.
Ambient mode does not install or invoke this helper.

Status, check, sync, loop, JSON, and MCP receipts expose secret-free provider
telemetry: authenticated source class, total/GraphQL/REST/gh-CLI call counts,
App slug/installation/expiry plus exact Git transport/repository when selected,
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

The two Linux targets are built by the tagged release workflow. `aarch64-darwin`
is published separately with `just release-backfill-target <tag> aarch64-darwin`
from a Mac, because no registered CI runner advertises that platform; scheduling
a job nothing can accept made the whole run hang rather than fail, so the matrix
omits it deliberately (bd-8b6d28). Restore the matrix entry once a darwin runner
exists.

When a repository pins Cara through a flake, `flake.lock` is the upgrade path
of record and `nix flake update` is the normal way to move versions. Self-update
remains available as a deliberate override for pulling a GitHub release anyway:
it installs in place for a user-managed binary, and for a Nix- or
Homebrew-managed binary it installs into a user-owned directory instead
(`CARA_SELF_UPDATE_INSTALL_DIR`, else `~/.local/bin`). It never writes into a
store path or cellar, and it refuses when the chosen directory does not precede
the managed binary on `PATH`, because an install that `PATH` would never resolve
is worse than a refusal. `cara status` reports the resolved executable, its
hash, and whether it came from a store path, so a shadowing install is provable
rather than inferred.

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

That one-shot path commits, tags, and pushes together. It deliberately ignores
the checkout's `origin`: managed Cacophony worktrees point `origin` at a local
daemon mirror. `scripts/release-remote.sh` resolves the canonical GitHub
repository from `CARA_RELEASE_REPO` (or Cargo.toml's package repository), uses
`CARA_RELEASE_REMOTE` only when it names the same github.com repository, and
defaults to its explicit GitHub SSH URL. Publication requires a clean local
`main` exactly equal to canonical GitHub main, atomically pushes main plus the
new tag there, then re-reads both remote refs before claiming the workflow will
run.

Caravan's reviewed flow instead lands the `release: vX.Y.Z` bump through the
ordinary agent and reintegration lifecycle, so by tagging time the bump is
already on `main` and `release.sh` refuses with "already current". Use this
sequence instead, and prefer `just release-tag` over a hand-rolled `git tag`:
it fetches canonical GitHub main (never daemon-mirror `origin`), verifies the
commit is contained there and that `Cargo.toml`, `Cargo.lock`, and `flake.nix`
all declare the exact version, refuses an existing local or true-GitHub tag,
and verifies the peeled remote tag after push. It never moves or force-pushes a
tag.

```sh
./scripts/release.sh 0.0.75 --no-push   # bump Cargo.toml/Cargo.lock/flake.nix
git tag -d v0.0.75                      # drop the provisional local tag
just validate                           # green on the versioned commit
just stress                             # timing-sensitive changes: shake out races
# land the release commit through the normal reintegration flow, then:
just release-tag v0.0.75                # fail-closed tag at exact landed main
```

A tag is immutable once pushed. If its release workflow fails, do not move or
reuse the tag: fix the cause and supersede it with the next patch version. A
bare tag with no release object is not returned by the releases API, so
`self-update` ignores it.

Run `./tests/release_contract.sh target/debug/cara` after building to exercise
the asset/checksum layout and `cara self-update status` with an isolated home;
it never stages or installs an update over the developer binary.

When a runner for one platform is down or exhausted, `just release-backfill-all
<tag>` (or `just release-backfill-target <tag> <target>`) builds the exact
tagged source in a detached worktree and uploads the same assets to the existing
tag. It never creates, moves, or force-pushes a tag.

Downstream consumers that record a reviewed digest per released platform derive
those rows with `just release-pin-rows <tag>`. It re-verifies every published
`.sha256` against the archive it downloaded and fails closed when a platform is
unpublished, so a partially published release can never be pinned as if it were
complete. [`docs/release/v0.0.9-rollout.md`](docs/release/v0.0.9-rollout.md) is
the worked example, including the read-only rollout proof and rollback pin.

Self-update is bound to the exact running executable, not a hard-coded default.
`status`, `check`, and `run` require that executable to be the first executable
`cara` on `PATH` and place `cara_next` beside it. Existing `~/.cargo/bin/cara`
and `~/.local/bin/cara` installations migrate automatically by updating in
place. A shadowed binary, a renamed/test binary, Cargo `target/debug` or
`target/release` binary, and package-manager location fail closed. For another
intentional user-managed directory, set `CARA_SELF_UPDATE_INSTALL_DIR` to its
absolute parent; it must still be the active first `PATH` entry. Startup staged
promotion is likewise skipped for unmanaged/development binaries.

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

Repository policy and hooks live at `.caravan/config.yaml`. `stack_type` defaults
to `caravan`, preserving the existing implementation without probing GitHub's
native Stack API. The explicit `github` value plus a reviewed
`stack_rollout.mutations_opt_in` enables exact Stack membership, reshape, and
lock-fenced landing. Capability, complete inventory, unique mapping, exact
generation, holds, compatibility, CI, and unsupported force intent all fail
closed before provider mutation. The installed
`gh stack` CLI does not merge Stacks; GitHub's web merge uses the top-SHA-only
async REST endpoint. A disposable sandbox proved an unlocked lower rewind can
merge at a changed generation. A follow-up sandbox then proved Cara's preventive
equivalent: one active no-bypass repository ruleset over every selected source
ref rejects owner SSH and REST mutations while the selected prefix merges and
the unselected suffix rebases. The adapter now acquires, verifies, checkpoints,
and exactly releases that lock. The ruleset path requires the explicit,
conditional Administration(write) App permission documented below; default
Caravan mode never needs it.
GitHub exposes no arbitrary Stack remove/reorder, so evict and split are a
phased unstack/rebuild transaction whose sealed checkpoints record
`preflighted`, `unstacked`, `reshape_applied`, `rebuilding`, `rebuilt`, and
`verified` and always persist `provider_atomic: false`. Retries resume from
provider truth without repeating a proven unstack or replacement creation.
`max_caravan_length` bounds one caravan as a merge batch within GitHub's
2..=100 Stack range. It is absent by default, preserving the existing dynamic
capacity model, and defaults to 8 only under `stack_type: github`. A full batch
is never extended: admission uses another compatible caravan or opens a new one,
while sync still lands the maximal contiguous ready prefix instead of waiting
for occupancy. Native mode is enabled per repository only with a reviewed
allowlist:

```yaml
min_cara_version: "0.0.65"
stack_type: github
max_caravan_length: 8
stack_rollout:
  mutations_opt_in: true
  reviewed_by: "operator/change-ticket"
sync:
  head_merge_actor: caravan
rebase_on_join: false
```

The optional `rebase_on_join: true` mode
physically rebases each owned PR's candidate-only, linear commit range onto its
exact predecessor under an exact force-with-lease; it is disabled by default and
never mutates the caller's worktree:

```yaml
version: 1
repository: owner/name  # optional; required only when git remotes cannot name it
force_merge: false
stack_type: caravan
github_auth:
  mode: ambient
writer:
  mode: local_only  # or read_only / remote_fenced; see remote-writer-lease.md
rebase_on_join: false
command_timeout_secs: 30
repair:
  materialization_timeout_secs: 180
sync:
  actions:
    join_unlabelled_prs: false
  terminal_red:
    action: block  # default; opt in to `park` for strict queue liveness
  max_candidates_per_tick: 8
  max_mutations_per_tick: 64
  max_github_requests_per_tick: 256
  max_duration_secs: 120
  missing_required_runs_grace_secs: 300
  retrigger_missing_required_runs: true
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

`sync.terminal_red.action` configures deterministic latest-verdict liveness.
`block` is the backward-compatible default: terminal red stops the tick. `park`
adds `caravan-parked` to the exact caravan head, disables its auto-merge,
preserves every member/base/head, excludes it from active convergence/tail
capacity, and allows independent green candidates to advance. Pending/running
and superseded historical red never park. A new head or current nonterminal/green
verdict removes the label and re-enters the caravan at its original FIFO age.
Hooks may repair parked work but are never required for unrelated throughput.
Run `cara init` after enabling park so the fixed label exists.

`sync.missing_required_runs_grace_secs` is the bounded wait before a required
context with zero reporting run or check-suite lineage on the exact current head
is reported as `missing_required_runs` instead of pending. GitHub sometimes
never starts a run for a freshly rebased head; the pull request then sits
`MERGEABLE`/`BLOCKED` with nothing pending and nothing failed, so waiting is
futile. Cara reports that member in `scheduler_status.missing_required_runs`
with the exact PR, head, and contexts, degrades the disposition to
`operator_action`, and — when `retrigger_missing_required_runs` is enabled and a
rerequestable check suite exists on the *unchanged* head — issues exactly one
auditable rerequest and rediscovers once. Cara never pushes an empty commit,
closes and reopens the PR, force-pushes, or broadly reruns another generation to
work around it; head, base, branch, and membership are always preserved. A
partial provider read is reported as `unknown_provider_state` and retried rather
than being mistaken for an absence.

For a complete, fast, agent-dispatching setup, see
[`examples/hooks/`](examples/hooks/README.md): a `caco` cron entry runs one
bounded `cara loop --once` tick, and an idempotent hook files exactly one
deduplicated bead per canonical event, which normal controller dispatch routes
to an agent. Hooks stay `blocking: false` so a delivery failure can never roll
back completed provider work.

When enabling cumulative mode, set `sync.max_duration_secs` high enough for
physical planning plus the typed apply reserve; this repository uses
900 seconds. `cara status` reports the required and retained reserve, the
processable prefix, the deferred members, the maximum admissible chain size,
and a safe next action for every caravan before any refusal, and
`cara plan sync --all` reports the exact prefix a tick would admit without
mutation.

Cumulative mode rejects fork heads, stale leases, and ambiguous ranges. Before
`join` creates/updates a PR, rewrites a branch, or changes membership, the
selected caravan root must target the exact current default generation; a stale
root returns `join_root_stale_default` and requires `sync --all`. The source
branch is separately bound to exact repository/branch/head, one merge-base
parent, source tree, binary-patch fingerprint/title provenance, selected tail,
and independently computed result tree. Stable `git cherry` patch identity plus
an effective merge-tree/diff against exact current main removes equivalent
patches already landed under different commit OIDs; mixed ranges replay only
the genuinely unique commits. Main changes outside the effective source range
are never replayed into the child. An empty effective source patch returns `join_empty_source_noop` with the complete receipt and zero
provider/branch mutation.

It preserves bounded owned two-parent candidate topology with
`rebase-merges=rebase-cousins`, so a stacked child is rooted on the selected
parent generation instead of preserving a stale cousin root. It independently
proves the exact clean `merge-tree` result and retains old/new commit-parent
mapping in the plan/receipt. A redundant merge of target history may be elided
only when every external parent was already target ancestry and the final tree
still equals that independent proof; the receipt names every elided merge.
Cara-created commits are also checked before push for parent-monotonic author
and committer dates, and a reconstructed merge directly involving the selected
target must name its actual branch. Octopus roots, unowned external parents,
unsafe topology drift, misleading merge provenance, timestamp reversal, or tree
mismatch stop before any write. When any other Git replay changes the commit
count, `rebase_topology_changed` reports source/rebuilt/dropped counts and OIDs,
likely already-present/empty-patch causes, and a safe source-rebase or
reviewed-repair next action. Every exact branch generation that Cara actually
rewrites receives one GitHub-visible line naming the typed reason (default
advanced, parent advanced, join, eviction, reshape, or reviewed repair) and the
short old/new head OIDs. A same-line hidden marker deduplicates provider retries;
dry-runs, failed pushes, and already-satisfied generations post nothing.
Membership rewrites one candidate. After the mandatory provider rediscovery,
`join`/`rejoin` require the exact live tail from
the rebase receipt, while `new`/`renew` require the exact current default and no
inferred candidate membership; a new caravan correctly has no join tail.
Candidate-head/default/tail drift stops before membership writes with an
operation-specific resumable receipt. `cara sync --all` builds every selected
caravan head-to-tail, feeding each retained planned
head into its child. It materializes each generation exactly once in a retained
detached worktree, then verifies every conflict, PR precondition, remote head,
dry-run permission, and exact lease across the complete plan before the first
write. Planning and final no-write verification stop at a precommit deadline
which preserves the apply reserve inside the one configured operation deadline.
That reserve is derived from the operations the tick actually runs: a member
whose exact ancestry already holds costs no push or auto-merge drop, and durable
force labels add no rewrite control mutation, so a completed prefix makes every
later tick cheaper. When
the complete reserve cannot remain but the irreversible part can, the tick still
plans and verifies the complete graph and applies the largest exact
root-to-descendant prefix that fits, checkpoints its receipts, and succeeds with
a `retry_tick` disposition plus the admitted/deferred member lists; the resumed
tick never replays a completed provider mutation. Growing a caravan past the
size the configured deadline can guarantee to drain is refused up front with
`caravan_budget_capacity_exhausted` while the admitted prefix keeps draining.
That bound is priced by the same actual-work reserve as `required_ms`, so
raising a proven-safe `command_timeout_secs` never closes admission. A bound
below two members is impossible to emit: a one-member caravan is never reported
at capacity, and an arithmetic result below that floor fails loudly as
`caravan_budget_capacity_defect` with the deadline that repairs it instead of a
drain suggestion that cannot.
`physical_sync_budget_insufficient` remains for the case where not even one
pending member fits, and reports required/remaining time, configured deadline,
maximum admissible chain size, processable prefix,
partial-or-complete plan count/hash, zero writes, and configuration guidance.
Only independent, disjoint caravans apply concurrently (at most two); each
caravan remains strictly parent-to-descendant. Confirmed control mutations are
checkpointed before branch apply. Apply revalidates the source/range branch,
current default, selected tail, result tree, and root provider precondition,
then lets the exact force-with-lease push detect source-head movement without a
redundant post-mutation dry-run. A child provider base OID that lags an
already-advanced parent branch is accepted only as an explicit retained
ancestor range. Successful join receipts and durable `join_failed` journal
events retain source/target/result evidence and every completed provider
mutation. Pushes use only
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
limits. Cara will not start another automatic candidate when less than a real
bounded exact-Git reserve remains; the receipt exposes reserved/remaining
milliseconds and leaves that candidate for a later tick. Once selected,
sync-owned membership reuses the already-discovered fleet snapshot and receives
a fresh exact-candidate deadline instead of re-running unrelated fleet
compatibility. The result returns exact joins, skips, remaining candidates, and
continuation. Hosted automation should run bounded `cara loop --once` ticks from PR/check,
workflow, default-branch, and scheduled events rather than one unbounded job.

This proves cumulative *tree content*, not stable GitHub check identity. Because
Caravan heads currently squash-merge, retargeting a child after its parent lands
can change GitHub's merge ref and trigger CI again even when that cumulative tree
was already tested. Instant no-rerun landing requires an ancestry-preserving
merge mode or an audited exact-tree/check receipt policy, neither of which is
currently implemented.

`command_timeout_secs` is the hard ceiling for lightweight `git` or `gh`
children and the complete operator-safe budget for `cara status` (30 seconds by
default). Stdout and stderr have independent hard capture bounds; exceeding one
returns `command_output_limit` with exact total/limit bytes and separate bounded
prefix/suffix evidence before JSON decoding, never a misleading malformed-JSON
error. Network-heavy repair clone/fetch/checkout uses the separately bounded
`repair.materialization_timeout_secs` (180 seconds by default), and persisted
repair status reports its exact phase, budget, process group, partial path, last
error, and resume/abort path. Exact retries reuse verified partial objects after
a sideband disconnect; changed repository/head/target/provider/config facts
still refuse rather than trusting the cache.
Human CLI errors render concise colored operational evidence rather than dumping
unbounded provider snapshots. Root-join drift lists only changed mutation facts
(head/base/labels/state/auto-merge), topology refusals show compact commit counts
and OIDs, and physical-budget decisions show required/remaining time. Evidence
over 4 KiB is summarized with a `--json` hint; JSON/MCP always retain the full
structured continuation. CI/check state transitions are deliberately excluded from topology, base,
label, comment, and auto-merge mutation identity, so queued→running progress
does not invalidate an otherwise exact sync/join retry. CI diagnostics and
rerun operations retain strict check/run/head identity.

Status propagates one absolute deadline through discovery, label inventory,
compatibility, and provider identity; every child receives only the remaining
budget. The 35-second read-only surface reserves 2 seconds for serialization and
8 seconds after compatibility for provider-backed/final projection. It performs
the minimal provider inventory before local graph work. Compatibility therefore
yields with a successful, unhealthy `status_partial` built from current bounded
evidence — even when no historical snapshot exists — rather than consuming the
whole deadline before a provider call. The receipt exposes candidate/caravan/
branch counts, planned/completed proofs, bounded skipped/deferred proof names,
phase timings, completion reserve, provider calls, explicit unknown fields, and
whether evidence is `current_bounded_evidence` or `historical_last_good`. Zero
provider calls omit the authentication verdict instead of diagnosing
`authenticated: false`. A separate 40-second CLI watchdog launches status in a
subprocess and atomically checkpoints provider/structural evidence before
compatibility. Worker stdout/stderr go to parent-owned bounded files, never
pipes whose EOF an orphan can retain. On Unix the worker also owns a dedicated
session, so provider groups remain identifiable after an intermediate worker
exits and reparents them. If the executor wedges, the parent preserves the
original PID/group inventory through TERM and KILL, binds Linux identities with
pidfds, adopts orphaned providers as a child subreaper, adds any remaining
session members, and performs only a bounded exact-child reap poll before returning the checkpoint
with phase `command_boundary_watchdog`, still inside Cacophony's 60-second
adapter window. Envelope emission is never behind an unbounded `wait`. Other
timeouts terminate and reap
the child process group and return stable `github_discovery_timeout` evidence
with the exact phase, operation `elapsed_ms`/`deadline_ms`, retryability, bounded
output, and a mutation-free safe next action.

Discovery performs one bounded all-open PR query containing current check
rollups, derives the current PR and caravan-labelled members from that snapshot,
and uses a separate bounded branch-history query that deliberately omits check
rollups. If the bounded open rollup omitted a reused branch, one unique exact
OPEN same-repository PR may take precedence over older unlabelled history only
when its local, remote-ref, and provider head OIDs all match. Multiple open
reuses, forks, OID mismatch, or older `caravan`/`caravan-evicted` membership
history still fail closed. Provider command count therefore remains constant as open PR count
grows; compatibility subprocesses share the same whole-status deadline. Explicit
remote `check/new/join/rejoin --pr` first completes that bounded fleet discovery,
then re-reads and binds only the selected PR under a fresh
`command_timeout_secs` deadline. Its status timing adds
`exact_candidate_provider_refetch` and `exact_candidate_merge_identity`; later
compatibility, physical Git, provider mutation, and post-rewrite rediscovery use
the independently reserved exact deadline and still refuse any head/base drift.

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
cara status                    # active, expired, stale, or retired hold evidence
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
or safe terminal check state no longer matches. When provider truth shows the exact
recorded head merged or closed, the hold is retired: it stays as history, never as an
active card, and never as an auto-merge repair request. See `SPEC.md` for recovery and
retry semantics.

```sh
cara loop --once --json     # one bounded sync --all tick for agents/schedulers
cara loop                   # foreground human stream until SIGINT/SIGTERM
```

The unbounded loop is deliberately not an MCP tool. Every tick starts from fresh
GitHub state, and a decision-point/error tick fires its configured hook and stops
instead of inventing an agent decision.
