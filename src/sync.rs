//! Deterministic, idempotent caravan synchronization.
//!
//! GitHub remains the only durable cursor. Every tick starts from fresh graph
//! facts, proves all compatibility decisions before mutation, applies exact
//! optimistic primitives, and records enough completed work for a rerun to
//! resume after interruption.

use std::collections::BTreeMap;

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::command::CommandRunError;
use crate::github::{DiscoveryError, GitHubMutationAdapter, GitHubMutationReceipt, MutationError};
use crate::model::{
    Caravan, CompatibilityOutcome, DecisionKind, DecisionPoint, GraphProblem, GraphProblemKind,
    MergeMethod, MutationKind, MutationStep, MutationStepState, OperationId, OperationReceipt,
    PrNumber, PullRequestPrecondition, PullRequestSnapshot, PullRequestState, RepositoryId,
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
    /// Fresh post-mutation discovery rather than a locally predicted graph.
    pub status: StatusOutput,
}

/// Provider facts and primitives required by sync policy.
pub trait SyncProvider {
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
}

impl<R: crate::command::CommandRunner> SyncProvider for GitHubMutationAdapter<R> {
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
}

/// Synchronize the current caravan or every caravan.
pub fn sync(context: &AppContext, input: &SyncInput) -> Result<SyncOutput, AppError> {
    let _lock = OperationLock::acquire(&context.repository_path, "sync")?;
    let status = read::status(context)?;
    let provider = GitHubMutationAdapter::new(crate::command::ProcessRunner::in_directory(
        &context.repository_path,
    ));
    let progress = execute(&status, &provider, input.all)?;

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
        status: final_status,
    })
}

fn execute(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    all: bool,
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
        let head = caravan.head().expect("caravans are non-empty");
        if let Some(predecessor) = merged_predecessor(status, caravan) {
            progress.ensure_base(provider, &status.repository, head, &status.default_branch)?;
            progress.head_advancements.push(HeadAdvancement {
                merged_predecessor: predecessor.number,
                new_head: head,
                previous_caravan_id: predecessor.number,
                new_caravan_id: head,
            });
        } else {
            progress.ensure_base(provider, &status.repository, head, &status.default_branch)?;
        }

        // Repair externally enabled non-heads before enabling the head so sync
        // never creates a transient two-auto-merge window.
        for number in caravan.members.iter().skip(1).copied() {
            progress.ensure_auto_merge_disabled(provider, &status.repository, number)?;
        }
        progress.ensure_squash_auto_merge(provider, &status.repository, head)?;
    }

    Ok(progress)
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
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "github_mutation_failed",
        error.to_string(),
        Some(json!({
            "error": format!("{error:?}"),
            "operation_receipt": progress.operation_receipt(),
            "provider_receipts": progress.provider_receipts,
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

    #[derive(Default)]
    struct FakeProvider {
        allows_auto_merge: bool,
        branch_protected: bool,
        pulls: RefCell<BTreeMap<PrNumber, PullRequestSnapshot>>,
        failures: RefCell<VecDeque<MutationKind>>,
        calls: RefCell<Vec<MutationKind>>,
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
            merged_at: (state == PullRequestState::Merged).then(|| "now".to_owned()),
            updated_at: None,
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
    fn repeated_healthy_sync_is_a_noop_with_explicit_steps() {
        let pulls = healthy_chain();
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(2)), &clean);

        let progress = execute(&status, &provider, false).expect("sync converges");

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

        let progress = execute(&status, &provider, false).expect("advancement converges");

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

        let error = execute(&initial, &provider, false).expect_err("enable fails");
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["operation_receipt"]["changed"], true);
        assert_eq!(provider.pulls.borrow()[&PrNumber(2)].base.name, "main");

        let resumed_pulls: Vec<_> = provider.pulls.borrow().values().cloned().collect();
        let resumed = status(resumed_pulls, Some(PrNumber(2)), &clean);
        let progress = execute(&resumed, &provider, false).expect("rerun resumes");
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

        let error = execute(&status, &provider, false).expect_err("conflict decides");

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

        let error = execute(&status, &provider, false).expect_err("race stops");

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

        let progress = execute(&status, &provider, true).expect("all converges");

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

        let error = execute(&status, &provider, false).expect_err("link decides");

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

        execute(&status, &provider, false).expect("sync repairs shape");

        assert_eq!(
            *provider.calls.borrow(),
            vec![
                MutationKind::DisableAutoMerge,
                MutationKind::EnableAutoMerge
            ]
        );
    }
}
