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

Cacophony-shaped PRs additionally carry bounded `Cacophony-Generation`,
`Cacophony-Agent`, `Cacophony-Head`, `Cacophony-Stack-Base`,
`Cacophony-Stack-State`, and `Beads` metadata. Ordinary PRs without those markers
remain unaffected; an immutable `-pr-g<oid>` branch with missing/partial metadata
fails closed. Within one exact agent, overlapping bead stream, and stack slot,
Cara accepts a unique provider-proved contained successor or the newest exact
source when identities are equal. A current reviewed
`caravan-dogfood-controller` priority audit may explicitly link one canonical PR
to named superseded PRs when source objects are no longer provider-addressable;
a later priority-clear audit revokes that link. Different agents and declared
stack parent/child slots are never conflated. Older proven generations are
reported as `superseded_generation`, excluded from priority/FIFO and Saloon
Ready without blocking their canonical successor, and are never auto-closed.
Divergent, unproved, conflicting-link, or invalid siblings are
`ambiguous_generation`/`invalid_generation_metadata` and block for owner choice.
Active noncanonical generations become graph problems. Every existing-PR
membership path re-lists generation metadata, current reviewed links, and exact
source relationships immediately before branch/label/base/auto-merge mutation;
a newer generation, disappeared candidate, metadata drift, or uncertain compare
stops with zero writes and a safe close/reflect continuation.

`status`, JSON, MCP, and `next-candidate` expose the same complete ordered attempt list, the resolved label/rank, and a reason for every candidate. This is a selection contract, not a claim that candidate/default and cross-caravan compatibility preflight has passed. Automation must select the canonical first attempt for automatic admission, run `cara check` and the corresponding `new`/`join` preflight, and must not re-sort or leapfrog it if preflight rejects it. Explicit owner intent naming one exact PR is evaluated separately and may attach ahead of unrelated unjoined rows without changing their order (see *Admission selection versus admission intent*). The oldest selected eligible PR becomes a new head targeting the default branch; each later selected eligible PR appends at the tail. Rejection fails closed: repair or explicitly change the PR's GitHub state/labels, then rediscover the canonical list. More than one configured priority label on a PR, or any unknown `caravan-priority:*` label, also fails closed and excludes that PR with an explicit rejection reason.

An operator may override this automatic order for an explicit canary selection only by supplying a non-empty reason and recording that reason as a comment on the selected PR. A canary override does not alter the automatic policy or the canonical order subsequently reported by Caravan.

The strict opt-in `sync.actions.join_unlabelled_prs` policy is the one automatic
exception to no-leapfrog admission. After the existing fleet converges, sync
uses `priority_fifo_greedy_v1` to try canonical candidates and existing caravan
tails in deterministic order. A candidate incompatible with every target is
labelled `caravan-join-skipped` and receives a durable generation-bound comment,
then later candidates may be considered. The skip binds repository, candidate
head/base, default generation, every tested tail generation, config fingerprint,
compatibility reasons, heuristic version, actor, and time. Unchanged evidence is
not retried; any bound generation/config/heuristic change invalidates and removes
the skip. Explicit `new`/`join`/`rejoin` always consumes the advisory skip label.

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

Within one long-lived Cara process, exact prepared Git revisions are reusable across the initial, midpoint, and final rediscoveries of a tick. The bounded cache key includes the canonical local repository, remote, provider repository, branch, and exact OID; an entry expires 600 seconds after preparation and the process retains at most 4096 entries. Cache access never refreshes that age. Provider/GitHub rediscovery and pairwise merge-tree analysis still run on every read. An unchanged exact generation may reuse its already-fetched local commit, while any repository, remote, branch, or OID movement misses and performs the ordinary pre/post-advertisement fetch verification. Expiry bounds local object-pruning or garbage-collection assumptions; the cache is an optimization, never provider or mutation authority.

The default branch may move independently. A caravan head that no longer merges cleanly into it is a decision point. A valid repair may change the PR, reshape the caravan, or land an outside-caravan fix on the default branch that restores compatibility without rerunning caravan PR CI.

### Squash-equivalent stacked history

A landed member arrives on the default branch as **one** squash commit. Its
content is identical to that member's cumulative content, but its commit
identity is unrelated to the pre-squash commits every surviving later member
still carries. Replaying that stacked history against the target therefore
re-applies changes the target already holds, and Git's own patch-identity
pruning cannot help once one squash combined several source commits.

Every non-clean attachment check — head to default, adjacent child to parent,
and one caravan head after another caravan tail — therefore also produces exact
squash-equivalence evidence for the same revisions. The evidence answers one
question: is there an ancestor-closed linear prefix of the candidate-only range
whose cumulative content the target already holds byte for byte, and does
replaying only the retained commits from that proven boundary merge cleanly?

The proof is exact and narrow:

1. Every path the prefix's cumulative diff changes — additions, modifications,
   deletions, and file modes — must be identical on the exact target tip. A
   path the prefix never touched is not evidence, so a vacuous match on an
   unrelated identical file proves nothing.
2. Commit messages, subjects, authorship, dates, and patch text are never
   proof. An identical patch that yields a different resulting blob is not
   equivalence.
3. Replaying the retained commits with the proven boundary as merge base must
   be independently clean.

Outcomes are `reconcilable`, `no_equivalence`, `residual_conflict` (the prefix
is represented but the retained commits still diverge), and `indeterminate`
(absent or ambiguous merge base, non-linear candidate range, or a path Git
cannot represent exactly). Only `reconcilable` authorizes a boundary, and
ordinary three-way divergence after the equality point — merge base, target
tip, and candidate head all distinct for the same file — is never reconciled.
Nothing is ever resolved by taking either side.

Detection is not authority. Evidence alone never rewrites a live provider
branch: reconciliation is applied only by an explicitly authorized rewrite,
which additionally reverifies that the replayed head tree equals the proven
cumulative tree and that the rebuilt commit count equals the proven retained
set, failing closed before any push. The receipt records the dropped and
retained commits, the proven boundary and its tree, the exact represented paths
with their blobs and modes, and the cumulative tree before and after
reconciliation.

## 4. Discovery and identity

Provider access is authenticated. Cara accepts an explicit ambient
`GH_TOKEN`/`GITHUB_TOKEN`, otherwise resolves a repository-accessible `gh auth`
account and injects that token only into provider subprocesses. Tokens and
credentials never enter command diagnostics, JSON/MCP receipts, hooks, or the
journal. An explicit ambient token is validated by the first real provider
request rather than an additional per-process access probe.

Every bounded provider operation accumulates secret-free API telemetry: auth
source class, total GraphQL/REST/gh-CLI calls, and the latest GraphQL
cost/remaining/reset observation. Queries collect rate-limit evidence in-band.
A GitHub App installation token is accepted through the same ambient-token path
and may identify its source through a non-secret runtime hint; app secrets never
belong in repository config. Automation should use one event-driven loop,
coalesce wakes, reuse one exact discovery snapshot within a tick, back off near
reset, and retain exact fresh reads before mutation. Cache data is never
provider mutation authority.

Before config discovery or any mutation, `cara` resolves the exact non-bare Git worktree root with a bounded noninteractive Git query. Root and nested invocations share that repository identity, default `.caravan/config.yaml`, locks, journal, repair state, status cache, and domain behavior. Linked worktrees retain their own worktree root while common-Git state uses Git's common directory. A relative explicit `--config` remains relative to the invocation directory and is converted to an absolute identity; outside Git and bare repositories return `repository_not_found` and write nothing.

### Rolling config compatibility

Repository policy remains strict: unknown top-level and nested keys are rejected,
so misspellings and unaudited security policy never silently degrade. Every newly
generated config declares `min_cara_version`. Before strict schema parsing, Cara
reads only that audited gate; a reader below the floor returns the typed,
non-mutating `cara_upgrade_required` error with running/required versions and an
upgrade action. This ordering also gives a stable upgrade diagnostic when the
new policy contains sections unknown to the older reader contract.

Adding a policy section requires advancing `min_cara_version` to its first Cara
release. A consuming repository must update its pinned Cara runtime and config in
the same change, and CI must run `CaravanConfig::check_reader_compatibility` (or
parse the repository config with that exact pinned binary) before merge. Lowering
the gate or adding a section without advancing it is not a supported rollout.
Old configs without the gate remain readable with safe defaults; arbitrary
unknown fields never become an extension mechanism.

`cara` discovers the base repository, default branch, current local branch, PRs, labels, bases, head revisions, auto-merge state, and checks through `git` and authenticated `gh`/GitHub APIs.

Graph identity is derived each run. No UUID is persisted. A hook receives the complete relevant graph snapshot so a changing head/ID is explicit.

A dangling base is reconciled when it belongs to a just-merged labelled predecessor. Its child is advanced to the default branch. Any other dangling, branching, cyclic, or ambiguous graph is invalid and requires repair.

## 5. Command contract

JSON, MCP, hooks, loops, and non-TTY CLI calls are non-interactive. Human CLI
membership commands may provide a terminal-only branch/commit/publish/PR
assistant; it is never active under JSON/MCP or without a controlling TTY, and
every Git/provider mutation remains confirmed and subject to the same exact
preconditions.

### Repository initialization

- `cara init` is the only automatic first-use mutation surface.
- When `.caravan/config.yaml` is absent it is created atomically with version-1
  defaults and the running release in `min_cara_version`. An existing valid file is preserved byte-for-byte; incompatible or
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
  member), and `caravan-force` (`D93F0B`, operator force exception). When
  `sync.actions.join_unlabelled_prs` is enabled it also requires
  `caravan-join-skipped` (`6F42C1`, generation-bound optimiser skip). Every
  configured `agent_priority_labels` entry follows in configured order. Priority-label
  colors come from the stable rank palette `B60205`, `D93F0B`, `FBCA04`,
  `0E8A16`, `1D76DB`, `5319E7` (cycling for additional ranks), and descriptions
  identify the one-based rank and highest-priority direction.
- Before the first missing-label mutation, init reads the authenticated GitHub
  `rate_limit` resources and distinguishes REST `core` from GraphQL. It requires
  a conservative request budget for all planned create and authoritative reread
  steps. If core remaining is insufficient, init returns typed
  `github_rest_rate_limit_wait` evidence with limit/used/remaining/reset,
  retry delay, completed/pending labels, and `mutation=false`; it does not issue
  a label create, wake repair, or encourage hot-looping. Verification-only init
  with every exact label present needs no rate probe.
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

`status.analysis.fleet.caravans` answers **now**, never **ever**: an empty list
means no caravan is in flight at this instant. `fleet.history` carries the
lifetime answer from the same bounded snapshot — `merged_members_observed`,
`earliest_merged_at`, `latest_merged_at` — so a current-state read can never be
mistaken for a lifetime claim. `unlabelled_merged_rows` counts merged rows the
label-filtered query returned whose own records carry no caravan label, which is
the signature of labels stripped after the fact. A zero count beside a non-zero
`unlabelled_merged_rows` means the history is UNPROVEN, never proven empty. Human `cara status` prints "N in flight now" and,
when the list is empty, states explicitly that this is not a lifetime claim.
- `cara next-candidate` — return the same canonical first ordered admission attempt and complete reasoning without mutation; it explicitly requires subsequent membership preflight and never authorizes leapfrogging a rejected first attempt.
- `cara show` — print the current PR's whole caravan and highlight its position.
- `cara check` — no-update validation. For an active member, check its whole caravan and fleet invariants. For an unenrolled candidate with no explicit target, first evaluate attachment to the one visible unheld caravan. A clean attachment returns the complete coherent `join` receipt; an ineligible attachment falls back to the ordinary `new` evaluation. Zero or multiple visible caravans retain `new` evaluation because a subsequent targetless `cara join` could not resolve the same target unambiguously. This recommendation policy is check-only: an explicit `cara new` mutation continues to preflight `new`.
- `cara check --pr N` — re-read and preflight exact remote PR `N` without changing checkout, branch, base, labels, or auto-merge. The receipt consumes the canonical provider candidate identity/freshness schema and includes exact head/base repositories and OIDs, draft/labels/auto-merge state, enrollment and canonical-order state, compatibility/conflicting paths, and one mechanical next action: `new`, `join`, `repair`, `wait`, or `reject`. When targetless check recommends the unique visible caravan, `mode`, `caravan_id`, `target_pr`, compatibility, and typed admission intent all describe that same join; it never flips only `next_action` on a new-caravan receipt. A provider head/ref race fails closed with exact old/new evidence.

Candidate freshness is always measured against a live provider generation, never against the pull request's own recorded base. GitHub keeps serving the base a PR was opened or last synced against, so a default-based candidate is compared to the current default tip; every identity records that `compared_base` generation so a consumer can verify the claim instead of trusting it. A recorded base superseded by the current tip is `stale_base` with an explicit reason, including when the synthetic merge ref is missing or has unexpected topology.
- `cara check [--pr N] --tail-pr T` — check whether the selected remote/current PR can join after exact tail `T`.
- `cara check [--pr N] --head-pr H` — resolve caravan `H`, then check against its current tail.

Remote `--pr` preflight reports the canonical first priority/FIFO attempt as evidence and evaluates the named candidate as explicit owner intent. Structurally ineligible PRs never enter that ordered attempt list: drafts, fork-only heads, externally enabled auto-merge, and superseded/ambiguous/invalid Cacophony generations are reported with exact reasons and excluded, so one wedged PR cannot starve every other owner. Unknown or conflicting configured priority labels still block, because canonical rank cannot be computed. An eligible candidate whose exact mechanical attempt fails remains canonical for *automatic* selection, which never silently leapfrogs it. Already enrolled candidates are reported without mutation.

#### Admission selection versus admission intent

Two axes are specified separately, because conflating them regressed this
behaviour once already (0.0.10 recognized explicit intent and then applied FIFO
anyway):

- **Selection** is *who chose this candidate*: `automatic` priority/FIFO order,
  an `explicit` owner request naming one exact remote PR, or the owner's own
  `checked_out` PR.
- **Intent** is *what the exact receipt evaluates*: `new` (form a caravan) or
  `join` (attach to a resolved live target). A targetless `check` may recommend
  `join` to the one unambiguous visible unheld caravan; mutation preflight keeps
  the operation the caller actually requested.

Priority-then-FIFO is the contract for *automatic selection*: which unjoined PR
sync picks next as the new root or the next automatically grown member. It binds
automatic selection for `new` and `join` intent alike, with no exception. It was
never a claim that an owner naming one exact PR must first wait for every
unrelated unjoined PR ahead of it, for either intent.

Cara therefore resolves selection and intent *before* FIFO canonical-candidate
rejection. An explicit owner request (`cara check --pr N`, with or without
`--tail-pr`/`--head-pr`) may attach ahead of earlier ordered rows only while
every bypassed row is an unrelated, unjoined first-admission attempt. Those rows
keep their canonical order and are still admitted in turn. An owner operating on
their own checked-out PR — local `cara check` and every membership operation,
including `renew`/`rejoin` — reports canonical position as evidence only and has
never been gated by it.

The relaxation is exact and fails closed. A row is never bypassed when it is an
active caravan member, when it is on the candidate's exact base chain, or when
its canonical rank cannot be computed (unknown/conflicting priority metadata).
A candidate that is not itself a current ordered admission attempt — rejected,
stale-pinned, or carrying a generation-bound skip — gains nothing from declaring
intent. An explicitly named unresolved, missing, or ambiguous join target is an
error before ordering is considered, never a guess. A targetless recommendation
uses only one unambiguous visible caravan; otherwise it evaluates `new`. Ordering never substitutes for
compatibility, dependency, policy, provider-candidate freshness, generation
integrity, or provider/auth success: those still reject the attach.

Reviewed operator resolution `choice-019f9d34` requires intent to be resolved
before ordering; reviewed resolution for `bd-7099e8` fixes the axis it was
applied to. Canonical position is reported as non-blocking `canonical_candidate`
plus `admission_note` evidence on every remote receipt, and explicit owner
selection is admitted ahead of unrelated unjoined rows for `new` as well as
`join`. Automatic selection is unchanged.

An owner may declare a canonical generation directly (bd-523dbf). A durable
provider comment carrying the `caravan-generation-supersession:v1` marker, bound
to the canonical PR **and its exact head**, names the superseded PRs. Generation
analysis prefers that declaration over the historical priority-comment
heuristic, and ignores a declaration written for a different head. This replaces
the previous recovery path, where a dead-ended diverged stream could only be
resolved by publishing a hand-crafted containment merge.

Every check receipt and every membership receipt carries a typed
`admission_intent` decision recording selection, intent, resolved target caravan
and tail, the canonical candidate at decision time, each ordered row ahead with
its exact disposition (`bypassed_unjoined`, `blocked_joined`,
`blocked_dependency`, `blocked_rank_indeterminate`, `blocked_automatic_order`),
compatibility and preflight cleanliness, provider mutation, and idempotency. The
outcome is one of `canonical`, `explicit_ahead_of_unjoined`, `owner_selected`,
`already_enrolled`, `blocked_by_order`, or `blocked_by_preflight`. The human
`admission_note` is derived from that same decision, so the CLI note, the typed
decision, and the mutation behaviour cannot disagree. The durable control-label
audit comment records the same selection, intent, order outcome, and reason.
`cara next-candidate` publishes the matching `automatic_selection` decision, so
the automatic and explicit surfaces can be compared directly from JSON.

`--tail-pr` names the intended merge target. `--head-pr` names a caravan whose tail is resolved at execution time. They are mutually exclusive.

### Per-pass tick receipts

Every `cara sync` emits one compact leading line naming the verb and the counts
that pass observed (bd-180cd3):

```
tick: verb=sync caravans=3 unqueued=7 synchronized=3 joins=0 changed=false
```

It exists because a fleet once spent hours unable to distinguish *the loop is
running and declining to join* from *the loop is not running at all*, testing
hypothesis after hypothesis against a process that did not exist. Non-zero
`unqueued` with zero `joins` is the first state; absence of the line entirely is
the second. The same facts are carried structurally as `tick` on `SyncOutput`
for `--json` and MCP callers.

A bare `while true; do cara sync; sleep N; done` is not a supervision strategy —
it dies with its terminal, silently. Schedulers should supervise the loop
(launchd, systemd, tmux) and retain these per-pass receipts, so "no receipts
since T" is itself the alarm.

### CI admission gate

`cara ci-gate --pr N [--head-evidence]` answers one question for a consuming
workflow: is existing CI evidence still exactly valid for this pull request
generation (bd-2a29c8). It returns `ci_valid`, `ci_required`, `ci_force_accepted`,
`ci_not_applicable`, or `ci_unknown`, plus `run_ci` and the exact evidence the
decision rests on, so a skipped run is auditable.

The gate is advisory about **cost**, never about **safety**. It may only assert
that exact existing evidence still applies; it may never assert that a head can
merge without evidence. `--head-evidence` is the caller's statement that a prior
successful required-check run exists for that exact head, and without it the
answer is always `ci_required`. Anything unproven is `ci_unknown`, which runs CI.
The bundled `.github/actions/ci-gate` composite action fails open to running CI
when Cara itself cannot answer.

### Creation and membership

- `cara new [--pr N | --create-pr]` — create a one-PR caravan from one exact remote Saloon PR, the current branch's unique open PR, or an explicitly created topic-branch PR.
- `cara join [--pr N] [--tail-pr N | --head-pr N] [--create-pr]` — append an exact remote/current PR to a valid tail.
- `cara renew [--pr N | --create-pr]` — reevaluate an exact evicted PR as a new caravan.
- `cara rejoin [--pr N] [--tail-pr N | --head-pr N] [--create-pr]` — reevaluate an exact evicted PR and append it.
- `cara priority set --pr N --label LABEL --actor A --reason R` — set one exact configured automatic-admission priority on an unenrolled PR.
- `cara priority clear --pr N --actor A --reason R` — remove configured priority metadata and restore FIFO ordering.

Without a current PR, automation fails unless exact `--pr N` or explicit `--create-pr` is set. Default-branch `HEAD` is never treated as an empty membership source. Creation uses the non-interactive equivalent of `gh pr create --fill` and then continues. In a human TTY, `new` and local `join` offer that continuation automatically. They publish an unpublished topic branch only after confirmation. When invoked from the default branch, the assistant displays the complete short status and may, after confirmation, create a topic branch, stage all displayed changes, commit with an editable default message, push, create the PR, and resume. A clean default branch can only become a new topic branch; Cara explains that a real commit is required and never fabricates an empty change. `NO_COLOR` suppresses ANSI styling; terminal OSC-8 PR links and titles are presentation-only and never enter JSON/MCP.

`join`/`rejoin` without a target succeed only when exactly one valid caravan tail exists; otherwise they return candidate tails and require an explicit target.

An explicit create on a branch previously used by exactly one same-repository merged unlabelled PR may proceed only when the current local and provider branch agree on a new head, the historical head is its ancestor, and the new generation differs from the merged head. The merged PR remains untouched and unlabelled. Multiple historical PRs, unchanged/unpublished generations, non-ancestry, deleted branches, and forks fail closed. Plain membership without create retains strict historical-navigation refusal.

`new`/`join` reject active or evicted PRs, closed PRs, PRs with auto-merge already enabled, and stale/incompatible graphs. `renew`/`rejoin` additionally remove `caravan-evicted` only after all other preconditions pass.

A new caravan applies `caravan`, targets the default branch, verifies fleet compatibility, and enables squash auto-merge. A historically reused branch may enter as a fresh root only when exactly one OPEN same-repository PR has provider, remote-ref, and local head OIDs equal and older history has no Caravan/evicted membership; duplicate open reuse, forks, mismatch, or conflicting retained membership fail closed. Joining retargets the PR to the tail branch, applies `caravan`, disables auto-merge, and verifies the resulting fleet. Membership commands accept `--reason TEXT` and `--priority-label LABEL`; an explicit label must exactly match a configured `agent_priority_labels` entry, while omission records FIFO admission.

Priority control is a separate audited scheduling mutation, not admission authority. It is persistent metadata that orders AUTOMATIC selection; it never grants permission to execute an explicit membership action, and an explicit `cara join --pr N --tail-pr T` (or the `new`/`rejoin` equivalent) already carries owner authority on its own. Setting a priority label to "unblock" an explicit operation is a misreading: the explicit operation owns exact mechanical and provider preflight, and the label only changes where the candidate would sit in automatic order (bd-72d56d). Set/clear requires an exact open, non-draft, same-repository, unenrolled, non-evicted PR; one configured label; fresh PR/config facts; repository write permission; and a durable before/after comment. It removes only competing configured priority labels and never changes membership, force, eviction, skip, auto-merge, base, or unrelated labels. Unknown/conflicting existing priority metadata fails closed. Exact retries are label no-ops while ensuring the audit remains durable. Priority changes canonical automatic order but never bypass compatibility or authorize an explicit join.

With `rebase_on_join: true`, every successful `new`/`renew`/`join`/`rejoin` emits the same exact `join_receipt` admission contract. Root operations encode the authoritative default branch as predecessor with sentinel PR number `0`; branch/OID, physical rebase, ancestry, membership, force-intent, config, provider receipts, and deterministic receipt hash remain mandatory. A root success without that complete receipt is `atomic_membership_receipt_incomplete`, never a partial success (bd-d15ba3).

Every successful mutation of `caravan`, `caravan-evicted`, `caravan-force`, `caravan-join-skipped`, or a configured priority label is completed by a durable PR comment. The comment records operation, before/after labels, actor and reason source, exact compatibility and clean-squash evidence where applicable, and explicit configured label/rank or canonical FIFO basis. A deterministic `caravan-control-label-audit` HTML marker fingerprints operation, PR/head, and the before→after control-label transition; GitHub-visible latest-transition evidence deduplicates partial retries without conflating a later transition on the same head. Comment failure after labels changed returns `github_comment_failed` with completed receipts and a resumable rerun instruction; it is never reported as full success.

### Navigation

- `cara next` — check out the labelled child of the current PR; error at the tail.
- `cara prev` — check out the predecessor/base PR; error at the head.

Navigation refuses dirty worktrees, in-progress Git operations, ambiguous PR mappings, and unsafe branch switches. A clean non-current destination branch may legitimately lag after Cara physically rewrote its provider head. Navigation fetches and reverifies that exact provider OID, atomically retains the stale local OID under a deterministic `refs/cara-backup/navigation/*` ref while advancing the named local branch, reports the complete reconciliation receipt, and only then checks it out. It never rewrites the currently checked-out branch, a branch checked out by another worktree, or an unpreserved local generation. When the checked-out branch still names an exact, retained, same-repository merged Caravan PR, discovery follows bounded base-ref change history through merged members to its unique active rolling successor. `show` reports both the historical predecessor and the active chain position; chain and fleet `next` enter that successor. Closed-unmerged or unlabelled history, branch reuse, deleted/fork-only heads, provider races, ambiguous successors, and exhausted history all fail closed with typed evidence. A valid historical predecessor with no successor reports `historical_successor_not_found` rather than silently entering another caravan.

`cara van` is the fleet-level command prefix:

- `cara van list` — list caravans.
- `cara van next` / `cara van prev` — check out the next/previous caravan head in deterministic status order.

From the repository's default branch, where there is no current PR, `cara van next` enters the first caravan head and `cara van prev` reports the lower navigation boundary. Chain-level `cara next`/`prev` and fleet navigation from any other non-PR branch still require a current caravan member.

Fleet navigation is browsing, not queue priority. V1 browses heads by PR number. Admission records either an explicit configured priority label or FIFO (oldest eligible PR first); this does not change browsing order.

### Reshaping

- `cara evict [--pr N] --reason TEXT` — remove a member (current PR by default).
- `cara split [--pr N]` — make a non-head member the head of a new caravan.

`cara evict --cascade` also releases every member after the selected PR, and
`cara evict --all` dissolves the whole caravan (bd-e9187e). Both release members
**tail-first**, so no surviving edge is ever re-linked across a removed member
and each step is an ordinary audited eviction with its own receipts. A refusal
stops the sequence and returns `eviction_cascade_interrupted`, naming the
members already released and the one that failed, so an interrupted cascade is
resumable rather than a silently half-dissolved chain. `--cascade` and `--all`
are mutually exclusive.

`cara plan concat --source-head-pr S --target-tail-pr T --actor A --reason R`
produces the immutable no-write recovery plan for appending one entire live
source caravan after a target tail. It binds exact repository, source/target
caravans, old/new ordering, every source branch/head/base and predecessor,
complete rewrite and rollback scope, actor/reason, and a stable plan hash.
`cara concat ... --expected-plan-hash H` rechecks that plan under one writer
operation, prepares the whole source chain, atomically moves every source ref
under exact force-with-lease, authoritatively rediscovers each rewritten head,
and commits one source-root base edit that merges the fleets. It never sequences
evict and rejoin. Failure before membership atomically restores source heads;
final verification failure restores the exact membership after-state and then
all original heads. Ambiguous compensation never claims success. The successful
event stores plan, physical, membership, operation and ordering receipts, so an
exact retry returns the original evidence without a provider call. Cycles,
forks, holds, conflicts, missing topology and stale plan hashes fail closed.

Eviction also unwinds the evicted member out of its descendants (bd-cef612).
Physical joins rebased each member onto its predecessor, so retargeting alone
leaves the evicted patch inside every descendant, which would silently
reintroduce discarded content when one lands. Each descendant is therefore
replayed strictly after the evicted head onto the surviving predecessor (or the
default branch), dropping exactly that member's commits while preserving the
descendant's own work, and the chain is rebuilt in order. Every rewrite is
proven before any is published, so a descendant that cannot be unwound cleanly
leaves the whole stack untouched. The receipt carries per-descendant rewrites;
when repository access or `rebase_on_join` is unavailable, the receipt instead
names the descendants that still carry the evicted patch rather than implying a
removal that did not happen.

Eviction adds `caravan-evicted`, removes `caravan` and `caravan-force`, and disables auto-merge. If the evicted PR has a child, that child is retargeted to the evicted PR's predecessor (or the default branch when evicting the head), but only if the new edge and fleet are compatible. The command fails before mutation if it cannot safely close the gap. Evicting a tail needs no rejoin.

Splitting retargets the selected non-head to the default branch, making it a new head. Both resulting caravans must satisfy all graph and fleet compatibility invariants.

### Synchronization

- `cara plan sync` / `cara plan sync --all` — run fresh physical conflict,
  dry-run lease, CI, convergence, and first auto-admission selection preflight
  without provider writes; return ordered exact actions and rediscovery barriers.
- `cara sync` — synchronize the current caravan.
- `cara sync --all` — synchronize every non-paused caravan in deterministic head order. Human invocations of `sync` and `loop` stream bounded stage progress to stderr — initial discovery, physical planning and apply, midpoint revalidation, provider convergence, auto-admission, and final rediscovery — so a long network tick is never silent. Progress is observational only: no policy depends on it, details are truncated, and JSON/MCP callers install no observer and keep byte-identical envelopes.
- `cara repair start --pr N [--target-pr T]` — create or reuse a durable isolated provider-owned workspace at PR `N`'s exact head, and start an exact-target non-committing merge. The target is current default when omitted.
- `cara repair status --session ID` — inspect the persisted exact head/target/conflict/workspace/publication receipt without mutation.
- `cara repair authorize-agent-edits --session ID --actor A --reason R [--expires-secs N]` — after exact session/config/provider head+target revalidation, authorize one identity to stage bounded arbitrary repository-content edits in the isolated workspace. Persist repository/PR/head/target/session/config/manifest/actor/reason/expiry; no provider mutation.
- `cara repair grant --session ID --path P... --source-revision SHA --actor A --reason R [--expires-secs N]` — after exact session/head/target/config/provider revalidation, three-way apply and stage reviewed changes from one exact source commit to bounded tracked regular paths. Persist actor/reason/source parent+blobs+patch fingerprint/original+expected result OIDs/expiry; no provider mutation.
- `cara repair revoke-grant --session ID --path P... --actor A --reason R` — before continue, the exact granting actor may revoke paths; Cara restores/stages their pre-grant blobs, removes receipts, and records bounded local-only revocation evidence.
- `cara repair continue --session ID [--actor A] [--no-sync]` — verify and commit typed conflict/grant edits plus any exact session-authorized agent edits. Broad edits require matching actor and unexpired authority; Cara records bounded path/staged-index/binary-diff fingerprints, publishes by non-force fast-forward under exact remote-head checks, marks fresh CI required, then resumes `sync --all` unless explicitly suppressed.
- `cara repair abort --session ID --confirm` — after explicit review, remove only the local persisted workspace/session; provider state is never changed.
- `cara pause --head-pr N --actor A --reason R` — place an explicit incident or maintenance hold on one exact caravan and disable only its head auto-merge.
- `cara resume --head-pr N --actor A` — explicitly revalidate and release that hold.
- `cara loop` — repeatedly run `sync --all` at the configured interval. A failed tick is bounded evidence, not a stop condition: canonical events are dispatched to configured hooks and the loop keeps ticking so retryable provider races, moved default branches, and unresolved external decisions converge without an operator restart. Only an explicit stop signal ends it, and the summary reports total, failed, and consecutive-failure counts plus bounded recent-failure receipts. `loop --once` remains a single bounded tick and still returns its typed error.
- `cara loop --manual [--shell COMMAND]` — CLI-only human controller. At an exact `external_decision`, persist private bounded decision JSON, release the operation lock, inherit the controlling TTY in a safe affected/repair workspace, and run `$SHELL -i` or the explicit command. Zero exit triggers fresh rediscovery and another exact tick; nonzero stops with evidence. Refuse JSON/MCP/non-TTY use.

`loop` is a lightweight foreground daemon. It keeps no authority beyond GitHub and performs the same idempotent ticks as direct `sync --all` calls.

### Incident and maintenance holds

A pause stores bounded, non-secret evidence below Git's shared common metadata directory: the rolling head and members, exact head SHA/base/labels, observed checks, actor, reason, creation time, optional expiry, and optional external incident/choice reference. The remote mutation uses the complete PR precondition and changes only head auto-merge. Branches, labels, bases, children, and PR heads are preserved.

Status classifies a matching hold as `active` or `expired` and suspends only the exact missing-head-auto-merge problem. Expiry is an alert, never authority to resume. Cycles, branching, closure, changed heads/bases/labels, incompatible links, and non-head auto-merge remain graph failures; stale hold evidence is reported and fails closed. Provider truth outranks every durable local record: once the exact recorded head is merged or closed, the hold becomes `retired` historical evidence that can never be resumed, never suspends an invariant, never presents as an active caravan, and never requests auto-merge repair on an unmergeable pull request. A checkpoint is recovery evidence, not mutation authority, so oversized checkpoint evidence after a completed provider merge is persisted as a bounded digest with counts, keys, and an exact original hash rather than failing the operation. `sync --all` emits a no-op skip receipt for held caravans and continues independent caravans. A targeted sync returns the same bounded no-op continuation.

Resume is operator/agent initiated only. Before enabling squash auto-merge it revalidates the exact head, base, labels, open state, membership topology, compatibility, and safe terminal checks. An authorization marker makes interrupted resume retries idempotent without treating an external re-enable as valid. Every pause/resume authorization is appended to a secret-free local audit record. No loop, expiry timer, status call, or sync call may remove a hold or enable its head.

### Ecosystem surfaces

- `cara help` — agent operating instructions and recovery examples.
- `cara init` / MCP `init` — bounded explicit repository initialization with typed receipts.
- `cara log [--limit N] [--kind KIND] [--pr N] [--since MS] [--until MS]` — bounded canonical event and hook-delivery journal snapshot.
- `cara log -f` — foreground existing-tail-then-follow stream; signal-aware and CLI-only.
- `cara mcp stdio` — expose typed command operations through `mcp-cli`.
- `cara mcp tools` — print MCP tool metadata.
- `cara self-update status|check|run` — `updatable-cli` release flow, bound to the exact running first-PATH-visible stable user binary. `~/.cargo/bin` and `~/.local/bin` update in place; shadowed, development, renamed/test, and package-manager binaries fail closed unless an exact active parent is explicitly selected with `CARA_SELF_UPDATE_INSTALL_DIR`.
- feedback MCP tools — `feedback-cli` reporting and status.
- `cara web --repo PATH...` — loopback path-scoped dashboard, typed actions/plans/progress/journal, optional authenticated GitHub webhook wake receiver, collapsible repository/attention sidebars, and a center Saloon for unenrolled PRs. Saloon groups are ordered Ready, Conflicting, Saddling Up, Other, Bounty List, with independent per-repository disclosure state retained across polling renders. Priority/FIFO candidacy never implies mechanical readiness: under fixed candidate/target/pair and wall-clock bounds, each candidate is checked against exact current default and every bounded live tail, then shows `Ready (main, PR #tail...)`, `Conflicting (main, PR #tail...)`, or checking/unknown evidence. Mixed candidates expose both lists and actions select only an exact clean target; stale, failed, truncated, or unevaluated projections are never Ready. Fresh candidate evidence returns a fixed unjoined PR to Ready, while draft/incomplete and skipped/evicted PRs remain separate groups. Eligible active heads expose audited typed Force/Unforce controls; eligible unenrolled PRs expose configured priority/FIFO controls. Both require explicit actor/reason confirmation and use the same exact domain operations as CLI/MCP, never raw label mutation. Accepted actions bind the reviewed compatibility projection, refresh sequence, and deterministic mutation-authority fingerprint over exact config/provider/topology facts. Refreshes coalesce behind active jobs; sequence-only drift with an identical fingerprint is harmless, while real drift fails before mutation with expected/actual fingerprints.

The webhook endpoint is disabled unless an operator supplies a secret environment
variable name and exact GitHub App installation ID. It bounds the body and
headers, verifies `X-Hub-Signature-256` in constant time, matches one explicit
repository, and durably deduplicates delivery IDs in common Git state. Accepted
default push, PR lifecycle, check-suite/check-run, and workflow-run events trigger
a coalesced status refresh or one configured bounded sync-all action. Webhooks
are never provider authority: every wake performs fresh discovery and ordinary
lock/budget/precondition enforcement. Invalid/unknown events mutate nothing;
periodic polling remains fallback reconciliation. Secrets never enter config,
status, journal, logs, or receipts. Deliveries route by the configured
`repository: owner/name` when set, falling back to observed status only for
repositories that declare none, so a repository whose provider read has not yet
succeeded is still routable rather than silently webhook-deaf.

`cara web --hosted` is the optional deployment contract for pre-provisioned
checkouts. It requires a signed webhook secret, one exact installation,
`--webhook-sync`, no `--read-only`, and for every served repository
`github_auth.mode: app_installation` pinned to that same installation,
`writer.mode: remote_fenced`, and an exact slug; mixed installations, ambient
auth, `local_only`, a missing slug, two worktrees declaring one slug, or missing
broker/host/writer identity all fail closed at startup. Hosted workers mutate
only from HMAC-verified deliveries: interactive mutating actions are refused,
because the same-origin CSRF token is cross-site protection rather than
authentication, so reachability through an operator proxy is never authority to
force, merge, or reshape. Non-mutating check/plan actions remain available.
Hosted mode provisions no clones, manages no tenancy, and performs no failover.

`GET /api/v1/health` is secret-free and monitorable: `ok` means this process is
serving, while `degraded` is the actionable signal, true when any served
repository has never refreshed successfully or is currently carrying a refresh
error. It also reports the hosted/read-only flags, repository counts including
never-refreshed and erroring, the oldest successful refresh, and webhook
counters with the last received timestamp, so a worker that is answering but
idle -- because deliveries stopped or a repository read keeps failing -- is
distinguishable from a healthy one.

## 6. Sync algorithm

A sync tick:

1. Acquire the local repository operation lock.
2. Discover and validate the fresh GitHub graph.
3. Reconcile merged heads: promote the child to the exact default branch in one fenced transaction. No history rewrite is required.
4. Walk head → tail.
5. Ensure the head merges cleanly into current default.
6. Ensure every child merges cleanly into its declared predecessor.
7. Inspect CI/check state for each PR.
8. Enforce exactly one merge actor: under `head_merge_actor: caravan` no member carries native auto-merge and Cara performs the bounded squash merges itself; under `github` exactly the root is armed.
9. Recheck cross-caravan head/tail compatibility.
10. Emit events/hooks for observed transitions.
11. When `sync --all` auto-admission is enabled, rediscover and greedily admit
    canonical unlabelled candidates only after steps 1–10 converge. Empty fleets
    form a head; non-empty fleets use the first compatible deterministic tail.
    Incompatible exact generations receive a durable skip and later candidates
    may be considered. Before beginning another candidate, preserve a bounded
    nonzero exact-Git reserve; exhaustion returns continuation without starting
    a doomed final fetch.
12. Re-run normal convergence for admitted members and return exact joins,
    skips, remaining candidates, safety-budget usage, continuation, and the
    stable health snapshot or first decision point.

Already-correct steps are no-ops. Rerunning after interruption resumes from rediscovered GitHub state rather than a local cursor. Auto-admission is disabled by default and targeted `sync` never grows the fleet. Fleet scanning shares one operation lock, absolute wall-clock deadline, authenticated `gh` request counter, candidate limit, and mutation limit. A selected exact candidate receives an independent bounded `command_timeout_secs` deadline for provider refetch, merge identity, compatibility/physical Git, mutation, and post-mutation rediscovery; sync-owned admission reuses the already-fresh fleet snapshot instead of repeating unrelated cross-caravan analysis. Exact candidate receipts expose reserved and remaining milliseconds. Provider/head/base drift still fails closed, and a post-mutation refresh cannot inherit an exhausted pre-admission deadline. Budget exhaustion returns a resumable continuation rather than leapfrogging or guessing. `loop` and `loop --once` call this exact path. Hosted automation uses bounded once-ticks from PR/check/workflow/default-branch events and a schedule, not one unbounded hosted process.

Sync never invents an agent decision. Every successful bounded tick returns a versioned scheduler projection over the fresh final discovery: exact default branch generation, each selected caravan's root/tail/member head and base generations, observed CI disposition, intentional holds, and one `healthy`, `waiting_ci`, or `held` state. These successful states always carry `wake_class=none`; fresh, empty, expected, queued, or running checks remain `waiting_ci` and do not wake a repair actor.

Terminal-red handling is repository policy. `sync.terminal_red.action: block`
(default) preserves strict historical behavior. `park` deterministically labels
the exact caravan head `caravan-parked` after latest-per-check evidence proves a
current terminal Failure/Cancelled/TimedOut/ActionRequired verdict, disables its
auto-merge, and excludes the whole preserved caravan from active convergence,
capacity and admission-tail selection. It never changes member labels, branches
or bases. Pending/running and superseded red remain active. A new head or latest
nonterminal/green verdict removes the parked label; re-entry retains the root's
immutable original FIFO age. Park/unpark events bind exact ordering, member
heads, current/superseded check evidence, classification, fingerprint and
provider receipt. Hooks are optional recovery and never a green-queue liveness
dependency. Phase one parks a complete caravan when any member is terminal red.

Failed ticks carry a scheduler classification alongside their typed error. A provider/head/tail precondition race is `wake_class=retry_tick`: the next tick rediscovers and retries, and no `sync_failed` repair hook is emitted. Mechanical/semantic graph decisions, terminal current-generation CI, proven provider-generation invariant violations, and deterministic unsupported physical range shapes (`rebase_nonlinear_range`, ambiguous/empty ranges, rewritten target history, or invalid owned topology) are non-retryable `wake_class=external_decision`. They emit exactly one canonical `ci_failed` or `sync_failed` repair-wake event even when the same failed tick also completed ordinary topology events. The receipt preserves exact affected PR/caravan, merge OIDs/plans, zero/partial mutation evidence, actionable first-party choices, and a stable decision fingerprint for external deduplication. Local checkout, configuration, permission, or policy work that an external repair agent cannot safely decide is `wake_class=operator_action`. At an incompatible edge or unhandled CI failure, sync stops like an interactive rebase, checks out the affected PR when safe, emits a structured decision point, fires its hook, and exits. A dirty or internally-remoted caller must not trigger raw Git surgery: `repair start` resolves an explicit provider-owned URL, creates an independent workspace below Git's common Caravan state, seeds only content-addressed objects from the current canonical checkout, binds the explicit provider as a separate remote, minimally/bloblessly fetches and checks out exact provider OIDs, and leaves caller HEAD, refs, config, index, and files untouched.

For deterministic repair, conflicts and exact semantic grants remain narrow. For a typed semantic/CI decision, an audited session-level authorization may permit one exact agent to add, modify, rename, or delete ordinary repository content inside the isolated workspace. It never authorizes Git internals, out-of-workspace paths, symlinks/gitlinks, staged secret-like operational files, unstaged/untracked residue, unresolved markers, excess bounded scope/diff, identity drift, or a different actor. Continue fingerprints the complete bounded broad path list, staged objects (including deletions), and binary diff before commit; the publication receipt carries that evidence and requires fresh CI.

`repair continue` verifies the persistent manifest, merge target, baseline index, authorized scope, conflict markers, remote head, and exact merge parents before ordinary non-force publication. When a conflict-free mechanical merge is semantically incomplete, `repair grant` may add a bounded reviewed source patch: Cara computes a three-way result from current path/source-parent/source blobs, stages it itself, and binds the exact result OID. This never reclassifies mechanical compatibility and grants no authority beyond those exact paths; every granted path must equal its expected staged result. Grant revocation is local-only, authority-matched, and preflights the complete requested set before restoring exact pre-grant staged blobs. Grant and revoke reconcile persisted receipt/index pairs after interrupted manifest publication; successful revocation leaves bounded receipts so exact retries remain idempotent. It then rediscovers and resumes the stored `sync --all`; interruption after commit or push is idempotent and preserves the workspace until convergence.

## 7. CI and merging

Normal behavior:

- Root: promoted to the exact default branch, then squash-merged by Cara itself.
- Non-root: auto-merge disabled, even if it was enabled externally.
- Pending CI: sync reports waiting and makes no speculative repair.
- Failed CI without `caravan-force`: decision point.

`sync.head_merge_actor` names the single merge actor: `caravan` or `github`. It
is deliberately self-describing — `github` never means "do not merge the root",
it means the provider's `autoMergeRequest` performs the merge. Caravan-owned
merging is the intended end state but it is **opt-in**, and an absent field
resolves to `github`. Both compatibility directions matter: older Cara builds
reject unknown configuration keys, so a repository can only adopt the field once
every consumer has upgraded; and deploying a newer runtime against an existing
config must never silently change who merges that repository's pull requests.
The historical `sync.auto_merge_head` boolean is still accepted (`true` =
provider, `false` = caravan).

Both keys live under `sync:`. A misplaced top-level key is not an opt-in: strict
parsing rejects unknown fields at every level, so the whole policy fails closed
with `config_parse_failed` rather than being silently ignored, because "ignored"
reads exactly like "applied" to an operator.

Migration is therefore ordered, and it is pinned to **exact commits rather than
release versions**, because binaries carrying the same version string can
predate or postdate any of these changes:

1. `3dd14ba` — admission capacity priced by the actual-work reserve.
2. A runtime containing `57639f4` (caravan-owned landing contract) and
   `c6e1e8e` (CI devShell gate), but *before* `dabefbd`. Deploy it with the key
   absent and prove every reader of that repository's
   `.caravan/config.yaml` runs it. On this runtime an absent key resolves to
   `caravan`, so this step is itself the direct-actor cutover.
3. Add the correctly nested `sync.head_merge_actor: caravan`. It is honoured
   identically before and after the next step, so behaviour does not move.
4. Deploy `dabefbd`, the default-github change. The explicit key preserves the
   behaviour established in step 2; a repository that never opted in keeps the
   historical provider-native actor across the same upgrade.
5. Disable provider-native auto-merge on the repository.

The backward-compatible default is covered by
`config::tests::the_merge_actor_is_optional_backward_compatible_and_self_describing`
(absent resolves to `github`; explicit `caravan` opts in; the historical boolean
keeps `true` = provider, `false` = caravan; an explicit field outranks the
alias; a serialized default document emits neither key),
`config::tests::the_merge_actor_key_must_be_nested_under_sync_and_fails_closed_otherwise`,
and end to end by
`sync::tests::an_existing_config_on_a_new_runtime_keeps_the_native_merge_actor`,
which proves a fleet with no key still arms its root and that cara performs no
squash merge of its own.

The auto-merge invariant is gated on the same fact, so a repository that
deliberately disabled provider-native auto-merge never reports a permanently
unsatisfiable problem: under `caravan` *no* member may carry native auto-merge,
under `github` exactly the root must.

Provider-native auto-merge cannot be ordered against caravan-owned topology. A
root armed while its base is still an already-merged predecessor branch merges
instantly into that predecessor: live PR2210 squash-landed on `main`, PR2213
then merged into PR2210's already-merged generation branch, its content never
reached `main`, and PR2215 inherited both the cumulative content and a dangling
base. Under `caravan` the tick is therefore one ordered fenced transaction per
root:

1. re-read the exact current root generation from a fresh single-PR read;
2. retarget it to the exact default branch when its base is anything else,
   including an already-merged predecessor branch;
3. re-read and prove base/ref/head after the retarget, and re-validate the
   required contexts of the *new* merge identity, because a head proven green
   against a predecessor base is not proven green against the default branch;
4. prove the cumulative tree, then perform exactly one **non-admin** SQUASH
   merge fenced on the exact head. Administrator merge stays reserved for the
   audited `caravan-force` bypass; routine landings respect branch protection;
5. prove the merge commit is contained by the freshly fetched default branch
   before the landing counts, then promote the successor and repeat until the
   bounded per-tick merge allowance or the first bounded wait.

Retarget alone is sufficient *and* CI-preserving because members are physically
rebased onto their predecessor before CI runs: the exact head SHA already
carries the cumulative reviewed content, and retargeting preserves its
head-attached check runs. That safety is proven mechanically rather than
assumed. `git merge-tree` constructs the result of merging the root into the
exact default branch and the tick compares it to the root head's own tree. Equal
trees mean the squash lands byte-identical already-validated content. A
different tree means the default branch gained content this generation never
saw; the caravan revalidates (physical rebase plus fresh CI) instead of merging.
An absent proof, or a proof built against a superseded default-branch
generation, is never permission.

Tree identity proves *what* would land, not that the default branch still wants
it, and both are required. The exact default branch must also be the generation
this caravan's retained patch set predicts: either it is contained by the root
head — the ordinary case, since members are rebased onto it before CI — or it is
precisely the generation this tick's own landing produced moments earlier. The
dangerous instance is an operator reverting or discarding an already-landed
ancestor: successors still carry that ancestor's diff, a three-way merge
silently reapplies it, and the result is still exactly the successor's own tree,
so tree identity alone would authorize reintroducing content the operator
deliberately removed. That is refused with
`default_branch_diverged_from_retained_patch_set` rather than waited on, because
no rerun can decide whether reverted content should return; only proving or
rescoping the caravan's content can. A rebase would replay the same commits, so
detection and refusal — never silent reintroduction — is the contract.

There is exactly one merge actor. A foreign `autoMergeRequest` observed on any
member is either converged away (`sync.external_auto_merge_policy: disable`,
default) or refused with `foreign_auto_merge_actor` (`refuse`); it is never
raced.

Every promoted root carries a sealed `root_promotion` receipt (exact head,
base before/after, default branch, predecessor and its merged state, derived
trigger, bounded reads, engine provenance) and emits `root_promoted` when the
engine retargeted. Every landing carries a sealed `root_merge` receipt with the
exact merged head/base, merge method, provider merge commit, default-branch
generation before and after, the authorizing cumulative-tree proof, and the
cumulative ancestry: the merged predecessor whose content the root already
carried, the remaining members, and the successor's base before and after
promotion. It emits `root_merged`. Together they prove already-merged
predecessor content is not duplicated and child content is not lost.

Cara is scheduler-neutral: `sync` and `loop --once` are bounded ticks with no
dependence on any hosted runner or service runtime, so a Caco-managed cron, a
hosted workflow, or a manual invocation are the same call. Because such a
scheduler dispatches repair actors from hooks rather than reading prose, the
caravan-owned merge actor classifies its own refusals by typed cause rather than
by error code: one code covers both bounded provider races and states no rerun
can resolve. A resumable cause is `wake_class=retry_tick` and emits no repair
wake; a non-resumable one is `wake_class=external_decision` and emits exactly
one canonical `sync_failed` carrying the exact repository, caravan, affected
PRs, typed cause, and a stable decision fingerprint for external deduplication.
A repository that cannot squash merge at all is `operator_action`, because no
rerun changes repository settings.

Promotion failure is the typed `root_promotion_incomplete` with cause
`base_retarget_not_observed`, `root_head_moved_during_promotion`, or
`stale_provider_view`, always before any merge is attempted. Merge refusal is
the typed `root_merge_refused` with cause `base_not_default_branch`,
`root_head_moved_before_merge`, `foreign_auto_merge_actor`,
`provider_did_not_persist_merge`, `merged_into_unexpected_base`,
`merge_not_reachable_from_default`, or
`default_branch_diverged_from_retained_patch_set`. Ordinary bounded waits — pending checks,
unsatisfied required contexts, an unproven or changed cumulative tree, an
already-merged root, or the spent per-tick merge allowance — are visible
no-op steps, never failures.

Creation flattens on the same terms as synchronization (bd-abd929): `cara new`
and `cara renew` authorize root flattening whenever the candidate has no join
target and `sync.head_merge_actor` is `caravan`. Wiring it into sync alone meant
a branch that merged the default branch into itself was admitted by a later
tick but refused at the moment the caravan was created, which is where an
operator meets it first.

A merge-preserving root that Cara will squash-merge is flattened rather than
replayed (bd-85b71d): its history is discarded at landing, so the tick commits
the already-proven merge tree directly onto the exact target instead of
re-resolving conflicts its author resolved by hand. Children are never
flattened, because their ancestry must physically follow the chain, and an
unauthorized merge-preserving replay still fails closed with
`rebase_merge_replay_conflict`. Root flattening is work only while the exact
candidate range still contains a merge. Once a prior tick produced a linear
head with the exact unchanged target as an ancestor, the next plan is
`already_satisfied` and retains that head OID; the persistent flatten-policy flag
must never replay identical trees under new timestamps and invalidate its own
exact-generation CI. Explicit squash reconciliation or descendant unwind remain
separate rewrite authorities.

Under the historical `github` actor the earlier contract still applies. Required
root squash auto-merge is scheduler-owned convergent state: the provider drops
`autoMergeRequest` whenever the root's head or base generation is rewritten, and
its list projection can still expose the pre-rewrite request, so every sync that
creates, rebases, renews, or advances a caravan root re-reads the exact current
root and idempotently proves SQUASH auto-merge on the *resulting* head. Root
convergence runs before a failing-CI stop, because native auto-merge merges only
a passing head. Every converged root carries a sealed `root_auto_merge` receipt
with derived triggers `idempotent_replay`, `root_admitted`,
`root_head_rewritten`, `root_base_advanced`, `externally_disarmed`, or
`non_squash_method`, emits `root_auto_merge_armed`, and unproven arming fails
with `root_auto_merge_not_durable` (`provider_did_not_persist_arming`,
`root_head_moved_during_arming`, `stale_provider_view`). Native auto-merge on an
*unadmitted* candidate keeps that candidate structurally ineligible under either
actor.

A raced state, draft, base, `caravan`, or `caravan-evicted` transition observed
by that fresh read is still an ordinary resumable `stale_precondition` decision
naming the exact changed field. Unrelated label churn — priority, force, or
review metadata — never blocks required root convergence, because it cannot make
arming wrong and treating it as a decision would reintroduce operator
babysitting.

A head with *no* required-run coverage is a separate failure class from pending
CI, and it is the one a rollup-only scheduler cannot see. A rebase-on-join can
publish a head GitHub never starts a workflow run for; the required contexts
then have zero reporting runs, the PR sits `MERGEABLE`/`BLOCKED` with nothing
pending and nothing failed, and waiting is futile. Every sync therefore proves,
per member and on the exact current head, that each protection-declared required
context has at least one run or check-suite lineage:

- required contexts come from protection on the *exact base branch*, so a member
  stacked on an unprotected branch requires nothing;
- an `EXPECTED` rollup placeholder is not reporting evidence, because that is
  precisely the state which used to look pending forever;
- lineage is matched on the exact head OID, so a run from a superseded
  generation is retained as `stale_head_runs` evidence and never as coverage;
- the expensive lineage read happens only when a required context is absent from
  the rollup, keeping the healthy path at one protection read per base branch.

The resulting `required_runs` receipt is sealed and carries a typed status:
`not_required`, `satisfied`, `pending`, `failing`, `cancelled_superseded`,
`awaiting_grace`, `missing_required_runs`, or `unknown_provider_state`. Nothing
is declared missing inside the bounded `sync.missing_required_runs_grace_secs`
window, measured from the latest provider timestamp that could have triggered CI
for that head, and nothing is ever declared missing from a partial provider read
or an unparsable head timestamp — those stay `unknown_provider_state`.

Recovery is exactly one auditable check-suite rerequest against the *unchanged*
head, followed by exactly one rediscovery. The suite is re-read first and
refused unless it belongs to the exact current head. Empty commits, close/reopen
loops, force pushes, retargets, and broad reruns are never implicit workarounds:
head, base, branch, and membership are preserved. When no rerequestable suite
exists, when the provider refuses, or when the rerequest does not produce
reporting lineage, the tick emits a typed problem —
`missing_required_runs` or `cancelled_superseded_required_runs` — naming the
exact PR, head, and contexts, and the reviewed manual recovery. Engine requests
emit `required_runs_retriggered`; visible stalls emit `required_runs_missing`
once per distinct problem fingerprint (kind, PR, head OID, contexts), and both
problem lists and hook evidence stay bounded.

A stalled member never fails the tick and never contaminates another member: the
scheduler status degrades instead. `missing_required_runs` in
`scheduler_status` makes the disposition `operator_action` with wake class
`operator_action`; `unknown_provider_state` makes it a bounded `retry_tick`;
`awaiting_grace` members join `waiting_prs` as ordinary CI waits. The scheduler
is never `healthy` while a caravan cannot start CI at all.

A failed-CI decision contains bounded structured run, job, and failed-step
facts. For an allowlisted lineage-verification step, Cara requests only the
first 60 KiB of the job log and retains only a strict
`ci-selected-ref-receipt`; all unrelated text, credentials, and raw log bytes
are discarded. Missing, malformed, unavailable, or range-truncated receipts
fail closed. Exact selected commit/parents and current synthetic-candidate
identity classify the run as stale generation, retryable infrastructure,
source/test failure, cancelled, or unknown. Only current-generation
infrastructure failures are rerunnable; stale or unproved lineage requires a
fresh exact-candidate trigger.

A user/agent may use the managed repair workspace, rerun failed checks,
evict/split, or arm a known acceptable failure with audited `cara force --pr N
--actor A --reason R`. Raw label edits, nested worktrees, manual `update-ref`, and
force publication are not valid Cara decision continuations.

`caravan-force` is durable PR-scoped operator intent to bypass every CI state, including successful, expected, queued, running, failed, unknown, mixed, or empty checks. When that PR becomes head, `cara sync` force-squashes it immediately only when:

1. `.caravan/config.yaml` sets `force_merge: true`;
2. the open head has `caravan-force`;
3. it remains mechanically conflict-free with the default branch;
4. the authenticated actor has repository ADMIN permission.

A force-labelled head armed through the ordinary `cara force` surface may keep
provider-native auto-merge disabled while Cara validates this policy. Sync posts
the durable PR-intent acceptance audit and invokes the administrator squash
primitive directly; that ordinary surface never arms native auto-merge as a
force prerequisite, because repositories without a holding requirement could
otherwise merge before Cara records its authorization. The separately reviewed
`force-intent apply` transaction likewise arms durable intent while ensuring
native auto-merge is disabled. A targeted sync may defer this intentional
disabled-auto-merge invariant on an unrelated force-labelled head when
`force_merge: true`; structural selected-Caravan errors, non-squash or
externally enabled auto-merge, ordinary unlabelled head gaps, and selected-head
drift remain blocking through final rediscovery.

Force intent is bound to PR identity, not one head OID. `cara force` and `cara
force revoke` rediscover exact provider facts, accept any active owned Caravan
member, require a mechanically clean current Caravan edge, selected-Caravan
hold/graph safety, and ADMIN permission, then change only `caravan-force` under
exact transition preconditions and post a deterministic actor/reason audit.
Problems on unrelated admission candidates or other caravans do not block the
transition. Both operations are idempotent; revoke never touches unrelated
labels. Cara-owned physical rewrites, joins, rebases, and position/base changes
preserve the label naturally on the same PR and perform no invalidate/restore
control mutations. Explicit revoke, eviction, or successful merge consumes
intent.

The controller-reviewed exception contract is a separate exact machine surface:
`cara --json force-intent preview|apply|revoke --pr N --head OID
--membership-generation G --failure-fingerprint F --reason R
--expires-at-ms T --auto-merge squash`, with matching MCP tools
`force_intent_preview`, `force_intent_apply`, and `force_intent_revoke`. Every
invocation performs fresh provider discovery. Membership generation hashes the
ordered Caravan membership, exact member head/base facts, and current default
branch. CI failures expose the same deterministic `fnv1a64` fingerprint in the
sync decision, exact check evidence, and current decision evidence.

Preview is always zero-write and returns the current provider head, membership,
checks, decision, fingerprint, and expiry. Apply requires unexpired authority,
`force_merge: true`, exact supplied head/generation/fingerprint, a current CI
failure, active owned membership, clean current-edge compatibility, no selected-Caravan
hold/graph problem, fresh default/head/check preconditions, and ADMIN permission. It
converges durable `caravan-force` while disabling native auto-merge in one
GraphQL provider mutation and independently refetches the complete postcondition; a
partially applied aliased response is a typed resumable error with before/after
evidence. The selected member's disabled-auto-merge invariant is repairable;
problems intersecting the selected Caravan remain blocking while unrelated
admission/Caravan problems do not. Revoke accepts expired
authority, removes only durable PR force intent, preserves queue-owned squash
auto-merge, and is idempotent. Apply and revoke post a deterministic
GitHub-visible audit bound to the complete reviewed authority; audit failure
retains provider receipts for exact retry. This reviewed surface does not consume
intent or merge: normal sync remains the sole queue actor.

Before consuming armed `caravan-force`, sync/loop skips CI and required-run
provider reads, then posts its separate durable acceptance audit containing the
currently visible checks for context, enabled force policy, authenticated ADMIN
permission, exact clean root/default compatibility proof, and squash action.
Comment failure is resumable and prevents the force merge. The attempt and
result are emitted as audit events. Force never bypasses textual conflicts,
stale provider facts, holds, ownership, permissions, or leases.

## 8. Decision points and errors

Every failure is typed and machine-readable through CLI JSON and MCP. Child
stdout and stderr have independent hard capture bounds. Exceeding either returns
`command_output_limit` before provider JSON decoding, with exit status, exact
total/limit bytes, and separate bounded prefix/suffix evidence for both streams;
truncated JSON is never mislabeled as malformed provider output. A repair
decision includes, when relevant:

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
- `repair_stale_head` / `repair_scope_changed` / `repair_conflicts_unresolved`;
- `cross_caravan_conflict`;
- `ci_failure`;
- `invalid_graph`;
- `stale_precondition`;
- `unsafe_checkout`;
- `hook_failure`;
- `force_merge_denied`;
- `root_promotion_incomplete`;
- `root_merge_refused`;
- `root_auto_merge_not_durable` (historical `head_merge_actor: github`);
- `squash_merge_not_enabled`;
- `missing_required_runs` / `cancelled_superseded_required_runs` /
  `unknown_required_runs_provider_state`.

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
- `caravan_parked`;
- `caravan_unparked`;
- `ci_failed`;
- `force_merge_attempted`;
- `force_merge_completed`;
- `root_promoted`;
- `root_merged`;
- `root_auto_merge_armed` (historical `head_merge_actor: github`);
- `required_runs_missing`;
- `required_runs_retriggered`.

`examples/hooks/caco-bead-dispatch.sh` is the worked scheduler-side consumer:
it routes on `wake_class`, staying silent for `none` and `retry_tick` because
the next tick resolves those itself, filing one bead and dispatching one agent
for `external_decision`, and filing the bead then notifying a human for
`operator_action` because no agent can change repository settings. It
deduplicates on `decision_fingerprint` rather than the per-emission event id, so
a caravan stuck for an hour dispatches one agent instead of one per cron tick.
Its behaviour is pinned by `tests/hook_example.rs` against a fake `caco`, not by
this prose. That target is an environmental acceptance, not an install gate: it
is guarded by Cargo feature `environmental-hook-acceptance` and MUST NOT run in
ordinary Cargo/Nix package checks, installations, pull-request CI, or release
contract builds. The scheduled/manual `Environmental hook acceptance` workflow
MUST run the complete target with its assertions unchanged and MUST NOT have a
`push` or `pull_request` trigger.

The environmental lane MUST always execute the assertions. Success emits no
feedback. Before reporting a failure, the lane MUST preflight an enabled webhook
strategy and MUST stay visibly failed rather than silently fall back to stderr.
A reportable failure emits exactly one bounded event per run with
the two exact assertion names, architecture, environment, source revision,
failure phase, exit status, output digest, and credential-redacted output tail.
The event carries a stable fingerprint over acceptance version, architecture,
environment, and phase; identical reruns therefore present the same receiver
idempotency identity. Receiver-side fingerprint-to-canonical-bead collapse is a
shared Cacophony ingress contract (`bd-52cf42`), not permission for Caravan to
weaken assertions or add a second local dedupe store.

A hook is a configured shell command. It receives one versioned metadata JSON object on stdin and non-secret context such as `CARA_EVENT`, repository, and PR numbers in environment variables. Hook metadata contains operation/event IDs suitable for external deduplication.
Before hook delivery, every canonical secret-free event is durably appended with
its exact IDs to a versioned journal under common Git metadata. Secret-free hook
delivery status is appended afterward. Locked append/read, bounded rotation, and
torn-final-record recovery make this an audit surface only; it is never queue
state or cursor authority. Journal I/O errors report that completed provider
mutations were not rolled back.

Hooks may coordinate arbitrarily complex external workflows. They remain noninteractive commands with JSON on stdin; manual TTY decision handling is a separate loop mode. Caravan does not wait for an agent protocol or hold a distributed lock. A decision-point sync always stops after firing its hook. Routine `retry_tick`, `waiting_ci`, and `held` outcomes do not fire the repair-wake hook; only `external_decision` does. A coordinator that outlives the hook process must own an external lock/dedupe record; repeated external-decision ticks may invoke the hook again, and the hook must no-op while that coordination is active.

Each hook has a timeout and `blocking` policy. Best-effort hook failure is reported but does not roll back a completed GitHub mutation. Blocking hook failure returns `hook_failure`; it still cannot roll back already-completed remote mutations.

Minimal config shape:

```yaml
version: 1
# Optional. Name the exact repository when the checkout cannot: `gh repo view`
# infers identity from git remotes only, takes the repository positionally, and
# ignores `GH_REPO`, so a managed checkout pointing at a local daemon mirror has
# no other input.
repository: owner/name
force_merge: false
# Optional backend axis. `caravan` is the stable default.
stack_type: caravan
github_auth:
  mode: ambient
# Writer authority. `local_only` (default) uses the machine-local operation lock.
# `read_only` permits config/status/check/log/plan surfaces and refuses every
# mutation. `remote_fenced` additionally requires an external compare-and-swap
# lease broker and fences every provider/Git write behind a monotonic token.
writer:
  mode: local_only
rebase_on_join: false
agent_priority_labels:
  - caravan-priority:high
  - caravan-priority:normal
  - caravan-priority:low
command_timeout_secs: 30
repair:
  materialization_timeout_secs: 180
loop:
  interval_secs: 60
hooks:
  sync_failed:
    command: ./scripts/on-caravan-sync-failed
    timeout_secs: 30
    blocking: false
```

Unknown config fields are errors. Secrets belong in environment variables, not committed YAML or hook metadata.

`stack_type` is optional and defaults to `caravan`, which preserves the existing
label/base-chain/merge behavior and performs no GitHub Stack capability probe.
`github` is an explicit native-Stack backend. It requires
`rebase_on_join: false`, `sync.head_merge_actor: caravan`, and a reviewed
`stack_rollout.mutations_opt_in`; status makes one bounded REST Stack inventory
read, distinguishes available, unavailable, and unknown capability, and
verifies provider member order, base refs, branch names, and exact head OIDs
against Cara's graph. A full 100-Stack page is reported as truncated rather than
complete. Missing opt-in, unavailable capability, truncated inventory,
ambiguous mapping, generation drift, holds, compatibility failures, incomplete
CI, and unsupported force intent all fail closed before provider mutation.

Exact REST create/add/unstack, asynchronous direct-merge planning, submission,
UUID-polling, receipt primitives, and the phased evict/split unstack/rebuild
transaction remain policy-free adapters composed by membership, reshape, and
sync. GitHub exposes no arbitrary Stack remove or reorder, so reshape is a
sealed `preflighted` → `unstacked` → `reshape_applied` → `rebuilding` →
`rebuilt` → `verified` sequence that persists `provider_atomic: false`, binds
the exact per-PR base/head/control-label/auto-merge postcondition the existing
Cara reshape must establish, creates replacement Stacks one at a time with exact
zero-write retries, and proves singleton chains by exact inventory absence.

`max_caravan_length` bounds one caravan as a merge batch. It accepts only
GitHub's 2..=100 Stack range, is absent by default so existing repositories keep
the dynamic mutation-budget capacity model unchanged, and defaults to 8 only
under explicit `stack_type: github`. Reaching the bound is ordinary fullness,
never a capacity defect: admission refuses growth with
`caravan_batch_capacity_exhausted`, deterministically selects another compatible
caravan, and opens a new one when every visible caravan is full, so a bounded
batch never stalls the queue. Sync never waits for occupancy; it lands the
maximal contiguous ready prefix, and a fully ready batch lands as one atomic
native merge. Status reports the effective bound and exact per-caravan
full-batch evidence.

Native rollout is additionally gated by an explicit per-repository allowlist.
`stack_rollout.mutations_opt_in` requires `stack_type: github` and a non-empty
`reviewed_by`. Without it status reports
`github_stack_repository_not_opted_in`; an unavailable or unproven capability
outranks the opt-in with `github_stack_capability_unavailable` or
`github_stack_capability_unknown`, so absence is never inferred. Opting into
`stack_type: github` also requires `min_cara_version` at or above the first
native-Stack reader release, so an older Cara can never read a provider Stack as
an ordinary caravan.
The 2026-07-31 disposable sandbox
proved successful full and partial atomic squash, all-or-none failure after an
ordinary lower fast-forward, top-SHA rejection, and UUID recovery. It also
proved the current API does not lease the complete group: after 202, a lower
rewind preserving upper ancestry merged every selected PR at the changed lower
generation. Cara seals that outcome as `indeterminate`, but post-merge detection
cannot prevent it. The installed `gh stack` CLI does not contain a merge command
or hidden stronger lease; web merge uses the same endpoint.

The 2026-08-01 follow-up proved the accepted preventive equivalent. Cara creates
one active repository ruleset with no bypass actors, exact selected source refs,
and exactly `update` plus `deletion` restrictions. Exact readback must report
`current_user_can_bypass: never`; owner SSH pushes and owner-authenticated REST
force-update/delete were all rejected while direct prefix merge and unselected
suffix rebase succeeded. The ruleset generation is checkpointed with the async
UUID, revalidated before each submit/poll, and released only by exact ID and
generation after terminal proof. Missing/drifted lock is `indeterminate`.
Repository ruleset mutation requires an explicit Administration(write) upgrade,
which remains outside the baseline App policy and default Caravan mode. Native
landing is executable only through the ruleset-locked orchestrator; unlocked
top-SHA-only merge remains invalid. An older Cara reader
must still be excluded with `min_cara_version` before repository opt-in. Exact
evidence is recorded in
`docs/validation/github-native-stack-sandbox-2026-07-31.md`.

`rebase_on_join` is a strict, explicit history-rewrite opt-in and defaults to
`false`, preserving the virtual compatibility contract above. Status and check
always expose the effective mode and config path; a disabled sync conflict gives
the exact `rebase_on_join: true` project-config action instead of implying a
manual hand-rebase.

When enabled, membership rebases one bounded candidate-only range. Before any
PR creation/update, branch rewrite, or membership mutation, a selected join
root must target the exact current default OID; otherwise `join_root_stale_default`
returns zero-write sync guidance. Join source provenance binds repository,
branch, head, one source/default merge-base parent, source tree, binary-patch
fingerprint/title, stable per-commit patch classification, exact selected tail,
and independent expected result tree. `git cherry` identity and an effective
merge-tree/diff against exact current main remove patches already represented
there under different commit OIDs; mixed ranges replay only unique commits.
Changes already on current main but absent from an older source parent are
subtracted rather than replayed into the child. An empty effective patch is `join_empty_source_noop`, with a complete receipt and
zero provider mutation. Linear ranges use the ordinary sequencer. Owned nonlinear ranges use an explicit
two-parent merge-preserving strategy: commits reachable only from the candidate
(after excluding retained old-base and current-target ancestry) are replayed
with no cousin rebasing, and every old/new parent edge is mapped in the receipt.
When a child merge commit names its exact old provider base as one parent and
that same parent branch has a retained simulated replacement earlier in the
same globally verified batch, Cara maps only that old parent generation to the
planned parent and seals the replacement in the topology receipt. This is the
ordinary cumulative merge shape, not cousin history. Octopus/root parents and
every unrelated cousin/external parent remain rejected. The rebuilt head tree must
exactly equal an independently computed clean `merge-tree` for current target +
old candidate head before any write. Post-rewrite
provider rediscovery is operation-specific: `join`/`rejoin` require the exact
live tail named by the rebase receipt; `new`/`renew` require the exact current
default branch generation and no inferred membership tail. A new caravan has no
join target by design. Candidate-head/default/tail or unexpected-membership drift
returns `join_target_moved_after_rebase` or `new_target_moved_after_rebase` with
`mutated_membership=false` before label/base/auto-merge writes.
`sync --all` plans each selected caravan head-to-tail from exact discovered
facts: the head targets the exact default OID and every descendant targets the
retained, simulated new head of its parent. Rebase objects are materialized once
and retained through apply; they are never recomputed. Every edge conflict,
workflow trigger, PR precondition, remote old head, branch-set disjointness,
dry-run permission, and exact lease is verified globally before provider or
branch writes. Planning and this no-write barrier share a precommit deadline
which is the one operation deadline minus a conservative apply reserve. The
reserve is derived from the operations the tick will actually run, not from a
whole-chain worst case: a member whose exact cumulative ancestry already holds
costs no push or auto-merge drop, and durable force labels add no rewrite
control mutation, so a completed prefix makes every later tick strictly cheaper. It splits into a hard part
(control mutations, bounded parallel branch-apply rounds, mandatory midpoint
verification) and a deferrable part (base/CI reconciliation and final
discovery), each planned command priced at `sync.reserve_secs_per_command`
(itself capped by `command_timeout_secs`, which remains the hard ceiling for one
child). Admission and the reserve share that one price: raising a proven-safe
`command_timeout_secs` never changes the admissible chain size.

When the complete reserve cannot remain but the hard reserve can, the tick
applies an exact bounded prefix instead of refusing forever. The complete graph
is still planned and globally verified; only the largest root-to-descendant
prefix that provably fits is applied strictly parent-to-descendant; durable
force intent remains on every admitted or deferred PR without control mutation.
Completed receipts are checkpointed before return and the tick succeeds with a
`retry_tick` scheduler disposition, deferred member list, admitted prefix,
required/complete reserve, and configured deadline. Ordinary convergence,
root arming, and automatic admission wait for the resumed tick; the resume is
idempotent and never replays a completed provider mutation. Independent
caravans grow their prefixes round-robin under the same bounded parallelism and
no caravan is reordered, evicted, or split to make a reserve fit.

The configured deadline and per-command reserve imply a maximum admissible
chain size: the largest chain whose trailing member is still guaranteed to
drain in one tick, priced by the same actual-work model that produces the
required reserve so the two can never disagree about the same chain. `status`
exposes that bound, the required and retained reserves, the
processable prefix, the deferred members, the blocked candidate, the configured
deadline and a safe next action before any sync refusal, and `plan sync`
reports the same prefix/deferral it would admit. Admission fails closed at that
bound: an explicit `join` returns `caravan_budget_capacity_exhausted` and
automatic admission stops with the same typed evidence, while the already
admitted prefix keeps draining. A bound below two members is never emitted and
is never enforced as gating: a caravan holding a single member is never
reported at capacity, and an arithmetic result below that floor is a typed
configuration defect (`sync_budget_capacity_unsound`, surfaced to a refused
join as `caravan_budget_capacity_defect`) carrying the computed bound, the
sound floor, the per-command reserve and the deadline that repairs it. Defect
guidance never recommends draining, which cannot change a bound derived from
configuration alone. If not even one pending member fits, both
`sync` and `plan sync` return `physical_sync_budget_insufficient` with
required/remaining milliseconds, configured deadline, maximum admissible chain
size, processable prefix, complete-or-partial plan count/hash, zero
provider/branch mutations, and configuration guidance; the absolute deadline
is never extended and unchanged exhaustion is not a retry tick. Auto-merge is
disabled only after that barrier, and a durable lock checkpoint records the
confirmed control receipts before any branch write. Apply revalidates remote
source/range/current-default generations, selected tail, root provider
precondition, and result tree, but uses the retained object's exact
force-with-lease as the source-head writer-race gate instead of repeating a slow
permission dry-run after irreversible control mutation. Independent caravans
may apply with bounded parallelism; each chain is strictly parent-to-descendant.
A child provider `BaseRefOid` which still names an ancestor of its
already-advanced parent branch is retained as an explicit historical range
boundary; a non-descendant or changed exact head remains a true race. A
mandatory midpoint rediscovery verifies every new head and refreshes invalidated
CI before ordinary sync policy runs.

A moved branch, unsupported merge topology, ambiguous range, conflict, tree or
topology mismatch, or apply-time lease race is a typed resumable decision and
is never forced. A replay commit-count mismatch reports source/rebuilt/dropped
counts and exact OIDs, explains duplicate/empty-patch pruning as a likely cause,
and gives a reviewed source-rebase/repair next action. Global preflight failure has
zero writes. Apply-time failure preserves the exact successfully rebuilt prefix
and skips its descendants; independent in-flight chains may complete. Recovery
never force-rolls back: rediscover GitHub and rerun the same idempotent sync.
Outputs and errors retain old/new head/base/tree, workflow proof, exact lease,
source patch provenance, provider receipts, and completed physical-rebase
receipts. Join refusal/no-op paths emit durable journal evidence containing the
bounded structured details, so zero-write decisions remain auditable.

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

One local process at a time may mutate a repository, enforced by an operation lock under Git metadata. Read-only commands may run concurrently. The owner file remains below 16 KiB even for large fleets: sync checkpoints store schema-versioned counts, complete deterministic hashes, and bounded first/last samples of affected PRs, steps, plans, receipts, and events rather than embedding unbounded histories. The latest provider preconditions remain in the tail sample; GitHub rediscovery is still recovery authority and hashes bind omitted evidence.

No local lock can serialize distributed machines. Every mutation therefore carries optimistic preconditions over PR number, head SHA, base ref/SHA, labels, state, and auto-merge state. CI/check progress is observation, not topology/base/label/comment/auto-merge mutation identity: queued/running/completed churn alone cannot stale an exact sync or join. CI diagnostics and rerun operations still bind exact checks, run IDs and heads. A real mismatch aborts with `stale_precondition`; the caller rediscover/reruns rather than overwriting concurrent work. Human CLI errors show bounded colored changed-field/count/OID/next-action summaries and direct operators to `--json` when full evidence exceeds 4 KiB; JSON/MCP always retain complete details.

Lightweight provider/Git children remain bounded by `command_timeout_secs`. Repair cache-seed/fetch/checkout materialization is separately bounded by `repair.materialization_timeout_secs` because authenticated object transfer is not a lightweight probe. The manifest is checkpointed before each external phase and retains exact phase, budget, process-group/error evidence, object-cache Git identity, partial path, and safe resume/abort guidance after timeout or transport disconnect. A valid partial repository is resumed in place and reuses verified objects; invalid partial state is removed only after exact manifest/path validation. Provider head and target are always re-read, so the local cache is never publication authority.

Multi-step remote mutations are not atomic. Errors report completed steps. The graph invariants and idempotent sync are the recovery mechanism; commands never hide partial remote progress.

## 11. MCP contract

The CLI and MCP tools share typed inputs, outputs, and domain errors. MCP exposes bounded single operations (`status`, `log`, `check`, `new`, `join`, `plan_sync`, `sync`, `evict`, and peers), not the unbounded `loop` or `log --follow` processes. An agent implements a long-lived loop by scheduling repeated `sync --all` calls or by running `cara loop` externally.

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
- Nix build/dev shell, CI, and baseline tests, including pinned `actionlint`/`shellcheck` workflow validation through `scripts/check-workflows.sh`.

Subsequent beads implement GitHub discovery, graph validation, compatibility, mutations, sync/CI, hooks/loop, and recovery behavior without changing this contract silently.

`sync.checkout_on_decision` (default `false`) leaves the working tree exactly
where it was. Setting it `true` restores the historical affordance of checking
out a decision's PR so it can be repaired in place. `CARA_CHECKOUT_ON_DECISION`
overrides the configured value for one invocation, so a scheduled hook or an
interactive shell can differ from repository policy without editing a shared
file. Only unambiguous values are honoured (`1/true/yes/on`, `0/false/no/off`);
anything else leaves configuration in force rather than guessing. That is right for an
interactive checkout and wrong for an unattended sync worktree, which otherwise
silently becomes whatever PR was last inspected — one was found parked on a dead
agent's branch 95 commits behind the default, so every policy value came from
that old commit. The default records a `skipped` checkout receipt naming the PR, so the
between-runs state is always explained rather than implied and a well-known
worktree never silently becomes the last PR inspected. `config_provenance.behind_default_branch`
reports the distance on every read, and a tick refuses with
`stale_repository_policy` when the effective config both differs from the
default branch and comes from a checkout that is behind it.

Configuration validation is two-tier. Structure, version, `min_cara_version`,
label syntax, command timeouts, journal bounds, and hook policy are always
enforced at load. Per-tick budgets — `sync.max_candidates_per_tick`,
`sync.max_mutations_per_tick`, `sync.max_github_requests_per_tick`,
`sync.max_duration_secs`, and `loop.interval_secs` — are enforced only by a
mutating tick, which refuses with `invalid_tick_bounds` (`operator_action`). A
read is never blocked by a bound it does not consume: `cara status`, `cara
check`, `cara log`, and `cara sync --dry-run` stay available while such a bound
is invalid, because those are the surfaces needed to diagnose it.

The journal is append-only observability, never a decision input, so a record
this binary cannot parse is skipped and counted rather than aborting the
operation. `log.source.unreadable_records` reports the count and human `cara log`
names it, because a non-zero value usually means a newer Cara wrote record types
this binary does not know. Unknown `GraphProblemKind` values likewise deserialize
to `unknown`, which is fleet-blocking, so forward tolerance never downgrades a
problem a newer Cara considered serious.
