//! Shared typed command contracts for the `cara` CLI and MCP server.
//!
//! Every bounded v1 domain tool is backed by the same GitHub-facing operation
//! used by the human and JSON CLI surfaces.

use std::path::{Path, PathBuf};
use std::time::Duration;

pub mod admission;
pub mod ci;
pub mod ci_gate;
pub mod command;
pub mod compatibility;
pub mod force;
pub mod force_intent;
pub mod generation;
pub mod github;
pub mod graph;
pub mod hooks;
pub mod initialization;
pub mod journal;
pub mod loop_runner;
pub mod membership;
pub mod navigation;
pub mod next;
pub mod operation_lock;
pub mod pause;
pub mod physical_rebase;
pub mod priority;
pub mod read;
pub mod repair;
pub mod required_runs;
pub mod reshape;
pub mod root_auto_merge;
pub mod root_merge;
pub mod squash_equivalence;
pub mod sync;
pub mod web;

use clap::Args;
use feedback_cli::{FeedbackConfig, FeedbackError, ReportStrategy, Reporter};
use mcp_cli::{ErrorCategory, StructuredError, ToolRouter};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::command::CommandRunner;
use crate::config::{CaravanConfig, ConfigError};

pub mod config;
pub mod config_provenance;
pub mod model;

/// GitHub release repository used by `updatable-cli`.
pub const UPDATE_REPO_SLUG: &str = "harryaskham/caravan";
/// Installed binary name.
pub const TOOL_NAME: &str = "cara";
/// Help contract source marker retained for stable JSON/MCP compatibility.
/// The complete contract is embedded in [`AGENT_HELP`]; no external file is required.
pub const SPEC_PATH: &str = "embedded";

/// Self-contained agent/operator instructions returned by `cara help` and MCP.
pub const AGENT_HELP: &str = r"Caravan is an agent-in-the-loop GitHub merge queue. GitHub is the durable
source of truth: every operation rediscovers exact PR, ref, check, label, and
auto-merge facts; local files are locks, bounded receipts, or disposable
workspaces, never queue authority.

CORE MODEL AND INVARIANTS
- A caravan is a linear ordered PR chain. Its first member is the head and its
  PR number is the caravan ID.
- Every active member has the `caravan` label and is open, non-draft, and owned
  by the base repository. Fork-only heads are rejected.
- The root targets the default branch. Each child targets its predecessor's
  branch. `sync.head_merge_actor` names the single merge actor: `caravan` or
  `github`. Under `caravan` no member may carry a provider `autoMergeRequest`;
  under `github` exactly the root must. The auto-merge invariant is gated on
  that same fact, so a repository that disabled native auto-merge never reports
  an unsatisfiable problem. Caravan-owned merging is opt-in: an absent field
  resolves to `github`, because a runtime upgrade must never silently change who
  merges a repository's pull requests. Older Cara builds also reject unknown
  configuration keys, so the field is optional in both directions, and the
  historical `sync.auto_merge_head` boolean is still accepted.
- Under `caravan`, a sync tick is the merge actor and treats each root as one
  ordered fenced transaction: re-read the exact generation; retarget to the
  exact default branch when its base is anything else, including an
  already-merged predecessor branch; re-read and prove base/ref/head plus the
  required contexts of the new merge identity; prove that merging into the
  default branch yields exactly the head's already-validated tree *and* that the
  default branch is the generation this caravan's retained patch set predicts,
  so an operator-reverted ancestor is refused rather than silently reintroduced;
  perform one non-admin SQUASH merge fenced on the exact head; prove the merge commit is
  contained by the freshly fetched default branch; then promote the successor
  and repeat within a bounded per-tick allowance. Administrator merge stays
  reserved for the audited `caravan-force` bypass. Sealed `root_promotion` and
  `root_merge` receipts carry the proven base transition, the authorizing
  cumulative-tree proof, the provider merge commit, the default-branch
  generation before and after, and the cumulative ancestry; they emit
  `root_promoted` and `root_merged`. Unproven promotion is
  `root_promotion_incomplete`, a refused merge is `root_merge_refused`, and
  ordinary waits (pending checks, unsatisfied required contexts, an unproven or
  changed cumulative tree, a spent merge allowance) are visible no-op steps.
  A foreign auto-merge request is converged away or refused, never raced.
- Under the historical `github` actor, required root squash auto-merge remains
  scheduler-owned convergent state: every sync that creates, rebases, renews, or
  advances a caravan root re-reads the exact current root from a fresh single-PR
  provider read and idempotently proves SQUASH auto-merge on the resulting head,
  never on a pre-rebase generation or a stale list projection. Each converged
  root carries a sealed `root_auto_merge` receipt and emits
  `root_auto_merge_armed`; unproven arming is the typed retryable
  `root_auto_merge_not_durable`.
- Required CI coverage is proven, not assumed. GitHub sometimes never starts a
  run for a freshly rebased head, so every sync checks that each context
  required by protection on the exact base branch has at least one run or
  check-suite lineage on the exact current head. An `EXPECTED` rollup
  placeholder is not evidence and a run from a superseded generation is not
  coverage. Nothing is claimed missing inside the bounded grace period or from a
  partial provider read. A proven gap is reported as `missing_required_runs`
  (or `cancelled_superseded_required_runs`) with the exact PR, head, and
  contexts, degrades `scheduler_status` to `operator_action`, and is recovered
  only by exactly one auditable check-suite rerequest against the unchanged
  head. Empty commits, close/reopen loops, force pushes, and broad reruns are
  never implicit workarounds.
- A successful tick names every `head_of_line` stall: the exact blocking PR, its
  one-based queue position, the members waiting behind it, the block class, and
  ordered repair/reshape/evict remedies. A stalled front is an external
  decision, never healthy or idle. Ticking more often cannot resolve a conflict,
  so select work by queue position, not by whichever member is cheapest to fix.
- Multiple caravans may exist. Cross-caravan compatibility and the configured
  admission bound are checked before automatic changes.
- Automatic admission is deterministic: configured `caravan-priority:*` labels
  from highest to lowest, then immutable GitHub `createdAt`, then PR number.
  Never re-sort, skip, or leapfrog the first canonical *eligible* attempt because
  it failed, except under the explicit sync-owned generation-bound greedy policy
  below. Structurally ineligible PRs are not attempts at all: drafts, fork-only
  heads, externally enabled auto-merge, and superseded/ambiguous/invalid
  generations are reported with exact reasons and excluded from ordering, so one
  wedged PR never starves the fleet. Unknown or conflicting configured priority
  labels still block because canonical rank cannot be computed. With zero
  caravans, the first eligible candidate forms a new root caravan.
- That order is the *automatic selection* contract. Selection and intent are
  separate axes, and conflating them is what regressed this behaviour in 0.0.10.
  Automatic priority/FIFO selection is bound by order for `new` and `join`
  alike. Explicit owner intent — naming one exact remote PR with `cara check
  --pr N`, with or without `--tail-pr`/`--head-pr` — is resolved *before* FIFO
  rejection for `new` and `join` alike: it may attach ahead of earlier rows
  while every bypassed row is an unrelated *unjoined* first-admission attempt,
  and those rows keep their canonical order. A joined row, a base-chain
  dependency, a rank-indeterminate row, a candidate that is not itself an
  ordered attempt, or an ambiguous/missing target all fail closed. An owner
  operating on their own checked-out PR (local `check`, membership,
  renew/rejoin) reports canonical position as evidence only. Every
  check/membership receipt carries the typed `admission_intent` decision:
  selection, intent, target, rows bypassed only because they are unjoined,
  compatibility, provider mutation, and idempotency; the human note is derived
  from that same decision, so note, decision, and mutation cannot disagree.
- Cacophony-shaped PRs bind generation, agent, source head, stack slot, and bead
  metadata. Within the same agent/bead/slot, only a unique exact contained
  successor or current reviewed canonical-link receipt is admissible. Proven
  older PRs are `superseded_generation` and excluded without blocking the
  canonical successor; divergent, invalid, or unproved siblings fail closed.
  Different agents and declared stack parent/child slots are independent. Every
  membership path re-lists exact generation facts immediately before mutation;
  Cara reports safe owner close/reflect actions but never auto-closes a PR.

FIRST USE AND EVERY SAFE TICK
Cara resolves one exact Git worktree root before config or mutation, so nested
invocations share root config/state; outside a non-bare worktree writes nothing.
Before landing a config/pin change, CI runs `cara config check --config PATH` with
the exact pinned binary. It performs strict parsing and no repository/provider access.
1. Run `cara status` (prefer `--json` for automation). It reports initialization,
   effective `rebase_on_join` mode, current/merged branch context, every caravan,
   canonical admission order, pauses, exact base/head/candidate lineage, CI
   generation freshness, compatibility, default-branch movement, and problems.
   Reused branch text selects a fresh PR only for one exact OPEN same-repository
   local/remote/provider head with no retained Caravan membership conflict;
   ambiguity, forks, or OID drift stop.
2. If initialization is not ready, run idempotent `cara init`. Do not hand-create
   labels or silently overwrite mismatched metadata. Repair the exact reported
   repository setting, protection, label, permission, or config mismatch.
3. Inspect `cara next-candidate`. Use `cara check --pr N` to validate the exact
   remote PR without checkout or mutation. A targetless check recommends joining
   the one visible unheld caravan when that attachment is clean, otherwise it
   falls back to `new`; zero or multiple caravans retain `new` because a later
   targetless join would be ambiguous. Add `--tail-pr T` (or `--head-pr H`) for
   an exact join target. Follow the typed `new`, `join`, `repair`, `wait`, or
   `reject` action and its coherent target from the returned receipt.
4. Use `cara new --pr N`, `renew --pr N`, `join --pr N`, or `rejoin --pr N`
   for checkout-free Saloon actions; omit `--pr` only when the checkout resolves
   one unique open PR. Use membership only after that preflight.
   With `rebase_on_join: true`, post-rewrite rediscovery is operation-specific:
   join/rejoin require the exact live tail, while new/renew require the exact
   current default generation and deliberately no join tail. Candidate-head,
   default, tail, or unexpected-membership drift stops before membership writes.
   For checkout-free orchestration, `cara join --pr N --tail-pr T` holds the
   repository operation lock, re-reads the canonical live tail, and first
   requires its root at the exact current default. It receipts source
   branch/head/parent/tree/patch/title plus tail/result-tree identity; stable
   patch matching removes changes already on current main, and empty effective
   source is a durable zero-mutation no-op. It rebases only the unique source patch
   onto the tail, re-reads provider facts, refuses admission
   if the tail moved, sets the provider base,
   and returns a versioned exact join receipt. Routine membership carries any
   durable PR-scoped `caravan-force` intent unchanged across base and history
   transitions. Membership operations are optimistic and resumable; rerun the same
   command after an indeterminate provider response rather than inventing state.
5. Run `cara plan sync` (usually `--all`) to inspect the exact current
   physical/conflict/lease and first auto-admission plan with zero provider
   writes. Review ordered actions, no-ops, decisions, and rediscovery barriers.
6. Run `cara sync` for the current caravan or `cara sync --all` for the fleet.
   Continue until it converges, reports waiting CI, or returns one typed decision.
   With `sync.actions.join_unlabelled_prs: true`, only sync-all also grows the
   fleet after existing caravans converge: `priority_fifo_greedy_v1` tries
   canonical candidates and deterministic live tails, joins the first compatible
   target, and records exact generation-bound `caravan-join-skipped` evidence
   before considering a later candidate. Manual membership consumes that label.
   The same path powers `loop` and `loop --once`; every tick reports exact
   candidate/mutation/GitHub/wall bounds, joins, skips, remaining work, and
   continuation. The unbounded `loop` never exits on a domain failure: it
   dispatches the canonical event to configured hooks, prints bounded failure
   evidence, and ticks again, so stale leases, moved default branches, and
   pending decisions converge without a restart. `loop --once` still returns one
   typed error for schedulers. Fleet scanning never starts a candidate below its reserved
   exact-Git budget; selected remote PRs reuse fresh fleet facts and receive a
   new bounded deadline for refetch, compatibility, physical Git, and mutation.
7. Preserve the complete structured error. Its category, code, exact OIDs,
   affected PRs, completed steps, provider receipts, suggested actions, and
   resumable command are the continuation contract. Never replace it with a
   generic summary or proceed around it manually.
8. Human `cara sync` and `cara loop` stream bounded stage progress (discovery,
   physical planning/apply, midpoint revalidation, provider convergence,
   auto-admission, final rediscovery) to stderr while the tick runs, so a long
   network operation is never silent. JSON and MCP callers install no observer
   and keep byte-identical envelopes.

VIRTUAL CHAINS (SAFE DEFAULT)
With `rebase_on_join: false` or an absent setting, Caravan does not rewrite PR
history. It maintains the chain through PR base refs and proves mechanical
compatibility. If a parent head changes and a child no longer applies, sync
returns a typed conflict and explicitly reports `rebase_on_join=disabled` plus
the exact config action. This mode is appropriate when force-pushing agent
branches is not authorized.

PHYSICAL FIXED-POINT CHAINS (`rebase_on_join: true`)
This opt-in authorizes Cara to rewrite owned PR branches under exact leases. It
is designed for cumulative, trustworthy parallel CI. If commits are logically:

  A, B, C, D, E

Cara materializes and pushes heads equivalent to:

  A, AB, ABC, ABCD, ABCDE

Thus every child contains the exact planned parent generation. After the serial
branch rebuild, GitHub can run fresh CI for all rewritten PR heads in parallel;
old check runs are invalidated and never treated as proof for the new generation.

For membership, Cara prepares one candidate. For `sync --all`, Cara:
1. selects non-paused caravans and plans each chain head-to-tail;
2. materializes every rebase exactly once in retained isolated worktrees;
3. feeds each planned parent OID into its child without pretending that
   not-yet-pushed OID already exists remotely;
4. verifies every conflict, workflow trigger, PR precondition, default/workflow
   OID, old branch head, branch-set disjointness, push permission, and exact
   force-with-lease before the first write;
5. disables auto-merge only on selected members whose branch generation is
   actually rewritten, after that global barrier. An already-satisfied ancestry
   plan retains native arming rather than opening a durability window;
6. applies parent before descendant. A chain is always serial. Only independent,
   disjoint caravans may apply concurrently, with a bounded worker count;
7. performs mandatory midpoint GitHub rediscovery, verifies every pushed head,
   refreshes invalidated CI, and only then runs normal base/CI/auto-merge sync;
8. performs final rediscovery as the authoritative completion receipt.

A planning conflict writes nothing. An apply-time race may leave an exact
successfully rebuilt prefix; Cara records that prefix and skips its descendants.
Never force-rollback it. Rediscover and rerun the same idempotent sync. Merged or
deleted predecessors are promoted safely using their durable pull-request head
ref, then the active successor is rebuilt onto the exact default branch and its
children onto the new successor generations.

Before enabling physical mode in an existing repository:
- ensure a global `pull_request` workflow has no `branches`/`branches-ignore`
  filter and includes `opened`, `synchronize`, `reopened`, `edited`, and
  `labeled` activity types (normally `unlabeled` too);
- gate jobs on default-base or `caravan` label so child PRs receive CI;
- canary a disposable or paused caravan: commit `rebase_on_join: true`, run
  `cara status`, `cara check`, then one `cara sync --all`, and inspect plans,
  leases, rewritten heads, midpoint facts, and fresh CI;
- roll back by reverting the config to false. Do not force-push branches back.

SQUASH-EQUIVALENT STACKED HISTORY
- A landed member arrives on the default branch as one squash commit: identical
  cumulative content, unrelated commit identity. Later members keep replaying
  the pre-squash commits, so an attachment can conflict against content that is
  identical to what it introduces. Git's own patch-identity pruning does not
  cover a squash that combined several source commits.
- Every non-clean attachment check (head/default, adjacent pair, head after
  another caravan tail) also emits exact squash-equivalence evidence for the
  same revisions. It is evidence, never authority.
- A prefix is proven only when it is an ancestor-closed linear prefix of the
  candidate-only range and every path its cumulative diff changes has an
  identical blob object and file mode on the exact target tip, and only when
  replaying the retained commits from that boundary is independently clean.
  Commit messages, subjects, authorship, and patch text are never proof; an
  identical patch with a different resulting blob is not equivalence; an
  untouched identical file proves nothing.
- Outcomes are `reconcilable`, `no_equivalence`, `residual_conflict` (prefix
  represented, retained commits still diverge), and `indeterminate` (absent or
  ambiguous merge base, non-linear range, unrepresentable path). Only
  `reconcilable` authorizes a boundary. Ordinary three-way divergence after the
  equality point is never reconciled and never resolved by taking either side.
- Applying reconciliation is a separate explicitly authorized rewrite. It
  reverifies that the replayed head tree equals the proven cumulative tree and
  that the rebuilt commit count equals the proven retained set, failing closed
  before any push. Receipts list dropped/retained commits, the proven boundary
  and its tree, represented paths with blobs and modes, and the cumulative tree
  before and after reconciliation.

SYNC-OWNED GREEDY ADMISSION
- The policy is disabled by default and requires both `sync --all` and
  `sync.actions.join_unlabelled_prs: true`. Targeted sync never grows the fleet.
- Existing graph/provider/CI/semantic/operator decisions stop before admission;
  waiting CI and intentional holds remain structurally valid.
- Candidates use configured priority then immutable FIFO; caravans use existing
  deterministic order. The first compatible tail wins. This is deliberately
  best-effort, not a global optimum.
- An incompatible exact generation gets `caravan-join-skipped` and a GitHub
  comment binding candidate head/base, default, all tested tails, config,
  reasons, heuristic, actor, and time. Unchanged evidence is not retried.
  Candidate/default/tail/config/heuristic changes invalidate the skip.
- One tick holds the operation lock and an absolute deadline, counts every
  authenticated `gh` subprocess, and stops at configured candidate/mutation
  bounds. Rediscovery follows every mutation; exact retries resume idempotently.

CI DECISIONS
- Empty, expected, queued, or running checks are waiting, never passing.
- Failed decisions contain bounded run/job/failed-step evidence and exact
  selected-ref lineage where the workflow emits the supported machine receipt.
  Raw logs, unrelated lines, and secrets are not retained.
- A stale run/candidate requires a fresh exact-candidate trigger and is never
  rerunnable. A current-generation infrastructure failure may be retried with
  `cara sync --rerun-failed`. A source/test failure requires repair. Cancelled,
  missing, truncated, or unknown evidence fails closed to wait/fresh-trigger.
- Rerun only IDs Cara lists. It verifies the exact PR association and head again
  immediately before requesting failed-job rerun.

REPAIR WORKSPACES; NO RAW GIT SURGERY
At a conflict/repair decision, use `cara repair start --pr N [--target-pr T]`.
Cara creates or reuses a provider-cloned exact-head workspace with bounded,
persisted evidence. Use `cara repair status` to inspect it. Typed conflicts and
semantic grants remain narrow deterministic scopes. After a typed semantic/CI
decision, `repair authorize-agent-edits --actor A --reason R` may authorize one
exact identity for bounded repository-content edits; continue requires the same
actor and records complete path/staged-index/diff fingerprints. Secret-like,
symlink/gitlink, unstaged/untracked, out-of-scope, and drifted edits fail closed.
`repair revoke-grant` restores recorded pre-grant blobs. Stage reviewed changes
and run `cara repair continue --session ID [--actor A]`: it verifies the session,
uses non-force publication, requires fresh CI, and resumes sync-all. For human
flow testing, `cara loop --manual [--shell 'zsh -i']` opens a real TTY only at
external decisions, exports a private `CARA_DECISION_FILE`, releases the lock,
and always rediscovers after shell success; it is never JSON/MCP/hook behavior.
Use `repair abort` only to remove a reviewed local session. Never create nested raw worktrees, call
`update-ref`, merge behind Cara, or force-push a repair branch.

RESHAPING AND EXPLICIT INTENT
- `cara evict --pr N --reason ...` removes a member and reconnects its child only
  after exact compatibility proof. `split` makes the selected PR a new head.
  `renew` and `rejoin` re-evaluate an evicted PR from fresh facts.
- `cara force --pr N --actor A --reason R` is the supported way to arm durable
  PR-scoped `caravan-force` intent; raw label edits are not an operator contract.
  It requires `force_merge: true`, an open non-draft active member, a clean
  current Caravan edge, no selected-Caravan hold/graph issue, ADMIN permission,
  and a durable actor/reason audit. Intent follows the PR through Cara-owned
  rewrites and position changes. When that PR reaches root, sync skips all CI
  observation and immediately performs fresh exact root/default compatibility,
  ownership, permission, and lease preflight before the ADMIN squash. Explicit
  revoke, eviction, or successful merge consumes intent; unrelated admission
  candidates never block it.
- Controller-reviewed exact exceptions use `cara --json force-intent
  preview|apply|revoke --pr N --head OID --membership-generation G
  --failure-fingerprint F --reason R --expires-at-ms T --auto-merge squash`
  and matching MCP tools. CI decisions expose the same deterministic fingerprint.
  Preview performs no writes. Apply independently re-reads exact head,
  membership, checks, decision, default branch, compatibility, holds, graph,
  and permission, then converges durable force intent with native auto-merge
  disabled in one provider mutation and refetches the complete postcondition. Revoke accepts
  expired authority, removes only force intent, preserves queue-owned auto-merge,
  and is idempotent. Partial provider/audit results retain exact retry receipts.
- Use audited `cara priority set|clear` to change one exact unenrolled PR's
  configured automatic-admission rank or restore FIFO. Priority is scheduling
  metadata only: it never authorizes membership, changes topology, or bypasses
  compatibility. Unknown/conflicting metadata and stale PR/config facts fail
  closed before overwrite.
- Use `cara pause` with actor and reason for incidents or maintenance. Pause
  disables only the exact head auto-merge and preserves topology. Expiry is a
  warning, never implicit resume. Only `cara resume` may revalidate exact facts,
  record authorization, and restore policy. A stale hold fails closed.

BUILT-IN WEB OPERATIONS
- `cara web --repo PATH [--repo PATH ...]` serves a responsive dashboard from
  assets embedded in the Cara binary. Repository arguments are filesystem
  paths, never slugs, and are the complete trust boundary for the process.
- The default listener is loopback-only. Status snapshots refresh on a bounded
  cadence and show active caravan trails, exact PR generations and CI, open
  unqueued/rejected PRs with reasons, decisions, pauses, and scheduler health.
- Use `--read-only` to disable mutation endpoints. Accepted actions bind the
  reviewed refresh sequence and a deterministic mutation-authority fingerprint.
  Poll/webhook refreshes coalesce behind active jobs; identical provider/config
  facts tolerate sequence-only drift, while changed fingerprints fail before
  mutation with exact expected/actual evidence. Interactive actions use the same
  typed domain functions, preconditions, operation locks, decisions, and receipts
  as CLI/JSON/MCP; the web server never shells out to human output.
  Long actions return an action ID immediately, continue under one repository
  lock across browser disconnects, and expose bounded durable checkpoint phases,
  terminal receipts, and the newest Cara event/hook journal records.
- Static HTML, CSS, and JavaScript ship inside `cara`; no CDN or separate web
  deployment is required. Future opt-in repository discovery must remain
  explicit and is not performed by the initial path-scoped release.
- Optional GitHub App webhooks require an HMAC secret environment variable and
  exact installation ID. Signed explicit-repository deliveries are durably
  deduped and coalesced into refresh or bounded sync-all wakes; they are never
  provider truth, and polling remains fallback reconciliation.

RECOVERY, LOCKS, AND OBSERVABILITY
- GitHub is the resume cursor. After timeout, interruption, or partial provider
  failure, inspect receipts and rerun the same command. Do not guess whether a
  mutation happened. Only genuine provider-generation races are `retry_tick`.
  Deterministic unsupported physical histories such as octopus/cousin/external
  merge topology, ambiguous, or empty candidate ranges are non-retryable
  external decisions: use their stable fingerprint and exact
  repair/reshape/evict/strategy choices rather than repeating the unchanged
  tick. Bounded owned two-parent topology is preserved only after exact topology
  mapping and independent clean merge-tree proof.
- `cara lock status` reports owner token, PID liveness, checkpoint phase, and
  provider indeterminacy. Sync checkpoint evidence is always bounded below the
  owner-file limit: complete counts/hashes plus first/last samples preserve
  recovery context without embedding unbounded plans, receipts, or events.
  Use `lock recover --confirm` only for the typed,
  verified-stale owner; never delete lock files manually.
- A dirty worktree or active Git operation blocks checkout-sensitive recovery
  before provider mutation. Make it safe or use the Cara repair workspace.
- `cara log` reads the bounded event journal; hooks consume canonical event IDs
  and must deduplicate retries. Hook failure cannot roll back completed GitHub
  work; blocking hooks return typed partial receipts. `examples/hooks/` contains
  a complete fast cron-driven setup: a bounded `cara loop --once` tick plus an
  idempotent hook that files exactly one deduplicated Cacophony bead per
  canonical event, which normal controller dispatch then routes to an agent.
- `cara show`, `next`, `prev`, and `van list|next|prev` are navigation surfaces;
  they never authorize skipping admission or mutation preflight. If a clean,
  non-current destination lags a Cara physical rewrite, navigation reverifies
  the provider OID, atomically preserves the old local OID under
  `refs/cara-backup/navigation/*`, reports that receipt, and then checks out the
  exact branch. Current/other-worktree branches and dirty state fail closed.
- `--json` and MCP return the same typed envelopes and schemas. Unknown provider
  values are preserved rather than guessed into success. A subprocess exceeding
  its independent stdout/stderr capture bound returns `command_output_limit`
  with total bytes and bounded separate prefix/suffix evidence; truncated JSON
  is never passed to a decoder or misreported as malformed provider data.
- GitHub credentials are selected statelessly per repository. Cara parses the
  origin host/owner, first accepts an ambient `GH_TOKEN`/`GITHUB_TOKEN` only if
  it can read that exact repository, then tries the stored token for the owner
  login, and finally probes other successful `gh` accounts. Selection is cached
  only for the process; Cara never runs `gh auth switch`, stores a preferred
  account in project config, or exposes a token in commands/errors/receipts.
  Opt-in App auth requires `CARA_GITHUB_AUTH_MODE=app_installation`, a
  `CARA_GITHUB_APP_CREDENTIAL_COMMAND`, expected App slug, and installation ID.
  A broker path alone never activates. Cara validates exact identity/expiry,
  single-flights refresh, exposes only secret-free App telemetry, and never
  falls back to ambient auth on incomplete/invalid App settings or broker failure. Remote Git operations use that same cached
  principal through an environment-only HTTPS credential helper. SSH, plaintext
  HTTP, local/mismatched remotes, and credential-bearing URLs fail before Git.
  The least-privilege permission/branch/webhook/single-writer baseline is
  machine-checked in `docs/github-app-policy.json` and explained in
  `docs/github-app.md`; live bot attribution remains a separate canary.
- `cara self-update status|check|run` updates only the exact running first-PATH
  stable user binary (`~/.cargo/bin`, `~/.local/bin`, or an exact explicit
  `CARA_SELF_UPDATE_INSTALL_DIR`). Shadowed, renamed/test, Cargo target, and
  package-manager binaries fail closed. A tagged patch release is required
  before release-only deployments can observe newly landed main behavior.

When in doubt: stop, preserve the structured evidence, make only the suggested
safe change, and rerun the same Cara command from fresh GitHub facts. This help
text is the complete operating contract; no external specification is required.";

/// Structured domain error shared by CLI JSON and MCP responses.
#[derive(Debug, Clone)]
pub struct AppError {
    category: ErrorCategory,
    code: String,
    message: String,
    details: Option<Value>,
}

impl AppError {
    pub(crate) fn structured(
        category: ErrorCategory,
        code: impl Into<String>,
        message: impl Into<String>,
        details: Option<Value>,
    ) -> Self {
        Self {
            category,
            code: code.into(),
            message: message.into(),
            details,
        }
    }

    /// Construct a validation error.
    #[must_use]
    pub fn validation(code: &str, message: impl Into<String>) -> Self {
        Self {
            category: ErrorCategory::Validation,
            code: code.to_owned(),
            message: message.into(),
            details: None,
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}

impl StructuredError for AppError {
    fn category(&self) -> ErrorCategory {
        self.category
    }

    fn code(&self) -> String {
        self.code.clone()
    }

    fn message(&self) -> String {
        self.message.clone()
    }

    fn details(&self) -> Option<Value> {
        self.details.clone()
    }
}

/// Context shared by MCP tools.
#[derive(Debug, Clone)]
pub struct AppContext {
    /// Repository/worktree used by Git, GitHub, and compatibility adapters.
    pub repository_path: PathBuf,
    /// Resolved `.caravan/config.yaml` path (or explicit override).
    pub config_path: PathBuf,
    /// Whether the resolved file existed; absent defaults remain visible.
    pub config_existed: bool,
    /// Validated repository policy shared by every tool call.
    pub config: CaravanConfig,
}

impl AppContext {
    /// Resolve and validate repository/config identity once for a CLI/MCP session.
    /// Default config is rooted at the exact Git worktree, never ambient cwd.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let invocation_directory =
            std::env::current_dir().map_err(|error| ConfigError::RepositoryNotFound {
                path: PathBuf::from("."),
                message: error.to_string(),
            })?;
        Self::load_from_directory(&invocation_directory, path)
    }

    /// Load policy for an explicit repository directory rather than the
    /// invocation directory, so `--repo` can target a checkout the caller is not
    /// standing in.
    pub fn load_from_directory(
        invocation_directory: &Path,
        path: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        // Preserve explicit-config parse precedence for stable machine errors:
        // a malformed requested file is reported even before repository lookup.
        let explicit_config = path
            .map(|explicit| {
                let resolved = if explicit.is_absolute() {
                    explicit.to_path_buf()
                } else {
                    invocation_directory.join(explicit)
                };
                CaravanConfig::load_or_default(Some(&resolved))
                    .map(|loaded| (resolved, loaded.existed, loaded.config))
            })
            .transpose()?;
        let repository_path = resolve_repository_root(invocation_directory)?;
        let (config_path, config_existed, config) = if let Some(explicit) = explicit_config {
            // A relative --config is relative to invocation cwd, not silently
            // rebased to repository root. Store it absolute so downstream
            // safety checks cannot reinterpret it against repository_path.
            explicit
        } else {
            let relative = PathBuf::from(config::DEFAULT_CONFIG_PATH);
            let resolved = repository_path.join(&relative);
            if resolved.exists() {
                (relative, true, CaravanConfig::load(&resolved)?)
            } else {
                (relative, false, CaravanConfig::default())
            }
        };
        Ok(Self {
            repository_path,
            config_path,
            config_existed,
            config,
        })
    }
}

fn resolve_repository_root(directory: &Path) -> Result<PathBuf, ConfigError> {
    let runner =
        command::ProcessRunner::in_directory(directory).with_timeout(Duration::from_secs(5));
    let request = command::CommandSpec::new("git").args(["rev-parse", "--show-toplevel"]);
    let output = runner.run(&request).map_err(|error| {
        // A probe that did not FINISH is not a repository that does not EXIST.
        // Mapping every runner failure to `RepositoryNotFound` sent readers to
        // check paths and permissions after a 5s deadline expired under load on
        // a perfectly valid checkout (bd-f42a5e).
        if matches!(error, command::CommandRunError::Timeout { .. }) {
            return ConfigError::RepositoryProbeTimeout {
                path: directory.to_path_buf(),
                message: error.to_string(),
            };
        }
        ConfigError::RepositoryNotFound {
            path: directory.to_path_buf(),
            message: error.to_string(),
        }
    })?;
    if !output.is_success() || output.stdout.trim().is_empty() {
        let stderr = output.stderr.trim();
        return Err(ConfigError::RepositoryNotFound {
            path: directory.to_path_buf(),
            message: if stderr.is_empty() {
                "not inside a non-bare Git worktree".to_owned()
            } else {
                stderr.to_owned()
            },
        });
    }
    std::fs::canonicalize(output.stdout.trim()).map_err(|error| ConfigError::RepositoryNotFound {
        path: PathBuf::from(output.stdout.trim()),
        message: error.to_string(),
    })
}

impl Default for AppContext {
    fn default() -> Self {
        Self {
            repository_path: PathBuf::from("."),
            config_path: PathBuf::from(config::DEFAULT_CONFIG_PATH),
            config_existed: false,
            config: CaravanConfig::default(),
        }
    }
}

/// Empty input for parameterless commands.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, Args)]
pub struct EmptyInput {}

/// Target an existing caravan either by its current tail or its rolling head ID.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, Args)]
pub struct TargetInput {
    /// Exact tail PR to use as the proposed merge target.
    #[arg(long, value_name = "PR", conflicts_with = "head_pr")]
    #[serde(default)]
    pub tail_pr: Option<u64>,

    /// Caravan head PR; its current tail is resolved immediately before mutation.
    #[arg(long, value_name = "PR", conflicts_with = "tail_pr")]
    #[serde(default)]
    pub head_pr: Option<u64>,
}

/// Input for `cara check`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, Args)]
pub struct CheckInput {
    /// Exact remote candidate PR. When omitted, use the current checkout's PR.
    /// Without a target, check may recommend the one unambiguous clean live
    /// caravan; mutation commands retain their explicitly requested operation.
    #[arg(long, value_name = "PR")]
    #[serde(default)]
    pub pr: Option<u64>,

    /// Exact tail PR to check as the proposed merge target.
    #[arg(long, value_name = "PR", conflicts_with = "head_pr")]
    #[serde(default)]
    pub tail_pr: Option<u64>,

    /// Caravan head PR; resolve and check against its current tail.
    #[arg(long, value_name = "PR", conflicts_with = "tail_pr")]
    #[serde(default)]
    pub head_pr: Option<u64>,
}

/// Input for `new` and `renew`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, Args)]
pub struct CreateInput {
    /// Exact remote candidate PR for checkout-free root admission.
    #[arg(long, value_name = "PR", conflicts_with = "create_pr")]
    #[serde(default)]
    pub pr: Option<u64>,

    /// Create the current branch's PR non-interactively with `gh pr create --fill`.
    #[arg(long)]
    #[serde(default)]
    pub create_pr: bool,

    /// Human/agent admission rationale; otherwise a deterministic mechanical reason is used.
    #[arg(long, value_name = "TEXT")]
    #[serde(default)]
    pub reason: Option<String>,

    /// Exact configured agent-priority label. Without it, admission is FIFO.
    ///
    /// This is persistent metadata for AUTOMATIC scheduling order only. It is
    /// never permission to run an explicit membership action: naming one exact
    /// candidate already carries owner authority (bd-72d56d).
    #[arg(long, value_name = "LABEL")]
    #[serde(default)]
    pub priority_label: Option<String>,
}

/// Input for `join` and `rejoin`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, Args)]
pub struct JoinInput {
    /// Exact remote candidate PR. Required for checkout-free atomic integration.
    #[arg(long, value_name = "PR", conflicts_with = "create_pr")]
    #[serde(default)]
    pub pr: Option<u64>,

    /// Exact tail PR to use as the proposed merge target.
    #[arg(long, value_name = "PR", conflicts_with = "head_pr")]
    #[serde(default)]
    pub tail_pr: Option<u64>,

    /// Caravan head PR; resolve its current tail immediately before mutation.
    #[arg(long, value_name = "PR", conflicts_with = "tail_pr")]
    #[serde(default)]
    pub head_pr: Option<u64>,

    /// Create the current branch's PR non-interactively when it does not exist.
    #[arg(long)]
    #[serde(default)]
    pub create_pr: bool,

    /// Human/agent admission rationale; otherwise selected-target policy is recorded.
    #[arg(long, value_name = "TEXT")]
    #[serde(default)]
    pub reason: Option<String>,

    /// Exact configured agent-priority label. Without it, admission is FIFO.
    ///
    /// This is persistent metadata for AUTOMATIC scheduling order only. It is
    /// never permission to run an explicit membership action: naming one exact
    /// candidate already carries owner authority (bd-72d56d).
    #[arg(long, value_name = "LABEL")]
    #[serde(default)]
    pub priority_label: Option<String>,
}

/// Input for `cara pause`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Args)]
pub struct PauseInput {
    /// Current rolling caravan head to freeze.
    #[arg(long, value_name = "PR")]
    pub head_pr: u64,
    /// Audited human or agent identity (non-secret).
    #[arg(long)]
    pub actor: String,
    /// Bounded incident/maintenance rationale.
    #[arg(long)]
    pub reason: String,
    /// Optional Unix timestamp after which status reports the hold expired.
    /// Expiry never resumes the caravan automatically.
    #[arg(long)]
    #[serde(default)]
    pub expires_unix_secs: Option<u64>,
    /// Optional external incident, hold, or choice reference.
    #[arg(long)]
    #[serde(default)]
    pub external_reference: Option<String>,
}

/// Input for explicit `cara resume`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Args)]
pub struct ResumeInput {
    /// Exact paused rolling head.
    #[arg(long, value_name = "PR")]
    pub head_pr: u64,
    /// Audited human or agent identity authorizing resume.
    #[arg(long)]
    pub actor: String,
}

/// Input for `cara sync`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, Args)]
pub struct SyncInput {
    /// Synchronize every caravan rather than only the current branch's caravan.
    #[arg(long)]
    #[serde(default)]
    pub all: bool,

    /// Preview the exact tick without any provider mutation.
    ///
    /// Identical to `cara plan sync`. Offered here because the person deciding
    /// whether a tick is safe is already typing `cara sync`, and a dry-run
    /// nobody can find is a dry-run nobody uses: an agent asked to predict a
    /// tick read admission rules from source instead, mixed the admission-time
    /// rule with the merge-time rule, and told an operator the wrong outcome
    /// (bd-3119d9).
    #[arg(long)]
    #[serde(default)]
    pub dry_run: bool,

    /// Rerun only the exact failed workflow runs identified by the first CI decision.
    #[arg(long)]
    #[serde(default)]
    pub rerun_failed: bool,
}

/// Input for `cara evict`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Args)]
pub struct EvictInput {
    /// PR to evict; defaults to the current branch's PR.
    #[arg(long, value_name = "PR")]
    #[serde(default)]
    pub pr: Option<u64>,

    /// Human/agent rationale included in the eviction event and hook metadata.
    #[arg(long, value_name = "TEXT")]
    pub reason: String,

    /// Also evict every member after the selected PR (bd-e9187e).
    ///
    /// Members are released tail-first, so no surviving edge is ever re-linked
    /// across a removed member and each individual removal is trivially
    /// compatible.
    #[arg(long)]
    #[serde(default)]
    pub cascade: bool,

    /// Evict every member of the selected caravan, dissolving it entirely.
    #[arg(long)]
    #[serde(default)]
    pub all: bool,
}

/// Input for `cara split`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, Args)]
pub struct SplitInput {
    /// Non-head PR that becomes the head of the new caravan; defaults to current.
    #[arg(long, value_name = "PR")]
    #[serde(default)]
    pub pr: Option<u64>,
}

/// Input for the foreground `cara loop` process.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, Args)]
pub struct LoopInput {
    /// Override `.caravan/config.yaml`'s tick interval.
    #[arg(long, value_name = "SECONDS")]
    #[serde(default)]
    pub interval_secs: Option<u64>,

    /// Run one `sync --all` tick and exit; useful for schedulers and smoke tests.
    #[arg(long)]
    #[serde(default)]
    pub once: bool,

    /// CLI-only human controller: open an interactive shell at external decisions, then rediscover and retry.
    #[arg(long)]
    #[serde(default)]
    pub manual: bool,

    /// Interactive command for --manual; defaults to `$SHELL -i`.
    #[arg(long, value_name = "COMMAND", requires = "manual")]
    #[serde(default)]
    pub shell: Option<String>,
}

fn default_lock_stale_after_secs() -> u64 {
    operation_lock::DEFAULT_STALE_AFTER.as_secs()
}

/// Input for read-only operation-lock inspection.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Args)]
pub struct LockStatusInput {
    /// Age threshold used to classify a lock as stale.
    #[arg(long, default_value_t = default_lock_stale_after_secs())]
    #[serde(default = "default_lock_stale_after_secs")]
    pub stale_after_secs: u64,
}

impl Default for LockStatusInput {
    fn default() -> Self {
        Self {
            stale_after_secs: default_lock_stale_after_secs(),
        }
    }
}

/// Input for guarded stale operation-lock recovery.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Args)]
pub struct LockRecoverInput {
    /// Exact owner token copied from `cara lock status`.
    #[arg(long)]
    pub token: String,
    /// Minimum lock age required before recovery.
    #[arg(long, default_value_t = default_lock_stale_after_secs())]
    #[serde(default = "default_lock_stale_after_secs")]
    pub stale_after_secs: u64,
    /// Explicit acknowledgement that this operation removes canonical lock state.
    #[arg(long)]
    #[serde(default)]
    pub confirm: bool,
}

/// Output of the real `cara help` command/tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HelpOutput {
    /// Normative agent operating instructions.
    pub instructions: String,
    /// Stable contract-source marker; all instructions are embedded.
    pub spec: String,
}

/// Return agent operating instructions.
#[must_use]
pub fn help() -> HelpOutput {
    HelpOutput {
        instructions: AGENT_HELP.to_owned(),
        spec: SPEC_PATH.to_owned(),
    }
}

#[cfg(test)]
fn validate_target(tail_pr: Option<u64>, head_pr: Option<u64>) -> Result<(), AppError> {
    if tail_pr.is_some() && head_pr.is_some() {
        return Err(AppError::validation(
            "ambiguous_target",
            "--tail-pr and --head-pr are mutually exclusive",
        ));
    }
    Ok(())
}

/// Build the complete MCP command router.
///
/// Keeping the manifest registrations together makes CLI/MCP surface drift
/// reviewable in one place despite the deliberately broad v1 command set.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn build_router() -> ToolRouter<AppContext> {
    let mut router = ToolRouter::new();

    router.add_typed_tool_with_output_schema(
        "help",
        "Return concise agent instructions for operating Caravan and recovering from sync decision points.",
        |_context: &AppContext, _input: EmptyInput| Ok::<_, AppError>(help()),
    );
    router.add_typed_tool_with_output_schema(
        "init",
        "Explicitly create a missing version-1 config and required repository labels, then verify permissions, default-branch protection, and squash auto-merge policy. Existing compatible resources are never changed.",
        |context: &AppContext, _input: EmptyInput| initialization::init(context),
    );
    router.add_typed_tool_with_output_schema(
        "log",
        "Return a bounded, deterministically ordered snapshot of canonical events and secret-free hook delivery receipts. Follow mode is CLI-only.",
        |context: &AppContext, input: journal::LogInput| journal::snapshot(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "status",
        "Discover the current repository, current PR, every caravan, invalid graph fragments, and unresolved decision points. Read-only.",
        |context: &AppContext, _input: EmptyInput| read::status(context),
    );
    router.add_typed_tool_with_output_schema(
        "queue",
        "Report the next pull request at a requested queue position (ready, skipped, conflict, evicted) and optionally check it out. Selection is not compatibility proof: run check/new preflight before acting. Nothing matching is an ordinary payload, never an error, so an empty queue is distinguishable from a provider outage.",
        |context: &AppContext, input: NextInput| next::next(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "check",
        "Preflight an exact remote candidate with --pr, or the current PR when omitted, without checkout or provider mutation. A targetless check recommends the one visible unheld caravan when its exact attachment is eligible, otherwise it falls back to new; zero or multiple caravans retain new because targetless join would be ambiguous. Optionally test an exact --tail-pr or resolved --head-pr. Returns exact facts, a coherent typed admission-intent decision/target, and one mechanical next action without changing explicit new mutation preflight.",
        |context: &AppContext, input: CheckInput| read::check(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "new",
        "After complete repository/graph/compatibility preflight, label and retarget the current open PR as a one-PR caravan and enable squash auto-merge. Exact stale facts abort; rediscover and rerun to resume partial receipts.",
        |context: &AppContext, input: CreateInput| membership::new(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "renew",
        "After complete preflight, reevaluate an evicted current PR as a new caravan; remove eviction while preserving any newly armed durable force intent, then converge the configured merge actor. On typed failure repair the evidence and rerun.",
        |context: &AppContext, input: CreateInput| membership::renew(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "join",
        "After complete compatibility preflight, retarget and label the current PR after a selected or uniquely inferred tail with auto-merge off. On ambiguity or stale facts, follow the typed candidates/evidence and rerun without guessing.",
        |context: &AppContext, input: JoinInput| membership::join(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "rejoin",
        "After complete compatibility preflight, append an evicted PR after a valid tail and remove eviction/force labels. Typed partial receipts are resumable by rediscovery and the same rejoin call.",
        |context: &AppContext, input: JoinInput| membership::rejoin(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "priority_set",
        "Set one exact configured automatic-admission priority on an unenrolled PR under fresh provider/config preconditions and post a durable audit. This changes ordering only, never compatibility or membership authority.",
        |context: &AppContext, input: priority::PrioritySetInput| {
            priority::set(context, &input)
        },
    );
    router.add_typed_tool_with_output_schema(
        "priority_clear",
        "Clear configured automatic-admission priority from an unenrolled PR under fresh provider/config preconditions, restoring FIFO with a durable audit.",
        |context: &AppContext, input: priority::PriorityClearInput| {
            priority::clear(context, &input)
        },
    );
    router.add_typed_tool_with_output_schema(
        "show",
        "Show the current branch's complete caravan and highlighted position. Read-only.",
        |context: &AppContext, _input: EmptyInput| read::show(context),
    );
    router.add_typed_tool_with_output_schema(
        "next",
        "Check out the next PR toward the current caravan tail. A stale clean non-current destination is retained under an exact internal backup ref before following a reverified Cara-rewritten provider head; current/other-worktree branches and dirty state fail closed.",
        |context: &AppContext, _input: EmptyInput| {
            navigation::navigate(
                context,
                navigation::Scope::Caravan,
                navigation::Direction::Next,
            )
        }
    );
    router.add_typed_tool_with_output_schema(
        "prev",
        "Check out the previous PR toward the current caravan head. A stale clean non-current destination is retained under an exact internal backup ref before following a reverified Cara-rewritten provider head; current/other-worktree branches and dirty state fail closed.",
        |context: &AppContext, _input: EmptyInput| {
            navigation::navigate(
                context,
                navigation::Scope::Caravan,
                navigation::Direction::Previous,
            )
        }
    );
    router.add_typed_tool_with_output_schema(
        "force_arm",
        "Arm durable PR-scoped caravan-force intent on an eligible active member after clean current-edge, policy, selected-Caravan hold/graph, permission, branch and PR preflight. Intent follows Cara-owned rewrites and positions; sync merges immediately once it reaches a mechanically mergeable root.",
        |context: &AppContext, input: force::ForceIntentInput| force::arm(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "force_revoke",
        "Idempotently revoke durable PR-scoped caravan-force intent from an eligible active member under exact preconditions and post an audit without touching unrelated labels.",
        |context: &AppContext, input: force::ForceIntentInput| force::revoke(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "force_intent_preview",
        "Re-read one exact provider head, Caravan membership generation, current check rollup, and CI-decision fingerprint for reviewed bounded force authority. Never mutates.",
        |context: &AppContext, input: force_intent::ReviewedForceIntentInput| {
            force_intent::preview(context, &input)
        },
    );
    router.add_typed_tool_with_output_schema(
        "force_intent_apply",
        "Validate exact reviewed transition evidence and converge durable caravan-force with native auto-merge disabled through one provider transaction under fresh head/check preconditions and durable audit.",
        |context: &AppContext, input: force_intent::ReviewedForceIntentInput| {
            force_intent::apply(context, &input)
        },
    );
    router.add_typed_tool_with_output_schema(
        "force_intent_revoke",
        "Idempotently revoke one reviewed durable PR force intent, including after expiry, while preserving normal queue ownership of squash auto-merge.",
        |context: &AppContext, input: force_intent::ReviewedForceIntentInput| {
            force_intent::revoke(context, &input)
        },
    );
    router.add_typed_tool_with_output_schema(
        "pause",
        "Explicitly freeze one exact caravan head, recording bounded incident metadata and disabling only its squash auto-merge under exact preconditions. Expiry never auto-resumes.",
        |context: &AppContext, input: PauseInput| pause::pause(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "resume",
        "Explicitly resume a paused caravan only after exact head, base, labels, checks, state, and topology revalidation; stale facts fail closed.",
        |context: &AppContext, input: ResumeInput| pause::resume(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "sync",
        "Idempotently synchronize one or all caravans under optimistic preconditions. With strict config opt-in, sync-all greedily admits canonical unlabelled candidates after the existing fleet converges, persisting generation-bound skip receipts under exact wall/GitHub/candidate/mutation bounds.",
        |context: &AppContext, input: SyncInput| sync::sync(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "plan_sync",
        "Build the exact current sync/auto-admission plan through physical conflict and lease dry-run preflight without any provider write. Returns ordered actions, exact preconditions, no-ops, decisions, first admission target, rediscovery barriers, and mutated=false.",
        |context: &AppContext, input: SyncInput| sync::plan_sync(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "repair_start",
        "Create or reuse a Cara-owned isolated exact-head workspace for one typed sync repair. Starts a non-committing exact-target merge without changing the caller checkout, provider branch, labels, or bases.",
        |context: &AppContext, input: repair::RepairStartInput| repair::start(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "repair_authorize_agent_edits",
        "Authorize one exact agent identity to make bounded arbitrary repository-content edits in an exact resolving session. Binds repository/PR/head/target/config/session/actor/reason/expiry, never mutates provider state, and requires complete staged diff receipts at continue.",
        |context: &AppContext, input: repair::RepairAuthorizeAgentEditsInput| {
            repair::authorize_agent_edits(context, &input)
        },
    );
    router.add_typed_tool_with_output_schema(
        "repair_grant",
        "Apply a bounded reviewed source commit's semantic changes to explicit tracked paths in one exact resolving session. Records actor/reason/source/blob/result/expiry receipts and never mutates provider state.",
        |context: &AppContext, input: repair::RepairGrantInput| {
            repair::grant_paths(context, &input)
        },
    );
    router.add_typed_tool_with_output_schema(
        "repair_revoke_grant",
        "Revoke exact semantic grants during resolving, restore their pre-grant staged blobs, and record actor/reason. Requires matching grant authority and never mutates provider state.",
        |context: &AppContext, input: repair::RepairRevokeGrantInput| {
            repair::revoke_grants(context, &input)
        },
    );
    router.add_typed_tool_with_output_schema(
        "repair_continue",
        "Verify staged conflict resolution stayed inside the typed path scope, recheck the exact provider head, create an exact-parent merge commit, publish by ordinary non-force fast-forward, and resume sync-all from the managed workspace.",
        |context: &AppContext, input: repair::RepairContinueInput| repair::continue_session(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "repair_status",
        "Inspect one persisted Cara-owned repair workspace and its exact head/target/conflict/publication evidence without mutation.",
        |context: &AppContext, input: repair::RepairStatusInput| repair::status(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "repair_abort",
        "After explicit confirmation, remove one reviewed local repair workspace/session. Never changes provider refs, branches, labels, or PR state.",
        |context: &AppContext, input: repair::RepairAbortInput| repair::abort(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "evict",
        "After full fleet preflight, evict a PR, remove active/force state, and close its graph gap when compatible. Requires a reason, never bypasses conflicts, and returns resumable exact receipts on partial failure.",
        |context: &AppContext, input: EvictInput| reshape::evict(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "split",
        "After full fleet preflight, split before a selected non-head and enable it as a new head only if both resulting caravans remain compatible. Repair typed evidence before retrying a rejected split.",
        |context: &AppContext, input: SplitInput| reshape::split(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "van_list",
        "List every caravan in deterministic fleet navigation order. Read-only.",
        |context: &AppContext, _input: EmptyInput| navigation::list(context),
    );
    router.add_typed_tool_with_output_schema(
        "van_next",
        "Check out the next caravan head in deterministic PR-number browsing order; refuses dirty/unsafe local Git state, which must be repaired before retry.",
        |context: &AppContext, _input: EmptyInput| {
            navigation::navigate(
                context,
                navigation::Scope::Fleet,
                navigation::Direction::Next,
            )
        },
    );
    router.add_typed_tool_with_output_schema(
        "van_prev",
        "Check out the previous caravan head in deterministic PR-number browsing order; refuses dirty/unsafe local Git state, which must be repaired before retry.",
        |context: &AppContext, _input: EmptyInput| {
            navigation::navigate(
                context,
                navigation::Scope::Fleet,
                navigation::Direction::Previous,
            )
        },
    );
    router.add_typed_tool_with_output_schema(
        "lock_status",
        "Inspect Caravan's repository operation lock, including age, owner token, stale classification, and verified PID liveness. Read-only.",
        |context: &AppContext, input: LockStatusInput| {
            operation_lock::inspect_lock(
                &context.repository_path,
                std::time::Duration::from_secs(input.stale_after_secs),
            )
        },
    );
    router.add_typed_tool_with_output_schema(
        "lock_recover",
        "Remove one verified-stale Caravan operation lock only after explicit confirmation, minimum age, dead-owner proof, and exact token revalidation.",
        |context: &AppContext, input: LockRecoverInput| {
            if !input.confirm {
                return Err(AppError::validation(
                    "operation_lock_recovery_confirmation_required",
                    "set confirm=true only after reviewing lock_status evidence",
                ));
            }
            operation_lock::recover_stale_lock(
                &context.repository_path,
                std::time::Duration::from_secs(input.stale_after_secs),
                &input.token,
            )
        },
    );

    register_self_update_tools(&mut router);
    register_feedback_tools(&mut router);
    router
}

fn register_self_update_tools(router: &mut ToolRouter<AppContext>) {
    router.add_typed_tool_with_output_schema(
        "self_update_status",
        "Report the exact active first-PATH stable user binary and adjacent staged path without network access. Shadowed, development, or unmanaged binaries fail closed.",
        |_context: &AppContext, _input: updatable_cli::EmptyArgs| self_update_status(),
    );
    router.add_typed_tool_with_output_schema(
        "self_update_check",
        "Check the GitHub releases feed for a newer cara version only after validating the exact active first-PATH stable user installation. Network failures are typed and safe to retry.",
        |_context: &AppContext, _input: updatable_cli::EmptyArgs| self_update_check(),
    );
    router.add_typed_tool_with_output_schema(
        "self_update_run",
        "Download, verify, stage, and atomically promote beside the exact active first-PATH stable user binary. Shadowed/development/unmanaged binaries fail closed and partial stages never count as success.",
        |_context: &AppContext, _input: updatable_cli::EmptyArgs| self_update_run(),
    );
}

fn feedback_strategy_name(strategy: &ReportStrategy) -> &'static str {
    match strategy {
        ReportStrategy::Disabled => "disabled",
        ReportStrategy::Stderr => "stderr",
        ReportStrategy::Webhook(_) => "webhook",
        ReportStrategy::CacoCli(_) => "caco_cli",
        ReportStrategy::File(_) => "file",
    }
}

/// Secret-free evidence explaining why configured feedback is unavailable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FeedbackConfigurationDiagnostic {
    pub code: String,
    pub message: String,
    pub next: String,
}

/// Effective feedback state returned by CLI and MCP without startup side effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FeedbackRuntimeStatus {
    pub enabled: bool,
    pub strategy: String,
    pub destination: String,
    pub component: Option<String>,
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration_error: Option<FeedbackConfigurationDiagnostic>,
}

/// Validate the startup-sensitive webhook fields without constructing a reporter
/// whose compatibility fallback writes directly to stderr.
#[must_use]
pub fn feedback_configuration_error(config: &FeedbackConfig) -> Option<FeedbackError> {
    if !config.enabled {
        return None;
    }
    let ReportStrategy::Webhook(webhook) = &config.strategy else {
        return None;
    };
    if webhook.url.trim().is_empty() {
        return Some(FeedbackError::Config(
            "webhook url must not be empty".to_owned(),
        ));
    }
    webhook.resolve_token_for(config.project.as_deref()).err()
}

fn feedback_configuration_diagnostic(error: &FeedbackError) -> FeedbackConfigurationDiagnostic {
    FeedbackConfigurationDiagnostic {
        code: error.code(),
        message: error.message(),
        next:
            "set the configured feedback token environment variable or disable feedback reporting"
                .to_owned(),
    }
}

/// Configure panic feedback for one output mode. Machine commands deliberately
/// install a disabled hook when feedback is invalid so optional startup
/// diagnostics cannot contaminate their stderr contract.
#[must_use]
pub fn feedback_panic_config(json: bool) -> FeedbackConfig {
    let mut config = feedback_config();
    if json && feedback_configuration_error(&config).is_some() {
        config.enabled = false;
    }
    config
}

/// Resolve secret-free effective feedback status without emitting diagnostics.
#[must_use]
pub fn feedback_status() -> FeedbackRuntimeStatus {
    let config = feedback_config();
    let strategy = feedback_strategy_name(&config.strategy).to_owned();
    if let Some(error) = feedback_configuration_error(&config) {
        return FeedbackRuntimeStatus {
            enabled: false,
            strategy,
            destination: "disabled".to_owned(),
            component: config.component,
            project: config.project,
            configuration_error: Some(feedback_configuration_diagnostic(&error)),
        };
    }
    let reporter = Reporter::from_config(&config);
    FeedbackRuntimeStatus {
        enabled: config.enabled,
        strategy,
        destination: reporter.destination(),
        component: config.component,
        project: config.project,
        configuration_error: None,
    }
}

fn register_feedback_tools(router: &mut ToolRouter<AppContext>) {
    router.add_typed_tool_with_output_schema(
        "feedback_report",
        "Report one structured feedback/error/performance event through the configured strategy. Returns a secret-free delivery receipt; retry only after inspecting a typed delivery error.",
        |_context: &AppContext, input: feedback_cli::ReportArgs| {
            let config = feedback_config();
            if let Some(error) = feedback_configuration_error(&config) {
                return Err(error);
            }
            let reporter = Reporter::from_config(&config);
            let destination = reporter.destination();
            reporter.report(&input.into_event())?;
            Ok::<_, feedback_cli::FeedbackError>(feedback_cli::ReportReceipt {
                reported: reporter.is_enabled(),
                destination,
            })
        },
    );
    router.add_typed_tool_with_output_schema(
        "feedback_status",
        "Return the resolved secret-free feedback strategy, destination, component, and project without sending an event.",
        |_context: &AppContext, _input: feedback_cli::EmptyArgs| {
            Ok::<_, feedback_cli::FeedbackError>(feedback_status())
        },
    );
}

/// Self-update configuration for GitHub release assets.
#[must_use]
pub fn updater_config() -> updatable_cli::UpdaterConfig {
    let config =
        updatable_cli::UpdaterConfig::new(TOOL_NAME, env!("CARGO_PKG_VERSION"), UPDATE_REPO_SLUG)
            .with_gh_token_fallback(true);
    match std::env::var("GITHUB_TOKEN") {
        Ok(token) if !token.trim().is_empty() => config.with_github_token(token),
        _ => config,
    }
}

/// Which queue position `cara next` should report (bd-a2407c).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum NextStatus {
    /// The canonical next automatic-admission candidate.
    Ready,
    /// Candidates skipped this generation.
    Skipped,
    /// Candidates proven incompatible with the exact default branch.
    Conflict,
    /// Previously evicted pull requests.
    Evicted,
}

/// Inputs for `cara next` (bd-a2407c).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, clap::Args)]
pub struct NextInput {
    /// Queue positions to report, tried in the order given.
    #[arg(long, value_delimiter = ',', default_value = "ready")]
    #[serde(default)]
    pub status: Vec<NextStatus>,

    /// Also move the working tree to the selected branch.
    ///
    /// The same JSON is still returned. Refuses on a dirty worktree rather than
    /// risk uncommitted work, and the receipt names the exact move.
    #[arg(long)]
    #[serde(default)]
    pub checkout: bool,
}

/// Exact record of a `--checkout` move, so it is auditable and reversible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckoutReceipt {
    pub pr: crate::model::PrNumber,
    pub from_branch: String,
    pub to_branch: String,
}

/// One reported queue position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NextMatch {
    pub status: NextStatus,
    pub pr: crate::model::PrNumber,
    pub branch: String,
    pub oid: crate::model::CommitOid,
    pub url: String,
    pub reason: String,
}

/// Result of `cara next`.
///
/// "Nothing matched" is an ordinary outcome carried in the payload, never an
/// error: a scheduler must be able to tell an empty queue from a provider
/// outage, and an error envelope conflates the two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NextOutput {
    pub schema_version: u32,
    pub repository: crate::model::RepositoryId,
    pub requested: Vec<NextStatus>,
    /// First match across the requested statuses, in the requested order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<NextMatch>,
    /// Every match found, so a caller can plan more than one step.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<NextMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout: Option<CheckoutReceipt>,
    /// Exact next step for a scheduler, including when nothing matched.
    pub next: String,
}

/// Inputs for the CI-admission gate (bd-2a29c8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, clap::Args)]
pub struct CiGateInput {
    /// Exact pull request to evaluate.
    #[arg(long, value_name = "PR")]
    pub pr: u64,

    /// Whether a prior successful required-check run exists for this exact head.
    ///
    /// The gate may only assert that existing evidence still applies; it can
    /// never assert that a head may merge without evidence.
    #[arg(long)]
    #[serde(default)]
    pub head_evidence: bool,
}

/// Typed CI-admission decision consumed by repository workflows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CiGateDecision {
    /// Existing head-SHA evidence still applies exactly; no re-run needed.
    CiValid,
    /// A new head or changed merge content requires CI.
    CiRequired,
    /// Operator explicitly accepted this head's CI state with caravan-force.
    CiForceAccepted,
    /// Not a caravan candidate; Caravan has no opinion.
    CiNotApplicable,
    /// Nothing could be proven, so CI must run.
    CiUnknown,
}

impl CiGateDecision {
    /// Stable code emitted to workflows.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::CiValid => "ci_valid",
            Self::CiRequired => "ci_required",
            Self::CiForceAccepted => "ci_force_accepted",
            Self::CiNotApplicable => "ci_not_applicable",
            Self::CiUnknown => "ci_unknown",
        }
    }

    /// Whether a consuming workflow should run its expensive jobs.
    #[must_use]
    pub const fn runs_ci(self) -> bool {
        matches!(self, Self::CiRequired | Self::CiUnknown)
    }
}

/// Bounded, auditable CI-admission gate result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CiGateOutput {
    pub schema_version: u32,
    pub decision: CiGateDecision,
    pub decision_code: String,
    pub run_ci: bool,
    pub pr: crate::model::PrNumber,
    pub reason: String,
    /// Exact facts the decision rests on, so a skipped run is auditable.
    pub evidence: serde_json::Value,
}

/// Optional explicit directory for one stable PATH-visible Cara installation.
pub const SELF_UPDATE_INSTALL_DIR_ENV: &str = "CARA_SELF_UPDATE_INSTALL_DIR";

/// Resolve an updater bound to the exact running, PATH-visible user installation.
pub fn active_updater_config() -> Result<updatable_cli::UpdaterConfig, AppError> {
    let current = std::env::current_exe().map_err(|error| {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "self_update_current_executable_unavailable",
            format!("could not resolve the running Cara executable: {error}"),
            None,
        )
    })?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let explicit = std::env::var_os(SELF_UPDATE_INSTALL_DIR_ENV).map(PathBuf::from);
    let install_dir = resolve_self_update_install_dir(
        &current,
        std::env::var_os("PATH").as_deref(),
        home.as_deref(),
        explicit.as_deref(),
    )?;
    let mut config = updater_config();
    config.install_dir = Some(install_dir);
    Ok(config)
}

fn resolve_self_update_install_dir(
    current: &Path,
    path: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
    explicit: Option<&Path>,
) -> Result<PathBuf, AppError> {
    let current = current.canonicalize().map_err(|error| {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "self_update_current_executable_unavailable",
            format!("could not canonicalize the running Cara executable: {error}"),
            Some(serde_json::json!({"current_executable": current})),
        )
    })?;
    let executable_name = current.file_name().ok_or_else(|| {
        AppError::validation(
            "self_update_current_executable_invalid",
            "the running Cara executable has no file name",
        )
    })?;
    let expected_name = std::ffi::OsStr::new(if cfg!(windows) { "cara.exe" } else { TOOL_NAME });
    if executable_name != expected_name {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "self_update_development_binary",
            "self-update is refused from a test or renamed development executable",
            Some(serde_json::json!({
                "current_executable": current,
                "safe_next_action": "run self-update from the installed PATH-visible cara binary"
            })),
        ));
    }
    let install_dir = current
        .parent()
        .expect("executable has parent")
        .to_path_buf();
    if is_cargo_development_binary(&current) {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "self_update_development_binary",
            "self-update will not overwrite a Cargo target/debug or target/release binary",
            Some(serde_json::json!({
                "current_executable": current,
                "safe_next_action": "install a release into ~/.cargo/bin or ~/.local/bin, then rerun it"
            })),
        ));
    }
    // bd-0308f3/bd-bd58cd: a package-managed binary is a reason to install
    // ELSEWHERE, not a reason to refuse. The store is immutable and a store path
    // whose contents no longer match its hash is a corrupted closure, so the
    // release is installed into a user-owned directory that PATH already
    // prefers, leaving the managed binary untouched underneath.
    if is_package_manager_binary(&current) {
        return package_managed_install_dir(&current, path, home, explicit);
    }

    if let Some(explicit) = explicit {
        if !explicit.is_absolute() || !same_directory(&install_dir, explicit) {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "self_update_install_dir_mismatch",
                format!(
                    "{SELF_UPDATE_INSTALL_DIR_ENV} must be the absolute parent of the running Cara executable"
                ),
                Some(serde_json::json!({
                    "current_executable": current,
                    "configured_install_dir": explicit,
                })),
            ));
        }
        return Ok(install_dir);
    }

    let supported = home.is_some_and(|home| {
        same_directory(&install_dir, &home.join(".cargo/bin"))
            || same_directory(&install_dir, &home.join(".local/bin"))
    });
    if !supported {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "self_update_install_unmanaged",
            "the running Cara executable is not in a supported user-managed install directory",
            Some(serde_json::json!({
                "current_executable": current,
                "safe_next_action": format!(
                    "set {SELF_UPDATE_INSTALL_DIR_ENV} to this absolute directory only if it is intentionally user-managed"
                )
            })),
        ));
    }
    Ok(install_dir)
}

/// Choose a user-owned install directory that shadows a package-managed Cara.
///
/// bd-bd58cd: forced self-update stays useful for deliberately pulling a GitHub
/// release, but it must never write into a Nix store or Homebrew cellar, and it
/// must refuse rather than silently install a binary PATH would never resolve.
fn package_managed_install_dir(
    current: &Path,
    path: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
    explicit: Option<&Path>,
) -> Result<PathBuf, AppError> {
    let install_dir = explicit.map(Path::to_path_buf).or_else(|| home.map(|home| home.join(".local/bin")))
        .ok_or_else(|| {
            AppError::structured(
                ErrorCategory::Validation,
                "self_update_install_dir_unresolved",
                "no user-owned install directory could be resolved for a package-managed Cara",
                Some(serde_json::json!({
                    "current_executable": current,
                    "safe_next_action": format!("set {SELF_UPDATE_INSTALL_DIR_ENV} to an absolute directory that precedes the managed binary on PATH"),
                })),
            )
        })?;
    if is_package_manager_binary(&install_dir) {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "self_update_install_dir_managed",
            "self-update will never write into a Nix store or Homebrew cellar",
            Some(serde_json::json!({
                "current_executable": current,
                "configured_install_dir": install_dir,
            })),
        ));
    }
    if !precedes_on_path(&install_dir, current, path) {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "self_update_install_dir_not_preferred",
            "the user-owned install directory does not precede the package-managed Cara on PATH, so the update would never be executed",
            Some(serde_json::json!({
                "current_executable": current,
                "configured_install_dir": install_dir,
                "safe_next_action": "put the install directory earlier on PATH, or upgrade through the package manager that owns the current binary",
            })),
        ));
    }
    Ok(install_dir.canonicalize().unwrap_or(install_dir))
}

/// Whether `candidate` appears before the directory holding `current` on PATH.
fn precedes_on_path(candidate: &Path, current: &Path, path: Option<&std::ffi::OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };
    let canonical = |value: &Path| value.canonicalize().unwrap_or_else(|_| value.to_path_buf());
    let candidate = canonical(candidate);
    let current_dir = current.parent().map(canonical);
    for directory in std::env::split_paths(path) {
        let directory = canonical(&directory);
        if directory == candidate {
            return true;
        }
        if current_dir
            .as_ref()
            .is_some_and(|parent| directory == *parent)
        {
            return false;
        }
    }
    false
}

fn same_directory(actual: &Path, expected: &Path) -> bool {
    expected
        .canonicalize()
        .map_or_else(|_| actual == expected, |expected| actual == expected)
}

fn is_cargo_development_binary(path: &Path) -> bool {
    path.ancestors().skip(1).any(|profile| {
        matches!(
            profile.file_name().and_then(std::ffi::OsStr::to_str),
            Some("debug" | "release")
        ) && profile.ancestors().skip(1).any(|ancestor| {
            ancestor.file_name().and_then(std::ffi::OsStr::to_str) == Some("target")
        })
    })
}

fn is_package_manager_binary(path: &Path) -> bool {
    path.starts_with("/nix/store")
        || path.starts_with("/opt/homebrew/Cellar")
        || path.starts_with("/usr/local/Cellar")
        || path.starts_with("/home/linuxbrew/.linuxbrew/Cellar")
}

fn update_error(error: &impl StructuredError) -> AppError {
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        error.details(),
    )
}

/// Return active-install self-update status.
pub fn self_update_status() -> Result<updatable_cli::UpdateStatus, AppError> {
    updatable_cli::Updater::new(active_updater_config()?)
        .current_status()
        .map_err(updatable_cli::UpdateError::from)
        .map_err(|error| update_error(&error))
}

/// Check releases using the active-install contract.
pub fn self_update_check() -> Result<updatable_cli::LatestReleaseInfo, AppError> {
    updatable_cli::Updater::new(active_updater_config()?)
        .check_latest()
        .map_err(updatable_cli::UpdateError::from)
        .map_err(|error| update_error(&error))
}

/// Update the exact active PATH-visible stable user installation.
pub fn self_update_run() -> Result<updatable_cli::UpdateOutcome, AppError> {
    updatable_cli::Updater::new(active_updater_config()?)
        .run_update()
        .map_err(updatable_cli::UpdateError::from)
        .map_err(|error| update_error(&error))
}

/// Feedback configuration using the shared ecosystem environment convention.
#[must_use]
pub fn feedback_config() -> FeedbackConfig {
    let mut config = FeedbackConfig::from_env();
    config.component.get_or_insert_with(|| TOOL_NAME.to_owned());
    config.project.get_or_insert_with(|| {
        std::env::var("CACOPHONY_PROJECT").unwrap_or_else(|_| "caravan".to_owned())
    });
    config
}

/// Secret-free feedback status used by the CLI.
#[must_use]
pub fn feedback_destination() -> String {
    Reporter::from_config(&feedback_config()).destination()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_test_executable(path: &Path) {
        std::fs::write(path, "installed").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn git(directory: &Path, arguments: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(directory)
            .args(arguments)
            .status()
            .unwrap();
        assert!(status.success(), "git {arguments:?}");
    }

    #[test]
    fn nested_context_resolves_one_repository_root_and_default_config() {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init"]);
        let nested = directory.path().join("a directory/inside");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(directory.path().join(".caravan")).unwrap();
        std::fs::write(
            directory.path().join(config::DEFAULT_CONFIG_PATH),
            "version: 1\n",
        )
        .unwrap();

        let context = AppContext::load_from_directory(&nested, None).unwrap();
        assert_eq!(
            context.repository_path,
            directory.path().canonicalize().unwrap()
        );
        assert_eq!(
            context.config_path,
            PathBuf::from(config::DEFAULT_CONFIG_PATH)
        );
        assert!(context.config_existed);
        assert!(!nested.join(".caravan").exists());
    }

    #[test]
    fn relative_explicit_config_remains_relative_to_invocation_directory() {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init"]);
        let nested = directory.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("cara.yaml"), "version: 1\n").unwrap();

        let context =
            AppContext::load_from_directory(&nested, Some(Path::new("cara.yaml"))).unwrap();
        assert_eq!(context.config_path, nested.join("cara.yaml"));
        assert_eq!(
            context.repository_path,
            directory.path().canonicalize().unwrap()
        );
    }

    /// A probe that did not FINISH is not a repository that does not EXIST.
    ///
    /// The two need opposite responses: "not found" is terminal and sends a
    /// reader to check paths, symlinks and permissions; a timed-out
    /// `git rev-parse --show-toplevel` means the checkout is probably fine and
    /// the filesystem was slow. Live: the 5s deadline expired twice under load
    /// on a valid repository whose root resolved instantly by hand (bd-f42a5e).
    #[test]
    fn a_timed_out_repository_probe_is_not_a_missing_repository() {
        let timeout = ConfigError::RepositoryProbeTimeout {
            path: std::path::PathBuf::from("/repo"),
            message: "`git rev-parse --show-toplevel` exceeded its 5000ms deadline".to_owned(),
        };

        assert_eq!(
            mcp_cli::StructuredError::code(&timeout),
            "repository_probe_timeout",
            "a slow probe must not be reported as a missing worktree"
        );
        let details = mcp_cli::StructuredError::details(&timeout).expect("details");
        assert_eq!(
            details["retryable"], true,
            "the remedy is to retry, not to go looking for the repository"
        );
        assert_eq!(details["mutated"], false);
    }

    #[test]
    fn outside_git_is_typed_and_writes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let error = AppContext::load_from_directory(directory.path(), None).unwrap_err();
        assert_eq!(error.code(), "repository_not_found");
        assert!(!directory.path().join(".caravan").exists());
    }

    #[test]
    fn help_describes_the_resumable_sync_loop() {
        let output = help();
        assert!(output.instructions.contains("typed decision"));
        assert!(
            output
                .instructions
                .contains("rerun the same idempotent sync")
        );
        assert!(output.instructions.contains("A, AB, ABC, ABCD, ABCDE"));
        assert!(output.instructions.contains("rebase_on_join: true"));
        assert!(output.instructions.contains("global barrier"));
        assert!(output.instructions.contains("cara join --pr N --tail-pr T"));
        assert!(output.instructions.contains("cara repair start"));
        assert!(output.instructions.contains("caravan-force"));
        assert!(output.instructions.contains("force-intent"));
        assert!(output.instructions.contains("--membership-generation"));
        assert!(
            output
                .instructions
                .contains("no external specification is required")
        );
        assert!(!output.instructions.contains("See SPEC.md"));
        assert_eq!(output.spec, "embedded");
    }

    #[test]
    fn target_forms_are_mutually_exclusive_even_over_mcp() {
        let error =
            validate_target(Some(10), Some(20)).expect_err("both target forms must be rejected");
        assert_eq!(error.code(), "ambiguous_target");
    }

    #[test]
    fn router_exposes_domain_and_ecosystem_tools() {
        let names: Vec<String> = build_router()
            .tool_metadata()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert!(
            !names.iter().any(|name| name == "loop"),
            "the unbounded foreground loop must not be exposed over MCP"
        );
        for expected in [
            "help",
            "init",
            "log",
            "status",
            "queue",
            "check",
            "new",
            "renew",
            "join",
            "rejoin",
            "show",
            "force_arm",
            "force_revoke",
            "force_intent_preview",
            "force_intent_apply",
            "force_intent_revoke",
            "next",
            "prev",
            "plan_sync",
            "sync",
            "evict",
            "split",
            "van_list",
            "van_next",
            "van_prev",
            "lock_status",
            "lock_recover",
            "self_update_status",
            "self_update_check",
            "self_update_run",
            "feedback_report",
            "feedback_status",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing {expected}"
            );
        }
    }

    #[test]
    fn plan_tool_schema_exposes_no_write_receipt() {
        let tools =
            serde_json::to_value(build_router().tool_metadata()).expect("tool metadata serializes");
        let plan = tools
            .as_array()
            .expect("metadata array")
            .iter()
            .find(|tool| tool["name"] == "plan_sync")
            .expect("plan_sync tool");
        let encoded = serde_json::to_string(plan).expect("plan metadata serializes");
        assert!(encoded.contains("mutated"));
        assert!(encoded.contains("provider_writes"));
        assert!(encoded.contains("auto_admission"));
        assert!(encoded.contains("plan_hash"));
    }

    #[test]
    fn reviewed_force_tool_schemas_match_caco_evidence_contract() {
        let tools =
            serde_json::to_value(build_router().tool_metadata()).expect("tool metadata serializes");
        for name in [
            "force_intent_preview",
            "force_intent_apply",
            "force_intent_revoke",
        ] {
            let tool = tools
                .as_array()
                .expect("metadata array")
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap_or_else(|| panic!("missing {name}"));
            let encoded = serde_json::to_string(tool).expect("force metadata serializes");
            for field in [
                "head",
                "provider_head",
                "membership_generation",
                "membership",
                "failure_fingerprint",
                "required_checks",
                "current_decision",
                "expires_at_ms",
                "force_intent_applied",
                "squash_auto_merge_enabled",
                "atomic_provider_transaction",
            ] {
                assert!(encoded.contains(field), "{name} schema omitted {field}");
            }
        }
    }

    #[test]
    fn check_tool_schema_has_remote_input_and_exact_receipt_output() {
        let tools =
            serde_json::to_value(build_router().tool_metadata()).expect("tool metadata serializes");
        let check = tools
            .as_array()
            .expect("metadata array")
            .iter()
            .find(|tool| tool["name"] == "check")
            .expect("check tool");
        let encoded = serde_json::to_string(check).expect("check metadata serializes");
        assert!(encoded.contains("\"pr\""));
        assert!(encoded.contains("merge_candidate"));
        assert!(encoded.contains("next_action"));
    }

    #[test]
    fn help_tool_returns_a_success_envelope() {
        let envelope =
            build_router().call_tool(&AppContext::default(), "help", serde_json::json!({}));
        let value = serde_json::to_value(envelope).expect("envelope serializes");
        assert_eq!(value["status"], "success");
        assert!(
            value["data"]["instructions"]
                .as_str()
                .expect("instructions")
                .contains("Caravan")
        );
    }

    #[test]
    fn self_update_targets_exact_first_path_visible_user_install() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        let cargo_bin = home.join(".cargo/bin");
        let local_bin = home.join(".local/bin");
        std::fs::create_dir_all(&cargo_bin).unwrap();
        std::fs::create_dir_all(&local_bin).unwrap();
        let active = cargo_bin.join(if cfg!(windows) { "cara.exe" } else { "cara" });
        write_test_executable(&active);
        let path = std::env::join_paths([&cargo_bin, &local_bin]).unwrap();
        assert_eq!(
            resolve_self_update_install_dir(&active, Some(&path), Some(home), None).unwrap(),
            cargo_bin.canonicalize().unwrap()
        );
    }

    /// bd-0308f3: a shadowed-but-user-managed binary is no longer refused. The
    /// flake pin is the upgrade path of record, so self-update only has to
    /// avoid writing somewhere the result would never be executed.
    #[test]
    fn self_update_allows_a_shadowed_user_install() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        let cargo_bin = home.join(".cargo/bin");
        let local_bin = home.join(".local/bin");
        std::fs::create_dir_all(&cargo_bin).unwrap();
        std::fs::create_dir_all(&local_bin).unwrap();
        let name = if cfg!(windows) { "cara.exe" } else { "cara" };
        let first = cargo_bin.join(name);
        let shadowed = local_bin.join(name);
        write_test_executable(&first);
        write_test_executable(&shadowed);
        let path = std::env::join_paths([&cargo_bin, &local_bin]).unwrap();

        assert_eq!(
            resolve_self_update_install_dir(&shadowed, Some(&path), Some(home), None).unwrap(),
            local_bin.canonicalize().unwrap()
        );
    }

    /// bd-bd58cd: a Nix-managed binary redirects into a user-owned directory
    /// that PATH already prefers, instead of refusing outright.
    #[test]
    fn self_update_redirects_a_package_managed_binary() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        let local_bin = home.join(".local/bin");
        let store_bin = home.join("nix/store/hash-cara/bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        std::fs::create_dir_all(&store_bin).unwrap();
        let name = if cfg!(windows) { "cara.exe" } else { "cara" };
        let managed = store_bin.join(name);
        write_test_executable(&managed);

        // The user directory precedes the managed binary, so the install lands
        // somewhere PATH will actually resolve.
        let preferred = std::env::join_paths([&local_bin, &store_bin]).unwrap();
        assert_eq!(
            package_managed_install_dir(&managed, Some(&preferred), Some(home), None).unwrap(),
            local_bin.canonicalize().unwrap()
        );

        // Reversed, the install would never be executed, so it fails closed.
        let ignored = std::env::join_paths([&store_bin, &local_bin]).unwrap();
        assert_eq!(
            package_managed_install_dir(&managed, Some(&ignored), Some(home), None)
                .unwrap_err()
                .code(),
            "self_update_install_dir_not_preferred"
        );
    }

    #[test]
    fn self_update_rejects_development_binaries() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        let name = if cfg!(windows) { "cara.exe" } else { "cara" };

        let debug = home.join("project/target/aarch64-unknown-linux-gnu/debug");
        std::fs::create_dir_all(&debug).unwrap();
        let development = debug.join(name);
        write_test_executable(&development);
        let path = std::env::join_paths([&debug]).unwrap();
        assert_eq!(
            resolve_self_update_install_dir(&development, Some(&path), Some(home), None)
                .unwrap_err()
                .code(),
            "self_update_development_binary"
        );
        assert!(is_package_manager_binary(Path::new(
            "/nix/store/hash-cara/bin/cara"
        )));
        assert!(is_package_manager_binary(Path::new(
            "/opt/homebrew/Cellar/cara/0.0.5/bin/cara"
        )));
    }

    #[test]
    fn explicit_self_update_dir_must_match_active_path_visible_parent() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        let managed = home.join("custom/bin");
        std::fs::create_dir_all(&managed).unwrap();
        let active = managed.join(if cfg!(windows) { "cara.exe" } else { "cara" });
        write_test_executable(&active);
        let path = std::env::join_paths([&managed]).unwrap();
        assert_eq!(
            resolve_self_update_install_dir(&active, Some(&path), Some(home), Some(&managed))
                .unwrap(),
            managed.canonicalize().unwrap()
        );
        assert_eq!(
            resolve_self_update_install_dir(
                &active,
                Some(&path),
                Some(home),
                Some(&home.join("other")),
            )
            .unwrap_err()
            .code(),
            "self_update_install_dir_mismatch"
        );
    }

    #[test]
    fn updater_targets_caravan_release_assets() {
        let config = updater_config();
        assert_eq!(config.tool_name, "cara");
        assert_eq!(config.repo_slug, "harryaskham/caravan");
    }
}
