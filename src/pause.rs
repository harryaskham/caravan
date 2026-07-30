//! Explicit, repository-scoped caravan incident holds.
//!
//! Holds live below Git's common directory so every linked worktree observes the
//! same state. Expiry is informational: only an explicit `resume` action may
//! remove a hold or re-enable auto-merge.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::command::{CommandRunner, CommandSpec, ProcessRunner};
use crate::github::{GitHubMutationAdapter, GitHubMutationReceipt, MutationError};
use crate::model::{
    AutoMergeState, CheckSnapshot, MutationKind, MutationStep, MutationStepState, OperationId,
    OperationReceipt, PrNumber, PullRequestPrecondition, PullRequestState, RepositoryId,
};
use crate::operation_lock::OperationLock;
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
}

/// How current live facts relate to a durable hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PauseState {
    Active,
    Expired,
    Stale,
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
        matches!(self, Self::Active | Self::Expired)
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
    let _lock = OperationLock::acquire(&context.repository_path, "pause")?;
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
    };
    validate_record_size(&record)?;
    let provider = GitHubMutationAdapter::new(
        ProcessRunner::in_directory(&context.repository_path).with_timeout(
            std::time::Duration::from_secs(context.config.command_timeout_secs),
        ),
    );
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
    let _lock = OperationLock::acquire(&context.repository_path, "resume")?;
    let head = PrNumber(input.head_pr);
    let Some(record) = load_one(&context.repository_path, head)? else {
        return Err(AppError::validation(
            "pause_not_found",
            format!("caravan #{head} has no durable pause to resume"),
        ));
    };
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
    let provider = GitHubMutationAdapter::new(
        ProcessRunner::in_directory(&context.repository_path).with_timeout(
            std::time::Duration::from_secs(context.config.command_timeout_secs),
        ),
    );
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
    let state = if retired_state.is_some() {
        PauseState::Retired
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
        }
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
