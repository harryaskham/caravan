//! Shared typed command contracts for the `cara` CLI and MCP server.
//!
//! Every bounded v1 domain tool is backed by the same GitHub-facing operation
//! used by the human and JSON CLI surfaces.

use std::path::PathBuf;

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
pub mod reshape;
pub mod sync;

use clap::Args;
use feedback_cli::{FeedbackConfig, FeedbackError, ReportStrategy, Reporter};
use mcp_cli::{ErrorCategory, StructuredError, ToolRouter};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{CaravanConfig, ConfigError};

pub mod config;
pub mod model;

/// GitHub release repository used by `updatable-cli`.
pub const UPDATE_REPO_SLUG: &str = "harryaskham/caravan";
/// Installed binary name.
pub const TOOL_NAME: &str = "cara";
/// Normative command contract.
pub const SPEC_PATH: &str = "SPEC.md";

/// Agent-facing operating instructions returned by `cara help` and the MCP tool.
pub const AGENT_HELP: &str = r"Caravan is an agent-in-the-loop GitHub merge queue.

Safe operating loop:
1. Run `cara status`; if repository initialization is not ready, run the explicit
   idempotent `cara init` command and reconcile any reported metadata mismatch.
2. Inspect the canonical automatic-admission order (explicit agent priority,
   then immutable PR createdAt) in status or with `cara next-candidate`; never
   re-sort or leapfrog its first ordered attempt.
3. Use `cara check --pr N` to preflight the exact remote candidate (optionally against a tail) without checkout or provider mutation, then follow its `new`, `join`, `repair`, `wait`, or `reject` next action.
4. Run `cara sync` (or `sync --all`) until it either converges or returns one
   typed decision point.
5. At a CI decision, optionally use `cara sync --rerun-failed` to rerun only
   exact failed workflow runs. Otherwise repair/push, evict, split, renew, or
   rejoin; then rerun the same sync.
6. For an incident/maintenance hold, run explicit `cara pause` with actor and
   reason. Expiry never resumes it. After recovery, only an audited `cara resume`
   may revalidate exact facts and restore squash auto-merge.

Caravan does not routinely rebase branches. A link means the child PR targets
its predecessor branch and is mechanically merge-compatible with it. Only a
caravan head targets the default branch and has squash auto-merge enabled.
Never hide an unresolved structured error: its evidence and suggested actions
are the continuation contract for the user or agent. See SPEC.md for invariants.";

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
    /// Resolve and validate repository configuration once for an MCP session.
    pub fn load(path: Option<&std::path::Path>) -> Result<Self, ConfigError> {
        let loaded = CaravanConfig::load_or_default(path)?;
        Ok(Self {
            repository_path: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            config_path: loaded.path,
            config_existed: loaded.existed,
            config: loaded.config,
        })
    }
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
    /// Specification path.
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
        "Idempotently synchronize one or all caravans under optimistic preconditions. Intentionally paused caravans return stable no-op receipts and are skipped by sync-all.",
        |context: &AppContext, input: SyncInput| sync::sync(context, &input),
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

    #[test]
    fn help_describes_the_resumable_sync_loop() {
        let output = help();
        assert!(output.instructions.contains("decision point"));
        assert!(output.instructions.contains("rerun the same sync"));
        assert_eq!(output.spec, "SPEC.md");
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
        assert!(value["data"]["instructions"]
            .as_str()
            .expect("instructions")
            .contains("Caravan"));
    }

    #[test]
    fn updater_targets_caravan_release_assets() {
        let config = updater_config();
        assert_eq!(config.tool_name, "cara");
        assert_eq!(config.repo_slug, "harryaskham/caravan");
    }
}
