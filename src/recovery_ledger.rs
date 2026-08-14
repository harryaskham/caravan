//! Secret-free recovery handoff and bounded attempt ledger (bd-78c835).
//!
//! This module performs provider reads only while building a request and local
//! atomic state writes while recording it. It never invokes a model or mutates
//! GitHub; an external Pi/dormant-agent host consumes the typed handoff.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::model::{CommitOid, GitHubApiTelemetry, PrNumber, RepositoryId};
use crate::{AppContext, AppError};

const SCHEMA_VERSION: u32 = 1;
const MAX_INPUT_BYTES: u64 = 256 * 1024;
const MAX_ATTEMPTS: u32 = 10;
const MAX_BACKOFF_SECS: u64 = 3_600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, clap::Args)]
pub struct RecoveryRequestInput {
    /// Captured Cara JSON error envelope from the failed canonical operation.
    #[arg(long, value_name = "PATH")]
    pub error: PathBuf,
    /// Operator-owned local ledger path.
    #[arg(long, value_name = "PATH")]
    pub ledger: PathBuf,
    /// Maximum transient attempts for this exact generation/fingerprint.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=10))]
    pub max_attempts: u32,
    /// Deterministic initial exponential backoff.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub initial_backoff_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, clap::Args)]
pub struct RecoveryAttemptInput {
    #[arg(long, value_name = "PATH")]
    pub ledger: PathBuf,
    /// Secret-free typed attempt receipt JSON.
    #[arg(long, value_name = "PATH")]
    pub receipt: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, clap::Args)]
pub struct RecoveryLedgerInput {
    #[arg(long, value_name = "PATH")]
    pub ledger: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryDisposition {
    RetryTransient,
    ExternalDecision,
    OperatorAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryAllowedAction {
    Rediscover,
    RetrySync,
    AwaitCi,
    OwnerRepair {
        pr: PrNumber,
        expected_head: CommitOid,
    },
    OperatorDecision {
        code: String,
    },
    ExactPlan {
        operation: String,
        operation_id: String,
        plan_hash: String,
        owner_pr: PrNumber,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecoveryHelpCursor {
    pub cli: String,
    pub mcp_tool: String,
    pub schema_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecoveryRequest {
    pub schema_version: u32,
    pub request_id: String,
    pub repository: RepositoryId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrNumber>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<CommitOid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<CommitOid>,
    pub main: CommitOid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack_root_pr: Option<PrNumber>,
    pub check_generation: String,
    pub config_fingerprint: String,
    pub policy_fingerprint: String,
    pub wake_class: String,
    pub disposition: RecoveryDisposition,
    pub decision_fingerprint: String,
    pub diagnostic_codes: Vec<String>,
    pub links: Vec<String>,
    pub allowed_actions: Vec<RecoveryAllowedAction>,
    pub max_attempts: u32,
    pub initial_backoff_secs: u64,
    pub help: RecoveryHelpCursor,
    pub observed_unix_ms: u64,
    pub provider_api: GitHubApiTelemetry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAttemptOutcome {
    Acknowledged,
    TransientFailure,
    Refused,
    TerminalFailure,
    Success,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecoveryAttemptReceipt {
    pub schema_version: u32,
    pub request_id: String,
    pub repository: RepositoryId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrNumber>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<CommitOid>,
    pub main: CommitOid,
    pub decision_fingerprint: String,
    pub outcome: RecoveryAttemptOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_receipt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub landed_commit: Option<CommitOid>,
    pub provider_main_verified: bool,
    pub observed_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecoveryAttemptRecord {
    pub attempt: u32,
    pub outcome: RecoveryAttemptOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_receipt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub landed_commit: Option<CommitOid>,
    pub provider_main_verified: bool,
    pub observed_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_retry_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecoveryEscalation {
    pub kind: String,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exact_plan: Option<RecoveryAllowedAction>,
    pub emitted_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecoveryLedgerEntry {
    pub request: RecoveryRequest,
    pub attempts: Vec<RecoveryAttemptRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escalation: Option<RecoveryEscalation>,
    pub completed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecoveryLedger {
    pub schema_version: u32,
    pub entries: BTreeMap<String, RecoveryLedgerEntry>,
}

impl Default for RecoveryLedger {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecoveryRequestOutput {
    pub request: RecoveryRequest,
    pub ledger: RecoveryLedger,
    pub created: bool,
    pub mutated_provider: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecoveryAttemptOutput {
    pub entry: RecoveryLedgerEntry,
    pub duplicate: bool,
    pub mutated_provider: bool,
}

/// Build and persist one exact request from canonical Cara error plus a fresh
/// provider/main status reread. No provider mutation is available in this path.
#[allow(clippy::too_many_lines)] // Linear exact-generation construction keeps all bound facts visible.
pub fn record_request(
    context: &AppContext,
    input: &RecoveryRequestInput,
) -> Result<RecoveryRequestOutput, AppError> {
    validate_bounds(input.max_attempts, input.initial_backoff_secs)?;
    let error = read_json(&input.error, "recovery_error_input_invalid")?;
    let code = error
        .pointer("/error/code")
        .and_then(Value::as_str)
        .filter(|code| safe_code(code))
        .ok_or_else(|| {
            validation(
                "recovery_error_code_invalid",
                "Cara error code is missing/unsafe",
            )
        })?
        .to_owned();
    let details = error
        .pointer("/error/details")
        .cloned()
        .unwrap_or(Value::Null);
    let status = crate::read::status(context)?;
    let pr = extract_pr(&details).or(status.current_pr).or_else(|| {
        status
            .analysis
            .fleet
            .caravans
            .first()
            .map(|caravan| caravan.id)
    });
    let candidate = pr.and_then(|pr| status.analysis.pull_requests.get(&pr));
    let head = candidate.map(|pull| pull.head.oid.clone());
    let base = candidate.map(|pull| pull.base.oid.clone());
    let main = status.analysis.fleet.default_branch.oid.clone();
    let stack_root_pr = pr.and_then(|pr| {
        status
            .analysis
            .fleet
            .caravans
            .iter()
            .find(|caravan| caravan.members.contains(&pr))
            .map(|caravan| caravan.id)
    });
    let disposition = classify_disposition(&code, &details);
    let links = extract_links(&details, &status.repository);
    let allowed_actions = allowed_actions(disposition, pr, head.as_ref(), &details);
    let check_generation = digest_json(&candidate.map_or(Value::Null, |candidate| {
        serde_json::to_value(&candidate.checks).unwrap_or(Value::Null)
    }));
    let config_fingerprint =
        digest_json(&serde_json::to_value(&context.config).expect("validated config serializes"));
    let policy_fingerprint = digest_json(&serde_json::json!({
        "sync": context.config.sync,
        "stack_type": context.config.stack_type,
        "stack_rollout": context.config.stack_rollout,
        "writer": context.config.writer,
    }));
    let wake_class = details
        .pointer("/decision/wake_class")
        .or_else(|| details.get("wake_class"))
        .and_then(Value::as_str)
        .filter(|value| safe_code(value))
        .unwrap_or("unknown")
        .to_owned();
    let decision_fingerprint = digest_json(&serde_json::json!({
        "repository": status.repository,
        "pr": pr,
        "head": head,
        "base": base,
        "main": main,
        "stack_root_pr": stack_root_pr,
        "check_generation": check_generation,
        "config_fingerprint": config_fingerprint,
        "policy_fingerprint": policy_fingerprint,
        "wake_class": wake_class,
        "disposition": disposition,
        "code": code,
        "links": links,
        "allowed_actions": allowed_actions,
    }));
    let request_id = format!("recovery:{}", &decision_fingerprint[7..23]);
    let request = RecoveryRequest {
        schema_version: SCHEMA_VERSION,
        request_id: request_id.clone(),
        repository: status.repository,
        pr,
        head,
        base,
        main,
        stack_root_pr,
        check_generation,
        config_fingerprint,
        policy_fingerprint,
        wake_class,
        disposition,
        decision_fingerprint,
        diagnostic_codes: vec![code],
        links,
        allowed_actions,
        max_attempts: input.max_attempts,
        initial_backoff_secs: input.initial_backoff_secs,
        help: RecoveryHelpCursor {
            cli: "cara help --json".to_owned(),
            mcp_tool: "help".to_owned(),
            schema_version: 1,
        },
        observed_unix_ms: unix_ms(),
        provider_api: status.provider_api,
    };
    reject_secrets(&serde_json::to_value(&request).expect("request serializes"))?;
    let mut ledger = load_ledger(&input.ledger)?;
    let created = !ledger.entries.contains_key(&request_id);
    if let Some(existing) = ledger.entries.get(&request_id) {
        if existing.request != request {
            return Err(validation(
                "recovery_request_generation_collision",
                "request ID exists with different exact generation",
            ));
        }
    } else {
        ledger.entries.insert(
            request_id,
            RecoveryLedgerEntry {
                request: request.clone(),
                attempts: Vec::new(),
                escalation: None,
                completed: false,
            },
        );
        persist_ledger(&input.ledger, &ledger)?;
    }
    Ok(RecoveryRequestOutput {
        request,
        ledger,
        created,
        mutated_provider: false,
    })
}

pub fn record_attempt(
    context: Option<&AppContext>,
    input: &RecoveryAttemptInput,
) -> Result<RecoveryAttemptOutput, AppError> {
    let receipt: RecoveryAttemptReceipt =
        serde_json::from_value(read_json(&input.receipt, "recovery_attempt_input_invalid")?)
            .map_err(|_| {
                validation(
                    "recovery_attempt_input_invalid",
                    "attempt receipt schema invalid",
                )
            })?;
    reject_secrets(&serde_json::to_value(&receipt).expect("attempt serializes"))?;
    let mut ledger = load_ledger(&input.ledger)?;
    let entry = ledger
        .entries
        .get_mut(&receipt.request_id)
        .ok_or_else(|| validation("recovery_request_unknown", "request ID is not in ledger"))?;
    validate_receipt_generation(&entry.request, &receipt)?;
    if receipt.outcome == RecoveryAttemptOutcome::Success {
        verify_success(context, &entry.request, &receipt)?;
    }
    let duplicate = entry.attempts.iter().any(|attempt| {
        attempt.outcome == receipt.outcome
            && attempt.operation_receipt == receipt.operation_receipt
            && receipt.operation_receipt.is_some()
    });
    if !duplicate {
        if receipt.outcome == RecoveryAttemptOutcome::TransientFailure
            && entry.request.disposition == RecoveryDisposition::RetryTransient
            && entry.attempts.len() >= entry.request.max_attempts as usize
        {
            return Err(validation(
                "recovery_attempts_exhausted",
                "transient retry budget is exhausted for this exact generation",
            ));
        }
        let attempt = u32::try_from(entry.attempts.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        let retryable = receipt.outcome == RecoveryAttemptOutcome::TransientFailure
            && entry.request.disposition == RecoveryDisposition::RetryTransient;
        let exhausted = retryable && attempt >= entry.request.max_attempts;
        let next_retry_unix_ms = (retryable && !exhausted)
            .then(|| next_retry_ms(&entry.request, attempt, receipt.observed_unix_ms));
        entry.attempts.push(RecoveryAttemptRecord {
            attempt,
            outcome: receipt.outcome,
            operation_receipt: receipt.operation_receipt.clone(),
            landed_commit: receipt.landed_commit.clone(),
            provider_main_verified: receipt.provider_main_verified,
            observed_unix_ms: receipt.observed_unix_ms,
            next_retry_unix_ms,
        });
        entry.completed = receipt.outcome == RecoveryAttemptOutcome::Success;
        if exhausted && entry.escalation.is_none() {
            entry.escalation = Some(escalation(&entry.request, receipt.observed_unix_ms));
        }
        persist_ledger(&input.ledger, &ledger)?;
    }
    Ok(RecoveryAttemptOutput {
        entry: ledger.entries[&receipt.request_id].clone(),
        duplicate,
        mutated_provider: false,
    })
}

pub fn show(input: &RecoveryLedgerInput) -> Result<RecoveryLedger, AppError> {
    load_ledger(&input.ledger)
}

fn validate_bounds(max_attempts: u32, backoff: u64) -> Result<(), AppError> {
    if !(1..=MAX_ATTEMPTS).contains(&max_attempts) || !(1..=MAX_BACKOFF_SECS).contains(&backoff) {
        return Err(validation(
            "recovery_retry_policy_invalid",
            "retry bounds exceed max_attempts=10 or backoff=3600s",
        ));
    }
    Ok(())
}

fn classify_disposition(code: &str, details: &Value) -> RecoveryDisposition {
    let disposition = details
        .pointer("/decision/disposition")
        .or_else(|| details.get("disposition"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if disposition == "retry_tick"
        || ["timeout", "transport", "temporarily", "rate_limit"]
            .iter()
            .any(|marker| code.contains(marker))
    {
        RecoveryDisposition::RetryTransient
    } else if disposition == "external_decision" {
        RecoveryDisposition::ExternalDecision
    } else {
        RecoveryDisposition::OperatorAction
    }
}

fn allowed_actions(
    disposition: RecoveryDisposition,
    pr: Option<PrNumber>,
    head: Option<&CommitOid>,
    details: &Value,
) -> Vec<RecoveryAllowedAction> {
    let mut actions = match disposition {
        RecoveryDisposition::RetryTransient => {
            vec![
                RecoveryAllowedAction::Rediscover,
                RecoveryAllowedAction::RetrySync,
            ]
        }
        RecoveryDisposition::ExternalDecision | RecoveryDisposition::OperatorAction => {
            vec![RecoveryAllowedAction::OperatorDecision {
                code: "review_exact_evidence".to_owned(),
            }]
        }
    };
    if let (Some(pr), Some(head)) = (pr, head) {
        actions.push(RecoveryAllowedAction::OwnerRepair {
            pr,
            expected_head: head.clone(),
        });
    }
    if let Some(plan) = extract_exact_plan(details) {
        actions.push(plan);
    }
    actions
}

fn extract_exact_plan(details: &Value) -> Option<RecoveryAllowedAction> {
    let plan = details
        .pointer("/top_eviction_plan")
        .or_else(|| details.pointer("/details/top_eviction_plan"))?;
    let operation_id = plan.get("operation_id")?.as_str()?.to_owned();
    let plan_hash = plan.get("plan_hash")?.as_str()?.to_owned();
    let owner_pr = plan
        .pointer("/affected_prs/0")
        .or_else(|| plan.get("evict_pr"))?
        .as_u64()
        .map(PrNumber)?;
    if !safe_receipt(&operation_id) || !safe_receipt(&plan_hash) {
        return None;
    }
    Some(RecoveryAllowedAction::ExactPlan {
        operation: "top_eviction".to_owned(),
        operation_id,
        plan_hash,
        owner_pr,
    })
}

fn escalation(request: &RecoveryRequest, now: u64) -> RecoveryEscalation {
    let exact_plan = request
        .allowed_actions
        .iter()
        .find(|action| matches!(action, RecoveryAllowedAction::ExactPlan { .. }))
        .cloned();
    RecoveryEscalation {
        kind: if exact_plan.is_some() {
            "exact_plan_owner_request"
        } else {
            "operator_escalation"
        }
        .to_owned(),
        request_id: request.request_id.clone(),
        exact_plan,
        emitted_unix_ms: now,
    }
}

fn next_retry_ms(request: &RecoveryRequest, attempt: u32, observed: u64) -> u64 {
    let shift = attempt.saturating_sub(1).min(20);
    let multiplier = 1_u64.checked_shl(shift).unwrap_or(u64::MAX);
    let seconds = request
        .initial_backoff_secs
        .saturating_mul(multiplier)
        .min(MAX_BACKOFF_SECS);
    observed.saturating_add(seconds.saturating_mul(1_000))
}

fn verify_success(
    context: Option<&AppContext>,
    request: &RecoveryRequest,
    receipt: &RecoveryAttemptReceipt,
) -> Result<(), AppError> {
    if !receipt.provider_main_verified {
        return Err(validation(
            "recovery_success_unverified",
            "success requires a fresh provider/main reread",
        ));
    }
    let pr = request.pr.ok_or_else(|| {
        validation(
            "recovery_success_unverified",
            "terminal success requires one exact PR generation",
        )
    })?;
    let landed_commit = receipt.landed_commit.as_ref().ok_or_else(|| {
        validation(
            "recovery_success_unverified",
            "terminal success requires the exact landed commit",
        )
    })?;
    let context = context.ok_or_else(|| {
        validation(
            "recovery_success_unverified",
            "terminal success requires repository context for provider/main reread",
        )
    })?;
    let status = crate::read::status(context)?;
    let pull = status.analysis.pull_requests.get(&pr).ok_or_else(|| {
        validation(
            "recovery_success_unverified",
            "fresh provider status does not contain the exact PR",
        )
    })?;
    if status.repository != request.repository
        || pull.state != crate::model::PullRequestState::Merged
        || &status.analysis.fleet.default_branch.oid != landed_commit
    {
        return Err(validation(
            "recovery_success_unverified",
            "fresh provider status does not prove the exact merged PR/commit",
        ));
    }
    Ok(())
}

fn validate_receipt_generation(
    request: &RecoveryRequest,
    receipt: &RecoveryAttemptReceipt,
) -> Result<(), AppError> {
    if receipt.schema_version != SCHEMA_VERSION
        || receipt.repository != request.repository
        || receipt.pr != request.pr
        || receipt.head != request.head
        || receipt.main != request.main
        || receipt.decision_fingerprint != request.decision_fingerprint
    {
        return Err(validation(
            "recovery_attempt_generation_mismatch",
            "attempt receipt does not match exact request generation/fingerprint",
        ));
    }
    if receipt
        .operation_receipt
        .as_deref()
        .is_some_and(|receipt| !safe_receipt(receipt))
    {
        return Err(validation(
            "recovery_attempt_receipt_invalid",
            "operation receipt contains unsafe characters",
        ));
    }
    Ok(())
}

fn extract_links(details: &Value, repository: &RepositoryId) -> Vec<String> {
    let mut candidates = Vec::new();
    for pointer in ["/url", "/details_url", "/decision/url", "/receipt/url"] {
        if let Some(url) = details.pointer(pointer).and_then(Value::as_str) {
            candidates.push(url);
        }
    }
    if let Some(links) = details.get("links").and_then(Value::as_array) {
        candidates.extend(links.iter().filter_map(Value::as_str));
    }
    let github_prefix = format!(
        "https://github.com/{}/{}",
        repository.owner, repository.name
    );
    let api_prefix = format!(
        "https://api.github.com/repos/{}/{}",
        repository.owner, repository.name
    );
    candidates
        .into_iter()
        .filter(|url| {
            url.len() <= 512
                && !url.contains(['?', '#', '@'])
                && (url.starts_with(&github_prefix) || url.starts_with(&api_prefix))
        })
        .take(5)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn extract_pr(details: &Value) -> Option<PrNumber> {
    ["/pr", "/candidate_pr", "/decision/pr", "/candidate/pr"]
        .iter()
        .find_map(|pointer| details.pointer(pointer).and_then(Value::as_u64))
        .map(PrNumber)
}

fn read_json(path: &Path, code: &'static str) -> Result<Value, AppError> {
    let metadata = fs::metadata(path).map_err(|_| validation(code, "input file is unavailable"))?;
    if metadata.len() > MAX_INPUT_BYTES {
        return Err(validation(code, "input exceeds bounded 256 KiB"));
    }
    let bytes = fs::read(path).map_err(|_| validation(code, "input could not be read"))?;
    serde_json::from_slice(&bytes).map_err(|_| validation(code, "input JSON is malformed"))
}

fn load_ledger(path: &Path) -> Result<RecoveryLedger, AppError> {
    validate_ledger_path(path)?;
    if !path.exists() {
        return Ok(RecoveryLedger::default());
    }
    let value = read_json(path, "recovery_ledger_invalid")?;
    let ledger: RecoveryLedger = serde_json::from_value(value)
        .map_err(|_| validation("recovery_ledger_invalid", "ledger schema invalid"))?;
    if ledger.schema_version != SCHEMA_VERSION {
        return Err(validation(
            "recovery_ledger_version_unsupported",
            "ledger schema version unsupported",
        ));
    }
    Ok(ledger)
}

fn persist_ledger(path: &Path, ledger: &RecoveryLedger) -> Result<(), AppError> {
    validate_ledger_path(path)?;
    reject_secrets(&serde_json::to_value(ledger).expect("ledger serializes"))?;
    let parent = path
        .parent()
        .ok_or_else(|| validation("recovery_ledger_path_invalid", "ledger path has no parent"))?;
    fs::create_dir_all(parent).map_err(|_| {
        validation(
            "recovery_ledger_write_failed",
            "ledger directory unavailable",
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|_| validation("recovery_ledger_write_failed", "ledger permissions failed"))?;
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
    let bytes = serde_json::to_vec_pretty(ledger)
        .map_err(|_| validation("recovery_ledger_invalid", "ledger serialization failed"))?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(|_| {
        validation(
            "recovery_ledger_write_failed",
            "temporary ledger unavailable",
        )
    })?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| validation("recovery_ledger_write_failed", "ledger sync failed"))?;
    fs::rename(&temporary, path).map_err(|_| {
        validation(
            "recovery_ledger_write_failed",
            "atomic ledger rename failed",
        )
    })?;
    Ok(())
}

fn validate_ledger_path(path: &Path) -> Result<(), AppError> {
    if path.file_name().and_then(std::ffi::OsStr::to_str) != Some("recovery-ledger.json") {
        return Err(validation(
            "recovery_ledger_path_invalid",
            "ledger filename must be exactly recovery-ledger.json",
        ));
    }
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        || path.parent().is_some_and(|parent| {
            fs::symlink_metadata(parent).is_ok_and(|metadata| metadata.file_type().is_symlink())
        })
    {
        return Err(validation(
            "recovery_ledger_path_invalid",
            "ledger path and parent must not be symbolic links",
        ));
    }
    Ok(())
}

fn reject_secrets(value: &Value) -> Result<(), AppError> {
    let rendered = serde_json::to_string(value).expect("value serializes");
    let lowered = rendered.to_ascii_lowercase();
    if [
        "ghp_",
        "ghs_",
        "github_pat_",
        "authorization:",
        "begin private key",
        "ignore previous instructions",
        "system prompt",
    ]
    .iter()
    .any(|marker| lowered.contains(marker))
    {
        return Err(validation(
            "recovery_secret_or_instruction_rejected",
            "recovery state contains secret/prompt-injection sentinel",
        ));
    }
    Ok(())
}

fn safe_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn safe_receipt(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        })
}

fn digest_json(value: &Value) -> String {
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(value).expect("value serializes"));
    format!("sha256:{:x}", digest.finalize())
}

fn validation(code: &'static str, message: &'static str) -> AppError {
    AppError::structured(
        ErrorCategory::Validation,
        code,
        message,
        Some(serde_json::json!({"mutated": false})),
    )
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_attempt(input: &RecoveryAttemptInput) -> Result<RecoveryAttemptOutput, AppError> {
        super::record_attempt(None, input)
    }

    fn request(disposition: RecoveryDisposition) -> RecoveryRequest {
        RecoveryRequest {
            schema_version: 1,
            request_id: "recovery:fixture".to_owned(),
            repository: RepositoryId {
                owner: "owner".to_owned(),
                name: "repo".to_owned(),
            },
            pr: Some(PrNumber(41)),
            head: Some(CommitOid("1".repeat(40))),
            base: Some(CommitOid("2".repeat(40))),
            main: CommitOid("3".repeat(40)),
            stack_root_pr: Some(PrNumber(41)),
            check_generation: "sha256:checks".to_owned(),
            config_fingerprint: "sha256:config".to_owned(),
            policy_fingerprint: "sha256:policy".to_owned(),
            wake_class: "retry_tick".to_owned(),
            disposition,
            decision_fingerprint: "sha256:decision".to_owned(),
            diagnostic_codes: vec!["transport_timeout".to_owned()],
            links: Vec::new(),
            allowed_actions: vec![
                RecoveryAllowedAction::Rediscover,
                RecoveryAllowedAction::RetrySync,
            ],
            max_attempts: 2,
            initial_backoff_secs: 10,
            help: RecoveryHelpCursor {
                cli: "cara help --json".to_owned(),
                mcp_tool: "help".to_owned(),
                schema_version: 1,
            },
            observed_unix_ms: 1,
            provider_api: GitHubApiTelemetry::default(),
        }
    }

    fn receipt(
        request: &RecoveryRequest,
        outcome: RecoveryAttemptOutcome,
        now: u64,
    ) -> RecoveryAttemptReceipt {
        RecoveryAttemptReceipt {
            schema_version: 1,
            request_id: request.request_id.clone(),
            repository: request.repository.clone(),
            pr: request.pr,
            head: request.head.clone(),
            main: request.main.clone(),
            decision_fingerprint: request.decision_fingerprint.clone(),
            outcome,
            operation_receipt: Some(format!("receipt:{now}")),
            landed_commit: None,
            provider_main_verified: outcome == RecoveryAttemptOutcome::Success,
            observed_unix_ms: now,
        }
    }

    fn write_ledger(path: &Path, request: RecoveryRequest) {
        let mut ledger = RecoveryLedger::default();
        ledger.entries.insert(
            request.request_id.clone(),
            RecoveryLedgerEntry {
                request,
                attempts: Vec::new(),
                escalation: None,
                completed: false,
            },
        );
        persist_ledger(path, &ledger).unwrap();
    }

    #[test]
    fn error_classification_and_exact_plan_mapping_are_deterministic() {
        assert_eq!(
            classify_disposition("github_discovery_timeout", &Value::Null),
            RecoveryDisposition::RetryTransient
        );
        assert_eq!(
            classify_disposition(
                "ci_failed",
                &serde_json::json!({"decision":{"disposition":"external_decision"}}),
            ),
            RecoveryDisposition::ExternalDecision
        );
        let details = serde_json::json!({
            "url": "https://github.com/owner/repo/pull/79",
            "links": [
                "https://api.github.com/repos/owner/repo/pulls/79",
                "https://evil.invalid/steal",
                "https://github.com/owner/repo/pull/79?token=secret"
            ],
            "top_eviction_plan": {
                "operation_id": "operation:stack:80:evict:79",
                "plan_hash": "fnv1a64:e93e539f28f65f8e",
                "affected_prs": [79]
            }
        });
        assert!(matches!(
            extract_exact_plan(&details),
            Some(RecoveryAllowedAction::ExactPlan { owner_pr, .. }) if owner_pr == PrNumber(79)
        ));
        let links = extract_links(
            &details,
            &RepositoryId {
                owner: "owner".to_owned(),
                name: "repo".to_owned(),
            },
        );
        assert_eq!(links.len(), 2);
        assert!(links.iter().all(|link| link.contains("owner/repo")));
        assert!(!links.iter().any(|link| link.contains("token")));
    }

    #[test]
    fn transient_attempts_backoff_exhaust_once_and_persist_across_restart() {
        let directory = tempfile::tempdir().unwrap();
        let ledger_path = directory.path().join("recovery-ledger.json");
        let request = request(RecoveryDisposition::RetryTransient);
        write_ledger(&ledger_path, request.clone());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&ledger_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        for now in [1_000, 2_000] {
            let receipt_path = directory.path().join(format!("receipt-{now}.json"));
            fs::write(
                &receipt_path,
                serde_json::to_vec(&receipt(
                    &request,
                    RecoveryAttemptOutcome::TransientFailure,
                    now,
                ))
                .unwrap(),
            )
            .unwrap();
            let output = record_attempt(&RecoveryAttemptInput {
                ledger: ledger_path.clone(),
                receipt: receipt_path,
            })
            .unwrap();
            assert_eq!(output.entry.attempts.len(), usize::from(now == 2_000) + 1);
        }
        let ledger = load_ledger(&ledger_path).unwrap();
        let entry = &ledger.entries[&request.request_id];
        assert_eq!(entry.attempts[0].next_retry_unix_ms, Some(11_000));
        assert_eq!(entry.attempts[1].next_retry_unix_ms, None);
        assert!(entry.escalation.is_some());

        let duplicate_path = directory.path().join("duplicate.json");
        fs::write(
            &duplicate_path,
            serde_json::to_vec(&receipt(
                &request,
                RecoveryAttemptOutcome::TransientFailure,
                2_000,
            ))
            .unwrap(),
        )
        .unwrap();
        let duplicate = record_attempt(&RecoveryAttemptInput {
            ledger: ledger_path.clone(),
            receipt: duplicate_path,
        })
        .unwrap();
        assert!(duplicate.duplicate);
        assert_eq!(duplicate.entry.attempts.len(), 2);

        let exhausted_path = directory.path().join("exhausted.json");
        fs::write(
            &exhausted_path,
            serde_json::to_vec(&receipt(
                &request,
                RecoveryAttemptOutcome::TransientFailure,
                3_000,
            ))
            .unwrap(),
        )
        .unwrap();
        let error = record_attempt(&RecoveryAttemptInput {
            ledger: ledger_path,
            receipt: exhausted_path,
        })
        .unwrap_err();
        assert_eq!(error.code, "recovery_attempts_exhausted");
    }

    #[test]
    fn deterministic_failure_never_retries_and_success_requires_reread() {
        let directory = tempfile::tempdir().unwrap();
        let ledger_path = directory.path().join("recovery-ledger.json");
        let request = request(RecoveryDisposition::OperatorAction);
        write_ledger(&ledger_path, request.clone());
        let receipt_path = directory.path().join("receipt.json");
        let mut attempt = receipt(&request, RecoveryAttemptOutcome::Success, 1_000);
        attempt.provider_main_verified = false;
        fs::write(&receipt_path, serde_json::to_vec(&attempt).unwrap()).unwrap();
        let error = record_attempt(&RecoveryAttemptInput {
            ledger: ledger_path.clone(),
            receipt: receipt_path,
        })
        .unwrap_err();
        assert_eq!(error.code, "recovery_success_unverified");

        let receipt_path = directory.path().join("no-context-success.json");
        let mut attempt = receipt(&request, RecoveryAttemptOutcome::Success, 1_500);
        attempt.provider_main_verified = true;
        attempt.landed_commit = Some(CommitOid("4".repeat(40)));
        fs::write(&receipt_path, serde_json::to_vec(&attempt).unwrap()).unwrap();
        let error = record_attempt(&RecoveryAttemptInput {
            ledger: ledger_path.clone(),
            receipt: receipt_path,
        })
        .unwrap_err();
        assert_eq!(error.code, "recovery_success_unverified");

        let receipt_path = directory.path().join("terminal.json");
        fs::write(
            &receipt_path,
            serde_json::to_vec(&receipt(
                &request,
                RecoveryAttemptOutcome::TerminalFailure,
                2_000,
            ))
            .unwrap(),
        )
        .unwrap();
        let output = record_attempt(&RecoveryAttemptInput {
            ledger: ledger_path,
            receipt: receipt_path,
        })
        .unwrap();
        assert_eq!(output.entry.attempts[0].next_retry_unix_ms, None);
        assert!(output.entry.escalation.is_none());
    }

    #[test]
    fn exhaustion_emits_only_the_exact_sealed_plan_and_preserves_other_entries() {
        let directory = tempfile::tempdir().unwrap();
        let ledger_path = directory.path().join("recovery-ledger.json");
        let mut first = request(RecoveryDisposition::RetryTransient);
        first.max_attempts = 1;
        first
            .allowed_actions
            .push(RecoveryAllowedAction::ExactPlan {
                operation: "top_eviction".to_owned(),
                operation_id: "operation:stack:80:evict:79".to_owned(),
                plan_hash: "fnv1a64:e93e539f28f65f8e".to_owned(),
                owner_pr: PrNumber(79),
            });
        let mut second = request(RecoveryDisposition::OperatorAction);
        second.request_id = "recovery:unrelated".to_owned();
        second.decision_fingerprint = "sha256:unrelated".to_owned();
        let mut ledger = RecoveryLedger::default();
        for request in [first.clone(), second.clone()] {
            ledger.entries.insert(
                request.request_id.clone(),
                RecoveryLedgerEntry {
                    request,
                    attempts: Vec::new(),
                    escalation: None,
                    completed: false,
                },
            );
        }
        persist_ledger(&ledger_path, &ledger).unwrap();
        let receipt_path = directory.path().join("attempt.json");
        fs::write(
            &receipt_path,
            serde_json::to_vec(&receipt(
                &first,
                RecoveryAttemptOutcome::TransientFailure,
                1_000,
            ))
            .unwrap(),
        )
        .unwrap();

        let output = record_attempt(&RecoveryAttemptInput {
            ledger: ledger_path.clone(),
            receipt: receipt_path,
        })
        .unwrap();

        let escalation = output.entry.escalation.expect("one exhaustion escalation");
        assert_eq!(escalation.kind, "exact_plan_owner_request");
        assert!(matches!(
            escalation.exact_plan,
            Some(RecoveryAllowedAction::ExactPlan { owner_pr, .. }) if owner_pr == PrNumber(79)
        ));
        let reloaded = load_ledger(&ledger_path).unwrap();
        assert!(reloaded.entries[&second.request_id].attempts.is_empty());
        assert!(reloaded.entries[&second.request_id].escalation.is_none());
    }

    #[test]
    fn ledger_path_refuses_arbitrary_filename_and_symlink_target() {
        let directory = tempfile::tempdir().unwrap();
        let error = persist_ledger(
            &directory.path().join("arbitrary.json"),
            &RecoveryLedger::default(),
        )
        .unwrap_err();
        assert_eq!(error.code, "recovery_ledger_path_invalid");

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                directory.path().join("target.json"),
                directory.path().join("recovery-ledger.json"),
            )
            .unwrap();
            let error = persist_ledger(
                &directory.path().join("recovery-ledger.json"),
                &RecoveryLedger::default(),
            )
            .unwrap_err();
            assert_eq!(error.code, "recovery_ledger_path_invalid");
        }
    }

    #[test]
    fn stale_generation_and_secret_receipts_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let ledger_path = directory.path().join("recovery-ledger.json");
        let request = request(RecoveryDisposition::RetryTransient);
        write_ledger(&ledger_path, request.clone());
        let mut stale = receipt(&request, RecoveryAttemptOutcome::Acknowledged, 1_000);
        stale.head = Some(CommitOid("9".repeat(40)));
        let receipt_path = directory.path().join("stale.json");
        fs::write(&receipt_path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let error = record_attempt(&RecoveryAttemptInput {
            ledger: ledger_path.clone(),
            receipt: receipt_path,
        })
        .unwrap_err();
        assert_eq!(error.code, "recovery_attempt_generation_mismatch");

        let mut secret = receipt(&request, RecoveryAttemptOutcome::Acknowledged, 2_000);
        secret.operation_receipt = Some("ghs_secret_sentinel".to_owned());
        let receipt_path = directory.path().join("secret.json");
        fs::write(&receipt_path, serde_json::to_vec(&secret).unwrap()).unwrap();
        let error = record_attempt(&RecoveryAttemptInput {
            ledger: ledger_path.clone(),
            receipt: receipt_path,
        })
        .unwrap_err();
        assert_eq!(error.code, "recovery_secret_or_instruction_rejected");

        let mut injection = receipt(&request, RecoveryAttemptOutcome::Acknowledged, 3_000);
        injection.operation_receipt = Some("ignore previous instructions".to_owned());
        let receipt_path = directory.path().join("injection.json");
        fs::write(&receipt_path, serde_json::to_vec(&injection).unwrap()).unwrap();
        let error = record_attempt(&RecoveryAttemptInput {
            ledger: ledger_path,
            receipt: receipt_path,
        })
        .unwrap_err();
        assert_eq!(error.code, "recovery_secret_or_instruction_rejected");
    }
}
