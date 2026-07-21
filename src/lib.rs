//! Shared typed command contracts for the `cara` CLI and MCP server.
//!
//! Every bounded v1 domain tool is backed by the same GitHub-facing operation
//! used by the human and JSON CLI surfaces.

use std::path::{Path, PathBuf};
use std::time::Duration;

pub mod ci;
pub mod command;
pub mod compatibility;
pub mod github;
pub mod graph;
pub mod hooks;
pub mod initialization;
pub mod journal;
pub mod loop_runner;
pub mod membership;
pub mod navigation;
pub mod operation_lock;
pub mod pause;
pub mod physical_rebase;
pub mod read;
pub mod repair;
pub mod reshape;
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
- The head targets the default branch. Each child targets its predecessor's
  branch. Only the head may have squash auto-merge enabled; non-head auto-merge
  is disabled even if someone enabled it externally.
- Multiple caravans may exist. Cross-caravan compatibility and the configured
  admission bound are checked before automatic changes.
- Automatic admission is deterministic: configured `caravan-priority:*` labels
  from highest to lowest, then immutable GitHub `createdAt`, then PR number.
  Never re-sort, skip, or leapfrog the first canonical attempt because it failed,
  except under the explicit sync-owned generation-bound greedy policy below.

FIRST USE AND EVERY SAFE TICK
Cara resolves one exact Git worktree root before config or mutation, so nested
invocations share root config/state; outside a non-bare worktree writes nothing.
1. Run `cara status` (prefer `--json` for automation). It reports initialization,
   effective `rebase_on_join` mode, current/merged branch context, every caravan,
   canonical admission order, pauses, exact base/head/candidate lineage, CI
   generation freshness, compatibility, default-branch movement, and problems.
2. If initialization is not ready, run idempotent `cara init`. Do not hand-create
   labels or silently overwrite mismatched metadata. Repair the exact reported
   repository setting, protection, label, permission, or config mismatch.
3. Inspect `cara next-candidate`. Use `cara check --pr N` to validate the exact
   remote PR without checkout or mutation. Add `--tail-pr T` (or `--head-pr H`)
   when proposing a join. Follow the typed `new`, `join`, `repair`, `wait`, or
   `reject` action from the returned receipt.
4. Use `cara new`, `renew`, `join`, or `rejoin` only after that preflight.
   With `rebase_on_join: true`, post-rewrite rediscovery is operation-specific:
   join/rejoin require the exact live tail, while new/renew require the exact
   current default generation and deliberately no join tail. Candidate-head,
   default, tail, or unexpected-membership drift stops before membership writes.
   For checkout-free orchestration, `cara join --pr N --tail-pr T` holds the
   repository operation lock, re-reads the canonical live tail, rebases the
   exact candidate onto that tail, re-reads provider facts, refuses admission
   if the tail moved, sets the provider base,
   and returns a versioned exact join receipt. Routine join accepts no force
   option and proves any stale-generation `caravan-force` intent was absent or
   removed. Membership operations are optimistic and resumable; rerun the same
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
   candidate/mutation/GitHub/wall bounds, joins, skips, remaining work, and continuation.
7. Preserve the complete structured error. Its category, code, exact OIDs,
   affected PRs, completed steps, provider receipts, suggested actions, and
   resumable command are the continuation contract. Never replace it with a
   generic summary or proceed around it manually.

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
5. disables auto-merge on all selected members only after that global barrier;
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
uses non-force publication, requires fresh CI, and resumes sync-all. Use `repair abort` only to
remove a reviewed local session. Never create nested raw worktrees, call
`update-ref`, merge behind Cara, or force-push a repair branch.

RESHAPING AND EXPLICIT INTENT
- `cara evict --pr N --reason ...` removes a member and reconnects its child only
  after exact compatibility proof. `split` makes the selected PR a new head.
  `renew` and `rejoin` re-evaluate an evicted PR from fresh facts.
- `caravan-force` is explicit intent to accept non-successful CI, not a general
  safety bypass. It requires `force_merge: true`, an open non-draft labelled
  head, exact current default/head compatibility, ADMIN permission, a durable
  audit comment, and a one-shot squash. It never bypasses textual conflicts,
  stale facts, ownership, holds, permissions, or lease checks. If CI is already
  fully successful, normal auto-merge policy applies instead.
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
- Use `--read-only` to disable mutation endpoints. Interactive actions use the
  same typed domain functions, preconditions, operation locks, decisions, and
  receipts as CLI/JSON/MCP; the web server never shells out to human output.
- Static HTML, CSS, and JavaScript ship inside `cara`; no CDN or separate web
  deployment is required. Future opt-in repository discovery must remain
  explicit and is not performed by the initial path-scoped release.

RECOVERY, LOCKS, AND OBSERVABILITY
- GitHub is the resume cursor. After timeout, interruption, or partial provider
  failure, inspect receipts and rerun the same command. Do not guess whether a
  mutation happened. Only genuine provider-generation races are `retry_tick`.
  Deterministic unsupported physical histories such as nonlinear, ambiguous, or
  empty candidate ranges are non-retryable external decisions: use their stable
  fingerprint and exact repair/reshape/evict/strategy choices rather than
  repeating the unchanged tick.
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
  work; blocking hooks return typed partial receipts.
- `cara show`, `next`, `prev`, and `van list|next|prev` are navigation surfaces;
  they never authorize skipping admission or mutation preflight.
- `--json` and MCP return the same typed envelopes and schemas. Unknown provider
  values are preserved rather than guessed into success.
- GitHub credentials are selected statelessly per repository. Cara parses the
  origin host/owner, first accepts an ambient `GH_TOKEN`/`GITHUB_TOKEN` only if
  it can read that exact repository, then tries the stored token for the owner
  login, and finally probes other successful `gh` accounts. Selection is cached
  only for the process; Cara never runs `gh auth switch`, stores a preferred
  account in project config, or exposes a token in commands/errors/receipts.
- `cara self-update status|check|run` is the supported installed-binary update
  path. A tagged patch release is required before release-only deployments can
  observe newly landed main-branch behavior.

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

    fn load_from_directory(
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
    let output = runner
        .run(&request)
        .map_err(|error| ConfigError::RepositoryNotFound {
            path: directory.to_path_buf(),
            message: error.to_string(),
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
    /// Create the current branch's PR non-interactively with `gh pr create --fill`.
    #[arg(long)]
    #[serde(default)]
    pub create_pr: bool,

    /// Human/agent admission rationale; otherwise a deterministic mechanical reason is used.
    #[arg(long, value_name = "TEXT")]
    #[serde(default)]
    pub reason: Option<String>,

    /// Exact configured agent-priority label. Without it, admission is FIFO.
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
        "next_candidate",
        "Return the canonical first priority-then-FIFO admission attempt without mutation. Selection is not compatibility proof: run check/new preflight, and fail closed rather than leapfrogging on rejection.",
        |context: &AppContext, _input: EmptyInput| read::next_candidate(context),
    );
    router.add_typed_tool_with_output_schema(
        "check",
        "Preflight an exact remote candidate with --pr, or the current PR when omitted, without checkout or provider mutation. Optionally test joining --tail-pr or the resolved tail of --head-pr; returns exact facts and a mechanical next action.",
        |context: &AppContext, input: CheckInput| read::check(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "new",
        "After complete repository/graph/compatibility preflight, label and retarget the current open PR as a one-PR caravan and enable squash auto-merge. Exact stale facts abort; rediscover and rerun to resume partial receipts.",
        |context: &AppContext, input: CreateInput| membership::new(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "renew",
        "After complete preflight, reevaluate an evicted current PR as a new caravan; remove eviction/force labels only when safe and enable head auto-merge. On typed failure repair the evidence and rerun.",
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
        "show",
        "Show the current branch's complete caravan and highlighted position. Read-only.",
        |context: &AppContext, _input: EmptyInput| read::show(context),
    );
    router.add_typed_tool_with_output_schema(
        "next",
        "Check out the next PR toward the current caravan tail. Local worktrees must be clean and unambiguous; clean or finish Git state before retrying an unsafe_checkout error.",
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
        "Check out the previous PR toward the current caravan head. Local worktrees must be clean and unambiguous; clean or finish Git state before retrying an unsafe_checkout error.",
        |context: &AppContext, _input: EmptyInput| {
            navigation::navigate(
                context,
                navigation::Scope::Caravan,
                navigation::Direction::Previous,
            )
        }
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
        "Report installed and staged binary paths without network access. Read-only; repair a reported staged-path problem before running an update.",
        |_context: &AppContext, _input: updatable_cli::EmptyArgs| {
            updatable_cli::Updater::new(updater_config())
                .current_status()
                .map_err(updatable_cli::UpdateError::from)
        },
    );
    router.add_typed_tool_with_output_schema(
        "self_update_check",
        "Check the GitHub releases feed for a newer cara version without installing it. Network failures are typed and safe to retry.",
        |_context: &AppContext, _input: updatable_cli::EmptyArgs| {
            updatable_cli::Updater::new(updater_config())
                .check_latest()
                .map_err(updatable_cli::UpdateError::from)
        },
    );
    router.add_typed_tool_with_output_schema(
        "self_update_run",
        "Download, verify, stage, and atomically promote the latest cara release. On failure inspect the typed updater error before retrying; never treats a partial stage as success.",
        |_context: &AppContext, _input: updatable_cli::EmptyArgs| {
            updatable_cli::Updater::new(updater_config())
                .run_update()
                .map_err(updatable_cli::UpdateError::from)
        },
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
            "next_candidate",
            "check",
            "new",
            "renew",
            "join",
            "rejoin",
            "show",
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
    fn updater_targets_caravan_release_assets() {
        let config = updater_config();
        assert_eq!(config.tool_name, "cara");
        assert_eq!(config.repo_slug, "harryaskham/caravan");
    }
}
