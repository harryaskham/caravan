//! Exact-generation recovery for engine-owned terminal-red parking.
//!
//! Unlike `pause::resume`, this operation consumes no explicit hold. It only
//! removes the scheduler-owned `caravan-parked` label after binding the caller
//! to the durable parking event and proving the newest authoritative check
//! generation on the unchanged provider generation is green.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::command::{CommandRunner, CommandSpec, ProcessRunner};
use crate::github::{GitHubMutationAdapter, GitHubMutationReceipt, MutationError};
use crate::journal::{JournalRecord, LogInput};
use crate::model::{
    CheckSnapshot, CheckState, MutationKind, MutationStep, MutationStepState, OperationId,
    OperationReceipt, PrNumber, PullRequestPrecondition, PullRequestSnapshot, PullRequestState,
    RepositoryId,
};
use crate::read::StatusOutput;
use crate::{AppContext, AppError};

const PARKED_LABEL: &str = "caravan-parked";
const MAX_TEXT: usize = 2_000;

/// Reviewed authority for one engine-owned parked generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Args)]
pub struct UnparkInput {
    /// Exact owner/name provider repository.
    #[arg(long = "repository-slug", value_name = "OWNER/NAME")]
    pub repository: String,
    /// Exact parked Caravan head PR.
    #[arg(long)]
    pub pr: u64,
    /// Exact 40-character current provider head OID.
    #[arg(long)]
    pub head: String,
    /// Exact current provider base branch name.
    #[arg(long)]
    pub base_ref: String,
    /// Exact 40-character current provider base OID.
    #[arg(long)]
    pub base: String,
    /// Exact current membership generation from Cara evidence.
    #[arg(long)]
    pub membership_generation: String,
    /// Fingerprint from the durable `caravan_parked` journal event.
    #[arg(long)]
    pub parking_fingerprint: String,
    /// Reviewed current provider state. The only accepted value is `open_parked`.
    #[arg(long)]
    pub provider_state: String,
    /// Audited human or agent identity.
    #[arg(long)]
    pub actor: String,
    /// Bounded recovery rationale.
    #[arg(long)]
    pub reason: String,
}

/// Durable exact-generation recovery receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UnparkOutput {
    pub schema_version: u32,
    pub receipt_id: String,
    pub repository: RepositoryId,
    pub pr: PrNumber,
    pub head: String,
    pub base_ref: String,
    pub base: String,
    pub membership_generation: String,
    pub parking_fingerprint: String,
    pub evidence_fingerprint: String,
    pub actor: String,
    pub reason: String,
    pub old_state: String,
    pub new_state: String,
    pub old_labels: BTreeSet<String>,
    pub new_labels: BTreeSet<String>,
    #[serde(default)]
    pub authoritative_checks: Vec<CheckSnapshot>,
    #[serde(default)]
    pub superseded_checks: Vec<CheckSnapshot>,
    pub mutated: bool,
    pub receipt: OperationReceipt,
    #[serde(default)]
    pub provider_receipts: Vec<GitHubMutationReceipt>,
    pub next: String,
}

/// Release one exact engine-owned parking fence. Explicit pauses remain a
/// separate authority and are never interpreted as parking provenance.
pub fn unpark(context: &AppContext, input: &UnparkInput) -> Result<UnparkOutput, AppError> {
    validate_input(input)?;
    let mut lock = context.acquire_writer_operation("unpark")?;
    if let Some(receipt) = load_receipt(&context.repository_path, input)? {
        return Ok(receipt);
    }

    let initial = crate::read::status(context)?;
    crate::initialization::require_ready(&initial.initialization)?;
    let parked_fingerprint = parked_provenance(context, input)?;
    let runner = ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(Duration::from_secs(context.config.command_timeout_secs));
    let provider = GitHubMutationAdapter::new(lock.runner(runner));
    let pending = load_pending(&context.repository_path, input)?;
    let prepared = if let Some(prepared) = pending {
        let current = initial.analysis.pull_requests.get(&PrNumber(input.pr));
        if current.is_some_and(|pull| !pull.has_label(PARKED_LABEL)) {
            let after = current.expect("checked current").clone();
            let recovered = GitHubMutationReceipt {
                kind: MutationKind::RemoveLabel,
                before: Some(prepared.pull.clone()),
                after,
                provider_output: Some(
                    "recovered durable unpark authorization after interrupted provider mutation"
                        .to_owned(),
                ),
            };
            let mut output = finish(&initial, &initial, input, prepared, recovered)?;
            "run `cara status`, then `cara sync --all`; Caravan membership was preserved"
                .clone_into(&mut output.next);
            write_receipt(&context.repository_path, input, &output)?;
            remove_pending(&context.repository_path, input)?;
            return Ok(output);
        }
        let fresh = prepare(&initial, input, &parked_fingerprint)?;
        if fresh.evidence_fingerprint != prepared.evidence_fingerprint {
            return Err(refusal(
                "unpark_pending_authority_drift",
                "fresh evidence differs from the durable pre-mutation authorization",
                &initial,
                input,
            ));
        }
        prepared
    } else {
        let prepared = prepare(&initial, input, &parked_fingerprint)?;
        write_pending(&context.repository_path, input, &prepared)?;
        prepared
    };

    lock.checkpoint(
        "unpark_provider_mutation_in_flight",
        json!({
            "repository": input.repository,
            "pr": input.pr,
            "head": input.head,
            "base_ref": input.base_ref,
            "base": input.base,
            "membership_generation": input.membership_generation,
            "parking_fingerprint": input.parking_fingerprint,
            "evidence_fingerprint": prepared.evidence_fingerprint,
        }),
        true,
    )?;
    let provider_receipt = provider
        .remove_label(
            &initial.repository,
            &PullRequestPrecondition::from(&prepared.pull),
            PARKED_LABEL,
        )
        .map_err(|error| provider_error(&error, input))?;

    // Re-run complete fleet analysis while the operation guard and provider
    // lease are still held. The provider receipt alone is not postcondition
    // authority: topology, membership, head/base, checks, and labels must all
    // survive rediscovery.
    let final_status = crate::read::status(context)?;
    let mut output = finish(&initial, &final_status, input, prepared, provider_receipt)?;
    "run `cara status`, then `cara sync --all`; Caravan membership was preserved"
        .clone_into(&mut output.next);
    write_receipt(&context.repository_path, input, &output)?;
    remove_pending(&context.repository_path, input)?;
    lock.checkpoint(
        "unpark_converged",
        json!({"receipt_id": output.receipt_id, "evidence_fingerprint": output.evidence_fingerprint}),
        false,
    )?;
    Ok(output)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Prepared {
    receipt_id: String,
    pull: PullRequestSnapshot,
    membership_generation: String,
    evidence_fingerprint: String,
    authoritative_checks: Vec<CheckSnapshot>,
    superseded_checks: Vec<CheckSnapshot>,
}

#[allow(clippy::too_many_lines)]
fn prepare(
    status: &StatusOutput,
    input: &UnparkInput,
    parked_fingerprint: &str,
) -> Result<Prepared, AppError> {
    if status.repository.slug() != input.repository {
        return Err(refusal(
            "unpark_repository_drift",
            "fresh provider repository differs from reviewed authority",
            status,
            input,
        ));
    }
    let pr = PrNumber(input.pr);
    let pull = status
        .analysis
        .pull_requests
        .get(&pr)
        .cloned()
        .ok_or_else(|| {
            refusal(
                "unpark_pr_not_found",
                "parked PR is absent from fresh provider discovery",
                status,
                input,
            )
        })?;
    if pull.state != PullRequestState::Open {
        return Err(refusal(
            "unpark_pr_not_open",
            "parked PR is no longer open",
            status,
            input,
        ));
    }
    if pull.head.oid.0 != input.head
        || pull.base.name != input.base_ref
        || pull.base.oid.0 != input.base
    {
        return Err(refusal(
            "unpark_generation_drift",
            "provider head or base differs from reviewed generation",
            status,
            input,
        ));
    }
    if !pull.has_label("caravan") || pull.has_label("caravan-evicted") {
        return Err(refusal(
            "unpark_membership_ineligible",
            "PR must remain enrolled and not evicted",
            status,
            input,
        ));
    }
    if !pull.has_label(PARKED_LABEL) {
        return Err(refusal(
            "unpark_parking_label_missing",
            "expected caravan-parked provider state is absent",
            status,
            input,
        ));
    }
    if status
        .pauses
        .iter()
        .any(|pause| pause.record.caravan_head == pr && pause.state.is_effective())
    {
        return Err(refusal(
            "unpark_explicit_pause_present",
            "an active explicit pause or recovery fence blocks engine-owned unpark",
            status,
            input,
        ));
    }
    let caravan = status.analysis.fleet.caravan(pr).ok_or_else(|| {
        refusal(
            "unpark_membership_missing",
            "PR is not exactly one current Caravan head",
            status,
            input,
        )
    })?;
    if status
        .analysis
        .fleet
        .caravans
        .iter()
        .filter(|candidate| candidate.members.contains(&pr))
        .count()
        != 1
    {
        return Err(refusal(
            "unpark_membership_ambiguous",
            "PR must be enrolled exactly once",
            status,
            input,
        ));
    }
    let generation = membership_generation(status, caravan)?;
    if generation != input.membership_generation {
        return Err(refusal(
            "unpark_membership_drift",
            "Caravan membership generation differs from reviewed authority",
            status,
            input,
        ));
    }
    if parked_fingerprint != input.parking_fingerprint {
        return Err(refusal(
            "unpark_parking_provenance_drift",
            "durable parking provenance differs from reviewed authority",
            status,
            input,
        ));
    }
    let (authoritative, superseded) = crate::model::latest_checks_per_identity(&pull.checks);
    if authoritative.is_empty()
        || authoritative.iter().any(|check| {
            !matches!(
                check.state,
                CheckState::Success | CheckState::Neutral | CheckState::Skipped
            )
        })
    {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "unpark_ci_not_green",
            "newest authoritative required-check generation is not green",
            Some(
                json!({"pr": pr, "head": input.head, "authoritative_checks": authoritative, "superseded_checks": superseded, "mutated": false}),
            ),
        ));
    }
    let authoritative_checks = authoritative.into_iter().cloned().collect::<Vec<_>>();
    let superseded_checks = superseded.into_iter().cloned().collect::<Vec<_>>();
    let evidence_fingerprint = crate::membership::fnv1a64(
        &serde_json::to_vec(&json!({
            "schema_version": 1,
            "repository": status.repository,
            "pr": pr,
            "head": pull.head,
            "base": pull.base,
            "labels": pull.labels,
            "membership_generation": generation,
            "parking_fingerprint": parked_fingerprint,
            "authoritative_checks": authoritative_checks,
            "superseded_checks": superseded_checks,
        }))
        .expect("unpark evidence serializes"),
    );
    Ok(Prepared {
        receipt_id: OperationId::new().0,
        pull,
        membership_generation: generation,
        evidence_fingerprint,
        authoritative_checks,
        superseded_checks,
    })
}

fn finish(
    initial: &StatusOutput,
    final_status: &StatusOutput,
    input: &UnparkInput,
    prepared: Prepared,
    provider_receipt: GitHubMutationReceipt,
) -> Result<UnparkOutput, AppError> {
    let pr = PrNumber(input.pr);
    let current = final_status
        .analysis
        .pull_requests
        .get(&pr)
        .ok_or_else(|| {
            postcondition(
                "unpark_postcondition_pr_missing",
                "PR disappeared after parking transition",
                input,
                &provider_receipt,
                final_status,
            )
        })?;
    let caravan = final_status.analysis.fleet.caravan(pr).ok_or_else(|| {
        postcondition(
            "unpark_postcondition_membership_missing",
            "Caravan topology changed after parking transition",
            input,
            &provider_receipt,
            final_status,
        )
    })?;
    let final_generation = membership_generation(final_status, caravan)?;
    if final_status.repository != initial.repository
        || current.state != PullRequestState::Open
        || current.head.oid.0 != input.head
        || current.base.name != input.base_ref
        || current.base.oid.0 != input.base
        || !current.has_label("caravan")
        || current.has_label("caravan-evicted")
        || current.has_label(PARKED_LABEL)
        || final_generation != prepared.membership_generation
        || final_status
            .pauses
            .iter()
            .any(|pause| pause.record.caravan_head == pr && pause.state.is_effective())
    {
        return Err(postcondition(
            "unpark_postcondition_drift",
            "provider or fleet facts drifted after parking transition",
            input,
            &provider_receipt,
            final_status,
        ));
    }
    let (authoritative, _) = crate::model::latest_checks_per_identity(&current.checks);
    if authoritative.is_empty()
        || authoritative.iter().any(|check| {
            !matches!(
                check.state,
                CheckState::Success | CheckState::Neutral | CheckState::Skipped
            )
        })
    {
        return Err(postcondition(
            "unpark_postcondition_ci_drift",
            "authoritative checks drifted after parking transition",
            input,
            &provider_receipt,
            final_status,
        ));
    }
    let step = MutationStep {
        kind: MutationKind::RemoveLabel,
        state: MutationStepState::Completed,
        pr: Some(pr),
        summary: "removed only engine-owned terminal-red parking state".to_owned(),
    };
    Ok(UnparkOutput {
        schema_version: 1,
        receipt_id: prepared.receipt_id.clone(),
        repository: initial.repository.clone(),
        pr,
        head: input.head.clone(),
        base_ref: input.base_ref.clone(),
        base: input.base.clone(),
        membership_generation: prepared.membership_generation,
        parking_fingerprint: input.parking_fingerprint.clone(),
        evidence_fingerprint: prepared.evidence_fingerprint,
        actor: input.actor.clone(),
        reason: input.reason.clone(),
        old_state: "open_parked".to_owned(),
        new_state: "open_enrolled".to_owned(),
        old_labels: prepared.pull.labels,
        new_labels: current.labels.clone(),
        authoritative_checks: prepared.authoritative_checks,
        superseded_checks: prepared.superseded_checks,
        mutated: true,
        receipt: OperationReceipt {
            operation_id: OperationId(prepared.receipt_id),
            operation: "unpark".to_owned(),
            completed_steps: vec![step],
            changed: true,
        },
        provider_receipts: vec![provider_receipt],
        next: String::new(),
    })
}

fn membership_generation(
    status: &StatusOutput,
    caravan: &crate::model::Caravan,
) -> Result<String, AppError> {
    let members = caravan
        .members
        .iter()
        .map(|number| {
            status
                .analysis
                .pull_requests
                .get(number)
                .map(|pull| json!({"pr": number, "head": pull.head, "base": pull.base}))
                .ok_or_else(|| {
                    AppError::validation(
                        "unpark_membership_incomplete",
                        format!("fresh membership is missing PR #{number}"),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(crate::membership::fnv1a64(
        &serde_json::to_vec(&json!({
            "schema_version": 1,
            "repository": status.repository,
            "caravan_id": caravan.id,
            "default_branch": status.analysis.fleet.default_branch,
            "members": members,
        }))
        .expect("membership evidence serializes"),
    ))
}

fn parked_provenance(context: &AppContext, input: &UnparkInput) -> Result<String, AppError> {
    let snapshot = crate::journal::snapshot(
        context,
        &LogInput {
            limit: 10_000,
            kind: Some("caravan_parked".to_owned()),
            pr: Some(input.pr),
            since: None,
            until: None,
        },
    )?;
    snapshot.records.iter().rev().find_map(|record| match record {
        JournalRecord::Event { event, .. } if event.caravan_id == Some(PrNumber(input.pr)) => event.metadata.get("fingerprint").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }).ok_or_else(|| AppError::structured(
        ErrorCategory::Validation,
        "unpark_parking_provenance_not_found",
        "no durable engine-owned caravan_parked event matches this PR",
        Some(json!({"pr": input.pr, "journal": snapshot.source, "mutated": false, "next": "recover the exact parking event fingerprint; never substitute pause_not_found or a raw label edit"})),
    ))
}

fn validate_input(input: &UnparkInput) -> Result<(), AppError> {
    if input.pr == 0 {
        return Err(AppError::validation(
            "unpark_pr_invalid",
            "--pr must be non-zero",
        ));
    }
    for (name, code, oid) in [
        ("head", "unpark_head_invalid", &input.head),
        ("base", "unpark_base_invalid", &input.base),
    ] {
        if oid.len() != 40 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(AppError::validation(
                code,
                format!("--{name} must be one exact 40-character hexadecimal OID"),
            ));
        }
    }
    if input.base_ref.trim().is_empty() || input.base_ref.len() > MAX_TEXT {
        return Err(AppError::validation(
            "unpark_base_ref_invalid",
            "--base-ref must name the exact current provider base branch",
        ));
    }
    if input.repository.split_once('/').is_none() {
        return Err(AppError::validation(
            "unpark_repository_invalid",
            "--repository must be exact owner/name",
        ));
    }
    if input.provider_state != "open_parked" {
        return Err(AppError::validation(
            "unpark_provider_state_invalid",
            "--provider-state must be exactly open_parked",
        ));
    }
    for (name, value) in [
        ("membership generation", &input.membership_generation),
        ("parking fingerprint", &input.parking_fingerprint),
        ("actor", &input.actor),
        ("reason", &input.reason),
    ] {
        if value.trim().is_empty() || value.len() > MAX_TEXT {
            return Err(AppError::validation(
                "unpark_input_invalid",
                format!("{name} must contain 1..={MAX_TEXT} bytes"),
            ));
        }
    }
    for (name, value) in [
        ("membership generation", &input.membership_generation),
        ("parking fingerprint", &input.parking_fingerprint),
    ] {
        if value
            .strip_prefix("fnv1a64:")
            .is_none_or(|hex| hex.len() != 16 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(AppError::validation(
                "unpark_fingerprint_invalid",
                format!("{name} must be exact fnv1a64: plus 16 hexadecimal digits"),
            ));
        }
    }
    Ok(())
}

fn receipt_path(repository_path: &Path, input: &UnparkInput) -> Result<PathBuf, AppError> {
    let runner = ProcessRunner::in_directory(repository_path);
    let output = runner
        .run(&CommandSpec::new("git").args(["rev-parse", "--git-common-dir"]))
        .map_err(|error| AppError::validation("unpark_receipt_path_failed", error.to_string()))?;
    if !output.is_success() {
        return Err(AppError::validation(
            "unpark_receipt_path_failed",
            output.stderr,
        ));
    }
    let common = PathBuf::from(output.stdout.trim());
    let common = if common.is_absolute() {
        common
    } else {
        repository_path.join(common)
    };
    let identity = serde_json::to_vec(input).expect("validated unpark retry identity serializes");
    let key = crate::membership::fnv1a64(&identity).replace(':', "-");
    Ok(common
        .join("caravan")
        .join("unpark")
        .join(format!("{}-{key}.json", input.pr)))
}

fn pending_path(repository_path: &Path, input: &UnparkInput) -> Result<PathBuf, AppError> {
    Ok(receipt_path(repository_path, input)?.with_extension("pending.json"))
}

fn load_pending(repository_path: &Path, input: &UnparkInput) -> Result<Option<Prepared>, AppError> {
    let path = pending_path(repository_path, input)?;
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            AppError::validation(
                "unpark_pending_invalid",
                format!("{}: {error}", path.display()),
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::validation(
            "unpark_pending_read_failed",
            format!("{}: {error}", path.display()),
        )),
    }
}

fn write_pending(
    repository_path: &Path,
    input: &UnparkInput,
    pending: &Prepared,
) -> Result<(), AppError> {
    let path = pending_path(repository_path, input)?;
    fs::create_dir_all(path.parent().expect("unpark pending parent"))
        .map_err(|error| AppError::validation("unpark_pending_write_failed", error.to_string()))?;
    let bytes = serde_json::to_vec_pretty(pending)
        .map_err(|error| AppError::validation("unpark_pending_encode_failed", error.to_string()))?;
    durable_create(&path, &bytes, "unpark_pending_write_failed")
}

fn remove_pending(repository_path: &Path, input: &UnparkInput) -> Result<(), AppError> {
    let path = pending_path(repository_path, input)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::validation(
            "unpark_pending_remove_failed",
            format!("{}: {error}", path.display()),
        )),
    }
}

fn load_receipt(
    repository_path: &Path,
    input: &UnparkInput,
) -> Result<Option<UnparkOutput>, AppError> {
    let path = receipt_path(repository_path, input)?;
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            AppError::validation(
                "unpark_receipt_invalid",
                format!("{}: {error}", path.display()),
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::validation(
            "unpark_receipt_read_failed",
            format!("{}: {error}", path.display()),
        )),
    }
}

fn write_receipt(
    repository_path: &Path,
    input: &UnparkInput,
    receipt: &UnparkOutput,
) -> Result<(), AppError> {
    let path = receipt_path(repository_path, input)?;
    fs::create_dir_all(path.parent().expect("unpark receipt parent"))
        .map_err(|error| AppError::validation("unpark_receipt_write_failed", error.to_string()))?;
    let bytes = serde_json::to_vec_pretty(receipt)
        .map_err(|error| AppError::validation("unpark_receipt_encode_failed", error.to_string()))?;
    durable_create(&path, &bytes, "unpark_receipt_write_failed")
}

fn durable_create(path: &Path, bytes: &[u8], code: &'static str) -> Result<(), AppError> {
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .map_err(|error| AppError::validation(code, error.to_string()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| AppError::validation(code, error.to_string()))?;
    fs::rename(&temp, path).map_err(|error| AppError::validation(code, error.to_string()))
}

fn refusal(
    code: &'static str,
    message: &'static str,
    status: &StatusOutput,
    input: &UnparkInput,
) -> AppError {
    AppError::structured(
        ErrorCategory::Validation,
        code,
        message,
        Some(
            json!({"repository": status.repository, "pr": input.pr, "expected_head": input.head, "expected_base": input.base, "mutated": false}),
        ),
    )
}
fn provider_error(error: &MutationError, input: &UnparkInput) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "unpark_provider_mutation_failed",
        error.to_string(),
        Some(json!({"pr": input.pr, "head": input.head, "mutated": false, "resumable": true})),
    )
}
fn postcondition(
    code: &'static str,
    message: &'static str,
    input: &UnparkInput,
    receipt: &GitHubMutationReceipt,
    status: &StatusOutput,
) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        code,
        message,
        Some(
            json!({"pr": input.pr, "head": input.head, "provider_receipt": receipt, "post_status": status, "mutated": true, "resumable": false, "next": "stop and inspect drift; never rejoin or edit labels directly"}),
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use mcp_cli::StructuredError;

    use super::*;
    use crate::model::{
        AutoMergeState, BranchSnapshot, CheckState, CommitOid, CompatibilityOutcome,
        PullRequestSnapshot, RepositorySnapshot,
    };

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        }
    }

    fn branch(name: &str, oid: char) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository(),
            name: name.to_owned(),
            oid: CommitOid(std::iter::repeat_n(oid, 40).collect()),
        }
    }

    fn check(name: &str, state: CheckState, run: u64) -> CheckSnapshot {
        CheckSnapshot {
            name: name.to_owned(),
            state,
            provider_kind: Some("CheckRun".to_owned()),
            workflow_name: Some("CI".to_owned()),
            details_url: Some(format!("https://example.invalid/actions/runs/{run}/job/1")),
            started_at: Some(format!("2026-08-05T05:{run:02}:00Z")),
            completed_at: Some(format!("2026-08-05T05:{run:02}:30Z")),
            ..CheckSnapshot::default()
        }
    }

    fn pull(checks: Vec<CheckSnapshot>) -> PullRequestSnapshot {
        PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(1),
            title: "parked".to_owned(),
            url: "https://example.invalid/1".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch("topic", 'a'),
            base: branch("main", 'b'),
            cross_repository: false,
            labels: BTreeSet::from(["caravan".to_owned(), PARKED_LABEL.to_owned()]),
            auto_merge: AutoMergeState::disabled(),
            checks,
            created_at: None,
            merged_at: None,
            updated_at: None,
        }
    }

    fn status(pull: PullRequestSnapshot) -> StatusOutput {
        let snapshot = RepositorySnapshot {
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            repository: repository(),
            default_branch: branch("main", 'b'),
            current_branch: Some("topic".to_owned()),
            current_pr: Some(PrNumber(1)),
            pull_requests: vec![pull],
            generation_facts: Vec::new(),
            observed_at: None,
        };
        let checker = |_candidate: &BranchSnapshot, target: &BranchSnapshot| {
            Ok(crate::model::CompatibilityReport {
                candidate: branch("topic", 'a'),
                target: target.clone(),
                outcome: CompatibilityOutcome::Clean,
                conflicting_paths: Vec::new(),
                diagnostic: None,
            })
        };
        let analysis = crate::graph::analyze_for_actor(
            &snapshot,
            &checker,
            crate::model::HeadMergeActor::Caravan,
        )
        .unwrap();
        StatusOutput {
            runtime: crate::read::RuntimeProvenance::default(),
            config_provenance: None,
            provider_api: crate::model::GitHubApiTelemetry::default(),
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            timing: None,
            repository: repository(),
            rebase_on_join: crate::read::RebaseOnJoinStatus::default(),
            stack_backend: crate::read::StackBackendStatus::default(),
            head_merge: crate::read::HeadMergeStatus::default(),
            auto_admission: crate::read::AutoAdmissionStatus::default(),
            sync_budget: crate::sync::SyncBudgetStatus::default(),
            default_branch: "main".to_owned(),
            current_branch: snapshot.current_branch,
            current_pr: snapshot.current_pr,
            healthy: analysis.healthy(),
            initialization: crate::initialization::InitializationStatus::default(),
            admission: crate::read::resolve_admission(&analysis, &[]),
            analysis,
            pauses: Vec::new(),
        }
    }

    fn input(status: &StatusOutput) -> UnparkInput {
        let caravan = status.analysis.fleet.caravan(PrNumber(1)).unwrap();
        UnparkInput {
            repository: "owner/repo".to_owned(),
            pr: 1,
            head: "a".repeat(40),
            base_ref: "main".to_owned(),
            base: "b".repeat(40),
            membership_generation: membership_generation(status, caravan).unwrap(),
            parking_fingerprint: "fnv1a64:0123456789abcdef".to_owned(),
            provider_state: "open_parked".to_owned(),
            actor: "oncall".to_owned(),
            reason: "exact head recovered".to_owned(),
        }
    }

    fn receipt(before: &PullRequestSnapshot, after: &PullRequestSnapshot) -> GitHubMutationReceipt {
        GitHubMutationReceipt {
            kind: MutationKind::RemoveLabel,
            before: Some(before.clone()),
            after: after.clone(),
            provider_output: None,
        }
    }

    #[test]
    fn green_recovery_preserves_membership_and_removes_only_parking() {
        let initial = status(pull(vec![check("CI", CheckState::Success, 2)]));
        let input = input(&initial);
        let prepared = prepare(&initial, &input, &input.parking_fingerprint).unwrap();
        let mut final_pull = prepared.pull.clone();
        final_pull.labels.remove(PARKED_LABEL);
        let final_status = status(final_pull.clone());
        let output = finish(
            &initial,
            &final_status,
            &input,
            prepared.clone(),
            receipt(&prepared.pull, &final_pull),
        )
        .unwrap();
        assert!(output.mutated);
        assert!(output.new_labels.contains("caravan"));
        assert!(!output.new_labels.contains(PARKED_LABEL));
    }

    #[test]
    fn still_red_refuses_without_mutation() {
        let status = status(pull(vec![check("CI", CheckState::Failure, 2)]));
        let input = input(&status);
        assert_eq!(
            prepare(&status, &input, &input.parking_fingerprint)
                .unwrap_err()
                .code(),
            "unpark_ci_not_green"
        );
    }

    #[test]
    fn newer_green_supersedes_older_red() {
        let status = status(pull(vec![
            check("CI", CheckState::Failure, 1),
            check("CI", CheckState::Success, 2),
        ]));
        let input = input(&status);
        let prepared = prepare(&status, &input, &input.parking_fingerprint).unwrap();
        assert_eq!(prepared.authoritative_checks[0].state, CheckState::Success);
        assert_eq!(prepared.superseded_checks[0].state, CheckState::Failure);
    }

    #[test]
    fn head_and_base_drift_refuse() {
        for field in ["head", "base"] {
            let status = status(pull(vec![check("CI", CheckState::Success, 2)]));
            let mut input = input(&status);
            if field == "head" {
                input.head = "c".repeat(40);
            } else {
                input.base = "c".repeat(40);
            }
            assert_eq!(
                prepare(&status, &input, &input.parking_fingerprint)
                    .unwrap_err()
                    .code(),
                "unpark_generation_drift"
            );
        }
    }

    #[test]
    fn explicit_pause_present_refuses() {
        let mut status = status(pull(vec![check("CI", CheckState::Success, 2)]));
        let current = status.analysis.pull_requests[&PrNumber(1)].clone();
        status.pauses.push(crate::pause::PauseStatus {
            record: crate::pause::PauseRecord {
                version: 1,
                caravan_head: PrNumber(1),
                members: vec![PrNumber(1)],
                expected_head: PullRequestPrecondition::from(&current),
                expected_checks: current.checks.clone(),
                actor: "incident".to_owned(),
                reason: "hold".to_owned(),
                paused_unix_secs: 1,
                expires_unix_secs: None,
                external_reference: None,
                resume_authorized_by: None,
                recovery: None,
            },
            state: crate::pause::PauseState::Active,
            auto_merge_suspended: true,
            retired_state: None,
            safe_next_action: "resume explicitly".to_owned(),
        });
        let input = input(&status);
        assert_eq!(
            prepare(&status, &input, &input.parking_fingerprint)
                .unwrap_err()
                .code(),
            "unpark_explicit_pause_present"
        );
    }

    #[test]
    fn evicted_and_missing_parking_label_refuse() {
        let clean = status(pull(vec![check("CI", CheckState::Success, 2)]));
        let input = input(&clean);
        for label in ["caravan-evicted", "missing-parking"] {
            let mut current = pull(vec![check("CI", CheckState::Success, 2)]);
            if label == "caravan-evicted" {
                current.labels.insert(label.to_owned());
            } else {
                current.labels.remove(PARKED_LABEL);
            }
            let status = status(current);
            let code = prepare(&status, &input, &input.parking_fingerprint)
                .unwrap_err()
                .code();
            assert!(matches!(
                code.as_str(),
                "unpark_membership_ineligible" | "unpark_parking_label_missing"
            ));
        }
    }

    #[test]
    fn already_unparked_exact_retry_returns_original_receipt() {
        let directory = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(directory.path())
            .status()
            .unwrap();
        let initial = status(pull(vec![check("CI", CheckState::Success, 2)]));
        let input = input(&initial);
        let prepared = prepare(&initial, &input, &input.parking_fingerprint).unwrap();
        let mut final_pull = prepared.pull.clone();
        final_pull.labels.remove(PARKED_LABEL);
        let output = finish(
            &initial,
            &status(final_pull.clone()),
            &input,
            prepared.clone(),
            receipt(&prepared.pull, &final_pull),
        )
        .unwrap();
        write_receipt(directory.path(), &input, &output).unwrap();
        assert_eq!(
            load_receipt(directory.path(), &input).unwrap(),
            Some(output)
        );
    }

    #[test]
    fn interrupted_mutation_reuses_durable_prepared_receipt_identity() {
        let directory = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(directory.path())
            .status()
            .unwrap();
        let initial = status(pull(vec![check("CI", CheckState::Success, 2)]));
        let input = input(&initial);
        let prepared = prepare(&initial, &input, &input.parking_fingerprint).unwrap();
        write_pending(directory.path(), &input, &prepared).unwrap();
        let replay = load_pending(directory.path(), &input).unwrap().unwrap();
        assert_eq!(replay.receipt_id, prepared.receipt_id);
        let mut recovered_pull = prepared.pull.clone();
        recovered_pull.labels.remove(PARKED_LABEL);
        let recovered_status = status(recovered_pull.clone());
        let output = finish(
            &recovered_status,
            &recovered_status,
            &input,
            replay,
            receipt(&prepared.pull, &recovered_pull),
        )
        .unwrap();
        assert_eq!(output.receipt_id, prepared.receipt_id);
    }

    #[test]
    fn concurrent_writer_lock_refuses_before_provider_access() {
        let directory = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(directory.path())
            .status()
            .unwrap();
        let _held =
            crate::operation_lock::OperationLock::acquire(directory.path(), "sync").unwrap();
        let context = AppContext {
            repository_path: directory.path().to_path_buf(),
            ..AppContext::default()
        };
        let status = status(pull(vec![check("CI", CheckState::Success, 2)]));
        let input = input(&status);
        assert!(unpark(&context, &input).is_err());
    }

    #[test]
    fn parking_provenance_mismatch_refuses() {
        let status = status(pull(vec![check("CI", CheckState::Success, 2)]));
        let input = input(&status);
        assert_eq!(
            prepare(&status, &input, "fnv1a64:ffffffffffffffff")
                .unwrap_err()
                .code(),
            "unpark_parking_provenance_drift"
        );
    }
}
