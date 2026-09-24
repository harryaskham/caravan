//! Applicable workflow execution selection and durable post-mutation CI starts.
//! Eligibility remains in `required_runs`; this module never waives a requirement.

use crate::{
    AppError, ErrorCategory,
    github::GitHubMutationReceipt,
    model::{CheckSnapshot, OperationId, PrNumber, PullRequestSnapshot, RepositoryId},
    required_runs::{
        CheckSuiteLineage, HeadRunLineage, RequiredContextsRead, WorkflowPullRequestBinding,
        WorkflowRunLineage,
    },
};
use mcp_cli::StructuredError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CiExecution {
    pub app_id: u64,
    pub app_slug: String,
    pub workflow_id: u64,
    pub workflow_name: String,
    pub contexts: Vec<String>,
    /// Derived only from applicable context reports, never optional failures in
    /// the workflow aggregate. Reusing a failed run is not a green CI verdict.
    pub restart_required: bool,
    pub run_id: u64,
    pub run_attempt: u64,
    pub check_suite_id: u64,
    pub pull_request: WorkflowPullRequestBinding,
    pub event: String,
    pub status: String,
    pub conclusion: String,
}

impl CiExecution {
    pub(crate) fn from_run(
        pull: &PullRequestSnapshot,
        run: &WorkflowRunLineage,
        suite: &CheckSuiteLineage,
        app_id: u64,
        contexts: &[String],
    ) -> Result<Self, AppError> {
        let execution = run
            .execution
            .as_ref()
            .ok_or_else(|| unavailable("workflow ID or run attempt is absent"))?;
        let expected = WorkflowPullRequestBinding {
            number: pull.number,
            head_sha: pull.head.oid.0.clone(),
            head_ref: pull.head.name.clone(),
            base_sha: pull.base.oid.0.clone(),
            base_ref: pull.base.name.clone(),
        };
        if app_id == 0
            || execution.workflow_id == 0
            || execution.run_attempt == 0
            || run.run_id == 0
            || run.check_suite_id == 0
            || run.workflow_name.is_empty()
            || run.event.is_empty()
            || run.head_sha != expected.head_sha
            || suite.head_sha != expected.head_sha
            || suite.id != run.check_suite_id
            || suite.app_slug != "github-actions"
            || execution
                .pull_requests
                .iter()
                .filter(|binding| binding.number == pull.number)
                .collect::<Vec<_>>()
                != vec![&expected]
        {
            return Err(unavailable(
                "App/workflow/run/suite/head/base/PR binding is absent, ambiguous or stale",
            ));
        }
        let current = crate::model::latest_checks_per_identity(&pull.checks).0;
        let restart_required = current.iter().any(|check| {
            contexts.contains(&check.name)
                && check.app_id == Some(app_id)
                && check.check_suite_id == Some(suite.id)
                && check.head_oid.as_ref() == Some(&pull.head.oid)
                && run_id(check) == Some(run.run_id)
                && matches!(
                    check.state,
                    crate::model::CheckState::Failure
                        | crate::model::CheckState::Cancelled
                        | crate::model::CheckState::TimedOut
                        | crate::model::CheckState::ActionRequired
                )
        });
        let selected = Self {
            contexts: contexts.to_vec(),
            restart_required,
            app_id,
            app_slug: suite.app_slug.clone(),
            workflow_id: execution.workflow_id,
            workflow_name: run.workflow_name.clone(),
            run_id: run.run_id,
            run_attempt: execution.run_attempt,
            check_suite_id: suite.id,
            pull_request: expected,
            event: run.event.clone(),
            status: run.status.clone(),
            conclusion: run.conclusion.clone(),
        };
        if !selected.reusable() && !selected.requestable() {
            return Err(unavailable("workflow state is unknown"));
        }
        Ok(selected)
    }

    pub(crate) fn reusable(&self) -> bool {
        (matches!(
            self.status.as_str(),
            "queued" | "requested" | "waiting" | "pending" | "in_progress"
        ) && self.conclusion.is_empty())
            || (self.status == "completed"
                && !self.restart_required
                && matches!(
                    self.conclusion.as_str(),
                    "success"
                        | "failure"
                        | "cancelled"
                        | "timed_out"
                        | "action_required"
                        | "neutral"
                        | "skipped"
                ))
    }

    pub(crate) fn requestable(&self) -> bool {
        self.restart_required
            && self.status == "completed"
            && matches!(
                self.conclusion.as_str(),
                "failure" | "cancelled" | "timed_out" | "action_required" | "neutral" | "skipped"
            )
    }

    fn same_execution(&self, other: &Self) -> bool {
        self.app_id == other.app_id
            && self.app_slug == other.app_slug
            && self.workflow_id == other.workflow_id
            && self.run_id == other.run_id
            && self.run_attempt == other.run_attempt
            && self.check_suite_id == other.check_suite_id
            && self.pull_request == other.pull_request
            && self.event == other.event
    }

    fn succeeds(&self, old: &Self) -> bool {
        self.app_id == old.app_id
            && self.app_slug == old.app_slug
            && self.workflow_id == old.workflow_id
            && self.pull_request == old.pull_request
            && self.event == old.event
            && (self.run_id > old.run_id
                || (self.run_id == old.run_id
                    && self.run_attempt > old.run_attempt
                    && self.check_suite_id == old.check_suite_id))
    }
}

fn unavailable(reason: &str) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "post_mutation_ci_dispatch_unavailable",
        reason,
        Some(
            serde_json::json!({"mutated": false, "safe_next_action": "refresh exact provider execution evidence; never choose another App or replay membership to start CI"}),
        ),
    )
}

/// Select every independently proved applicable Actions workflow, not the first
/// suite. A configured gate names its workflow; otherwise actual requirements
/// name the workflows. Non-Actions obligations remain untouched and unwaived.
pub(crate) fn select(
    pull: &PullRequestSnapshot,
    policy: &RequiredContextsRead,
    gate: Option<&str>,
    lineage: &HeadRunLineage,
) -> Result<Vec<CiExecution>, AppError> {
    let policy = policy.clone().normalized();
    if !policy.complete {
        return Err(unavailable("effective landing-target policy is incomplete"));
    }
    if gate.is_none() && policy.checks.is_empty() {
        return Ok(Vec::new());
    }
    validate_lineage(pull, lineage)?;
    let checks = crate::model::latest_checks_per_identity(&pull.checks).0;
    let names = gate.map_or_else(
        || {
            policy
                .checks
                .iter()
                .map(|check| check.context.as_str())
                .collect::<BTreeSet<_>>()
        },
        |name| BTreeSet::from([name]),
    );
    let mut anchors: BTreeMap<u64, WorkflowAnchor<'_>> = BTreeMap::new();
    for name in names {
        let applicable = checks
            .iter()
            .copied()
            .filter(|check| {
                check.name == name
                    && (policy
                        .checks
                        .iter()
                        .all(|required| required.context != name)
                        || policy.checks.iter().any(|required| {
                            required.context == name && required.matches(check, &pull.head.oid)
                        }))
            })
            .collect::<Vec<_>>();
        if applicable.is_empty() {
            return Err(unavailable(
                "applicable context has no current reporting identity",
            ));
        }
        for check in applicable {
            let Some(anchor) = check_anchor(pull, check, lineage)? else {
                continue;
            };
            let app_id = anchor.app_id;
            let entry = anchors.entry(anchor.workflow_id).or_insert(anchor);
            if entry.app_id != app_id {
                return Err(unavailable("one workflow has conflicting App identities"));
            }
            entry.contexts.insert(check.name.clone());
        }
    }
    anchors
        .values()
        .map(|anchor| newest_execution(pull, anchor, lineage))
        .collect()
}

fn validate_lineage(pull: &PullRequestSnapshot, lineage: &HeadRunLineage) -> Result<(), AppError> {
    if !lineage.complete
        || lineage.head_sha != pull.head.oid.0
        || lineage
            .workflow_runs
            .iter()
            .map(|run| run.run_id)
            .collect::<BTreeSet<_>>()
            .len()
            != lineage.workflow_runs.len()
        || lineage
            .check_suites
            .iter()
            .map(|suite| suite.id)
            .collect::<BTreeSet<_>>()
            .len()
            != lineage.check_suites.len()
    {
        return Err(unavailable(
            "workflow lineage is incomplete, duplicated or belongs to another head",
        ));
    }
    Ok(())
}

struct WorkflowAnchor<'a> {
    app_id: u64,
    workflow_id: u64,
    run: &'a WorkflowRunLineage,
    contexts: BTreeSet<String>,
}

// Historical reports identify the workflow, not the base to run now. Only the
// selected newest execution below can prove the current PR/base generation.
fn check_anchor<'a>(
    pull: &PullRequestSnapshot,
    check: &CheckSnapshot,
    lineage: &'a HeadRunLineage,
) -> Result<Option<WorkflowAnchor<'a>>, AppError> {
    if check.provider_kind.as_deref() == Some("StatusContext") {
        return Ok(None);
    }
    if check.provider_kind.as_deref() != Some("CheckRun") {
        return Err(unavailable(
            "only a real provider check run can identify an applicable workflow",
        ));
    }
    let suite = lineage
        .check_suites
        .iter()
        .find(|suite| Some(suite.id) == check.check_suite_id && suite.head_sha == pull.head.oid.0)
        .ok_or_else(|| unavailable("reporting check has no exact owning suite"))?;
    if suite.app_slug != "github-actions" {
        return Ok(None);
    }
    let app_id = check
        .app_id
        .filter(|id| *id > 0)
        .ok_or_else(|| unavailable("reporting check has no App identity"))?;
    if check.head_oid.as_ref() != Some(&pull.head.oid) {
        return Err(unavailable("reporting check lacks current-head proof"));
    }
    let id =
        run_id(check).ok_or_else(|| unavailable("reporting check has no workflow run identity"))?;
    let run = lineage
        .workflow_runs
        .iter()
        .find(|run| {
            run.run_id == id && run.check_suite_id == suite.id && run.head_sha == pull.head.oid.0
        })
        .ok_or_else(|| unavailable("reporting check and workflow suite disagree"))?;
    let execution = run
        .execution
        .as_ref()
        .filter(|execution| execution.workflow_id > 0 && execution.run_attempt > 0)
        .ok_or_else(|| unavailable("reporting check has no actual workflow/attempt identity"))?;
    if check
        .workflow_name
        .as_ref()
        .is_none_or(|name| name != &run.workflow_name)
        || run.workflow_name.is_empty()
    {
        return Err(unavailable(
            "reporting check and workflow identity disagree",
        ));
    }
    Ok(Some(WorkflowAnchor {
        app_id,
        workflow_id: execution.workflow_id,
        run,
        contexts: BTreeSet::new(),
    }))
}

fn newest_execution(
    pull: &PullRequestSnapshot,
    anchor: &WorkflowAnchor<'_>,
    lineage: &HeadRunLineage,
) -> Result<CiExecution, AppError> {
    if lineage.workflow_runs.iter().any(|run| {
        run.head_sha == pull.head.oid.0
            && run.run_id >= anchor.run.run_id
            && run.execution.is_none()
    }) {
        return Err(unavailable(
            "a newer workflow has unknown execution identity",
        ));
    }
    let newest = lineage
        .workflow_runs
        .iter()
        .filter(|run| {
            run.head_sha == pull.head.oid.0
                && run
                    .execution
                    .as_ref()
                    .is_some_and(|identity| identity.workflow_id == anchor.workflow_id)
        })
        .max_by_key(|run| {
            (
                run.run_id,
                run.execution
                    .as_ref()
                    .map_or(0, |identity| identity.run_attempt),
            )
        })
        .ok_or_else(|| unavailable("applicable workflow disappeared"))?;
    let suite = lineage
        .check_suites
        .iter()
        .find(|suite| suite.id == newest.check_suite_id)
        .ok_or_else(|| unavailable("current execution has no suite"))?;
    CiExecution::from_run(
        pull,
        newest,
        suite,
        anchor.app_id,
        &anchor.contexts.iter().cloned().collect::<Vec<_>>(),
    )
}

fn run_id(check: &CheckSnapshot) -> Option<u64> {
    check
        .details_url
        .as_deref()?
        .split_once("/actions/runs/")?
        .1
        .split('/')
        .next()?
        .parse()
        .ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum DispatchDisposition {
    #[default]
    Requested,
    Reused,
    SuccessorObserved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DispatchBinding {
    pub repository: RepositoryId,
    pub pull_request: WorkflowPullRequestBinding,
    pub caravan_members: Vec<PrNumber>,
    pub workflow_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DispatchCheckpoint {
    schema_version: u32,
    binding: DispatchBinding,
    selected: CiExecution,
    operation_id: OperationId,
    #[serde(default)]
    disposition: DispatchDisposition,
    // Missing acknowledgement on a requested intent is uncertain, not rejected.
    provider_receipt: Option<GitHubMutationReceipt>,
    observed_successor: Option<CiExecution>,
    // Only a typed, pre-write refusal may populate this; provider errors after
    // POST never do. Rejections are archived before a fresh request can proceed.
    #[serde(default)]
    rejected_before_write: Option<String>,
}

pub(crate) struct DispatchResult {
    pub disposition: DispatchDisposition,
    pub operation_id: OperationId,
    pub provider_receipt: Option<GitHubMutationReceipt>,
}

/// Caller holds the normal repository writer guard. A crash, failed readback,
/// or failed receipt save leaves an intent that forbids repeating the write.
pub(crate) fn dispatch(
    repository_path: &Path,
    binding: &DispatchBinding,
    selected: &CiExecution,
    operation_id: &OperationId,
    preflight: impl FnOnce() -> Result<(), AppError>,
    request: impl FnOnce() -> Result<GitHubMutationReceipt, AppError>,
) -> Result<DispatchResult, AppError> {
    if binding.pull_request != selected.pull_request || binding.workflow_id != selected.workflow_id
    {
        return Err(unavailable(
            "CI-start binding does not match its selected execution",
        ));
    }
    let encoded = serde_json::to_vec(binding).map_err(|error| unavailable(&error.to_string()))?;
    let key = format!("ci-start-{:x}", Sha256::digest(encoded));
    if let Some(saved) = crate::stack_checkpoint::load::<DispatchCheckpoint>(repository_path, &key)?
    {
        if saved.schema_version != 1
            || &saved.binding != binding
            || saved.disposition == DispatchDisposition::SuccessorObserved
        {
            return Err(unavailable("CI dispatch checkpoint identity is invalid"));
        }
        if saved.rejected_before_write.is_none() {
            return reconcile(repository_path, &key, saved, selected);
        }
        let bytes = serde_json::to_vec(&saved).map_err(|error| unavailable(&error.to_string()))?;
        let archive = format!("ci-refused-{:x}", Sha256::digest(bytes));
        crate::stack_checkpoint::write(repository_path, &archive, &saved)?;
        crate::stack_checkpoint::remove(repository_path, &key)?;
    }
    let disposition = if selected.reusable() {
        DispatchDisposition::Reused
    } else {
        if !selected.requestable() {
            return Err(unavailable("execution is not restartable"));
        }
        preflight()?;
        DispatchDisposition::Requested
    };
    let mut checkpoint = DispatchCheckpoint {
        schema_version: 1,
        binding: binding.clone(),
        selected: selected.clone(),
        operation_id: operation_id.clone(),
        disposition,
        provider_receipt: None,
        observed_successor: None,
        rejected_before_write: None,
    };
    crate::stack_checkpoint::write(repository_path, &key, &checkpoint)?;
    if disposition == DispatchDisposition::Reused {
        return Ok(DispatchResult {
            disposition,
            operation_id: operation_id.clone(),
            provider_receipt: None,
        });
    }
    let receipt = match request() {
        Ok(receipt) => receipt,
        Err(error) => {
            if error
                .details()
                .as_ref()
                .and_then(|details| details.get("provider_write_attempted"))
                .and_then(serde_json::Value::as_bool)
                == Some(false)
            {
                checkpoint.rejected_before_write = Some(error.code());
                crate::stack_checkpoint::write(repository_path, &key, &checkpoint)?;
            }
            return Err(error);
        }
    };
    checkpoint.provider_receipt = Some(receipt.clone());
    crate::stack_checkpoint::write(repository_path, &key, &checkpoint)?;
    Ok(DispatchResult {
        disposition,
        operation_id: operation_id.clone(),
        provider_receipt: Some(receipt),
    })
}

fn reconcile(
    repository_path: &Path,
    key: &str,
    mut saved: DispatchCheckpoint,
    selected: &CiExecution,
) -> Result<DispatchResult, AppError> {
    if saved
        .observed_successor
        .as_ref()
        .is_some_and(|observed| !selected.same_execution(observed) && !selected.succeeds(observed))
    {
        return Err(unavailable(
            "provider execution regressed behind a durably observed successor",
        ));
    }
    let disposition = if selected.succeeds(&saved.selected) {
        saved.observed_successor = Some(selected.clone());
        crate::stack_checkpoint::write(repository_path, key, &saved)?;
        DispatchDisposition::SuccessorObserved
    } else if selected.same_execution(&saved.selected)
        && (saved.provider_receipt.is_some() || saved.disposition == DispatchDisposition::Reused)
    {
        saved.disposition
    } else {
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "ci_dispatch_indeterminate",
            "an exact CI-start intent already exists without proved successor; no provider write was repeated",
            Some(
                serde_json::json!({"checkpoint": key, "intent": saved, "observed": selected, "membership_replay_allowed": false}),
            ),
        ));
    };
    Ok(DispatchResult {
        disposition,
        operation_id: saved.operation_id,
        provider_receipt: None,
    })
}

#[cfg(test)]
pub(crate) mod tests;
