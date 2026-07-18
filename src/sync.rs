//! Deterministic, idempotent caravan synchronization.
//!
//! GitHub remains the only durable cursor. Every tick starts from fresh graph
//! facts, proves all compatibility decisions before mutation, applies exact
//! optimistic primitives, and records enough completed work for a rerun to
//! resume after interruption.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mcp_cli::{ErrorCategory, StructuredError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ci::{WorkflowFailureDiagnostics, WorkflowRunFailureDiagnostic};
use crate::command::CommandRunError;
use crate::github::{
    ControlLabelAudit, DiscoveryError, GitHubMutationAdapter, GitHubMutationReceipt, MutationError,
    WorkflowRunSnapshot, control_label_marker,
};
use crate::hooks::{self, HookDelivery};
use crate::model::{
    Caravan, CaravanEvent, CheckSnapshot, CheckState, CompatibilityOutcome, DecisionKind,
    DecisionPoint, EventId, EventKind, GraphProblem, GraphProblemKind, MergeCandidateIdentity,
    MergeMethod, MutationKind, MutationStep, MutationStepState, OperationId, OperationReceipt,
    PrNumber, PullRequestPrecondition, PullRequestSnapshot, PullRequestState, RepositoryId,
};
use crate::operation_lock::{OperationLock, OperationLockRecovery};
use crate::read::{self, StatusOutput};
use crate::{AppContext, AppError, SyncInput};

const MAX_SYNC_OPERATION_SECS: u64 = 150;
const SYNC_BUDGET_MULTIPLIER: u64 = 5;

/// One observed rolling-head transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HeadAdvancement {
    pub merged_predecessor: PrNumber,
    pub new_head: PrNumber,
    pub previous_caravan_id: PrNumber,
    pub new_caravan_id: PrNumber,
}

/// Normalized CI policy outcome observed for one PR during a sync tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CiDisposition {
    Passing,
    Waiting,
    Failed,
    Forced,
}

/// Run-to-current-PR generation relationship derived from immutable provider facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunGeneration {
    Current,
    StaleHead,
    StaleBase,
    StaleHeadAndBase,
    MissingAssociation,
}

/// Deterministic sync-owned failure class; raw logs are never required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowFailureClass {
    StaleGeneration,
    RetryableInfrastructure,
    SourceOrTestFailure,
    Cancelled,
    Unknown,
}

/// Safe sync action selected from generation and structured failure evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowFailureAction {
    FreshCandidateTrigger,
    RerunFailedJobs,
    RepairSource,
    WaitOrInspect,
}

/// Classified evidence consumed directly by the CI decision and hook event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClassifiedWorkflowRunFailure {
    pub diagnostic: WorkflowRunFailureDiagnostic,
    pub generation: WorkflowRunGeneration,
    pub classification: WorkflowFailureClass,
    pub action: WorkflowFailureAction,
    #[serde(default)]
    pub reasons: Vec<String>,
}

/// Exact check and workflow-run evidence for one selected PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CiObservation {
    pub pr: PrNumber,
    pub disposition: CiDisposition,
    #[serde(default)]
    pub checks: Vec<CheckSnapshot>,
    #[serde(default)]
    pub failed_runs: Vec<WorkflowRunSnapshot>,
    /// Bounded run/job/failed-step evidence with generation-aware policy.
    #[serde(default)]
    pub failure_diagnostics: Vec<ClassifiedWorkflowRunFailure>,
    /// Exact current-generation infrastructure runs safe to rerun.
    #[serde(default)]
    pub rerunnable_run_ids: Vec<u64>,
}

/// Bounded whole-sync phase timings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncTiming {
    pub deadline_ms: u64,
    pub total_ms: u64,
    pub initial_status_ms: u64,
    pub provider_convergence_ms: u64,
    pub final_status_ms: u64,
}

/// Stable result of one converged synchronization tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncOutput {
    pub receipt: OperationReceipt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<SyncTiming>,
    /// Exact dead-owner cleanup performed before this sync acquired its lock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_recovery: Option<OperationLockRecovery>,
    /// Exact provider before/after facts for completed remote mutations.
    #[serde(default)]
    pub provider_receipts: Vec<GitHubMutationReceipt>,
    /// Merged branch-local predecessor that selected the active caravan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub historical_predecessor: Option<PrNumber>,
    /// Caravan IDs actually selected from the initial snapshot.
    #[serde(default)]
    pub synchronized_caravans: Vec<PrNumber>,
    /// Intentional holds skipped without any provider mutation. Expired holds
    /// remain here until an explicit resume; they never resume by time alone.
    #[serde(default)]
    pub paused_caravans: Vec<crate::pause::PauseStatus>,
    #[serde(default)]
    pub head_advancements: Vec<HeadAdvancement>,
    /// CI policy facts in deterministic head-to-tail order.
    #[serde(default)]
    pub ci: Vec<CiObservation>,
    /// Canonical auditable events consumed by configured hooks.
    #[serde(default)]
    pub events: Vec<CaravanEvent>,
    /// Bounded status for configured hooks which consumed `events`.
    #[serde(default)]
    pub hook_deliveries: Vec<HookDelivery>,
    /// Fresh post-mutation discovery rather than a locally predicted graph.
    pub status: StatusOutput,
}

/// Provider facts and primitives required by sync policy.
pub trait SyncProvider {
    fn verify_branch_head(
        &self,
        repository: &RepositoryId,
        branch: &str,
        expected: &crate::model::CommitOid,
    ) -> Result<(), MutationError>;

    fn branch_is_protected(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<bool, MutationError>;

    fn repository_allows_auto_merge(
        &self,
        repository: &RepositoryId,
    ) -> Result<bool, MutationError>;

    fn set_base(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        base: &str,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn enable_squash_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn disable_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn failed_runs_for_pull_request(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<Vec<WorkflowRunSnapshot>, MutationError>;

    fn failed_run_diagnostics(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        run_ids: &[u64],
    ) -> Result<WorkflowFailureDiagnostics, MutationError>;

    fn rerun_failed_run(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        run_id: u64,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn viewer_permission(&self, repository: &RepositoryId) -> Result<String, MutationError>;

    fn ensure_control_label_comment(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        audit: &ControlLabelAudit,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn admin_squash_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError>;
}

impl<R: crate::command::CommandRunner> SyncProvider for GitHubMutationAdapter<R> {
    fn verify_branch_head(
        &self,
        repository: &RepositoryId,
        branch: &str,
        expected: &crate::model::CommitOid,
    ) -> Result<(), MutationError> {
        self.verify_branch_head(repository, branch, expected)
    }

    fn branch_is_protected(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<bool, MutationError> {
        self.branch_is_protected(repository, branch)
    }

    fn repository_allows_auto_merge(
        &self,
        repository: &RepositoryId,
    ) -> Result<bool, MutationError> {
        self.repository_allows_auto_merge(repository)
    }

    fn set_base(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        base: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.set_base(repository, expected, base)
    }

    fn enable_squash_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.enable_squash_auto_merge(repository, expected)
    }

    fn disable_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.disable_auto_merge(repository, expected)
    }

    fn failed_runs_for_pull_request(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<Vec<WorkflowRunSnapshot>, MutationError> {
        self.failed_runs_for_pull_request(repository, expected)
    }

    fn failed_run_diagnostics(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        run_ids: &[u64],
    ) -> Result<WorkflowFailureDiagnostics, MutationError> {
        self.failed_run_diagnostics(repository, expected, run_ids)
    }

    fn rerun_failed_run(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        run_id: u64,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.rerun_failed_run(repository, expected, run_id)
    }

    fn viewer_permission(&self, repository: &RepositoryId) -> Result<String, MutationError> {
        self.viewer_permission(repository)
    }

    fn ensure_control_label_comment(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        audit: &ControlLabelAudit,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.ensure_control_label_comment(repository, expected, audit)
    }

    fn admin_squash_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.admin_squash_merge(repository, expected)
    }
}

/// Synchronize the current caravan or every caravan and dispatch its canonical events.
pub fn sync(context: &AppContext, input: &SyncInput) -> Result<SyncOutput, AppError> {
    let started = Instant::now();
    let budget = sync_operation_budget(context);
    let operation_deadline = started + budget;
    match sync_without_hooks(context, input, started, operation_deadline) {
        Ok(mut output) => {
            output.hook_deliveries = hooks::dispatch_events(context, &output.events)?;
            Ok(output)
        }
        Err(error) => {
            let error = checkout_for_decision(context, error, operation_deadline);
            let mut events = hooks::events_from_error(&error);
            if events.is_empty() {
                if let Some(event) = sync_failed_event(&error) {
                    events.push(event);
                }
            }
            let deliveries = hooks::dispatch_events(context, &events)?;
            Err(hooks::attach_deliveries(error, &deliveries))
        }
    }
}

fn sync_operation_budget(context: &AppContext) -> Duration {
    Duration::from_secs(
        context
            .config
            .command_timeout_secs
            .saturating_mul(SYNC_BUDGET_MULTIPLIER)
            .min(MAX_SYNC_OPERATION_SECS),
    )
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn checkout_for_decision(
    context: &AppContext,
    error: AppError,
    operation_deadline: Instant,
) -> AppError {
    let Some(details) = error.details() else {
        return error;
    };
    let Some(decision_value) = details.get("decision") else {
        return error;
    };
    let Ok(decision) = serde_json::from_value::<DecisionPoint>(decision_value.clone()) else {
        return error;
    };
    let target = decision_checkout_target(&decision);
    let Some(target) = target else {
        return error;
    };
    let Some(pull_request) = decision_checkout_pull_request(&decision, target) else {
        return attach_checkout_evidence(
            &error,
            details,
            json!({
                "state": "skipped",
                "pr": target,
                "error": {
                    "category": "validation",
                    "code": "decision_checkout_snapshot_missing",
                    "message": "the decision did not preserve the affected PR snapshot",
                },
                "next": "rediscover status and check out the affected PR before repairing the decision",
            }),
        );
    };
    let checkout = match crate::navigation::checkout_decision_snapshot(
        context,
        &pull_request,
        operation_deadline,
    ) {
        Ok(lock_recovery) => json!({
            "state": "completed",
            "pr": target,
            "branch": pull_request.head.name,
            "oid": pull_request.head.oid,
            "lock_recovery": lock_recovery,
        }),
        Err(checkout_error) => json!({
            "state": "skipped",
            "pr": target,
            "error": {
                "category": checkout_error.category(),
                "code": checkout_error.code(),
                "message": checkout_error.message(),
                "details": checkout_error.details(),
            },
            "next": "make the local worktree safe, then check out the affected PR before repairing the decision",
        }),
    };
    attach_checkout_evidence(&error, details, checkout)
}

fn attach_checkout_evidence(error: &AppError, mut details: Value, checkout: Value) -> AppError {
    if let Some(object) = details.as_object_mut() {
        object.insert("checkout".to_owned(), checkout);
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

fn decision_checkout_pull_request(
    decision: &DecisionPoint,
    target: PrNumber,
) -> Option<PullRequestSnapshot> {
    decision
        .evidence
        .get("pull_request")
        .and_then(|value| serde_json::from_value::<PullRequestSnapshot>(value.clone()).ok())
        .filter(|pull_request| pull_request.number == target)
        .or_else(|| {
            decision
                .evidence
                .get("pull_requests")
                .and_then(|value| {
                    serde_json::from_value::<Vec<PullRequestSnapshot>>(value.clone()).ok()
                })
                .and_then(|pull_requests| {
                    pull_requests
                        .into_iter()
                        .find(|pull_request| pull_request.number == target)
                })
        })
}

fn decision_checkout_target(decision: &DecisionPoint) -> Option<PrNumber> {
    match decision.kind {
        DecisionKind::HeadConflict | DecisionKind::CiFailure => {
            decision.affected_prs.first().copied()
        }
        DecisionKind::LinkConflict => decision.affected_prs.last().copied(),
        _ => None,
    }
}

fn sync_failed_event(error: &AppError) -> Option<CaravanEvent> {
    let details = error.details()?;
    let decision =
        serde_json::from_value::<DecisionPoint>(details.get("decision")?.clone()).ok()?;
    let fleet = decision
        .evidence
        .get("fleet")
        .and_then(|value| serde_json::from_value(value.clone()).ok());
    Some(hooks::event(
        EventKind::SyncFailed,
        decision.operation_id,
        decision.repository,
        decision.caravan_id,
        decision.affected_prs,
        fleet,
        Some(decision.message),
        BTreeMap::from([("error_code".to_owned(), json!(error.code()))]),
    ))
}

fn sync_without_hooks(
    context: &AppContext,
    input: &SyncInput,
    started: Instant,
    operation_deadline: Instant,
) -> Result<SyncOutput, AppError> {
    let lock = OperationLock::acquire(&context.repository_path, "sync")?;
    let lock_recovery = lock.recovered_dead_owner().cloned();
    sync_with_lock(
        context,
        input,
        started,
        operation_deadline,
        lock,
        lock_recovery.clone(),
    )
    .map_err(|error| attach_lock_recovery(error, lock_recovery.as_ref()))
}

#[allow(clippy::too_many_lines)]
fn sync_with_lock(
    context: &AppContext,
    input: &SyncInput,
    started: Instant,
    operation_deadline: Instant,
    mut lock: OperationLock,
    lock_recovery: Option<OperationLockRecovery>,
) -> Result<SyncOutput, AppError> {
    lock.checkpoint(
        "initial_discovery_in_flight",
        json!({
            "operation": "sync",
            "all": input.all,
            "deadline_ms": duration_millis(operation_deadline.saturating_duration_since(started)),
        }),
        false,
    )?;
    let timeout = Duration::from_secs(context.config.command_timeout_secs);
    let initial_status_started = Instant::now();
    let status = read::status_with_deadline(context, operation_deadline)?;
    let initial_status_elapsed = initial_status_started.elapsed();
    crate::initialization::require_ready(&status.initialization)?;
    let runner = crate::command::ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(timeout)
        .with_operation_deadline(operation_deadline);
    // A decision can require an exact branch checkout. Prove checkout safety
    // before the first provider mutation so a dirty worktree can never turn a
    // partially-mutated sync into an unrepairable decision receipt.
    crate::navigation::ensure_safe_worktree(
        &context.repository_path,
        &context.config_path,
        &runner,
    )?;
    let provider = GitHubMutationAdapter::new(runner);
    lock.checkpoint(
        "provider_convergence_in_flight",
        json!({
            "operation": "sync",
            "repository": &status.repository,
            "default_branch": &status.analysis.fleet.default_branch,
            "initial_status_timing": &status.timing,
            "selected_caravans": if input.all {
                json!(status.analysis.fleet.caravans.iter().map(|caravan| caravan.id).collect::<Vec<_>>())
            } else {
                json!(status.current_pr.and_then(|pr| status.analysis.fleet.containing(pr).map(|caravan| caravan.id)))
            },
            "recovery": "rediscover provider state and replay the same idempotent sync",
        }),
        true,
    )?;
    let convergence_started = Instant::now();
    let progress = execute(
        &status,
        &provider,
        input.all,
        input.rerun_failed,
        context.config.force_merge,
    )?;
    let convergence_elapsed = convergence_started.elapsed();
    lock.checkpoint(
        "provider_converged",
        sync_checkpoint_evidence(&progress),
        false,
    )?;
    lock.checkpoint(
        "final_discovery_in_flight",
        sync_checkpoint_evidence(&progress),
        false,
    )?;

    // A fresh graph is the authoritative completion receipt. It detects a
    // default-branch or fleet change that raced after the preflight proof.
    let final_status_started = Instant::now();
    let final_status = read::status_with_deadline(context, operation_deadline).map_err(|error| {
        AppError::structured(
            error.category(),
            if error.category() == ErrorCategory::Timeout {
                "sync_operation_timeout"
            } else {
                "sync_rediscovery_failed"
            },
            error.to_string(),
            Some(json!({
                "operation_receipt": progress.operation_receipt(),
                "provider_receipts": progress.provider_receipts,
                "events": progress.events,
                "phase": "final_status",
                "elapsed_ms": duration_millis(started.elapsed()),
                "deadline_ms": duration_millis(operation_deadline.saturating_duration_since(started)),
                "source": error.details(),
                "resumable": true,
                "next": "rerun `cara sync` to rediscover GitHub state",
            })),
        )
    })?;
    let final_status_elapsed = final_status_started.elapsed();
    if let Some(problem) = final_status.analysis.fleet.problems.first() {
        return Err(decision_error(
            &decision_for_problem(problem, &final_status, &progress),
            &progress,
        ));
    }

    lock.checkpoint("completed", sync_checkpoint_evidence(&progress), false)?;

    Ok(SyncOutput {
        receipt: progress.operation_receipt(),
        timing: Some(SyncTiming {
            deadline_ms: duration_millis(operation_deadline.saturating_duration_since(started)),
            total_ms: duration_millis(started.elapsed()),
            initial_status_ms: duration_millis(initial_status_elapsed),
            provider_convergence_ms: duration_millis(convergence_elapsed),
            final_status_ms: duration_millis(final_status_elapsed),
        }),
        lock_recovery,
        provider_receipts: progress.provider_receipts,
        historical_predecessor: read::historical_predecessor(&status),
        synchronized_caravans: progress.synchronized_caravans,
        paused_caravans: progress.paused_caravans,
        head_advancements: progress.head_advancements,
        ci: progress.ci,
        events: progress.events,
        hook_deliveries: Vec::new(),
        status: final_status,
    })
}

fn attach_lock_recovery(
    error: AppError,
    lock_recovery: Option<&OperationLockRecovery>,
) -> AppError {
    let Some(lock_recovery) = lock_recovery else {
        return error;
    };
    let mut details = error.details().unwrap_or_else(|| json!({}));
    if let Some(object) = details.as_object_mut() {
        object.insert("lock_recovery".to_owned(), json!(lock_recovery));
        object.insert(
            "lock_recovery_next".to_owned(),
            json!("provider state was rediscovered after exact dead-owner cleanup; rerun the same idempotent command after repairing this error"),
        );
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

fn execute(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    all: bool,
    rerun_failed: bool,
    force_merge: bool,
) -> Result<SyncProgress, AppError> {
    let mut caravans = select_caravans(status, all)?;
    let paused_caravans = status
        .pauses
        .iter()
        .filter(|pause| {
            pause.state != crate::pause::PauseState::Stale
                && caravans
                    .iter()
                    .any(|caravan| caravan.id == pause.record.caravan_head)
        })
        .cloned()
        .collect::<Vec<_>>();
    caravans.retain(|caravan| {
        !paused_caravans
            .iter()
            .any(|pause| pause.record.caravan_head == caravan.id)
    });
    let synchronized_caravans = caravans.iter().map(|caravan| caravan.id).collect();
    let mut progress = SyncProgress::new(status, synchronized_caravans);
    progress.paused_caravans = paused_caravans;
    for pause in &progress.paused_caravans {
        progress.steps.push(MutationStep {
            kind: MutationKind::DisableAutoMerge,
            state: MutationStepState::AlreadySatisfied,
            pr: Some(pause.record.caravan_head),
            summary: format!("caravan #{} intentionally paused ({:?}); no mutation; after recovery explicitly run `cara resume --head-pr {} --actor <actor>`", pause.record.caravan_head, pause.state, pause.record.caravan_head),
        });
    }
    if caravans.is_empty() {
        return Ok(progress);
    }

    preflight_repository(provider, status, &progress)?;
    validate_graph(status, &caravans, &progress)?;

    for caravan in &caravans {
        reconcile_caravan(
            status,
            provider,
            caravan,
            rerun_failed,
            force_merge,
            &mut progress,
        )?;
    }

    Ok(progress)
}

fn reconcile_caravan(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    caravan: &Caravan,
    rerun_failed: bool,
    force_merge: bool,
    progress: &mut SyncProgress,
) -> Result<(), AppError> {
    let head = caravan.head().expect("caravans are non-empty");
    if let Some(predecessor) = merged_predecessor(status, caravan) {
        progress.ensure_base(provider, &status.repository, head, &status.default_branch)?;
        progress.record_head_advancement(predecessor.number, head, status);
    } else {
        progress.ensure_base(provider, &status.repository, head, &status.default_branch)?;
    }

    let mut forced_head = false;
    for number in caravan.members.iter().copied() {
        let observation = progress.observe_ci(provider, &status.repository, number)?;
        let disposition = observation.disposition;
        progress.ci.push(observation.clone());
        if disposition == CiDisposition::Failed {
            if rerun_failed {
                progress.rerun_exact_failed_runs(
                    provider,
                    &status.repository,
                    number,
                    &observation.rerunnable_run_ids,
                )?;
            }
            return Err(ci_decision_error(status, caravan, &observation, progress));
        }
        forced_head |= number == head && disposition == CiDisposition::Forced;
    }

    if forced_head {
        return force_merge_head(status, provider, caravan, force_merge, progress);
    }

    // Repair externally enabled non-heads before enabling the head so sync
    // never creates a transient two-auto-merge window.
    for number in caravan.members.iter().skip(1).copied() {
        progress.ensure_auto_merge_disabled(provider, &status.repository, number)?;
    }
    progress.ensure_squash_auto_merge(provider, &status.repository, head)
}

#[allow(clippy::too_many_lines)]
fn force_merge_head(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    caravan: &Caravan,
    force_merge: bool,
    progress: &mut SyncProgress,
) -> Result<(), AppError> {
    let head = caravan.head().expect("caravan head");
    let current = progress
        .current
        .get(&head)
        .expect("current head facts")
        .clone();
    if !force_merge {
        return Err(force_merge_denied(
            status,
            caravan,
            progress,
            "repository policy has force_merge=false",
            None,
        ));
    }
    if current.state != PullRequestState::Open
        || current.draft
        || !current.has_label("caravan-force")
    {
        return Err(force_merge_denied(
            status,
            caravan,
            progress,
            "forced head must remain open, non-draft, and labelled caravan-force",
            None,
        ));
    }
    if !head_is_conflict_free_with_default(status, &current) {
        return Err(force_merge_denied(
            status,
            caravan,
            progress,
            "forced head is not proven conflict-free with the exact default-branch revision",
            None,
        ));
    }

    provider
        .verify_branch_head(
            &status.repository,
            &status.default_branch,
            &status.analysis.fleet.default_branch.oid,
        )
        .map_err(|error| mutation_error(&error, progress, Some(head)))?;

    progress.events.push(progress.event(
        EventKind::ForceMergeAttempted,
        Some(caravan.id),
        vec![head],
        Some("non-successful CI state accepted by explicit caravan-force policy".to_owned()),
        force_event_metadata(progress, head),
    ));

    let permission = provider
        .viewer_permission(&status.repository)
        .map_err(|error| mutation_error(&error, progress, Some(head)))?;
    if permission != "ADMIN" {
        let error = MutationError::PermissionDenied {
            required: "ADMIN".to_owned(),
            actual: permission,
        };
        return Err(force_merge_denied(
            status,
            caravan,
            progress,
            "authenticated actor cannot perform the required administrator squash merge",
            Some(&error),
        ));
    }

    let mut before_labels = current.labels.clone();
    before_labels.remove("caravan-force");
    let observation = progress
        .ci
        .iter()
        .find(|item| item.pr == head)
        .expect("forced head has CI observation");
    let compatibility = status
        .analysis
        .compatibility
        .iter()
        .find(|report| {
            report.candidate == current.head
                && report.target == status.analysis.fleet.default_branch
        })
        .expect("force policy requires exact compatibility proof");
    let audit = ControlLabelAudit {
        operation: "force_accept".to_owned(),
        marker: control_label_marker(
            "force_accept",
            head,
            &current.head.oid,
            &before_labels,
            &current.labels,
        ),
        before_labels,
        after_labels: current.labels.clone(),
        actor: "authenticated GitHub comment author; cara sync/loop force policy".to_owned(),
        reason: format!(
            "observed external `caravan-force`; force_merge=true; ADMIN permission confirmed; observed checks (including pending, running, failed, or empty): {}",
            serde_json::to_string(&observation.checks).expect("checks serialize")
        ),
        reason_source: "deterministic evidence from GitHub checks, external label, repository config, and permission preflight".to_owned(),
        compatibility_evidence: format!(
            "{}@{} -> {}@{} = {:?}",
            compatibility.candidate.name,
            compatibility.candidate.oid.0,
            compatibility.target.name,
            compatibility.target.oid.0,
            compatibility.outcome,
        ),
        clean_squash_evidence: "exact head/default compatibility is clean; ADMIN squash merge is the configured force action".to_owned(),
        admission_priority_basis: "not applicable: force acceptance preserves existing caravan order".to_owned(),
    };
    progress.ensure_control_label_comment(provider, &status.repository, head, &audit)?;

    // Non-heads are repaired before the exceptional merge so force cannot
    // create a transient second auto-merge candidate.
    for number in caravan.members.iter().skip(1).copied() {
        progress.ensure_auto_merge_disabled(provider, &status.repository, number)?;
    }
    let receipt =
        match provider.admin_squash_merge(&status.repository, &progress.precondition(head)) {
            Ok(receipt) => receipt,
            Err(error @ MutationError::PermissionDenied { .. }) => {
                return Err(force_merge_denied(
                    status,
                    caravan,
                    progress,
                    "authenticated actor cannot perform the required administrator squash merge",
                    Some(&error),
                ));
            }
            Err(error) => return Err(mutation_error(&error, progress, Some(head))),
        };
    progress.record(receipt, "administrator squash-merged forced caravan head");
    progress.events.push(progress.event(
        EventKind::ForceMergeCompleted,
        Some(caravan.id),
        vec![head],
        Some("administrator squash merge completed".to_owned()),
        force_event_metadata(progress, head),
    ));

    // At most one exceptional merge is attempted per tick. If a child exists,
    // advance it normally and leave any forced child for the next fresh tick.
    if let Some(new_head) = caravan.members.get(1).copied() {
        progress.ensure_base(
            provider,
            &status.repository,
            new_head,
            &status.default_branch,
        )?;
        progress.record_head_advancement(head, new_head, status);
        for number in caravan.members.iter().skip(2).copied() {
            progress.ensure_auto_merge_disabled(provider, &status.repository, number)?;
        }
        progress.ensure_squash_auto_merge(provider, &status.repository, new_head)?;
    }
    Ok(())
}

fn head_is_conflict_free_with_default(status: &StatusOutput, head: &PullRequestSnapshot) -> bool {
    status.analysis.compatibility.iter().any(|report| {
        report.candidate == head.head
            && report.target == status.analysis.fleet.default_branch
            && report.outcome == CompatibilityOutcome::Clean
    })
}

fn sync_checkpoint_evidence(progress: &SyncProgress) -> Value {
    json!({
        "operation_receipt": progress.operation_receipt(),
        "provider_receipts": progress.provider_receipts.iter().map(|receipt| json!({
            "kind": receipt.kind,
            "before": receipt.before.as_ref().map(PullRequestPrecondition::from),
            "after": PullRequestPrecondition::from(&receipt.after),
        })).collect::<Vec<_>>(),
        "events": progress.events.iter().map(|event| json!({
            "event_id": event.event_id,
            "kind": event.kind,
            "prs": event.prs,
        })).collect::<Vec<_>>(),
        "recovery": "rediscover provider state and replay the same idempotent sync",
    })
}

fn force_event_metadata(progress: &SyncProgress, head: PrNumber) -> BTreeMap<String, Value> {
    let mut metadata = BTreeMap::new();
    metadata.insert("head".to_owned(), json!(progress.current.get(&head)));
    metadata.insert(
        "ci".to_owned(),
        json!(progress.ci.iter().find(|item| item.pr == head)),
    );
    metadata.insert(
        "operation_receipt".to_owned(),
        json!(progress.operation_receipt()),
    );
    metadata
}

fn force_merge_denied(
    status: &StatusOutput,
    caravan: &Caravan,
    progress: &SyncProgress,
    message: &str,
    error: Option<&MutationError>,
) -> AppError {
    let head = caravan.head().expect("caravan head");
    let mut evidence = BTreeMap::new();
    evidence.insert(
        "default_branch".to_owned(),
        json!(status.analysis.fleet.default_branch),
    );
    evidence.insert("head".to_owned(), json!(progress.current.get(&head)));
    evidence.insert(
        "ci".to_owned(),
        json!(progress.ci.iter().find(|item| item.pr == head)),
    );
    evidence.insert("events".to_owned(), json!(progress.events));
    if let Some(error) = error {
        evidence.insert("provider_error".to_owned(), json!(format!("{error:?}")));
        if let MutationError::PermissionDenied { required, actual } = error {
            evidence.insert(
                "permission".to_owned(),
                json!({"required": required, "actual": actual}),
            );
        }
    }
    let decision = DecisionPoint {
        kind: DecisionKind::ForceMergeDenied,
        operation_id: progress.operation_id.clone(),
        repository: status.repository.clone(),
        caravan_id: Some(caravan.id),
        affected_prs: vec![head],
        message: message.to_owned(),
        evidence,
        completed_steps: progress.steps.clone(),
        resumable: true,
        suggested_actions: vec![
            "repair CI and remove caravan-force, or satisfy the complete force-merge policy"
                .to_owned(),
            "rerun the same `cara sync` command after GitHub facts change".to_owned(),
        ],
    };
    decision_error(&decision, progress)
}

fn ci_decision_error(
    status: &StatusOutput,
    caravan: &Caravan,
    observation: &CiObservation,
    progress: &mut SyncProgress,
) -> AppError {
    let event = progress.event(
        EventKind::CiFailed,
        Some(caravan.id),
        vec![observation.pr],
        Some("unforced terminal or unknown CI state requires a decision".to_owned()),
        BTreeMap::from([
            ("ci".to_owned(), json!(observation)),
            (
                "pull_request".to_owned(),
                json!(progress.current.get(&observation.pr)),
            ),
        ]),
    );
    progress.events.push(event.clone());
    let mut evidence = BTreeMap::new();
    evidence.insert(
        "default_branch".to_owned(),
        json!(status.analysis.fleet.default_branch),
    );
    evidence.insert("fleet".to_owned(), json!(status.analysis.fleet));
    evidence.insert("ci".to_owned(), json!(observation));
    evidence.insert(
        "pull_request".to_owned(),
        json!(progress.current.get(&observation.pr)),
    );
    evidence.insert("event".to_owned(), json!(event));
    let mut suggested_actions = vec![
        "repair and push the affected PR, then rerun `cara sync`".to_owned(),
        format!(
            "run `cara evict --pr {} --reason <text>` or split the caravan",
            observation.pr
        ),
        "apply caravan-force only for a known acceptable failure".to_owned(),
    ];
    if observation
        .failure_diagnostics
        .iter()
        .any(|diagnostic| diagnostic.action == WorkflowFailureAction::FreshCandidateTrigger)
    {
        suggested_actions.insert(
            0,
            "trigger a fresh exact-candidate workflow; do not rerun the listed stale generation"
                .to_owned(),
        );
    } else if !observation.rerunnable_run_ids.is_empty() {
        suggested_actions.insert(
            0,
            "rerun only the listed current-generation infrastructure runs with `cara sync --rerun-failed`"
                .to_owned(),
        );
    }
    if observation
        .failure_diagnostics
        .iter()
        .any(|diagnostic| diagnostic.action == WorkflowFailureAction::WaitOrInspect)
    {
        suggested_actions.insert(
            0,
            "wait for provider evidence or inspect the bounded run/job/step receipts before retrying"
                .to_owned(),
        );
    }
    let decision = DecisionPoint {
        kind: DecisionKind::CiFailure,
        operation_id: progress.operation_id.clone(),
        repository: status.repository.clone(),
        caravan_id: Some(caravan.id),
        affected_prs: vec![observation.pr],
        message: format!("PR #{} has unresolved CI failure", observation.pr),
        evidence,
        completed_steps: progress.steps.clone(),
        resumable: true,
        suggested_actions,
    };
    decision_error(&decision, progress)
}

fn classify_workflow_failure(
    diagnostic: WorkflowRunFailureDiagnostic,
    candidate: Option<&MergeCandidateIdentity>,
) -> ClassifiedWorkflowRunFailure {
    let (mut generation, mut reasons) = workflow_run_generation(&diagnostic);
    let mut candidate_lineage_unknown = false;
    if let Some(candidate) = candidate {
        if candidate.pr != diagnostic.expected_pr {
            generation = WorkflowRunGeneration::MissingAssociation;
            reasons.push(format!(
                "candidate identity belongs to PR {}, expected {}",
                candidate.pr, diagnostic.expected_pr
            ));
        } else if candidate.stale_head || candidate.stale_base {
            generation = match (candidate.stale_head, candidate.stale_base) {
                (true, true) => WorkflowRunGeneration::StaleHeadAndBase,
                (true, false) => WorkflowRunGeneration::StaleHead,
                (false, true) => WorkflowRunGeneration::StaleBase,
                (false, false) => WorkflowRunGeneration::Current,
            };
            reasons.extend(
                candidate
                    .stale_reasons
                    .iter()
                    .map(|reason| format!("current synthetic candidate is stale: {reason}")),
            );
        } else if matches!(
            candidate.freshness,
            crate::model::MergeCandidateFreshness::Missing
                | crate::model::MergeCandidateFreshness::Unknown
        ) {
            candidate_lineage_unknown = true;
            reasons.push("current synthetic candidate lineage is missing or unknown".to_owned());
        }
    }

    let failed_lineage_step = diagnostic.failed_jobs.iter().any(|job| {
        job.failed_steps.iter().any(|step| {
            let name = step.name.to_ascii_lowercase();
            name.contains("lineage") || name.contains("selected ref")
        })
    });
    let lineage_receipt_present =
        apply_selected_lineage_receipt(&diagnostic, &mut generation, &mut reasons);
    let (classification, action) = if generation != WorkflowRunGeneration::Current {
        (
            WorkflowFailureClass::StaleGeneration,
            WorkflowFailureAction::FreshCandidateTrigger,
        )
    } else if failed_lineage_step && !lineage_receipt_present {
        let cause = if candidate_lineage_unknown {
            "current candidate identity and bounded lineage receipt are unavailable"
        } else {
            "bounded lineage receipt is unavailable"
        };
        reasons.push(format!("lineage verification failed while {cause}"));
        (
            WorkflowFailureClass::Unknown,
            WorkflowFailureAction::FreshCandidateTrigger,
        )
    } else if diagnostic.conclusion.eq_ignore_ascii_case("cancelled") {
        reasons.push("current-generation run was cancelled".to_owned());
        (
            WorkflowFailureClass::Cancelled,
            WorkflowFailureAction::FreshCandidateTrigger,
        )
    } else if retryable_infrastructure(&diagnostic) {
        reasons.push("structured job conclusion indicates retryable infrastructure".to_owned());
        (
            WorkflowFailureClass::RetryableInfrastructure,
            WorkflowFailureAction::RerunFailedJobs,
        )
    } else if diagnostic.failed_jobs.is_empty() {
        reasons.push("provider exposed no bounded failed job/step evidence".to_owned());
        (
            WorkflowFailureClass::Unknown,
            WorkflowFailureAction::WaitOrInspect,
        )
    } else {
        reasons.push("current-generation jobs contain source or test failures".to_owned());
        (
            WorkflowFailureClass::SourceOrTestFailure,
            WorkflowFailureAction::RepairSource,
        )
    };
    ClassifiedWorkflowRunFailure {
        diagnostic,
        generation,
        classification,
        action,
        reasons,
    }
}

fn apply_selected_lineage_receipt(
    diagnostic: &WorkflowRunFailureDiagnostic,
    generation: &mut WorkflowRunGeneration,
    reasons: &mut Vec<String>,
) -> bool {
    let Some(receipt) = diagnostic
        .failed_jobs
        .iter()
        .find_map(|job| job.selected_lineage.as_ref())
    else {
        return false;
    };
    let stale_base = receipt.parents.first() != Some(&diagnostic.expected_base_oid)
        || receipt.expected_base != diagnostic.expected_base_oid;
    let stale_head = receipt.parents.get(1) != Some(&diagnostic.expected_head_oid)
        || receipt.expected_head != diagnostic.expected_head_oid;
    if stale_base || stale_head {
        *generation = match (stale_head, stale_base) {
            (true, true) => WorkflowRunGeneration::StaleHeadAndBase,
            (true, false) => WorkflowRunGeneration::StaleHead,
            (false, true) => WorkflowRunGeneration::StaleBase,
            (false, false) => WorkflowRunGeneration::Current,
        };
    }
    reasons.push(format!(
        "bounded lineage receipt selected ref {} commit {} with parents [{}]",
        receipt.selected_ref,
        receipt.selected_commit,
        receipt
            .parents
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    ));
    true
}

fn workflow_run_generation(
    diagnostic: &WorkflowRunFailureDiagnostic,
) -> (WorkflowRunGeneration, Vec<String>) {
    let Some(association) = diagnostic
        .pull_requests
        .iter()
        .find(|association| association.pr == diagnostic.expected_pr)
    else {
        return (
            WorkflowRunGeneration::MissingAssociation,
            vec!["run has no immutable association for the expected PR".to_owned()],
        );
    };
    let stale_head = association
        .head_oid
        .as_ref()
        .is_some_and(|oid| *oid != diagnostic.expected_head_oid);
    let stale_base = association
        .base_oid
        .as_ref()
        .is_some_and(|oid| *oid != diagnostic.expected_base_oid);
    let generation = match (stale_head, stale_base) {
        (true, true) => WorkflowRunGeneration::StaleHeadAndBase,
        (true, false) => WorkflowRunGeneration::StaleHead,
        (false, true) => WorkflowRunGeneration::StaleBase,
        (false, false) => WorkflowRunGeneration::Current,
    };
    let mut reasons = Vec::new();
    if stale_head {
        reasons.push(format!(
            "run PR head {} does not match current head {}",
            association
                .head_oid
                .as_ref()
                .map_or("missing", |oid| oid.0.as_str()),
            diagnostic.expected_head_oid
        ));
    }
    if stale_base {
        reasons.push(format!(
            "run PR base {} does not match current base {}",
            association
                .base_oid
                .as_ref()
                .map_or("missing", |oid| oid.0.as_str()),
            diagnostic.expected_base_oid
        ));
    }
    (generation, reasons)
}

fn retryable_infrastructure(diagnostic: &WorkflowRunFailureDiagnostic) -> bool {
    const INFRA_CONCLUSIONS: [&str; 4] =
        ["timed_out", "startup_failure", "stale", "action_required"];
    let conclusion_is_infra = |value: &str| {
        INFRA_CONCLUSIONS
            .iter()
            .any(|candidate| value.eq_ignore_ascii_case(candidate))
    };
    conclusion_is_infra(&diagnostic.conclusion)
        || diagnostic
            .failed_jobs
            .iter()
            .any(|job| conclusion_is_infra(&job.conclusion))
}

fn classify_checks(checks: &[CheckSnapshot], forced: bool) -> CiDisposition {
    let fully_successful = !checks.is_empty()
        && checks.iter().all(|check| {
            matches!(
                check.state,
                CheckState::Success | CheckState::Neutral | CheckState::Skipped
            )
        });
    if forced && !fully_successful {
        return CiDisposition::Forced;
    }

    let pending = checks.is_empty()
        || checks.iter().any(|check| {
            matches!(
                check.state,
                CheckState::Expected | CheckState::Queued | CheckState::InProgress
            )
        });
    let failed = checks.iter().any(|check| {
        matches!(
            check.state,
            CheckState::Failure
                | CheckState::Cancelled
                | CheckState::TimedOut
                | CheckState::ActionRequired
                | CheckState::Unknown
        )
    });
    if pending {
        CiDisposition::Waiting
    } else if failed {
        CiDisposition::Failed
    } else {
        CiDisposition::Passing
    }
}

fn check_is_failure(check: &CheckSnapshot) -> bool {
    matches!(
        check.state,
        CheckState::Failure
            | CheckState::Cancelled
            | CheckState::TimedOut
            | CheckState::ActionRequired
            | CheckState::Unknown
    )
}

fn workflow_run_id(url: &str) -> Option<u64> {
    let suffix = url.split_once("/actions/runs/")?.1;
    suffix.split('/').next()?.parse().ok()
}

fn select_rerunnable_run_ids(checks: &[CheckSnapshot], runs: &[WorkflowRunSnapshot]) -> Vec<u64> {
    let failed_run_ids = runs
        .iter()
        .map(|run| run.database_id)
        .collect::<BTreeSet<_>>();
    checks
        .iter()
        .filter(|check| check_is_failure(check))
        .filter_map(|check| check.details_url.as_deref().and_then(workflow_run_id))
        .filter(|run_id| failed_run_ids.contains(run_id))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn event_timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
        .to_string()
}

fn select_caravans(status: &StatusOutput, all: bool) -> Result<Vec<Caravan>, AppError> {
    let mut caravans = if all {
        status.analysis.fleet.caravans.clone()
    } else {
        let current = status.current_pr.ok_or_else(|| {
            if let Some(predecessor) = read::historical_predecessor(status) {
                AppError::structured(
                    ErrorCategory::TargetNotFound,
                    "historical_successor_not_found",
                    format!(
                        "merged Caravan PR #{predecessor} has no unique active rolling successor"
                    ),
                    Some(json!({
                        "historical_predecessor": predecessor,
                        "current_branch": status.current_branch,
                        "fail_closed": true,
                    })),
                )
            } else {
                AppError::validation(
                    "current_pr_not_found",
                    "the current branch has no unique open PR; use `cara sync --all`",
                )
            }
        })?;
        vec![
            status
                .analysis
                .fleet
                .containing(current)
                .cloned()
                .ok_or_else(|| {
                    AppError::validation(
                        "current_pr_not_in_caravan",
                        format!("PR #{current} is not an active caravan member"),
                    )
                })?,
        ]
    };
    caravans.sort_by_key(|caravan| caravan.id);
    Ok(caravans)
}

fn preflight_repository(
    provider: &impl SyncProvider,
    status: &StatusOutput,
    progress: &SyncProgress,
) -> Result<(), AppError> {
    let allows_auto_merge = provider
        .repository_allows_auto_merge(&status.repository)
        .map_err(|error| mutation_error(&error, progress, None))?;
    if !allows_auto_merge {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "auto_merge_not_enabled",
            "repository settings must allow squash auto-merge before synchronization",
            Some(json!({
                "repository": status.repository,
                "resumable": true,
                "next": "enable repository auto-merge and squash merge, then rerun `cara sync`",
            })),
        ));
    }
    let protected = provider
        .branch_is_protected(&status.repository, &status.default_branch)
        .map_err(|error| mutation_error(&error, progress, None))?;
    if !protected {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "default_branch_not_protected",
            "the default branch must have a protection requirement before synchronization",
            Some(json!({
                "repository": status.repository,
                "default_branch": status.default_branch,
                "resumable": true,
                "next": "configure a required status check or review, then rerun `cara sync`",
            })),
        ));
    }
    Ok(())
}

fn validate_graph(
    status: &StatusOutput,
    selected: &[Caravan],
    progress: &SyncProgress,
) -> Result<(), AppError> {
    for problem in &status.analysis.fleet.problems {
        let correctable_auto_merge = problem.kind == GraphProblemKind::AutoMergeInvariant
            && problem.prs.iter().all(|number| {
                selected
                    .iter()
                    .any(|caravan| caravan.members.contains(number))
            });
        let correctable_advancement = problem.kind == GraphProblemKind::DanglingBase
            && recoverable_dangling_problem(status, selected, problem);
        if correctable_auto_merge || correctable_advancement {
            continue;
        }
        return Err(decision_error(
            &decision_for_problem(problem, status, progress),
            progress,
        ));
    }
    Ok(())
}

fn recoverable_dangling_problem(
    status: &StatusOutput,
    selected: &[Caravan],
    problem: &GraphProblem,
) -> bool {
    let [child, predecessor] = problem.prs.as_slice() else {
        return false;
    };
    if !selected
        .iter()
        .any(|caravan| caravan.head() == Some(*child))
    {
        return false;
    }
    let (Some(child), Some(predecessor)) = (
        status.analysis.pull_requests.get(child),
        status.analysis.pull_requests.get(predecessor),
    ) else {
        return false;
    };
    let matching_predecessors = status
        .analysis
        .pull_requests
        .values()
        .filter(|candidate| {
            candidate.state == PullRequestState::Merged
                && candidate.has_label("caravan")
                && candidate.head.name == child.base.name
        })
        .count();
    predecessor.state == PullRequestState::Merged
        && predecessor.has_label("caravan")
        && child.base.name == predecessor.head.name
        && matching_predecessors == 1
}

fn merged_predecessor<'a>(
    status: &'a StatusOutput,
    caravan: &Caravan,
) -> Option<&'a PullRequestSnapshot> {
    let head = status
        .analysis
        .pull_requests
        .get(&caravan.head().expect("caravan head"))?;
    if head.base.name == status.default_branch {
        return None;
    }
    let mut matches = status.analysis.pull_requests.values().filter(|candidate| {
        candidate.state == PullRequestState::Merged
            && candidate.has_label("caravan")
            && candidate.head.name == head.base.name
    });
    let predecessor = matches.next()?;
    matches.next().is_none().then_some(predecessor)
}

fn decision_for_problem(
    problem: &GraphProblem,
    status: &StatusOutput,
    progress: &SyncProgress,
) -> DecisionPoint {
    let kind = match problem.kind {
        GraphProblemKind::Incompatible if problem.prs.len() == 1 => DecisionKind::HeadConflict,
        GraphProblemKind::Incompatible if is_adjacent_pair(status, &problem.prs) => {
            DecisionKind::LinkConflict
        }
        GraphProblemKind::Incompatible => DecisionKind::CrossCaravanConflict,
        _ => DecisionKind::InvalidGraph,
    };
    let caravan_id = problem.prs.iter().find_map(|number| {
        status
            .analysis
            .fleet
            .containing(*number)
            .map(|caravan| caravan.id)
    });
    let mut evidence = BTreeMap::new();
    evidence.insert("problem".to_owned(), json!(problem));
    evidence.insert(
        "default_branch".to_owned(),
        json!(status.analysis.fleet.default_branch),
    );
    evidence.insert("fleet".to_owned(), json!(status.analysis.fleet));
    evidence.insert(
        "pull_requests".to_owned(),
        json!(
            problem
                .prs
                .iter()
                .filter_map(|number| status.analysis.pull_requests.get(number))
                .collect::<Vec<_>>()
        ),
    );
    evidence.insert(
        "compatibility".to_owned(),
        json!(
            status
                .analysis
                .compatibility
                .iter()
                .filter(|report| report.outcome != CompatibilityOutcome::Clean)
                .collect::<Vec<_>>()
        ),
    );
    evidence.insert("events".to_owned(), json!(progress.events));
    evidence.insert("rebase_on_join".to_owned(), json!(status.rebase_on_join));
    let message = if matches!(
        kind,
        DecisionKind::HeadConflict | DecisionKind::LinkConflict
    ) && !status.rebase_on_join.enabled
    {
        format!(
            "{}; rebase_on_join=disabled, so Cara will not rewrite the affected branch",
            problem.message
        )
    } else {
        problem.message.clone()
    };
    DecisionPoint {
        kind,
        operation_id: progress.operation_id.clone(),
        repository: status.repository.clone(),
        caravan_id,
        affected_prs: problem.prs.clone(),
        message,
        evidence,
        completed_steps: progress.steps.clone(),
        resumable: true,
        suggested_actions: suggested_actions(kind, problem, &status.rebase_on_join),
    }
}

fn is_adjacent_pair(status: &StatusOutput, prs: &[PrNumber]) -> bool {
    let [first, second] = prs else {
        return false;
    };
    status.analysis.fleet.caravans.iter().any(|caravan| {
        caravan
            .members
            .windows(2)
            .any(|pair| pair == [*first, *second] || pair == [*second, *first])
    })
}

fn suggested_actions(
    kind: DecisionKind,
    problem: &GraphProblem,
    rebase: &crate::read::RebaseOnJoinStatus,
) -> Vec<String> {
    match kind {
        DecisionKind::HeadConflict | DecisionKind::LinkConflict if !rebase.enabled => vec![
            rebase.required_action.clone().unwrap_or_else(|| {
                "set `rebase_on_join: true` in .caravan/config.yaml".to_owned()
            }),
            "after committing the config change, run `cara check` and then rerun `cara sync --all`"
                .to_owned(),
            problem.prs.last().map_or_else(
                || "inspect `cara status --json` before reshaping".to_owned(),
                |number| {
                    format!("if rewriting is not acceptable, run `cara evict --pr {number} --reason <text>` or split the chain")
                },
            ),
        ],
        DecisionKind::HeadConflict | DecisionKind::LinkConflict => vec![
            "inspect the exact rebase preflight conflict and repair the affected PR before rerunning `cara sync --all`"
                .to_owned(),
            problem.prs.last().map_or_else(
                || "inspect `cara status --json` before reshaping".to_owned(),
                |number| {
                    format!("run `cara evict --pr {number} --reason <text>` or split the chain")
                },
            ),
        ],
        DecisionKind::CrossCaravanConflict => vec![
            "repair one affected caravan head or tail and rerun `cara sync --all`".to_owned(),
            "reshape one caravan with `cara split` or `cara evict`".to_owned(),
        ],
        _ => vec![
            "inspect `cara status --json` and repair the reported graph facts".to_owned(),
            "rerun the same `cara sync` command after the graph is valid".to_owned(),
        ],
    }
}

fn decision_error(decision: &DecisionPoint, progress: &SyncProgress) -> AppError {
    let code = match decision.kind {
        DecisionKind::HeadConflict => "head_conflict",
        DecisionKind::LinkConflict => "link_conflict",
        DecisionKind::CrossCaravanConflict => "cross_caravan_conflict",
        DecisionKind::CiFailure => "ci_failure",
        DecisionKind::InvalidGraph => "invalid_graph",
        DecisionKind::StalePrecondition => "stale_precondition",
        DecisionKind::UnsafeCheckout => "unsafe_checkout",
        DecisionKind::HookFailure => "hook_failure",
        DecisionKind::ForceMergeDenied => "force_merge_denied",
    };
    AppError::structured(
        ErrorCategory::Validation,
        code,
        decision.message.clone(),
        Some(json!({
            "decision": decision,
            "provider_receipts": progress.provider_receipts,
        })),
    )
}

#[derive(Debug)]
struct SyncProgress {
    operation_id: OperationId,
    repository: RepositoryId,
    steps: Vec<MutationStep>,
    provider_receipts: Vec<GitHubMutationReceipt>,
    synchronized_caravans: Vec<PrNumber>,
    paused_caravans: Vec<crate::pause::PauseStatus>,
    head_advancements: Vec<HeadAdvancement>,
    ci: Vec<CiObservation>,
    events: Vec<CaravanEvent>,
    current: BTreeMap<PrNumber, PullRequestSnapshot>,
    merge_candidates: BTreeMap<PrNumber, MergeCandidateIdentity>,
}

impl SyncProgress {
    fn new(status: &StatusOutput, synchronized_caravans: Vec<PrNumber>) -> Self {
        Self {
            operation_id: OperationId::new(),
            repository: status.repository.clone(),
            steps: Vec::new(),
            provider_receipts: Vec::new(),
            synchronized_caravans,
            paused_caravans: Vec::new(),
            head_advancements: Vec::new(),
            ci: Vec::new(),
            events: Vec::new(),
            current: status.analysis.pull_requests.clone(),
            merge_candidates: status
                .merge_candidates
                .iter()
                .cloned()
                .map(|candidate| (candidate.pr, candidate))
                .collect(),
        }
    }

    fn operation_receipt(&self) -> OperationReceipt {
        OperationReceipt {
            operation_id: self.operation_id.clone(),
            operation: "sync".to_owned(),
            changed: self
                .steps
                .iter()
                .any(|step| step.state == MutationStepState::Completed),
            completed_steps: self.steps.clone(),
        }
    }

    fn precondition(&self, number: PrNumber) -> PullRequestPrecondition {
        PullRequestPrecondition::from(
            self.current
                .get(&number)
                .expect("sync member has current PR facts"),
        )
    }

    fn record(&mut self, receipt: GitHubMutationReceipt, summary: &str) {
        let number = receipt.after.number;
        self.current.insert(number, receipt.after.clone());
        self.steps.push(MutationStep {
            kind: receipt.kind,
            state: MutationStepState::Completed,
            pr: Some(number),
            summary: summary.to_owned(),
        });
        self.provider_receipts.push(receipt);
    }

    fn already(&mut self, kind: MutationKind, number: PrNumber, summary: &str) {
        self.steps.push(MutationStep {
            kind,
            state: MutationStepState::AlreadySatisfied,
            pr: Some(number),
            summary: summary.to_owned(),
        });
    }

    fn event(
        &self,
        kind: EventKind,
        caravan_id: Option<PrNumber>,
        prs: Vec<PrNumber>,
        reason: Option<String>,
        metadata: BTreeMap<String, Value>,
    ) -> CaravanEvent {
        CaravanEvent {
            version: 1,
            event_id: EventId::new(),
            operation_id: self.operation_id.clone(),
            kind,
            repository: self.repository.clone(),
            caravan_id,
            prs,
            fleet: None,
            reason,
            metadata,
            timestamp: event_timestamp(),
        }
    }

    fn record_head_advancement(
        &mut self,
        predecessor: PrNumber,
        new_head: PrNumber,
        status: &StatusOutput,
    ) {
        self.head_advancements.push(HeadAdvancement {
            merged_predecessor: predecessor,
            new_head,
            previous_caravan_id: predecessor,
            new_caravan_id: new_head,
        });
        self.events.push(self.event(
            EventKind::HeadAdvanced,
            Some(new_head),
            vec![predecessor, new_head],
            Some("merged predecessor advanced the rolling caravan head".to_owned()),
            BTreeMap::from([
                (
                    "default_branch".to_owned(),
                    json!(status.analysis.fleet.default_branch),
                ),
                (
                    "merged_predecessor".to_owned(),
                    json!(self.current.get(&predecessor)),
                ),
                ("new_head".to_owned(), json!(self.current.get(&new_head))),
            ]),
        ));
    }

    fn observe_ci(
        &self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<CiObservation, AppError> {
        let current = self.current.get(&number).expect("sync member");
        let disposition = classify_checks(&current.checks, current.has_label("caravan-force"));
        let mut failed_runs =
            if matches!(disposition, CiDisposition::Failed | CiDisposition::Forced) {
                provider
                    .failed_runs_for_pull_request(repository, &self.precondition(number))
                    .map_err(|error| mutation_error(&error, self, Some(number)))?
            } else {
                Vec::new()
            };
        failed_runs.sort_by_key(|run| run.database_id);
        failed_runs.dedup_by_key(|run| run.database_id);
        let selected_run_ids = select_rerunnable_run_ids(&current.checks, &failed_runs);
        let failure_diagnostics = if selected_run_ids.is_empty() {
            Vec::new()
        } else {
            provider
                .failed_run_diagnostics(repository, &self.precondition(number), &selected_run_ids)
                .map_err(|error| mutation_error(&error, self, Some(number)))?
                .runs
                .into_iter()
                .map(|diagnostic| {
                    classify_workflow_failure(diagnostic, self.merge_candidates.get(&number))
                })
                .collect::<Vec<_>>()
        };
        let mut rerunnable_run_ids = failure_diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.action == WorkflowFailureAction::RerunFailedJobs)
            .map(|diagnostic| diagnostic.diagnostic.run_id)
            .collect::<Vec<_>>();
        rerunnable_run_ids.sort_unstable();
        rerunnable_run_ids.dedup();
        Ok(CiObservation {
            pr: number,
            disposition,
            checks: current.checks.clone(),
            failed_runs,
            failure_diagnostics,
            rerunnable_run_ids,
        })
    }

    fn rerun_exact_failed_runs(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        number: PrNumber,
        run_ids: &[u64],
    ) -> Result<(), AppError> {
        for run_id in run_ids {
            let receipt = provider
                .rerun_failed_run(repository, &self.precondition(number), *run_id)
                .map_err(|error| mutation_error(&error, self, Some(number)))?;
            self.record(
                receipt,
                &format!("reran failed jobs for exact workflow run {run_id}"),
            );
        }
        Ok(())
    }

    fn ensure_control_label_comment(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        number: PrNumber,
        audit: &ControlLabelAudit,
    ) -> Result<(), AppError> {
        let receipt = provider
            .ensure_control_label_comment(repository, &self.precondition(number), audit)
            .map_err(|error| comment_mutation_error(&error, self, number))?;
        let already = receipt
            .provider_output
            .as_deref()
            .is_some_and(|output| output.starts_with("existing GitHub comment"));
        if already {
            self.already(
                MutationKind::Comment,
                number,
                "control-label audit comment already present",
            );
            self.current.insert(number, receipt.after);
        } else {
            self.record(receipt, "posted durable force-label audit comment");
        }
        Ok(())
    }

    fn ensure_base(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        number: PrNumber,
        base: &str,
    ) -> Result<(), AppError> {
        if self.current.get(&number).expect("sync member").base.name == base {
            self.already(
                MutationKind::SetBase,
                number,
                "head already targets the default branch",
            );
            return Ok(());
        }
        let receipt = provider
            .set_base(repository, &self.precondition(number), base)
            .map_err(|error| mutation_error(&error, self, Some(number)))?;
        self.record(
            receipt,
            "advanced merged predecessor's child to the default branch",
        );
        Ok(())
    }

    fn ensure_auto_merge_disabled(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<(), AppError> {
        if !self
            .current
            .get(&number)
            .expect("sync member")
            .auto_merge
            .enabled
        {
            self.already(
                MutationKind::DisableAutoMerge,
                number,
                "non-head auto-merge already disabled",
            );
            return Ok(());
        }
        let receipt = provider
            .disable_auto_merge(repository, &self.precondition(number))
            .map_err(|error| mutation_error(&error, self, Some(number)))?;
        self.record(receipt, "disabled auto-merge on non-head PR");
        Ok(())
    }

    fn ensure_squash_auto_merge(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<(), AppError> {
        let auto_merge = &self.current.get(&number).expect("sync member").auto_merge;
        if auto_merge.enabled && auto_merge.merge_method == Some(MergeMethod::Squash) {
            self.already(
                MutationKind::EnableAutoMerge,
                number,
                "head squash auto-merge already enabled",
            );
            return Ok(());
        }
        if auto_merge.enabled {
            let receipt = provider
                .disable_auto_merge(repository, &self.precondition(number))
                .map_err(|error| mutation_error(&error, self, Some(number)))?;
            self.record(receipt, "disabled non-squash auto-merge on head");
        }
        let receipt = provider
            .enable_squash_auto_merge(repository, &self.precondition(number))
            .map_err(|error| mutation_error(&error, self, Some(number)))?;
        self.record(receipt, "enabled squash auto-merge on head PR");
        Ok(())
    }
}

fn comment_mutation_error(
    error: &MutationError,
    progress: &SyncProgress,
    affected_pr: PrNumber,
) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "github_comment_failed",
        format!("force label was accepted but its durable GitHub comment failed: {error}"),
        Some(json!({
            "stage": "control_label_comment",
            "affected_pr": affected_pr,
            "operation_receipt": progress.operation_receipt(),
            "provider_receipts": progress.provider_receipts,
            "events": progress.events,
            "resumable": true,
            "dedupe": "deterministic GitHub-visible caravan-control-label-audit marker",
            "next": "rediscover and rerun `cara sync`",
        })),
    )
}

fn mutation_error(
    error: &MutationError,
    progress: &SyncProgress,
    affected_pr: Option<PrNumber>,
) -> AppError {
    if let MutationError::Provider(DiscoveryError::Runner(CommandRunError::Timeout {
        command,
        timeout_ms,
        stdout,
        stderr,
    })) = error
    {
        return AppError::structured(
            ErrorCategory::Timeout,
            "github_mutation_timeout",
            error.to_string(),
            Some(json!({
                "stage": "github_mutation",
                "command": command.display(),
                "timeout_ms": timeout_ms,
                "stdout": stdout,
                "stderr": stderr,
                "operation_receipt": progress.operation_receipt(),
                "provider_receipts": progress.provider_receipts,
                "events": progress.events,
                "affected_pr": affected_pr,
                "resumable": true,
                "next": "rediscover and rerun the same `cara sync` command",
            })),
        );
    }
    if let MutationError::StalePrecondition {
        expected,
        actual,
        changed_fields,
    } = error
    {
        let mut evidence = BTreeMap::<String, Value>::new();
        evidence.insert("expected".to_owned(), json!(expected));
        evidence.insert("actual".to_owned(), json!(actual));
        evidence.insert("changed_fields".to_owned(), json!(changed_fields));
        evidence.insert("events".to_owned(), json!(progress.events));
        let decision = DecisionPoint {
            kind: DecisionKind::StalePrecondition,
            operation_id: progress.operation_id.clone(),
            repository: progress.repository.clone(),
            caravan_id: progress.synchronized_caravans.first().copied(),
            affected_prs: affected_pr.into_iter().collect(),
            message: error.to_string(),
            evidence,
            completed_steps: progress.steps.clone(),
            resumable: true,
            suggested_actions: vec![
                "rediscover GitHub state and rerun the same `cara sync` command".to_owned(),
            ],
        };
        return decision_error(&decision, progress);
    }
    if let MutationError::BranchHeadMismatch {
        branch,
        expected,
        actual,
    } = error
    {
        let decision = DecisionPoint {
            kind: DecisionKind::StalePrecondition,
            operation_id: progress.operation_id.clone(),
            repository: progress.repository.clone(),
            caravan_id: progress.synchronized_caravans.first().copied(),
            affected_prs: affected_pr.into_iter().collect(),
            message: error.to_string(),
            evidence: BTreeMap::from([
                ("branch".to_owned(), json!(branch)),
                ("expected".to_owned(), json!(expected)),
                ("actual".to_owned(), json!(actual)),
                ("events".to_owned(), json!(progress.events)),
            ]),
            completed_steps: progress.steps.clone(),
            resumable: true,
            suggested_actions: vec![
                "rediscover compatibility against the new default branch and rerun `cara sync`"
                    .to_owned(),
            ],
        };
        return decision_error(&decision, progress);
    }
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "github_mutation_failed",
        error.to_string(),
        Some(json!({
            "error": format!("{error:?}"),
            "operation_receipt": progress.operation_receipt(),
            "provider_receipts": progress.provider_receipts,
            "events": progress.events,
            "affected_pr": affected_pr,
            "resumable": true,
            "next": "rediscover and rerun the same `cara sync` command",
        })),
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet, VecDeque};

    use super::*;
    use crate::graph;
    use crate::model::{
        AutoMergeState, BranchSnapshot, CheckSnapshot, CommitOid, CompatibilityReport,
        RepositorySnapshot,
    };

    struct FakeProvider {
        allows_auto_merge: bool,
        branch_protected: bool,
        pulls: RefCell<BTreeMap<PrNumber, PullRequestSnapshot>>,
        failures: RefCell<VecDeque<MutationKind>>,
        calls: RefCell<Vec<MutationKind>>,
        failed_runs: RefCell<BTreeMap<PrNumber, Vec<WorkflowRunSnapshot>>>,
        diagnostic_heads: RefCell<BTreeMap<PrNumber, CommitOid>>,
        diagnostic_job_conclusions: RefCell<BTreeMap<PrNumber, String>>,
        diagnostic_lineage: RefCell<BTreeMap<PrNumber, crate::ci::SelectedRefLineageReceipt>>,
        admin_permission: bool,
        branch_head: RefCell<crate::model::CommitOid>,
        audits: RefCell<Vec<ControlLabelAudit>>,
    }

    impl FakeProvider {
        fn with_pull_requests(pulls: Vec<PullRequestSnapshot>) -> Self {
            Self {
                allows_auto_merge: true,
                branch_protected: true,
                pulls: RefCell::new(
                    pulls
                        .into_iter()
                        .map(|pull_request| (pull_request.number, pull_request))
                        .collect(),
                ),
                failures: RefCell::new(VecDeque::new()),
                calls: RefCell::new(Vec::new()),
                failed_runs: RefCell::new(BTreeMap::new()),
                diagnostic_heads: RefCell::new(BTreeMap::new()),
                diagnostic_job_conclusions: RefCell::new(BTreeMap::new()),
                diagnostic_lineage: RefCell::new(BTreeMap::new()),
                admin_permission: true,
                branch_head: RefCell::new(branch("main").oid),
                audits: RefCell::new(Vec::new()),
            }
        }

        fn fail_once(&self, kind: MutationKind) {
            self.failures.borrow_mut().push_back(kind);
        }

        fn mutate(
            &self,
            expected: &PullRequestPrecondition,
            kind: MutationKind,
            change: impl FnOnce(&mut PullRequestSnapshot),
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.calls.borrow_mut().push(kind);
            if self.failures.borrow().front() == Some(&kind) {
                self.failures.borrow_mut().pop_front();
                return Err(MutationError::Provider(
                    crate::github::DiscoveryError::CommandFailed {
                        command: crate::command::CommandSpec::new("fake"),
                        code: Some(1),
                        stderr: "injected failure".to_owned(),
                    },
                ));
            }
            let before = self
                .pulls
                .borrow()
                .get(&expected.number)
                .cloned()
                .expect("fake PR");
            let actual = PullRequestPrecondition::from(&before);
            if &actual != expected {
                return Err(MutationError::StalePrecondition {
                    expected: Box::new(expected.clone()),
                    actual: Box::new(actual),
                    changed_fields: vec!["fake_race".to_owned()],
                });
            }
            let mut after = before.clone();
            change(&mut after);
            self.pulls.borrow_mut().insert(after.number, after.clone());
            Ok(GitHubMutationReceipt {
                kind,
                before: Some(before),
                after,
                provider_output: None,
            })
        }
    }

    impl SyncProvider for FakeProvider {
        fn verify_branch_head(
            &self,
            _repository: &RepositoryId,
            branch: &str,
            expected: &crate::model::CommitOid,
        ) -> Result<(), MutationError> {
            let actual = self.branch_head.borrow().clone();
            if &actual != expected {
                return Err(MutationError::BranchHeadMismatch {
                    branch: branch.to_owned(),
                    expected: expected.clone(),
                    actual,
                });
            }
            Ok(())
        }

        fn branch_is_protected(
            &self,
            _repository: &RepositoryId,
            _branch: &str,
        ) -> Result<bool, MutationError> {
            Ok(self.branch_protected)
        }

        fn repository_allows_auto_merge(
            &self,
            _repository: &RepositoryId,
        ) -> Result<bool, MutationError> {
            Ok(self.allows_auto_merge)
        }

        fn set_base(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            base: &str,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::SetBase, |pull_request| {
                pull_request.base = branch(base);
            })
        }

        fn enable_squash_auto_merge(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::EnableAutoMerge, |pull_request| {
                pull_request.auto_merge = AutoMergeState::squash();
            })
        }

        fn disable_auto_merge(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::DisableAutoMerge, |pull_request| {
                pull_request.auto_merge = AutoMergeState::disabled();
            })
        }

        fn failed_runs_for_pull_request(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
        ) -> Result<Vec<WorkflowRunSnapshot>, MutationError> {
            let current = self
                .pulls
                .borrow()
                .get(&expected.number)
                .cloned()
                .expect("fake PR");
            let actual = PullRequestPrecondition::from(&current);
            if &actual != expected {
                return Err(MutationError::StalePrecondition {
                    expected: Box::new(expected.clone()),
                    actual: Box::new(actual),
                    changed_fields: vec!["fake_race".to_owned()],
                });
            }
            Ok(self
                .failed_runs
                .borrow()
                .get(&expected.number)
                .cloned()
                .unwrap_or_default())
        }

        fn failed_run_diagnostics(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            run_ids: &[u64],
        ) -> Result<WorkflowFailureDiagnostics, MutationError> {
            let failed_runs = self
                .failed_runs
                .borrow()
                .get(&expected.number)
                .cloned()
                .unwrap_or_default();
            let runs = run_ids
                .iter()
                .filter_map(|run_id| {
                    failed_runs
                        .iter()
                        .find(|run| run.database_id == *run_id)
                        .map(|run| crate::ci::WorkflowRunFailureDiagnostic {
                            run_id: *run_id,
                            attempt: 1,
                            workflow_id: *run_id,
                            check_suite_id: *run_id,
                            workflow_name: run.workflow_name.clone(),
                            event: "pull_request".to_owned(),
                            status: run.status.clone(),
                            conclusion: run.conclusion.clone(),
                            head_branch: "feature".to_owned(),
                            head_sha: CommitOid(run.head_sha.clone()),
                            expected_pr: expected.number,
                            expected_head_oid: expected.head_oid.clone(),
                            expected_base_oid: expected.base_oid.clone(),
                            pull_requests: vec![crate::ci::WorkflowRunPullRequestAssociation {
                                pr: expected.number,
                                head_oid: Some(
                                    self.diagnostic_heads
                                        .borrow()
                                        .get(&expected.number)
                                        .cloned()
                                        .unwrap_or_else(|| expected.head_oid.clone()),
                                ),
                                base_oid: Some(expected.base_oid.clone()),
                            }],
                            failed_jobs: vec![crate::ci::WorkflowJobFailureDiagnostic {
                                job_id: *run_id,
                                name: "test infrastructure".to_owned(),
                                status: "completed".to_owned(),
                                conclusion: self
                                    .diagnostic_job_conclusions
                                    .borrow()
                                    .get(&expected.number)
                                    .cloned()
                                    .unwrap_or_else(|| "timed_out".to_owned()),
                                url: run.url.clone(),
                                runner_name: None,
                                runner_labels: Vec::new(),
                                failed_steps: Vec::new(),
                                steps_truncated: false,
                                selected_lineage: self
                                    .diagnostic_lineage
                                    .borrow()
                                    .get(&expected.number)
                                    .cloned(),
                                lineage_evidence_status: if self
                                    .diagnostic_lineage
                                    .borrow()
                                    .contains_key(&expected.number)
                                {
                                    crate::ci::LineageEvidenceStatus::Parsed
                                } else {
                                    crate::ci::LineageEvidenceStatus::NotRequested
                                },
                            }],
                            jobs_total: 1,
                            jobs_truncated: false,
                        })
                })
                .collect();
            Ok(WorkflowFailureDiagnostics {
                requested_run_ids: run_ids.to_vec(),
                runs,
                runs_truncated: false,
            })
        }

        fn rerun_failed_run(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            run_id: u64,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            let run = self
                .failed_runs
                .borrow()
                .get(&expected.number)
                .and_then(|runs| runs.iter().find(|run| run.database_id == run_id))
                .cloned()
                .expect("exact failed run");
            if !run.pull_requests.contains(&expected.number) {
                return Err(MutationError::RunPullRequestMismatch {
                    run_id,
                    expected_pr: expected.number,
                    actual_prs: run.pull_requests,
                });
            }
            if run.head_sha != expected.head_oid.0 {
                return Err(MutationError::RunHeadMismatch {
                    run_id,
                    expected_head: expected.head_oid.0.clone(),
                    actual_head: run.head_sha,
                });
            }
            self.mutate(expected, MutationKind::RerunChecks, |pull_request| {
                for check in &mut pull_request.checks {
                    if check.details_url.as_deref().and_then(workflow_run_id) == Some(run_id) {
                        check.state = CheckState::Queued;
                        check.provider_state = Some("QUEUED".to_owned());
                    }
                }
            })
        }

        fn viewer_permission(&self, _repository: &RepositoryId) -> Result<String, MutationError> {
            Ok(if self.admin_permission {
                "ADMIN"
            } else {
                "WRITE"
            }
            .to_owned())
        }

        fn ensure_control_label_comment(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            audit: &ControlLabelAudit,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.audits.borrow_mut().push(audit.clone());
            self.mutate(expected, MutationKind::Comment, |_| {})
        }

        fn admin_squash_merge(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            if !self.admin_permission {
                return Err(MutationError::PermissionDenied {
                    required: "ADMIN".to_owned(),
                    actual: "WRITE".to_owned(),
                });
            }
            self.mutate(expected, MutationKind::SquashMerge, |pull_request| {
                pull_request.state = PullRequestState::Merged;
                pull_request.merged_at = Some("now".to_owned());
                pull_request.auto_merge = AutoMergeState::disabled();
            })
        }
    }

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "harryaskham".to_owned(),
            name: "caravan".to_owned(),
        }
    }

    fn branch(name: &str) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository(),
            name: name.to_owned(),
            oid: CommitOid(format!("{name:0<40}")),
        }
    }

    fn pull_request(
        number: u64,
        head: &str,
        base: &str,
        state: PullRequestState,
        auto_merge: AutoMergeState,
    ) -> PullRequestSnapshot {
        PullRequestSnapshot {
            number: PrNumber(number),
            title: format!("PR {number}"),
            url: format!("https://example.invalid/{number}"),
            state,
            draft: false,
            head: branch(head),
            base: branch(base),
            cross_repository: false,
            labels: BTreeSet::from(["caravan".to_owned()]),
            auto_merge,
            checks: Vec::<CheckSnapshot>::new(),
            created_at: Some(format!("2026-01-01T00:00:{number:02}Z")),
            merged_at: (state == PullRequestState::Merged).then(|| "now".to_owned()),
            updated_at: None,
        }
    }

    fn check(name: &str, state: CheckState, run_id: Option<u64>) -> CheckSnapshot {
        CheckSnapshot {
            name: name.to_owned(),
            state,
            provider_state: Some(format!("{state:?}").to_uppercase()),
            details_url: run_id.map(|id| {
                format!("https://github.com/harryaskham/caravan/actions/runs/{id}/job/1")
            }),
        }
    }

    fn selected_lineage_receipt(
        pull_request: &PullRequestSnapshot,
    ) -> crate::ci::SelectedRefLineageReceipt {
        crate::ci::SelectedRefLineageReceipt {
            event: "pull_request".to_owned(),
            head_ref: pull_request.head.name.clone(),
            selected_ref: "refs/pull/1/merge".to_owned(),
            selected_commit: CommitOid("selected-merge".to_owned()),
            actual_head: CommitOid("selected-merge".to_owned()),
            expected_head: pull_request.head.oid.clone(),
            expected_base: pull_request.base.oid.clone(),
            parents: vec![
                pull_request.base.oid.clone(),
                CommitOid("prior-head".to_owned()),
            ],
        }
    }

    fn failed_run(id: u64, head: &PullRequestSnapshot) -> WorkflowRunSnapshot {
        WorkflowRunSnapshot {
            database_id: id,
            pull_requests: vec![head.number],
            head_sha: head.head.oid.0.clone(),
            status: "completed".to_owned(),
            conclusion: "failure".to_owned(),
            event: "pull_request".to_owned(),
            name: "CI".to_owned(),
            workflow_name: "CI".to_owned(),
            url: format!("https://github.com/harryaskham/caravan/actions/runs/{id}"),
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn clean(
        candidate: &BranchSnapshot,
        target: &BranchSnapshot,
    ) -> Result<CompatibilityReport, AppError> {
        Ok(CompatibilityReport {
            candidate: candidate.clone(),
            target: target.clone(),
            outcome: CompatibilityOutcome::Clean,
            conflicting_paths: Vec::new(),
            diagnostic: None,
        })
    }

    fn status(
        pulls: Vec<PullRequestSnapshot>,
        current: Option<PrNumber>,
        checker: &impl graph::CompatibilityChecker,
    ) -> StatusOutput {
        let snapshot = RepositorySnapshot {
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            repository: repository(),
            default_branch: branch("main"),
            current_branch: current.map(|number| format!("pr-{number}")),
            current_pr: current,
            pull_requests: pulls,
            observed_at: None,
        };
        let analysis = graph::analyze(&snapshot, checker).expect("analysis");
        StatusOutput {
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            timing: None,
            repository: repository(),
            rebase_on_join: crate::read::RebaseOnJoinStatus::default(),
            default_branch: "main".to_owned(),
            current_branch: snapshot.current_branch,
            current_pr: snapshot.current_pr,
            healthy: analysis.healthy(),
            initialization: crate::initialization::InitializationStatus::default(),
            admission: read::resolve_admission(
                &analysis,
                &crate::config::CaravanConfig::default().agent_priority_labels,
            ),
            analysis,
            pauses: Vec::new(),
        }
    }

    fn healthy_chain() -> Vec<PullRequestSnapshot> {
        vec![
            pull_request(
                1,
                "one",
                "main",
                PullRequestState::Open,
                AutoMergeState::squash(),
            ),
            pull_request(
                2,
                "two",
                "one",
                PullRequestState::Open,
                AutoMergeState::disabled(),
            ),
            pull_request(
                3,
                "three",
                "two",
                PullRequestState::Open,
                AutoMergeState::disabled(),
            ),
        ]
    }

    #[test]
    fn pending_ci_reports_waiting_without_speculative_mutation() {
        let mut pulls = healthy_chain();
        pulls[0].checks = vec![check("build-test", CheckState::Queued, Some(7))];
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let progress = execute(&status, &provider, false, false, false).expect("pending waits");

        assert_eq!(progress.ci[0].disposition, CiDisposition::Waiting);
        assert!(!progress.operation_receipt().changed);
        assert!(progress.events.is_empty());
        assert!(provider.calls.borrow().is_empty());
    }

    #[test]
    fn unforced_failure_returns_exact_ci_decision_and_canonical_event() {
        let mut pulls = healthy_chain();
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(10))];
        let matching = failed_run(10, &pulls[0]);
        let spurious = failed_run(11, &pulls[0]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![spurious, matching]);
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let error =
            execute(&status, &provider, false, false, false).expect_err("failed CI decides");

        assert_eq!(mcp_cli::StructuredError::code(&error), "ci_failure");
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["decision"]["evidence"]["ci"]["pr"], 1);
        assert_eq!(
            details["decision"]["evidence"]["ci"]["rerunnable_run_ids"],
            json!([10])
        );
        assert_eq!(
            details["decision"]["evidence"]["event"]["kind"],
            "ci_failed"
        );
        assert_eq!(
            details["decision"]["evidence"]["event"]["operation_id"],
            details["decision"]["operation_id"]
        );
        assert!(provider.calls.borrow().is_empty());
    }

    #[test]
    fn stale_run_generation_requires_fresh_trigger_and_is_never_rerunnable() {
        let mut pulls = healthy_chain();
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(10))];
        let matching = failed_run(10, &pulls[0]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![matching]);
        provider
            .diagnostic_lineage
            .borrow_mut()
            .insert(PrNumber(1), selected_lineage_receipt(&pulls[0]));
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let error = execute(&status, &provider, false, true, false)
            .expect_err("stale generation cannot be rerun");

        assert!(provider.calls.borrow().is_empty());
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        let ci = &details["decision"]["evidence"]["ci"];
        assert_eq!(ci["rerunnable_run_ids"], json!([]));
        assert_eq!(ci["failure_diagnostics"][0]["generation"], "stale_head");
        assert_eq!(
            ci["failure_diagnostics"][0]["diagnostic"]["failed_jobs"][0]["selected_lineage"]["selected_commit"],
            "selected-merge"
        );
        assert_eq!(
            ci["failure_diagnostics"][0]["action"],
            "fresh_candidate_trigger"
        );
        assert!(
            details["decision"]["suggested_actions"][0]
                .as_str()
                .expect("action")
                .contains("fresh exact-candidate")
        );
    }

    #[test]
    fn current_generation_source_failure_recommends_repair_not_rerun() {
        let mut pulls = healthy_chain();
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(10))];
        let matching = failed_run(10, &pulls[0]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![matching]);
        provider
            .diagnostic_job_conclusions
            .borrow_mut()
            .insert(PrNumber(1), "failure".to_owned());
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let error = execute(&status, &provider, false, true, false)
            .expect_err("source failure requires repair");

        assert!(provider.calls.borrow().is_empty());
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        let ci = &details["decision"]["evidence"]["ci"];
        assert_eq!(ci["rerunnable_run_ids"], json!([]));
        assert_eq!(
            ci["failure_diagnostics"][0]["classification"],
            "source_or_test_failure"
        );
        assert_eq!(ci["failure_diagnostics"][0]["action"], "repair_source");
    }

    #[test]
    fn unknown_provider_state_is_a_non_rerunnable_ci_decision() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].checks = vec![CheckSnapshot {
            name: "future-ci".to_owned(),
            state: CheckState::Unknown,
            provider_state: Some("FUTURE_PROVIDER_STATE".to_owned()),
            details_url: None,
        }];
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let error = execute(&status, &provider, false, true, false)
            .expect_err("unknown CI cannot be guessed or rerun");

        assert_eq!(mcp_cli::StructuredError::code(&error), "ci_failure");
        assert!(provider.calls.borrow().is_empty());
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(
            details["decision"]["evidence"]["ci"]["checks"][0]["provider_state"],
            "FUTURE_PROVIDER_STATE"
        );
        assert_eq!(
            details["decision"]["evidence"]["ci"]["rerunnable_run_ids"],
            json!([])
        );
    }

    #[test]
    fn rerun_failed_selects_only_exact_current_run_then_stops() {
        let mut pulls = healthy_chain();
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(10))];
        let matching = failed_run(10, &pulls[0]);
        let spurious = failed_run(11, &pulls[0]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![spurious, matching]);
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let error = execute(&status, &provider, false, true, false)
            .expect_err("rerun still returns unresolved decision");

        assert_eq!(*provider.calls.borrow(), vec![MutationKind::RerunChecks]);
        assert_eq!(
            provider.pulls.borrow()[&PrNumber(1)].checks[0].state,
            CheckState::Queued
        );
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert!(
            details["decision"]["completed_steps"]
                .as_array()
                .expect("steps")
                .iter()
                .any(|step| step["summary"] == "reran failed jobs for exact workflow run 10")
        );
    }

    #[test]
    fn forced_downstream_failure_remains_in_chain_without_blocking() {
        let mut pulls = healthy_chain();
        pulls[0].checks = vec![check("build-test", CheckState::Success, Some(1))];
        pulls[1].labels.insert("caravan-force".to_owned());
        pulls[1].checks = vec![check("build-test", CheckState::Failure, Some(20))];
        let run = failed_run(20, &pulls[1]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(2), vec![run]);
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let progress =
            execute(&status, &provider, false, false, true).expect("force bypasses downstream");

        assert_eq!(progress.ci[1].disposition, CiDisposition::Forced);
        assert_eq!(progress.current[&PrNumber(2)].state, PullRequestState::Open);
        assert!(progress.events.is_empty());
        assert!(provider.calls.borrow().is_empty());
    }

    #[test]
    fn force_merge_requires_config_before_provider_attempt() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(30))];
        let run = failed_run(30, &pulls[0]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![run]);
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let error = execute(&status, &provider, false, false, false)
            .expect_err("config denies force merge");

        assert_eq!(mcp_cli::StructuredError::code(&error), "force_merge_denied");
        assert!(provider.calls.borrow().is_empty());
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["decision"]["evidence"]["events"], json!([]));
    }

    #[test]
    fn stale_forced_head_stops_before_admin_attempt() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(32))];
        let run = failed_run(32, &pulls[0]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![run]);
        let status = status(pulls, Some(PrNumber(1)), &clean);
        provider
            .pulls
            .borrow_mut()
            .get_mut(&PrNumber(1))
            .expect("head")
            .labels
            .insert("external-change".to_owned());

        let error =
            execute(&status, &provider, false, false, true).expect_err("stale head fails closed");

        assert_eq!(mcp_cli::StructuredError::code(&error), "stale_precondition");
        assert!(provider.calls.borrow().is_empty());
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["decision"]["evidence"]["events"], json!([]));
    }

    #[test]
    fn moved_default_branch_invalidates_force_compatibility_proof() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(33))];
        let run = failed_run(33, &pulls[0]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![run]);
        let status = status(pulls, Some(PrNumber(1)), &clean);
        *provider.branch_head.borrow_mut() = branch("moved-main").oid;

        let error = execute(&status, &provider, false, false, true)
            .expect_err("moved default invalidates proof");

        assert_eq!(mcp_cli::StructuredError::code(&error), "stale_precondition");
        assert!(provider.calls.borrow().is_empty());
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["decision"]["evidence"]["branch"], "main");
        assert_eq!(details["decision"]["evidence"]["events"], json!([]));
    }

    #[test]
    fn force_comment_failure_is_structured_and_prevents_admin_merge() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(34))];
        let run = failed_run(34, &pulls[0]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![run]);
        provider
            .failures
            .borrow_mut()
            .push_back(MutationKind::Comment);
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let error = execute(&status, &provider, false, false, true)
            .expect_err("comment is part of force receipt");

        assert_eq!(error.code(), "github_comment_failed");
        let details = error.details().expect("details");
        assert_eq!(details["stage"], "control_label_comment");
        assert_eq!(details["resumable"], true);
        assert_eq!(details["events"][0]["kind"], "force_merge_attempted");
        let extracted = crate::hooks::events_from_error(&error);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].kind, EventKind::ForceMergeAttempted);
        assert_eq!(
            json!(extracted[0].event_id),
            details["events"][0]["event_id"]
        );
        assert_eq!(*provider.calls.borrow(), vec![MutationKind::Comment]);
    }

    #[test]
    fn force_merge_permission_denial_preserves_attempt_event() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(31))];
        let run = failed_run(31, &pulls[0]);
        let mut provider = FakeProvider::with_pull_requests(pulls.clone());
        provider.admin_permission = false;
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![run]);
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let error = execute(&status, &provider, false, false, true)
            .expect_err("permission denies force merge");

        assert_eq!(mcp_cli::StructuredError::code(&error), "force_merge_denied");
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(
            details["decision"]["evidence"]["events"][0]["kind"],
            "force_merge_attempted"
        );
        assert!(provider.calls.borrow().is_empty());
    }

    #[test]
    fn forced_head_bypasses_queued_expected_in_progress_and_empty_checks() {
        for checks in [
            vec![check("build-test", CheckState::Queued, Some(40))],
            vec![check("build-test", CheckState::Expected, None)],
            vec![check("build-test", CheckState::InProgress, Some(41))],
            vec![],
        ] {
            let mut pulls = healthy_chain();
            pulls.truncate(1);
            pulls[0].labels.insert("caravan-force".to_owned());
            pulls[0].checks = checks;
            let provider = FakeProvider::with_pull_requests(pulls.clone());
            let status = status(pulls, Some(PrNumber(1)), &clean);

            let progress = execute(&status, &provider, false, false, true)
                .expect("explicit force bypasses every non-successful CI state");

            assert_eq!(progress.ci[0].disposition, CiDisposition::Forced);
            assert_eq!(
                *provider.calls.borrow(),
                vec![MutationKind::Comment, MutationKind::SquashMerge]
            );
            assert_eq!(
                progress.current[&PrNumber(1)].state,
                PullRequestState::Merged
            );
        }
    }

    #[test]
    fn forced_head_bypasses_mixed_pending_and_failed_checks_with_accurate_audit() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![
            check("build-test", CheckState::Failure, Some(42)),
            check("security", CheckState::InProgress, Some(43)),
        ];
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let progress = execute(&status, &provider, false, false, true)
            .expect("explicit force bypasses mixed pending and failed checks");

        assert_eq!(progress.ci[0].disposition, CiDisposition::Forced);
        let audits = provider.audits.borrow();
        assert_eq!(audits.len(), 1);
        assert!(audits[0].reason.contains("observed checks"));
        assert!(audits[0].reason.contains("INPROGRESS"));
        assert!(audits[0].reason.contains("FAILURE"));
        assert!(!audits[0].reason.contains("failed checks:"));
    }

    #[test]
    fn passing_checks_with_stale_force_label_use_normal_auto_merge() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![check("build-test", CheckState::Success, Some(44))];
        pulls[0].auto_merge = AutoMergeState::disabled();
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let progress = execute(&status, &provider, false, false, true)
            .expect("successful CI does not invoke exceptional force");

        assert_eq!(progress.ci[0].disposition, CiDisposition::Passing);
        assert_eq!(
            *provider.calls.borrow(),
            vec![MutationKind::EnableAutoMerge]
        );
        assert!(progress.events.is_empty());
        assert!(provider.audits.borrow().is_empty());
    }

    #[test]
    fn successful_force_merge_is_one_shot_and_advances_child() {
        let mut pulls = healthy_chain();
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(50))];
        pulls[1].labels.insert("caravan-force".to_owned());
        pulls[1].checks = vec![check("build-test", CheckState::Failure, Some(51))];
        pulls[2].checks = vec![check("build-test", CheckState::Success, Some(52))];
        let head_run = failed_run(50, &pulls[0]);
        let child_run = failed_run(51, &pulls[1]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider.failed_runs.borrow_mut().extend([
            (PrNumber(1), vec![head_run]),
            (PrNumber(2), vec![child_run]),
        ]);
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let progress =
            execute(&status, &provider, false, false, true).expect("force merge succeeds");

        assert_eq!(
            progress.current[&PrNumber(1)].state,
            PullRequestState::Merged
        );
        assert_eq!(progress.current[&PrNumber(2)].state, PullRequestState::Open);
        assert_eq!(progress.current[&PrNumber(2)].base.name, "main");
        assert_eq!(
            progress.current[&PrNumber(2)].auto_merge,
            AutoMergeState::squash()
        );
        assert_eq!(
            *provider.calls.borrow(),
            vec![
                MutationKind::Comment,
                MutationKind::SquashMerge,
                MutationKind::SetBase,
                MutationKind::EnableAutoMerge,
            ]
        );
        assert_eq!(progress.head_advancements[0].new_head, PrNumber(2));
        assert_eq!(
            progress
                .events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![
                EventKind::ForceMergeAttempted,
                EventKind::ForceMergeCompleted,
                EventKind::HeadAdvanced,
            ]
        );
        assert_eq!(
            progress.events[0].operation_id,
            progress.events[1].operation_id
        );
    }

    #[test]
    fn dead_owner_recovery_is_preserved_on_later_sync_error() {
        let recovery = OperationLockRecovery {
            path: ".git/caravan/operation.lock".to_owned(),
            removed_owner: crate::operation_lock::OperationLockOwner {
                version: 1,
                pid: 99,
                operation: "sync_decision_checkout".to_owned(),
                created_unix_secs: 1,
                token: "exact-token".to_owned(),
                checkpoint: Some(crate::operation_lock::OperationLockCheckpoint {
                    phase: "decision_checkout_in_flight".to_owned(),
                    updated_unix_ms: 2,
                    evidence: json!({ "pr": 2008 }),
                    provider_state_indeterminate: false,
                }),
            },
            age_secs: 3,
            owner_alive: false,
            token_verified: true,
        };
        let error = AppError::validation("repository_not_initialized", "repair init");

        let error = attach_lock_recovery(error, Some(&recovery));

        let details = error.details().unwrap();
        assert_eq!(
            details["lock_recovery"]["removed_owner"]["token"],
            "exact-token"
        );
        assert_eq!(
            details["lock_recovery"]["removed_owner"]["checkpoint"]["phase"],
            "decision_checkout_in_flight"
        );
    }

    #[test]
    fn whole_sync_budget_is_bounded_below_the_client_ceiling() {
        let mut context = AppContext::default();
        assert_eq!(sync_operation_budget(&context), Duration::from_secs(150));
        context.config.command_timeout_secs = 10;
        assert_eq!(sync_operation_budget(&context), Duration::from_secs(50));
        context.config.command_timeout_secs = 100;
        assert_eq!(sync_operation_budget(&context), Duration::from_secs(150));
    }

    #[test]
    fn decision_checkout_targets_the_repair_pr_only_when_unambiguous() {
        let decision = |kind, affected_prs| DecisionPoint {
            kind,
            operation_id: OperationId::new(),
            repository: repository(),
            caravan_id: Some(PrNumber(1)),
            affected_prs,
            message: "repair".to_owned(),
            evidence: BTreeMap::new(),
            completed_steps: Vec::new(),
            resumable: true,
            suggested_actions: Vec::new(),
        };

        assert_eq!(
            decision_checkout_target(&decision(DecisionKind::HeadConflict, vec![PrNumber(1)])),
            Some(PrNumber(1))
        );
        assert_eq!(
            decision_checkout_target(&decision(
                DecisionKind::LinkConflict,
                vec![PrNumber(1), PrNumber(2)]
            )),
            Some(PrNumber(2))
        );
        assert_eq!(
            decision_checkout_target(&decision(
                DecisionKind::CrossCaravanConflict,
                vec![PrNumber(1), PrNumber(4)]
            )),
            None
        );
    }

    #[test]
    fn unsafe_decision_checkout_preserves_the_original_decision_error() {
        let temp = tempfile::tempdir().unwrap();
        let context = AppContext {
            repository_path: temp.path().to_path_buf(),
            config_path: temp.path().join("config.yaml"),
            config_existed: false,
            config: crate::config::CaravanConfig::default(),
        };
        let pull_request = pull_request(
            1,
            "one",
            "main",
            PullRequestState::Open,
            AutoMergeState::squash(),
        );
        let decision = DecisionPoint {
            kind: DecisionKind::CiFailure,
            operation_id: OperationId::new(),
            repository: repository(),
            caravan_id: Some(PrNumber(1)),
            affected_prs: vec![PrNumber(1)],
            message: "repair".to_owned(),
            evidence: BTreeMap::from([("pull_request".to_owned(), json!(pull_request))]),
            completed_steps: Vec::new(),
            resumable: true,
            suggested_actions: Vec::new(),
        };
        let error = AppError::structured(
            ErrorCategory::Validation,
            "ci_failure",
            "repair",
            Some(json!({ "decision": decision })),
        );

        let error = checkout_for_decision(&context, error, Instant::now() + Duration::from_secs(1));

        assert_eq!(error.code(), "ci_failure");
        let details = error.details().unwrap();
        assert_eq!(details["checkout"]["state"], "skipped");
        assert_eq!(details["checkout"]["pr"], 1);
        assert_eq!(
            details["checkout"]["error"]["code"],
            "git_repository_not_found"
        );
    }

    #[test]
    fn repeated_healthy_sync_is_a_noop_with_explicit_steps() {
        let pulls = healthy_chain();
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(2)), &clean);

        let progress = execute(&status, &provider, false, false, false).expect("sync converges");

        assert!(!progress.operation_receipt().changed);
        assert!(provider.calls.borrow().is_empty());
        assert_eq!(progress.synchronized_caravans, vec![PrNumber(1)]);
        assert_eq!(progress.steps.len(), 4);
    }

    #[test]
    fn merged_head_advances_child_and_rolls_caravan_id() {
        let pulls = vec![
            pull_request(
                1,
                "one",
                "main",
                PullRequestState::Merged,
                AutoMergeState::disabled(),
            ),
            pull_request(
                2,
                "two",
                "one",
                PullRequestState::Open,
                AutoMergeState::disabled(),
            ),
            pull_request(
                3,
                "three",
                "two",
                PullRequestState::Open,
                AutoMergeState::disabled(),
            ),
        ];
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(2)), &clean);

        let progress =
            execute(&status, &provider, false, false, false).expect("advancement converges");

        assert!(progress.operation_receipt().changed);
        assert_eq!(
            progress.head_advancements,
            vec![HeadAdvancement {
                merged_predecessor: PrNumber(1),
                new_head: PrNumber(2),
                previous_caravan_id: PrNumber(1),
                new_caravan_id: PrNumber(2),
            }]
        );
        let pulls = provider.pulls.borrow();
        assert_eq!(pulls[&PrNumber(2)].base.name, "main");
        assert_eq!(pulls[&PrNumber(2)].auto_merge, AutoMergeState::squash());
        assert!(!pulls[&PrNumber(3)].auto_merge.enabled);
    }

    #[test]
    fn interrupted_advancement_reports_receipt_and_rerun_resumes() {
        let pulls = vec![
            pull_request(
                1,
                "one",
                "main",
                PullRequestState::Merged,
                AutoMergeState::disabled(),
            ),
            pull_request(
                2,
                "two",
                "one",
                PullRequestState::Open,
                AutoMergeState::disabled(),
            ),
        ];
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider.fail_once(MutationKind::EnableAutoMerge);
        let initial = status(pulls, Some(PrNumber(2)), &clean);

        let error = execute(&initial, &provider, false, false, false).expect_err("enable fails");
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["operation_receipt"]["changed"], true);
        assert_eq!(provider.pulls.borrow()[&PrNumber(2)].base.name, "main");

        let resumed_pulls: Vec<_> = provider.pulls.borrow().values().cloned().collect();
        let resumed = status(resumed_pulls, Some(PrNumber(2)), &clean);
        let progress = execute(&resumed, &provider, false, false, false).expect("rerun resumes");
        assert!(progress.operation_receipt().changed);
        assert_eq!(
            provider.pulls.borrow()[&PrNumber(2)].auto_merge,
            AutoMergeState::squash()
        );
    }

    #[test]
    fn head_conflict_stops_before_mutation_with_exact_evidence() {
        let conflict = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: CompatibilityOutcome::Conflict,
                conflicting_paths: vec!["src/lib.rs".to_owned()],
                diagnostic: Some("merge-tree conflict".to_owned()),
            })
        };
        let pulls = healthy_chain();
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &conflict);

        let error = execute(&status, &provider, false, false, false).expect_err("conflict decides");

        assert_eq!(mcp_cli::StructuredError::code(&error), "head_conflict");
        assert!(provider.calls.borrow().is_empty());
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["decision"]["affected_prs"], json!([1]));
        assert_eq!(
            details["decision"]["evidence"]["compatibility"][0]["conflicting_paths"],
            json!(["src/lib.rs"])
        );
    }

    #[test]
    fn caravan_force_never_bypasses_textual_conflict() {
        let conflict = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: CompatibilityOutcome::Conflict,
                conflicting_paths: vec!["src/conflict.rs".to_owned()],
                diagnostic: None,
            })
        };
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![check("build-test", CheckState::Expected, None)];
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &conflict);

        let error = execute(&status, &provider, false, false, true)
            .expect_err("force cannot bypass conflict");

        assert_eq!(mcp_cli::StructuredError::code(&error), "head_conflict");
        assert!(provider.calls.borrow().is_empty());
    }

    #[test]
    fn mutation_timeout_preserves_category_and_completed_steps() {
        let pulls = healthy_chain();
        let status = status(pulls, Some(PrNumber(1)), &clean);
        let mut progress = SyncProgress::new(&status, vec![PrNumber(1)]);
        progress.steps.push(MutationStep {
            kind: MutationKind::SetBase,
            state: MutationStepState::Completed,
            pr: Some(PrNumber(1)),
            summary: "base advanced".to_owned(),
        });
        let error = mutation_error(
            &MutationError::Provider(DiscoveryError::Runner(CommandRunError::Timeout {
                command: crate::command::CommandSpec::new("gh").args(["pr", "merge"]),
                timeout_ms: 1_200,
                stdout: "partial".to_owned(),
                stderr: "stalled".to_owned(),
            })),
            &progress,
            Some(PrNumber(1)),
        );

        assert_eq!(
            mcp_cli::StructuredError::category(&error),
            ErrorCategory::Timeout
        );
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "github_mutation_timeout"
        );
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["timeout_ms"], 1_200);
        assert_eq!(
            details["operation_receipt"]["completed_steps"][0]["summary"],
            "base advanced"
        );
    }

    #[test]
    fn stale_provider_facts_stop_with_a_resumable_decision() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].auto_merge = AutoMergeState::disabled();
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);
        provider
            .pulls
            .borrow_mut()
            .get_mut(&PrNumber(1))
            .unwrap()
            .labels
            .insert("external-change".to_owned());

        let error = execute(&status, &provider, false, false, false).expect_err("race stops");

        assert_eq!(mcp_cli::StructuredError::code(&error), "stale_precondition");
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["decision"]["kind"], "stale_precondition");
        assert_eq!(details["decision"]["resumable"], true);
        assert_eq!(
            details["decision"]["evidence"]["changed_fields"],
            json!(["fake_race"])
        );
    }

    #[test]
    fn sync_all_processes_caravans_in_head_number_order() {
        let pulls = vec![
            pull_request(
                10,
                "ten",
                "main",
                PullRequestState::Open,
                AutoMergeState::disabled(),
            ),
            pull_request(
                2,
                "two",
                "main",
                PullRequestState::Open,
                AutoMergeState::disabled(),
            ),
        ];
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, None, &clean);

        let progress = execute(&status, &provider, true, false, false).expect("all converges");

        assert_eq!(
            progress.synchronized_caravans,
            vec![PrNumber(2), PrNumber(10)]
        );
        assert_eq!(
            progress
                .provider_receipts
                .iter()
                .map(|receipt| receipt.after.number)
                .collect::<Vec<_>>(),
            vec![PrNumber(2), PrNumber(10)]
        );
    }

    #[test]
    fn adjacent_conflict_is_a_link_decision() {
        let checker = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: if target.name == "main" {
                    CompatibilityOutcome::Clean
                } else {
                    CompatibilityOutcome::Conflict
                },
                conflicting_paths: vec!["src/link.rs".to_owned()],
                diagnostic: None,
            })
        };
        let pulls = healthy_chain();
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(2)), &checker);

        let error = execute(&status, &provider, false, false, false).expect_err("link decides");

        assert_eq!(mcp_cli::StructuredError::code(&error), "link_conflict");
        let details = error.details().expect("decision details");
        assert_eq!(
            details["decision"]["evidence"]["rebase_on_join"]["state"],
            "disabled"
        );
        assert!(
            details["decision"]["message"]
                .as_str()
                .expect("message")
                .contains("rebase_on_join=disabled")
        );
        assert!(
            details["decision"]["suggested_actions"][0]
                .as_str()
                .expect("action")
                .contains("rebase_on_join: true")
        );
        assert!(provider.calls.borrow().is_empty());
    }

    #[test]
    fn sync_all_skips_paused_caravan_and_progresses_independent_caravan() {
        let mut pulls = healthy_chain();
        pulls[0].auto_merge = AutoMergeState::disabled();
        pulls[2].base = branch("main");
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let mut status = status(pulls, Some(PrNumber(1)), &clean);
        let head = status.analysis.pull_requests[&PrNumber(1)].clone();
        let record = crate::pause::PauseRecord {
            version: 1,
            caravan_head: PrNumber(1),
            members: vec![PrNumber(1), PrNumber(2)],
            expected_head: {
                let mut expected = PullRequestPrecondition::from(&head);
                expected.auto_merge = AutoMergeState::squash();
                expected
            },
            expected_checks: head.checks.clone(),
            actor: "oncall".to_owned(),
            reason: "incident".to_owned(),
            paused_unix_secs: 1,
            expires_unix_secs: None,
            external_reference: Some("INC-1".to_owned()),
            resume_authorized_by: None,
        };
        status.pauses.push(crate::pause::PauseStatus {
            record,
            state: crate::pause::PauseState::Active,
            auto_merge_suspended: true,
            safe_next_action: "explicit resume".to_owned(),
        });
        status.analysis.fleet.problems.retain(|problem| {
            !(problem.kind == GraphProblemKind::AutoMergeInvariant
                && problem.prs == vec![PrNumber(1)])
        });

        let progress = execute(&status, &provider, true, false, false)
            .expect("independent caravan progresses");

        assert_eq!(progress.synchronized_caravans, vec![PrNumber(3)]);
        assert_eq!(progress.paused_caravans.len(), 1);
        assert_eq!(
            *provider.calls.borrow(),
            vec![MutationKind::EnableAutoMerge]
        );
        assert!(
            progress
                .steps
                .iter()
                .any(|step| step.summary.contains("intentionally paused"))
        );
    }

    #[test]
    fn externally_enabled_non_head_is_disabled_before_head_repair() {
        let mut pulls = healthy_chain();
        pulls[0].auto_merge = AutoMergeState::disabled();
        pulls[1].auto_merge = AutoMergeState::squash();
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);

        execute(&status, &provider, false, false, false).expect("sync repairs shape");

        assert_eq!(
            *provider.calls.borrow(),
            vec![
                MutationKind::DisableAutoMerge,
                MutationKind::EnableAutoMerge
            ]
        );
    }
}
