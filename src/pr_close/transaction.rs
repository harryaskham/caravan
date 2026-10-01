//! One close attempt per retained intent; all later calls are reconciliation.

use std::path::Path;

use mcp_cli::{ErrorCategory, StructuredError};
use sha2::{Digest, Sha256};

use crate::config::WriterMode;
use crate::model::PullRequestState;
use crate::{AppError, stack_checkpoint};

use super::{CloseFacts, CloseOutcome, PrCloseInput, PrCloseOutput, PrCloseRecord, fingerprint};

pub(super) trait CloseProvider {
    fn observe(&self, input: &PrCloseInput) -> Result<CloseFacts, AppError>;
    fn revalidate_fence(&self) -> Result<String, AppError>;
    fn close_once(&self, facts: &CloseFacts) -> Result<(), AppError>;
}

pub(super) trait ReceiptStore {
    fn load(&self) -> Result<Option<PrCloseRecord>, AppError>;
    fn save(&self, record: &PrCloseRecord) -> Result<(), AppError>;
}

pub(super) struct CheckpointStore<'a> {
    repository: &'a Path,
    key: String,
}

impl<'a> CheckpointStore<'a> {
    pub(super) fn new(repository: &'a Path, operation_key: &str) -> Self {
        Self {
            repository,
            key: format!("pr-close-v1-{:x}", Sha256::digest(operation_key.as_bytes())),
        }
    }
}

impl ReceiptStore for CheckpointStore<'_> {
    fn load(&self) -> Result<Option<PrCloseRecord>, AppError> {
        stack_checkpoint::load(self.repository, &self.key).map_err(|error| {
            AppError::structured(
                ErrorCategory::ExecutionFailure,
                "pr_close_receipt_read_failed",
                "retained close receipt could not be read",
                Some(serde_json::json!({"storage_code": error.code()})),
            )
        })
    }

    fn save(&self, record: &PrCloseRecord) -> Result<(), AppError> {
        stack_checkpoint::write_private(self.repository, &self.key, record).map_err(|error| {
            AppError::structured(ErrorCategory::ExecutionFailure, "pr_close_receipt_write_failed", "close receipt persistence failed; preserve intent and reconcile before another action", Some(serde_json::json!({"storage_code": error.code(), "intent_recorded": record.intent.is_some()})))
        })
    }
}

pub(super) fn load_record(repository: &Path, key: &str) -> Result<Option<PrCloseRecord>, AppError> {
    CheckpointStore::new(repository, key).load()
}

pub(super) fn execute(
    provider: &impl CloseProvider,
    store: &impl ReceiptStore,
    input: &PrCloseInput,
    policy_fingerprint: &str,
    writer_mode: WriterMode,
) -> Result<PrCloseOutput, AppError> {
    let mut record = if let Some(record) = store.load()? {
        if record.schema_version != 1
            || record.request != *input
            || record.policy_fingerprint != policy_fingerprint
            || record.writer_mode != writer_mode
        {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "pr_close_retry_identity_changed",
                "operation key is already bound to a different request or policy; inspect its retained receipt",
                Some(
                    serde_json::json!({"close_intent_recorded": record.intent.is_some(), "new_close_attempted": false}),
                ),
            ));
        }
        record
    } else {
        let record = PrCloseRecord {
            schema_version: 1,
            request: input.clone(),
            policy_fingerprint: policy_fingerprint.to_owned(),
            writer_mode,
            intent: None,
            intent_fence_fingerprint: None,
            write_returned_success: None,
            write_error_code: None,
            first_result: None,
            latest_result: None,
        };
        store.save(&record)?;
        record
    };
    if record.intent.is_some() {
        return reconcile(provider, store, &mut record, false);
    }
    let before = match preflight(provider, store, &mut record)? {
        Preflight::Ready(facts) => *facts,
        Preflight::Complete(output) => return Ok(*output),
    };
    let immediate = match preflight(provider, store, &mut record)? {
        Preflight::Ready(facts) => *facts,
        Preflight::Complete(output) => return Ok(*output),
    };
    if before != immediate {
        return finish(
            store,
            &mut record,
            CloseOutcome::Refused,
            "pr_close_preflight_drift",
            Some(immediate),
            false,
        );
    }
    let fence = match provider.revalidate_fence() {
        Ok(fence) => fence,
        Err(error) => {
            return finish(
                store,
                &mut record,
                CloseOutcome::Refused,
                error.code(),
                Some(immediate),
                false,
            );
        }
    };
    // A crash after this save is ambiguous even if no HTTP call was reached.
    // The next invocation reads provider state and NEVER repeats this close.
    record.intent = Some(immediate.clone());
    record.intent_fence_fingerprint = Some(fence);
    store.save(&record)?;
    let result = provider.close_once(&immediate);
    record.write_returned_success = Some(result.is_ok());
    record.write_error_code = result.err().map(|error| error.code());
    store.save(&record)?;
    reconcile(provider, store, &mut record, true)
}

enum Preflight {
    Ready(Box<CloseFacts>),
    Complete(Box<PrCloseOutput>),
}

fn preflight(
    provider: &impl CloseProvider,
    store: &impl ReceiptStore,
    record: &mut PrCloseRecord,
) -> Result<Preflight, AppError> {
    let facts = match provider.observe(&record.request) {
        Ok(facts) => facts,
        Err(error) => {
            let outcome = if error.category() == ErrorCategory::Validation {
                CloseOutcome::Refused
            } else {
                CloseOutcome::Unavailable
            };
            return finish(store, record, outcome, error.code(), None, false)
                .map(|output| Preflight::Complete(Box::new(output)));
        }
    };
    if let Some(code) = refusal(&record.request, &facts) {
        return finish(
            store,
            record,
            CloseOutcome::Refused,
            code,
            Some(facts),
            false,
        )
        .map(|output| Preflight::Complete(Box::new(output)));
    }
    if let Some(outcome) = external_outcome(&facts) {
        return finish(
            store,
            record,
            outcome,
            "pr_close_external_completion",
            Some(facts),
            false,
        )
        .map(|output| Preflight::Complete(Box::new(output)));
    }
    Ok(Preflight::Ready(Box::new(facts)))
}

fn external_outcome(facts: &CloseFacts) -> Option<CloseOutcome> {
    if facts.pull.state == PullRequestState::Merged || facts.pull.merged_at.is_some() {
        Some(CloseOutcome::ExternallyMerged)
    } else if facts.pull.state == PullRequestState::Closed {
        Some(CloseOutcome::ExternallyClosed)
    } else {
        None
    }
}

/// No independent caller claim is accepted as representation or non-membership.
fn refusal(input: &PrCloseInput, facts: &CloseFacts) -> Option<&'static str> {
    let pull = &facts.pull;
    if facts.repository.slug() != input.repository
        || pull.number.0 != input.pr
        || pull.head.repository != facts.repository
        || pull.base.repository != facts.repository
        || pull.head.name != input.head_ref
        || pull.head.oid.0 != input.head
        || pull.base.name != input.base_ref
        || pull.base.oid.0 != input.base
        || facts.default_branch.repository != facts.repository
        || facts.default_branch.name != input.main_ref
        || facts.default_branch.oid.0 != input.main
        || facts.source.oid.0 != input.head
        || facts.represented_at.oid.0 != input.represented_at
    {
        return Some("pr_close_identity_drift");
    }
    if !super::valid_oid(&facts.source.tree_oid.0)
        || !super::valid_oid(&facts.represented_at.tree_oid.0)
    {
        return Some("pr_close_commit_evidence_invalid");
    }
    if pull.draft || pull.cross_repository || pull.auto_merge {
        return Some("pr_close_subject_ineligible");
    }
    if ["caravan", "caravan-parked", "caravan-force"]
        .iter()
        .any(|label| pull.labels.contains(*label))
    {
        return Some("pr_close_member_refused");
    }
    if !facts.native_stacks.is_empty() {
        return Some("pr_close_native_member_refused");
    }
    if !matches!(facts.permission.as_str(), "WRITE" | "MAINTAIN" | "ADMIN") {
        return Some("pr_close_permission_required");
    }
    if !facts.represented_on_main || !facts.source_in_representation {
        return Some("pr_close_representation_unproven");
    }
    if input.representation == super::MainRepresentation::SameTree
        && facts.source.tree_oid != facts.represented_at.tree_oid
    {
        return Some("pr_close_representation_unproven");
    }
    None
}

fn reconcile(
    provider: &impl CloseProvider,
    store: &impl ReceiptStore,
    record: &mut PrCloseRecord,
    attempted: bool,
) -> Result<PrCloseOutput, AppError> {
    let facts = match provider.observe(&record.request) {
        Ok(facts) => facts,
        Err(error) => {
            return finish(
                store,
                record,
                CloseOutcome::Indeterminate,
                format!("{}_readback", error.code()),
                None,
                attempted,
            );
        }
    };
    if let Some(code) = refusal(&record.request, &facts) {
        return finish(
            store,
            record,
            CloseOutcome::Indeterminate,
            code,
            Some(facts),
            attempted,
        );
    }
    if facts.pull.state == PullRequestState::Merged || facts.pull.merged_at.is_some() {
        return finish(
            store,
            record,
            CloseOutcome::ExternallyMerged,
            "pr_close_external_merge",
            Some(facts),
            attempted,
        );
    }
    if facts.pull.state != PullRequestState::Closed {
        return finish(
            store,
            record,
            CloseOutcome::Indeterminate,
            "pr_close_not_observed_closed",
            Some(facts),
            attempted,
        );
    }
    // Preserve non-closure control drift even after an apparently successful PATCH.
    let mut normalized = facts.clone();
    normalized.pull.state = PullRequestState::Open;
    if record.intent.as_ref() != Some(&normalized) {
        return finish(
            store,
            record,
            CloseOutcome::Indeterminate,
            "pr_close_post_write_drift",
            Some(facts),
            attempted,
        );
    }
    let outcome = if attempted && record.write_returned_success == Some(true) {
        CloseOutcome::Closed
    } else {
        CloseOutcome::ReconciledClosed
    };
    finish(
        store,
        record,
        outcome,
        "pr_close_observed_closed",
        Some(facts),
        attempted,
    )
}

fn finish(
    store: &impl ReceiptStore,
    record: &mut PrCloseRecord,
    outcome: CloseOutcome,
    code: impl AsRef<str>,
    observed: Option<CloseFacts>,
    attempted: bool,
) -> Result<PrCloseOutput, AppError> {
    let code = code.as_ref();
    let intent = record.intent.is_some();
    let next = match outcome {
        CloseOutcome::Refused => {
            "no close sent; correct the named precondition without changing this key's bound identity"
        }
        CloseOutcome::Unavailable => {
            "no close sent; retry the same bound request after restoring the named observation phase"
        }
        CloseOutcome::Indeterminate => {
            "preserve this receipt; retry only this same key in its retained Git common directory to reconcile, never reopen or resend close"
        }
        _ => {
            "no further close is required; this does not authorize queue cleanup, source or release changes"
        }
    };
    let output = PrCloseOutput {
        schema_version: 1,
        operation_key: record.request.operation_key.clone(),
        request_fingerprint: fingerprint(&record.request),
        policy_fingerprint: record.policy_fingerprint.clone(),
        writer_mode: record.writer_mode,
        journal_scope: "git_common_dir; remote lease is not journal replication".to_owned(),
        atomic_provider_transaction: false,
        close_intent_recorded: intent,
        close_attempted_this_call: attempted,
        provider_mutated: if !attempted {
            Some(false)
        } else if record.write_returned_success == Some(true) && outcome == CloseOutcome::Closed {
            Some(true)
        } else {
            None
        },
        operation_write_returned_success: record.write_returned_success,
        operation_write_error_code: record.write_error_code.clone(),
        intent_fence_fingerprint: record.intent_fence_fingerprint.clone(),
        outcome,
        code: code.to_owned(),
        observed,
        next: next.to_owned(),
    };
    if record.first_result.is_none() {
        record.first_result = Some(output.clone());
    }
    record.latest_result = Some(output.clone());
    store.save(record)?;
    match outcome {
        CloseOutcome::Refused | CloseOutcome::Unavailable | CloseOutcome::Indeterminate => {
            Err(AppError::structured(
                if outcome == CloseOutcome::Refused {
                    ErrorCategory::Validation
                } else {
                    ErrorCategory::ExecutionFailure
                },
                match outcome {
                    CloseOutcome::Indeterminate => "pr_close_indeterminate",
                    CloseOutcome::Unavailable => "pr_close_unavailable",
                    _ => "pr_close_refused",
                },
                code,
                Some(serde_json::to_value(output).expect("close receipt serializes")),
            ))
        }
        _ => Ok(output),
    }
}
