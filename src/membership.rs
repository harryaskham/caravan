//! Membership policy for creating, renewing, joining, and rejoining caravans.
//!
//! The provider adapter owns exact optimistic commands. This module owns only
//! operation ordering, complete preflight, idempotent resume, and receipts.

use std::collections::{BTreeMap, BTreeSet};

use mcp_cli::{ErrorCategory, StructuredError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::command::CommandRunError;
use crate::github::{
    ControlLabelAudit, CreatePullRequestInput, DiscoveryError, GitHubMutationAdapter,
    GitHubMutationReceipt, MutationError, control_label_marker,
};
use crate::graph::{CompatibilityChecker, GitCompatibilityChecker};
use crate::hooks::{self, HookDelivery};
use crate::model::{
    Caravan, CaravanEvent, EventKind, MergeMethod, MutationKind, MutationStep, MutationStepState,
    OperationId, OperationReceipt, PrNumber, PullRequestPrecondition, PullRequestSnapshot,
    PullRequestState, RepositoryId,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipRequest {
    pub operation: MembershipOperation,
    pub create_pr: bool,
    pub tail_pr: Option<u64>,
    pub head_pr: Option<u64>,
    pub reason: Option<String>,
    pub priority_label: Option<String>,
    pub agent_priority_labels: Vec<String>,
}

/// Successful, resumable membership result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MembershipOutput {
    pub receipt: OperationReceipt,
    /// Exact old/new OIDs for the optional physical ancestry rewrite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebase_receipt: Option<crate::physical_rebase::RebaseReceipt>,
    /// Exact provider before/after facts for every completed remote mutation.
    #[serde(default)]
    pub provider_receipts: Vec<GitHubMutationReceipt>,
    pub pull_request: PullRequestSnapshot,
    pub caravan_id: PrNumber,
    /// Canonical events emitted after the complete membership operation.
    #[serde(default)]
    pub events: Vec<CaravanEvent>,
    /// Bounded status for configured hooks which consumed `events`.
    #[serde(default)]
    pub hook_deliveries: Vec<HookDelivery>,
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

    fn ensure_control_label_comment(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        audit: &ControlLabelAudit,
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

    fn ensure_control_label_comment(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        audit: &ControlLabelAudit,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.ensure_control_label_comment(repository, expected, audit)
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
        &MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: input.create_pr,
            tail_pr: None,
            head_pr: None,
            reason: input.reason.clone(),
            priority_label: input.priority_label.clone(),
            agent_priority_labels: context.config.agent_priority_labels.clone(),
        },
    )
}

/// Renew an evicted PR as a new caravan.
pub fn renew(context: &AppContext, input: &CreateInput) -> Result<MembershipOutput, AppError> {
    execute_live(
        context,
        &MembershipRequest {
            operation: MembershipOperation::Renew,
            create_pr: input.create_pr,
            tail_pr: None,
            head_pr: None,
            reason: input.reason.clone(),
            priority_label: input.priority_label.clone(),
            agent_priority_labels: context.config.agent_priority_labels.clone(),
        },
    )
}

/// Append the current PR after a selected or uniquely inferred tail.
pub fn join(context: &AppContext, input: &JoinInput) -> Result<MembershipOutput, AppError> {
    execute_live(
        context,
        &MembershipRequest {
            operation: MembershipOperation::Join,
            create_pr: input.create_pr,
            tail_pr: input.tail_pr,
            head_pr: input.head_pr,
            reason: input.reason.clone(),
            priority_label: input.priority_label.clone(),
            agent_priority_labels: context.config.agent_priority_labels.clone(),
        },
    )
}

/// Rejoin an evicted PR after a selected or uniquely inferred tail.
pub fn rejoin(context: &AppContext, input: &JoinInput) -> Result<MembershipOutput, AppError> {
    execute_live(
        context,
        &MembershipRequest {
            operation: MembershipOperation::Rejoin,
            create_pr: input.create_pr,
            tail_pr: input.tail_pr,
            head_pr: input.head_pr,
            reason: input.reason.clone(),
            priority_label: input.priority_label.clone(),
            agent_priority_labels: context.config.agent_priority_labels.clone(),
        },
    )
}

#[allow(clippy::too_many_lines)]
fn execute_live(
    context: &AppContext,
    request: &MembershipRequest,
) -> Result<MembershipOutput, AppError> {
    let _lock = OperationLock::acquire(&context.repository_path, request.operation.name())?;
    let timeout = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let mut status = read::status(context)?;
    let checker =
        GitCompatibilityChecker::new(&context.repository_path, "origin").with_timeout(timeout);
    let provider = GitHubMutationAdapter::new(
        crate::command::ProcessRunner::in_directory(&context.repository_path).with_timeout(timeout),
    );
    let repository = status.repository.clone();
    let failure_status = status.clone();
    let rebase_receipt = if context.config.rebase_on_join {
        if request.create_pr && status.current_pr.is_none() {
            return Err(AppError::validation(
                "rebase_requires_existing_pr",
                "rebase_on_join requires an existing discovered PR so no provider write precedes the complete rewrite preflight",
            ));
        }
        let number = status.current_pr.ok_or_else(|| {
            AppError::validation(
                "current_pr_not_found",
                "rebase_on_join requires the current branch to have an open PR",
            )
        })?;
        let candidate = status
            .analysis
            .pull_requests
            .get(&number)
            .cloned()
            .ok_or_else(|| {
                AppError::validation(
                    "current_pr_missing_from_snapshot",
                    "current PR was not included in discovery",
                )
            })?;
        if request
            .reason
            .as_deref()
            .is_some_and(|reason| reason.trim().is_empty())
            || (request.tail_pr.is_some() && request.head_pr.is_some())
        {
            return Err(AppError::validation(
                "rebase_membership_input_invalid",
                "membership input must pass complete validation before a branch rewrite",
            ));
        }
        if request.priority_label.as_deref().is_some_and(|label| {
            !request
                .agent_priority_labels
                .iter()
                .any(|configured| configured == label.trim())
        }) {
            return Err(AppError::validation(
                "priority_label_not_configured",
                "priority label is not an exact configured agent_priority_labels entry",
            ));
        }
        let join_target = request
            .operation
            .is_join()
            .then(|| resolve_join_target(&status, request))
            .transpose()?;
        let desired_base = join_target.as_ref().map_or_else(
            || status.default_branch.clone(),
            |target| target.tail.head.name.clone(),
        );
        let preflight_state = ExecutionState::new(request.operation);
        crate::initialization::require_ready(&status.initialization)?;
        preflight_repository(
            &provider,
            &status.repository,
            &status.default_branch,
            request.operation,
            &request.agent_priority_labels,
            &preflight_state,
        )?;
        validate_operation_shape(&candidate, request, &desired_base)?;
        preflight_eligibility(&status, &candidate, request, join_target.as_ref(), &checker)?;
        let target = join_target.map_or_else(
            || status.analysis.fleet.default_branch.clone(),
            |target| target.tail.head,
        );
        let receipt = crate::physical_rebase::rewrite_candidate(
            &context.repository_path,
            &repository,
            &candidate,
            &target,
            &status.analysis.fleet.default_branch,
            timeout,
        )?;
        // GitHub is authoritative after a push. Never apply base/label changes
        // against the stale pre-rewrite PR snapshot.
        status = read::status(context)?;
        let observed = status.analysis.pull_requests.get(&number).ok_or_else(|| {
            AppError::structured(
                ErrorCategory::ExecutionFailure,
                "rebase_rediscovery_failed",
                "rewritten PR was absent from post-push discovery",
                Some(json!({"rebase_receipt": receipt, "resumable": true})),
            )
        })?;
        if observed.head.oid != receipt.new_head_oid {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "rebase_rediscovery_stale",
                "provider did not report the exact pushed head after rewrite",
                Some(
                    json!({"rebase_receipt": receipt, "observed_oid": observed.head.oid, "resumable": true}),
                ),
            ));
        }
        Some(receipt)
    } else {
        None
    };
    let execution = execute(status, &checker, &provider, request.clone())
        .map_err(|error| attach_rebase_receipt(error, rebase_receipt.as_ref()));
    let mut output = match execution {
        Ok(output) => output,
        Err(error) if request.operation.is_join() => {
            let event = join_failed_event(&failure_status, request, &error);
            let error = hooks::attach_events(error, std::slice::from_ref(&event));
            let deliveries = hooks::dispatch_events(context, std::slice::from_ref(&event))?;
            return Err(hooks::attach_deliveries(error, &deliveries));
        }
        Err(error) => return Err(error),
    };
    let kind = if request.operation.is_join() {
        EventKind::PrJoined
    } else {
        EventKind::CaravanCreated
    };
    let event = hooks::event(
        kind,
        output.receipt.operation_id.clone(),
        repository,
        Some(output.caravan_id),
        vec![output.pull_request.number],
        None,
        None,
        BTreeMap::from([("receipt".to_owned(), json!(output.receipt))]),
    );
    output.rebase_receipt = rebase_receipt;
    output.events.push(event);
    output.hook_deliveries = hooks::dispatch_events(context, &output.events)?;
    Ok(output)
}

fn attach_rebase_receipt(
    error: AppError,
    receipt: Option<&crate::physical_rebase::RebaseReceipt>,
) -> AppError {
    let Some(receipt) = receipt else {
        return error;
    };
    let mut details = error.details().unwrap_or_else(|| json!({}));
    if let Some(object) = details.as_object_mut() {
        object.insert("rebase_receipt".to_owned(), json!(receipt));
        object.insert("resumable".to_owned(), json!(true));
        object.insert(
            "next".to_owned(),
            json!("rediscover provider state and rerun the same idempotent membership command"),
        );
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

fn join_failed_event(
    status: &StatusOutput,
    request: &MembershipRequest,
    error: &AppError,
) -> CaravanEvent {
    let mut prs = BTreeSet::new();
    prs.extend(status.current_pr);
    prs.extend(request.tail_pr.map(PrNumber));
    prs.extend(request.head_pr.map(PrNumber));
    let caravan_id = request.head_pr.map(PrNumber).or_else(|| {
        request
            .tail_pr
            .map(PrNumber)
            .and_then(|tail| status.analysis.fleet.containing(tail))
            .map(|caravan| caravan.id)
    });
    hooks::event(
        EventKind::JoinFailed,
        hooks::operation_id_from_error(error),
        status.repository.clone(),
        caravan_id,
        prs.into_iter().collect(),
        Some(status.analysis.fleet.clone()),
        Some(error.to_string()),
        BTreeMap::from([("error_code".to_owned(), json!(error.code()))]),
    )
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
fn execute(
    mut status: StatusOutput,
    checker: &impl CompatibilityChecker,
    provider: &impl MembershipProvider,
    request: MembershipRequest,
) -> Result<MembershipOutput, AppError> {
    if request
        .reason
        .as_deref()
        .is_some_and(|reason| reason.trim().is_empty())
    {
        return Err(AppError::validation(
            "membership_reason_empty",
            "--reason must contain a non-empty rationale when supplied",
        ));
    }
    if request.tail_pr.is_some() && request.head_pr.is_some() {
        return Err(AppError::validation(
            "ambiguous_target",
            "--tail-pr and --head-pr are mutually exclusive",
        ));
    }

    crate::initialization::require_ready(&status.initialization)?;
    let mut state = ExecutionState::new(request.operation);
    preflight_repository(
        provider,
        &status.repository,
        &status.default_branch,
        request.operation,
        &request.agent_priority_labels,
        &state,
    )?;
    let desired_priority_label = request.priority_label.as_deref().map(str::trim);
    if let Some(label) = desired_priority_label {
        if label.is_empty()
            || !request
                .agent_priority_labels
                .iter()
                .any(|item| item == label)
        {
            return Err(AppError::validation(
                "priority_label_not_configured",
                format!(
                    "priority label `{label}` is not an exact configured agent_priority_labels entry"
                ),
            ));
        }
    }

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
    let eligibility =
        preflight_eligibility(&status, &candidate, &request, target.as_ref(), checker)?;
    let before_labels = candidate.labels.clone();
    let admission_priority_basis = desired_priority_label.map_or_else(
        || {
            status
                .admission
                .candidates
                .iter()
                .find(|item| item.pr == current_number)
                .map_or_else(
                    || "FIFO (oldest eligible PR first); no explicit agent priority".to_owned(),
                    |item| item.reason.clone(),
                )
        },
        |label| {
            let rank = request
                .agent_priority_labels
                .iter()
                .position(|item| item == label)
                .expect("validated label")
                + 1;
            format!("explicit configured agent priority label `{label}` (rank {rank}, 1 highest)")
        },
    );
    state.current = Some(candidate);

    state.ensure_base(provider, &status.repository, &desired_base)?;
    state.ensure_label_absent(provider, &status.repository, FORCE_LABEL)?;
    for label in &request.agent_priority_labels {
        if Some(label.as_str()) != desired_priority_label {
            state.ensure_label_absent(provider, &status.repository, label)?;
        }
    }
    if let Some(label) = desired_priority_label {
        state.ensure_label_present(provider, &status.repository, label)?;
    }
    if request.operation.is_renewal() {
        state.ensure_label_absent(provider, &status.repository, EVICTED_LABEL)?;
    }
    state.ensure_label_present(provider, &status.repository, ACTIVE_LABEL)?;
    if request.operation.is_join() {
        state.ensure_auto_merge_disabled(provider, &status.repository)?;
    } else {
        state.ensure_squash_auto_merge(provider, &status.repository)?;
    }
    let audit = membership_audit(
        &request,
        &before_labels,
        &eligibility,
        state.current.as_ref().expect("current PR"),
        admission_priority_basis,
    );
    state.ensure_control_label_comment(provider, &status.repository, &audit)?;

    let receipt = state.operation_receipt();
    let pull_request = state
        .current
        .expect("membership operation has a current PR");
    let caravan_id = target
        .as_ref()
        .map_or(pull_request.number, |target| target.caravan.id);
    Ok(MembershipOutput {
        receipt,
        rebase_receipt: None,
        provider_receipts: state.provider_receipts,
        pull_request,
        caravan_id,
        events: Vec::new(),
        hook_deliveries: Vec::new(),
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
            candidate: candidate.clone(),
            enrolled: true,
            canonical_candidate: status.admission.next_candidate == Some(candidate.number),
            next_action: if request.operation.is_join() {
                read::CandidateNextAction::Join
            } else {
                read::CandidateNextAction::New
            },
            caravan_id: target
                .map(|target| target.caravan.id)
                .or(Some(candidate.number)),
            target_pr: target.and_then(|target| target.caravan.tail()),
            eligible: true,
            compatibility: status.analysis.compatibility.clone(),
            problems: Vec::new(),
            initialization: status.initialization.clone(),
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
        pr: None,
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
    priority_labels: &[String],
    state: &ExecutionState,
) -> Result<(), AppError> {
    let labels = provider
        .repository_labels(repository)
        .map_err(|error| mutation_error(&error, state))?;
    require_labels(repository, &labels)?;
    let missing_priorities: Vec<_> = priority_labels
        .iter()
        .filter(|label| !labels.contains(*label))
        .cloned()
        .collect();
    if !missing_priorities.is_empty() {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "required_priority_labels_missing",
            "configured priority labels must exist before mutation",
            Some(json!({ "repository": repository, "missing_labels": missing_priorities })),
        ));
    }
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

    fn ensure_control_label_comment(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &RepositoryId,
        audit: &ControlLabelAudit,
    ) -> Result<(), AppError> {
        let receipt = provider
            .ensure_control_label_comment(repository, &self.precondition(), audit)
            .map_err(|error| comment_error(&error, self))?;
        let already = receipt
            .provider_output
            .as_deref()
            .is_some_and(|output| output.starts_with("existing GitHub comment"));
        if already {
            self.already(
                MutationKind::Comment,
                "control-label audit comment already present",
            );
            self.current = Some(receipt.after);
        } else {
            self.record(receipt, "posted durable control-label audit comment");
        }
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

fn membership_audit(
    request: &MembershipRequest,
    before_labels: &BTreeSet<String>,
    eligibility: &CheckOutput,
    after: &PullRequestSnapshot,
    admission_priority_basis: String,
) -> ControlLabelAudit {
    let (reason, source) = request.reason.as_ref().map_or_else(
        || {
            let generated = if request.operation.is_join() {
                if request.tail_pr.is_some() || request.head_pr.is_some() {
                    "admitted after the explicitly selected caravan target"
                } else {
                    "admitted to the only mechanically inferred caravan tail"
                }
            } else if request.operation.is_renewal() {
                "evicted PR passed renewed queue eligibility"
            } else {
                "eligible PR admitted as a new caravan"
            };
            (
                generated.to_owned(),
                "deterministic Caravan policy".to_owned(),
            )
        },
        |reason| {
            (
                reason.trim().to_owned(),
                "explicit --reason input".to_owned(),
            )
        },
    );
    let compatibility = if eligibility.compatibility.is_empty() {
        "no new chain edge; repository and graph preflight passed".to_owned()
    } else {
        eligibility
            .compatibility
            .iter()
            .map(|report| {
                format!(
                    "{}@{} -> {}@{} = {:?}",
                    report.candidate.name,
                    report.candidate.oid.0,
                    report.target.name,
                    report.target.oid.0,
                    report.outcome
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    ControlLabelAudit {
        operation: request.operation.name().to_owned(),
        marker: control_label_marker(
            request.operation.name(),
            after.number,
            &after.head.oid,
            before_labels,
            &after.labels,
        ),
        before_labels: before_labels.clone(),
        after_labels: after.labels.clone(),
        actor: "authenticated GitHub actor invoked through cara CLI/JSON/MCP".to_owned(),
        reason,
        reason_source: source,
        compatibility_evidence: compatibility,
        clean_squash_evidence: if request.operation.is_join() {
            "compatibility check was clean; non-head auto-merge is disabled".to_owned()
        } else {
            "compatibility check was clean; squash auto-merge is enabled on the head".to_owned()
        },
        admission_priority_basis,
    }
}

fn comment_error(error: &MutationError, state: &ExecutionState) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "github_comment_failed",
        format!("control labels changed but their durable GitHub comment failed: {error}"),
        Some(json!({
            "stage": "control_label_comment",
            "operation_id": state.operation_id,
            "completed_steps": state.steps,
            "provider_receipts": state.provider_receipts,
            "resumable": true,
            "dedupe": "deterministic GitHub-visible caravan-control-label-audit marker",
            "next": format!("rediscover and rerun `cara {}`", state.operation.name()),
        })),
    )
}

fn mutation_error(error: &MutationError, state: &ExecutionState) -> AppError {
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
                "operation_id": state.operation_id,
                "completed_steps": state.steps,
                "provider_receipts": state.provider_receipts,
                "resumable": true,
                "next": format!("rediscover and rerun `cara {}`", state.operation.name()),
            })),
        );
    }
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

        fn ensure_control_label_comment(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            _audit: &ControlLabelAudit,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::Comment, |_| {})
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
            created_at: Some(format!("2026-01-01T00:00:{number:02}Z")),
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
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            repository: repository(),
            default_branch: branch("main"),
            current_branch: Some("current".to_owned()),
            current_pr: Some(current_number),
            pull_requests,
            observed_at: None,
        };
        let analysis = analyze(&snapshot, &clean).unwrap();
        StatusOutput {
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            timing: None,
            repository: repository(),
            default_branch: "main".to_owned(),
            current_branch: snapshot.current_branch,
            current_pr: snapshot.current_pr,
            healthy: analysis.healthy(),
            initialization: crate::initialization::InitializationStatus::default(),
            admission: crate::read::resolve_admission(
                &analysis,
                &crate::config::CaravanConfig::default().agent_priority_labels,
            ),
            analysis,
            pauses: Vec::new(),
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
    fn join_failure_event_carries_target_fleet_and_error_code() {
        let head = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
        let candidate = pull_request(2, "two", "main", &[]);
        let status = status(candidate, vec![head]);
        let request = MembershipRequest {
            operation: MembershipOperation::Join,
            create_pr: false,
            tail_pr: Some(1),
            head_pr: None,
            reason: None,
            priority_label: None,
            agent_priority_labels: Vec::new(),
        };
        let error = AppError::validation("candidate_rejected", "cannot join");

        let event = join_failed_event(&status, &request, &error);

        assert_eq!(event.kind, EventKind::JoinFailed);
        assert_eq!(event.caravan_id, Some(PrNumber(1)));
        assert_eq!(event.prs, vec![PrNumber(1), PrNumber(2)]);
        assert_eq!(event.fleet, Some(status.analysis.fleet));
        assert_eq!(event.metadata["error_code"], "candidate_rejected");
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
                reason: None,
                priority_label: None,
                agent_priority_labels: Vec::new(),
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
                reason: None,
                priority_label: None,
                agent_priority_labels: Vec::new(),
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
                reason: None,
                priority_label: None,
                agent_priority_labels: Vec::new(),
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
                reason: None,
                priority_label: None,
                agent_priority_labels: Vec::new(),
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
                reason: None,
                priority_label: None,
                agent_priority_labels: Vec::new(),
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
            reason: None,
            priority_label: None,
            agent_priority_labels: Vec::new(),
        };
        let error = execute(
            status(candidate, Vec::new()),
            &clean,
            &provider,
            request.clone(),
        )
        .unwrap_err();
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
    fn explicit_membership_reason_must_not_be_whitespace() {
        let candidate = pull_request(1, "one", "main", &[]);
        let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
        let error = execute(
            status(candidate, Vec::new()),
            &clean,
            &provider,
            MembershipRequest {
                operation: MembershipOperation::New,
                create_pr: false,
                tail_pr: None,
                head_pr: None,
                reason: Some("  \n".to_owned()),
                priority_label: None,
                agent_priority_labels: Vec::new(),
            },
        )
        .unwrap_err();

        assert_eq!(error.code(), "membership_reason_empty");
        assert!(!provider.pull_requests.borrow()[&PrNumber(1)].has_label(ACTIVE_LABEL));
    }

    #[test]
    fn comment_failure_is_a_resumable_partial_label_mutation() {
        let candidate = pull_request(1, "one", "main", &[]);
        let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
        *provider.fail_kind.borrow_mut() = Some(MutationKind::Comment);
        let error = execute(
            status(candidate, Vec::new()),
            &clean,
            &provider,
            MembershipRequest {
                operation: MembershipOperation::New,
                create_pr: false,
                tail_pr: None,
                head_pr: None,
                reason: Some("operator admission".to_owned()),
                priority_label: None,
                agent_priority_labels: Vec::new(),
            },
        )
        .unwrap_err();

        assert_eq!(error.code(), "github_comment_failed");
        let details = error.details().unwrap();
        assert_eq!(details["stage"], "control_label_comment");
        assert_eq!(details["resumable"], true);
        assert!(
            details["completed_steps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|step| step["kind"] == "add_label")
        );
    }

    #[test]
    fn explicit_priority_applies_configured_control_label() {
        let candidate = pull_request(1, "one", "main", &[]);
        let mut provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
        provider.labels.insert("caravan-priority:high".to_owned());
        let output = execute(
            status(candidate, Vec::new()),
            &clean,
            &provider,
            MembershipRequest {
                operation: MembershipOperation::New,
                create_pr: false,
                tail_pr: None,
                head_pr: None,
                reason: None,
                priority_label: Some("caravan-priority:high".to_owned()),
                agent_priority_labels: vec!["caravan-priority:high".to_owned()],
            },
        )
        .unwrap();

        assert!(output.pull_request.has_label("caravan-priority:high"));
        assert!(
            output
                .receipt
                .completed_steps
                .iter()
                .any(|step| step.kind == MutationKind::Comment)
        );
    }

    #[test]
    fn mutation_timeout_preserves_timeout_category_and_partial_receipts() {
        let mut state = ExecutionState::new(MembershipOperation::Join);
        state.steps.push(MutationStep {
            kind: MutationKind::AddLabel,
            state: MutationStepState::Completed,
            pr: Some(PrNumber(2)),
            summary: "label added".to_owned(),
        });
        let error = mutation_error(
            &MutationError::Provider(DiscoveryError::Runner(CommandRunError::Timeout {
                command: crate::command::CommandSpec::new("gh").args(["pr", "edit"]),
                timeout_ms: 900,
                stdout: "partial".to_owned(),
                stderr: "stalled".to_owned(),
            })),
            &state,
        );

        assert_eq!(
            mcp_cli::StructuredError::category(&error),
            ErrorCategory::Timeout
        );
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "github_mutation_timeout"
        );
        let details = mcp_cli::StructuredError::details(&error).unwrap();
        assert_eq!(details["stage"], "github_mutation");
        assert_eq!(details["timeout_ms"], 900);
        assert_eq!(details["completed_steps"][0]["summary"], "label added");
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
                reason: None,
                priority_label: None,
                agent_priority_labels: Vec::new(),
            },
        )
        .unwrap();

        assert!(output.pull_request.has_label(ACTIVE_LABEL));
        assert!(!output.pull_request.has_label(EVICTED_LABEL));
        assert!(!output.pull_request.has_label(FORCE_LABEL));
        assert_eq!(output.pull_request.base.name, "one");
    }
}
