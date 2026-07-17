//! Membership policy for creating, renewing, joining, and rejoining caravans.
//!
//! The provider adapter owns exact optimistic commands. This module owns only
//! operation ordering, complete preflight, idempotent resume, and receipts.

use std::collections::BTreeSet;

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::github::{
    CreatePullRequestInput, GitHubMutationAdapter, GitHubMutationReceipt, MutationError,
};
use crate::graph::{CompatibilityChecker, GitCompatibilityChecker};
use crate::model::{
    Caravan, MergeMethod, MutationKind, MutationStep, MutationStepState, OperationId,
    OperationReceipt, PrNumber, PullRequestPrecondition, PullRequestSnapshot, PullRequestState,
    RepositoryId,
};
use crate::operation_lock::OperationLock;
use crate::read::{self, CheckOutput, StatusOutput};
use crate::{AppContext, AppError, CheckInput, CreateInput, JoinInput};

const ACTIVE_LABEL: &str = "caravan";
const EVICTED_LABEL: &str = "caravan-evicted";
const FORCE_LABEL: &str = "caravan-force";
const REQUIRED_LABELS: [&str; 3] = [ACTIVE_LABEL, EVICTED_LABEL, FORCE_LABEL];

/// Membership command kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MembershipOperation {
    New,
    Renew,
    Join,
    Rejoin,
}

impl MembershipOperation {
    fn name(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Renew => "renew",
            Self::Join => "join",
            Self::Rejoin => "rejoin",
        }
    }

    const fn is_join(self) -> bool {
        matches!(self, Self::Join | Self::Rejoin)
    }

    const fn is_renewal(self) -> bool {
        matches!(self, Self::Renew | Self::Rejoin)
    }
}

/// Input normalized across the four membership commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MembershipRequest {
    pub operation: MembershipOperation,
    pub create_pr: bool,
    pub tail_pr: Option<u64>,
    pub head_pr: Option<u64>,
}

/// Successful, resumable membership result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MembershipOutput {
    pub receipt: OperationReceipt,
    /// Exact provider before/after facts for every completed remote mutation.
    #[serde(default)]
    pub provider_receipts: Vec<GitHubMutationReceipt>,
    pub pull_request: PullRequestSnapshot,
    pub caravan_id: PrNumber,
}

/// Provider operations required by membership policy.
pub trait MembershipProvider {
    fn branch_is_protected(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<bool, MutationError>;

    fn repository_allows_auto_merge(
        &self,
        repository: &RepositoryId,
    ) -> Result<bool, MutationError>;

    fn repository_labels(
        &self,
        repository: &RepositoryId,
    ) -> Result<BTreeSet<String>, MutationError>;

    fn create_pull_request(
        &self,
        repository: &RepositoryId,
        input: &CreatePullRequestInput,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn set_base(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        base: &str,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn add_label(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn remove_label(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
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

impl<R: crate::command::CommandRunner> MembershipProvider for GitHubMutationAdapter<R> {
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

    fn repository_labels(
        &self,
        repository: &RepositoryId,
    ) -> Result<BTreeSet<String>, MutationError> {
        self.repository_labels(repository)
    }

    fn create_pull_request(
        &self,
        repository: &RepositoryId,
        input: &CreatePullRequestInput,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.create_pull_request(repository, input)
    }

    fn set_base(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        base: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.set_base(repository, expected, base)
    }

    fn add_label(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.add_label(repository, expected, label)
    }

    fn remove_label(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.remove_label(repository, expected, label)
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

/// Create a new one-PR caravan.
pub fn new(context: &AppContext, input: &CreateInput) -> Result<MembershipOutput, AppError> {
    execute_live(
        context,
        MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: input.create_pr,
            tail_pr: None,
            head_pr: None,
        },
    )
}

/// Renew an evicted PR as a new caravan.
pub fn renew(context: &AppContext, input: &CreateInput) -> Result<MembershipOutput, AppError> {
    execute_live(
        context,
        MembershipRequest {
            operation: MembershipOperation::Renew,
            create_pr: input.create_pr,
            tail_pr: None,
            head_pr: None,
        },
    )
}

/// Append the current PR after a selected or uniquely inferred tail.
pub fn join(context: &AppContext, input: &JoinInput) -> Result<MembershipOutput, AppError> {
    execute_live(
        context,
        MembershipRequest {
            operation: MembershipOperation::Join,
            create_pr: input.create_pr,
            tail_pr: input.tail_pr,
            head_pr: input.head_pr,
        },
    )
}

/// Rejoin an evicted PR after a selected or uniquely inferred tail.
pub fn rejoin(context: &AppContext, input: &JoinInput) -> Result<MembershipOutput, AppError> {
    execute_live(
        context,
        MembershipRequest {
            operation: MembershipOperation::Rejoin,
            create_pr: input.create_pr,
            tail_pr: input.tail_pr,
            head_pr: input.head_pr,
        },
    )
}

fn execute_live(
    context: &AppContext,
    request: MembershipRequest,
) -> Result<MembershipOutput, AppError> {
    let _lock = OperationLock::acquire(&context.repository_path, request.operation.name())?;
    let status = read::status(context)?;
    let checker = GitCompatibilityChecker::new(&context.repository_path, "origin");
    let provider = GitHubMutationAdapter::new(crate::command::ProcessRunner::in_directory(
        &context.repository_path,
    ));
    execute(status, &checker, &provider, request)
}

fn execute(
    mut status: StatusOutput,
    checker: &impl CompatibilityChecker,
    provider: &impl MembershipProvider,
    request: MembershipRequest,
) -> Result<MembershipOutput, AppError> {
    if request.tail_pr.is_some() && request.head_pr.is_some() {
        return Err(AppError::validation(
            "ambiguous_target",
            "--tail-pr and --head-pr are mutually exclusive",
        ));
    }

    let mut state = ExecutionState::new(request.operation);
    preflight_repository(
        provider,
        &status.repository,
        &status.default_branch,
        request.operation,
        &state,
    )?;

    let target = if request.operation.is_join() {
        Some(resolve_join_target(&status, &request)?)
    } else {
        None
    };
    let desired_base = target.as_ref().map_or_else(
        || status.default_branch.clone(),
        |target| target.tail.head.name.clone(),
    );

    if status.current_pr.is_none() {
        if !request.create_pr {
            return Err(AppError::validation(
                "current_pr_not_found",
                "the current branch has no open PR; pass --create-pr to create one non-interactively",
            ));
        }
        let current_branch = status.current_branch.clone().ok_or_else(|| {
            AppError::validation(
                "current_branch_not_found",
                "--create-pr requires a named current branch",
            )
        })?;
        let receipt = provider
            .create_pull_request(
                &status.repository,
                &CreatePullRequestInput {
                    head: current_branch,
                    base: desired_base.clone(),
                    draft: false,
                },
            )
            .map_err(|error| mutation_error(&error, &state))?;
        let created = receipt.after.clone();
        status.current_pr = Some(created.number);
        status
            .analysis
            .pull_requests
            .insert(created.number, created.clone());
        state.record(receipt, "created pull request non-interactively");
    }

    let current_number = status.current_pr.expect("created or discovered current PR");
    let candidate = status
        .analysis
        .pull_requests
        .get(&current_number)
        .cloned()
        .ok_or_else(|| {
            AppError::validation(
                "current_pr_missing_from_snapshot",
                format!("PR #{current_number} was not included in discovery"),
            )
        })?;

    validate_operation_shape(&candidate, &request, &desired_base)?;
    preflight_eligibility(&status, &candidate, &request, target.as_ref(), checker)?;
    state.current = Some(candidate);

    state.ensure_base(provider, &status.repository, &desired_base)?;
    state.ensure_label_absent(provider, &status.repository, FORCE_LABEL)?;
    if request.operation.is_renewal() {
        state.ensure_label_absent(provider, &status.repository, EVICTED_LABEL)?;
    }
    state.ensure_label_present(provider, &status.repository, ACTIVE_LABEL)?;
    if request.operation.is_join() {
        state.ensure_auto_merge_disabled(provider, &status.repository)?;
    } else {
        state.ensure_squash_auto_merge(provider, &status.repository)?;
    }

    let receipt = state.operation_receipt();
    let pull_request = state
        .current
        .expect("membership operation has a current PR");
    let caravan_id = target
        .as_ref()
        .map_or(pull_request.number, |target| target.caravan.id);
    Ok(MembershipOutput {
        receipt,
        provider_receipts: state.provider_receipts,
        pull_request,
        caravan_id,
    })
}

#[derive(Debug, Clone)]
struct JoinTarget {
    caravan: Caravan,
    tail: PullRequestSnapshot,
}

fn resolve_join_target(
    status: &StatusOutput,
    request: &MembershipRequest,
) -> Result<JoinTarget, AppError> {
    let caravan = if let Some(head) = request.head_pr.map(PrNumber) {
        status
            .analysis
            .fleet
            .caravan(head)
            .cloned()
            .ok_or_else(|| {
                AppError::validation(
                    "caravan_head_not_found",
                    format!("PR #{head} is not a current caravan head"),
                )
            })?
    } else if let Some(tail) = request.tail_pr.map(PrNumber) {
        status
            .analysis
            .fleet
            .caravans
            .iter()
            .find(|caravan| caravan.tail() == Some(tail))
            .cloned()
            .ok_or_else(|| {
                AppError::validation(
                    "caravan_tail_not_found",
                    format!("PR #{tail} is not a current caravan tail"),
                )
            })?
    } else {
        match status.analysis.fleet.caravans.as_slice() {
            [caravan] => caravan.clone(),
            [] => {
                return Err(AppError::validation(
                    "caravan_tail_not_found",
                    "there is no caravan to join; use `cara new`",
                ));
            }
            caravans => {
                return Err(AppError::structured(
                    ErrorCategory::Validation,
                    "ambiguous_caravan_tail",
                    "multiple caravan tails exist; pass --tail-pr or --head-pr",
                    Some(json!({
                        "candidate_tails": caravans.iter().filter_map(Caravan::tail).collect::<Vec<_>>(),
                    })),
                ));
            }
        }
    };
    let tail_number = caravan.tail().expect("caravans are non-empty");
    let tail = status
        .analysis
        .pull_requests
        .get(&tail_number)
        .cloned()
        .expect("derived tail has a snapshot");
    Ok(JoinTarget { caravan, tail })
}

fn validate_operation_shape(
    candidate: &PullRequestSnapshot,
    request: &MembershipRequest,
    desired_base: &str,
) -> Result<(), AppError> {
    if candidate.state != PullRequestState::Open {
        return Err(AppError::validation(
            "current_pr_not_open",
            format!("PR #{} is not open", candidate.number),
        ));
    }
    if candidate.draft {
        return Err(AppError::validation(
            "current_pr_is_draft",
            format!("PR #{} is a draft", candidate.number),
        ));
    }
    if candidate.cross_repository {
        return Err(AppError::validation(
            "fork_only_head",
            "Caravan v1 requires the PR head branch in the base repository",
        ));
    }

    let active = candidate.has_label(ACTIVE_LABEL);
    let evicted = candidate.has_label(EVICTED_LABEL);
    match request.operation {
        MembershipOperation::New | MembershipOperation::Join if evicted => {
            return Err(AppError::validation(
                "current_pr_is_evicted",
                "the current PR is evicted; use renew or rejoin",
            ));
        }
        MembershipOperation::Renew | MembershipOperation::Rejoin if !evicted && !active => {
            return Err(AppError::validation(
                "current_pr_not_evicted",
                "renew and rejoin require an evicted PR",
            ));
        }
        _ => {}
    }

    if active && candidate.base.name != desired_base {
        return Err(AppError::validation(
            "active_pr_wrong_target",
            format!(
                "active PR #{} targets `{}` instead of `{desired_base}`",
                candidate.number, candidate.base.name
            ),
        ));
    }
    Ok(())
}

fn preflight_eligibility(
    status: &StatusOutput,
    candidate: &PullRequestSnapshot,
    request: &MembershipRequest,
    target: Option<&JoinTarget>,
    checker: &impl CompatibilityChecker,
) -> Result<CheckOutput, AppError> {
    if candidate.has_label(ACTIVE_LABEL) {
        let unrelated = status.analysis.fleet.problems.iter().filter(|problem| {
            !(problem.kind == crate::model::GraphProblemKind::AutoMergeInvariant
                && problem.prs == [candidate.number])
        });
        if let Some(problem) = unrelated.into_iter().next() {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "invalid_graph",
                "cannot resume membership while unrelated graph problems remain",
                Some(json!({ "problem": problem })),
            ));
        }
        return Ok(CheckOutput {
            mode: if request.operation.is_join() {
                read::CheckMode::JoinTail
            } else {
                read::CheckMode::NewCaravan
            },
            current_pr: candidate.number,
            caravan_id: target
                .map(|target| target.caravan.id)
                .or(Some(candidate.number)),
            target_pr: target.and_then(|target| target.caravan.tail()),
            eligible: true,
            compatibility: status.analysis.compatibility.clone(),
            problems: Vec::new(),
        });
    }

    let mut virtual_status = status.clone();
    if request.operation.is_renewal() {
        let virtual_candidate = virtual_status
            .analysis
            .pull_requests
            .get_mut(&candidate.number)
            .expect("current candidate is present");
        virtual_candidate.labels.remove(EVICTED_LABEL);
        virtual_candidate.labels.remove(FORCE_LABEL);
    }
    let check_input = target.map_or_else(CheckInput::default, |target| CheckInput {
        tail_pr: target.caravan.tail().map(|number| number.0),
        head_pr: None,
    });
    read::check_analysis(&virtual_status, &check_input, checker)
}

fn preflight_repository(
    provider: &impl MembershipProvider,
    repository: &RepositoryId,
    default_branch: &str,
    operation: MembershipOperation,
    state: &ExecutionState,
) -> Result<(), AppError> {
    let labels = provider
        .repository_labels(repository)
        .map_err(|error| mutation_error(&error, state))?;
    require_labels(repository, &labels)?;
    if !operation.is_join()
        && !provider
            .repository_allows_auto_merge(repository)
            .map_err(|error| mutation_error(&error, state))?
    {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "auto_merge_not_enabled",
            "repository settings must allow squash auto-merge before creating a caravan head",
            Some(json!({
                "repository": repository,
                "next": "enable GitHub repository auto-merge and keep squash merge enabled, then rerun the same command",
            })),
        ));
    }
    if !operation.is_join()
        && !provider
            .branch_is_protected(repository, default_branch)
            .map_err(|error| mutation_error(&error, state))?
    {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "default_branch_not_protected",
            "the default branch must have a protection requirement before enabling auto-merge",
            Some(json!({
                "repository": repository,
                "default_branch": default_branch,
                "next": "configure a required status check or review on the default branch, then rerun the same command",
            })),
        ));
    }
    Ok(())
}

fn require_labels(repository: &RepositoryId, labels: &BTreeSet<String>) -> Result<(), AppError> {
    let missing: Vec<_> = REQUIRED_LABELS
        .iter()
        .filter(|label| !labels.contains(**label))
        .copied()
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(AppError::structured(
        ErrorCategory::Validation,
        "required_labels_missing",
        "Caravan's operational labels must exist before the first mutation",
        Some(json!({
            "repository": repository,
            "missing_labels": missing,
            "next": "create caravan, caravan-evicted, and caravan-force labels, then rerun the same command",
        })),
    ))
}

struct ExecutionState {
    operation_id: OperationId,
    operation: MembershipOperation,
    steps: Vec<MutationStep>,
    provider_receipts: Vec<GitHubMutationReceipt>,
    current: Option<PullRequestSnapshot>,
}

impl ExecutionState {
    fn new(operation: MembershipOperation) -> Self {
        Self {
            operation_id: OperationId::new(),
            operation,
            steps: Vec::new(),
            provider_receipts: Vec::new(),
            current: None,
        }
    }

    fn operation_receipt(&self) -> OperationReceipt {
        OperationReceipt {
            operation_id: self.operation_id.clone(),
            operation: self.operation.name().to_owned(),
            changed: self
                .steps
                .iter()
                .any(|step| step.state == MutationStepState::Completed),
            completed_steps: self.steps.clone(),
        }
    }

    fn record(&mut self, receipt: GitHubMutationReceipt, summary: &str) {
        let number = receipt.after.number;
        self.current = Some(receipt.after.clone());
        self.steps.push(MutationStep {
            kind: receipt.kind,
            state: MutationStepState::Completed,
            pr: Some(number),
            summary: summary.to_owned(),
        });
        self.provider_receipts.push(receipt);
    }

    fn already(&mut self, kind: MutationKind, summary: &str) {
        self.steps.push(MutationStep {
            kind,
            state: MutationStepState::AlreadySatisfied,
            pr: self
                .current
                .as_ref()
                .map(|pull_request| pull_request.number),
            summary: summary.to_owned(),
        });
    }

    fn precondition(&self) -> PullRequestPrecondition {
        PullRequestPrecondition::from(
            self.current
                .as_ref()
                .expect("membership execution has current PR facts"),
        )
    }

    fn ensure_base(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &RepositoryId,
        base: &str,
    ) -> Result<(), AppError> {
        if self.current.as_ref().expect("current PR").base.name == base {
            self.already(
                MutationKind::SetBase,
                "PR already targets the required base",
            );
            return Ok(());
        }
        let receipt = provider
            .set_base(repository, &self.precondition(), base)
            .map_err(|error| mutation_error(&error, self))?;
        self.record(receipt, "changed PR base branch");
        Ok(())
    }

    fn ensure_label_present(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &RepositoryId,
        label: &str,
    ) -> Result<(), AppError> {
        if self.current.as_ref().expect("current PR").has_label(label) {
            self.already(
                MutationKind::AddLabel,
                &format!("label `{label}` already present"),
            );
            return Ok(());
        }
        let receipt = provider
            .add_label(repository, &self.precondition(), label)
            .map_err(|error| mutation_error(&error, self))?;
        self.record(receipt, &format!("added label `{label}`"));
        Ok(())
    }

    fn ensure_label_absent(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &RepositoryId,
        label: &str,
    ) -> Result<(), AppError> {
        if !self.current.as_ref().expect("current PR").has_label(label) {
            self.already(
                MutationKind::RemoveLabel,
                &format!("label `{label}` already absent"),
            );
            return Ok(());
        }
        let receipt = provider
            .remove_label(repository, &self.precondition(), label)
            .map_err(|error| mutation_error(&error, self))?;
        self.record(receipt, &format!("removed label `{label}`"));
        Ok(())
    }

    fn ensure_squash_auto_merge(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &RepositoryId,
    ) -> Result<(), AppError> {
        let current = self.current.as_ref().expect("current PR");
        if current.auto_merge.enabled
            && current.auto_merge.merge_method == Some(MergeMethod::Squash)
        {
            self.already(
                MutationKind::EnableAutoMerge,
                "squash auto-merge already enabled",
            );
            return Ok(());
        }
        let receipt = provider
            .enable_squash_auto_merge(repository, &self.precondition())
            .map_err(|error| mutation_error(&error, self))?;
        self.record(receipt, "enabled squash auto-merge");
        Ok(())
    }

    fn ensure_auto_merge_disabled(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &RepositoryId,
    ) -> Result<(), AppError> {
        if !self
            .current
            .as_ref()
            .expect("current PR")
            .auto_merge
            .enabled
        {
            self.already(
                MutationKind::DisableAutoMerge,
                "auto-merge already disabled",
            );
            return Ok(());
        }
        let receipt = provider
            .disable_auto_merge(repository, &self.precondition())
            .map_err(|error| mutation_error(&error, self))?;
        self.record(receipt, "disabled auto-merge");
        Ok(())
    }
}

fn mutation_error(error: &MutationError, state: &ExecutionState) -> AppError {
    let (category, code) = if matches!(error, MutationError::StalePrecondition { .. }) {
        (ErrorCategory::Validation, "stale_precondition")
    } else {
        (ErrorCategory::ExecutionFailure, "github_mutation_failed")
    };
    AppError::structured(
        category,
        code,
        error.to_string(),
        Some(json!({
            "error": format!("{error:?}"),
            "operation_id": state.operation_id,
            "completed_steps": state.steps,
            "provider_receipts": state.provider_receipts,
            "resumable": true,
            "next": format!("rediscover and rerun `cara {}`", state.operation.name()),
        })),
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::graph::analyze;
    use crate::model::{
        AutoMergeState, BranchSnapshot, CommitOid, CompatibilityOutcome, CompatibilityReport,
    };

    #[derive(Default)]
    struct FakeProvider {
        labels: BTreeSet<String>,
        allows_auto_merge: bool,
        branch_protected: bool,
        pull_requests: RefCell<BTreeMap<PrNumber, PullRequestSnapshot>>,
        fail_kind: RefCell<Option<MutationKind>>,
    }

    impl FakeProvider {
        fn with_pull_requests(pull_requests: Vec<PullRequestSnapshot>) -> Self {
            Self {
                labels: REQUIRED_LABELS.into_iter().map(str::to_owned).collect(),
                allows_auto_merge: true,
                branch_protected: true,
                pull_requests: RefCell::new(
                    pull_requests
                        .into_iter()
                        .map(|pull_request| (pull_request.number, pull_request))
                        .collect(),
                ),
                fail_kind: RefCell::new(None),
            }
        }

        fn mutate(
            &self,
            expected: &PullRequestPrecondition,
            kind: MutationKind,
            update: impl FnOnce(&mut PullRequestSnapshot),
        ) -> Result<GitHubMutationReceipt, MutationError> {
            let mut pull_requests = self.pull_requests.borrow_mut();
            let current = pull_requests.get_mut(&expected.number).expect("fake PR");
            let actual = PullRequestPrecondition::from(&*current);
            if actual != *expected {
                return Err(MutationError::StalePrecondition {
                    expected: Box::new(expected.clone()),
                    actual: Box::new(actual),
                    changed_fields: vec!["fake_race".to_owned()],
                });
            }
            if self.fail_kind.borrow().as_ref() == Some(&kind) {
                return Err(MutationError::Provider(
                    crate::github::DiscoveryError::CommandFailed {
                        command: crate::command::CommandSpec::new("gh"),
                        code: Some(1),
                        stderr: "injected provider failure".to_owned(),
                    },
                ));
            }
            let before = current.clone();
            update(current);
            Ok(GitHubMutationReceipt {
                kind,
                before: Some(before),
                after: current.clone(),
                provider_output: None,
            })
        }
    }

    impl MembershipProvider for FakeProvider {
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

        fn repository_labels(
            &self,
            _repository: &RepositoryId,
        ) -> Result<BTreeSet<String>, MutationError> {
            Ok(self.labels.clone())
        }

        fn create_pull_request(
            &self,
            _repository: &RepositoryId,
            _input: &CreatePullRequestInput,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            panic!("fixture tests use an existing PR")
        }

        fn set_base(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            base: &str,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::SetBase, |pull_request| {
                pull_request.base.name = base.to_owned();
                pull_request.base.oid = CommitOid(format!("{base}-oid"));
            })
        }

        fn add_label(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            label: &str,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::AddLabel, |pull_request| {
                pull_request.labels.insert(label.to_owned());
            })
        }

        fn remove_label(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            label: &str,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::RemoveLabel, |pull_request| {
                pull_request.labels.remove(label);
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
            oid: CommitOid(format!("{name}-oid")),
        }
    }

    fn pull_request(number: u64, head: &str, base: &str, labels: &[&str]) -> PullRequestSnapshot {
        PullRequestSnapshot {
            number: PrNumber(number),
            title: format!("PR {number}"),
            url: format!("https://example.invalid/{number}"),
            state: PullRequestState::Open,
            draft: false,
            head: branch(head),
            base: branch(base),
            cross_repository: false,
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            auto_merge: if labels.contains(&ACTIVE_LABEL) && base == "main" {
                AutoMergeState::squash()
            } else {
                AutoMergeState::disabled()
            },
            checks: Vec::new(),
            merged_at: None,
            updated_at: None,
        }
    }

    fn status(current: PullRequestSnapshot, others: Vec<PullRequestSnapshot>) -> StatusOutput {
        let current_number = current.number;
        let mut pull_requests = others;
        if !pull_requests
            .iter()
            .any(|item| item.number == current_number)
        {
            pull_requests.push(current);
        }
        let snapshot = crate::model::RepositorySnapshot {
            repository: repository(),
            default_branch: branch("main"),
            current_branch: Some("current".to_owned()),
            current_pr: Some(current_number),
            pull_requests,
            observed_at: None,
        };
        let analysis = analyze(&snapshot, &clean).unwrap();
        StatusOutput {
            repository: repository(),
            default_branch: "main".to_owned(),
            current_branch: snapshot.current_branch,
            current_pr: snapshot.current_pr,
            healthy: analysis.healthy(),
            analysis,
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

    #[test]
    fn new_applies_active_label_and_squash_auto_merge() {
        let candidate = pull_request(1, "one", "main", &[]);
        let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
        let output = execute(
            status(candidate, Vec::new()),
            &clean,
            &provider,
            MembershipRequest {
                operation: MembershipOperation::New,
                create_pr: false,
                tail_pr: None,
                head_pr: None,
            },
        )
        .unwrap();

        assert!(output.pull_request.has_label(ACTIVE_LABEL));
        assert_eq!(output.pull_request.auto_merge, AutoMergeState::squash());
        assert_eq!(output.caravan_id, PrNumber(1));
        assert!(output.receipt.changed);
    }

    #[test]
    fn join_infers_unique_tail_and_preserves_non_head_auto_merge_off() {
        let head = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
        let candidate = pull_request(2, "two", "main", &[]);
        let provider = FakeProvider::with_pull_requests(vec![head.clone(), candidate.clone()]);
        let output = execute(
            status(candidate, vec![head]),
            &clean,
            &provider,
            MembershipRequest {
                operation: MembershipOperation::Join,
                create_pr: false,
                tail_pr: None,
                head_pr: None,
            },
        )
        .unwrap();

        assert_eq!(output.pull_request.base.name, "one");
        assert!(output.pull_request.has_label(ACTIVE_LABEL));
        assert!(!output.pull_request.auto_merge.enabled);
        assert_eq!(output.caravan_id, PrNumber(1));
    }

    #[test]
    fn unprotected_default_branch_fails_before_head_mutation() {
        let candidate = pull_request(1, "one", "main", &[]);
        let mut provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
        provider.branch_protected = false;
        let error = execute(
            status(candidate, Vec::new()),
            &clean,
            &provider,
            MembershipRequest {
                operation: MembershipOperation::New,
                create_pr: false,
                tail_pr: None,
                head_pr: None,
            },
        )
        .unwrap_err();

        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "default_branch_not_protected"
        );
        assert!(
            !provider
                .pull_requests
                .borrow()
                .get(&PrNumber(1))
                .unwrap()
                .has_label(ACTIVE_LABEL)
        );
    }

    #[test]
    fn disabled_repository_auto_merge_fails_before_head_mutation() {
        let candidate = pull_request(1, "one", "main", &[]);
        let mut provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
        provider.allows_auto_merge = false;
        let error = execute(
            status(candidate, Vec::new()),
            &clean,
            &provider,
            MembershipRequest {
                operation: MembershipOperation::New,
                create_pr: false,
                tail_pr: None,
                head_pr: None,
            },
        )
        .unwrap_err();

        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "auto_merge_not_enabled"
        );
        assert!(
            !provider
                .pull_requests
                .borrow()
                .get(&PrNumber(1))
                .unwrap()
                .has_label(ACTIVE_LABEL)
        );
    }

    #[test]
    fn missing_labels_fail_before_provider_mutation() {
        let candidate = pull_request(1, "one", "main", &[]);
        let mut provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
        provider.labels.remove(FORCE_LABEL);
        let error = execute(
            status(candidate, Vec::new()),
            &clean,
            &provider,
            MembershipRequest {
                operation: MembershipOperation::New,
                create_pr: false,
                tail_pr: None,
                head_pr: None,
            },
        )
        .unwrap_err();

        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "required_labels_missing"
        );
        assert!(
            !provider
                .pull_requests
                .borrow()
                .get(&PrNumber(1))
                .unwrap()
                .has_label(ACTIVE_LABEL)
        );
    }

    #[test]
    fn partial_failure_reports_receipts_and_rerun_resumes() {
        let candidate = pull_request(1, "one", "main", &[]);
        let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
        *provider.fail_kind.borrow_mut() = Some(MutationKind::EnableAutoMerge);
        let request = MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
        };
        let error = execute(status(candidate, Vec::new()), &clean, &provider, request).unwrap_err();
        let details = mcp_cli::StructuredError::details(&error).unwrap();
        assert!(
            details["provider_receipts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|receipt| {
                    receipt["kind"] == serde_json::Value::String("add_label".to_owned())
                })
        );

        *provider.fail_kind.borrow_mut() = None;
        let partial = provider
            .pull_requests
            .borrow()
            .get(&PrNumber(1))
            .unwrap()
            .clone();
        let output = execute(status(partial, Vec::new()), &clean, &provider, request).unwrap();
        assert_eq!(output.pull_request.auto_merge, AutoMergeState::squash());
    }

    #[test]
    fn rejoin_removes_evicted_and_force_after_full_preflight() {
        let head = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
        let candidate = pull_request(2, "two", "main", &[EVICTED_LABEL, FORCE_LABEL]);
        let provider = FakeProvider::with_pull_requests(vec![head.clone(), candidate.clone()]);
        let output = execute(
            status(candidate, vec![head]),
            &clean,
            &provider,
            MembershipRequest {
                operation: MembershipOperation::Rejoin,
                create_pr: false,
                tail_pr: Some(1),
                head_pr: None,
            },
        )
        .unwrap();

        assert!(output.pull_request.has_label(ACTIVE_LABEL));
        assert!(!output.pull_request.has_label(EVICTED_LABEL));
        assert!(!output.pull_request.has_label(FORCE_LABEL));
        assert_eq!(output.pull_request.base.name, "one");
    }
}
