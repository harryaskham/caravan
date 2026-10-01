//! Fenced, journaled provider close for one exact already-represented non-member.

mod provider;
#[cfg(test)]
mod tests;
mod transaction;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use clap::{Args, ValueEnum};
use mcp_cli::StructuredError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::command::{GithubRequestBudget, ProcessRunner};
use crate::model::{BranchSnapshot, GitCommitIdentity, PrNumber, PullRequestState, RepositoryId};
use crate::{AppContext, AppError};

/// Mechanical source representation; neither variant accepts a patch-id assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum MainRepresentation {
    Ancestor,
    SameTree,
}

/// One explicit close intent. Custody references are audit data, not authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Args)]
#[serde(deny_unknown_fields)]
pub struct PrCloseInput {
    /// Exact GitHub owner/name (distinct from global `--repo`, a local checkout path).
    #[arg(long = "expected-repository")]
    pub repository: String,
    /// Exact provider PR number.
    #[arg(long)]
    pub pr: u64,
    /// Exact source branch name.
    #[arg(long)]
    pub head_ref: String,
    /// Full source head OID.
    #[arg(long)]
    pub head: String,
    /// Exact provider base branch name.
    #[arg(long)]
    pub base_ref: String,
    /// Full provider base OID.
    #[arg(long)]
    pub base: String,
    /// Expected repository default branch name.
    #[arg(long)]
    pub main_ref: String,
    /// Full expected live default-branch OID.
    #[arg(long)]
    pub main: String,
    /// Exact representation commit, which must be contained in the expected main.
    #[arg(long)]
    pub represented_at: String,
    /// Required mechanically verified representation form.
    #[arg(long, value_enum)]
    pub representation: MainRepresentation,
    /// Non-secret authenticated caller identity, validated by the integrating caller.
    #[arg(long)]
    pub actor: String,
    /// Non-secret owner/assignment/generation receipt reference; not a credential.
    #[arg(long)]
    pub custody_reference: String,
    /// Bounded non-secret close rationale.
    #[arg(long)]
    pub reason: String,
    /// Stable key retained for every retry of this exact request.
    #[arg(long)]
    pub operation_key: String,
    /// Explicitly authorize this close request; never inferred from a default.
    #[arg(long)]
    #[serde(default)]
    pub confirmed: bool,
}

/// Read a historical local receipt, without provider access or mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Args)]
#[serde(deny_unknown_fields)]
pub struct PrCloseStatusInput {
    #[arg(long)]
    pub operation_key: String,
}

/// Only identity/control facts, not PR bodies, check logs or credential output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClosePull {
    pub number: PrNumber,
    pub head: BranchSnapshot,
    pub base: BranchSnapshot,
    pub state: PullRequestState,
    pub merged_at: Option<String>,
    pub draft: bool,
    pub cross_repository: bool,
    pub auto_merge: bool,
    pub labels: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CloseFacts {
    pub repository: RepositoryId,
    pub pull: ClosePull,
    pub default_branch: BranchSnapshot,
    pub source: GitCommitIdentity,
    pub represented_at: GitCommitIdentity,
    pub represented_on_main: bool,
    pub source_in_representation: bool,
    /// Open provider-native Stack resources containing this PR; closed history is not membership.
    pub native_stacks: Vec<u64>,
    pub permission: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CloseOutcome {
    Closed,
    ReconciledClosed,
    ExternallyClosed,
    ExternallyMerged,
    Refused,
    Unavailable,
    Indeterminate,
}

/// An observation of one operation, not a GitHub head-CAS guarantee.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PrCloseOutput {
    pub schema_version: u32,
    pub operation_key: String,
    pub request_fingerprint: String,
    pub policy_fingerprint: String,
    pub writer_mode: crate::config::WriterMode,
    pub journal_scope: String,
    pub atomic_provider_transaction: bool,
    pub close_intent_recorded: bool,
    pub close_attempted_this_call: bool,
    /// Provider mutation in this invocation; unknown after an uncertain attempt.
    pub provider_mutated: Option<bool>,
    /// Historical HTTP result for the retained intent, independent of this call.
    pub operation_write_returned_success: Option<bool>,
    pub operation_write_error_code: Option<String>,
    pub intent_fence_fingerprint: Option<String>,
    pub outcome: CloseOutcome,
    pub code: String,
    pub observed: Option<CloseFacts>,
    pub next: String,
}

/// Durable request/intent and both the original and latest readback survive restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrCloseRecord {
    pub schema_version: u32,
    pub request: PrCloseInput,
    pub policy_fingerprint: String,
    pub writer_mode: crate::config::WriterMode,
    pub intent: Option<CloseFacts>,
    pub intent_fence_fingerprint: Option<String>,
    pub write_returned_success: Option<bool>,
    pub write_error_code: Option<String>,
    pub first_result: Option<PrCloseOutput>,
    pub latest_result: Option<PrCloseOutput>,
}

/// Execute one explicit close, or reconcile the retained exact intent without replay.
pub fn apply(context: &AppContext, input: &PrCloseInput) -> Result<PrCloseOutput, AppError> {
    validate_input(input)?;
    let mut guard = context.acquire_writer_operation("pr_close")?;
    let deadline = Instant::now() + Duration::from_secs(context.config.command_timeout_secs);
    let runner = ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(Duration::from_secs(context.config.command_timeout_secs))
        .with_operation_deadline(deadline)
        .with_github_request_budget(GithubRequestBudget::new(
            context.config.sync.max_github_requests_per_tick,
        ));
    let provider = provider::LiveProvider::new(context, &guard, runner);
    let store = transaction::CheckpointStore::new(&context.repository_path, &input.operation_key);
    let result = transaction::execute(
        &provider,
        &store,
        input,
        &policy_fingerprint(context),
        context.config.writer.mode,
    );
    guard.checkpoint(
        "pr_close_returned",
        json!({"operation_key": input.operation_key, "result_available": result.is_ok()}),
        result
            .as_ref()
            .is_err_and(|error| error.code() == "pr_close_indeterminate"),
    )?;
    guard.release()?;
    result
}

/// This is retained local evidence only; it never claims current provider state.
pub fn status(context: &AppContext, input: &PrCloseStatusInput) -> Result<PrCloseRecord, AppError> {
    validate_key(&input.operation_key)?;
    transaction::load_record(&context.repository_path, &input.operation_key)?.ok_or_else(|| {
        AppError::validation(
            "pr_close_receipt_missing",
            "no retained receipt for this operation key",
        )
    })
}

fn validate_input(input: &PrCloseInput) -> Result<(), AppError> {
    validate_key(&input.operation_key)?;
    let valid_slug = input
        .repository
        .split_once('/')
        .is_some_and(|(owner, name)| {
            !owner.is_empty()
                && !name.is_empty()
                && input.repository.len() <= 256
                && [owner, name].iter().all(|part| {
                    part.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                })
        });
    let text = |value: &str, limit| {
        !value.trim().is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
    };
    if !input.confirmed
        || input.pr == 0
        || !valid_slug
        || ![&input.head, &input.base, &input.main, &input.represented_at]
            .iter()
            .all(|value| valid_oid(value))
        || ![&input.head_ref, &input.base_ref, &input.main_ref]
            .iter()
            .all(|value| text(value, 1024))
        || !text(&input.actor, 256)
        || !text(&input.custody_reference, 256)
        || !text(&input.reason, 2048)
    {
        return Err(AppError::validation(
            "pr_close_input_invalid",
            "confirmed exact repository/PR/ref/OID identities and bounded non-secret audit references are required",
        ));
    }
    Ok(())
}

fn valid_oid(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_key(key: &str) -> Result<(), AppError> {
    if key.is_empty()
        || key.len() > 96
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
    {
        return Err(AppError::validation(
            "pr_close_key_invalid",
            "operation key must be 1..=96 ASCII letters, digits, dash, underscore, dot or colon",
        ));
    }
    Ok(())
}

fn fingerprint<T: Serialize>(value: &T) -> String {
    format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("close identity serializes"))
    )
}

fn policy_fingerprint(context: &AppContext) -> String {
    fingerprint(&context.config)
}
