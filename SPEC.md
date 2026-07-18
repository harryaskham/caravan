# Caravan specification

Caravan (`cara`) is a non-interactive, agent-friendly merge queue for GitHub pull requests. It keeps long chains moving with mechanical checks and exposes ambiguous repairs as explicit decision points for a user or external agent.

## 1. Model

GitHub is the source of truth. Caravan stores no authoritative queue database.

A **caravan** is a linear chain of open PRs:

- every member has the `caravan` label;
- the **head** targets the repository default branch;
- every non-head targets its predecessor's head branch;
- the **tail** has no labelled child;
- the caravan ID is its current head PR number;
- when the head merges, its child becomes the new head and therefore the new ID.

Order is head → tail. `next` moves toward the tail; `prev` moves toward the head. This chain is the priority order.

A PR with `caravan-evicted` is excluded and cannot use `new` or `join`. It must use `renew` or `rejoin`.

### Automatic admission order

GitHub is authoritative for automatic admission. `.caravan/config.yaml` lists `agent_priority_labels` from highest to lowest priority. Unqueued, non-draft admission attempts with an explicit configured label precede lower-priority and unlabelled attempts. Ties, including all candidates without an explicit priority, are FIFO by immutable GitHub `createdAt` ascending. PR number ascending is only the deterministic equal-time tie-break (and legacy missing-time fallback after timestamped peers). Head commit/author timestamps and PR updated time never affect position: pushes and rebases cannot reset queue age. Automatic admission is never LIFO.

`status`, JSON, MCP, and `next-candidate` expose the same complete ordered attempt list, the resolved label/rank, and a reason for every candidate. This is a selection contract, not a claim that candidate/default and cross-caravan compatibility preflight has passed. Automation must select the canonical first attempt, run `cara check` and the corresponding `new`/`join` preflight, and must not re-sort or leapfrog it if preflight rejects it. The oldest selected eligible PR becomes a new head targeting the default branch; each later selected eligible PR appends at the tail. Rejection fails closed: repair or explicitly change the PR's GitHub state/labels, then rediscover the canonical list. More than one configured priority label on a PR, or any unknown `caravan-priority:*` label, also fails closed and excludes that PR with an explicit rejection reason.

An operator may override this automatic order for an explicit canary selection only by supplying a non-empty reason and recording that reason as a comment on the selected PR. A canary override does not alter the automatic policy or the canonical order subsequently reported by Caravan.

Caravan v1 requires member head branches to exist in the base repository: GitHub cannot target a PR at a fork-only branch.

## 2. Graph invariants

For all open `caravan` PRs:

1. Every node belongs to exactly one acyclic linear chain.
2. Each chain has exactly one head and one tail.
3. A head targets the default branch.
4. A non-head targets exactly one predecessor member's head branch.
5. A member has at most one labelled child.
6. Active and evicted labels are mutually exclusive.
7. Exactly the head has squash auto-merge enabled; every non-head has auto-merge disabled.
8. Every adjacent child/base pair is mechanically compatible.
9. Every head remains mechanically compatible with the current default branch.
10. For each ordered pair of distinct caravans, one caravan's head can be attached after the other's tail without a textual merge conflict.

The final invariant keeps caravans mutually composable. It is rechecked whenever a chain is created, joined, split, evicted, advanced, or synchronized.

## 3. Compatibility

“Rebase” in product language means a compatibility test, not history rewriting.

PR `X` is **mechanically compatible** with target branch `Y` when Git can construct the merge of `X` into `Y` without textual conflicts. Caravan fetches the exact GitHub head/base revisions and uses a non-mutating merge-tree or equivalent GitHub mergeability check. It does not modify the worktree or rewrite `X` merely to prove compatibility.

Mechanical compatibility does not prove semantic correctness. CI, the user, or an external agent owns semantic decisions. Pairwise checks deliberately avoid factorial permutation testing.

The default branch may move independently. A caravan head that no longer merges cleanly into it is a decision point. A valid repair may change the PR, reshape the caravan, or land an outside-caravan fix on the default branch that restores compatibility without rerunning caravan PR CI.

## 4. Discovery and identity

`cara` discovers the base repository, default branch, current local branch, PRs, labels, bases, head revisions, auto-merge state, and checks through `git` and authenticated `gh`/GitHub APIs.

Graph identity is derived each run. No UUID is persisted. A hook receives the complete relevant graph snapshot so a changing head/ID is explicit.

A dangling base is reconciled when it belongs to a just-merged labelled predecessor. Its child is advanced to the default branch. Any other dangling, branching, cyclic, or ambiguous graph is invalid and requires repair.

## 5. Command contract

All commands are non-interactive.

### Repository initialization

- `cara init` is the only automatic first-use mutation surface.
- When `.caravan/config.yaml` is absent it is created atomically with version-1
  defaults. An existing valid file is preserved byte-for-byte; incompatible or
  unreadable files are bounded errors and are never merged or overwritten.
  The checkout guard permits only this exact, validated regular file when it is
  untracked; tracked modifications, symlinks/path escapes, and every other
  untracked file (including neighbors under `.caravan/`) remain dirty.
- Init resolves only repository identity and the default-branch name before
  preflight. It never enumerates PRs, reads PR head OIDs, runs graph compatibility,
  or verifies a moving Git ref, so unrelated pushes cannot make repeated init stale.
- Init preflights repository write capability, squash auto-merge support, and
  default-branch protection with a required check or review policy. It creates
  `caravan` (`5319E7`, active member), `caravan-evicted` (`B60205`, evicted
  member), and `caravan-force` (`D93F0B`, operator force exception), plus every
  configured `agent_priority_labels` entry in configured order. Priority-label
  colors come from the stable rank palette `B60205`, `D93F0B`, `FBCA04`,
  `0E8A16`, `1D76DB`, `5319E7` (cycling for additional ranks), and descriptions
  identify the one-based rank and highest-priority direction.
- Exact existing labels no-op. The historical active-label definition
  `1D76DB` / `Active member of a Caravan merge chain` is also compatible and
  preserved byte-for-byte; receipts report its actual metadata. No other
  variation is accepted. Unexpected metadata is operator-owned and is never
  overwritten. Every create is followed by an exact re-read; concurrent
  creation and indeterminate provider responses converge only when metadata is
  exact.
- Status and check are read-only and report initialization readiness and the
  `cara init` continuation. Membership and sync fail before PR mutation while
  initialization is incomplete. Mutating commands perform a complete preflight before their first mutation, rediscover immediately before each GitHub mutation, and abort on stale preconditions.

### Inspection

- `cara status` — repository overview: current PR, all caravans, the canonical priority-then-FIFO admission list with per-PR reasons, invalid graph fragments, and pending decision points.
- `cara next-candidate` — return the same canonical first ordered admission attempt and complete reasoning without mutation; it explicitly requires subsequent membership preflight and never authorizes leapfrogging a rejected first attempt.
- `cara show` — print the current PR's whole caravan and highlight its position.
- `cara check` — no-update validation. For an active member, check its whole caravan and fleet invariants. Otherwise check whether `new` would succeed.
- `cara check --tail-pr N` — check whether the current PR can join after tail `N`.
- `cara check --head-pr N` — resolve caravan `N`, then check against its current tail.

`--tail-pr` names the intended merge target. `--head-pr` names a caravan whose tail is resolved at execution time. They are mutually exclusive.

### Creation and membership

- `cara new [--create-pr]` — create a one-PR caravan from the current branch's open PR.
- `cara join [--tail-pr N | --head-pr N] [--create-pr]` — append the current PR to a valid tail.
- `cara renew [--create-pr]` — reevaluate an evicted current PR as a new caravan.
- `cara rejoin [--tail-pr N | --head-pr N] [--create-pr]` — reevaluate an evicted current PR and append it.

Without a current PR, these commands fail unless `--create-pr` is set. Creation uses the non-interactive equivalent of `gh pr create --fill` and then continues.

`join`/`rejoin` without a target succeed only when exactly one valid caravan tail exists; otherwise they return candidate tails and require an explicit target.

`new`/`join` reject active or evicted PRs, closed PRs, PRs with auto-merge already enabled, and stale/incompatible graphs. `renew`/`rejoin` additionally remove `caravan-evicted` only after all other preconditions pass.

A new caravan applies `caravan`, targets the default branch, verifies fleet compatibility, and enables squash auto-merge. Joining retargets the PR to the tail branch, applies `caravan`, disables auto-merge, and verifies the resulting fleet. Membership commands accept `--reason TEXT` and `--priority-label LABEL`; an explicit label must exactly match a configured `agent_priority_labels` entry, while omission records FIFO admission.

Every successful mutation of `caravan`, `caravan-evicted`, `caravan-force`, or a configured priority label is completed by a durable PR comment. The comment records operation, before/after labels, actor and reason source, exact compatibility and clean-squash evidence where applicable, and explicit configured label/rank or canonical FIFO basis. A deterministic `caravan-control-label-audit` HTML marker fingerprints operation, PR/head, and the before→after control-label transition; GitHub-visible latest-transition evidence deduplicates partial retries without conflating a later transition on the same head. Comment failure after labels changed returns `github_comment_failed` with completed receipts and a resumable rerun instruction; it is never reported as full success.

### Navigation

- `cara next` — check out the labelled child of the current PR; error at the tail.
- `cara prev` — check out the predecessor/base PR; error at the head.

Navigation refuses dirty worktrees, in-progress Git operations, ambiguous PR mappings, and unsafe branch switches. When the checked-out branch still names an exact, retained, same-repository merged Caravan PR, discovery follows bounded base-ref change history through merged members to its unique active rolling successor. `show` reports both the historical predecessor and the active chain position; chain and fleet `next` enter that successor. Closed-unmerged or unlabelled history, branch reuse, deleted/fork-only heads, stale local or remote OIDs, ambiguous successors, and exhausted history all fail closed with typed evidence. A valid historical predecessor with no successor reports `historical_successor_not_found` rather than silently entering another caravan.

`cara van` is the fleet-level command prefix:

- `cara van list` — list caravans.
- `cara van next` / `cara van prev` — check out the next/previous caravan head in deterministic status order.

From the repository's default branch, where there is no current PR, `cara van next` enters the first caravan head and `cara van prev` reports the lower navigation boundary. Chain-level `cara next`/`prev` and fleet navigation from any other non-PR branch still require a current caravan member.

Fleet navigation is browsing, not queue priority. V1 browses heads by PR number. Admission records either an explicit configured priority label or FIFO (oldest eligible PR first); this does not change browsing order.

### Reshaping

- `cara evict [--pr N] --reason TEXT` — remove a member (current PR by default).
- `cara split [--pr N]` — make a non-head member the head of a new caravan.

Eviction adds `caravan-evicted`, removes `caravan` and `caravan-force`, and disables auto-merge. If the evicted PR has a child, that child is retargeted to the evicted PR's predecessor (or the default branch when evicting the head), but only if the new edge and fleet are compatible. The command fails before mutation if it cannot safely close the gap. Evicting a tail needs no rejoin.

Splitting retargets the selected non-head to the default branch, making it a new head. Both resulting caravans must satisfy all graph and fleet compatibility invariants.

### Synchronization

- `cara sync` — synchronize the current caravan.
- `cara sync --all` — synchronize every non-paused caravan in deterministic head order.
- `cara pause --head-pr N --actor A --reason R` — place an explicit incident or maintenance hold on one exact caravan and disable only its head auto-merge.
- `cara resume --head-pr N --actor A` — explicitly revalidate and release that hold.
- `cara loop` — repeatedly run `sync --all` at the configured interval.

`loop` is a lightweight foreground daemon. It keeps no authority beyond GitHub and performs the same idempotent ticks as direct `sync --all` calls.

### Incident and maintenance holds

A pause stores bounded, non-secret evidence below Git's shared common metadata directory: the rolling head and members, exact head SHA/base/labels, observed checks, actor, reason, creation time, optional expiry, and optional external incident/choice reference. The remote mutation uses the complete PR precondition and changes only head auto-merge. Branches, labels, bases, children, and PR heads are preserved.

Status classifies a matching hold as `active` or `expired` and suspends only the exact missing-head-auto-merge problem. Expiry is an alert, never authority to resume. Cycles, branching, closure, changed heads/bases/labels, incompatible links, and non-head auto-merge remain graph failures; stale hold evidence is reported and fails closed. `sync --all` emits a no-op skip receipt for held caravans and continues independent caravans. A targeted sync returns the same bounded no-op continuation.

Resume is operator/agent initiated only. Before enabling squash auto-merge it revalidates the exact head, base, labels, open state, membership topology, compatibility, and safe terminal checks. An authorization marker makes interrupted resume retries idempotent without treating an external re-enable as valid. Every pause/resume authorization is appended to a secret-free local audit record. No loop, expiry timer, status call, or sync call may remove a hold or enable its head.

### Ecosystem surfaces

- `cara help` — agent operating instructions and recovery examples.
- `cara init` / MCP `init` — bounded explicit repository initialization with typed receipts.
- `cara log [--limit N] [--kind KIND] [--pr N] [--since MS] [--until MS]` — bounded canonical event and hook-delivery journal snapshot.
- `cara log -f` — foreground existing-tail-then-follow stream; signal-aware and CLI-only.
- `cara mcp stdio` — expose typed command operations through `mcp-cli`.
- `cara mcp tools` — print MCP tool metadata.
- `cara self-update status|check|run` — `updatable-cli` release flow.
- feedback MCP tools — `feedback-cli` reporting and status.

## 6. Sync algorithm

A sync tick:

1. Acquire the local repository operation lock.
2. Discover and validate the fresh GitHub graph.
3. Reconcile merged heads: retarget the child to the default branch and enable squash auto-merge. No history rewrite is required.
4. Walk head → tail.
5. Ensure the head merges cleanly into current default.
6. Ensure every child merges cleanly into its declared predecessor.
7. Inspect CI/check state for each PR.
8. Enforce auto-merge on the head and off on all non-heads.
9. Recheck cross-caravan head/tail compatibility.
10. Emit events/hooks for observed transitions.
11. Return a stable health snapshot or the first decision point.

Already-correct steps are no-ops. Rerunning after interruption resumes from rediscovered GitHub state rather than a local cursor.

Sync never invents an agent decision. At an incompatible edge or unhandled CI failure, it stops like an interactive rebase, checks out the affected PR when safe, emits a structured decision point, fires its hook, and exits. After a user/agent pushes a repair or reshapes the chain, the next sync reaches the same edge, observes it fixed, and continues.

## 7. CI and merging

Normal behavior:

- Head: squash auto-merge enabled.
- Non-head: auto-merge disabled, even if it was enabled externally.
- Pending CI: sync reports waiting and makes no speculative repair.
- Failed CI without `caravan-force`: decision point.

A user/agent may repair and push, rerun failed checks, evict/split, or mark a known acceptable failure with `caravan-force`.

`caravan-force` is explicit operator intent to bypass any CI state that is not fully successful, including expected, queued, running, failed, unknown, mixed, or empty checks. When it becomes head, `cara sync` may force-squash it only when:

1. `.caravan/config.yaml` sets `force_merge: true`;
2. the open head has `caravan-force`;
3. it remains mechanically conflict-free with the default branch;
4. the authenticated actor has repository permission.

No approval hook or interactive reason is required. Before accepting the externally applied `caravan-force` label, sync/loop posts a durable generated-reason comment containing the exact observed checks (including pending, running, failed, mixed, or empty observations), enabled force policy, authenticated ADMIN permission, exact clean compatibility proof, and squash action. Comment failure is resumable and prevents the force merge. The attempt and result are emitted as audit events. Force never bypasses textual conflicts.

## 8. Decision points and errors

Every failure is typed and machine-readable through CLI JSON and MCP. A repair decision includes, when relevant:

- repository and default-branch revisions;
- caravan ID and full ordered PR graph;
- affected parent/child/head PRs and exact revisions;
- conflict/check/hook evidence;
- mutations already completed, if any;
- `resumable: true`;
- valid next commands.

Core decision kinds include:

- `head_conflict`;
- `link_conflict`;
- `cross_caravan_conflict`;
- `ci_failure`;
- `invalid_graph`;
- `stale_precondition`;
- `unsafe_checkout`;
- `hook_failure`;
- `force_merge_denied`.

Human output is concise; `--json` uses stable `mcp-cli` envelopes. Exit status is non-zero for every unresolved decision point.

## 9. Hooks

Configuration lives in the managed repository at `.caravan/config.yaml`.

Hook events include:

- `caravan_created`;
- `pr_joined`;
- `ready_pr_unqueued`;
- `sync_failed`;
- `join_failed`;
- `eviction_failed`;
- `head_advanced`;
- `evicted`;
- `split`;
- `ci_failed`;
- `force_merge_attempted`;
- `force_merge_completed`.

A hook is a configured shell command. It receives one versioned metadata JSON object on stdin and non-secret context such as `CARA_EVENT`, repository, and PR numbers in environment variables. Hook metadata contains operation/event IDs suitable for external deduplication.
Before hook delivery, every canonical secret-free event is durably appended with
its exact IDs to a versioned journal under common Git metadata. Secret-free hook
delivery status is appended afterward. Locked append/read, bounded rotation, and
torn-final-record recovery make this an audit surface only; it is never queue
state or cursor authority. Journal I/O errors report that completed provider
mutations were not rolled back.

Hooks may coordinate arbitrarily complex external workflows. Caravan does not wait for an agent protocol or hold a distributed lock. A decision-point sync always stops after firing its hook. A coordinator that outlives the hook process must own an external lock/dedupe record; repeated ticks may invoke the hook again, and the hook must no-op while that coordination is active.

Each hook has a timeout and `blocking` policy. Best-effort hook failure is reported but does not roll back a completed GitHub mutation. Blocking hook failure returns `hook_failure`; it still cannot roll back already-completed remote mutations.

Minimal config shape:

```yaml
version: 1
force_merge: false
rebase_on_join: false
agent_priority_labels:
  - caravan-priority:high
  - caravan-priority:normal
  - caravan-priority:low
command_timeout_secs: 30
loop:
  interval_secs: 60
hooks:
  sync_failed:
    command: ./scripts/on-caravan-sync-failed
    timeout_secs: 30
    blocking: false
```

Unknown config fields are errors. Secrets belong in environment variables, not committed YAML or hook metadata.

`rebase_on_join` is a strict, explicit history-rewrite opt-in and defaults to
`false`, preserving the virtual compatibility contract above. When enabled,
membership rebases the candidate-only linear range from its exact old base onto
the exact selected predecessor (or default for a new head) in an isolated
temporary worktree. Exact remote OIDs, same-repository ownership, a clean linear
range, conflict-free simulation, push permission, and the old head lease are
proved before `git push --force-with-lease=refs/heads/<branch>:<old-oid>`.
A moved branch, merge commit, ambiguous range, or conflict is a typed resumable
decision and is never forced. The command rediscovers GitHub after a push and
returns old/new OID and tree evidence together with provider receipts.

Provider CI configuration remains a repository precondition. GitHub Actions
`pull_request.branches` filters apply to the PR base: a workflow restricted to
the default branch will not run on a child targeting its parent. Cumulative mode requires a global `pull_request` trigger without `branches` or
`branches-ignore`, with `opened`, `synchronize`, `reopened`, `edited`, and
`labeled` activity types. A stack/full job may gate on default base or the
`caravan` label. `labeled` closes the race where `edited` occurs before the
membership label mutation. Missing trigger evidence is
`rebase_ci_trigger_missing`; an empty downstream check set is never passing.
Ancestry rewriting alone cannot cause checks to run.

Cumulative tree proof does not imply stable provider check identity. With the
current squash-merge heads, retargeting a child after its parent lands may
change GitHub's merge ref and rerun CI. Caravan does not claim instant no-rerun
landing without ancestry-preserving merges or an audited exact-tree/check
receipt policy.

## 10. Concurrency and idempotency

One local process at a time may mutate a repository, enforced by an operation lock under Git metadata. Read-only commands may run concurrently.

No local lock can serialize distributed machines. Every mutation therefore carries optimistic preconditions over PR number, head SHA, base ref/SHA, labels, state, and auto-merge state. A mismatch aborts with `stale_precondition`; the caller rediscover/reruns rather than overwriting concurrent work.

Multi-step remote mutations are not atomic. Errors report completed steps. The graph invariants and idempotent sync are the recovery mechanism; commands never hide partial remote progress.

## 11. MCP contract

The CLI and MCP tools share typed inputs, outputs, and domain errors. MCP exposes bounded single operations (`status`, `log`, `check`, `new`, `join`, `sync`, `evict`, and peers), not the unbounded `loop` or `log --follow` processes. An agent implements a long-lived loop by scheduling repeated `sync --all` calls or by running `cara loop` externally.

Tool descriptions must explain preconditions, side effects, decision-point behavior, and safe recovery. Self-update and feedback registrars are included in the same router.

## 12. Non-goals

Caravan v1 does not:

- prove semantic compatibility;
- rewrite branch history merely to maintain a chain unless the repository explicitly enables `rebase_on_join`;
- host or spawn a specific agent runtime;
- maintain a second authoritative queue database;
- provide a distributed consensus lock;
- support fork-only predecessor branches;
- bypass merge conflicts or repository permissions;
- guarantee hooks run exactly once.

## 13. Initial implementation boundary

The first skeleton establishes:

- the Rust `cara` binary and `caravan` library;
- clap command shape and agent-oriented help;
- shared typed CLI/MCP contracts and structured not-implemented decision errors;
- `mcp-cli`, `updatable-cli`, and `feedback-cli` integration;
- Nix build/dev shell, CI, and baseline tests.

Subsequent beads implement GitHub discovery, graph validation, compatibility, mutations, sync/CI, hooks/loop, and recovery behavior without changing this contract silently.
