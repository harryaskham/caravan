//! Audited recovery for a parked generation whose lifecycle labels were stripped.
//!
//! This is intentionally narrower than admission or `unpark`: it consumes the
//! latest durable engine-owned parking event, binds the exact open provider
//! generation, and restores `caravan` plus `caravan-parked` in one complete-label
//! write. It never creates a replacement PR, changes topology, or activates the
//! parked caravan.

use std::collections::BTreeSet;
use std::time::Duration;

use clap::Args;
use mcp_cli::{ErrorCategory, StructuredError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::command::ProcessRunner;
use crate::github::{GitHubMutationAdapter, GitHubMutationReceipt, MutationError};
use crate::journal::{JournalRecord, LogInput};
use crate::model::{
    Caravan, CaravanEvent, EventKind, MutationKind, MutationStep, MutationStepState, OperationId,
    OperationReceipt, PrNumber, PullRequestPrecondition, PullRequestSnapshot, PullRequestState,
    RepositoryId,
};
use crate::read::StatusOutput;
use crate::{AppContext, AppError};

const ACTIVE_LABEL: &str = "caravan";
const PARKED_LABEL: &str = "caravan-parked";
const CLOSED_LABEL: &str = "caravan-closed";
const MAX_TEXT: usize = 2_000;

/// Reviewed authority to restore one exact engine-owned parked generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Args)]
pub struct RestoreParkedInput {
    /// Exact owner/name provider repository.
    #[arg(long = "repository-slug", value_name = "OWNER/NAME")]
    pub repository: String,
    /// Exact parked Caravan head PR whose labels are missing.
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
    /// Exact prior membership generation derived from durable parking evidence.
    #[arg(long)]
    pub membership_generation: String,
    /// Fingerprint from the latest durable `caravan_parked` journal event.
    #[arg(long)]
    pub parking_fingerprint: String,
    /// Reviewed current provider state. Only `open_labels_missing` is accepted.
    #[arg(long)]
    pub provider_state: String,
    /// Audited human or agent identity.
    #[arg(long)]
    pub actor: String,
    /// Bounded recovery rationale.
    #[arg(long)]
    pub reason: String,
}

/// Durable exact-generation parked-membership restoration receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RestoreParkedOutput {
    pub schema_version: u32,
    pub receipt_id: String,
    pub repository: RepositoryId,
    pub pr: PrNumber,
    pub head: String,
    pub base_ref: String,
    pub base: String,
    pub membership_generation: String,
    pub parking_fingerprint: String,
    pub parking_event_id: String,
    pub actor: String,
    pub reason: String,
    pub old_state: String,
    pub new_state: String,
    pub old_labels: BTreeSet<String>,
    pub new_labels: BTreeSet<String>,
    pub mutated: bool,
    pub receipt: OperationReceipt,
    #[serde(default)]
    pub provider_receipts: Vec<GitHubMutationReceipt>,
    pub next: String,
}

#[derive(Debug, Clone)]
struct Prepared {
    pull: PullRequestSnapshot,
    desired_labels: BTreeSet<String>,
    caravan: Caravan,
    event: CaravanEvent,
    membership_generation: String,
    receipt_id: String,
}

/// Restore the exact active and parking labels removed by a stale closed-row
/// cleanup. Durable parking provenance remains the sole topology authority.
// Keep the lease, provider write, rediscovery, and receipt checkpoints visible
// in one audited linear transaction; splitting it would obscure partial-state
// evidence and exact postcondition ordering.
#[allow(clippy::too_many_lines)]
pub fn restore_parked(
    context: &AppContext,
    input: &RestoreParkedInput,
) -> Result<RestoreParkedOutput, AppError> {
    validate_input(input)?;
    let mut lock = context.acquire_writer_operation("restore_parked")?;
    let initial = crate::read::status(context)?;
    crate::initialization::require_ready(&initial.initialization)?;
    let event = latest_parking_event(context, input)?;
    let prepared = prepare(&initial, input, event)?;

    let runner = ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(Duration::from_secs(context.config.command_timeout_secs));
    let provider = GitHubMutationAdapter::new(lock.runner(runner));
    let fresh = provider
        .refetch_pull_request(&initial.repository, prepared.pull.number)
        .map_err(|error| provider_read_error(&error, input))?;
    if !PullRequestPrecondition::from(&prepared.pull)
        .mutation_identity_eq(&PullRequestPrecondition::from(&fresh))
    {
        return Err(refusal(
            "restore_parked_provider_drift",
            "provider state changed after reviewed restore planning",
            input,
            json!({"planned": prepared.pull, "fresh": fresh}),
        ));
    }

    lock.checkpoint(
        "restore_parked_provider_mutation_in_flight",
        json!({
            "receipt_id": prepared.receipt_id,
            "repository": input.repository,
            "pr": input.pr,
            "head": input.head,
            "base_ref": input.base_ref,
            "base": input.base,
            "membership_generation": prepared.membership_generation,
            "parking_fingerprint": input.parking_fingerprint,
            "parking_event_id": prepared.event.event_id,
            "desired_labels": prepared.desired_labels,
        }),
        true,
    )?;

    let old_labels = fresh.labels.clone();
    let provider_receipt = if fresh.labels == prepared.desired_labels {
        None
    } else {
        Some(
            provider
                .replace_labels(
                    &initial.repository,
                    &PullRequestPrecondition::from(&fresh),
                    &prepared.desired_labels,
                )
                .map_err(|error| provider_mutation_error(&error, input))?,
        )
    };
    let mutated = provider_receipt.is_some();
    let after = provider_receipt
        .as_ref()
        .map_or_else(|| fresh.clone(), |receipt| receipt.after.clone());
    validate_provider_postcondition(&after, input, &prepared.desired_labels).map_err(
        |message| {
            refusal(
                "restore_parked_provider_postcondition_failed",
                &message,
                input,
                json!({"before": fresh, "after": after, "provider_receipt": provider_receipt, "mutated": mutated}),
            )
        },
    )?;

    // Fresh full discovery proves the old parked topology reappeared and did
    // not consume active capacity before the writer lease is released.
    let final_status = crate::read::status(context).map_err(|error| {
        AppError::structured(
            error.category(),
            "restore_parked_rediscovery_failed",
            "parked labels were restored but authoritative topology rediscovery failed",
            Some(json!({
                "source": error.details(),
                "provider_receipt": provider_receipt,
                "mutated": mutated,
                "resumable": true,
                "next": "rerun the exact restore-parked request; never raw-edit labels or create a replacement generation",
            })),
        )
    })?;
    let final_pull = final_status
        .analysis
        .pull_requests
        .get(&PrNumber(input.pr))
        .ok_or_else(|| {
            refusal(
                "restore_parked_rediscovery_missing",
                "restored PR is absent from authoritative post-mutation discovery",
                input,
                json!({"provider_receipt": provider_receipt, "mutated": mutated}),
            )
        })?;
    validate_provider_postcondition(final_pull, input, &prepared.desired_labels).map_err(
        |message| {
            refusal(
                "restore_parked_rediscovery_drift",
                &message,
                input,
                json!({"provider_receipt": provider_receipt, "fresh": final_pull, "mutated": mutated}),
            )
        },
    )?;
    let restored = final_status
        .analysis
        .fleet
        .caravan(PrNumber(input.pr))
        .ok_or_else(|| {
            refusal(
                "restore_parked_topology_missing",
                "restored labels did not recover the durable parked caravan topology",
                input,
                json!({"expected_caravan": prepared.caravan, "provider_receipt": provider_receipt, "mutated": mutated}),
            )
        })?;
    if restored.members != prepared.caravan.members
        || !final_pull.has_label(PARKED_LABEL)
        || !final_status
            .auto_admission
            .parked_caravan_ids
            .contains(&PrNumber(input.pr))
        || final_status
            .auto_admission
            .active_caravan_ids
            .contains(&PrNumber(input.pr))
    {
        return Err(refusal(
            "restore_parked_topology_drift",
            "fresh parked topology differs from durable parking evidence",
            input,
            json!({
                "expected_caravan": prepared.caravan,
                "actual_caravan": restored,
                "provider_receipt": provider_receipt,
                "mutated": mutated,
            }),
        ));
    }

    let step = MutationStep {
        kind: MutationKind::SetLabels,
        state: if mutated {
            MutationStepState::Completed
        } else {
            MutationStepState::AlreadySatisfied
        },
        pr: Some(PrNumber(input.pr)),
        summary:
            "restored exact active and parked lifecycle labels from durable parking provenance"
                .to_owned(),
    };
    let output = RestoreParkedOutput {
        schema_version: 1,
        receipt_id: prepared.receipt_id.clone(),
        repository: initial.repository,
        pr: PrNumber(input.pr),
        head: input.head.clone(),
        base_ref: input.base_ref.clone(),
        base: input.base.clone(),
        membership_generation: prepared.membership_generation,
        parking_fingerprint: input.parking_fingerprint.clone(),
        parking_event_id: prepared.event.event_id.0,
        actor: input.actor.clone(),
        reason: input.reason.clone(),
        old_state: "open_labels_missing".to_owned(),
        new_state: "open_parked".to_owned(),
        old_labels,
        new_labels: final_pull.labels.clone(),
        mutated,
        receipt: OperationReceipt {
            operation_id: OperationId(prepared.receipt_id),
            operation: "restore_parked".to_owned(),
            completed_steps: vec![step],
            changed: mutated,
        },
        provider_receipts: provider_receipt.into_iter().collect(),
        next: "run `cara status`, then `cara sync --all`; the same PR generation remains parked and no replacement was created".to_owned(),
    };
    lock.checkpoint(
        "restore_parked_converged",
        json!({
            "receipt_id": output.receipt_id,
            "parking_event_id": output.parking_event_id,
            "provider_receipts": output.provider_receipts,
            "mutated": output.mutated,
        }),
        false,
    )?;
    Ok(output)
}

fn prepare(
    status: &StatusOutput,
    input: &RestoreParkedInput,
    event: CaravanEvent,
) -> Result<Prepared, AppError> {
    if status.repository.slug() != input.repository || event.repository != status.repository {
        return Err(refusal(
            "restore_parked_repository_drift",
            "fresh provider or parking-event repository differs from reviewed authority",
            input,
            json!({"fresh_repository": status.repository, "event_repository": event.repository}),
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
                "restore_parked_pr_not_found",
                "PR is absent from fresh provider discovery",
                input,
                json!({}),
            )
        })?;
    validate_restore_candidate(&pull, input)?;
    let fleet = event.fleet.as_ref().ok_or_else(|| {
        refusal(
            "restore_parked_event_incomplete",
            "parking event has no durable fleet topology",
            input,
            json!({"parking_event": event}),
        )
    })?;
    let caravan = fleet.caravan(pr).cloned().ok_or_else(|| {
        refusal(
            "restore_parked_event_topology_missing",
            "parking event does not contain the reviewed caravan head",
            input,
            json!({"parking_event": event}),
        )
    })?;
    let generation = membership_generation(status, &caravan)?;
    if generation != input.membership_generation {
        return Err(refusal(
            "restore_parked_membership_drift",
            "fresh exact members differ from the reviewed parked generation",
            input,
            json!({"expected": input.membership_generation, "actual": generation, "caravan": caravan}),
        ));
    }
    if !parking_event_proves_prior_labels(&event, input) {
        return Err(refusal(
            "restore_parked_event_provider_proof_missing",
            "parking event does not prove both lifecycle labels on this exact provider generation",
            input,
            json!({"parking_event": event}),
        ));
    }
    let mut desired_labels = pull.labels.clone();
    desired_labels.insert(ACTIVE_LABEL.to_owned());
    desired_labels.insert(PARKED_LABEL.to_owned());
    let receipt_id = crate::membership::fnv1a64(
        &serde_json::to_vec(&json!({
            "schema_version": 1,
            "operation": "restore_parked",
            "input": input,
            "event_id": event.event_id,
            "desired_labels": desired_labels,
        }))
        .expect("restore identity serializes"),
    );
    Ok(Prepared {
        pull,
        desired_labels,
        caravan,
        event,
        membership_generation: generation,
        receipt_id,
    })
}

fn parking_event_proves_prior_labels(event: &CaravanEvent, input: &RestoreParkedInput) -> bool {
    event
        .metadata
        .get("provider_receipts")
        .and_then(|value| serde_json::from_value::<Vec<GitHubMutationReceipt>>(value.clone()).ok())
        .is_some_and(|receipts| {
            receipts.iter().any(|receipt| {
                receipt.after.number == PrNumber(input.pr)
                    && receipt.after.head.oid.0 == input.head
                    && receipt.after.base.name == input.base_ref
                    && receipt.after.base.oid.0 == input.base
                    && receipt.after.has_label(ACTIVE_LABEL)
                    && receipt.after.has_label(PARKED_LABEL)
            })
        })
}

fn validate_restore_candidate(
    pull: &PullRequestSnapshot,
    input: &RestoreParkedInput,
) -> Result<(), AppError> {
    if pull.state != PullRequestState::Open || pull.merged_at.is_some() {
        return Err(refusal(
            "restore_parked_pr_not_open",
            "only an exact open, unmerged PR can regain parked membership",
            input,
            json!({"fresh": pull}),
        ));
    }
    if pull.head.oid.0 != input.head
        || pull.base.name != input.base_ref
        || pull.base.oid.0 != input.base
    {
        return Err(refusal(
            "restore_parked_generation_drift",
            "provider head or base differs from reviewed parking generation",
            input,
            json!({"fresh": pull}),
        ));
    }
    if pull.draft || pull.cross_repository {
        return Err(refusal(
            "restore_parked_provider_shape_invalid",
            "draft or cross-repository PRs cannot regain parked membership",
            input,
            json!({"fresh": pull}),
        ));
    }
    if pull.auto_merge.enabled {
        return Err(refusal(
            "restore_parked_auto_merge_enabled",
            "a parked generation must remain disarmed during label restoration",
            input,
            json!({"fresh": pull}),
        ));
    }
    if pull.has_label(CLOSED_LABEL) || pull.has_label("caravan-evicted") {
        return Err(refusal(
            "restore_parked_conflicting_lifecycle_label",
            "terminal or evicted lifecycle evidence forbids parked restoration",
            input,
            json!({"fresh": pull}),
        ));
    }
    let active = pull.has_label(ACTIVE_LABEL);
    let parked = pull.has_label(PARKED_LABEL);
    if active != parked {
        return Err(refusal(
            "restore_parked_partial_labels",
            "exactly one lifecycle label is present; refuse to normalize ambiguous provider state",
            input,
            json!({"fresh": pull}),
        ));
    }
    Ok(())
}

fn validate_provider_postcondition(
    pull: &PullRequestSnapshot,
    input: &RestoreParkedInput,
    desired_labels: &BTreeSet<String>,
) -> Result<(), String> {
    if pull.state != PullRequestState::Open || pull.merged_at.is_some() {
        return Err("provider no longer reports the exact open, unmerged generation".to_owned());
    }
    if pull.head.oid.0 != input.head
        || pull.base.name != input.base_ref
        || pull.base.oid.0 != input.base
    {
        return Err("provider head or base drifted during restoration".to_owned());
    }
    if &pull.labels != desired_labels
        || !pull.has_label(ACTIVE_LABEL)
        || !pull.has_label(PARKED_LABEL)
    {
        return Err("complete active-plus-parked label postcondition was not preserved".to_owned());
    }
    if pull.draft || pull.cross_repository {
        return Err("restored generation became draft or cross-repository".to_owned());
    }
    if pull.auto_merge.enabled {
        return Err("restored parked generation unexpectedly has auto-merge enabled".to_owned());
    }
    Ok(())
}

fn latest_parking_event(
    context: &AppContext,
    input: &RestoreParkedInput,
) -> Result<CaravanEvent, AppError> {
    let snapshot = crate::journal::snapshot(
        context,
        &LogInput {
            limit: 10_000,
            kind: None,
            pr: Some(input.pr),
            since: None,
            until: None,
        },
    )?;
    let event = snapshot
        .records
        .iter()
        .rev()
        .find_map(|record| match record {
            JournalRecord::Event { event, .. }
                if event.caravan_id == Some(PrNumber(input.pr))
                    && matches!(
                        event.kind,
                        EventKind::CaravanParked | EventKind::CaravanUnparked
                    ) =>
            {
                Some(event.clone())
            }
            _ => None,
        });
    let Some(event) = event else {
        return Err(refusal(
            "restore_parked_provenance_not_found",
            "no durable parking lifecycle event matches this PR",
            input,
            json!({"journal": snapshot.source}),
        ));
    };
    if event.kind != EventKind::CaravanParked {
        return Err(refusal(
            "restore_parked_provenance_retired",
            "latest durable parking lifecycle event is an unpark",
            input,
            json!({"parking_event": event}),
        ));
    }
    let fingerprint = event
        .metadata
        .get("fingerprint")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if fingerprint != input.parking_fingerprint {
        return Err(refusal(
            "restore_parked_provenance_drift",
            "latest durable parking fingerprint differs from reviewed authority",
            input,
            json!({"parking_event": event}),
        ));
    }
    Ok(event)
}

fn membership_generation(status: &StatusOutput, caravan: &Caravan) -> Result<String, AppError> {
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
                        "restore_parked_membership_incomplete",
                        format!("fresh provider discovery is missing PR #{number}"),
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

fn validate_input(input: &RestoreParkedInput) -> Result<(), AppError> {
    if input.pr == 0 {
        return Err(AppError::validation(
            "restore_parked_pr_invalid",
            "--pr must be non-zero",
        ));
    }
    for (name, oid) in [("head", &input.head), ("base", &input.base)] {
        if oid.len() != 40 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(AppError::validation(
                "restore_parked_oid_invalid",
                format!("--{name} must be one exact 40-character hexadecimal OID"),
            ));
        }
    }
    if input.repository.split_once('/').is_none() {
        return Err(AppError::validation(
            "restore_parked_repository_invalid",
            "--repository-slug must be exact owner/name",
        ));
    }
    if input.base_ref.trim().is_empty() || input.base_ref.len() > MAX_TEXT {
        return Err(AppError::validation(
            "restore_parked_base_ref_invalid",
            "--base-ref must name the exact current provider base branch",
        ));
    }
    if input.provider_state != "open_labels_missing" {
        return Err(AppError::validation(
            "restore_parked_provider_state_invalid",
            "--provider-state must be exactly open_labels_missing",
        ));
    }
    for (name, value) in [
        ("membership-generation", &input.membership_generation),
        ("parking-fingerprint", &input.parking_fingerprint),
        ("actor", &input.actor),
        ("reason", &input.reason),
    ] {
        if value.trim().is_empty() || value.len() > MAX_TEXT {
            return Err(AppError::validation(
                "restore_parked_input_invalid",
                format!("--{name} must be non-empty and at most {MAX_TEXT} bytes"),
            ));
        }
    }
    Ok(())
}

fn provider_read_error(error: &MutationError, input: &RestoreParkedInput) -> AppError {
    refusal(
        "restore_parked_provider_read_failed",
        "fresh authoritative provider state could not be read; no labels were written",
        input,
        json!({"source": error.to_string()}),
    )
}

fn provider_mutation_error(error: &MutationError, input: &RestoreParkedInput) -> AppError {
    refusal(
        "restore_parked_provider_mutation_failed",
        "complete parked-label replacement failed with an indeterminate provider outcome",
        input,
        json!({
            "source": error.to_string(),
            "mutated": "unknown",
            "provider_outcome": "indeterminate",
            "next": "freshly inspect the exact PR before retrying; never raw-edit labels or create a replacement generation",
        }),
    )
}

// Structured details are assembled as an owned JSON object at each refusal
// site, then merged here without a second clone at the caller.
#[allow(clippy::needless_pass_by_value)]
fn refusal(
    code: &'static str,
    message: &str,
    input: &RestoreParkedInput,
    extra: Value,
) -> AppError {
    let mut details = json!({
        "repository": input.repository,
        "pr": input.pr,
        "head": input.head,
        "base_ref": input.base_ref,
        "base": input.base,
        "membership_generation": input.membership_generation,
        "parking_fingerprint": input.parking_fingerprint,
        "provider_state": input.provider_state,
        "mutated": false,
        "resumable": true,
        "next": "refresh exact Cara status and parking-event evidence; never raw-edit labels or create a replacement generation",
    });
    if let (Some(target), Some(source)) = (details.as_object_mut(), extra.as_object()) {
        target.extend(source.clone());
    }
    AppError::structured(ErrorCategory::Validation, code, message, Some(details))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::model::{AutoMergeState, BranchSnapshot, CommitOid};

    fn input() -> RestoreParkedInput {
        RestoreParkedInput {
            repository: "acme/widgets".to_owned(),
            pr: 2451,
            head: "a".repeat(40),
            base_ref: "main".to_owned(),
            base: "b".repeat(40),
            membership_generation: "generation".to_owned(),
            parking_fingerprint: "fingerprint".to_owned(),
            provider_state: "open_labels_missing".to_owned(),
            actor: "cara-recovery".to_owned(),
            reason: "restore labels stripped by stale closed cleanup".to_owned(),
        }
    }

    fn pull(labels: &[&str]) -> PullRequestSnapshot {
        let repository = RepositoryId {
            owner: "acme".to_owned(),
            name: "widgets".to_owned(),
        };
        PullRequestSnapshot {
            number: PrNumber(2451),
            title: "parked".to_owned(),
            url: "https://example.test/pr/2451".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: BranchSnapshot {
                repository: repository.clone(),
                name: "feature".to_owned(),
                oid: CommitOid("a".repeat(40)),
            },
            base: BranchSnapshot {
                repository,
                name: "main".to_owned(),
                oid: CommitOid("b".repeat(40)),
            },
            cross_repository: false,
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
            merge_state_status: Some("BEHIND".to_owned()),
        }
    }

    #[test]
    fn exact_open_missing_labels_is_the_only_repair_candidate() {
        validate_restore_candidate(&pull(&[]), &input()).expect("exact stripped row is eligible");

        let error = validate_restore_candidate(&pull(&[ACTIVE_LABEL]), &input())
            .expect_err("partial labels are ambiguous");
        assert_eq!(error.code(), "restore_parked_partial_labels");
    }

    #[test]
    fn terminal_or_reopened_generation_cannot_be_repaired_as_parked() {
        let error = validate_restore_candidate(&pull(&[CLOSED_LABEL]), &input())
            .expect_err("terminal evidence refuses restore");
        assert_eq!(error.code(), "restore_parked_conflicting_lifecycle_label");

        let mut closed = pull(&[]);
        closed.state = PullRequestState::Closed;
        let error = validate_restore_candidate(&closed, &input())
            .expect_err("closed provider state refuses restore");
        assert_eq!(error.code(), "restore_parked_pr_not_open");
    }

    #[test]
    fn parking_event_must_prove_both_prior_labels_on_the_exact_generation() {
        let input = input();
        let mut after = pull(&[ACTIVE_LABEL, PARKED_LABEL]);
        let receipt = GitHubMutationReceipt {
            kind: MutationKind::AddLabel,
            before: None,
            after: after.clone(),
            provider_output: None,
        };
        let mut event = CaravanEvent {
            version: 1,
            event_id: crate::model::EventId::new(),
            operation_id: OperationId::new(),
            kind: EventKind::CaravanParked,
            repository: after.head.repository.clone(),
            caravan_id: Some(after.number),
            prs: vec![after.number],
            fleet: None,
            reason: None,
            metadata: BTreeMap::from([(
                "provider_receipts".to_owned(),
                serde_json::to_value(vec![receipt]).unwrap(),
            )]),
            timestamp: "2026-08-05T00:00:00Z".to_owned(),
        };
        assert!(parking_event_proves_prior_labels(&event, &input));

        after.labels.remove(PARKED_LABEL);
        event.metadata.insert(
            "provider_receipts".to_owned(),
            serde_json::to_value(vec![GitHubMutationReceipt {
                kind: MutationKind::RemoveLabel,
                before: None,
                after,
                provider_output: None,
            }])
            .unwrap(),
        );
        assert!(!parking_event_proves_prior_labels(&event, &input));
    }
}
