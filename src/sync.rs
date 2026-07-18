//! Deterministic, idempotent caravan synchronization.
//!
//! GitHub remains the only durable cursor. Every tick starts from fresh graph
//! facts, proves all compatibility decisions before mutation, applies exact
//! optimistic primitives, and records enough completed work for a rerun to
//! resume after interruption.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use mcp_cli::{ErrorCategory, StructuredError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::command::CommandRunError;
use crate::github::{
    DiscoveryError, GitHubMutationAdapter, GitHubMutationReceipt, MutationError,
    WorkflowRunSnapshot,
};
use crate::hooks::{self, HookDelivery};
use crate::model::{
    Caravan, CaravanEvent, CheckSnapshot, CheckState, CompatibilityOutcome, DecisionKind,
    DecisionPoint, EventId, EventKind, GraphProblem, GraphProblemKind, MergeMethod, MutationKind,
    MutationStep, MutationStepState, OperationId, OperationReceipt, PrNumber,
    PullRequestPrecondition, PullRequestSnapshot, PullRequestState, RepositoryId,
};
use crate::operation_lock::OperationLock;
use crate::read::{self, StatusOutput};
use crate::{AppContext, AppError, SyncInput};

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

/// Exact check and workflow-run evidence for one selected PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CiObservation {
    pub pr: PrNumber,
    pub disposition: CiDisposition,
    #[serde(default)]
    pub checks: Vec<CheckSnapshot>,
    #[serde(default)]
    pub failed_runs: Vec<WorkflowRunSnapshot>,
    #[serde(default)]
    pub rerunnable_run_ids: Vec<u64>,
}

/// Stable result of one converged synchronization tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncOutput {
    pub receipt: OperationReceipt,
    /// Exact provider before/after facts for completed remote mutations.
    #[serde(default)]
    pub provider_receipts: Vec<GitHubMutationReceipt>,
    /// Caravan IDs selected from the initial snapshot, in deterministic order.
    #[serde(default)]
    pub synchronized_caravans: Vec<PrNumber>,
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

    fn rerun_failed_run(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        run_id: u64,
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

    fn rerun_failed_run(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        run_id: u64,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.rerun_failed_run(repository, expected, run_id)
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
    match sync_without_hooks(context, input) {
        Ok(mut output) => {
            output.hook_deliveries = hooks::dispatch_events(context, &output.events)?;
            Ok(output)
        }
        Err(error) => {
            let error = checkout_for_decision(context, error);
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

fn checkout_for_decision(context: &AppContext, error: AppError) -> AppError {
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
    let checkout = match crate::navigation::checkout_decision_pr(context, target) {
        Ok(pull_request) => json!({
            "state": "completed",
            "pr": target,
            "branch": pull_request.head.name,
            "oid": pull_request.head.oid,
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
    let mut details = details;
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

fn sync_without_hooks(context: &AppContext, input: &SyncInput) -> Result<SyncOutput, AppError> {
    let _lock = OperationLock::acquire(&context.repository_path, "sync")?;
    let timeout = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let status = read::status(context)?;
    crate::initialization::require_ready(&status.initialization)?;
    let provider = GitHubMutationAdapter::new(
        crate::command::ProcessRunner::in_directory(&context.repository_path).with_timeout(timeout),
    );
    let progress = execute(
        &status,
        &provider,
        input.all,
        input.rerun_failed,
        context.config.force_merge,
    )?;

    // A fresh graph is the authoritative completion receipt. It detects a
    // default-branch or fleet change that raced after the preflight proof.
    let final_status = read::status(context).map_err(|error| {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "sync_rediscovery_failed",
            error.to_string(),
            Some(json!({
                "operation_receipt": progress.operation_receipt(),
                "provider_receipts": progress.provider_receipts,
                "events": progress.events,
                "resumable": true,
                "next": "rerun `cara sync` to rediscover GitHub state",
            })),
        )
    })?;
    if let Some(problem) = final_status.analysis.fleet.problems.first() {
        return Err(decision_error(
            &decision_for_problem(problem, &final_status, &progress),
            &progress,
        ));
    }

    Ok(SyncOutput {
        receipt: progress.operation_receipt(),
        provider_receipts: progress.provider_receipts,
        synchronized_caravans: progress.synchronized_caravans,
        head_advancements: progress.head_advancements,
        ci: progress.ci,
        events: progress.events,
        hook_deliveries: Vec::new(),
        status: final_status,
    })
}

fn execute(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    all: bool,
    rerun_failed: bool,
    force_merge: bool,
) -> Result<SyncProgress, AppError> {
    let caravans = select_caravans(status, all)?;
    let synchronized_caravans = caravans.iter().map(|caravan| caravan.id).collect();
    let mut progress = SyncProgress::new(status, synchronized_caravans);
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

fn force_merge_head(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    caravan: &Caravan,
    force_merge: bool,
    progress: &mut SyncProgress,
) -> Result<(), AppError> {
    let head = caravan.head().expect("caravan head");
    let current = progress.current.get(&head).expect("current head facts");
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
    if !head_is_conflict_free_with_default(status, current) {
        return Err(force_merge_denied(
            status,
            caravan,
            progress,
            "forced head is not proven conflict-free with the exact default-branch revision",
            None,
        ));
    }

    // Non-heads are repaired before the exceptional merge so force cannot
    // create a transient second auto-merge candidate.
    for number in caravan.members.iter().skip(1).copied() {
        progress.ensure_auto_merge_disabled(provider, &status.repository, number)?;
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
        Some("terminal CI failure accepted by caravan-force policy".to_owned()),
        force_event_metadata(progress, head),
    ));

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
    if !observation.rerunnable_run_ids.is_empty() {
        suggested_actions.insert(
            0,
            "rerun only the listed runs with `cara sync --rerun-failed`".to_owned(),
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

fn classify_checks(checks: &[CheckSnapshot], forced: bool) -> CiDisposition {
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
    } else if failed && forced {
        CiDisposition::Forced
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
            AppError::validation(
                "current_pr_not_found",
                "the current branch has no unique open PR; use `cara sync --all`",
            )
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
    DecisionPoint {
        kind,
        operation_id: progress.operation_id.clone(),
        repository: status.repository.clone(),
        caravan_id,
        affected_prs: problem.prs.clone(),
        message: problem.message.clone(),
        evidence,
        completed_steps: progress.steps.clone(),
        resumable: true,
        suggested_actions: suggested_actions(kind, problem),
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

fn suggested_actions(kind: DecisionKind, problem: &GraphProblem) -> Vec<String> {
    match kind {
        DecisionKind::HeadConflict | DecisionKind::LinkConflict => vec![
            "check out the affected PR, repair and push its exact head, then rerun `cara sync`"
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
    head_advancements: Vec<HeadAdvancement>,
    ci: Vec<CiObservation>,
    events: Vec<CaravanEvent>,
    current: BTreeMap<PrNumber, PullRequestSnapshot>,
}

impl SyncProgress {
    fn new(status: &StatusOutput, synchronized_caravans: Vec<PrNumber>) -> Self {
        Self {
            operation_id: OperationId::new(),
            repository: status.repository.clone(),
            steps: Vec::new(),
            provider_receipts: Vec::new(),
            synchronized_caravans,
            head_advancements: Vec::new(),
            ci: Vec::new(),
            events: Vec::new(),
            current: status.analysis.pull_requests.clone(),
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
        let rerunnable_run_ids = select_rerunnable_run_ids(&current.checks, &failed_runs);
        Ok(CiObservation {
            pr: number,
            disposition,
            checks: current.checks.clone(),
            failed_runs,
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
        admin_permission: bool,
        branch_head: RefCell<crate::model::CommitOid>,
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
                admin_permission: true,
                branch_head: RefCell::new(branch("main").oid),
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
            repository: repository(),
            default_branch: branch("main"),
            current_branch: current.map(|number| format!("pr-{number}")),
            current_pr: current,
            pull_requests: pulls,
            observed_at: None,
        };
        let analysis = graph::analyze(&snapshot, checker).expect("analysis");
        StatusOutput {
            repository: repository(),
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
    fn mixed_pending_and_failure_never_force_merges() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![
            check("build-test", CheckState::Failure, Some(40)),
            check("security", CheckState::InProgress, Some(41)),
        ];
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let progress = execute(&status, &provider, false, false, true).expect("pending wins");

        assert_eq!(progress.ci[0].disposition, CiDisposition::Waiting);
        assert!(provider.calls.borrow().is_empty());
        assert!(progress.events.is_empty());
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
        let decision = DecisionPoint {
            kind: DecisionKind::CiFailure,
            operation_id: OperationId::new(),
            repository: repository(),
            caravan_id: Some(PrNumber(1)),
            affected_prs: vec![PrNumber(1)],
            message: "repair".to_owned(),
            evidence: BTreeMap::new(),
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

        let error = checkout_for_decision(&context, error);

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
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(60))];
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
        assert!(provider.calls.borrow().is_empty());
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
