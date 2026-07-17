//! Shared typed command contracts for the `cara` CLI and MCP server.
//!
//! This foundation intentionally exposes the complete command shape while queue
//! operations return structured `not_implemented` errors. Follow-up beads replace
//! each stub with the GitHub-backed behavior specified in `SPEC.md`.

use std::path::PathBuf;

pub mod command;
pub mod compatibility;
pub mod github;
pub mod graph;
pub mod membership;
pub mod navigation;
pub mod operation_lock;
pub mod read;
pub mod reshape;

use clap::Args;
use feedback_cli::{FeedbackConfig, Reporter};
use mcp_cli::{ErrorCategory, StructuredError, ToolRouter};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
1. Run `cara status` to discover caravans and decision points.
2. Use `cara check`, `new`, or `join` to validate a proposed queue change.
3. Run `cara sync` (or `sync --all`) until it either converges or returns one
   typed decision point.
4. At a decision point, repair and push the affected PR, or use `cara evict`,
   `split`, `renew`, or `rejoin`; then rerun the same sync.

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

    /// Mark a specified operation as not implemented by the foundation slice.
    #[must_use]
    pub fn not_implemented(operation: &str) -> Self {
        Self {
            category: ErrorCategory::UnsupportedCapability,
            code: "not_implemented".to_owned(),
            message: format!(
                "`cara {operation}` is specified but not implemented in the initial skeleton"
            ),
            details: Some(json!({
                "operation": operation,
                "spec": SPEC_PATH,
                "resumable": false,
                "next": "implement the corresponding domain bead; do not treat this as queue success"
            })),
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
}

/// Input for `cara sync`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, Args)]
pub struct SyncInput {
    /// Synchronize every caravan rather than only the current branch's caravan.
    #[arg(long)]
    #[serde(default)]
    pub all: bool,
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

/// Placeholder success shape for domain operations.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OperationOutput {
    /// Stable operation name.
    pub operation: String,
    /// False for the initial scaffold; real implementations return true.
    pub implemented: bool,
    /// Agent-facing outcome.
    pub message: String,
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

/// Honest placeholder for a specified domain operation.
pub fn scaffold_operation<T>(operation: &str, _input: &T) -> Result<OperationOutput, AppError> {
    Err(AppError::not_implemented(operation))
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
        "status",
        "Discover the current repository, current PR, every caravan, invalid graph fragments, and unresolved decision points. Read-only.",
        |context: &AppContext, _input: EmptyInput| read::status(context),
    );
    router.add_typed_tool_with_output_schema(
        "check",
        "Validate the current PR/caravan without mutation. Optionally test joining --tail-pr or the resolved tail of --head-pr.",
        |context: &AppContext, input: CheckInput| read::check(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "new",
        "Create a one-PR caravan from the current branch after full graph and cross-caravan compatibility checks.",
        |context: &AppContext, input: CreateInput| membership::new(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "renew",
        "Reevaluate an evicted current PR as a new caravan, removing caravan-evicted only after preflight succeeds.",
        |context: &AppContext, input: CreateInput| membership::renew(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "join",
        "Append the current PR after a caravan tail. On ambiguity, supply tail_pr or head_pr; returns typed decision errors without guessing.",
        |context: &AppContext, input: JoinInput| membership::join(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "rejoin",
        "Reevaluate an evicted PR and append it after a valid tail; removes caravan-evicted only after preflight succeeds.",
        |context: &AppContext, input: JoinInput| membership::rejoin(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "show",
        "Show the current branch's complete caravan and highlighted position. Read-only.",
        |context: &AppContext, _input: EmptyInput| read::show(context),
    );
    router.add_typed_tool_with_output_schema(
        "next",
        "Safely check out the next PR toward the current caravan tail; refuses dirty or ambiguous repositories.",
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
        "Safely check out the previous PR toward the current caravan head; refuses dirty or ambiguous repositories.",
        |context: &AppContext, _input: EmptyInput| {
            navigation::navigate(
                context,
                navigation::Scope::Caravan,
                navigation::Direction::Previous,
            )
        }
    );
    router.add_typed_tool_with_output_schema(
        "sync",
        "Idempotently synchronize one or all caravans. Stops at the first agent decision point with evidence and recovery actions.",
        |_context: &AppContext, input: SyncInput| scaffold_operation("sync", &input),
    );
    router.add_typed_tool_with_output_schema(
        "evict",
        "Evict a PR and safely close the graph gap when compatible; requires a reason and never bypasses textual conflicts.",
        |context: &AppContext, input: EvictInput| reshape::evict(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "split",
        "Split a caravan before the selected PR, which becomes a new head only if fleet invariants remain valid.",
        |context: &AppContext, input: SplitInput| reshape::split(context, &input),
    );
    router.add_typed_tool_with_output_schema(
        "van_list",
        "List every caravan in deterministic fleet navigation order. Read-only.",
        |context: &AppContext, _input: EmptyInput| navigation::list(context),
    );
    router.add_typed_tool_with_output_schema(
        "van_next",
        "Safely check out the next caravan head in deterministic fleet browsing order.",
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
        "Safely check out the previous caravan head in deterministic fleet browsing order.",
        |context: &AppContext, _input: EmptyInput| {
            navigation::navigate(
                context,
                navigation::Scope::Fleet,
                navigation::Direction::Previous,
            )
        },
    );

    updatable_cli::register_update_tool(&mut router, |_context: &AppContext| updater_config());
    feedback_cli::register_feedback_tools(&mut router, |_context: &AppContext| feedback_config());
    router
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
    fn domain_stub_is_structured_and_honest() {
        let error = scaffold_operation("status", &EmptyInput::default())
            .expect_err("the foundation must not pretend discovery exists");
        assert_eq!(error.category(), ErrorCategory::UnsupportedCapability);
        assert_eq!(error.code(), "not_implemented");
        assert_eq!(error.details().expect("details")["operation"], "status");
    }

    #[test]
    fn router_exposes_domain_and_ecosystem_tools() {
        let names: Vec<String> = build_router()
            .tool_metadata()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        for expected in [
            "help",
            "status",
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
    fn help_tool_returns_a_success_envelope() {
        let envelope = build_router().call_tool(&AppContext::default(), "help", json!({}));
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
