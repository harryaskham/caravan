//! Exact provider adapter for GitHub's native Stack REST resource.
//!
//! The adapter deliberately owns no scheduler policy. Callers supply one exact
//! pre/post topology and an operation identity; this layer fresh-reads provider
//! truth, performs at most one documented REST mutation, then rediscovers the
//! Stack before returning a sealed receipt. It never invokes the local
//! `gh stack` tracking, sync, rebase, or link workflows.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    GitHubMutationAdapter, GitHubStackInventory, GitHubStackReadError, GitHubStackSnapshot,
    MutationError,
};
use crate::command::{CommandOutput, CommandRunError, CommandRunner, CommandSpec};
use crate::model::{
    BranchSnapshot, CommitOid, PrNumber, PullRequestSnapshot, PullRequestState, RepositoryId,
};

const STACK_ACCEPT: &str = "Accept: application/vnd.github+json";
const STACK_API_VERSION: &str = "X-GitHub-Api-Version: 2026-03-10";
const STACK_SCHEMA_VERSION: u32 = 1;

/// Exact ordered Stack topology. Entries are bottom-to-top.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackTopology {
    pub base: BranchSnapshot,
    pub entries: Vec<GitHubStackEntryGeneration>,
}

/// One exact provider PR generation in a Stack topology.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackEntryGeneration {
    /// Zero-based position, bottom-to-top.
    pub position: u32,
    pub pr: PrNumber,
    /// Stack-resource state, preserved verbatim for queued/merging evidence.
    pub stack_state: String,
    pub pull_request_state: PullRequestState,
    pub draft: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<String>,
    pub base: BranchSnapshot,
    pub head: BranchSnapshot,
}

/// Exact Stack identity plus its fresh ordered PR/base/head generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackGeneration {
    pub id: u64,
    pub number: u64,
    pub node_id: String,
    pub open: bool,
    pub created_at: String,
    pub topology: GitHubStackTopology,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackCreatePlan {
    pub operation_id: String,
    pub actor: String,
    pub desired: GitHubStackTopology,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackAddPlan {
    pub operation_id: String,
    pub actor: String,
    pub before: GitHubStackGeneration,
    pub desired: GitHubStackTopology,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackUnstackPlan {
    pub operation_id: String,
    pub actor: String,
    pub before: GitHubStackGeneration,
    /// `None` means the Stack must disappear after every removable entry is
    /// unstacked. A retained merged/queued generation can be supplied exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_after: Option<GitHubStackTopology>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitHubStackMutationOperation {
    Create,
    Add,
    Unstack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitHubStackMutationDisposition {
    Completed,
    /// The provider response was unsuccessful/ambiguous, but fresh provider
    /// truth proves the complete requested postcondition.
    RecoveredAfterAmbiguousResponse,
    AlreadySatisfied,
}

/// Non-secret provider request identity retained even across response loss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackRequestIdentity {
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub ordered_pull_requests: Vec<PrNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_request_id: Option<String>,
}

/// Sealed exact pre/post receipt for one native Stack mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackMutationReceipt {
    pub schema_version: u32,
    pub operation_id: String,
    pub operation: GitHubStackMutationOperation,
    pub repository: RepositoryId,
    pub actor: String,
    pub disposition: GitHubStackMutationDisposition,
    pub request: GitHubStackRequestIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<GitHubStackGeneration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<GitHubStackGeneration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_output: Option<String>,
    pub evidence_hash: String,
}

impl GitHubStackMutationReceipt {
    fn seal(mut self) -> Self {
        self.evidence_hash.clear();
        let material = serde_json::to_vec(&self).expect("GitHub Stack receipt serializes");
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

/// Typed refusal from the Stack mutation adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHubStackMutationError {
    InvalidPlan {
        code: String,
        message: String,
    },
    Unavailable {
        diagnostic: String,
    },
    CapabilityUnknown {
        diagnostic: String,
    },
    Provider(MutationError),
    InventoryTruncated,
    InconsistentProviderState {
        diagnostic: String,
    },
    StaleGeneration {
        expected: Box<GitHubStackTopology>,
        actual: Option<Box<GitHubStackGeneration>>,
        changed_fields: Vec<String>,
    },
    AmbiguousResponse {
        operation: GitHubStackMutationOperation,
        diagnostic: String,
        rediscovery_diagnostic: Option<String>,
        observed: Option<Box<GitHubStackGeneration>>,
    },
    PostconditionFailed {
        operation: GitHubStackMutationOperation,
        expected: Option<Box<GitHubStackTopology>>,
        actual: Option<Box<GitHubStackGeneration>>,
    },
}

impl std::fmt::Display for GitHubStackMutationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPlan { code, message } => write!(formatter, "{code}: {message}"),
            Self::Unavailable { diagnostic } => {
                write!(
                    formatter,
                    "GitHub native Stacks are unavailable: {diagnostic}"
                )
            }
            Self::CapabilityUnknown { diagnostic } => {
                write!(
                    formatter,
                    "GitHub Stack capability is unknown: {diagnostic}"
                )
            }
            Self::Provider(error) => error.fmt(formatter),
            Self::InventoryTruncated => write!(
                formatter,
                "GitHub Stack inventory is truncated; exact absence cannot be proven"
            ),
            Self::InconsistentProviderState { diagnostic } => {
                write!(formatter, "inconsistent GitHub Stack state: {diagnostic}")
            }
            Self::StaleGeneration { changed_fields, .. } => write!(
                formatter,
                "GitHub Stack generation changed: {}",
                changed_fields.join(", ")
            ),
            Self::AmbiguousResponse {
                operation,
                diagnostic,
                ..
            } => write!(
                formatter,
                "GitHub Stack {operation:?} response is ambiguous: {diagnostic}"
            ),
            Self::PostconditionFailed { operation, .. } => write!(
                formatter,
                "GitHub Stack {operation:?} returned without the exact requested postcondition"
            ),
        }
    }
}

impl std::error::Error for GitHubStackMutationError {}

impl From<MutationError> for GitHubStackMutationError {
    fn from(error: MutationError) -> Self {
        Self::Provider(error)
    }
}

#[derive(Debug, Deserialize)]
struct StackRefResponse {
    object: StackRefObject,
}

#[derive(Debug, Deserialize)]
struct StackRefObject {
    sha: String,
}

struct KnownStackMutation<'a> {
    repository: &'a RepositoryId,
    operation_id: &'a str,
    actor: &'a str,
    operation: GitHubStackMutationOperation,
    before: &'a GitHubStackGeneration,
    desired_after: Option<&'a GitHubStackTopology>,
    request: GitHubStackRequestIdentity,
    command: &'a CommandSpec,
}

struct StackReceiptInput<'a> {
    repository: &'a RepositoryId,
    operation_id: &'a str,
    actor: &'a str,
    operation: GitHubStackMutationOperation,
    disposition: GitHubStackMutationDisposition,
    request: GitHubStackRequestIdentity,
    before: Option<GitHubStackGeneration>,
    after: Option<GitHubStackGeneration>,
    provider_output: Option<String>,
}

impl<R: CommandRunner> GitHubMutationAdapter<R> {
    /// Read one Stack and bind its Stack identity, base SHA, ordered PR bases,
    /// and exact heads. A 404 is absence only after a successful capability
    /// probe; a feature-level 404 remains `Unavailable`.
    pub fn native_stack_generation(
        &self,
        repository: &RepositoryId,
        stack_number: u64,
    ) -> Result<Option<GitHubStackGeneration>, GitHubStackMutationError> {
        let snapshot = self.read_native_stack_snapshot(repository, stack_number)?;
        snapshot
            .map(|stack| self.observe_stack_generation(repository, &stack))
            .transpose()
    }

    /// Create a two-to-100 entry Stack, or prove an exact prior retry already
    /// created it. Any unsuccessful response is rediscovered before retry is
    /// left to the caller.
    pub fn native_stack_create(
        &self,
        repository: &RepositoryId,
        plan: &GitHubStackCreatePlan,
    ) -> Result<GitHubStackMutationReceipt, GitHubStackMutationError> {
        validate_operation_identity(&plan.operation_id, &plan.actor)?;
        validate_topology(repository, &plan.desired, 2)?;
        validate_open_entries(&plan.desired, 0)?;
        let inventory = self.exact_inventory(repository)?;
        if let Some(existing) =
            self.find_intersecting_generation(repository, &inventory, &plan.desired)?
        {
            if existing.topology == plan.desired {
                return Ok(stack_receipt(StackReceiptInput {
                    repository,
                    operation_id: &plan.operation_id,
                    actor: &plan.actor,
                    operation: GitHubStackMutationOperation::Create,
                    disposition: GitHubStackMutationDisposition::AlreadySatisfied,
                    request: create_request(repository, &plan.desired),
                    before: Some(existing.clone()),
                    after: Some(existing),
                    provider_output: None,
                }));
            }
            return Err(stale_topology(&plan.desired, Some(&existing)));
        }
        self.verify_topology_fresh(repository, &plan.desired)?;

        let command = native_stack_create_command(repository, &plan.desired);
        let response = self.runner.run(&command);
        let provider_output = response.as_ref().ok().and_then(bounded_provider_output);
        let request_id = response.as_ref().ok().and_then(github_request_id);
        let response_diagnostic = mutation_response_diagnostic(&response);
        let rediscovered = self.rediscover_topology(repository, &plan.desired);

        match (response, rediscovered) {
            (Ok(output), Ok(Some(after)))
                if output.is_success() && after.topology == plan.desired =>
            {
                let mut request = create_request(repository, &plan.desired);
                request.github_request_id = request_id;
                Ok(stack_receipt(StackReceiptInput {
                    repository,
                    operation_id: &plan.operation_id,
                    actor: &plan.actor,
                    operation: GitHubStackMutationOperation::Create,
                    disposition: GitHubStackMutationDisposition::Completed,
                    request,
                    before: None,
                    after: Some(after),
                    provider_output,
                }))
            }
            (Ok(output), Ok(Some(after)))
                if !output.is_success() && after.topology == plan.desired =>
            {
                let mut request = create_request(repository, &plan.desired);
                request.github_request_id = request_id;
                Ok(stack_receipt(StackReceiptInput {
                    repository,
                    operation_id: &plan.operation_id,
                    actor: &plan.actor,
                    operation: GitHubStackMutationOperation::Create,
                    disposition: GitHubStackMutationDisposition::RecoveredAfterAmbiguousResponse,
                    request,
                    before: None,
                    after: Some(after),
                    provider_output,
                }))
            }
            (Err(_), Ok(Some(after))) if after.topology == plan.desired => {
                Ok(stack_receipt(StackReceiptInput {
                    repository,
                    operation_id: &plan.operation_id,
                    actor: &plan.actor,
                    operation: GitHubStackMutationOperation::Create,
                    disposition: GitHubStackMutationDisposition::RecoveredAfterAmbiguousResponse,
                    request: create_request(repository, &plan.desired),
                    before: None,
                    after: Some(after),
                    provider_output,
                }))
            }
            (Ok(output), Ok(observed)) if output.is_success() => {
                Err(GitHubStackMutationError::PostconditionFailed {
                    operation: GitHubStackMutationOperation::Create,
                    expected: Some(Box::new(plan.desired.clone())),
                    actual: observed.map(Box::new),
                })
            }
            (_, rediscovery) => Err(ambiguous_error(
                GitHubStackMutationOperation::Create,
                response_diagnostic,
                rediscovery,
            )),
        }
    }

    /// Append only the exact desired suffix to one exact Stack generation.
    pub fn native_stack_add(
        &self,
        repository: &RepositoryId,
        plan: &GitHubStackAddPlan,
    ) -> Result<GitHubStackMutationReceipt, GitHubStackMutationError> {
        validate_operation_identity(&plan.operation_id, &plan.actor)?;
        validate_add_plan(repository, plan)?;
        let actual = self.native_stack_generation(repository, plan.before.number)?;
        if actual.as_ref().is_some_and(|generation| {
            same_stack_identity(generation, &plan.before) && generation.topology == plan.desired
        }) {
            let actual = actual.expect("checked Some");
            return Ok(stack_receipt(StackReceiptInput {
                repository,
                operation_id: &plan.operation_id,
                actor: &plan.actor,
                operation: GitHubStackMutationOperation::Add,
                disposition: GitHubStackMutationDisposition::AlreadySatisfied,
                request: add_request(repository, plan),
                before: Some(actual.clone()),
                after: Some(actual),
                provider_output: None,
            }));
        }
        let Some(before) = actual else {
            return Err(stale_topology(&plan.before.topology, None));
        };
        if before != plan.before {
            return Err(stale_generation(&plan.before, Some(&before)));
        }
        self.verify_topology_fresh(repository, &plan.desired)?;
        let lease_read = self.native_stack_generation(repository, plan.before.number)?;
        if lease_read.as_ref() != Some(&plan.before) {
            return Err(stale_generation(&plan.before, lease_read.as_ref()));
        }

        let command = native_stack_add_command(repository, plan);
        self.complete_known_stack_mutation(KnownStackMutation {
            repository,
            operation_id: &plan.operation_id,
            actor: &plan.actor,
            operation: GitHubStackMutationOperation::Add,
            before: &plan.before,
            desired_after: Some(&plan.desired),
            request: add_request(repository, plan),
            command: &command,
        })
    }

    /// Unstack one exact generation. This operation is resumable, not atomic
    /// with any later Cara reshape; its receipt says only what this REST call
    /// proved.
    pub fn native_stack_unstack(
        &self,
        repository: &RepositoryId,
        plan: &GitHubStackUnstackPlan,
    ) -> Result<GitHubStackMutationReceipt, GitHubStackMutationError> {
        validate_operation_identity(&plan.operation_id, &plan.actor)?;
        validate_topology(repository, &plan.before.topology, 1)?;
        if let Some(desired) = plan.desired_after.as_ref() {
            validate_topology(repository, desired, 1)?;
        }
        let actual = self.native_stack_generation(repository, plan.before.number)?;
        let already_satisfied = match (&actual, &plan.desired_after) {
            (None, None) => true,
            (Some(actual), Some(desired)) => {
                same_stack_identity(actual, &plan.before) && &actual.topology == desired
            }
            _ => false,
        };
        if already_satisfied {
            return Ok(stack_receipt(StackReceiptInput {
                repository,
                operation_id: &plan.operation_id,
                actor: &plan.actor,
                operation: GitHubStackMutationOperation::Unstack,
                disposition: GitHubStackMutationDisposition::AlreadySatisfied,
                request: unstack_request(repository, plan.before.number),
                before: actual.clone(),
                after: actual,
                provider_output: None,
            }));
        }
        let Some(before) = actual else {
            return Err(stale_topology(&plan.before.topology, None));
        };
        if before != plan.before {
            return Err(stale_generation(&plan.before, Some(&before)));
        }

        let command = native_stack_unstack_command(repository, plan.before.number);
        self.complete_known_stack_mutation(KnownStackMutation {
            repository,
            operation_id: &plan.operation_id,
            actor: &plan.actor,
            operation: GitHubStackMutationOperation::Unstack,
            before: &plan.before,
            desired_after: plan.desired_after.as_ref(),
            request: unstack_request(repository, plan.before.number),
            command: &command,
        })
    }

    fn complete_known_stack_mutation(
        &self,
        mut mutation: KnownStackMutation<'_>,
    ) -> Result<GitHubStackMutationReceipt, GitHubStackMutationError> {
        let response = self.runner.run(mutation.command);
        let provider_output = response.as_ref().ok().and_then(bounded_provider_output);
        mutation.request.github_request_id = response.as_ref().ok().and_then(github_request_id);
        let response_diagnostic = mutation_response_diagnostic(&response);
        let rediscovered =
            self.native_stack_generation(mutation.repository, mutation.before.number);
        let postcondition = |actual: &Option<GitHubStackGeneration>| match mutation.desired_after {
            Some(desired) => actual.as_ref().is_some_and(|generation| {
                same_stack_identity(generation, mutation.before) && &generation.topology == desired
            }),
            None => actual.is_none(),
        };

        match (response, rediscovered) {
            (Ok(output), Ok(after)) if output.is_success() && postcondition(&after) => {
                Ok(stack_receipt(StackReceiptInput {
                    repository: mutation.repository,
                    operation_id: mutation.operation_id,
                    actor: mutation.actor,
                    operation: mutation.operation,
                    disposition: GitHubStackMutationDisposition::Completed,
                    request: mutation.request,
                    before: Some(mutation.before.clone()),
                    after,
                    provider_output,
                }))
            }
            (Ok(output), Ok(after)) if !output.is_success() && postcondition(&after) => {
                Ok(stack_receipt(StackReceiptInput {
                    repository: mutation.repository,
                    operation_id: mutation.operation_id,
                    actor: mutation.actor,
                    operation: mutation.operation,
                    disposition: GitHubStackMutationDisposition::RecoveredAfterAmbiguousResponse,
                    request: mutation.request,
                    before: Some(mutation.before.clone()),
                    after,
                    provider_output,
                }))
            }
            (Err(_), Ok(after)) if postcondition(&after) => Ok(stack_receipt(StackReceiptInput {
                repository: mutation.repository,
                operation_id: mutation.operation_id,
                actor: mutation.actor,
                operation: mutation.operation,
                disposition: GitHubStackMutationDisposition::RecoveredAfterAmbiguousResponse,
                request: mutation.request,
                before: Some(mutation.before.clone()),
                after,
                provider_output,
            })),
            (Ok(output), Ok(after)) if output.is_success() => {
                Err(GitHubStackMutationError::PostconditionFailed {
                    operation: mutation.operation,
                    expected: mutation.desired_after.cloned().map(Box::new),
                    actual: after.map(Box::new),
                })
            }
            (_, rediscovery) => Err(ambiguous_error(
                mutation.operation,
                response_diagnostic,
                rediscovery,
            )),
        }
    }

    fn exact_inventory(
        &self,
        repository: &RepositoryId,
    ) -> Result<GitHubStackInventory, GitHubStackMutationError> {
        let inventory = self
            .native_stack_inventory(repository)
            .map_err(stack_read_error)?;
        if inventory.truncated {
            return Err(GitHubStackMutationError::InventoryTruncated);
        }
        Ok(inventory)
    }

    fn read_native_stack_snapshot(
        &self,
        repository: &RepositoryId,
        stack_number: u64,
    ) -> Result<Option<GitHubStackSnapshot>, GitHubStackMutationError> {
        let command = native_stack_read_command(repository, stack_number);
        let output = self.runner.run(&command).map_err(|error| {
            GitHubStackMutationError::CapabilityUnknown {
                diagnostic: error.to_string(),
            }
        })?;
        if output.is_success() {
            return serde_json::from_str(&output.stdout)
                .map(Some)
                .map_err(|error| GitHubStackMutationError::CapabilityUnknown {
                    diagnostic: format!("invalid Stack JSON: {error}"),
                });
        }
        if !output.stderr.contains("HTTP 404") {
            return Err(GitHubStackMutationError::CapabilityUnknown {
                diagnostic: format!(
                    "Stack read exited {:?}: {}",
                    output.code,
                    output.stderr.trim()
                ),
            });
        }
        // A known Stack number and a feature-level endpoint failure both use
        // 404. One bounded list probe separates absence from unavailability.
        match self.native_stack_inventory(repository) {
            Ok(inventory) => {
                if inventory
                    .stacks
                    .iter()
                    .any(|stack| stack.number == stack_number)
                {
                    Err(GitHubStackMutationError::InconsistentProviderState {
                        diagnostic: format!(
                            "Stack #{stack_number} returned 404 but remains present in inventory"
                        ),
                    })
                } else {
                    Ok(None)
                }
            }
            Err(error) => Err(stack_read_error(error)),
        }
    }

    fn observe_stack_generation(
        &self,
        repository: &RepositoryId,
        stack: &GitHubStackSnapshot,
    ) -> Result<GitHubStackGeneration, GitHubStackMutationError> {
        let base: StackRefResponse = self
            .json(native_stack_base_ref_command(
                repository,
                &stack.base.ref_name,
            ))
            .map_err(GitHubStackMutationError::Provider)?;
        let base = BranchSnapshot {
            repository: repository.clone(),
            name: stack.base.ref_name.clone(),
            oid: CommitOid(base.object.sha),
        };
        let mut entries = Vec::with_capacity(stack.pull_requests.len());
        for (position, stack_pr) in stack.pull_requests.iter().enumerate() {
            let pr = self.refetch_pull_request(repository, PrNumber(stack_pr.number))?;
            if pr.head.name != stack_pr.head.ref_name || pr.head.oid != stack_pr.head.sha {
                return Err(GitHubStackMutationError::InconsistentProviderState {
                    diagnostic: format!(
                        "Stack #{} entry PR #{} head {}@{} disagrees with fresh PR head {}@{}",
                        stack.number,
                        stack_pr.number,
                        stack_pr.head.ref_name,
                        stack_pr.head.sha,
                        pr.head.name,
                        pr.head.oid
                    ),
                });
            }
            if pr.draft != stack_pr.draft || pr.merged_at != stack_pr.merged_at {
                return Err(GitHubStackMutationError::InconsistentProviderState {
                    diagnostic: format!(
                        "Stack #{} entry PR #{} draft/merged state disagrees with fresh PR truth",
                        stack.number, stack_pr.number
                    ),
                });
            }
            if let Some(stack_state) = normalized_stack_pr_state(stack_pr) {
                if stack_state != pr.state {
                    return Err(GitHubStackMutationError::InconsistentProviderState {
                        diagnostic: format!(
                            "Stack #{} entry PR #{} state `{}` disagrees with fresh PR state {:?}",
                            stack.number, stack_pr.number, stack_pr.state, pr.state
                        ),
                    });
                }
            }
            entries.push(entry_from_pr(position, &stack_pr.state, pr)?);
        }
        let topology = GitHubStackTopology { base, entries };
        validate_topology(repository, &topology, 1)?;
        Ok(GitHubStackGeneration {
            id: stack.id,
            number: stack.number,
            node_id: stack.node_id.clone(),
            open: stack.open,
            created_at: stack.created_at.clone(),
            topology,
        })
    }

    fn verify_topology_fresh(
        &self,
        repository: &RepositoryId,
        expected: &GitHubStackTopology,
    ) -> Result<(), GitHubStackMutationError> {
        let base: StackRefResponse = self
            .json(native_stack_base_ref_command(
                repository,
                &expected.base.name,
            ))
            .map_err(GitHubStackMutationError::Provider)?;
        let mut actual = GitHubStackTopology {
            base: BranchSnapshot {
                repository: repository.clone(),
                name: expected.base.name.clone(),
                oid: CommitOid(base.object.sha),
            },
            entries: Vec::with_capacity(expected.entries.len()),
        };
        for (position, entry) in expected.entries.iter().enumerate() {
            let pr = self.refetch_pull_request(repository, entry.pr)?;
            actual
                .entries
                .push(entry_from_pr(position, &entry.stack_state, pr)?);
        }
        if &actual == expected {
            Ok(())
        } else {
            Err(GitHubStackMutationError::StaleGeneration {
                changed_fields: changed_topology_fields(expected, &actual),
                expected: Box::new(expected.clone()),
                actual: None,
            })
        }
    }

    fn find_intersecting_generation(
        &self,
        repository: &RepositoryId,
        inventory: &GitHubStackInventory,
        desired: &GitHubStackTopology,
    ) -> Result<Option<GitHubStackGeneration>, GitHubStackMutationError> {
        let desired_prs = desired
            .entries
            .iter()
            .map(|entry| entry.pr.0)
            .collect::<std::collections::BTreeSet<_>>();
        let candidates = inventory
            .stacks
            .iter()
            .filter(|stack| {
                stack
                    .pull_requests
                    .iter()
                    .any(|entry| desired_prs.contains(&entry.number))
            })
            .collect::<Vec<_>>();
        if candidates.len() > 1 {
            return Err(GitHubStackMutationError::InconsistentProviderState {
                diagnostic: format!(
                    "desired PRs intersect multiple provider Stacks: {}",
                    candidates
                        .iter()
                        .map(|stack| stack.number.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        candidates
            .first()
            .map(|stack| self.observe_stack_generation(repository, stack))
            .transpose()
    }

    fn rediscover_topology(
        &self,
        repository: &RepositoryId,
        desired: &GitHubStackTopology,
    ) -> Result<Option<GitHubStackGeneration>, GitHubStackMutationError> {
        let inventory = self.exact_inventory(repository)?;
        self.find_intersecting_generation(repository, &inventory, desired)
    }
}

fn entry_from_pr(
    position: usize,
    stack_state: &str,
    pr: PullRequestSnapshot,
) -> Result<GitHubStackEntryGeneration, GitHubStackMutationError> {
    Ok(GitHubStackEntryGeneration {
        position: u32::try_from(position).map_err(|_| GitHubStackMutationError::InvalidPlan {
            code: "github_stack_too_large".to_owned(),
            message: "Stack position exceeds u32".to_owned(),
        })?,
        pr: pr.number,
        stack_state: stack_state.to_owned(),
        pull_request_state: pr.state,
        draft: pr.draft,
        merged_at: pr.merged_at,
        base: pr.base,
        head: pr.head,
    })
}

fn normalized_stack_pr_state(
    pull_request: &super::GitHubStackPullRequest,
) -> Option<PullRequestState> {
    if pull_request.merged_at.is_some() || pull_request.state.eq_ignore_ascii_case("merged") {
        Some(PullRequestState::Merged)
    } else if pull_request.state.eq_ignore_ascii_case("open") {
        Some(PullRequestState::Open)
    } else if pull_request.state.eq_ignore_ascii_case("closed") {
        Some(PullRequestState::Closed)
    } else {
        // Preview-specific queued/merging states stay verbatim in the receipt;
        // the fresh canonical PR read still supplies PullRequestState.
        None
    }
}

fn validate_add_plan(
    repository: &RepositoryId,
    plan: &GitHubStackAddPlan,
) -> Result<(), GitHubStackMutationError> {
    validate_topology(repository, &plan.before.topology, 2)?;
    validate_topology(repository, &plan.desired, 2)?;
    if plan.desired.entries.len() <= plan.before.topology.entries.len() {
        return Err(invalid_plan(
            "github_stack_add_empty",
            "Stack add requires at least one new top entry",
        ));
    }
    if plan.desired.base != plan.before.topology.base
        || !plan
            .desired
            .entries
            .starts_with(&plan.before.topology.entries)
    {
        return Err(invalid_plan(
            "github_stack_add_not_append_only",
            "Stack add may append only an exact top suffix",
        ));
    }
    validate_open_entries(&plan.desired, plan.before.topology.entries.len())
}

fn validate_topology(
    repository: &RepositoryId,
    topology: &GitHubStackTopology,
    minimum_entries: usize,
) -> Result<(), GitHubStackMutationError> {
    if topology.entries.len() < minimum_entries || topology.entries.len() > 100 {
        return Err(invalid_plan(
            "github_stack_size_invalid",
            &format!(
                "Stack requires {minimum_entries}..=100 entries, observed {}",
                topology.entries.len()
            ),
        ));
    }
    if topology.base.repository != *repository {
        return Err(invalid_plan(
            "github_stack_repository_mismatch",
            "Stack base belongs to another repository",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut previous_open_head: Option<&BranchSnapshot> = None;
    for (position, entry) in topology.entries.iter().enumerate() {
        if entry.position != u32::try_from(position).unwrap_or(u32::MAX) {
            return Err(invalid_plan(
                "github_stack_position_invalid",
                "Stack positions must be contiguous and zero-based",
            ));
        }
        if !seen.insert(entry.pr) {
            return Err(invalid_plan(
                "github_stack_duplicate_pr",
                "A pull request may appear only once in a Stack",
            ));
        }
        if entry.base.repository != *repository || entry.head.repository != *repository {
            return Err(invalid_plan(
                "github_stack_repository_mismatch",
                "Stack entries must use same-repository base and head branches",
            ));
        }
        if entry.pull_request_state == PullRequestState::Open {
            // After an atomic prefix merge GitHub retains the merged entries in
            // the Stack but rebases/retargets the first remaining open PR to
            // the Stack base. Subsequent open entries continue the live chain.
            let expected_base = previous_open_head.unwrap_or(&topology.base);
            if entry.base != *expected_base {
                return Err(invalid_plan(
                    "github_stack_base_chain_invalid",
                    &format!(
                        "open PR #{} does not target the exact previous open generation",
                        entry.pr
                    ),
                ));
            }
            previous_open_head = Some(&entry.head);
        } else if previous_open_head.is_some() {
            return Err(invalid_plan(
                "github_stack_state_order_invalid",
                "closed or merged Stack entries may only precede the open suffix",
            ));
        }
    }
    Ok(())
}

fn validate_open_entries(
    topology: &GitHubStackTopology,
    from: usize,
) -> Result<(), GitHubStackMutationError> {
    for entry in topology.entries.iter().skip(from) {
        if entry.pull_request_state != PullRequestState::Open
            || entry.draft
            || !entry.stack_state.eq_ignore_ascii_case("open")
        {
            return Err(invalid_plan(
                "github_stack_entry_ineligible",
                &format!(
                    "PR #{} must be open, non-draft, and requested in open Stack state",
                    entry.pr
                ),
            ));
        }
    }
    Ok(())
}

fn validate_operation_identity(
    operation_id: &str,
    actor: &str,
) -> Result<(), GitHubStackMutationError> {
    if operation_id.trim().is_empty() {
        return Err(invalid_plan(
            "github_stack_operation_id_missing",
            "Stack mutation requires a non-empty operation identity",
        ));
    }
    if actor.trim().is_empty() {
        return Err(invalid_plan(
            "github_stack_actor_missing",
            "Stack mutation requires a non-empty actor",
        ));
    }
    Ok(())
}

fn same_stack_identity(left: &GitHubStackGeneration, right: &GitHubStackGeneration) -> bool {
    left.id == right.id
        && left.number == right.number
        && left.node_id == right.node_id
        && left.created_at == right.created_at
}

fn changed_topology_fields(
    expected: &GitHubStackTopology,
    actual: &GitHubStackTopology,
) -> Vec<String> {
    let mut changed = Vec::new();
    if expected.base != actual.base {
        changed.push("base".to_owned());
    }
    if expected.entries.len() != actual.entries.len() {
        changed.push("entry_count".to_owned());
    }
    for (index, expected_entry) in expected.entries.iter().enumerate() {
        let Some(actual_entry) = actual.entries.get(index) else {
            break;
        };
        if expected_entry.pr != actual_entry.pr {
            changed.push(format!("entries[{index}].pr"));
        }
        if expected_entry.head != actual_entry.head {
            changed.push(format!("entries[{index}].head"));
        }
        if expected_entry.base != actual_entry.base {
            changed.push(format!("entries[{index}].base"));
        }
        if expected_entry.pull_request_state != actual_entry.pull_request_state
            || expected_entry.draft != actual_entry.draft
            || expected_entry.stack_state != actual_entry.stack_state
        {
            changed.push(format!("entries[{index}].state"));
        }
    }
    if changed.is_empty() {
        changed.push("stack_identity".to_owned());
    }
    changed
}

fn stale_topology(
    expected: &GitHubStackTopology,
    actual: Option<&GitHubStackGeneration>,
) -> GitHubStackMutationError {
    let changed_fields = actual.map_or_else(
        || vec!["stack_absent".to_owned()],
        |actual| changed_topology_fields(expected, &actual.topology),
    );
    GitHubStackMutationError::StaleGeneration {
        expected: Box::new(expected.clone()),
        actual: actual.cloned().map(Box::new),
        changed_fields,
    }
}

fn stale_generation(
    expected: &GitHubStackGeneration,
    actual: Option<&GitHubStackGeneration>,
) -> GitHubStackMutationError {
    let mut error = stale_topology(&expected.topology, actual);
    if let GitHubStackMutationError::StaleGeneration { changed_fields, .. } = &mut error {
        if actual.is_some_and(|actual| !same_stack_identity(actual, expected)) {
            changed_fields.push("stack_identity".to_owned());
        }
    }
    error
}

fn invalid_plan(code: &str, message: &str) -> GitHubStackMutationError {
    GitHubStackMutationError::InvalidPlan {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

fn stack_read_error(error: GitHubStackReadError) -> GitHubStackMutationError {
    match error {
        GitHubStackReadError::Unavailable { diagnostic } => {
            GitHubStackMutationError::Unavailable { diagnostic }
        }
        other => GitHubStackMutationError::CapabilityUnknown {
            diagnostic: other.to_string(),
        },
    }
}

fn mutation_response_diagnostic(response: &Result<CommandOutput, CommandRunError>) -> String {
    match response {
        Ok(output) => format!(
            "provider exited {:?}: {}",
            output.code,
            if output.stderr.trim().is_empty() {
                "no diagnostic"
            } else {
                output.stderr.trim()
            }
        ),
        Err(error) => error.to_string(),
    }
}

fn ambiguous_error(
    operation: GitHubStackMutationOperation,
    diagnostic: String,
    rediscovery: Result<Option<GitHubStackGeneration>, GitHubStackMutationError>,
) -> GitHubStackMutationError {
    match rediscovery {
        Ok(observed) => GitHubStackMutationError::AmbiguousResponse {
            operation,
            diagnostic,
            rediscovery_diagnostic: None,
            observed: observed.map(Box::new),
        },
        Err(error) => GitHubStackMutationError::AmbiguousResponse {
            operation,
            diagnostic,
            rediscovery_diagnostic: Some(error.to_string()),
            observed: None,
        },
    }
}

fn bounded_provider_output(output: &CommandOutput) -> Option<String> {
    let value = output.stdout.trim();
    (!value.is_empty()).then(|| super::diagnostic_excerpt(value))
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

fn stack_receipt(input: StackReceiptInput<'_>) -> GitHubStackMutationReceipt {
    GitHubStackMutationReceipt {
        schema_version: STACK_SCHEMA_VERSION,
        operation_id: input.operation_id.to_owned(),
        operation: input.operation,
        repository: input.repository.clone(),
        actor: input.actor.to_owned(),
        disposition: input.disposition,
        request: input.request,
        before: input.before,
        after: input.after,
        provider_output: input.provider_output,
        evidence_hash: String::new(),
    }
    .seal()
}

fn create_request(
    repository: &RepositoryId,
    topology: &GitHubStackTopology,
) -> GitHubStackRequestIdentity {
    GitHubStackRequestIdentity {
        method: "POST".to_owned(),
        path: format!("repos/{}/stacks", repository.slug()),
        ordered_pull_requests: topology.entries.iter().map(|entry| entry.pr).collect(),
        github_request_id: None,
    }
}

fn add_request(repository: &RepositoryId, plan: &GitHubStackAddPlan) -> GitHubStackRequestIdentity {
    GitHubStackRequestIdentity {
        method: "POST".to_owned(),
        path: format!(
            "repos/{}/stacks/{}/add",
            repository.slug(),
            plan.before.number
        ),
        ordered_pull_requests: plan.desired.entries[plan.before.topology.entries.len()..]
            .iter()
            .map(|entry| entry.pr)
            .collect(),
        github_request_id: None,
    }
}

fn unstack_request(repository: &RepositoryId, stack_number: u64) -> GitHubStackRequestIdentity {
    GitHubStackRequestIdentity {
        method: "POST".to_owned(),
        path: format!("repos/{}/stacks/{stack_number}/unstack", repository.slug()),
        ordered_pull_requests: Vec::new(),
        github_request_id: None,
    }
}

pub(super) fn native_stack_read_command(
    repository: &RepositoryId,
    stack_number: u64,
) -> CommandSpec {
    stack_api_command(
        "GET",
        format!("repos/{}/stacks/{stack_number}", repository.slug()),
        false,
    )
}

pub(super) fn native_stack_base_ref_command(
    repository: &RepositoryId,
    branch: &str,
) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        format!(
            "repos/{}/git/ref/heads/{}",
            repository.slug(),
            super::encode_path_segment(branch)
        ),
    ])
}

fn native_stack_create_command(
    repository: &RepositoryId,
    topology: &GitHubStackTopology,
) -> CommandSpec {
    let pull_requests = topology
        .entries
        .iter()
        .map(|entry| entry.pr.0)
        .collect::<Vec<_>>();
    stack_api_command("POST", format!("repos/{}/stacks", repository.slug()), true)
        .args(["--input", "-"])
        .stdin(serde_json::json!({"pull_requests": pull_requests}).to_string())
}

fn native_stack_add_command(repository: &RepositoryId, plan: &GitHubStackAddPlan) -> CommandSpec {
    let pull_requests = plan.desired.entries[plan.before.topology.entries.len()..]
        .iter()
        .map(|entry| entry.pr.0)
        .collect::<Vec<_>>();
    stack_api_command(
        "POST",
        format!(
            "repos/{}/stacks/{}/add",
            repository.slug(),
            plan.before.number
        ),
        true,
    )
    .args(["--input", "-"])
    .stdin(serde_json::json!({"pull_requests": pull_requests}).to_string())
}

fn native_stack_unstack_command(repository: &RepositoryId, stack_number: u64) -> CommandSpec {
    stack_api_command(
        "POST",
        format!("repos/{}/stacks/{stack_number}/unstack", repository.slug()),
        true,
    )
}

fn stack_api_command(method: &str, path: String, include_headers: bool) -> CommandSpec {
    let mut command = CommandSpec::new("gh").args([
        "api".to_owned(),
        "--method".to_owned(),
        method.to_owned(),
        "-H".to_owned(),
        STACK_ACCEPT.to_owned(),
        "-H".to_owned(),
        STACK_API_VERSION.to_owned(),
    ]);
    if include_headers {
        command = command.arg("--include");
    }
    let command = command.arg(path);
    if method == "GET" {
        command
    } else {
        command.provider_write()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;
    use crate::command::CommandRunError;

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
            let (expected, output) = self
                .calls
                .borrow_mut()
                .pop_front()
                .expect("unexpected provider command");
            assert_eq!(expected, *command);
            output
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

    fn topology(entry_count: usize) -> GitHubStackTopology {
        let names = ["root", "child", "tail"];
        let oids = ["aaa111", "bbb222", "ccc333"];
        let mut entries: Vec<GitHubStackEntryGeneration> = Vec::new();
        let base = branch("main", "base000");
        for position in 0..entry_count {
            let entry_base = if position == 0 {
                base.clone()
            } else {
                entries[position - 1].head.clone()
            };
            entries.push(GitHubStackEntryGeneration {
                position: u32::try_from(position).unwrap(),
                pr: PrNumber(101 + u64::try_from(position).unwrap()),
                stack_state: "open".to_owned(),
                pull_request_state: PullRequestState::Open,
                draft: false,
                merged_at: None,
                base: entry_base,
                head: branch(names[position], oids[position]),
            });
        }
        GitHubStackTopology { base, entries }
    }

    fn generation(topology: GitHubStackTopology) -> GitHubStackGeneration {
        GitHubStackGeneration {
            id: 9_876_543,
            number: 42,
            node_id: "S_stack".to_owned(),
            open: true,
            created_at: "2026-07-31T10:00:00Z".to_owned(),
            topology,
        }
    }

    fn stack_snapshot(topology: &GitHubStackTopology) -> GitHubStackSnapshot {
        GitHubStackSnapshot {
            id: 9_876_543,
            number: 42,
            node_id: "S_stack".to_owned(),
            base: super::super::GitHubStackBase {
                ref_name: topology.base.name.clone(),
            },
            open: true,
            created_at: "2026-07-31T10:00:00Z".to_owned(),
            pull_requests: topology
                .entries
                .iter()
                .map(|entry| super::super::GitHubStackPullRequest {
                    number: entry.pr.0,
                    state: entry.stack_state.clone(),
                    draft: entry.draft,
                    merged_at: entry.merged_at.clone(),
                    head: super::super::GitHubStackPullRequestHead {
                        ref_name: entry.head.name.clone(),
                        sha: entry.head.oid.clone(),
                    },
                })
                .collect(),
        }
    }

    fn stack_json(topology: &GitHubStackTopology) -> String {
        serde_json::to_string(&stack_snapshot(topology)).unwrap()
    }

    fn inventory_json(topologies: &[GitHubStackTopology]) -> String {
        serde_json::to_string(&topologies.iter().map(stack_snapshot).collect::<Vec<_>>()).unwrap()
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

    fn inventory_call(topologies: &[GitHubStackTopology]) -> (CommandSpec, CommandOutput) {
        (
            super::super::native_stack_list_command(&repository()),
            CommandOutput::success(inventory_json(topologies)),
        )
    }

    fn generation_observation_calls(
        topology: &GitHubStackTopology,
    ) -> Vec<(CommandSpec, CommandOutput)> {
        let mut calls = vec![(
            native_stack_base_ref_command(&repository(), &topology.base.name),
            CommandOutput::success(
                serde_json::json!({"object": {"sha": topology.base.oid.0}}).to_string(),
            ),
        )];
        calls.extend(topology.entries.iter().map(|entry| {
            (
                super::super::pull_request_command(&repository(), &entry.pr.to_string()),
                CommandOutput::success(pull_request_json(entry)),
            )
        }));
        calls
    }

    fn direct_generation_calls(
        topology: &GitHubStackTopology,
    ) -> Vec<(CommandSpec, CommandOutput)> {
        let mut calls = vec![(
            native_stack_read_command(&repository(), 42),
            CommandOutput::success(stack_json(topology)),
        )];
        calls.extend(generation_observation_calls(topology));
        calls
    }

    fn fresh_topology_calls(topology: &GitHubStackTopology) -> Vec<(CommandSpec, CommandOutput)> {
        generation_observation_calls(topology)
    }

    fn success_with_request_id() -> CommandOutput {
        CommandOutput::success(
            "HTTP/2 201 Created\nx-github-request-id: REQ-123\ncontent-type: application/json\n\n{}",
        )
    }

    fn successful_results(
        calls: Vec<(CommandSpec, CommandOutput)>,
    ) -> Vec<(CommandSpec, Result<CommandOutput, CommandRunError>)> {
        calls
            .into_iter()
            .map(|(command, output)| (command, Ok(output)))
            .collect()
    }

    #[test]
    fn direct_read_binds_stack_base_and_every_pr_base_and_head() {
        let expected = topology(2);
        let runner = FakeRunner::new(direct_generation_calls(&expected));
        let adapter = GitHubMutationAdapter::new(runner);

        let observed = adapter
            .native_stack_generation(&repository(), 42)
            .unwrap()
            .unwrap();

        assert_eq!(observed, generation(expected));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn create_posts_only_documented_rest_body_and_seals_exact_postcondition() {
        let desired = topology(2);
        let mut calls = vec![inventory_call(&[])];
        calls.extend(fresh_topology_calls(&desired));
        calls.push((
            native_stack_create_command(&repository(), &desired),
            success_with_request_id(),
        ));
        calls.push(inventory_call(std::slice::from_ref(&desired)));
        calls.extend(generation_observation_calls(&desired));
        let runner = FakeRunner::new(calls);
        let adapter = GitHubMutationAdapter::new(runner);
        let plan = GitHubStackCreatePlan {
            operation_id: "op-create".to_owned(),
            actor: "cara".to_owned(),
            desired: desired.clone(),
        };

        let receipt = adapter.native_stack_create(&repository(), &plan).unwrap();

        assert_eq!(
            receipt.disposition,
            GitHubStackMutationDisposition::Completed
        );
        assert_eq!(
            receipt.request.ordered_pull_requests,
            [PrNumber(101), PrNumber(102)]
        );
        assert_eq!(
            receipt.request.github_request_id.as_deref(),
            Some("REQ-123")
        );
        assert_eq!(receipt.after.as_ref().unwrap(), &generation(desired));
        assert!(receipt.verify());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn create_rediscovery_recovers_an_ambiguous_provider_response() {
        let desired = topology(2);
        let mut calls = vec![inventory_call(&[])];
        calls.extend(fresh_topology_calls(&desired));
        calls.push((
            native_stack_create_command(&repository(), &desired),
            CommandOutput::failure(1, "gh: upstream response lost (HTTP 502)"),
        ));
        calls.push(inventory_call(std::slice::from_ref(&desired)));
        calls.extend(generation_observation_calls(&desired));
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));
        let plan = GitHubStackCreatePlan {
            operation_id: "op-create-recover".to_owned(),
            actor: "cara".to_owned(),
            desired,
        };

        let receipt = adapter.native_stack_create(&repository(), &plan).unwrap();

        assert_eq!(
            receipt.disposition,
            GitHubStackMutationDisposition::RecoveredAfterAmbiguousResponse
        );
        assert!(receipt.verify());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn create_rediscovery_recovers_a_transport_timeout() {
        let desired = topology(2);
        let mut calls = successful_results(vec![inventory_call(&[])]);
        calls.extend(successful_results(fresh_topology_calls(&desired)));
        let command = native_stack_create_command(&repository(), &desired);
        calls.push((
            command.clone(),
            Err(CommandRunError::Timeout {
                command,
                process_group_id: Some(1234),
                timeout_ms: 30_000,
                stdout: String::new(),
                stderr: String::new(),
            }),
        ));
        calls.extend(successful_results(vec![inventory_call(
            std::slice::from_ref(&desired),
        )]));
        calls.extend(successful_results(generation_observation_calls(&desired)));
        let adapter = GitHubMutationAdapter::new(FakeRunner::with_results(calls));
        let plan = GitHubStackCreatePlan {
            operation_id: "op-create-timeout".to_owned(),
            actor: "cara".to_owned(),
            desired,
        };

        let receipt = adapter.native_stack_create(&repository(), &plan).unwrap();

        assert_eq!(
            receipt.disposition,
            GitHubStackMutationDisposition::RecoveredAfterAmbiguousResponse
        );
        assert!(receipt.verify());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn exact_create_retry_is_zero_write() {
        let desired = topology(2);
        let mut calls = vec![inventory_call(std::slice::from_ref(&desired))];
        calls.extend(generation_observation_calls(&desired));
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));
        let plan = GitHubStackCreatePlan {
            operation_id: "op-create-retry".to_owned(),
            actor: "cara".to_owned(),
            desired,
        };

        let receipt = adapter.native_stack_create(&repository(), &plan).unwrap();

        assert_eq!(
            receipt.disposition,
            GitHubStackMutationDisposition::AlreadySatisfied
        );
        assert_eq!(receipt.before, receipt.after);
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn capability_404_and_rate_failure_are_distinct_and_zero_write() {
        let unavailable = GitHubMutationAdapter::new(FakeRunner::new(vec![(
            super::super::native_stack_list_command(&repository()),
            CommandOutput::failure(1, "gh: Not Found (HTTP 404)"),
        )]));
        let plan = GitHubStackCreatePlan {
            operation_id: "op".to_owned(),
            actor: "cara".to_owned(),
            desired: topology(2),
        };
        assert!(matches!(
            unavailable.native_stack_create(&repository(), &plan),
            Err(GitHubStackMutationError::Unavailable { .. })
        ));
        unavailable.runner.assert_exhausted();

        let unknown = GitHubMutationAdapter::new(FakeRunner::new(vec![(
            super::super::native_stack_list_command(&repository()),
            CommandOutput::failure(1, "gh: rate limit exceeded (HTTP 403)"),
        )]));
        assert!(matches!(
            unknown.native_stack_create(&repository(), &plan),
            Err(GitHubStackMutationError::CapabilityUnknown { .. })
        ));
        unknown.runner.assert_exhausted();
    }

    #[test]
    fn add_requires_exact_before_rechecks_lease_and_appends_only_suffix() {
        let before_topology = topology(2);
        let desired = topology(3);
        let before = generation(before_topology.clone());
        let plan = GitHubStackAddPlan {
            operation_id: "op-add".to_owned(),
            actor: "cara".to_owned(),
            before: before.clone(),
            desired: desired.clone(),
        };
        let mut calls = direct_generation_calls(&before_topology);
        calls.extend(fresh_topology_calls(&desired));
        calls.extend(direct_generation_calls(&before_topology));
        calls.push((
            native_stack_add_command(&repository(), &plan),
            success_with_request_id(),
        ));
        calls.extend(direct_generation_calls(&desired));
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));

        let receipt = adapter.native_stack_add(&repository(), &plan).unwrap();

        assert_eq!(
            receipt.disposition,
            GitHubStackMutationDisposition::Completed
        );
        assert_eq!(receipt.before, Some(before));
        assert_eq!(receipt.after, Some(generation(desired)));
        assert_eq!(receipt.request.ordered_pull_requests, [PrNumber(103)]);
        assert!(receipt.verify());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn exact_add_retry_is_zero_write_on_the_desired_generation() {
        let before_topology = topology(2);
        let desired = topology(3);
        let calls = direct_generation_calls(&desired);
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));
        let plan = GitHubStackAddPlan {
            operation_id: "op-add-retry".to_owned(),
            actor: "cara".to_owned(),
            before: generation(before_topology),
            desired: desired.clone(),
        };

        let receipt = adapter.native_stack_add(&repository(), &plan).unwrap();

        assert_eq!(
            receipt.disposition,
            GitHubStackMutationDisposition::AlreadySatisfied
        );
        assert_eq!(receipt.after, Some(generation(desired)));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn add_refuses_non_append_plan_before_provider_access() {
        let before_topology = topology(2);
        let mut desired = topology(3);
        desired.entries.swap(0, 1);
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(Vec::new()));
        let error = adapter
            .native_stack_add(
                &repository(),
                &GitHubStackAddPlan {
                    operation_id: "op-add-invalid".to_owned(),
                    actor: "cara".to_owned(),
                    before: generation(before_topology),
                    desired,
                },
            )
            .unwrap_err();

        assert!(matches!(
            error,
            GitHubStackMutationError::InvalidPlan { .. }
        ));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn unstack_recovers_lost_response_only_after_absence_and_capability_proof() {
        let before_topology = topology(2);
        let before = generation(before_topology.clone());
        let plan = GitHubStackUnstackPlan {
            operation_id: "op-unstack".to_owned(),
            actor: "cara".to_owned(),
            before: before.clone(),
            desired_after: None,
        };
        let mut calls = direct_generation_calls(&before_topology);
        calls.push((
            native_stack_unstack_command(&repository(), 42),
            CommandOutput::failure(1, "gh: response lost (HTTP 502)"),
        ));
        calls.push((
            native_stack_read_command(&repository(), 42),
            CommandOutput::failure(1, "gh: Not Found (HTTP 404)"),
        ));
        calls.push(inventory_call(&[]));
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));

        let receipt = adapter.native_stack_unstack(&repository(), &plan).unwrap();

        assert_eq!(
            receipt.disposition,
            GitHubStackMutationDisposition::RecoveredAfterAmbiguousResponse
        );
        assert_eq!(receipt.before, Some(before));
        assert_eq!(receipt.after, None);
        assert!(receipt.verify());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn exact_unstack_retry_proves_absence_without_an_unstack_write() {
        let before = generation(topology(2));
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(vec![
            (
                native_stack_read_command(&repository(), 42),
                CommandOutput::failure(1, "gh: Not Found (HTTP 404)"),
            ),
            inventory_call(&[]),
        ]));
        let plan = GitHubStackUnstackPlan {
            operation_id: "op-unstack-retry".to_owned(),
            actor: "cara".to_owned(),
            before,
            desired_after: None,
        };

        let receipt = adapter.native_stack_unstack(&repository(), &plan).unwrap();

        assert_eq!(
            receipt.disposition,
            GitHubStackMutationDisposition::AlreadySatisfied
        );
        assert!(receipt.before.is_none());
        assert!(receipt.after.is_none());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn direct_404_is_not_absence_when_the_capability_probe_is_unavailable() {
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(vec![
            (
                native_stack_read_command(&repository(), 42),
                CommandOutput::failure(1, "gh: Not Found (HTTP 404)"),
            ),
            (
                super::super::native_stack_list_command(&repository()),
                CommandOutput::failure(1, "gh: Not Found (HTTP 404)"),
            ),
        ]));

        assert!(matches!(
            adapter.native_stack_generation(&repository(), 42),
            Err(GitHubStackMutationError::Unavailable { .. })
        ));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn stack_write_methods_are_marked_and_get_remains_read_only() {
        let write = stack_api_command("POST", "repos/o/r/stacks".to_owned(), true);
        assert_eq!(write.intent(), crate::command::CommandIntent::ProviderWrite);
        assert_eq!(write.inferred_write_intent(), Some(write.intent()));
        let read = stack_api_command("GET", "repos/o/r/stacks/1".to_owned(), false);
        assert_eq!(read.intent(), crate::command::CommandIntent::Read);
        assert_eq!(read.inferred_write_intent(), None);
    }

    #[test]
    fn stack_commands_are_rest_only_and_never_invoke_local_gh_stack_state() {
        let desired = topology(2);
        let create = native_stack_create_command(&repository(), &desired);
        let before = generation(desired.clone());
        let mut desired_add = topology(3);
        desired_add.entries[..2].clone_from_slice(&desired.entries);
        let add_plan = GitHubStackAddPlan {
            operation_id: "op".to_owned(),
            actor: "cara".to_owned(),
            before,
            desired: desired_add,
        };
        let commands = [
            create,
            native_stack_add_command(&repository(), &add_plan),
            native_stack_unstack_command(&repository(), 42),
        ];

        for command in commands {
            assert_eq!(command.program, "gh");
            assert_eq!(command.args.first().map(String::as_str), Some("api"));
            assert!(!command.args.iter().any(|argument| argument == "stack"));
            assert!(
                command
                    .args
                    .iter()
                    .any(|argument| argument == STACK_API_VERSION)
            );
        }
    }
}
