//! Explicit, repository-scoped caravan incident holds.
//!
//! Holds live below Git's common directory so every linked worktree observes the
//! same state. Expiry is informational: only an explicit `resume` action may
//! remove a hold or re-enable auto-merge.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, ValueEnum};
use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::command::{CommandRunner, CommandSpec, ProcessRunner};
use crate::github::{GitHubMutationAdapter, GitHubMutationReceipt, MutationError};
use crate::model::{
    AutoMergeState, CheckSnapshot, CommitOid, GitCommitIdentity, MergeCandidateFreshness,
    MutationKind, MutationStep, MutationStepState, OperationId, OperationReceipt, PrNumber,
    PullRequestPrecondition, PullRequestState, RepositoryId, SyntheticMergeCandidate,
};
use crate::read::{self, StatusOutput};
use crate::{AppContext, AppError, PauseInput, ResumeInput};

const MAX_TEXT: usize = 512;
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// Durable, bounded evidence captured when one caravan is frozen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PauseRecord {
    pub version: u32,
    pub caravan_head: PrNumber,
    pub members: Vec<PrNumber>,
    /// Exact facts before auto-merge was disabled. The auto-merge member is
    /// intentionally squash-enabled; every other field remains a resume guard.
    pub expected_head: PullRequestPrecondition,
    pub expected_checks: Vec<CheckSnapshot>,
    pub actor: String,
    pub reason: String,
    pub paused_unix_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_unix_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_reference: Option<String>,
    /// Written by an explicit resume before remote mutation, making retries
    /// distinguishable from an unauthorized external auto-merge re-enable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_authorized_by: Option<String>,
    /// Exact-owner external recovery authority. While present, the pause stays
    /// effective even when the reviewed base/head transition makes the ordinary
    /// pause snapshot stale. Ordinary resume never consumes this authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<PauseRecoveryRecord>,
}

/// Versioned transition requested through CLI, JSON, web API, and MCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "kebab-case")]
pub enum PauseRecoveryPhase {
    Prepare,
    CheckpointBase,
    CheckpointHead,
    Finalize,
    Rollback,
}

/// Exact aggregate attribution for check rows on the final replacement head.
/// Every provider row must classify as either a `CheckRun` or `StatusContext`; an
/// unknown kind cannot disappear into either count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PauseRecoveryCheckAttribution {
    pub head_oid: CommitOid,
    pub check_run_count: u64,
    pub status_context_count: u64,
}

impl FromStr for PauseRecoveryCheckAttribution {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(value)
            .map_err(|error| format!("invalid check-attribution JSON: {error}"))
    }
}

/// Complete exact-owner recovery request. Stable identity fields are repeated
/// on every phase; a mismatch never inherits authority from an earlier call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Args)]
pub struct PauseRecoveryInput {
    #[arg(long, default_value_t = 1)]
    #[serde(default = "pause_recovery_schema_version")]
    pub schema_version: u32,
    /// External recovery operation identifier, e.g. owned-pr-retarget-head.
    #[arg(long)]
    pub operation_id: String,
    #[arg(long)]
    pub external_reference: String,
    #[arg(long)]
    pub idempotency_key: String,
    #[arg(long)]
    pub actor: String,
    #[arg(long)]
    pub owner_project: String,
    #[arg(long)]
    pub owner_agent: String,
    #[arg(long)]
    pub ownership_generation: String,
    /// Exact owner/name repository slug. CLI spelling avoids the existing
    /// global `--repository PATH` checkout selector; MCP/JSON remains `repository`.
    #[arg(long = "repository-slug")]
    pub repository: String,
    #[arg(long)]
    pub caravan_id: u64,
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    pub members: Vec<u64>,
    #[arg(long)]
    pub pause_id: String,
    #[arg(long)]
    pub pause_generation: String,
    #[arg(long)]
    pub target_pr: u64,
    #[arg(long)]
    pub expected_base_ref: String,
    #[arg(long)]
    pub expected_base_oid: String,
    #[arg(long)]
    pub expected_head_oid: String,
    #[arg(long)]
    pub desired_base_ref: String,
    #[arg(long)]
    pub desired_base_oid: String,
    #[arg(long)]
    pub desired_head_oid: String,
    #[arg(long)]
    pub desired_head_tree: String,
    /// Required only by finalize; exact provider order, normally base then head.
    #[arg(long, value_delimiter = ',')]
    #[serde(default)]
    pub virtual_merge_parents: Vec<String>,
    /// Required only by finalize.
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_merge_tree: Option<String>,
    /// Required only by finalize. CLI accepts one JSON object; MCP uses the same object.
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_attribution: Option<PauseRecoveryCheckAttribution>,
    #[arg(long)]
    pub reason: String,
}

const fn pause_recovery_schema_version() -> u32 {
    1
}

/// Stable identity bound at prepare and revalidated at every later phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PauseRecoveryBinding {
    pub schema_version: u32,
    pub operation_id: String,
    pub external_reference: String,
    pub idempotency_key: String,
    pub actor: String,
    pub owner_project: String,
    pub owner_agent: String,
    pub ownership_generation: String,
    pub repository: String,
    pub caravan_id: PrNumber,
    pub members: Vec<PrNumber>,
    pub pause_id: String,
    pub pause_generation: String,
    pub target_pr: PrNumber,
    pub expected_base_ref: String,
    pub expected_base_oid: CommitOid,
    pub expected_head_oid: CommitOid,
    pub desired_base_ref: String,
    pub desired_base_oid: CommitOid,
    pub desired_head_oid: CommitOid,
    pub desired_head_tree: CommitOid,
    pub reason: String,
}

/// Last durably acknowledged external provider step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PauseRecoveryCheckpoint {
    Prepared,
    BaseCheckpointed,
    HeadCheckpointed,
}

/// Durable in-pause ownership and checkpoint evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PauseRecoveryRecord {
    pub version: u32,
    pub binding: PauseRecoveryBinding,
    pub checkpoint: PauseRecoveryCheckpoint,
    pub receipt_id: String,
    pub prepared_unix_ms: u64,
    pub updated_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PauseRecoveryStatus {
    Prepared,
    BaseCheckpointed,
    HeadCheckpointed,
    Finalized,
    RolledBack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PauseRecoveryFenceState {
    Active,
    Released,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PauseRecoveryRollbackState {
    Available,
    Completed,
    Unavailable,
}

/// Final provider evidence independently rediscovered by Cara.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PauseRecoveryFinalEvidence {
    pub head: CommitOid,
    pub head_tree: CommitOid,
    pub head_parents: Vec<CommitOid>,
    pub virtual_merge: SyntheticMergeCandidate,
    pub check_attribution: PauseRecoveryCheckAttribution,
}

/// Stable flat response contract consumed from the standard mcp-cli `data` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PauseRecoveryOutput {
    pub schema_version: u32,
    pub phase: PauseRecoveryPhase,
    pub status: PauseRecoveryStatus,
    /// This surface only verifies and checkpoints external writes.
    pub provider_mutated: bool,
    pub operation_changed: bool,
    pub receipt_id: String,
    pub next_action: String,
    pub operation_id: String,
    pub external_reference: String,
    pub idempotency_key: String,
    pub actor: String,
    pub owner_project: String,
    pub owner_agent: String,
    pub ownership_generation: String,
    pub repository: String,
    pub caravan_id: PrNumber,
    pub members: Vec<PrNumber>,
    pub pause_id: String,
    pub pause_generation: String,
    pub target_pr: PrNumber,
    pub expected_base_ref: String,
    pub expected_base_oid: CommitOid,
    pub expected_head_oid: CommitOid,
    pub desired_base_ref: String,
    pub desired_base_oid: CommitOid,
    pub desired_head_oid: CommitOid,
    pub desired_head_tree: CommitOid,
    pub fence_state: PauseRecoveryFenceState,
    pub rollback_state: PauseRecoveryRollbackState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_merge_parents: Option<Vec<CommitOid>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_merge_tree: Option<CommitOid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_attribution: Option<PauseRecoveryCheckAttribution>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum PauseRecoveryTerminalOutcome {
    Finalized,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
struct PauseRecoveryTerminalReceipt {
    version: u32,
    binding: PauseRecoveryBinding,
    outcome: PauseRecoveryTerminalOutcome,
    receipt_id: String,
    completed_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    final_evidence: Option<PauseRecoveryFinalEvidence>,
}

/// How current live facts relate to a durable hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PauseState {
    Active,
    Expired,
    Stale,
    /// An exact-owner recovery phase is active and current provider facts are
    /// one of its reviewed old/transitional/desired states.
    Recovering,
    /// Provider facts escaped every reviewed recovery state. The fence remains
    /// effective; only exact rollback/finalize reconciliation may release it.
    RecoveryDrift,
    /// Provider truth shows the recorded head merged (or closed). The hold is
    /// historical evidence only: it can never be resumed, never implies an
    /// auto-merge repair, and never represents an active caravan.
    Retired,
}

impl PauseState {
    /// Whether this hold still constrains live operations. Stale and retired
    /// records are diagnostics, never authority.
    #[must_use]
    pub fn is_effective(self) -> bool {
        matches!(
            self,
            Self::Active | Self::Expired | Self::Recovering | Self::RecoveryDrift
        )
    }
}

/// Status-facing hold report. Stale holds never suppress graph errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PauseStatus {
    pub record: PauseRecord,
    pub state: PauseState,
    pub auto_merge_suspended: bool,
    /// Exact provider-truth terminal state for the recorded head, when the
    /// head is no longer open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_state: Option<PullRequestState>,
    pub safe_next_action: String,
}

/// Receipt for an explicit pause or resume action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PauseOutput {
    pub receipt: OperationReceipt,
    pub pause: PauseRecord,
    #[serde(default)]
    pub provider_receipts: Vec<GitHubMutationReceipt>,
    pub next: String,
}

pub trait PauseProvider {
    fn disable_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError>;
    fn enable_squash_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError>;
}
impl<R: CommandRunner> PauseProvider for GitHubMutationAdapter<R> {
    fn disable_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.disable_auto_merge(repository, expected)
    }
    fn enable_squash_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.enable_squash_auto_merge(repository, expected)
    }
}

pub fn pause(context: &AppContext, input: &PauseInput) -> Result<PauseOutput, AppError> {
    validate_text("actor", &input.actor)?;
    validate_text("reason", &input.reason)?;
    if let Some(reference) = &input.external_reference {
        validate_text("external reference", reference)?;
    }
    let lock = context.acquire_writer_operation("pause")?;
    let status = read::status(context)?;
    crate::initialization::require_ready(&status.initialization)?;
    let head = PrNumber(input.head_pr);
    if let Some(existing) = load_one(&context.repository_path, head)? {
        let report = classify(&status, existing.clone());
        if report.state.is_effective() {
            return Ok(noop(
                "pause",
                existing,
                "hold already exists; use `cara resume --head-pr <PR> --actor <actor>` after recovery",
            ));
        }
        return Err(stale_error("pause", &report));
    }
    let caravan = status.analysis.fleet.caravan(head).ok_or_else(|| {
        AppError::validation(
            "caravan_head_not_found",
            format!("#{head} is not a current caravan head"),
        )
    })?;
    reject_other_graph_problems(&status, caravan.members.as_slice(), true)?;
    let current = status
        .analysis
        .pull_requests
        .get(&head)
        .expect("caravan head snapshot");
    let mut expected = PullRequestPrecondition::from(current);
    // A pause retry may start after containment already disabled auto-merge.
    // Record the invariant resume must restore, never disabled as the target.
    expected.auto_merge = AutoMergeState::squash();
    let record = PauseRecord {
        version: 1,
        caravan_head: head,
        members: caravan.members.clone(),
        expected_head: expected,
        expected_checks: current.checks.clone(),
        actor: input.actor.clone(),
        reason: input.reason.clone(),
        paused_unix_secs: now(),
        expires_unix_secs: input.expires_unix_secs,
        external_reference: input.external_reference.clone(),
        resume_authorized_by: None,
        recovery: None,
    };
    validate_record_size(&record)?;
    let runner = ProcessRunner::in_directory(&context.repository_path).with_timeout(
        std::time::Duration::from_secs(context.config.command_timeout_secs),
    );
    let provider = GitHubMutationAdapter::new(lock.runner(runner));
    let mut receipts = Vec::new();
    let mut steps = Vec::new();
    if current.auto_merge.enabled {
        let receipt = provider
            .disable_auto_merge(&status.repository, &PullRequestPrecondition::from(current))
            .map_err(|error| mutation_error("pause", &error))?;
        receipts.push(receipt);
        steps.push(step(
            MutationKind::DisableAutoMerge,
            MutationStepState::Completed,
            head,
            "disabled squash auto-merge on paused head",
        ));
    } else {
        steps.push(step(
            MutationKind::DisableAutoMerge,
            MutationStepState::AlreadySatisfied,
            head,
            "paused head auto-merge already disabled",
        ));
    }
    write_record(&context.repository_path, &record)?;
    append_audit(&context.repository_path, "pause", &record, &input.actor)?;
    Ok(PauseOutput {
        receipt: receipt("pause", steps),
        pause: record,
        provider_receipts: receipts,
        next:
            "leave the hold in place until CI/operator recovery, then explicitly run `cara resume`"
                .to_owned(),
    })
}

pub fn resume(context: &AppContext, input: &ResumeInput) -> Result<PauseOutput, AppError> {
    validate_text("actor", &input.actor)?;
    let lock = context.acquire_writer_operation("resume")?;
    let head = PrNumber(input.head_pr);
    let Some(record) = load_one(&context.repository_path, head)? else {
        return Err(AppError::validation(
            "pause_not_found",
            format!("caravan #{head} has no durable pause to resume"),
        ));
    };
    reject_ordinary_resume_during_recovery(&record, head)?;
    let status = read::status(context)?;
    crate::initialization::require_ready(&status.initialization)?;
    let report = classify(&status, record.clone());
    if !report.state.is_effective() {
        return Err(stale_error("resume", &report));
    }
    reject_other_graph_problems(&status, &record.members, true)?;
    let current = status
        .analysis
        .pull_requests
        .get(&head)
        .expect("classified pause head");
    if current.checks.is_empty()
        || current.checks.iter().any(|check| {
            !matches!(
                check.state,
                crate::model::CheckState::Success
                    | crate::model::CheckState::Neutral
                    | crate::model::CheckState::Skipped
            )
        })
    {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "paused_ci_not_ready",
            "paused head checks are not all in a safe terminal state",
            Some(
                json!({"head": head, "checks": current.checks, "next": "leave the hold in place; after CI is safely terminal, explicitly rerun `cara resume`"}),
            ),
        ));
    }
    let mut record = record;
    if record.resume_authorized_by.is_none() {
        record.resume_authorized_by = Some(input.actor.clone());
        write_record(&context.repository_path, &record)?;
        append_audit(
            &context.repository_path,
            "resume_authorized",
            &record,
            &input.actor,
        )?;
    }
    let runner = ProcessRunner::in_directory(&context.repository_path).with_timeout(
        std::time::Duration::from_secs(context.config.command_timeout_secs),
    );
    let provider = GitHubMutationAdapter::new(lock.runner(runner));
    let mut receipts = Vec::new();
    let mut steps = Vec::new();
    if status.head_merge.actor.caravan() {
        // Cara is the merge actor, so a resumed caravan is released by removing
        // the hold itself: there is no provider arming to restore, and arming
        // one would install a second merge actor on the exact head this hold
        // was protecting.
        steps.push(step(
            MutationKind::EnableAutoMerge,
            MutationStepState::AlreadySatisfied,
            head,
            "cara is the single merge actor; the resumed root is merged by the next bounded sync tick with no provider auto-merge request",
        ));
    } else if current.auto_merge == AutoMergeState::squash() {
        steps.push(step(
            MutationKind::EnableAutoMerge,
            MutationStepState::AlreadySatisfied,
            head,
            "squash auto-merge already restored by a prior resume attempt",
        ));
    } else {
        provider
            .verify_precondition_with_checks(
                &status.repository,
                &PullRequestPrecondition::from(current),
            )
            .map_err(|error| mutation_error("resume", &error))?;
        let receipt = provider
            .enable_squash_auto_merge(&status.repository, &PullRequestPrecondition::from(current))
            .map_err(|error| mutation_error("resume", &error))?;
        receipts.push(receipt);
        steps.push(step(
            MutationKind::EnableAutoMerge,
            MutationStepState::Completed,
            head,
            "re-enabled squash auto-merge after exact hold revalidation",
        ));
    }
    append_audit(&context.repository_path, "resume", &record, &input.actor)?;
    remove_record(&context.repository_path, head)?;
    Ok(PauseOutput {
        receipt: receipt("resume", steps),
        pause: record,
        provider_receipts: receipts,
        next: "run `cara status`, then `cara sync` to continue the caravan".to_owned(),
    })
}

fn reject_ordinary_resume_during_recovery(
    record: &PauseRecord,
    head: PrNumber,
) -> Result<(), AppError> {
    let Some(recovery) = &record.recovery else {
        return Ok(());
    };
    Err(AppError::structured(
        ErrorCategory::Validation,
        "pause_recovery_in_progress",
        "ordinary resume cannot consume exact-owner recovery authority",
        Some(json!({
            "caravan_id": head,
            "operation_id": recovery.binding.operation_id,
            "owner_project": recovery.binding.owner_project,
            "owner_agent": recovery.binding.owner_agent,
            "ownership_generation": recovery.binding.ownership_generation,
            "checkpoint": recovery.checkpoint,
            "next": "use the exact matching `cara pause-recovery finalize` or `rollback`; never overwrite the pause or bypass the fence",
        })),
    ))
}

/// Checkpoint or release one externally executed exact-owner recovery.
///
/// Cara never performs the provider base/head mutation in this operation. It
/// durably owns the fence, independently rediscovers every checkpoint, and only
/// then advances or releases the pause.
#[allow(clippy::too_many_lines)]
pub fn pause_recovery(
    context: &AppContext,
    phase: PauseRecoveryPhase,
    input: &PauseRecoveryInput,
) -> Result<PauseRecoveryOutput, AppError> {
    let binding = recovery_binding(input)?;
    let mut lock = context.acquire_writer_operation("pause-recovery")?;

    if let Some(terminal) = load_terminal_receipt(&context.repository_path, &binding)? {
        return replay_terminal(context, phase, input, &binding, terminal);
    }

    let mut record = load_one(&context.repository_path, binding.caravan_id)?.ok_or_else(|| {
        AppError::validation(
            "pause_not_found",
            format!(
                "caravan #{} has no durable pause for exact-owner recovery",
                binding.caravan_id
            ),
        )
    })?;

    if phase == PauseRecoveryPhase::Prepare {
        if let Some(existing) = &record.recovery {
            require_same_binding(&binding, existing)?;
            return Ok(recovery_output(
                &binding,
                phase,
                checkpoint_status(existing.checkpoint),
                false,
                existing.receipt_id.clone(),
                PauseRecoveryFenceState::Active,
                PauseRecoveryRollbackState::Available,
                checkpoint_next(existing.checkpoint),
                None,
            ));
        }

        let status = read::status(context)?;
        crate::initialization::require_ready(&status.initialization)?;
        verify_prepare(&status, &record, &binding)?;
        let receipt_id = OperationId::new().0;
        let timestamp = now_ms();
        record.recovery = Some(PauseRecoveryRecord {
            version: 1,
            binding: binding.clone(),
            checkpoint: PauseRecoveryCheckpoint::Prepared,
            receipt_id: receipt_id.clone(),
            prepared_unix_ms: timestamp,
            updated_unix_ms: timestamp,
        });
        lock.checkpoint(
            "pause_recovery_prepared",
            json!({"binding": binding, "receipt_id": receipt_id}),
            false,
        )?;
        write_record(&context.repository_path, &record)?;
        append_audit(
            &context.repository_path,
            "pause_recovery_prepared",
            &record,
            &input.actor,
        )?;
        return Ok(recovery_output(
            &binding,
            phase,
            PauseRecoveryStatus::Prepared,
            true,
            receipt_id,
            PauseRecoveryFenceState::Active,
            PauseRecoveryRollbackState::Available,
            checkpoint_next(PauseRecoveryCheckpoint::Prepared),
            None,
        ));
    }

    let recovery = record.recovery.clone().ok_or_else(|| {
        AppError::validation(
            "pause_recovery_not_prepared",
            "run the exact matching `cara pause-recovery prepare` before any provider mutation",
        )
    })?;
    require_same_binding(&binding, &recovery)?;
    let status = read::status(context)?;
    crate::initialization::require_ready(&status.initialization)?;

    match phase {
        PauseRecoveryPhase::Prepare => unreachable!("handled above"),
        PauseRecoveryPhase::CheckpointBase => {
            if checkpoint_rank(recovery.checkpoint)
                >= checkpoint_rank(PauseRecoveryCheckpoint::BaseCheckpointed)
            {
                let replay_state =
                    if recovery.checkpoint == PauseRecoveryCheckpoint::BaseCheckpointed {
                        TargetState::DesiredBaseOldHead
                    } else {
                        TargetState::Desired
                    };
                verify_target(&status, &binding, replay_state)?;
                if recovery.checkpoint == PauseRecoveryCheckpoint::HeadCheckpointed {
                    let head_commit = rediscover_head_commit(context, &lock, &status, &binding)?;
                    verify_head_commit(&binding, &head_commit)?;
                }
                return Ok(recovery_output(
                    &binding,
                    phase,
                    checkpoint_status(recovery.checkpoint),
                    false,
                    recovery.receipt_id,
                    PauseRecoveryFenceState::Active,
                    PauseRecoveryRollbackState::Available,
                    checkpoint_next(recovery.checkpoint),
                    None,
                ));
            }
            verify_target(&status, &binding, TargetState::DesiredBaseOldHead)?;
            let recovery = record.recovery.as_mut().expect("checked recovery");
            recovery.checkpoint = PauseRecoveryCheckpoint::BaseCheckpointed;
            recovery.updated_unix_ms = now_ms();
            let receipt_id = recovery.receipt_id.clone();
            lock.checkpoint(
                "pause_recovery_base_checkpointed",
                json!({"binding": binding, "receipt_id": receipt_id}),
                false,
            )?;
            write_record(&context.repository_path, &record)?;
            append_audit(
                &context.repository_path,
                "pause_recovery_base_checkpointed",
                &record,
                &input.actor,
            )?;
            Ok(recovery_output(
                &binding,
                phase,
                PauseRecoveryStatus::BaseCheckpointed,
                true,
                receipt_id,
                PauseRecoveryFenceState::Active,
                PauseRecoveryRollbackState::Available,
                checkpoint_next(PauseRecoveryCheckpoint::BaseCheckpointed),
                None,
            ))
        }
        PauseRecoveryPhase::CheckpointHead => {
            require_checkpoint(
                recovery.checkpoint,
                PauseRecoveryCheckpoint::BaseCheckpointed,
                "checkpoint-base",
            )?;
            if recovery.checkpoint == PauseRecoveryCheckpoint::HeadCheckpointed {
                verify_target(&status, &binding, TargetState::Desired)?;
                let head_commit = rediscover_head_commit(context, &lock, &status, &binding)?;
                verify_head_commit(&binding, &head_commit)?;
                return Ok(recovery_output(
                    &binding,
                    phase,
                    PauseRecoveryStatus::HeadCheckpointed,
                    false,
                    recovery.receipt_id,
                    PauseRecoveryFenceState::Active,
                    PauseRecoveryRollbackState::Available,
                    checkpoint_next(PauseRecoveryCheckpoint::HeadCheckpointed),
                    None,
                ));
            }
            verify_target(&status, &binding, TargetState::Desired)?;
            let head_commit = rediscover_head_commit(context, &lock, &status, &binding)?;
            verify_head_commit(&binding, &head_commit)?;
            let recovery = record.recovery.as_mut().expect("checked recovery");
            recovery.checkpoint = PauseRecoveryCheckpoint::HeadCheckpointed;
            recovery.updated_unix_ms = now_ms();
            let receipt_id = recovery.receipt_id.clone();
            lock.checkpoint(
                "pause_recovery_head_checkpointed",
                json!({
                    "binding": binding,
                    "receipt_id": receipt_id,
                    "head_commit": head_commit,
                }),
                false,
            )?;
            write_record(&context.repository_path, &record)?;
            append_audit(
                &context.repository_path,
                "pause_recovery_head_checkpointed",
                &record,
                &input.actor,
            )?;
            Ok(recovery_output(
                &binding,
                phase,
                PauseRecoveryStatus::HeadCheckpointed,
                true,
                receipt_id,
                PauseRecoveryFenceState::Active,
                PauseRecoveryRollbackState::Available,
                checkpoint_next(PauseRecoveryCheckpoint::HeadCheckpointed),
                None,
            ))
        }
        PauseRecoveryPhase::Finalize => {
            require_checkpoint(
                recovery.checkpoint,
                PauseRecoveryCheckpoint::HeadCheckpointed,
                "checkpoint-head",
            )?;
            verify_target(&status, &binding, TargetState::Desired)?;
            let head_commit = rediscover_head_commit(context, &lock, &status, &binding)?;
            let evidence = verify_final_evidence(&status, input, &binding, head_commit)?;
            advance_pause_evidence(&status, &mut record, &binding)?;
            lock.checkpoint(
                "pause_recovery_final_verified",
                json!({
                    "binding": binding,
                    "receipt_id": recovery.receipt_id,
                    "final_evidence": evidence,
                }),
                false,
            )?;
            // Keep recovery authority in the advanced pause until the durable
            // terminal receipt exists. A crash at either write remains fenced.
            write_record(&context.repository_path, &record)?;
            let terminal = PauseRecoveryTerminalReceipt {
                version: 1,
                binding: binding.clone(),
                outcome: PauseRecoveryTerminalOutcome::Finalized,
                receipt_id: recovery.receipt_id.clone(),
                completed_unix_ms: now_ms(),
                final_evidence: Some(evidence.clone()),
            };
            write_terminal_receipt(&context.repository_path, &terminal)?;
            append_audit(
                &context.repository_path,
                "pause_recovery_finalized",
                &record,
                &input.actor,
            )?;
            remove_record(&context.repository_path, binding.caravan_id)?;
            Ok(recovery_output(
                &binding,
                phase,
                PauseRecoveryStatus::Finalized,
                true,
                recovery.receipt_id,
                PauseRecoveryFenceState::Released,
                PauseRecoveryRollbackState::Unavailable,
                "recovery finalized and the exact pause fence released; rediscover before ordinary sync",
                Some(&evidence),
            ))
        }
        PauseRecoveryPhase::Rollback => {
            verify_target(&status, &binding, TargetState::Old)?;
            verify_original_topology(&status, &binding)?;
            lock.checkpoint(
                "pause_recovery_rollback_verified",
                json!({"binding": binding, "receipt_id": recovery.receipt_id}),
                false,
            )?;
            let terminal = PauseRecoveryTerminalReceipt {
                version: 1,
                binding: binding.clone(),
                outcome: PauseRecoveryTerminalOutcome::RolledBack,
                receipt_id: recovery.receipt_id.clone(),
                completed_unix_ms: now_ms(),
                final_evidence: None,
            };
            write_terminal_receipt(&context.repository_path, &terminal)?;
            append_audit(
                &context.repository_path,
                "pause_recovery_rolled_back",
                &record,
                &input.actor,
            )?;
            remove_record(&context.repository_path, binding.caravan_id)?;
            Ok(recovery_output(
                &binding,
                phase,
                PauseRecoveryStatus::RolledBack,
                true,
                recovery.receipt_id,
                PauseRecoveryFenceState::Released,
                PauseRecoveryRollbackState::Completed,
                "rollback to the exact old provider state verified and the pause fence released",
                None,
            ))
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum TargetState {
    Old,
    DesiredBaseOldHead,
    Desired,
}

fn recovery_binding(input: &PauseRecoveryInput) -> Result<PauseRecoveryBinding, AppError> {
    if input.schema_version != 1 {
        return Err(AppError::validation(
            "pause_recovery_schema_unsupported",
            "pause-recovery requires schema_version 1",
        ));
    }
    for (name, value) in [
        ("operation_id", &input.operation_id),
        ("external_reference", &input.external_reference),
        ("idempotency_key", &input.idempotency_key),
        ("actor", &input.actor),
        ("owner_project", &input.owner_project),
        ("owner_agent", &input.owner_agent),
        ("ownership_generation", &input.ownership_generation),
        ("repository", &input.repository),
        ("pause_id", &input.pause_id),
        ("pause_generation", &input.pause_generation),
        ("expected_base_ref", &input.expected_base_ref),
        ("desired_base_ref", &input.desired_base_ref),
        ("reason", &input.reason),
    ] {
        validate_recovery_text(name, value)?;
    }
    if input.members.is_empty()
        || input.members.first().copied() != Some(input.caravan_id)
        || !input.members.contains(&input.target_pr)
    {
        return Err(AppError::validation(
            "pause_recovery_members_invalid",
            "members must be non-empty, start with caravan_id, and contain target_pr",
        ));
    }
    let unique = input
        .members
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    if unique.len() != input.members.len() {
        return Err(AppError::validation(
            "pause_recovery_members_invalid",
            "members must not contain duplicate PR numbers",
        ));
    }
    Ok(PauseRecoveryBinding {
        schema_version: 1,
        operation_id: input.operation_id.clone(),
        external_reference: input.external_reference.clone(),
        idempotency_key: input.idempotency_key.clone(),
        actor: input.actor.clone(),
        owner_project: input.owner_project.clone(),
        owner_agent: input.owner_agent.clone(),
        ownership_generation: input.ownership_generation.clone(),
        repository: input.repository.clone(),
        caravan_id: PrNumber(input.caravan_id),
        members: input.members.iter().copied().map(PrNumber).collect(),
        pause_id: input.pause_id.clone(),
        pause_generation: input.pause_generation.clone(),
        target_pr: PrNumber(input.target_pr),
        expected_base_ref: input.expected_base_ref.clone(),
        expected_base_oid: recovery_oid("expected_base_oid", &input.expected_base_oid)?,
        expected_head_oid: recovery_oid("expected_head_oid", &input.expected_head_oid)?,
        desired_base_ref: input.desired_base_ref.clone(),
        desired_base_oid: recovery_oid("desired_base_oid", &input.desired_base_oid)?,
        desired_head_oid: recovery_oid("desired_head_oid", &input.desired_head_oid)?,
        desired_head_tree: recovery_oid("desired_head_tree", &input.desired_head_tree)?,
        reason: input.reason.clone(),
    })
}

fn verify_prepare(
    status: &StatusOutput,
    record: &PauseRecord,
    binding: &PauseRecoveryBinding,
) -> Result<(), AppError> {
    if status.repository.slug() != binding.repository {
        return recovery_mismatch(
            "repository",
            json!(binding.repository),
            json!(status.repository.slug()),
        );
    }
    if record.actor != binding.actor {
        return recovery_mismatch("pause.actor", json!(binding.actor), json!(record.actor));
    }
    if record.external_reference.as_deref() != Some(binding.external_reference.as_str()) {
        return recovery_mismatch(
            "pause.external_reference",
            json!(binding.external_reference),
            json!(record.external_reference),
        );
    }
    if record.caravan_head != binding.caravan_id || record.members != binding.members {
        return recovery_mismatch(
            "pause.topology",
            json!({"caravan_id":binding.caravan_id,"members":binding.members}),
            json!({"caravan_id":record.caravan_head,"members":record.members}),
        );
    }
    let report = classify(status, record.clone());
    if !matches!(report.state, PauseState::Active | PauseState::Expired) {
        return Err(stale_error("prepare exact-owner recovery for", &report));
    }
    verify_original_topology(status, binding)?;
    verify_target(status, binding, TargetState::Old)
}

fn verify_original_topology(
    status: &StatusOutput,
    binding: &PauseRecoveryBinding,
) -> Result<(), AppError> {
    let actual = status
        .analysis
        .fleet
        .caravan(binding.caravan_id)
        .map(|caravan| caravan.members.clone());
    if actual.as_ref() != Some(&binding.members) {
        return recovery_mismatch("caravan.members", json!(binding.members), json!(actual));
    }
    Ok(())
}

fn verify_target(
    status: &StatusOutput,
    binding: &PauseRecoveryBinding,
    state: TargetState,
) -> Result<(), AppError> {
    let target = status
        .analysis
        .pull_requests
        .get(&binding.target_pr)
        .ok_or_else(|| {
            AppError::validation(
                "pause_recovery_target_missing",
                format!(
                    "target PR #{} is absent from provider discovery",
                    binding.target_pr
                ),
            )
        })?;
    let (base_ref, base_oid, head_oid) = match state {
        TargetState::Old => (
            &binding.expected_base_ref,
            &binding.expected_base_oid,
            &binding.expected_head_oid,
        ),
        TargetState::DesiredBaseOldHead => (
            &binding.desired_base_ref,
            &binding.desired_base_oid,
            &binding.expected_head_oid,
        ),
        TargetState::Desired => (
            &binding.desired_base_ref,
            &binding.desired_base_oid,
            &binding.desired_head_oid,
        ),
    };
    if target.state != PullRequestState::Open
        || &target.base.name != base_ref
        || &target.base.oid != base_oid
        || &target.head.oid != head_oid
    {
        return recovery_mismatch(
            "target.provider_state",
            json!({
                "state":"open","base_ref":base_ref,"base_oid":base_oid,"head_oid":head_oid,
            }),
            json!({
                "state":target.state,"base_ref":target.base.name,"base_oid":target.base.oid,"head_oid":target.head.oid,
            }),
        );
    }
    Ok(())
}

fn rediscover_head_commit(
    context: &AppContext,
    lock: &crate::writer_guard::WriterOperationGuard,
    status: &StatusOutput,
    binding: &PauseRecoveryBinding,
) -> Result<GitCommitIdentity, AppError> {
    let runner = ProcessRunner::in_directory(&context.repository_path).with_timeout(
        std::time::Duration::from_secs(context.config.command_timeout_secs),
    );
    GitHubMutationAdapter::new(lock.runner(runner))
        .commit_identity(&status.repository, &binding.desired_head_oid)
        .map_err(|error| recovery_provider_error("head_commit", &error))
}

fn verify_head_commit(
    binding: &PauseRecoveryBinding,
    head: &GitCommitIdentity,
) -> Result<(), AppError> {
    if head.oid != binding.desired_head_oid
        || head.tree_oid != binding.desired_head_tree
        || head.parents != vec![binding.desired_base_oid.clone()]
    {
        return recovery_mismatch(
            "replacement_head",
            json!({
                "oid":binding.desired_head_oid,
                "tree_oid":binding.desired_head_tree,
                "parents":[binding.desired_base_oid],
            }),
            json!(head),
        );
    }
    Ok(())
}

fn verify_final_evidence(
    status: &StatusOutput,
    input: &PauseRecoveryInput,
    binding: &PauseRecoveryBinding,
    head: GitCommitIdentity,
) -> Result<PauseRecoveryFinalEvidence, AppError> {
    verify_head_commit(binding, &head)?;
    verify_original_topology(status, binding)?;
    let target = status
        .analysis
        .pull_requests
        .get(&binding.target_pr)
        .expect("target verified before final evidence");
    let supplied_attribution = input
        .check_attribution
        .as_ref()
        .ok_or_else(|| final_field_required("check_attribution"))?;
    let discovered_attribution = check_attribution(target)?;
    if supplied_attribution != &discovered_attribution {
        return recovery_mismatch(
            "final.check_attribution",
            json!(supplied_attribution),
            json!(discovered_attribution),
        );
    }
    let candidate = status
        .merge_candidates
        .iter()
        .find(|candidate| candidate.pr == binding.target_pr)
        .ok_or_else(|| {
            AppError::validation(
                "pause_recovery_virtual_merge_missing",
                "provider discovery omitted the target virtual merge candidate",
            )
        })?;
    if candidate.freshness != MergeCandidateFreshness::Fresh
        || candidate.base.name != binding.desired_base_ref
        || candidate.base.oid != binding.desired_base_oid
        || candidate.head.oid != binding.desired_head_oid
    {
        return recovery_mismatch(
            "final.virtual_merge_identity",
            json!({
                "freshness":"fresh",
                "base_ref":binding.desired_base_ref,
                "base_oid":binding.desired_base_oid,
                "head_oid":binding.desired_head_oid,
            }),
            json!(candidate),
        );
    }
    let synthetic = candidate.synthetic.clone().ok_or_else(|| {
        AppError::validation(
            "pause_recovery_virtual_merge_missing",
            "fresh target has no provider virtual merge commit",
        )
    })?;
    let supplied_tree = input
        .virtual_merge_tree
        .as_deref()
        .ok_or_else(|| final_field_required("virtual_merge_tree"))?;
    let supplied_parents = input
        .virtual_merge_parents
        .iter()
        .map(|oid| recovery_oid("virtual_merge_parents", oid))
        .collect::<Result<Vec<_>, _>>()?;
    let supplied_tree = recovery_oid("virtual_merge_tree", supplied_tree)?;
    if supplied_parents
        != vec![
            binding.desired_base_oid.clone(),
            binding.desired_head_oid.clone(),
        ]
        || synthetic.git_ref != format!("refs/pull/{}/merge", binding.target_pr.0)
        || synthetic.tree_oid != supplied_tree
        || synthetic.parents != supplied_parents
    {
        return recovery_mismatch(
            "final.virtual_merge",
            json!({
                "git_ref":format!("refs/pull/{}/merge", binding.target_pr.0),
                "tree_oid":supplied_tree,
                "parents":supplied_parents,
            }),
            json!(synthetic),
        );
    }
    Ok(PauseRecoveryFinalEvidence {
        head: head.oid,
        head_tree: head.tree_oid,
        head_parents: head.parents,
        virtual_merge: synthetic,
        check_attribution: discovered_attribution,
    })
}

fn check_attribution(
    target: &crate::model::PullRequestSnapshot,
) -> Result<PauseRecoveryCheckAttribution, AppError> {
    let mut check_run_count = 0_u64;
    let mut status_context_count = 0_u64;
    for check in &target.checks {
        match check.provider_kind.as_deref() {
            Some("CheckRun") => check_run_count = check_run_count.saturating_add(1),
            Some("StatusContext") => {
                status_context_count = status_context_count.saturating_add(1);
            }
            kind => {
                return Err(AppError::structured(
                    ErrorCategory::Validation,
                    "pause_recovery_check_attribution_unknown",
                    "every final provider check row must have exact CheckRun or StatusContext attribution",
                    Some(json!({"check":check,"provider_kind":kind})),
                ));
            }
        }
    }
    Ok(PauseRecoveryCheckAttribution {
        head_oid: target.head.oid.clone(),
        check_run_count,
        status_context_count,
    })
}

fn advance_pause_evidence(
    status: &StatusOutput,
    record: &mut PauseRecord,
    binding: &PauseRecoveryBinding,
) -> Result<(), AppError> {
    let caravan = status
        .analysis
        .fleet
        .caravan(binding.caravan_id)
        .ok_or_else(|| {
            AppError::validation(
                "pause_recovery_final_topology_missing",
                "final provider topology no longer contains the paused caravan",
            )
        })?;
    if caravan.members != binding.members {
        return recovery_mismatch(
            "final.caravan_members",
            json!(binding.members),
            json!(caravan.members),
        );
    }
    let head = status
        .analysis
        .pull_requests
        .get(&binding.caravan_id)
        .ok_or_else(|| {
            AppError::validation(
                "pause_recovery_final_head_missing",
                "final provider topology omitted the paused caravan head",
            )
        })?;
    let mut expected = PullRequestPrecondition::from(head);
    // Preserve the same post-release invariant as ordinary pause/resume.
    expected.auto_merge = AutoMergeState::squash();
    record.expected_head = expected;
    record.expected_checks.clone_from(&head.checks);
    record.members.clone_from(&caravan.members);
    Ok(())
}

fn replay_terminal(
    context: &AppContext,
    phase: PauseRecoveryPhase,
    input: &PauseRecoveryInput,
    binding: &PauseRecoveryBinding,
    terminal: PauseRecoveryTerminalReceipt,
) -> Result<PauseRecoveryOutput, AppError> {
    if terminal.binding != *binding {
        return recovery_mismatch("terminal.binding", json!(binding), json!(terminal.binding));
    }
    let (expected_phase, status, rollback_state, next) = match terminal.outcome {
        PauseRecoveryTerminalOutcome::Finalized => (
            PauseRecoveryPhase::Finalize,
            PauseRecoveryStatus::Finalized,
            PauseRecoveryRollbackState::Unavailable,
            "exact finalized receipt replayed; the fence remains released",
        ),
        PauseRecoveryTerminalOutcome::RolledBack => (
            PauseRecoveryPhase::Rollback,
            PauseRecoveryStatus::RolledBack,
            PauseRecoveryRollbackState::Completed,
            "exact rollback receipt replayed; the fence remains released",
        ),
    };
    if phase != expected_phase {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "pause_recovery_already_terminal",
            "this idempotency key already completed a different terminal phase",
            Some(json!({"requested_phase":phase,"terminal_outcome":terminal.outcome})),
        ));
    }
    if terminal.outcome == PauseRecoveryTerminalOutcome::Finalized {
        verify_replayed_final_input(input, terminal.final_evidence.as_ref())?;
    }
    // A timeout may have happened after receipt publication but before pause
    // removal. Reconcile only the exact same owner generation.
    if let Some(record) = load_one(&context.repository_path, binding.caravan_id)? {
        let recovery = record.recovery.as_ref().ok_or_else(|| {
            AppError::validation(
                "pause_recovery_terminal_conflict",
                "terminal receipt exists but the remaining pause has no matching recovery owner",
            )
        })?;
        require_same_binding(binding, recovery)?;
        remove_record(&context.repository_path, binding.caravan_id)?;
    }
    Ok(recovery_output(
        binding,
        phase,
        status,
        false,
        terminal.receipt_id,
        PauseRecoveryFenceState::Released,
        rollback_state,
        next,
        terminal.final_evidence.as_ref(),
    ))
}

fn verify_replayed_final_input(
    input: &PauseRecoveryInput,
    evidence: Option<&PauseRecoveryFinalEvidence>,
) -> Result<(), AppError> {
    let evidence = evidence.ok_or_else(|| {
        AppError::validation(
            "pause_recovery_terminal_invalid",
            "finalized terminal receipt omitted final evidence",
        )
    })?;
    let supplied_attribution = input
        .check_attribution
        .as_ref()
        .ok_or_else(|| final_field_required("check_attribution"))?;
    let supplied_tree = input
        .virtual_merge_tree
        .as_deref()
        .ok_or_else(|| final_field_required("virtual_merge_tree"))?;
    let supplied_parents = input
        .virtual_merge_parents
        .iter()
        .map(|oid| recovery_oid("virtual_merge_parents", oid))
        .collect::<Result<Vec<_>, _>>()?;
    if evidence.virtual_merge.tree_oid != recovery_oid("virtual_merge_tree", supplied_tree)?
        || evidence.virtual_merge.parents != supplied_parents
        || &evidence.check_attribution != supplied_attribution
    {
        return recovery_mismatch(
            "terminal.final_evidence",
            json!({
                "virtual_merge_tree":supplied_tree,
                "virtual_merge_parents":input.virtual_merge_parents,
                "check_attribution":supplied_attribution,
            }),
            json!(evidence),
        );
    }
    Ok(())
}

fn require_same_binding(
    expected: &PauseRecoveryBinding,
    actual: &PauseRecoveryRecord,
) -> Result<(), AppError> {
    if actual.version != 1 || actual.binding != *expected {
        return recovery_mismatch("recovery.binding", json!(expected), json!(actual));
    }
    Ok(())
}

fn require_checkpoint(
    actual: PauseRecoveryCheckpoint,
    required: PauseRecoveryCheckpoint,
    required_phase: &str,
) -> Result<(), AppError> {
    if checkpoint_rank(actual) < checkpoint_rank(required) {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "pause_recovery_phase_out_of_order",
            format!("{required_phase} must be durably acknowledged first"),
            Some(json!({"actual_checkpoint":actual,"required_checkpoint":required})),
        ));
    }
    Ok(())
}

const fn checkpoint_rank(checkpoint: PauseRecoveryCheckpoint) -> u8 {
    match checkpoint {
        PauseRecoveryCheckpoint::Prepared => 0,
        PauseRecoveryCheckpoint::BaseCheckpointed => 1,
        PauseRecoveryCheckpoint::HeadCheckpointed => 2,
    }
}

const fn checkpoint_status(checkpoint: PauseRecoveryCheckpoint) -> PauseRecoveryStatus {
    match checkpoint {
        PauseRecoveryCheckpoint::Prepared => PauseRecoveryStatus::Prepared,
        PauseRecoveryCheckpoint::BaseCheckpointed => PauseRecoveryStatus::BaseCheckpointed,
        PauseRecoveryCheckpoint::HeadCheckpointed => PauseRecoveryStatus::HeadCheckpointed,
    }
}

const fn checkpoint_next(checkpoint: PauseRecoveryCheckpoint) -> &'static str {
    match checkpoint {
        PauseRecoveryCheckpoint::Prepared => {
            "perform the exact provider base transition, reread it, then checkpoint-base"
        }
        PauseRecoveryCheckpoint::BaseCheckpointed => {
            "perform the exact reverse-lease head replacement, reread it, then checkpoint-head"
        }
        PauseRecoveryCheckpoint::HeadCheckpointed => {
            "supply exact virtual merge and check attribution to finalize, or restore old leases and rollback"
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn recovery_output(
    binding: &PauseRecoveryBinding,
    phase: PauseRecoveryPhase,
    status: PauseRecoveryStatus,
    operation_changed: bool,
    receipt_id: String,
    fence_state: PauseRecoveryFenceState,
    rollback_state: PauseRecoveryRollbackState,
    next_action: &str,
    final_evidence: Option<&PauseRecoveryFinalEvidence>,
) -> PauseRecoveryOutput {
    let virtual_merge_parents = final_evidence
        .as_ref()
        .map(|evidence| evidence.virtual_merge.parents.clone());
    let virtual_merge_tree = final_evidence
        .as_ref()
        .map(|evidence| evidence.virtual_merge.tree_oid.clone());
    let check_attribution = final_evidence
        .as_ref()
        .map(|evidence| evidence.check_attribution.clone());
    PauseRecoveryOutput {
        schema_version: 1,
        phase,
        status,
        provider_mutated: false,
        operation_changed,
        receipt_id,
        next_action: next_action.to_owned(),
        operation_id: binding.operation_id.clone(),
        external_reference: binding.external_reference.clone(),
        idempotency_key: binding.idempotency_key.clone(),
        actor: binding.actor.clone(),
        owner_project: binding.owner_project.clone(),
        owner_agent: binding.owner_agent.clone(),
        ownership_generation: binding.ownership_generation.clone(),
        repository: binding.repository.clone(),
        caravan_id: binding.caravan_id,
        members: binding.members.clone(),
        pause_id: binding.pause_id.clone(),
        pause_generation: binding.pause_generation.clone(),
        target_pr: binding.target_pr,
        expected_base_ref: binding.expected_base_ref.clone(),
        expected_base_oid: binding.expected_base_oid.clone(),
        expected_head_oid: binding.expected_head_oid.clone(),
        desired_base_ref: binding.desired_base_ref.clone(),
        desired_base_oid: binding.desired_base_oid.clone(),
        desired_head_oid: binding.desired_head_oid.clone(),
        desired_head_tree: binding.desired_head_tree.clone(),
        fence_state,
        rollback_state,
        virtual_merge_parents,
        virtual_merge_tree,
        check_attribution,
    }
}

fn recovery_oid(name: &str, value: &str) -> Result<CommitOid, AppError> {
    if !(40..=64).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::validation(
            "pause_recovery_oid_invalid",
            format!("{name} must be a complete 40-64 character hexadecimal Git OID"),
        ));
    }
    Ok(CommitOid(value.to_ascii_lowercase()))
}

fn validate_recovery_text(name: &str, value: &str) -> Result<(), AppError> {
    if value.trim().is_empty() || value.len() > MAX_TEXT || value.contains(['\n', '\r']) {
        return Err(AppError::validation(
            "pause_recovery_metadata_invalid",
            format!("{name} must be non-empty, single-line, and at most {MAX_TEXT} bytes"),
        ));
    }
    Ok(())
}

fn final_field_required(name: &str) -> AppError {
    AppError::validation(
        "pause_recovery_final_evidence_required",
        format!("finalize requires {name}"),
    )
}

#[allow(clippy::needless_pass_by_value)]
fn recovery_mismatch<T>(
    field: &str,
    expected: serde_json::Value,
    actual: serde_json::Value,
) -> Result<T, AppError> {
    Err(AppError::structured(
        ErrorCategory::Validation,
        "pause_recovery_exact_mismatch",
        format!("exact-owner recovery mismatch at {field}"),
        Some(json!({
            "field":field,
            "expected":expected,
            "actual":actual,
            "next":"leave the fence active; rediscover and use the exact matching owner generation or restore old provider state",
        })),
    ))
}

fn recovery_provider_error(stage: &str, error: &MutationError) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "pause_recovery_provider_read_failed",
        format!("pause-recovery provider read failed at {stage}: {error}"),
        Some(
            json!({"stage":stage,"provider_mutated":false,"next":"leave the fence active and retry exact rediscovery"}),
        ),
    )
}

fn terminal_directory(repository: &Path) -> Result<PathBuf, AppError> {
    Ok(pause_directory(repository)?
        .parent()
        .expect("caravan metadata parent")
        .join("pause-recoveries"))
}

fn terminal_path(repository: &Path, binding: &PauseRecoveryBinding) -> Result<PathBuf, AppError> {
    let digest = Sha256::digest(binding.idempotency_key.as_bytes());
    Ok(terminal_directory(repository)?.join(format!("{digest:x}.json")))
}

fn load_terminal_receipt(
    repository: &Path,
    binding: &PauseRecoveryBinding,
) -> Result<Option<PauseRecoveryTerminalReceipt>, AppError> {
    let path = terminal_path(repository, binding)?;
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(storage_error(
                "pause_recovery_receipt_read_failed",
                "could not inspect terminal recovery receipt",
                &path,
                Some(&error),
            ));
        }
    };
    if metadata.len() > MAX_FILE_BYTES {
        return Err(storage_error(
            "pause_recovery_receipt_too_large",
            "terminal recovery receipt exceeds its safe bound",
            &path,
            None,
        ));
    }
    let bytes = fs::read(&path).map_err(|error| {
        storage_error(
            "pause_recovery_receipt_read_failed",
            "could not read terminal recovery receipt",
            &path,
            Some(&error),
        )
    })?;
    let receipt: PauseRecoveryTerminalReceipt =
        serde_json::from_slice(&bytes).map_err(|error| {
            storage_error(
                "pause_recovery_receipt_invalid",
                &format!("terminal recovery receipt is invalid: {error}"),
                &path,
                None,
            )
        })?;
    if receipt.binding.idempotency_key != binding.idempotency_key {
        return recovery_mismatch(
            "terminal.idempotency_key",
            json!(binding.idempotency_key),
            json!(receipt.binding.idempotency_key),
        );
    }
    Ok(Some(receipt))
}

fn write_terminal_receipt(
    repository: &Path,
    receipt: &PauseRecoveryTerminalReceipt,
) -> Result<(), AppError> {
    let path = terminal_path(repository, &receipt.binding)?;
    fs::create_dir_all(path.parent().expect("terminal receipt parent")).map_err(|error| {
        storage_error(
            "pause_recovery_receipt_write_failed",
            "could not create terminal recovery receipt directory",
            &path,
            Some(&error),
        )
    })?;
    let bytes = serde_json::to_vec_pretty(receipt).map_err(|error| {
        storage_error(
            "pause_recovery_receipt_write_failed",
            &error.to_string(),
            &path,
            None,
        )
    })?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(AppError::validation(
            "pause_recovery_receipt_too_large",
            "terminal recovery receipt exceeds its 64 KiB non-secret evidence bound",
        ));
    }
    if let Ok(existing) = fs::read(&path) {
        if existing == bytes {
            return Ok(());
        }
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "pause_recovery_idempotency_conflict",
            "idempotency key already names a different terminal receipt",
            Some(json!({"path":path})),
        ));
    }
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .map_err(|error| {
            storage_error(
                "pause_recovery_receipt_write_failed",
                "could not create terminal recovery receipt temporary",
                &temp,
                Some(&error),
            )
        })?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            storage_error(
                "pause_recovery_receipt_write_failed",
                "could not persist terminal recovery receipt",
                &temp,
                Some(&error),
            )
        })?;
    fs::rename(&temp, &path).map_err(|error| {
        storage_error(
            "pause_recovery_receipt_write_failed",
            "could not publish terminal recovery receipt",
            &path,
            Some(&error),
        )
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Load and reconcile all holds into status. Only the one intentionally absent
/// head auto-merge invariant is removed; every other graph problem remains.
pub fn apply_to_status(repository_path: &Path, status: &mut StatusOutput) -> Result<(), AppError> {
    let records = load_all(repository_path)?;
    let mut reports = records
        .into_iter()
        .map(|record| classify(status, record))
        .collect::<Vec<_>>();
    for report in &reports {
        if report.state.is_effective() && report.auto_merge_suspended {
            let head = report.record.caravan_head;
            status.analysis.fleet.problems.retain(|problem| {
                !(problem.kind == crate::model::GraphProblemKind::AutoMergeInvariant
                    && problem.prs == vec![head])
            });
        }
    }
    reports.sort_by_key(|report| report.record.caravan_head);
    status.pauses = reports;
    Ok(())
}

fn classify(status: &StatusOutput, record: PauseRecord) -> PauseStatus {
    let current = status.analysis.pull_requests.get(&record.caravan_head);
    let caravan = status.analysis.fleet.caravan(record.caravan_head);
    // Provider truth dominates every durable local record. A merged or closed
    // head is history, so it must never be resumable, never suspend an
    // invariant, and never request an auto-merge repair on an unmergeable PR.
    let retired_state = current
        .map(|pr| pr.state)
        .filter(|state| *state != PullRequestState::Open);
    let facts_match = current.is_some_and(|pr| {
        let actual = PullRequestPrecondition::from(pr);
        actual.number == record.expected_head.number
            && actual.state == PullRequestState::Open
            && actual.head_oid == record.expected_head.head_oid
            && actual.base_ref == record.expected_head.base_ref
            && actual.base_oid == record.expected_head.base_oid
            && actual.labels == record.expected_head.labels
            && (actual.auto_merge == AutoMergeState::disabled()
                || (actual.auto_merge == AutoMergeState::squash()
                    && record.resume_authorized_by.is_some()))
    });
    let topology_match = caravan.is_some_and(|item| item.members == record.members);
    let stale = !facts_match || !topology_match;
    let expired = record
        .expires_unix_secs
        .is_some_and(|expiry| now() >= expiry);
    let recovery_match = record.recovery.as_ref().is_some_and(|recovery| {
        recovery.binding.caravan_id == record.caravan_head
            && recovery.binding.members == record.members
            && status.repository.slug() == recovery.binding.repository
            && current.is_some_and(|head| head.state == PullRequestState::Open)
            && status
                .analysis
                .pull_requests
                .get(&recovery.binding.target_pr)
                .is_some_and(|target| {
                    target.state == PullRequestState::Open
                        && [
                            (
                                &recovery.binding.expected_base_ref,
                                &recovery.binding.expected_base_oid,
                                &recovery.binding.expected_head_oid,
                            ),
                            (
                                &recovery.binding.desired_base_ref,
                                &recovery.binding.desired_base_oid,
                                &recovery.binding.expected_head_oid,
                            ),
                            (
                                &recovery.binding.desired_base_ref,
                                &recovery.binding.desired_base_oid,
                                &recovery.binding.desired_head_oid,
                            ),
                        ]
                        .iter()
                        .any(|(base_ref, base_oid, head_oid)| {
                            target.base.name == **base_ref
                                && target.base.oid == **base_oid
                                && target.head.oid == **head_oid
                        })
                })
    });
    let state = if retired_state.is_some() {
        PauseState::Retired
    } else if record.recovery.is_some() {
        if recovery_match {
            PauseState::Recovering
        } else {
            PauseState::RecoveryDrift
        }
    } else if stale {
        PauseState::Stale
    } else if expired {
        PauseState::Expired
    } else {
        PauseState::Active
    };
    let suspended = retired_state.is_none() && current.is_some_and(|pr| !pr.auto_merge.enabled);
    let safe_next_action = match state {
        PauseState::Retired => {
            "provider truth retired this head; keep the record as history and never resume or repair auto-merge on it"
        }
        PauseState::Stale => {
            "facts changed: inspect and repair without resuming; stale holds fail closed"
        }
        PauseState::Recovering => {
            "exact-owner recovery is fenced; use only the matching checkpoint, finalize, or rollback phase"
        }
        PauseState::RecoveryDrift => {
            "recovery provider facts drifted; the fence remains active until exact finalize or rollback reconciliation"
        }
        PauseState::Expired => {
            "expiry never resumes automatically; explicitly resume after revalidation or replace the hold"
        }
        PauseState::Active => "after recovery, explicitly run `cara resume` as an audited action",
    }
    .to_owned();
    PauseStatus {
        record,
        state,
        auto_merge_suspended: suspended,
        retired_state,
        safe_next_action,
    }
}

fn reject_other_graph_problems(
    status: &StatusOutput,
    members: &[PrNumber],
    allow_head_auto: bool,
) -> Result<(), AppError> {
    if let Some(problem) = status.analysis.fleet.problems.iter().find(|problem| {
        !(allow_head_auto
            && problem.kind == crate::model::GraphProblemKind::AutoMergeInvariant
            && problem.prs == members.first().copied().into_iter().collect::<Vec<_>>())
    }) {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "pause_unsafe_graph",
            "a pause cannot hide structural, state, label, compatibility, or non-head auto-merge failures",
            Some(
                json!({"problem": problem, "next": "repair the reported graph problem before pausing or resuming"}),
            ),
        ));
    }
    Ok(())
}

fn pause_directory(repository: &Path) -> Result<PathBuf, AppError> {
    let output = ProcessRunner::in_directory(repository)
        .run(&CommandSpec::new("git").args([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ]))
        .map_err(|error| {
            AppError::structured(
                ErrorCategory::ExecutionFailure,
                "pause_storage_discovery_failed",
                error.to_string(),
                None,
            )
        })?;
    if !output.is_success() {
        return Err(AppError::structured(
            ErrorCategory::TargetNotFound,
            "git_repository_not_found",
            "pause state requires a Git repository",
            Some(json!({"stderr": output.stderr})),
        ));
    }
    Ok(PathBuf::from(output.stdout.trim())
        .join("caravan")
        .join("pauses"))
}
fn record_path(repository: &Path, head: PrNumber) -> Result<PathBuf, AppError> {
    Ok(pause_directory(repository)?.join(format!("{}.json", head.0)))
}
fn load_one(repository: &Path, head: PrNumber) -> Result<Option<PauseRecord>, AppError> {
    let path = record_path(repository, head)?;
    match fs::metadata(&path) {
        Ok(metadata) if metadata.len() > MAX_FILE_BYTES => {
            return Err(storage_error(
                "pause_record_too_large",
                "pause record exceeds its safe bound",
                &path,
                None,
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(storage_error(
                "pause_record_read_failed",
                "could not inspect pause record",
                &path,
                Some(&error),
            ));
        }
        _ => {}
    }
    let bytes = fs::read(&path).map_err(|error| {
        storage_error(
            "pause_record_read_failed",
            "could not read pause record",
            &path,
            Some(&error),
        )
    })?;
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        storage_error(
            "pause_record_invalid",
            &format!("pause record is invalid: {error}"),
            &path,
            None,
        )
    })
}
fn load_all(repository: &Path) -> Result<Vec<PauseRecord>, AppError> {
    let directory = pause_directory(repository)?;
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(storage_error(
                "pause_inventory_failed",
                "could not list pause records",
                &directory,
                Some(&error),
            ));
        }
    };
    let mut records = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            storage_error(
                "pause_inventory_failed",
                "could not inspect pause record",
                &directory,
                Some(&error),
            )
        })?;
        if entry.path().extension().and_then(|v| v.to_str()) == Some("json") {
            let stem = entry
                .path()
                .file_stem()
                .and_then(|v| v.to_str())
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| {
                    storage_error(
                        "pause_record_invalid",
                        "pause filename is not a PR number",
                        &entry.path(),
                        None,
                    )
                })?;
            if let Some(record) = load_one(repository, PrNumber(stem))? {
                records.push(record);
            }
        }
    }
    Ok(records)
}
fn write_record(repository: &Path, record: &PauseRecord) -> Result<(), AppError> {
    validate_record_size(record)?;
    let path = record_path(repository, record.caravan_head)?;
    fs::create_dir_all(path.parent().expect("pause parent")).map_err(|e| {
        storage_error(
            "pause_record_write_failed",
            "could not create pause directory",
            &path,
            Some(&e),
        )
    })?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| storage_error("pause_record_write_failed", &e.to_string(), &path, None))?;
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .map_err(|e| {
            storage_error(
                "pause_record_write_failed",
                "could not create temporary pause record",
                &temp,
                Some(&e),
            )
        })?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| {
            storage_error(
                "pause_record_write_failed",
                "could not persist pause record",
                &temp,
                Some(&e),
            )
        })?;
    fs::rename(&temp, &path).map_err(|e| {
        storage_error(
            "pause_record_write_failed",
            "could not publish pause record",
            &path,
            Some(&e),
        )
    })
}
fn remove_record(repository: &Path, head: PrNumber) -> Result<(), AppError> {
    let path = record_path(repository, head)?;
    fs::remove_file(&path).map_err(|e| {
        storage_error(
            "pause_record_remove_failed",
            "auto-merge is restored but the hold record could not be removed; rerun resume",
            &path,
            Some(&e),
        )
    })
}
fn append_audit(
    repository: &Path,
    action: &str,
    record: &PauseRecord,
    actor: &str,
) -> Result<(), AppError> {
    let path = pause_directory(repository)?
        .parent()
        .expect("caravan dir")
        .join("pause-audit.jsonl");
    fs::create_dir_all(path.parent().expect("audit parent")).map_err(|e| {
        storage_error(
            "pause_audit_failed",
            "could not create audit directory",
            &path,
            Some(&e),
        )
    })?;
    let line = serde_json::to_string(&json!({"version":1,"action":action,"head":record.caravan_head,"actor":actor,"timestamp":now(),"reference":record.external_reference})).map_err(|e| storage_error("pause_audit_failed", &e.to_string(), &path, None))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| {
            storage_error(
                "pause_audit_failed",
                "could not open pause audit",
                &path,
                Some(&e),
            )
        })?;
    writeln!(file, "{line}")
        .and_then(|()| file.sync_all())
        .map_err(|e| {
            storage_error(
                "pause_audit_failed",
                "could not persist pause audit",
                &path,
                Some(&e),
            )
        })
}
fn validate_record_size(record: &PauseRecord) -> Result<(), AppError> {
    let encoded = serde_json::to_vec(record).map_err(|error| {
        AppError::structured(
            ErrorCategory::SerializationError,
            "pause_record_encode_failed",
            error.to_string(),
            None,
        )
    })?;
    if encoded.len() as u64 > MAX_FILE_BYTES {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "pause_metadata_too_large",
            "pause metadata exceeds its 64 KiB non-secret evidence bound",
            Some(
                json!({"bytes": encoded.len(), "max_bytes": MAX_FILE_BYTES, "next": "reduce provider check evidence before retrying; no hold mutation was authorized"}),
            ),
        ));
    }
    Ok(())
}

fn validate_text(name: &str, value: &str) -> Result<(), AppError> {
    if value.trim().is_empty() || value.len() > MAX_TEXT || value.contains(['\n', '\r']) {
        Err(AppError::validation(
            "invalid_pause_metadata",
            format!("{name} must be non-empty, single-line, and at most {MAX_TEXT} bytes"),
        ))
    } else {
        Ok(())
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
fn step(
    kind: MutationKind,
    state: MutationStepState,
    head: PrNumber,
    summary: &str,
) -> MutationStep {
    MutationStep {
        kind,
        state,
        pr: Some(head),
        summary: summary.to_owned(),
    }
}
fn receipt(operation: &str, steps: Vec<MutationStep>) -> OperationReceipt {
    OperationReceipt {
        operation_id: OperationId::new(),
        operation: operation.to_owned(),
        changed: steps
            .iter()
            .any(|s| s.state == MutationStepState::Completed),
        completed_steps: steps,
    }
}
fn noop(operation: &str, record: PauseRecord, next: &str) -> PauseOutput {
    PauseOutput {
        receipt: receipt(
            operation,
            vec![step(
                if operation == "pause" {
                    MutationKind::DisableAutoMerge
                } else {
                    MutationKind::EnableAutoMerge
                },
                MutationStepState::AlreadySatisfied,
                record.caravan_head,
                "requested hold state already satisfied",
            )],
        ),
        pause: record,
        provider_receipts: Vec::new(),
        next: next.to_owned(),
    }
}
fn stale_error(action: &str, report: &PauseStatus) -> AppError {
    AppError::structured(
        ErrorCategory::Validation,
        "stale_pause_facts",
        format!(
            "cannot {action} caravan #{} because recorded head, base, labels, checks, state, or topology changed",
            report.record.caravan_head
        ),
        Some(
            json!({"pause":report,"next":"inspect status and repair changed facts; never overwrite or auto-resume a stale hold"}),
        ),
    )
}
fn mutation_error(action: &str, error: &MutationError) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        if matches!(error, MutationError::StalePrecondition { .. }) {
            "stale_precondition"
        } else {
            "pause_mutation_failed"
        },
        format!("{action} failed: {error}"),
        Some(
            json!({"next":format!("rediscover and retry `cara {action}`; exact preconditions prevented an overwrite")}),
        ),
    )
}
fn storage_error(
    code: &str,
    message: &str,
    path: &Path,
    error: Option<&std::io::Error>,
) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        code,
        message,
        Some(
            json!({"path":path,"io_error":error.map(ToString::to_string),"next":"repair local Git metadata storage; do not bypass the hold"}),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphAnalysis;
    use crate::model::{
        BranchSnapshot, Caravan, CaravanFleet, CheckState, CommitOid, PullRequestSnapshot,
    };
    use std::collections::{BTreeMap, BTreeSet};

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "o".to_owned(),
            name: "r".to_owned(),
        }
    }
    fn branch(name: &str, oid: &str) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository(),
            name: name.to_owned(),
            oid: CommitOid(oid.to_owned()),
        }
    }
    fn head(auto_merge: AutoMergeState, oid: &str, check: CheckState) -> PullRequestSnapshot {
        PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(1),
            title: "head".to_owned(),
            url: "https://invalid/1".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch("pr-1", oid),
            base: branch("main", "base"),
            cross_repository: false,
            labels: BTreeSet::from(["caravan".to_owned()]),
            auto_merge,
            checks: vec![CheckSnapshot {
                name: "CI".to_owned(),
                state: check,
                provider_state: None,
                details_url: None,
                ..crate::model::CheckSnapshot::default()
            }],
            created_at: None,
            merged_at: None,
            updated_at: None,
        }
    }
    fn status(pr: PullRequestSnapshot) -> StatusOutput {
        StatusOutput {
            config_provenance: None,
            head_merge: crate::read::HeadMergeStatus::default(),
            runtime: crate::read::RuntimeProvenance::default(),
            provider_api: crate::model::GitHubApiTelemetry::default(),
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            repository: repository(),
            rebase_on_join: crate::read::RebaseOnJoinStatus::default(),
            stack_backend: crate::read::StackBackendStatus::default(),
            auto_admission: crate::read::AutoAdmissionStatus::default(),
            default_branch: "main".to_owned(),
            current_branch: Some("pr-1".to_owned()),
            current_pr: Some(PrNumber(1)),
            healthy: true,
            initialization: crate::initialization::InitializationStatus::default(),
            analysis: GraphAnalysis {
                fleet: CaravanFleet {
                    repository: repository(),
                    default_branch: branch("main", "base"),
                    caravans: vec![Caravan::new(vec![PrNumber(1), PrNumber(2)]).unwrap()],
                    unqueued: Vec::new(),
                    problems: Vec::new(),
                    history: crate::model::CaravanHistory::default(),
                },
                pull_requests: BTreeMap::from([(PrNumber(1), pr)]),
                compatibility: Vec::new(),
                cumulative_trees: Vec::new(),
                squash_reconciliations: Vec::new(),
            },
            pauses: Vec::new(),
            timing: None,
            admission: crate::read::AdmissionStatus {
                policy: String::new(),
                priority_labels: Vec::new(),
                generation_integrity: crate::generation::GenerationIntegrityStatus::default(),
                candidates: Vec::new(),
                skipped: Vec::new(),
                rejected: Vec::new(),
                next_candidate: None,
            },
            sync_budget: crate::sync::SyncBudgetStatus::default(),
        }
    }
    fn record(pr: &PullRequestSnapshot) -> PauseRecord {
        let mut expected = PullRequestPrecondition::from(pr);
        expected.auto_merge = AutoMergeState::squash();
        PauseRecord {
            version: 1,
            caravan_head: PrNumber(1),
            members: vec![PrNumber(1), PrNumber(2)],
            expected_head: expected,
            expected_checks: pr.checks.clone(),
            actor: "oncall".to_owned(),
            reason: "incident".to_owned(),
            paused_unix_secs: now(),
            expires_unix_secs: None,
            external_reference: Some("INC-1".to_owned()),
            resume_authorized_by: None,
            recovery: None,
        }
    }

    fn oid(byte: char) -> String {
        std::iter::repeat_n(byte, 40).collect()
    }

    fn recovery_input(
        target_pr: u64,
        expected_base_ref: &str,
        expected_base_oid: &str,
        expected_head_oid: &str,
    ) -> PauseRecoveryInput {
        PauseRecoveryInput {
            schema_version: 1,
            operation_id: "owned-pr-retarget-head".to_owned(),
            external_reference: "INC-1".to_owned(),
            idempotency_key: "recovery-idempotency-1".to_owned(),
            actor: "oncall".to_owned(),
            owner_project: "cacophony".to_owned(),
            owner_agent: "owner-agent".to_owned(),
            ownership_generation: "generation-1".to_owned(),
            repository: "o/r".to_owned(),
            caravan_id: 1,
            members: vec![1, 2],
            pause_id: "pause-operation-1".to_owned(),
            pause_generation: "pause-generation-1".to_owned(),
            target_pr,
            expected_base_ref: expected_base_ref.to_owned(),
            expected_base_oid: expected_base_oid.to_owned(),
            expected_head_oid: expected_head_oid.to_owned(),
            desired_base_ref: "desired-base".to_owned(),
            desired_base_oid: oid('c'),
            desired_head_oid: oid('d'),
            desired_head_tree: oid('e'),
            virtual_merge_parents: Vec::new(),
            virtual_merge_tree: None,
            check_attribution: None,
            reason: "reviewed recovery".to_owned(),
        }
    }

    fn child(head_oid: &str, base_oid: &str) -> PullRequestSnapshot {
        let mut child = head(AutoMergeState::disabled(), head_oid, CheckState::Success);
        child.number = PrNumber(2);
        child.title = "child".to_owned();
        child.url = "https://invalid/2".to_owned();
        child.head = branch("pr-2", head_oid);
        child.base = branch("pr-1", base_oid);
        child
    }

    fn recovery_record(
        mut pause: PauseRecord,
        binding: PauseRecoveryBinding,
        checkpoint: PauseRecoveryCheckpoint,
    ) -> PauseRecord {
        pause.recovery = Some(PauseRecoveryRecord {
            version: 1,
            binding,
            checkpoint,
            receipt_id: "receipt-1".to_owned(),
            prepared_unix_ms: 1,
            updated_unix_ms: 1,
        });
        pause
    }

    #[test]
    fn target_as_head_and_target_as_child_prepare_bind_exact_topology() {
        let old_head = oid('a');
        let old_base = oid('b');
        let mut root = head(AutoMergeState::disabled(), &old_head, CheckState::Success);
        root.base = branch("main", &old_base);
        let root_pause = record(&root);
        let root_status = status(root.clone());
        let root_input = recovery_input(1, "main", &old_base, &old_head);
        let root_binding = recovery_binding(&root_input).unwrap();
        verify_prepare(&root_status, &root_pause, &root_binding)
            .expect("the paused head is an exact recovery target");

        let child_head = oid('f');
        let mut child_status = status(root.clone());
        child_status
            .analysis
            .pull_requests
            .insert(PrNumber(2), child(&child_head, &old_head));
        let child_input = recovery_input(2, "pr-1", &old_head, &child_head);
        let child_binding = recovery_binding(&child_input).unwrap();
        verify_prepare(&child_status, &record(&root), &child_binding)
            .expect("a paused caravan child is an exact recovery target");
    }

    #[test]
    fn foreign_pause_owner_generation_cannot_replay_or_checkpoint() {
        let input = recovery_input(1, "main", &oid('b'), &oid('a'));
        let binding = recovery_binding(&input).unwrap();
        let pause = record(&head(
            AutoMergeState::disabled(),
            &oid('a'),
            CheckState::Success,
        ));
        let recovery = recovery_record(pause, binding, PauseRecoveryCheckpoint::Prepared)
            .recovery
            .unwrap();
        let mut foreign = input;
        foreign.owner_agent = "foreign-agent".to_owned();
        let error = require_same_binding(&recovery_binding(&foreign).unwrap(), &recovery)
            .expect_err("foreign owner must be refused");
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "pause_recovery_exact_mismatch"
        );
    }

    #[test]
    fn provider_drift_keeps_exact_recovery_fenced() {
        let old_head = oid('a');
        let old_base = oid('b');
        let mut root = head(AutoMergeState::disabled(), &old_head, CheckState::Success);
        root.base = branch("main", &old_base);
        let input = recovery_input(1, "main", &old_base, &old_head);
        let binding = recovery_binding(&input).unwrap();
        let pause = recovery_record(
            record(&root),
            binding,
            PauseRecoveryCheckpoint::BaseCheckpointed,
        );
        let mut drifted = status(root);
        drifted
            .analysis
            .pull_requests
            .get_mut(&PrNumber(1))
            .unwrap()
            .head
            .oid = CommitOid(oid('9'));

        let report = classify(&drifted, pause);

        assert_eq!(report.state, PauseState::RecoveryDrift);
        assert!(report.state.is_effective());
        assert!(report.safe_next_action.contains("fence remains active"));
    }

    #[test]
    fn finalization_requires_exact_virtual_merge_tree_parents_and_check_attribution() {
        let old_head = oid('a');
        let old_base = oid('b');
        let mut input = recovery_input(1, "main", &old_base, &old_head);
        input.virtual_merge_parents = vec![oid('c'), oid('d')];
        input.virtual_merge_tree = Some(oid('9'));
        input.check_attribution = Some(PauseRecoveryCheckAttribution {
            head_oid: CommitOid(oid('d')),
            check_run_count: 1,
            status_context_count: 0,
        });
        let binding = recovery_binding(&input).unwrap();
        let mut target = head(AutoMergeState::disabled(), &oid('d'), CheckState::Success);
        target.base = branch("desired-base", &oid('c'));
        target.checks[0].provider_kind = Some("CheckRun".to_owned());
        let mut final_status = status(target.clone());
        final_status
            .merge_candidates
            .push(crate::model::MergeCandidateIdentity {
                pr: PrNumber(1),
                provider_updated_at: "2026-08-04T00:00:00Z".to_owned(),
                observed_at: "2026-08-04T00:00:01Z".to_owned(),
                base: target.base.clone(),
                head: target.head.clone(),
                synthetic: Some(SyntheticMergeCandidate {
                    git_ref: "refs/pull/1/merge".to_owned(),
                    oid: CommitOid(oid('8')),
                    tree_oid: CommitOid(oid('9')),
                    parents: vec![CommitOid(oid('c')), CommitOid(oid('d'))],
                }),
                auto_merge: crate::model::NativeAutoMergeState {
                    enabled: false,
                    merge_method: None,
                    actor: None,
                },
                freshness: MergeCandidateFreshness::Fresh,
                compared_base: None,
                stale_base: false,
                stale_head: false,
                stale_reasons: Vec::new(),
            });
        let evidence = verify_final_evidence(
            &final_status,
            &input,
            &binding,
            GitCommitIdentity {
                oid: CommitOid(oid('d')),
                tree_oid: CommitOid(oid('e')),
                parents: vec![CommitOid(oid('c'))],
            },
        )
        .expect("exact final provider evidence verifies");
        assert_eq!(evidence.virtual_merge.tree_oid, CommitOid(oid('9')));
        assert_eq!(evidence.check_attribution.check_run_count, 1);
    }

    #[test]
    fn prepare_replay_reports_durable_checkpoint_without_provider_read() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
        let input = recovery_input(1, "main", &oid('b'), &oid('a'));
        let binding = recovery_binding(&input).unwrap();
        let pause = recovery_record(
            record(&head(
                AutoMergeState::disabled(),
                &oid('a'),
                CheckState::Success,
            )),
            binding,
            PauseRecoveryCheckpoint::BaseCheckpointed,
        );
        write_record(directory.path(), &pause).unwrap();
        let context = AppContext {
            repository_path: directory.path().to_path_buf(),
            config_path: directory.path().join("config.yaml"),
            config_existed: false,
            config: crate::config::CaravanConfig::default(),
        };

        let output = pause_recovery(&context, PauseRecoveryPhase::Prepare, &input).unwrap();

        assert_eq!(output.status, PauseRecoveryStatus::BaseCheckpointed);
        assert_eq!(output.fence_state, PauseRecoveryFenceState::Active);
        assert!(!output.operation_changed);
        assert!(!output.provider_mutated);
        let json = serde_json::to_value(&output).unwrap();
        assert_eq!(json["phase"], "prepare");
        assert_eq!(json["status"], "base_checkpointed");
        assert_eq!(json["operation_id"], "owned-pr-retarget-head");
        assert_eq!(json["repository"], "o/r");
        assert_eq!(json["fence_state"], "active");
        assert!(json.get("fence").is_none());
        assert!(json.get("check_attribution").is_none());
    }

    #[test]
    fn ordinary_resume_never_consumes_exact_recovery_authority() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
        let input = recovery_input(1, "main", &oid('b'), &oid('a'));
        let binding = recovery_binding(&input).unwrap();
        let pause = recovery_record(
            record(&head(
                AutoMergeState::disabled(),
                &oid('a'),
                CheckState::Success,
            )),
            binding,
            PauseRecoveryCheckpoint::Prepared,
        );
        write_record(directory.path(), &pause).unwrap();
        let context = AppContext {
            repository_path: directory.path().to_path_buf(),
            config_path: directory.path().join("config.yaml"),
            config_existed: false,
            config: crate::config::CaravanConfig::default(),
        };

        let error = resume(
            &context,
            &ResumeInput {
                head_pr: 1,
                actor: "oncall".to_owned(),
            },
        )
        .expect_err("ordinary resume must remain strict during recovery");

        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "pause_recovery_in_progress"
        );
        assert!(load_one(directory.path(), PrNumber(1)).unwrap().is_some());
    }

    #[test]
    fn timeout_replay_of_verified_rollback_is_exact_and_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
        let input = recovery_input(1, "main", &oid('b'), &oid('a'));
        let binding = recovery_binding(&input).unwrap();
        let pause = recovery_record(
            record(&head(
                AutoMergeState::disabled(),
                &oid('a'),
                CheckState::Success,
            )),
            binding.clone(),
            PauseRecoveryCheckpoint::HeadCheckpointed,
        );
        write_record(directory.path(), &pause).unwrap();
        write_terminal_receipt(
            directory.path(),
            &PauseRecoveryTerminalReceipt {
                version: 1,
                binding,
                outcome: PauseRecoveryTerminalOutcome::RolledBack,
                receipt_id: "receipt-1".to_owned(),
                completed_unix_ms: 2,
                final_evidence: None,
            },
        )
        .unwrap();
        let context = AppContext {
            repository_path: directory.path().to_path_buf(),
            config_path: directory.path().join("config.yaml"),
            config_existed: false,
            config: crate::config::CaravanConfig::default(),
        };

        let first = pause_recovery(&context, PauseRecoveryPhase::Rollback, &input).unwrap();
        let replay = pause_recovery(&context, PauseRecoveryPhase::Rollback, &input).unwrap();

        assert_eq!(first, replay);
        assert_eq!(first.status, PauseRecoveryStatus::RolledBack);
        assert_eq!(first.fence_state, PauseRecoveryFenceState::Released);
        assert_eq!(first.rollback_state, PauseRecoveryRollbackState::Completed);
        assert!(!first.operation_changed);
        assert!(load_one(directory.path(), PrNumber(1)).unwrap().is_none());
    }

    #[test]
    fn rollback_requires_exact_old_base_head_and_original_topology() {
        let old_head = oid('a');
        let old_base = oid('b');
        let mut root = head(AutoMergeState::disabled(), &old_head, CheckState::Success);
        root.base = branch("main", &old_base);
        let input = recovery_input(1, "main", &old_base, &old_head);
        let binding = recovery_binding(&input).unwrap();
        let old = status(root.clone());
        verify_target(&old, &binding, TargetState::Old).unwrap();
        verify_original_topology(&old, &binding).unwrap();

        let mut drifted = status(root);
        drifted
            .analysis
            .pull_requests
            .get_mut(&PrNumber(1))
            .unwrap()
            .base
            .oid = CommitOid(oid('7'));
        assert!(verify_target(&drifted, &binding, TargetState::Old).is_err());
    }

    #[test]
    fn status_suspends_only_head_auto_merge_and_preserves_structural_failures() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
        let pr = head(AutoMergeState::disabled(), "sha", CheckState::InProgress);
        let record = record(&pr);
        write_record(directory.path(), &record).unwrap();
        let mut output = status(pr);
        output.analysis.fleet.problems = vec![
            crate::model::GraphProblem {
                kind: crate::model::GraphProblemKind::AutoMergeInvariant,
                prs: vec![PrNumber(1)],
                message: "head auto-merge".to_owned(),
            },
            crate::model::GraphProblem {
                kind: crate::model::GraphProblemKind::Branching,
                prs: vec![PrNumber(1), PrNumber(2)],
                message: "branching".to_owned(),
            },
        ];

        apply_to_status(directory.path(), &mut output).unwrap();

        assert_eq!(output.pauses[0].state, PauseState::Active);
        assert_eq!(output.analysis.fleet.problems.len(), 1);
        assert_eq!(
            output.analysis.fleet.problems[0].kind,
            crate::model::GraphProblemKind::Branching
        );
    }

    #[test]
    fn merged_head_retires_the_hold_and_never_requests_auto_merge_repair() {
        let mut merged = head(AutoMergeState::disabled(), "sha", CheckState::Success);
        let record = record(&merged);
        merged.state = PullRequestState::Merged;
        merged.merged_at = Some("2026-07-25T01:07:18Z".to_owned());
        let mut output = status(merged);
        // A merged head cannot be an active caravan member, so provider truth
        // leaves the fleet empty even while the durable hold survives.
        output.analysis.fleet.caravans.clear();
        output.analysis.fleet.problems = vec![crate::model::GraphProblem {
            kind: crate::model::GraphProblemKind::AutoMergeInvariant,
            prs: vec![PrNumber(1)],
            message: "head auto-merge".to_owned(),
        }];

        let report = classify(&output, record);

        assert_eq!(report.state, PauseState::Retired);
        assert_eq!(report.retired_state, Some(PullRequestState::Merged));
        assert!(!report.auto_merge_suspended);
        assert!(!report.state.is_effective());
        assert!(report.safe_next_action.contains("never resume"));
    }

    #[test]
    fn incident_hold_and_ci_wait_remain_intentionally_paused() {
        let waiting = head(AutoMergeState::disabled(), "sha", CheckState::InProgress);
        let record = record(&waiting);
        let report = classify(&status(waiting), record);
        assert_eq!(report.state, PauseState::Active);
        assert!(report.auto_merge_suspended);
    }

    #[test]
    fn expiry_warns_without_silent_resume() {
        let pr = head(AutoMergeState::disabled(), "sha", CheckState::Success);
        let mut record = record(&pr);
        record.expires_unix_secs = Some(now().saturating_sub(1));
        let report = classify(&status(pr), record);
        assert_eq!(report.state, PauseState::Expired);
        assert!(report.auto_merge_suspended);
        assert!(report.safe_next_action.contains("never resumes"));
    }

    #[test]
    fn changed_head_fails_closed_but_ci_progress_does_not_stale_hold() {
        let waiting = head(AutoMergeState::disabled(), "sha", CheckState::InProgress);
        let record = record(&waiting);
        let passing = head(AutoMergeState::disabled(), "sha", CheckState::Success);
        assert_eq!(
            classify(&status(passing), record.clone()).state,
            PauseState::Active
        );
        let changed = head(AutoMergeState::disabled(), "different", CheckState::Success);
        assert_eq!(classify(&status(changed), record).state, PauseState::Stale);
    }

    #[test]
    fn external_reenable_is_stale_while_authorized_resume_retry_is_idempotent() {
        let disabled = head(AutoMergeState::disabled(), "sha", CheckState::Success);
        let mut record = record(&disabled);
        let enabled = head(AutoMergeState::squash(), "sha", CheckState::Success);
        assert_eq!(
            classify(&status(enabled.clone()), record.clone()).state,
            PauseState::Stale
        );
        record.resume_authorized_by = Some("oncall".to_owned());
        assert_eq!(classify(&status(enabled), record).state, PauseState::Active);
    }

    #[test]
    fn changed_labels_and_topology_cannot_be_hidden() {
        let pr = head(AutoMergeState::disabled(), "sha", CheckState::Success);
        let record = record(&pr);
        let mut unsafe_pr = pr;
        unsafe_pr.labels.insert("caravan-evicted".to_owned());
        assert_eq!(
            classify(&status(unsafe_pr), record.clone()).state,
            PauseState::Stale
        );
        let mut changed = status(head(AutoMergeState::disabled(), "sha", CheckState::Success));
        changed.analysis.fleet.caravans[0] = Caravan::new(vec![PrNumber(1)]).unwrap();
        assert_eq!(classify(&changed, record).state, PauseState::Stale);
    }
}
