//! Bounded asynchronous merge transactions for GitHub native Stacks.
//!
//! The provider accepts a lease only for the selected (highest) pull request.
//! Cara therefore seals every lower generation in the plan, re-reads the whole
//! Stack immediately before submission, persists the returned UUID, and proves
//! terminal state from fresh pull-request and Stack truth. A response timeout
//! without a UUID is indeterminate and is never permission to submit again.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{
    GitHubMutationAdapter, GitHubStackEntryGeneration, GitHubStackGeneration,
    GitHubStackMutationError, MutationError,
};
use crate::command::{CommandOutput, CommandRunner, CommandSpec};
use crate::model::{
    BranchSnapshot, CommitOid, PrNumber, PullRequestSnapshot, PullRequestState, RepositoryId,
};

const STACK_ACCEPT: &str = "Accept: application/vnd.github+json";
const STACK_API_VERSION: &str = "X-GitHub-Api-Version: 2026-03-10";
const STACK_MERGE_SCHEMA_VERSION: u32 = 1;

/// Policy evidence for one exact Stack entry. The generation is copied rather
/// than named only by PR number so stale CI/graph evidence cannot be reused
/// after a provider rewrite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackMergeEntryEvidence {
    pub generation: GitHubStackEntryGeneration,
    /// Scheduler-owned policy refusals for this exact generation. Provider
    /// open/draft/Stack state is derived again by the planner.
    #[serde(default)]
    pub blockers: Vec<GitHubStackMergeBlocker>,
}

/// Stable reasons why an entry cannot extend the selected ready prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitHubStackMergeBlocker {
    StackClosed,
    PullRequestNotOpen,
    Draft,
    StackEntryNotOpen,
    GraphInexact,
    Held,
    MechanicallyBlocked,
    RequiredChecksNotReady,
    ForceUnsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackBlockedEntry {
    pub pr: PrNumber,
    pub position: u32,
    pub blockers: Vec<GitHubStackMergeBlocker>,
}

/// Maximal contiguous ready prefix, bottom-to-top. An empty selection is an
/// ordinary wait result, not permission to invoke a legacy merge endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackReadyPrefix {
    pub stack: GitHubStackGeneration,
    pub selected: Vec<GitHubStackEntryGeneration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_blocked: Option<GitHubStackBlockedEntry>,
}

impl GitHubStackReadyPrefix {
    /// Convert a non-empty selection into the only initially accepted native
    /// merge mode: a direct atomic squash. Merge-queue semantics are a separate
    /// rollout because `enqueued` is not landing proof.
    pub fn direct_squash_plan(
        &self,
        operation_id: impl Into<String>,
        actor: impl Into<String>,
    ) -> Result<GitHubStackAsyncMergePlan, GitHubStackMergeError> {
        if self.selected.is_empty() {
            return Err(invalid_plan(
                "github_stack_no_ready_prefix",
                "native Stack merge requires at least one ready bottom entry",
            ));
        }
        Ok(GitHubStackAsyncMergePlan {
            operation_id: operation_id.into(),
            actor: actor.into(),
            before: self.stack.clone(),
            selected: self.selected.clone(),
            merge_method: GitHubStackMergeMethod::Squash,
            merge_action: GitHubStackMergeAction::DirectMerge,
        })
    }
}

/// Compute the maximal ready prefix without provider access. Evidence must
/// cover the complete exact Stack generation in order; partial/misaligned
/// evidence is a typed refusal rather than a shortened merge group.
pub fn plan_github_stack_ready_prefix(
    stack: &GitHubStackGeneration,
    evidence: &[GitHubStackMergeEntryEvidence],
) -> Result<GitHubStackReadyPrefix, GitHubStackMergeError> {
    if evidence.len() != stack.topology.entries.len() {
        return Err(invalid_plan(
            "github_stack_merge_evidence_incomplete",
            "merge readiness evidence must cover every Stack entry",
        ));
    }
    for (index, (expected, actual)) in stack.topology.entries.iter().zip(evidence).enumerate() {
        if expected != &actual.generation {
            return Err(invalid_plan(
                "github_stack_merge_evidence_stale",
                &format!(
                    "merge readiness evidence at position {index} is not the exact Stack generation"
                ),
            ));
        }
    }

    let mut selected = Vec::new();
    let mut first_blocked = None;
    for item in evidence {
        let entry = &item.generation;
        let mut blockers = Vec::new();
        if !stack.open {
            blockers.push(GitHubStackMergeBlocker::StackClosed);
        }
        if entry.pull_request_state != PullRequestState::Open {
            blockers.push(GitHubStackMergeBlocker::PullRequestNotOpen);
        }
        if entry.draft {
            blockers.push(GitHubStackMergeBlocker::Draft);
        }
        if !entry.stack_state.eq_ignore_ascii_case("open") {
            blockers.push(GitHubStackMergeBlocker::StackEntryNotOpen);
        }
        blockers.extend(item.blockers.iter().copied());
        if blockers.is_empty() {
            selected.push(entry.clone());
        } else {
            first_blocked = Some(GitHubStackBlockedEntry {
                pr: entry.pr,
                position: entry.position,
                blockers,
            });
            break;
        }
    }

    Ok(GitHubStackReadyPrefix {
        stack: stack.clone(),
        selected,
        first_blocked,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitHubStackMergeMethod {
    Squash,
}

impl GitHubStackMergeMethod {
    const fn provider_value(self) -> &'static str {
        match self {
            Self::Squash => "squash",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitHubStackMergeAction {
    DirectMerge,
}

impl GitHubStackMergeAction {
    const fn provider_value(self) -> &'static str {
        match self {
            Self::DirectMerge => "direct_merge",
        }
    }
}

/// Exact one-shot submission intent. `selected` must be a non-empty prefix of
/// `before`; its last entry supplies the only SHA lease accepted by GitHub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackAsyncMergePlan {
    pub operation_id: String,
    pub actor: String,
    pub before: GitHubStackGeneration,
    pub selected: Vec<GitHubStackEntryGeneration>,
    pub merge_method: GitHubStackMergeMethod,
    pub merge_action: GitHubStackMergeAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackMergeRequestIdentity {
    pub method: String,
    pub path: String,
    pub selected_top: PrNumber,
    pub selected_top_sha: CommitOid,
    pub ordered_pull_requests: Vec<PrNumber>,
    pub ordered_heads: Vec<CommitOid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_request_id: Option<String>,
}

/// Durable polling checkpoint. It is self-contained so a new process can poll
/// without rediscovering or reconstructing the original selected generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackMergeCheckpoint {
    pub schema_version: u32,
    pub repository: RepositoryId,
    pub plan: GitHubStackAsyncMergePlan,
    pub request: GitHubStackMergeRequestIdentity,
    pub uuid: String,
    pub initial_provider_status: String,
    pub evidence_hash: String,
}

impl GitHubStackMergeCheckpoint {
    fn seal(mut self) -> Self {
        self.evidence_hash.clear();
        let material = serde_json::to_vec(&self).expect("GitHub Stack merge checkpoint serializes");
        self.evidence_hash = crate::membership::fnv1a64(&material);
        self
    }

    #[must_use]
    pub fn verify(&self) -> bool {
        let expected = self.evidence_hash.clone();
        let mut material = self.clone();
        material.evidence_hash.clear();
        serde_json::to_vec(&material)
            .ok()
            .is_some_and(|bytes| crate::membership::fnv1a64(&bytes) == expected)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitHubStackMergeStatus {
    Submitted,
    Pending,
    Enqueued,
    Merged,
    Failed,
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackMergePullRequestObservation {
    pub pr: PrNumber,
    pub state: PullRequestState,
    pub draft: bool,
    pub base: BranchSnapshot,
    pub head: BranchSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<String>,
}

impl From<PullRequestSnapshot> for GitHubStackMergePullRequestObservation {
    fn from(pr: PullRequestSnapshot) -> Self {
        Self {
            pr: pr.number,
            state: pr.state,
            draft: pr.draft,
            base: pr.base,
            head: pr.head,
            merged_at: pr.merged_at,
        }
    }
}

/// Fresh post-submit truth used to prove all, none, or an impossible partial
/// result. `remaining_order_exact` compares the unmerged provider Stack order
/// with the original suffix; every resulting base/head generation is retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackMergeObservation {
    pub default_branch: BranchSnapshot,
    pub selected: Vec<GitHubStackMergePullRequestObservation>,
    pub remaining: Vec<GitHubStackMergePullRequestObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_after: Option<GitHubStackGeneration>,
    pub selected_merged: usize,
    /// Every selected PR still names the exact source head sealed before PUT.
    /// GitHub accepts a lease only for the selected top, so this can become
    /// false even when the provider reports every PR as merged.
    pub selected_heads_exact: bool,
    #[serde(default)]
    pub changed_selected_heads: Vec<PrNumber>,
    pub remaining_order_exact: bool,
}

/// Sealed result of submission or one bounded poll. `Indeterminate` is sticky
/// safety evidence, not a retry instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackMergeReceipt {
    pub schema_version: u32,
    pub repository: RepositoryId,
    pub plan: GitHubStackAsyncMergePlan,
    pub request: GitHubStackMergeRequestIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<GitHubStackMergeCheckpoint>,
    pub status: GitHubStackMergeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_sha: Option<CommitOid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<GitHubStackMergeObservation>,
    pub evidence_hash: String,
}

impl GitHubStackMergeReceipt {
    fn seal(mut self) -> Self {
        self.evidence_hash.clear();
        let material = serde_json::to_vec(&self).expect("GitHub Stack merge receipt serializes");
        self.evidence_hash = crate::membership::fnv1a64(&material);
        self
    }

    #[must_use]
    pub fn verify(&self) -> bool {
        let expected = self.evidence_hash.clone();
        let mut material = self.clone();
        material.evidence_hash.clear();
        serde_json::to_vec(&material)
            .ok()
            .is_some_and(|bytes| crate::membership::fnv1a64(&bytes) == expected)
    }
}

#[derive(Debug)]
pub enum GitHubStackMergeError {
    InvalidPlan {
        code: String,
        message: String,
    },
    StaleGeneration {
        expected: Box<GitHubStackGeneration>,
        actual: Option<Box<GitHubStackGeneration>>,
    },
    InvalidCheckpoint {
        diagnostic: String,
    },
    Stack(GitHubStackMutationError),
    Provider(MutationError),
}

impl std::fmt::Display for GitHubStackMergeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPlan { code, message } => write!(formatter, "{code}: {message}"),
            Self::StaleGeneration { .. } => {
                write!(
                    formatter,
                    "GitHub Stack changed before async merge submission"
                )
            }
            Self::InvalidCheckpoint { diagnostic } => {
                write!(
                    formatter,
                    "invalid GitHub Stack merge checkpoint: {diagnostic}"
                )
            }
            Self::Stack(error) => error.fmt(formatter),
            Self::Provider(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GitHubStackMergeError {}

impl From<GitHubStackMutationError> for GitHubStackMergeError {
    fn from(error: GitHubStackMutationError) -> Self {
        Self::Stack(error)
    }
}

impl From<MutationError> for GitHubStackMergeError {
    fn from(error: MutationError) -> Self {
        Self::Provider(error)
    }
}

#[derive(Debug, Default)]
struct ProviderAsyncMergeResult {
    uuid: Option<String>,
    status: Option<String>,
    message: Option<String>,
    sha: Option<CommitOid>,
    expected_head_sha: Option<CommitOid>,
    merge_method: Option<String>,
    merge_action: Option<String>,
}

impl<R: CommandRunner> GitHubMutationAdapter<R> {
    /// Submit exactly one direct atomic Stack merge. Callers must persist the
    /// returned checkpoint before polling. If the response loses its UUID this
    /// returns an indeterminate sealed receipt and deliberately does not retry.
    pub fn native_stack_merge_submit(
        &self,
        repository: &RepositoryId,
        plan: &GitHubStackAsyncMergePlan,
    ) -> Result<GitHubStackMergeReceipt, GitHubStackMergeError> {
        validate_merge_plan(repository, plan)?;

        // This is the final complete-group lease read. GitHub accepts only the
        // top SHA, so every lower head must still equal the sealed generation at
        // the instant immediately preceding PUT.
        let actual = self.native_stack_generation(repository, plan.before.number)?;
        if actual.as_ref() != Some(&plan.before) {
            return Err(GitHubStackMergeError::StaleGeneration {
                expected: Box::new(plan.before.clone()),
                actual: actual.map(Box::new),
            });
        }

        let mut request = merge_request_identity(repository, plan);
        let command = native_stack_merge_submit_command(repository, plan);
        let response = self.runner.run(&command);
        if let Ok(output) = &response {
            request.github_request_id = github_request_id(output);
        }
        let parsed = response
            .as_ref()
            .ok()
            .and_then(|output| parse_async_merge_response(output).ok())
            .unwrap_or_default();
        let provider_status = parsed.status.clone();
        let provider_message = parsed.message.clone().or_else(|| {
            response
                .as_ref()
                .err()
                .map(ToString::to_string)
                .or_else(|| response.as_ref().ok().and_then(provider_diagnostic))
        });

        let checkpoint = stack_merge_checkpoint(repository, plan, &request, &parsed);

        let successful_response = response.as_ref().is_ok_and(CommandOutput::is_success);
        match normalized_provider_status(parsed.status.as_deref()) {
            Some(GitHubStackMergeStatus::Merged | GitHubStackMergeStatus::Failed) => Ok(self
                .terminal_stack_merge_receipt(StackMergeReceiptInput {
                    repository,
                    plan,
                    request,
                    checkpoint,
                    status: GitHubStackMergeStatus::Indeterminate,
                    provider_status,
                    provider_message,
                    provider_sha: parsed.sha,
                    observation: None,
                })),
            Some(GitHubStackMergeStatus::Enqueued) if checkpoint.is_some() => {
                Ok(stack_merge_receipt(StackMergeReceiptInput {
                    repository,
                    plan,
                    request,
                    checkpoint,
                    status: GitHubStackMergeStatus::Enqueued,
                    provider_status,
                    provider_message,
                    provider_sha: parsed.sha,
                    observation: None,
                }))
            }
            Some(GitHubStackMergeStatus::Pending)
                if checkpoint.is_some()
                    && (successful_response || provider_request_matches_plan(&parsed, plan)) =>
            {
                Ok(stack_merge_receipt(StackMergeReceiptInput {
                    repository,
                    plan,
                    request,
                    checkpoint,
                    status: GitHubStackMergeStatus::Submitted,
                    provider_status,
                    provider_message,
                    provider_sha: parsed.sha,
                    observation: None,
                }))
            }
            _ => {
                // A successful merge may race ahead of a lost/invalid response.
                // Fresh truth can prove all selected entries, but none/partial
                // never permits another submission without the original UUID.
                let (observation, rediscovery_error) =
                    match self.observe_stack_merge(repository, plan) {
                        Ok(observation) => (Some(observation), None),
                        Err(error) => (None, Some(error.to_string())),
                    };
                let status = observation
                    .as_ref()
                    .map(|observed| classify_observation(plan, observed))
                    .filter(|status| *status == GitHubStackMergeStatus::Merged)
                    .unwrap_or(GitHubStackMergeStatus::Indeterminate);
                let provider_message =
                    append_rediscovery_error(provider_message, rediscovery_error);
                Ok(stack_merge_receipt(StackMergeReceiptInput {
                    repository,
                    plan,
                    request,
                    checkpoint,
                    status,
                    provider_status,
                    provider_message,
                    provider_sha: parsed.sha,
                    observation,
                }))
            }
        }
    }

    /// Poll one persisted UUID once. Tick-level cadence and deadline remain
    /// scheduler policy; this primitive never sleeps or loops invisibly.
    pub fn native_stack_merge_poll(
        &self,
        repository: &RepositoryId,
        checkpoint: &GitHubStackMergeCheckpoint,
    ) -> Result<GitHubStackMergeReceipt, GitHubStackMergeError> {
        validate_checkpoint(repository, checkpoint)?;
        let mut request = checkpoint.request.clone();
        "GET".clone_into(&mut request.method);
        request.path = format!(
            "repos/{}/pulls/{}/merge-async/{}",
            repository.slug(),
            request.selected_top,
            checkpoint.uuid
        );
        request.github_request_id = None;

        let command = native_stack_merge_poll_command(repository, checkpoint);
        let response = self.runner.run(&command);
        if let Ok(output) = &response {
            request.github_request_id = github_request_id(output);
        }
        let parsed = response
            .as_ref()
            .ok()
            .and_then(|output| parse_async_merge_response(output).ok())
            .unwrap_or_default();
        let provider_status = parsed.status.clone();
        let provider_message = parsed.message.clone().or_else(|| {
            response
                .as_ref()
                .err()
                .map(ToString::to_string)
                .or_else(|| response.as_ref().ok().and_then(provider_diagnostic))
        });

        if !response.as_ref().is_ok_and(CommandOutput::is_success) {
            return Ok(self.terminal_stack_merge_receipt(StackMergeReceiptInput {
                repository,
                plan: &checkpoint.plan,
                request,
                checkpoint: Some(checkpoint.clone()),
                status: GitHubStackMergeStatus::Indeterminate,
                provider_status,
                provider_message,
                provider_sha: parsed.sha,
                observation: None,
            }));
        }

        match normalized_provider_status(parsed.status.as_deref()) {
            Some(GitHubStackMergeStatus::Pending) => {
                Ok(stack_merge_receipt(StackMergeReceiptInput {
                    repository,
                    plan: &checkpoint.plan,
                    request,
                    checkpoint: Some(checkpoint.clone()),
                    status: GitHubStackMergeStatus::Pending,
                    provider_status,
                    provider_message,
                    provider_sha: parsed.sha,
                    observation: None,
                }))
            }
            Some(GitHubStackMergeStatus::Enqueued) => {
                Ok(stack_merge_receipt(StackMergeReceiptInput {
                    repository,
                    plan: &checkpoint.plan,
                    request,
                    checkpoint: Some(checkpoint.clone()),
                    status: GitHubStackMergeStatus::Enqueued,
                    provider_status,
                    provider_message,
                    provider_sha: parsed.sha,
                    observation: None,
                }))
            }
            Some(GitHubStackMergeStatus::Merged | GitHubStackMergeStatus::Failed) | None => {
                Ok(self.terminal_stack_merge_receipt(StackMergeReceiptInput {
                    repository,
                    plan: &checkpoint.plan,
                    request,
                    checkpoint: Some(checkpoint.clone()),
                    status: GitHubStackMergeStatus::Indeterminate,
                    provider_status,
                    provider_message,
                    provider_sha: parsed.sha,
                    observation: None,
                }))
            }
            Some(other) => Ok(stack_merge_receipt(StackMergeReceiptInput {
                repository,
                plan: &checkpoint.plan,
                request,
                checkpoint: Some(checkpoint.clone()),
                status: other,
                provider_status,
                provider_message,
                provider_sha: parsed.sha,
                observation: None,
            })),
        }
    }

    fn terminal_stack_merge_receipt(
        &self,
        mut input: StackMergeReceiptInput<'_>,
    ) -> GitHubStackMergeReceipt {
        let observation = match self.observe_stack_merge(input.repository, input.plan) {
            Ok(observation) => observation,
            Err(error) => {
                input.provider_message = Some(match input.provider_message {
                    Some(message) => format!("{message}; provider rediscovery failed: {error}"),
                    None => format!("provider rediscovery failed: {error}"),
                });
                input.status = GitHubStackMergeStatus::Indeterminate;
                return stack_merge_receipt(input);
            }
        };
        let observed_status = classify_observation(input.plan, &observation);
        let provider_result_exact = input
            .provider_sha
            .as_ref()
            .is_none_or(|sha| observation.default_branch.oid == *sha);
        input.status = if observed_status == GitHubStackMergeStatus::Merged && provider_result_exact
        {
            GitHubStackMergeStatus::Merged
        } else if observed_status == GitHubStackMergeStatus::Indeterminate {
            GitHubStackMergeStatus::Indeterminate
        } else if input
            .provider_status
            .as_deref()
            .is_some_and(|status| status.eq_ignore_ascii_case("failed"))
        {
            GitHubStackMergeStatus::Failed
        } else {
            GitHubStackMergeStatus::Indeterminate
        };
        input.observation = Some(observation);
        stack_merge_receipt(input)
    }

    fn observe_stack_merge(
        &self,
        repository: &RepositoryId,
        plan: &GitHubStackAsyncMergePlan,
    ) -> Result<GitHubStackMergeObservation, GitHubStackMergeError> {
        let default: GitRefResponse =
            self.json(git_ref_command(repository, &plan.before.topology.base.name))?;
        let default_branch = BranchSnapshot {
            repository: repository.clone(),
            name: plan.before.topology.base.name.clone(),
            oid: CommitOid(default.object.sha),
        };

        let mut observed = Vec::with_capacity(plan.before.topology.entries.len());
        for entry in &plan.before.topology.entries {
            observed.push(GitHubStackMergePullRequestObservation::from(
                self.refetch_pull_request(repository, entry.pr)?,
            ));
        }
        let selected_count = plan.selected.len();
        let selected = observed[..selected_count].to_vec();
        let remaining = observed[selected_count..].to_vec();
        let selected_merged = selected
            .iter()
            .filter(|entry| entry.state == PullRequestState::Merged && entry.merged_at.is_some())
            .count();
        let changed_selected_heads = selected
            .iter()
            .zip(&plan.selected)
            .filter(|(actual, expected)| actual.head != expected.head)
            .map(|(actual, _)| actual.pr)
            .collect::<Vec<_>>();
        let selected_heads_exact = changed_selected_heads.is_empty();
        let stack_after = self.native_stack_generation(repository, plan.before.number)?;
        let expected_remaining = plan.before.topology.entries[selected_count..]
            .iter()
            .map(|entry| entry.pr)
            .collect::<Vec<_>>();
        let remaining_order_exact = match stack_after.as_ref() {
            Some(stack) => {
                stack
                    .topology
                    .entries
                    .iter()
                    .filter(|entry| entry.pull_request_state == PullRequestState::Open)
                    .map(|entry| entry.pr)
                    .collect::<Vec<_>>()
                    == expected_remaining
            }
            None => expected_remaining.is_empty(),
        };

        Ok(GitHubStackMergeObservation {
            default_branch,
            selected,
            remaining,
            stack_after,
            selected_merged,
            selected_heads_exact,
            changed_selected_heads,
            remaining_order_exact,
        })
    }
}

struct StackMergeReceiptInput<'a> {
    repository: &'a RepositoryId,
    plan: &'a GitHubStackAsyncMergePlan,
    request: GitHubStackMergeRequestIdentity,
    checkpoint: Option<GitHubStackMergeCheckpoint>,
    status: GitHubStackMergeStatus,
    provider_status: Option<String>,
    provider_message: Option<String>,
    provider_sha: Option<CommitOid>,
    observation: Option<GitHubStackMergeObservation>,
}

fn stack_merge_checkpoint(
    repository: &RepositoryId,
    plan: &GitHubStackAsyncMergePlan,
    request: &GitHubStackMergeRequestIdentity,
    provider: &ProviderAsyncMergeResult,
) -> Option<GitHubStackMergeCheckpoint> {
    provider
        .uuid
        .as_ref()
        .filter(|uuid| valid_async_uuid(uuid))
        .map(|uuid| {
            GitHubStackMergeCheckpoint {
                schema_version: STACK_MERGE_SCHEMA_VERSION,
                repository: repository.clone(),
                plan: plan.clone(),
                request: request.clone(),
                uuid: uuid.clone(),
                initial_provider_status: provider
                    .status
                    .clone()
                    .unwrap_or_else(|| "unknown".to_owned()),
                evidence_hash: String::new(),
            }
            .seal()
        })
}

fn stack_merge_receipt(input: StackMergeReceiptInput<'_>) -> GitHubStackMergeReceipt {
    GitHubStackMergeReceipt {
        schema_version: STACK_MERGE_SCHEMA_VERSION,
        repository: input.repository.clone(),
        plan: input.plan.clone(),
        request: input.request,
        checkpoint: input.checkpoint,
        status: input.status,
        provider_status: input.provider_status,
        provider_message: input.provider_message,
        provider_sha: input.provider_sha,
        observation: input.observation,
        evidence_hash: String::new(),
    }
    .seal()
}

fn classify_observation(
    plan: &GitHubStackAsyncMergePlan,
    observation: &GitHubStackMergeObservation,
) -> GitHubStackMergeStatus {
    if observation.selected_merged == plan.selected.len()
        && observation.selected_heads_exact
        && observation.remaining_order_exact
    {
        GitHubStackMergeStatus::Merged
    } else if observation.selected_merged == 0 {
        GitHubStackMergeStatus::Failed
    } else {
        GitHubStackMergeStatus::Indeterminate
    }
}

fn validate_merge_plan(
    repository: &RepositoryId,
    plan: &GitHubStackAsyncMergePlan,
) -> Result<(), GitHubStackMergeError> {
    if plan.operation_id.trim().is_empty() {
        return Err(invalid_plan(
            "github_stack_merge_operation_id_missing",
            "native Stack merge requires a non-empty operation identity",
        ));
    }
    if plan.actor.trim().is_empty() {
        return Err(invalid_plan(
            "github_stack_merge_actor_missing",
            "native Stack merge requires a non-empty actor",
        ));
    }
    if plan.before.topology.base.repository != *repository {
        return Err(invalid_plan(
            "github_stack_merge_repository_mismatch",
            "native Stack merge generation belongs to another repository",
        ));
    }
    if plan.selected.is_empty() || plan.selected.len() > plan.before.topology.entries.len() {
        return Err(invalid_plan(
            "github_stack_merge_prefix_invalid",
            "selected Stack merge prefix must be non-empty and bounded by the Stack",
        ));
    }
    if !plan.before.open
        || plan.selected.as_slice() != &plan.before.topology.entries[..plan.selected.len()]
    {
        return Err(invalid_plan(
            "github_stack_merge_prefix_invalid",
            "selected entries must be the exact bottom prefix of one open Stack generation",
        ));
    }
    for entry in &plan.selected {
        if entry.pull_request_state != PullRequestState::Open
            || entry.draft
            || !entry.stack_state.eq_ignore_ascii_case("open")
        {
            return Err(invalid_plan(
                "github_stack_merge_entry_ineligible",
                "every selected Stack entry must be open and non-draft",
            ));
        }
    }
    Ok(())
}

fn validate_checkpoint(
    repository: &RepositoryId,
    checkpoint: &GitHubStackMergeCheckpoint,
) -> Result<(), GitHubStackMergeError> {
    if checkpoint.schema_version != STACK_MERGE_SCHEMA_VERSION {
        return Err(GitHubStackMergeError::InvalidCheckpoint {
            diagnostic: format!(
                "schema version {} is unsupported",
                checkpoint.schema_version
            ),
        });
    }
    if !checkpoint.verify() {
        return Err(GitHubStackMergeError::InvalidCheckpoint {
            diagnostic: "evidence hash does not match".to_owned(),
        });
    }
    if checkpoint.repository != *repository || !valid_async_uuid(&checkpoint.uuid) {
        return Err(GitHubStackMergeError::InvalidCheckpoint {
            diagnostic: "repository or UUID does not match the persisted intent".to_owned(),
        });
    }
    validate_merge_plan(repository, &checkpoint.plan)?;
    let expected = merge_request_identity(repository, &checkpoint.plan);
    if checkpoint.request.method != expected.method
        || checkpoint.request.path != expected.path
        || checkpoint.request.selected_top != expected.selected_top
        || checkpoint.request.selected_top_sha != expected.selected_top_sha
        || checkpoint.request.ordered_pull_requests != expected.ordered_pull_requests
        || checkpoint.request.ordered_heads != expected.ordered_heads
    {
        return Err(GitHubStackMergeError::InvalidCheckpoint {
            diagnostic: "request identity does not match the sealed merge plan".to_owned(),
        });
    }
    Ok(())
}

fn invalid_plan(code: &str, message: &str) -> GitHubStackMergeError {
    GitHubStackMergeError::InvalidPlan {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

fn merge_request_identity(
    repository: &RepositoryId,
    plan: &GitHubStackAsyncMergePlan,
) -> GitHubStackMergeRequestIdentity {
    let selected_top = plan.selected.last().expect("validated non-empty prefix");
    GitHubStackMergeRequestIdentity {
        method: "PUT".to_owned(),
        path: format!(
            "repos/{}/pulls/{}/merge-async",
            repository.slug(),
            selected_top.pr
        ),
        selected_top: selected_top.pr,
        selected_top_sha: selected_top.head.oid.clone(),
        ordered_pull_requests: plan.selected.iter().map(|entry| entry.pr).collect(),
        ordered_heads: plan
            .selected
            .iter()
            .map(|entry| entry.head.oid.clone())
            .collect(),
        github_request_id: None,
    }
}

fn native_stack_merge_submit_command(
    repository: &RepositoryId,
    plan: &GitHubStackAsyncMergePlan,
) -> CommandSpec {
    let top = plan.selected.last().expect("validated non-empty prefix");
    stack_merge_api_command(
        "PUT",
        format!("repos/{}/pulls/{}/merge-async", repository.slug(), top.pr),
    )
    .args(["--input", "-"])
    .stdin(
        serde_json::json!({
            "sha": top.head.oid.0,
            "merge_method": plan.merge_method.provider_value(),
            "merge_action": plan.merge_action.provider_value(),
        })
        .to_string(),
    )
}

fn native_stack_merge_poll_command(
    repository: &RepositoryId,
    checkpoint: &GitHubStackMergeCheckpoint,
) -> CommandSpec {
    stack_merge_api_command(
        "GET",
        format!(
            "repos/{}/pulls/{}/merge-async/{}",
            repository.slug(),
            checkpoint.request.selected_top,
            checkpoint.uuid
        ),
    )
}

fn stack_merge_api_command(method: &str, path: String) -> CommandSpec {
    let command = CommandSpec::new("gh").args([
        "api".to_owned(),
        "--method".to_owned(),
        method.to_owned(),
        "-H".to_owned(),
        STACK_ACCEPT.to_owned(),
        "-H".to_owned(),
        STACK_API_VERSION.to_owned(),
        "--include".to_owned(),
        path,
    ]);
    if method == "GET" {
        command
    } else {
        command.provider_write()
    }
}

fn git_ref_command(repository: &RepositoryId, branch: &str) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        format!(
            "repos/{}/git/ref/heads/{}",
            repository.slug(),
            super::encode_path_segment(branch)
        ),
    ])
}

#[derive(Debug, Deserialize)]
struct GitRefResponse {
    object: GitRefObject,
}

#[derive(Debug, Deserialize)]
struct GitRefObject {
    sha: String,
}

fn parse_async_merge_response(
    output: &CommandOutput,
) -> Result<ProviderAsyncMergeResult, serde_json::Error> {
    let value: serde_json::Value = included_json(&output.stdout)?;
    let details = value.get("details");
    let string = |name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                details
                    .and_then(|details| details.get(name))
                    .and_then(serde_json::Value::as_str)
            })
            .map(ToOwned::to_owned)
    };
    Ok(ProviderAsyncMergeResult {
        uuid: string("uuid"),
        status: string("status"),
        message: string("message"),
        sha: string("sha").map(CommitOid),
        expected_head_sha: string("expected_head_sha").map(CommitOid),
        merge_method: string("merge_method"),
        merge_action: string("merge_action"),
    })
}

fn provider_request_matches_plan(
    provider: &ProviderAsyncMergeResult,
    plan: &GitHubStackAsyncMergePlan,
) -> bool {
    let Some(top) = plan.selected.last() else {
        return false;
    };
    provider.expected_head_sha.as_ref() == Some(&top.head.oid)
        && provider.merge_method.as_deref() == Some(plan.merge_method.provider_value())
        && provider.merge_action.as_deref() == Some(plan.merge_action.provider_value())
}

fn included_json<T: DeserializeOwned>(stdout: &str) -> Result<T, serde_json::Error> {
    if let Ok(value) = serde_json::from_str(stdout.trim()) {
        return Ok(value);
    }
    let body = stdout
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .or_else(|| stdout.split_once("\n\n").map(|(_, body)| body))
        .unwrap_or(stdout)
        .trim();
    serde_json::from_str(body)
}

fn normalized_provider_status(status: Option<&str>) -> Option<GitHubStackMergeStatus> {
    match status?.to_ascii_lowercase().as_str() {
        "pending" => Some(GitHubStackMergeStatus::Pending),
        "enqueued" => Some(GitHubStackMergeStatus::Enqueued),
        "merged" => Some(GitHubStackMergeStatus::Merged),
        "failed" => Some(GitHubStackMergeStatus::Failed),
        _ => None,
    }
}

fn github_request_id(output: &CommandOutput) -> Option<String> {
    output
        .stdout
        .lines()
        .chain(output.stderr.lines())
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("x-github-request-id")
                .then(|| value.trim().to_owned())
        })
}

fn valid_async_uuid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

fn append_rediscovery_error(
    message: Option<String>,
    rediscovery_error: Option<String>,
) -> Option<String> {
    match (message, rediscovery_error) {
        (Some(message), Some(error)) => {
            Some(format!("{message}; provider rediscovery failed: {error}"))
        }
        (None, Some(error)) => Some(format!("provider rediscovery failed: {error}")),
        (message, None) => message,
    }
}

fn provider_diagnostic(output: &CommandOutput) -> Option<String> {
    let value = if output.stderr.trim().is_empty() {
        output.stdout.trim()
    } else {
        output.stderr.trim()
    };
    (!value.is_empty()).then(|| super::diagnostic_excerpt(value))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;
    use crate::command::CommandRunError;
    use crate::model::RepositoryId;

    struct FakeRunner {
        calls: RefCell<VecDeque<(CommandSpec, Result<CommandOutput, CommandRunError>)>>,
    }

    impl FakeRunner {
        fn new(calls: Vec<(CommandSpec, CommandOutput)>) -> Self {
            Self::with_results(
                calls
                    .into_iter()
                    .map(|(command, output)| (command, Ok(output)))
                    .collect(),
            )
        }

        fn with_results(calls: Vec<(CommandSpec, Result<CommandOutput, CommandRunError>)>) -> Self {
            Self {
                calls: RefCell::new(calls.into()),
            }
        }

        fn assert_exhausted(&self) {
            assert!(self.calls.borrow().is_empty(), "not all calls were used");
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
            let (expected, result) = self
                .calls
                .borrow_mut()
                .pop_front()
                .expect("unexpected provider command");
            assert_eq!(expected, *command);
            result
        }
    }

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "acme".to_owned(),
            name: "widgets".to_owned(),
        }
    }

    fn branch(name: &str, oid: &str) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository(),
            name: name.to_owned(),
            oid: CommitOid(oid.to_owned()),
        }
    }

    fn generation() -> GitHubStackGeneration {
        let base = branch("main", "base000");
        let root = GitHubStackEntryGeneration {
            position: 0,
            pr: PrNumber(101),
            stack_state: "open".to_owned(),
            pull_request_state: PullRequestState::Open,
            draft: false,
            merged_at: None,
            base: base.clone(),
            head: branch("root", "aaa111"),
        };
        let child = GitHubStackEntryGeneration {
            position: 1,
            pr: PrNumber(102),
            stack_state: "open".to_owned(),
            pull_request_state: PullRequestState::Open,
            draft: false,
            merged_at: None,
            base: root.head.clone(),
            head: branch("child", "bbb222"),
        };
        GitHubStackGeneration {
            id: 9,
            number: 42,
            node_id: "S_stack".to_owned(),
            open: true,
            created_at: "2026-07-31T10:00:00Z".to_owned(),
            topology: super::super::GitHubStackTopology {
                base,
                entries: vec![root, child],
            },
        }
    }

    fn ready_evidence(stack: &GitHubStackGeneration) -> Vec<GitHubStackMergeEntryEvidence> {
        stack
            .topology
            .entries
            .iter()
            .cloned()
            .map(|generation| GitHubStackMergeEntryEvidence {
                generation,
                blockers: Vec::new(),
            })
            .collect()
    }

    fn stack_snapshot_json(stack: &GitHubStackGeneration) -> String {
        serde_json::json!({
            "id": stack.id,
            "number": stack.number,
            "node_id": stack.node_id,
            "base": {"ref": stack.topology.base.name},
            "open": stack.open,
            "created_at": stack.created_at,
            "pull_requests": stack.topology.entries.iter().map(|entry| serde_json::json!({
                "number": entry.pr.0,
                "state": entry.stack_state,
                "draft": entry.draft,
                "merged_at": entry.merged_at,
                "head": {"ref": entry.head.name, "sha": entry.head.oid},
            })).collect::<Vec<_>>()
        })
        .to_string()
    }

    fn pull_request_json(entry: &GitHubStackEntryGeneration) -> String {
        serde_json::json!({
            "number": entry.pr.0,
            "title": format!("PR {}", entry.pr.0),
            "body": "",
            "state": match entry.pull_request_state {
                PullRequestState::Open => "OPEN",
                PullRequestState::Closed => "CLOSED",
                PullRequestState::Merged => "MERGED",
            },
            "isDraft": entry.draft,
            "headRefName": entry.head.name,
            "headRefOid": entry.head.oid,
            "headRepository": {"name": "widgets", "nameWithOwner": "acme/widgets"},
            "headRepositoryOwner": {"login": "acme"},
            "isCrossRepository": false,
            "baseRefName": entry.base.name,
            "baseRefOid": entry.base.oid,
            "labels": [],
            "autoMergeRequest": null,
            "statusCheckRollup": [],
            "createdAt": "2026-07-31T09:00:00Z",
            "mergedAt": entry.merged_at,
            "url": format!("https://github.com/acme/widgets/pull/{}", entry.pr.0),
            "updatedAt": "2026-07-31T10:00:00Z",
            "mergeStateStatus": "CLEAN"
        })
        .to_string()
    }

    fn direct_generation_calls(stack: &GitHubStackGeneration) -> Vec<(CommandSpec, CommandOutput)> {
        let mut calls = vec![(
            super::super::stack::native_stack_read_command(&repository(), stack.number),
            CommandOutput::success(stack_snapshot_json(stack)),
        )];
        calls.push((
            super::super::stack::native_stack_base_ref_command(
                &repository(),
                &stack.topology.base.name,
            ),
            CommandOutput::success(
                serde_json::json!({"object": {"sha": stack.topology.base.oid.0}}).to_string(),
            ),
        ));
        calls.extend(stack.topology.entries.iter().map(|entry| {
            (
                super::super::pull_request_command(&repository(), &entry.pr.to_string()),
                CommandOutput::success(pull_request_json(entry)),
            )
        }));
        calls
    }

    fn merge_observation_calls(
        stack_after: &GitHubStackGeneration,
        default_oid: &str,
    ) -> Vec<(CommandSpec, CommandOutput)> {
        let mut calls = vec![(
            git_ref_command(&repository(), &stack_after.topology.base.name),
            CommandOutput::success(serde_json::json!({"object": {"sha": default_oid}}).to_string()),
        )];
        calls.extend(stack_after.topology.entries.iter().map(|entry| {
            (
                super::super::pull_request_command(&repository(), &entry.pr.to_string()),
                CommandOutput::success(pull_request_json(entry)),
            )
        }));
        calls.extend(direct_generation_calls(stack_after));
        calls
    }

    fn direct_plan(stack: &GitHubStackGeneration) -> GitHubStackAsyncMergePlan {
        plan_github_stack_ready_prefix(stack, &ready_evidence(stack))
            .unwrap()
            .direct_squash_plan("op-1", "cara")
            .unwrap()
    }

    fn checkpoint(plan: GitHubStackAsyncMergePlan) -> GitHubStackMergeCheckpoint {
        let request = merge_request_identity(&repository(), &plan);
        GitHubStackMergeCheckpoint {
            schema_version: STACK_MERGE_SCHEMA_VERSION,
            repository: repository(),
            plan,
            request,
            uuid: "merge-uuid".to_owned(),
            initial_provider_status: "pending".to_owned(),
            evidence_hash: String::new(),
        }
        .seal()
    }

    fn terminal_stack(
        mut stack: GitHubStackGeneration,
        merged_count: usize,
    ) -> GitHubStackGeneration {
        for entry in stack.topology.entries.iter_mut().take(merged_count) {
            entry.stack_state = "merged".to_owned();
            entry.pull_request_state = PullRequestState::Merged;
            entry.merged_at = Some("2026-07-31T11:00:00Z".to_owned());
        }
        if merged_count == stack.topology.entries.len() {
            stack.open = false;
        }
        stack
    }

    #[test]
    fn planner_selects_only_the_maximal_contiguous_ready_prefix() {
        let stack = generation();
        let mut evidence = ready_evidence(&stack);
        evidence[1].blockers = vec![
            GitHubStackMergeBlocker::RequiredChecksNotReady,
            GitHubStackMergeBlocker::ForceUnsupported,
        ];

        let selected = plan_github_stack_ready_prefix(&stack, &evidence).unwrap();

        assert_eq!(selected.selected, stack.topology.entries[..1]);
        assert_eq!(
            selected.first_blocked.unwrap().blockers,
            vec![
                GitHubStackMergeBlocker::RequiredChecksNotReady,
                GitHubStackMergeBlocker::ForceUnsupported
            ]
        );
    }

    #[test]
    fn planner_rejects_stale_or_partial_policy_evidence() {
        let stack = generation();
        let evidence = ready_evidence(&stack);
        assert!(matches!(
            plan_github_stack_ready_prefix(&stack, &evidence[..1]),
            Err(GitHubStackMergeError::InvalidPlan { code, .. })
                if code == "github_stack_merge_evidence_incomplete"
        ));
        let mut stale = evidence;
        stale[0].generation.head.oid = CommitOid("moved".to_owned());
        assert!(matches!(
            plan_github_stack_ready_prefix(&stack, &stale),
            Err(GitHubStackMergeError::InvalidPlan { code, .. })
                if code == "github_stack_merge_evidence_stale"
        ));
    }

    #[test]
    fn submit_binds_every_head_but_sends_only_documented_top_sha() {
        let stack = generation();
        let prefix = plan_github_stack_ready_prefix(&stack, &ready_evidence(&stack)).unwrap();
        let plan = prefix.direct_squash_plan("op-1", "cara").unwrap();
        let mut calls = direct_generation_calls(&stack);
        calls.push((
            native_stack_merge_submit_command(&repository(), &plan),
            CommandOutput::success(
                "HTTP/2 202 Accepted\nx-github-request-id: REQ-7\n\n{\"uuid\":\"merge-uuid\",\"status\":\"pending\",\"details\":{\"message\":\"in progress\"}}",
            ),
        ));
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));

        let receipt = adapter
            .native_stack_merge_submit(&repository(), &plan)
            .unwrap();

        assert_eq!(receipt.status, GitHubStackMergeStatus::Submitted);
        assert_eq!(receipt.request.selected_top, PrNumber(102));
        assert_eq!(
            receipt.request.selected_top_sha,
            CommitOid("bbb222".to_owned())
        );
        assert_eq!(
            receipt.request.ordered_heads,
            vec![
                CommitOid("aaa111".to_owned()),
                CommitOid("bbb222".to_owned())
            ]
        );
        let checkpoint = receipt.checkpoint.as_ref().unwrap();
        assert_eq!(checkpoint.uuid, "merge-uuid");
        assert!(checkpoint.verify());
        assert!(receipt.verify());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn exact_conflict_response_recovers_existing_uuid_without_another_write() {
        let stack = generation();
        let plan = direct_plan(&stack);
        let mut calls = direct_generation_calls(&stack);
        calls.push((
            native_stack_merge_submit_command(&repository(), &plan),
            CommandOutput {
                code: Some(1),
                stdout: "HTTP/2 409 Conflict\nx-github-request-id: REQ-CONFLICT\n\n{\"status\":\"pending\",\"details\":{\"message\":\"A merge request already exists for this pull request.\",\"uuid\":\"existing-uuid\",\"merge_method\":\"squash\",\"merge_action\":\"direct_merge\",\"expected_head_sha\":\"bbb222\"}}".to_owned(),
                stderr: "gh: HTTP 409".to_owned(),
            },
        ));
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));

        let receipt = adapter
            .native_stack_merge_submit(&repository(), &plan)
            .unwrap();

        assert_eq!(receipt.status, GitHubStackMergeStatus::Submitted);
        assert_eq!(receipt.checkpoint.as_ref().unwrap().uuid, "existing-uuid");
        assert_eq!(
            receipt.request.github_request_id.as_deref(),
            Some("REQ-CONFLICT")
        );
        assert!(receipt.verify());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn poll_pending_is_one_bounded_get_and_retains_checkpoint() {
        let stack = generation();
        let prefix = plan_github_stack_ready_prefix(&stack, &ready_evidence(&stack)).unwrap();
        let plan = prefix.direct_squash_plan("op-1", "cara").unwrap();
        let request = merge_request_identity(&repository(), &plan);
        let checkpoint = GitHubStackMergeCheckpoint {
            schema_version: STACK_MERGE_SCHEMA_VERSION,
            repository: repository(),
            plan,
            request,
            uuid: "merge-uuid".to_owned(),
            initial_provider_status: "pending".to_owned(),
            evidence_hash: String::new(),
        }
        .seal();
        let runner = FakeRunner::new(vec![(
            native_stack_merge_poll_command(&repository(), &checkpoint),
            CommandOutput::success(
                "HTTP/2 200 OK\n\n{\"uuid\":\"merge-uuid\",\"status\":\"pending\"}",
            ),
        )]);
        let adapter = GitHubMutationAdapter::new(runner);

        let receipt = adapter
            .native_stack_merge_poll(&repository(), &checkpoint)
            .unwrap();

        assert_eq!(receipt.status, GitHubStackMergeStatus::Pending);
        assert_eq!(receipt.checkpoint, Some(checkpoint));
        assert!(receipt.verify());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn lower_entry_movement_before_submit_refuses_without_a_put() {
        let before = generation();
        let plan = direct_plan(&before);
        let mut moved = before.clone();
        moved.topology.entries[0].head.oid = CommitOid("moved111".to_owned());
        moved.topology.entries[1].base.oid = CommitOid("moved111".to_owned());
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(direct_generation_calls(&moved)));

        let result = adapter.native_stack_merge_submit(&repository(), &plan);

        assert!(matches!(
            result,
            Err(GitHubStackMergeError::StaleGeneration { actual: Some(actual), .. })
                if *actual == moved
        ));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn submit_timeout_without_uuid_is_indeterminate_and_never_retried() {
        let stack = generation();
        let plan = direct_plan(&stack);
        let submit = native_stack_merge_submit_command(&repository(), &plan);
        let mut calls = direct_generation_calls(&stack)
            .into_iter()
            .map(|(command, output)| (command, Ok(output)))
            .collect::<Vec<_>>();
        calls.push((
            submit.clone(),
            Err(CommandRunError::Timeout {
                command: submit,
                process_group_id: Some(44),
                timeout_ms: 30_000,
                stdout: String::new(),
                stderr: String::new(),
            }),
        ));
        calls.extend(
            merge_observation_calls(&stack, "base000")
                .into_iter()
                .map(|(command, output)| (command, Ok(output))),
        );
        let adapter = GitHubMutationAdapter::new(FakeRunner::with_results(calls));

        let receipt = adapter
            .native_stack_merge_submit(&repository(), &plan)
            .unwrap();

        assert_eq!(receipt.status, GitHubStackMergeStatus::Indeterminate);
        assert!(receipt.checkpoint.is_none());
        assert_eq!(receipt.observation.as_ref().unwrap().selected_merged, 0);
        assert!(receipt.verify());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn failed_poll_proves_no_selected_entry_merged() {
        let stack = generation();
        let checkpoint = checkpoint(direct_plan(&stack));
        let mut calls = vec![(
            native_stack_merge_poll_command(&repository(), &checkpoint),
            CommandOutput::success(
                "HTTP/2 200 OK\n\n{\"uuid\":\"merge-uuid\",\"status\":\"failed\",\"details\":{\"message\":\"checks changed\"}}",
            ),
        )];
        calls.extend(merge_observation_calls(&stack, "base000"));
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));

        let receipt = adapter
            .native_stack_merge_poll(&repository(), &checkpoint)
            .unwrap();

        assert_eq!(receipt.status, GitHubStackMergeStatus::Failed);
        assert_eq!(receipt.observation.as_ref().unwrap().selected_merged, 0);
        assert!(receipt.verify());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn merged_poll_requires_all_selected_entries_and_seals_provider_truth() {
        let before = generation();
        let checkpoint = checkpoint(direct_plan(&before));
        let after = terminal_stack(before, 2);
        let mut calls = vec![(
            native_stack_merge_poll_command(&repository(), &checkpoint),
            CommandOutput::success(
                "HTTP/2 200 OK\n\n{\"uuid\":\"merge-uuid\",\"status\":\"merged\",\"details\":{\"sha\":\"merge999\",\"message\":\"merged\"}}",
            ),
        )];
        calls.extend(merge_observation_calls(&after, "merge999"));
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));

        let receipt = adapter
            .native_stack_merge_poll(&repository(), &checkpoint)
            .unwrap();

        assert_eq!(receipt.status, GitHubStackMergeStatus::Merged);
        assert_eq!(receipt.provider_sha, Some(CommitOid("merge999".to_owned())));
        assert_eq!(receipt.observation.as_ref().unwrap().selected_merged, 2);
        assert!(receipt.observation.as_ref().unwrap().remaining_order_exact);
        assert!(receipt.verify());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn merged_provider_status_with_changed_lower_head_is_indeterminate() {
        let before = generation();
        let checkpoint = checkpoint(direct_plan(&before));
        let mut after = terminal_stack(before, 2);
        after.topology.entries[0].head.oid = CommitOid("rewound-root".to_owned());
        let mut calls = vec![(
            native_stack_merge_poll_command(&repository(), &checkpoint),
            CommandOutput::success(
                "HTTP/2 200 OK\n\n{\"uuid\":\"merge-uuid\",\"status\":\"merged\",\"details\":{\"sha\":\"merge999\"}}",
            ),
        )];
        calls.extend(merge_observation_calls(&after, "merge999"));
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));

        let receipt = adapter
            .native_stack_merge_poll(&repository(), &checkpoint)
            .unwrap();

        assert_eq!(receipt.status, GitHubStackMergeStatus::Indeterminate);
        let observation = receipt.observation.as_ref().unwrap();
        assert_eq!(observation.selected_merged, 2);
        assert!(!observation.selected_heads_exact);
        assert_eq!(observation.changed_selected_heads, vec![PrNumber(101)]);
        assert!(receipt.verify());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn partial_prefix_merge_accepts_retargeted_open_suffix() {
        let before = generation();
        let mut evidence = ready_evidence(&before);
        evidence[1].blockers = vec![GitHubStackMergeBlocker::RequiredChecksNotReady];
        let plan = plan_github_stack_ready_prefix(&before, &evidence)
            .unwrap()
            .direct_squash_plan("op-partial", "cara")
            .unwrap();
        let checkpoint = checkpoint(plan);
        let mut after = terminal_stack(before, 1);
        after.topology.base.oid = CommitOid("merge111".to_owned());
        after.topology.entries[1].base = after.topology.base.clone();
        after.topology.entries[1].head.oid = CommitOid("rebased222".to_owned());
        let mut calls = vec![(
            native_stack_merge_poll_command(&repository(), &checkpoint),
            CommandOutput::success(
                "HTTP/2 200 OK\n\n{\"uuid\":\"merge-uuid\",\"status\":\"merged\",\"details\":{\"sha\":\"merge111\"}}",
            ),
        )];
        calls.extend(merge_observation_calls(&after, "merge111"));
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));

        let receipt = adapter
            .native_stack_merge_poll(&repository(), &checkpoint)
            .unwrap();

        assert_eq!(receipt.status, GitHubStackMergeStatus::Merged);
        let observation = receipt.observation.as_ref().unwrap();
        assert_eq!(observation.selected_merged, 1);
        assert!(observation.remaining_order_exact);
        assert_eq!(observation.remaining[0].base, observation.default_branch);
        assert!(receipt.verify());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn impossible_partial_terminal_result_is_indeterminate() {
        let before = generation();
        let checkpoint = checkpoint(direct_plan(&before));
        let mut after = terminal_stack(before, 1);
        after.topology.base.oid = CommitOid("merge111".to_owned());
        after.topology.entries[1].base = after.topology.base.clone();
        after.topology.entries[1].head.oid = CommitOid("rebased222".to_owned());
        let mut calls = vec![(
            native_stack_merge_poll_command(&repository(), &checkpoint),
            CommandOutput::success(
                "HTTP/2 200 OK\n\n{\"uuid\":\"merge-uuid\",\"status\":\"failed\",\"details\":{\"message\":\"partial\"}}",
            ),
        )];
        calls.extend(merge_observation_calls(&after, "merge111"));
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));

        let receipt = adapter
            .native_stack_merge_poll(&repository(), &checkpoint)
            .unwrap();

        assert_eq!(receipt.status, GitHubStackMergeStatus::Indeterminate);
        assert_eq!(receipt.observation.as_ref().unwrap().selected_merged, 1);
        assert!(receipt.verify());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn tampered_checkpoint_is_rejected_before_provider_access() {
        let stack = generation();
        let mut checkpoint = checkpoint(direct_plan(&stack));
        checkpoint.uuid = "different".to_owned();
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(Vec::new()));

        assert!(matches!(
            adapter.native_stack_merge_poll(&repository(), &checkpoint),
            Err(GitHubStackMergeError::InvalidCheckpoint { .. })
        ));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn async_stack_put_is_marked_and_poll_get_remains_read_only() {
        let write = stack_merge_api_command("PUT", "repos/o/r/pulls/1/merge-async".to_owned());
        assert_eq!(write.intent(), crate::command::CommandIntent::ProviderWrite);
        assert_eq!(write.inferred_write_intent(), Some(write.intent()));
        let read = stack_merge_api_command("GET", "repos/o/r/pulls/1/merge-async/u".to_owned());
        assert_eq!(read.intent(), crate::command::CommandIntent::Read);
        assert_eq!(read.inferred_write_intent(), None);
    }

    #[test]
    fn command_surface_never_invokes_local_gh_stack_state() {
        let stack = generation();
        let plan = plan_github_stack_ready_prefix(&stack, &ready_evidence(&stack))
            .unwrap()
            .direct_squash_plan("op", "cara")
            .unwrap();
        let request = merge_request_identity(&repository(), &plan);
        let checkpoint = GitHubStackMergeCheckpoint {
            schema_version: STACK_MERGE_SCHEMA_VERSION,
            repository: repository(),
            plan: plan.clone(),
            request,
            uuid: "uuid".to_owned(),
            initial_provider_status: "pending".to_owned(),
            evidence_hash: String::new(),
        }
        .seal();
        for command in [
            native_stack_merge_submit_command(&repository(), &plan),
            native_stack_merge_poll_command(&repository(), &checkpoint),
        ] {
            assert_eq!(command.program, "gh");
            assert_eq!(command.args.first().map(String::as_str), Some("api"));
            assert!(!command.args.iter().any(|arg| arg == "stack"));
            assert!(
                !command
                    .args
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "sync" | "rebase" | "link"))
            );
        }
    }
}
