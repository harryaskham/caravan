//! Resumable native-Stack rebuild transaction for Cara reshape operations.
//!
//! GitHub exposes no arbitrary remove or reorder operation. Cara therefore
//! preflights one exact unqueued Stack generation and every replacement chain,
//! un-stacks it, runs the existing Cara reshape policy, and recreates only the
//! replacement chains that contain at least two PRs. Each boundary is a sealed
//! checkpoint. This transaction is deliberately not described as provider
//! atomic: retries converge from fresh provider truth one phase at a time.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::stack::{
    validate_open_entries, validate_operation_identity, validate_topology,
    validate_topology_allowing_stale_tail_base,
};
use super::{
    GitHubMutationAdapter, GitHubStackCreatePlan, GitHubStackGeneration, GitHubStackMutationError,
    GitHubStackMutationOperation, GitHubStackMutationReceipt, GitHubStackReadError,
    GitHubStackTopology, GitHubStackUnstackPlan, MutationError,
};
use crate::command::{CommandRunError, CommandRunner};
use crate::model::{
    BranchSnapshot, OperationReceipt, PrNumber, PullRequestSnapshot, PullRequestState, RepositoryId,
};

const SCHEMA_VERSION: u32 = 1;
const ACTIVE_LABEL: &str = "caravan";
const EVICTED_LABEL: &str = "caravan-evicted";
const FORCE_LABEL: &str = "caravan-force";
const MAX_REPLACEMENT_CHAINS: usize = 2;

/// Existing Cara reshape rule whose provider Stack is being rebuilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitHubStackReshapeOperation {
    Evict,
    Split,
}

impl GitHubStackReshapeOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Evict => "evict",
            Self::Split => "split",
        }
    }
}

/// Exact provider postcondition that the existing Cara reshape must establish.
///
/// Labels are intentionally expressed as required/forbidden control labels so
/// unrelated user labels and durable priority metadata remain untouched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackReshapePrPostcondition {
    pub pr: PrNumber,
    pub head: BranchSnapshot,
    pub base: BranchSnapshot,
    pub required_labels: BTreeSet<String>,
    pub forbidden_labels: BTreeSet<String>,
    pub auto_merge_enabled: bool,
}

/// Complete no-write replacement plan for one exact native Stack generation.
///
/// `replacement_chains` includes singletons so absence of a provider Stack for
/// those chains is part of final proof. Only chains of length two or greater
/// are sent to the Stack create endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackReshapePlan {
    pub schema_version: u32,
    pub repository: RepositoryId,
    pub operation_id: String,
    pub actor: String,
    pub operation: GitHubStackReshapeOperation,
    pub selected_pr: PrNumber,
    pub before: GitHubStackGeneration,
    pub replacement_chains: Vec<GitHubStackTopology>,
    pub pr_postconditions: Vec<GitHubStackReshapePrPostcondition>,
    pub plan_hash: String,
}

impl GitHubStackReshapePlan {
    /// Build and seal a complete replacement plan. This performs no provider
    /// access; `native_stack_reshape_preflight` binds it to fresh provider truth.
    pub fn new(
        repository: RepositoryId,
        operation_id: impl Into<String>,
        actor: impl Into<String>,
        operation: GitHubStackReshapeOperation,
        selected_pr: PrNumber,
        before: GitHubStackGeneration,
        replacement_chains: Vec<GitHubStackTopology>,
    ) -> Result<Self, GitHubStackReshapeError> {
        let pr_postconditions =
            derive_postconditions(operation, selected_pr, &before, &replacement_chains)?;
        let mut plan = Self {
            schema_version: SCHEMA_VERSION,
            repository,
            operation_id: operation_id.into(),
            actor: actor.into(),
            operation,
            selected_pr,
            before,
            replacement_chains,
            pr_postconditions,
            plan_hash: String::new(),
        };
        validate_plan_semantics(&plan)?;
        plan.plan_hash = plan_material_hash(&plan);
        Ok(plan)
    }

    #[must_use]
    pub fn verify(&self) -> bool {
        self.schema_version == SCHEMA_VERSION
            && !self.plan_hash.is_empty()
            && self.plan_hash == plan_material_hash(self)
            && validate_plan_semantics(self).is_ok()
    }
}

/// Durable non-atomic transaction phase. The names intentionally expose the
/// provider gap between unstack and replacement creation.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum GitHubStackReshapePhase {
    Preflighted,
    Unstacked,
    ReshapeApplied,
    Rebuilding,
    Rebuilt,
    Verified,
}

/// Self-contained resumable receipt for every completed reshape phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackReshapeCheckpoint {
    pub schema_version: u32,
    pub repository: RepositoryId,
    pub plan: GitHubStackReshapePlan,
    pub phase: GitHubStackReshapePhase,
    /// Always false. GitHub does not atomically couple unstack, Cara reshape,
    /// and replacement Stack creation.
    pub provider_atomic: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unstack_receipt: Option<GitHubStackMutationReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reshape_receipt: Option<OperationReceipt>,
    #[serde(default)]
    pub replacement_receipts: Vec<GitHubStackMutationReceipt>,
    #[serde(default)]
    pub final_stacks: Vec<GitHubStackGeneration>,
    pub evidence_hash: String,
}

impl GitHubStackReshapeCheckpoint {
    fn preflighted(plan: GitHubStackReshapePlan) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            repository: plan.repository.clone(),
            plan,
            phase: GitHubStackReshapePhase::Preflighted,
            provider_atomic: false,
            unstack_receipt: None,
            reshape_receipt: None,
            replacement_receipts: Vec::new(),
            final_stacks: Vec::new(),
            evidence_hash: String::new(),
        }
        .seal()
    }

    fn seal(mut self) -> Self {
        self.evidence_hash.clear();
        let material = serde_json::to_vec(&self).expect("Stack reshape checkpoint serializes");
        self.evidence_hash = crate::membership::fnv1a64(&material);
        self
    }

    #[must_use]
    pub fn verify(&self) -> bool {
        let expected = self.evidence_hash.clone();
        let mut material = self.clone();
        material.evidence_hash.clear();
        self.schema_version == SCHEMA_VERSION
            && !self.provider_atomic
            && self.repository == self.plan.repository
            && self.plan.verify()
            && serde_json::to_vec(&material)
                .ok()
                .is_some_and(|bytes| crate::membership::fnv1a64(&bytes) == expected)
            && validate_checkpoint_shape(self).is_ok()
    }
}

/// Typed refusal from the phased native Stack reshape transaction.
#[derive(Debug)]
pub enum GitHubStackReshapeError {
    InvalidPlan {
        code: String,
        message: String,
    },
    InvalidCheckpoint {
        diagnostic: String,
    },
    StaleGeneration {
        expected: Box<GitHubStackGeneration>,
        actual: Option<Box<GitHubStackGeneration>>,
    },
    ExistingReshapeIncomplete {
        pr: PrNumber,
        changed_fields: Vec<String>,
    },
    FinalTopologyMismatch {
        diagnostic: String,
    },
    StackMutation(GitHubStackMutationError),
    StackRead(GitHubStackReadError),
    Provider(MutationError),
}

impl std::fmt::Display for GitHubStackReshapeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPlan { code, message } => write!(formatter, "{code}: {message}"),
            Self::InvalidCheckpoint { diagnostic } => {
                write!(formatter, "invalid Stack reshape checkpoint: {diagnostic}")
            }
            Self::StaleGeneration { .. } => {
                write!(formatter, "native Stack reshape generation changed")
            }
            Self::ExistingReshapeIncomplete { pr, changed_fields } => write!(
                formatter,
                "existing Cara reshape has not established PR #{pr}: {}",
                changed_fields.join(", ")
            ),
            Self::FinalTopologyMismatch { diagnostic } => {
                write!(
                    formatter,
                    "native Stack reshape final topology mismatch: {diagnostic}"
                )
            }
            Self::StackMutation(error) => error.fmt(formatter),
            Self::StackRead(error) => error.fmt(formatter),
            Self::Provider(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GitHubStackReshapeError {}

impl From<GitHubStackMutationError> for GitHubStackReshapeError {
    fn from(error: GitHubStackMutationError) -> Self {
        Self::StackMutation(error)
    }
}

impl From<GitHubStackReadError> for GitHubStackReshapeError {
    fn from(error: GitHubStackReadError) -> Self {
        Self::StackRead(error)
    }
}

impl From<MutationError> for GitHubStackReshapeError {
    fn from(error: MutationError) -> Self {
        Self::Provider(error)
    }
}

impl<R: CommandRunner> GitHubMutationAdapter<R> {
    /// Fresh-read one exact unqueued generation and seal a zero-write preflight.
    pub fn native_stack_reshape_preflight(
        &self,
        repository: &RepositoryId,
        plan: &GitHubStackReshapePlan,
    ) -> Result<GitHubStackReshapeCheckpoint, GitHubStackReshapeError> {
        validate_plan(repository, plan)?;
        let actual = self.native_stack_generation_allowing_stale_tail_base(
            repository,
            plan.before.number,
            stale_tail_base_exception(plan),
        )?;
        if actual.as_ref() != Some(&plan.before) {
            return Err(GitHubStackReshapeError::StaleGeneration {
                expected: Box::new(plan.before.clone()),
                actual: actual.map(Box::new),
            });
        }
        Ok(GitHubStackReshapeCheckpoint::preflighted(plan.clone()))
    }

    /// Unstack the exact preflighted generation. A lost response or crash after
    /// the provider write converges through the CRUD adapter's exact absence
    /// proof and does not issue a second unstack write.
    pub fn native_stack_reshape_unstack(
        &self,
        repository: &RepositoryId,
        checkpoint: &GitHubStackReshapeCheckpoint,
    ) -> Result<GitHubStackReshapeCheckpoint, GitHubStackReshapeError> {
        validate_checkpoint(repository, checkpoint)?;
        if checkpoint.phase > GitHubStackReshapePhase::Preflighted {
            return Ok(checkpoint.clone());
        }
        let plan = &checkpoint.plan;
        let receipt = self.native_stack_unstack(
            repository,
            &GitHubStackUnstackPlan {
                operation_id: unstack_operation_id(plan),
                actor: plan.actor.clone(),
                before: plan.before.clone(),
                allowed_stale_tail_base: stale_tail_base_exception(plan),
                desired_after: None,
            },
        )?;
        let mut next = checkpoint.clone();
        next.phase = GitHubStackReshapePhase::Unstacked;
        next.unstack_receipt = Some(receipt);
        Ok(next.seal())
    }

    /// Bind the existing Cara evict/split receipt only after fresh PR truth
    /// proves every planned base, head, control-label, and auto-merge outcome.
    /// The existing reshape remains policy authority; this adapter never
    /// reimplements or weakens those rules.
    pub fn native_stack_reshape_record_existing(
        &self,
        repository: &RepositoryId,
        checkpoint: &GitHubStackReshapeCheckpoint,
        reshape_receipt: &OperationReceipt,
    ) -> Result<GitHubStackReshapeCheckpoint, GitHubStackReshapeError> {
        validate_checkpoint(repository, checkpoint)?;
        if checkpoint.phase >= GitHubStackReshapePhase::ReshapeApplied {
            if checkpoint.reshape_receipt.as_ref() == Some(reshape_receipt) {
                return Ok(checkpoint.clone());
            }
            return Err(invalid_checkpoint(
                "a different existing-reshape receipt is already bound",
            ));
        }
        if checkpoint.phase != GitHubStackReshapePhase::Unstacked {
            return Err(invalid_checkpoint(
                "existing reshape can be recorded only after exact unstack proof",
            ));
        }
        validate_existing_receipt(&checkpoint.plan, reshape_receipt)?;
        self.verify_reshape_pr_postconditions(repository, &checkpoint.plan)?;

        let mut next = checkpoint.clone();
        next.phase = GitHubStackReshapePhase::ReshapeApplied;
        next.reshape_receipt = Some(reshape_receipt.clone());
        Ok(next.seal())
    }

    /// Create at most one missing replacement Stack. Exact already-created
    /// topology is a zero-write retry, so a crash between POST and checkpoint
    /// persistence resumes without duplicate replacement creation.
    pub fn native_stack_reshape_rebuild_next(
        &self,
        repository: &RepositoryId,
        checkpoint: &GitHubStackReshapeCheckpoint,
    ) -> Result<GitHubStackReshapeCheckpoint, GitHubStackReshapeError> {
        validate_checkpoint(repository, checkpoint)?;
        if checkpoint.phase >= GitHubStackReshapePhase::Rebuilt {
            return Ok(checkpoint.clone());
        }
        if !matches!(
            checkpoint.phase,
            GitHubStackReshapePhase::ReshapeApplied | GitHubStackReshapePhase::Rebuilding
        ) {
            return Err(invalid_checkpoint(
                "replacement creation requires a proven existing reshape",
            ));
        }

        // Revalidate all exact PR generations immediately before every create.
        self.verify_reshape_pr_postconditions(repository, &checkpoint.plan)?;
        let desired = provider_replacement_chains(&checkpoint.plan);
        let index = checkpoint.replacement_receipts.len();
        let mut next = checkpoint.clone();
        if let Some(topology) = desired.get(index) {
            let receipt = self.native_stack_create(
                repository,
                &GitHubStackCreatePlan {
                    operation_id: replacement_operation_id(&checkpoint.plan, index),
                    actor: checkpoint.plan.actor.clone(),
                    desired: (*topology).clone(),
                },
            )?;
            next.replacement_receipts.push(receipt);
        }
        next.phase = if next.replacement_receipts.len() == desired.len() {
            GitHubStackReshapePhase::Rebuilt
        } else {
            GitHubStackReshapePhase::Rebuilding
        };
        Ok(next.seal())
    }

    /// Prove that every multi-member desired chain exists exactly once and no
    /// original member appears in an extra provider Stack. Singleton chains and
    /// a fully dissolved caravan are proved by exact inventory absence.
    pub fn native_stack_reshape_verify_final(
        &self,
        repository: &RepositoryId,
        checkpoint: &GitHubStackReshapeCheckpoint,
    ) -> Result<GitHubStackReshapeCheckpoint, GitHubStackReshapeError> {
        validate_checkpoint(repository, checkpoint)?;
        if checkpoint.phase == GitHubStackReshapePhase::Verified {
            return Ok(checkpoint.clone());
        }
        if checkpoint.phase != GitHubStackReshapePhase::Rebuilt {
            return Err(invalid_checkpoint(
                "final verification requires every replacement create receipt",
            ));
        }
        self.verify_reshape_pr_postconditions(repository, &checkpoint.plan)?;
        let final_stacks = self.verify_final_stack_topology(repository, &checkpoint.plan)?;
        let mut next = checkpoint.clone();
        next.phase = GitHubStackReshapePhase::Verified;
        next.final_stacks = final_stacks;
        Ok(next.seal())
    }

    fn verify_reshape_pr_postconditions(
        &self,
        repository: &RepositoryId,
        plan: &GitHubStackReshapePlan,
    ) -> Result<(), GitHubStackReshapeError> {
        for expected in &plan.pr_postconditions {
            let actual = self.refetch_pull_request(repository, expected.pr)?;
            let changed_fields = changed_pr_fields(expected, &actual);
            if !changed_fields.is_empty() {
                return Err(GitHubStackReshapeError::ExistingReshapeIncomplete {
                    pr: expected.pr,
                    changed_fields,
                });
            }
        }
        Ok(())
    }

    fn verify_final_stack_topology(
        &self,
        repository: &RepositoryId,
        plan: &GitHubStackReshapePlan,
    ) -> Result<Vec<GitHubStackGeneration>, GitHubStackReshapeError> {
        let inventory = self.native_stack_inventory(repository)?;
        if inventory.truncated {
            return Err(GitHubStackReshapeError::FinalTopologyMismatch {
                diagnostic: "Stack inventory is truncated; exact absence is unproven".to_owned(),
            });
        }
        let affected = plan
            .before
            .topology
            .entries
            .iter()
            .map(|entry| entry.pr.0)
            .collect::<BTreeSet<_>>();
        let overlapping = inventory
            .stacks
            .iter()
            .filter(|stack| {
                stack
                    .pull_requests
                    .iter()
                    .any(|pr| affected.contains(&pr.number))
            })
            .collect::<Vec<_>>();
        let desired = provider_replacement_chains(plan);
        if overlapping.len() != desired.len() {
            return Err(GitHubStackReshapeError::FinalTopologyMismatch {
                diagnostic: format!(
                    "expected {} replacement Stacks intersecting the original members, observed {}",
                    desired.len(),
                    overlapping.len()
                ),
            });
        }

        let mut observed = Vec::with_capacity(overlapping.len());
        for summary in overlapping {
            let generation = self
                .native_stack_generation(repository, summary.number)?
                .ok_or_else(|| GitHubStackReshapeError::FinalTopologyMismatch {
                    diagnostic: format!(
                        "Stack #{} disappeared during final exact read",
                        summary.number
                    ),
                })?;
            observed.push(generation);
        }
        let mut ordered = Vec::with_capacity(desired.len());
        for expected in desired {
            let matches = observed
                .iter()
                .filter(|generation| generation.topology == *expected)
                .collect::<Vec<_>>();
            if matches.len() != 1 {
                return Err(GitHubStackReshapeError::FinalTopologyMismatch {
                    diagnostic: format!(
                        "replacement chain {:?} matched {} provider Stacks",
                        expected
                            .entries
                            .iter()
                            .map(|entry| entry.pr)
                            .collect::<Vec<_>>(),
                        matches.len()
                    ),
                });
            }
            ordered.push((*matches[0]).clone());
        }
        Ok(ordered)
    }
}

fn validate_plan(
    repository: &RepositoryId,
    plan: &GitHubStackReshapePlan,
) -> Result<(), GitHubStackReshapeError> {
    if &plan.repository != repository || !plan.verify() {
        return Err(invalid_plan(
            "github_stack_reshape_plan_invalid",
            "reshape plan repository, schema, semantics, or hash is invalid",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate_plan_semantics(plan: &GitHubStackReshapePlan) -> Result<(), GitHubStackReshapeError> {
    validate_operation_identity(&plan.operation_id, &plan.actor).map_err(map_plan_error)?;
    if plan.schema_version != SCHEMA_VERSION {
        return Err(invalid_plan(
            "github_stack_reshape_schema_invalid",
            "unsupported Stack reshape plan schema",
        ));
    }
    if !plan.before.open || plan.before.number == 0 || plan.before.id == 0 {
        return Err(invalid_plan(
            "github_stack_reshape_generation_invalid",
            "reshape requires one open identified provider Stack",
        ));
    }
    validate_topology_allowing_stale_tail_base(
        &plan.repository,
        &plan.before.topology,
        2,
        stale_tail_base_exception(plan),
    )
    .map_err(map_plan_error)?;
    validate_open_entries(&plan.before.topology, 0).map_err(map_plan_error)?;
    if plan.replacement_chains.len() > MAX_REPLACEMENT_CHAINS {
        return Err(invalid_plan(
            "github_stack_reshape_too_many_chains",
            "evict/split replacement is bounded to at most two chains",
        ));
    }
    for chain in &plan.replacement_chains {
        validate_topology(&plan.repository, chain, 1).map_err(map_plan_error)?;
        validate_open_entries(chain, 0).map_err(map_plan_error)?;
        if chain.base != plan.before.topology.base {
            return Err(invalid_plan(
                "github_stack_reshape_base_changed",
                "every replacement chain must retain the exact Stack base generation",
            ));
        }
    }

    let before = &plan.before.topology.entries;
    let selected_index = before
        .iter()
        .position(|entry| entry.pr == plan.selected_pr)
        .ok_or_else(|| {
            invalid_plan(
                "github_stack_reshape_selected_missing",
                "selected PR is not in the exact provider Stack generation",
            )
        })?;
    let desired_entries = plan
        .replacement_chains
        .iter()
        .flat_map(|chain| chain.entries.iter())
        .collect::<Vec<_>>();
    let expected = match plan.operation {
        GitHubStackReshapeOperation::Evict => {
            if plan.replacement_chains.len() > usize::from(before.len() > 1) {
                return Err(invalid_plan(
                    "github_stack_reshape_evict_partition_invalid",
                    "eviction produces zero or one surviving chain",
                ));
            }
            before
                .iter()
                .filter(|entry| entry.pr != plan.selected_pr)
                .collect::<Vec<_>>()
        }
        GitHubStackReshapeOperation::Split => {
            if selected_index == 0 || plan.replacement_chains.len() != 2 {
                return Err(invalid_plan(
                    "github_stack_reshape_split_partition_invalid",
                    "split requires a non-head selected PR and exactly two replacement chains",
                ));
            }
            if plan.replacement_chains[0].entries.len() != selected_index
                || plan.replacement_chains[1]
                    .entries
                    .first()
                    .is_none_or(|entry| entry.pr != plan.selected_pr)
            {
                return Err(invalid_plan(
                    "github_stack_reshape_split_boundary_invalid",
                    "replacement chains do not split immediately before the selected PR",
                ));
            }
            before.iter().collect::<Vec<_>>()
        }
    };
    if desired_entries.len() != expected.len() {
        return Err(invalid_plan(
            "github_stack_reshape_member_set_invalid",
            "replacement chains do not contain the complete expected member set",
        ));
    }
    for (desired, original) in desired_entries.iter().zip(expected) {
        if desired.pr != original.pr
            || desired.head != original.head
            || desired.pull_request_state != original.pull_request_state
            || desired.draft != original.draft
            || desired.merged_at != original.merged_at
        {
            return Err(invalid_plan(
                "github_stack_reshape_generation_changed",
                "replacement chains must preserve every surviving exact PR/head generation",
            ));
        }
    }

    let derived = derive_postconditions(
        plan.operation,
        plan.selected_pr,
        &plan.before,
        &plan.replacement_chains,
    )?;
    if plan.pr_postconditions != derived {
        return Err(invalid_plan(
            "github_stack_reshape_postconditions_invalid",
            "PR postconditions do not exactly match the existing Cara reshape contract",
        ));
    }
    Ok(())
}

fn derive_postconditions(
    operation: GitHubStackReshapeOperation,
    selected_pr: PrNumber,
    before: &GitHubStackGeneration,
    replacement_chains: &[GitHubStackTopology],
) -> Result<Vec<GitHubStackReshapePrPostcondition>, GitHubStackReshapeError> {
    let replacements = replacement_chains
        .iter()
        .flat_map(|chain| chain.entries.iter())
        .map(|entry| (entry.pr, entry))
        .collect::<BTreeMap<_, _>>();
    let mut result = Vec::with_capacity(before.topology.entries.len());
    for original in &before.topology.entries {
        let evicted = operation == GitHubStackReshapeOperation::Evict && original.pr == selected_pr;
        let generation = if evicted {
            original
        } else {
            replacements.get(&original.pr).copied().ok_or_else(|| {
                invalid_plan(
                    "github_stack_reshape_member_missing",
                    "a non-evicted member is absent from replacement chains",
                )
            })?
        };
        let (required_labels, forbidden_labels) = if evicted {
            (
                BTreeSet::from([EVICTED_LABEL.to_owned()]),
                BTreeSet::from([ACTIVE_LABEL.to_owned(), FORCE_LABEL.to_owned()]),
            )
        } else {
            (
                BTreeSet::from([ACTIVE_LABEL.to_owned()]),
                BTreeSet::from([EVICTED_LABEL.to_owned()]),
            )
        };
        result.push(GitHubStackReshapePrPostcondition {
            pr: original.pr,
            head: generation.head.clone(),
            base: generation.base.clone(),
            required_labels,
            forbidden_labels,
            auto_merge_enabled: false,
        });
    }
    Ok(result)
}

fn changed_pr_fields(
    expected: &GitHubStackReshapePrPostcondition,
    actual: &PullRequestSnapshot,
) -> Vec<String> {
    let mut changed = Vec::new();
    if actual.state != PullRequestState::Open {
        changed.push("state".to_owned());
    }
    if actual.draft {
        changed.push("draft".to_owned());
    }
    if actual.cross_repository {
        changed.push("repository".to_owned());
    }
    if actual.head != expected.head {
        changed.push("head".to_owned());
    }
    if actual.base != expected.base {
        changed.push("base".to_owned());
    }
    if !expected.required_labels.is_subset(&actual.labels) {
        changed.push("required_labels".to_owned());
    }
    if !expected.forbidden_labels.is_disjoint(&actual.labels) {
        changed.push("forbidden_labels".to_owned());
    }
    if actual.auto_merge.enabled != expected.auto_merge_enabled {
        changed.push("auto_merge".to_owned());
    }
    changed
}

fn validate_existing_receipt(
    plan: &GitHubStackReshapePlan,
    receipt: &OperationReceipt,
) -> Result<(), GitHubStackReshapeError> {
    if receipt.operation != plan.operation.as_str() || receipt.operation_id.0.trim().is_empty() {
        return Err(invalid_checkpoint(
            "existing Cara receipt does not match the planned evict/split operation",
        ));
    }
    Ok(())
}

fn validate_checkpoint(
    repository: &RepositoryId,
    checkpoint: &GitHubStackReshapeCheckpoint,
) -> Result<(), GitHubStackReshapeError> {
    if &checkpoint.repository != repository || !checkpoint.verify() {
        return Err(invalid_checkpoint(
            "schema, repository, plan, phase evidence, or hash is invalid",
        ));
    }
    Ok(())
}

fn validate_checkpoint_shape(
    checkpoint: &GitHubStackReshapeCheckpoint,
) -> Result<(), GitHubStackReshapeError> {
    let create_chains = provider_replacement_chains(&checkpoint.plan);
    if checkpoint.replacement_receipts.len() > create_chains.len() {
        return Err(invalid_checkpoint("too many replacement receipts"));
    }
    if let Some(receipt) = checkpoint.unstack_receipt.as_ref() {
        if !receipt.verify()
            || receipt.repository != checkpoint.repository
            || receipt.operation != GitHubStackMutationOperation::Unstack
            || receipt.operation_id != unstack_operation_id(&checkpoint.plan)
            || receipt.after.is_some()
        {
            return Err(invalid_checkpoint("unstack receipt is not exact"));
        }
    }
    for (index, receipt) in checkpoint.replacement_receipts.iter().enumerate() {
        if !receipt.verify()
            || receipt.repository != checkpoint.repository
            || receipt.operation != GitHubStackMutationOperation::Create
            || receipt.operation_id != replacement_operation_id(&checkpoint.plan, index)
            || receipt
                .after
                .as_ref()
                .is_none_or(|after| after.topology != *create_chains[index])
        {
            return Err(invalid_checkpoint(
                "replacement create receipt is not exact",
            ));
        }
    }
    if let Some(receipt) = checkpoint.reshape_receipt.as_ref() {
        validate_existing_receipt(&checkpoint.plan, receipt)?;
    }

    let fields_match_phase = match checkpoint.phase {
        GitHubStackReshapePhase::Preflighted => {
            checkpoint.unstack_receipt.is_none()
                && checkpoint.reshape_receipt.is_none()
                && checkpoint.replacement_receipts.is_empty()
                && checkpoint.final_stacks.is_empty()
        }
        GitHubStackReshapePhase::Unstacked => {
            checkpoint.unstack_receipt.is_some()
                && checkpoint.reshape_receipt.is_none()
                && checkpoint.replacement_receipts.is_empty()
                && checkpoint.final_stacks.is_empty()
        }
        GitHubStackReshapePhase::ReshapeApplied => {
            checkpoint.unstack_receipt.is_some()
                && checkpoint.reshape_receipt.is_some()
                && checkpoint.replacement_receipts.is_empty()
                && checkpoint.final_stacks.is_empty()
        }
        GitHubStackReshapePhase::Rebuilding => {
            checkpoint.unstack_receipt.is_some()
                && checkpoint.reshape_receipt.is_some()
                && !checkpoint.replacement_receipts.is_empty()
                && checkpoint.replacement_receipts.len() < create_chains.len()
                && checkpoint.final_stacks.is_empty()
        }
        GitHubStackReshapePhase::Rebuilt => {
            checkpoint.unstack_receipt.is_some()
                && checkpoint.reshape_receipt.is_some()
                && checkpoint.replacement_receipts.len() == create_chains.len()
                && checkpoint.final_stacks.is_empty()
        }
        GitHubStackReshapePhase::Verified => {
            checkpoint.unstack_receipt.is_some()
                && checkpoint.reshape_receipt.is_some()
                && checkpoint.replacement_receipts.len() == create_chains.len()
                && checkpoint.final_stacks.len() == create_chains.len()
                && checkpoint
                    .final_stacks
                    .iter()
                    .zip(create_chains)
                    .all(|(actual, expected)| actual.topology == *expected)
        }
    };
    if !fields_match_phase {
        return Err(invalid_checkpoint(
            "receipts do not match the declared non-atomic phase",
        ));
    }
    Ok(())
}

fn provider_replacement_chains(plan: &GitHubStackReshapePlan) -> Vec<&GitHubStackTopology> {
    plan.replacement_chains
        .iter()
        .filter(|chain| chain.entries.len() >= 2)
        .collect()
}

fn stale_tail_base_exception(plan: &GitHubStackReshapePlan) -> Option<PrNumber> {
    (plan.operation == GitHubStackReshapeOperation::Evict
        && plan.before.topology.entries.last().map(|entry| entry.pr) == Some(plan.selected_pr))
    .then_some(plan.selected_pr)
}

fn unstack_operation_id(plan: &GitHubStackReshapePlan) -> String {
    format!("{}:unstack", plan.operation_id)
}

fn replacement_operation_id(plan: &GitHubStackReshapePlan, index: usize) -> String {
    format!("{}:replacement:{index}", plan.operation_id)
}

fn plan_material_hash(plan: &GitHubStackReshapePlan) -> String {
    let mut material = plan.clone();
    material.plan_hash.clear();
    crate::membership::fnv1a64(
        &serde_json::to_vec(&material).expect("Stack reshape plan serializes"),
    )
}

fn map_plan_error(error: GitHubStackMutationError) -> GitHubStackReshapeError {
    match error {
        GitHubStackMutationError::InvalidPlan { code, message } => {
            GitHubStackReshapeError::InvalidPlan { code, message }
        }
        other => GitHubStackReshapeError::StackMutation(other),
    }
}

fn invalid_plan(code: &str, message: &str) -> GitHubStackReshapeError {
    GitHubStackReshapeError::InvalidPlan {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

fn invalid_checkpoint(diagnostic: &str) -> GitHubStackReshapeError {
    GitHubStackReshapeError::InvalidCheckpoint {
        diagnostic: diagnostic.to_owned(),
    }
}

// Keep transport errors represented through the public error graph when this
// module is used with custom command runners.
impl From<CommandRunError> for GitHubStackReshapeError {
    fn from(error: CommandRunError) -> Self {
        Self::StackRead(GitHubStackReadError::Runner(error))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;
    use crate::command::{CommandIntent, CommandOutput, CommandSpec};
    use crate::github::{
        GitHubStackBase, GitHubStackEntryGeneration, GitHubStackPullRequest,
        GitHubStackPullRequestHead, GitHubStackSnapshot,
    };
    use crate::model::{CommitOid, OperationId};

    struct RecordingRunner {
        outputs: RefCell<VecDeque<Result<CommandOutput, CommandRunError>>>,
        commands: RefCell<Vec<CommandSpec>>,
    }

    impl RecordingRunner {
        fn new(outputs: Vec<CommandOutput>) -> Self {
            Self {
                outputs: RefCell::new(outputs.into_iter().map(Ok).collect()),
                commands: RefCell::new(Vec::new()),
            }
        }

        fn assert_exhausted(&self) {
            assert!(
                self.outputs.borrow().is_empty(),
                "unused fake provider output"
            );
        }

        fn write_count(&self) -> usize {
            self.commands
                .borrow()
                .iter()
                .filter(|command| command.intent() == CommandIntent::ProviderWrite)
                .count()
        }
    }

    impl CommandRunner for RecordingRunner {
        fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
            self.commands.borrow_mut().push(command.clone());
            self.outputs
                .borrow_mut()
                .pop_front()
                .expect("unexpected provider command")
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

    fn entry(
        position: u32,
        number: u64,
        base: BranchSnapshot,
        name: &str,
        oid: &str,
    ) -> GitHubStackEntryGeneration {
        GitHubStackEntryGeneration {
            position,
            pr: PrNumber(number),
            stack_state: "open".to_owned(),
            pull_request_state: PullRequestState::Open,
            draft: false,
            merged_at: None,
            base,
            head: branch(name, oid),
        }
    }

    fn before_generation() -> GitHubStackGeneration {
        let base = branch("main", "base000");
        let root = entry(0, 101, base.clone(), "root", "aaa111");
        let child = entry(1, 102, root.head.clone(), "child", "bbb222");
        let tail = entry(2, 103, child.head.clone(), "tail", "ccc333");
        GitHubStackGeneration {
            id: 8_001,
            number: 42,
            node_id: "S_before".to_owned(),
            open: true,
            created_at: "2026-08-01T10:00:00Z".to_owned(),
            topology: GitHubStackTopology {
                base,
                entries: vec![root, child, tail],
            },
        }
    }

    fn evict_middle_plan() -> GitHubStackReshapePlan {
        let before = before_generation();
        let root = before.topology.entries[0].clone();
        let mut tail = before.topology.entries[2].clone();
        tail.position = 1;
        tail.base.clone_from(&root.head);
        GitHubStackReshapePlan::new(
            repository(),
            "reshape-op",
            "cara-test",
            GitHubStackReshapeOperation::Evict,
            PrNumber(102),
            before.clone(),
            vec![GitHubStackTopology {
                base: before.topology.base,
                entries: vec![root, tail],
            }],
        )
        .unwrap()
    }

    fn stale_tail_eviction_plan() -> GitHubStackReshapePlan {
        // Exact authority-local PR 2483 -> PR 2486 evidence: the parent head
        // advanced to e420e514, while the tail still targets prior head
        // 05b78276. Removing that exact stale tail closes the bad edge; every
        // surviving generation remains byte-for-byte unchanged.
        let base = branch(
            "agent/ms-dev-2/cacophony/mqegnry93bttwu43-pr-g2494e4ac3f1f6fef588b7afe7359c20489d85070",
            "b9929b3b0c2bfac4f0e262c31fe9ee56be611cd0",
        );
        let parent = entry(
            0,
            2483,
            base.clone(),
            "agent/ms-dev-3/cacophony/e0371ba58e492e06-pr-g05b782765a958553a3928c2dda8f44556eb0b735",
            "e420e514282f5ee7d7da78776082de1a955cb5a5",
        );
        let stale_parent = branch(
            &parent.head.name,
            "05b782765a958553a3928c2dda8f44556eb0b735",
        );
        let tail = entry(
            1,
            2486,
            stale_parent,
            "agent/ms-dev-3/cacophony/4e1a88065f44732f-pr-g60ac31df852749da6d4dc3f91c286646587f9bbc",
            "60ac31df852749da6d4dc3f91c286646587f9bbc",
        );
        let before = GitHubStackGeneration {
            id: 8_483,
            number: 2_470,
            node_id: "S_pr2483_pr2486".to_owned(),
            open: true,
            created_at: "2026-08-06T08:00:00Z".to_owned(),
            topology: GitHubStackTopology {
                base: base.clone(),
                entries: vec![parent.clone(), tail],
            },
        };
        GitHubStackReshapePlan::new(
            repository(),
            "evict-pr2486-stale-tail",
            "cara-test",
            GitHubStackReshapeOperation::Evict,
            PrNumber(2486),
            before,
            vec![GitHubStackTopology {
                base,
                entries: vec![parent],
            }],
        )
        .expect("tail eviction removes the sole stale base edge")
    }

    fn split_plan() -> GitHubStackReshapePlan {
        let before = before_generation();
        let first = before.topology.entries[0].clone();
        let mut child = before.topology.entries[1].clone();
        child.position = 0;
        child.base.clone_from(&before.topology.base);
        let mut tail = before.topology.entries[2].clone();
        tail.position = 1;
        tail.base.clone_from(&child.head);
        GitHubStackReshapePlan::new(
            repository(),
            "split-op",
            "cara-test",
            GitHubStackReshapeOperation::Split,
            PrNumber(102),
            before.clone(),
            vec![
                GitHubStackTopology {
                    base: before.topology.base.clone(),
                    entries: vec![first],
                },
                GitHubStackTopology {
                    base: before.topology.base,
                    entries: vec![child, tail],
                },
            ],
        )
        .unwrap()
    }

    fn stack_snapshot(generation: &GitHubStackGeneration) -> GitHubStackSnapshot {
        GitHubStackSnapshot {
            id: generation.id,
            number: generation.number,
            node_id: generation.node_id.clone(),
            base: GitHubStackBase {
                ref_name: generation.topology.base.name.clone(),
            },
            open: generation.open,
            created_at: generation.created_at.clone(),
            pull_requests: generation
                .topology
                .entries
                .iter()
                .map(|entry| GitHubStackPullRequest {
                    number: entry.pr.0,
                    state: entry.stack_state.clone(),
                    draft: entry.draft,
                    merged_at: entry.merged_at.clone(),
                    head: GitHubStackPullRequestHead {
                        ref_name: entry.head.name.clone(),
                        sha: entry.head.oid.clone(),
                    },
                })
                .collect(),
        }
    }

    fn replacement_generation(topology: &GitHubStackTopology) -> GitHubStackGeneration {
        GitHubStackGeneration {
            id: 8_002,
            number: 77,
            node_id: "S_replacement".to_owned(),
            open: true,
            created_at: "2026-08-01T10:05:00Z".to_owned(),
            topology: topology.clone(),
        }
    }

    fn stack_json(generation: &GitHubStackGeneration) -> CommandOutput {
        CommandOutput::success(serde_json::to_string(&stack_snapshot(generation)).unwrap())
    }

    fn inventory_json(generations: &[GitHubStackGeneration]) -> CommandOutput {
        let inventory = generations.iter().map(stack_snapshot).collect::<Vec<_>>();
        CommandOutput::success(serde_json::to_string(&inventory).unwrap())
    }

    fn ref_json(branch: &BranchSnapshot) -> CommandOutput {
        CommandOutput::success(serde_json::json!({"object": {"sha": branch.oid}}).to_string())
    }

    fn pr_json(
        number: PrNumber,
        head: &BranchSnapshot,
        base: &BranchSnapshot,
        labels: &BTreeSet<String>,
        auto_merge_enabled: bool,
    ) -> CommandOutput {
        let auto_merge = auto_merge_enabled
            .then(|| serde_json::json!({"mergeMethod": "SQUASH", "enabledBy": {"login": "cara"}}));
        CommandOutput::success(
            serde_json::json!({
                "number": number.0,
                "title": format!("PR {}", number.0),
                "body": "",
                "state": "OPEN",
                "isDraft": false,
                "headRefName": head.name,
                "headRefOid": head.oid,
                "headRepository": {"name": "widgets", "nameWithOwner": "acme/widgets"},
                "headRepositoryOwner": {"login": "acme"},
                "isCrossRepository": false,
                "baseRefName": base.name,
                "baseRefOid": base.oid,
                "labels": labels.iter().map(|name| serde_json::json!({"name": name})).collect::<Vec<_>>(),
                "autoMergeRequest": auto_merge,
                "statusCheckRollup": [],
                "createdAt": "2026-08-01T09:00:00Z",
                "mergedAt": null,
                "url": format!("https://github.com/acme/widgets/pull/{}", number.0),
                "updatedAt": "2026-08-01T10:00:00Z",
                "mergeStateStatus": "CLEAN"
            })
            .to_string(),
        )
    }

    fn topology_pr_outputs(topology: &GitHubStackTopology) -> Vec<CommandOutput> {
        topology
            .entries
            .iter()
            .map(|entry| {
                pr_json(
                    entry.pr,
                    &entry.head,
                    &entry.base,
                    &BTreeSet::from([ACTIVE_LABEL.to_owned()]),
                    false,
                )
            })
            .collect()
    }

    fn postcondition_outputs(plan: &GitHubStackReshapePlan) -> Vec<CommandOutput> {
        plan.pr_postconditions
            .iter()
            .map(|expected| {
                pr_json(
                    expected.pr,
                    &expected.head,
                    &expected.base,
                    &expected.required_labels,
                    expected.auto_merge_enabled,
                )
            })
            .collect()
    }

    fn observe_outputs(generation: &GitHubStackGeneration) -> Vec<CommandOutput> {
        let mut outputs = vec![ref_json(&generation.topology.base)];
        outputs.extend(topology_pr_outputs(&generation.topology));
        outputs
    }

    fn existing_receipt(operation: &str) -> OperationReceipt {
        OperationReceipt {
            operation_id: OperationId("cara-reshape-receipt".to_owned()),
            operation: operation.to_owned(),
            completed_steps: Vec::new(),
            changed: true,
        }
    }

    fn not_found() -> CommandOutput {
        CommandOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: "gh: HTTP 404".to_owned(),
        }
    }

    #[test]
    fn eviction_plan_binds_complete_survivor_chain_and_existing_policy() {
        let plan = evict_middle_plan();
        assert!(plan.verify());
        assert_eq!(
            plan.replacement_chains[0]
                .entries
                .iter()
                .map(|entry| entry.pr)
                .collect::<Vec<_>>(),
            vec![PrNumber(101), PrNumber(103)]
        );
        let evicted = &plan.pr_postconditions[1];
        assert!(evicted.required_labels.contains(EVICTED_LABEL));
        assert!(evicted.forbidden_labels.contains(ACTIVE_LABEL));
        assert!(evicted.forbidden_labels.contains(FORCE_LABEL));
        assert!(!evicted.auto_merge_enabled);
        let tail = &plan.pr_postconditions[2];
        assert_eq!(tail.base, plan.before.topology.entries[0].head);
    }

    #[test]
    fn split_plan_is_exactly_two_chains_and_rejects_queued_generation() {
        let plan = split_plan();
        assert!(plan.verify());
        assert_eq!(
            plan.replacement_chains
                .iter()
                .map(|chain| chain.entries.len())
                .collect::<Vec<_>>(),
            vec![1, 2]
        );

        let mut queued = before_generation();
        queued.topology.entries[1].stack_state = "queued".to_owned();
        assert!(matches!(
            GitHubStackReshapePlan::new(
                repository(),
                "queued-op",
                "cara-test",
                GitHubStackReshapeOperation::Split,
                PrNumber(102),
                queued.clone(),
                vec![
                    GitHubStackTopology {
                        base: queued.topology.base.clone(),
                        entries: vec![queued.topology.entries[0].clone()],
                    },
                    GitHubStackTopology {
                        base: queued.topology.base.clone(),
                        entries: vec![
                            queued.topology.entries[1].clone(),
                            queued.topology.entries[2].clone()
                        ],
                    },
                ],
            ),
            Err(GitHubStackReshapeError::InvalidPlan { .. })
        ));
    }

    #[test]
    fn preflight_is_zero_write_and_seals_non_atomic_phase() {
        let plan = evict_middle_plan();
        let mut outputs = vec![
            stack_json(&plan.before),
            ref_json(&plan.before.topology.base),
        ];
        outputs.extend(topology_pr_outputs(&plan.before.topology));
        let adapter = GitHubMutationAdapter::new(RecordingRunner::new(outputs));

        let checkpoint = adapter
            .native_stack_reshape_preflight(&repository(), &plan)
            .unwrap();
        assert_eq!(checkpoint.phase, GitHubStackReshapePhase::Preflighted);
        assert!(!checkpoint.provider_atomic);
        assert!(checkpoint.verify());
        assert_eq!(adapter.runner.write_count(), 0);
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn stale_tail_eviction_preflights_and_unstacks_with_exact_receipts() {
        let plan = stale_tail_eviction_plan();
        assert!(plan.verify());
        assert_eq!(plan.selected_pr, PrNumber(2486));
        assert_eq!(plan.before.topology.entries[0].pr, PrNumber(2483));
        assert_eq!(
            plan.before.topology.entries[0].head.oid.0,
            "e420e514282f5ee7d7da78776082de1a955cb5a5"
        );
        assert_eq!(
            plan.before.topology.entries[1].base.oid.0,
            "05b782765a958553a3928c2dda8f44556eb0b735"
        );

        let mut outputs = vec![stack_json(&plan.before)];
        outputs.extend(observe_outputs(&plan.before));
        outputs.push(stack_json(&plan.before));
        outputs.extend(observe_outputs(&plan.before));
        outputs.push(CommandOutput::success("{}"));
        outputs.push(not_found());
        outputs.push(inventory_json(&[]));
        let adapter = GitHubMutationAdapter::new(RecordingRunner::new(outputs));

        let preflight = adapter
            .native_stack_reshape_preflight(&repository(), &plan)
            .expect("exact stale-tail generation preflights without mutation");
        assert_eq!(preflight.phase, GitHubStackReshapePhase::Preflighted);
        assert!(preflight.verify());
        assert_eq!(adapter.runner.write_count(), 0);

        let unstacked = adapter
            .native_stack_reshape_unstack(&repository(), &preflight)
            .expect("whole-Stack removal accepts only the selected stale tail edge");
        assert_eq!(unstacked.phase, GitHubStackReshapePhase::Unstacked);
        let receipt = unstacked
            .unstack_receipt
            .as_ref()
            .expect("durable before/after unstack receipt");
        assert_eq!(receipt.before.as_ref(), Some(&plan.before));
        assert!(receipt.after.is_none());
        assert!(receipt.verify());
        assert!(unstacked.verify());
        assert_eq!(adapter.runner.write_count(), 1);
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn stale_non_tail_base_remains_fail_closed_before_provider_access() {
        let mut before = before_generation();
        before.topology.entries[1].base = branch("root", "prior-root-head");
        let root = before.topology.entries[0].clone();
        let child = before.topology.entries[1].clone();
        let error = GitHubStackReshapePlan::new(
            repository(),
            "reject-stale-middle",
            "cara-test",
            GitHubStackReshapeOperation::Evict,
            PrNumber(103),
            before.clone(),
            vec![GitHubStackTopology {
                base: before.topology.base,
                entries: vec![root, child],
            }],
        )
        .expect_err("only the exact selected tail may carry a stale base");
        assert!(matches!(
            error,
            GitHubStackReshapeError::InvalidPlan { ref code, .. }
                if code == "github_stack_base_chain_invalid"
        ));
    }

    #[test]
    fn crash_recovery_converges_unstack_and_replacement_without_repeat_writes() {
        let plan = evict_middle_plan();
        let replacement = replacement_generation(&plan.replacement_chains[0]);
        let mut outputs = vec![not_found(), inventory_json(&[])];
        outputs.extend(postcondition_outputs(&plan));
        outputs.extend(postcondition_outputs(&plan));
        outputs.push(inventory_json(std::slice::from_ref(&replacement)));
        outputs.extend(observe_outputs(&replacement));
        outputs.extend(postcondition_outputs(&plan));
        outputs.push(inventory_json(std::slice::from_ref(&replacement)));
        outputs.push(stack_json(&replacement));
        outputs.extend(observe_outputs(&replacement));
        let adapter = GitHubMutationAdapter::new(RecordingRunner::new(outputs));

        let preflight = GitHubStackReshapeCheckpoint::preflighted(plan.clone());
        let unstacked = adapter
            .native_stack_reshape_unstack(&repository(), &preflight)
            .unwrap();
        assert_eq!(unstacked.phase, GitHubStackReshapePhase::Unstacked);
        assert!(unstacked.unstack_receipt.as_ref().unwrap().verify());

        let applied = adapter
            .native_stack_reshape_record_existing(
                &repository(),
                &unstacked,
                &existing_receipt("evict"),
            )
            .unwrap();
        let rebuilt = adapter
            .native_stack_reshape_rebuild_next(&repository(), &applied)
            .unwrap();
        assert_eq!(rebuilt.phase, GitHubStackReshapePhase::Rebuilt);
        assert_eq!(rebuilt.replacement_receipts.len(), 1);
        assert_eq!(
            rebuilt.replacement_receipts[0].disposition,
            super::super::GitHubStackMutationDisposition::AlreadySatisfied
        );

        let verified = adapter
            .native_stack_reshape_verify_final(&repository(), &rebuilt)
            .unwrap();
        assert_eq!(verified.phase, GitHubStackReshapePhase::Verified);
        assert!(!verified.provider_atomic);
        assert_eq!(verified.final_stacks, vec![replacement]);
        assert!(verified.verify());
        assert_eq!(adapter.runner.write_count(), 0);
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn partial_existing_reshape_refuses_before_replacement_creation() {
        let plan = evict_middle_plan();
        let mut wrong = plan.pr_postconditions[0].clone();
        wrong.base = branch("wrong", "bad000");
        let outputs = vec![
            not_found(),
            inventory_json(&[]),
            pr_json(
                wrong.pr,
                &wrong.head,
                &wrong.base,
                &wrong.required_labels,
                false,
            ),
        ];
        let adapter = GitHubMutationAdapter::new(RecordingRunner::new(outputs));
        let checkpoint = adapter
            .native_stack_reshape_unstack(
                &repository(),
                &GitHubStackReshapeCheckpoint::preflighted(plan),
            )
            .unwrap();

        assert!(matches!(
            adapter.native_stack_reshape_record_existing(
                &repository(),
                &checkpoint,
                &existing_receipt("evict"),
            ),
            Err(GitHubStackReshapeError::ExistingReshapeIncomplete {
                pr: PrNumber(101),
                ..
            })
        ));
        assert_eq!(adapter.runner.write_count(), 0);
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn tampered_checkpoint_is_rejected_before_provider_access() {
        let plan = evict_middle_plan();
        let mut checkpoint = GitHubStackReshapeCheckpoint::preflighted(plan);
        checkpoint.provider_atomic = true;
        let adapter = GitHubMutationAdapter::new(RecordingRunner::new(Vec::new()));

        assert!(matches!(
            adapter.native_stack_reshape_unstack(&repository(), &checkpoint),
            Err(GitHubStackReshapeError::InvalidCheckpoint { .. })
        ));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn split_with_singleton_halves_rebuilds_zero_provider_stacks() {
        let before = {
            let full = before_generation();
            GitHubStackGeneration {
                topology: GitHubStackTopology {
                    base: full.topology.base.clone(),
                    entries: full.topology.entries[..2].to_vec(),
                },
                ..full
            }
        };
        let first = before.topology.entries[0].clone();
        let mut second = before.topology.entries[1].clone();
        second.position = 0;
        second.base.clone_from(&before.topology.base);
        let plan = GitHubStackReshapePlan::new(
            repository(),
            "singleton-split",
            "cara-test",
            GitHubStackReshapeOperation::Split,
            second.pr,
            before.clone(),
            vec![
                GitHubStackTopology {
                    base: before.topology.base.clone(),
                    entries: vec![first],
                },
                GitHubStackTopology {
                    base: before.topology.base,
                    entries: vec![second],
                },
            ],
        )
        .unwrap();
        let mut outputs = vec![not_found(), inventory_json(&[])];
        outputs.extend(postcondition_outputs(&plan));
        outputs.extend(postcondition_outputs(&plan));
        outputs.extend(postcondition_outputs(&plan));
        outputs.push(inventory_json(&[]));
        let adapter = GitHubMutationAdapter::new(RecordingRunner::new(outputs));

        let unstacked = adapter
            .native_stack_reshape_unstack(
                &repository(),
                &GitHubStackReshapeCheckpoint::preflighted(plan),
            )
            .unwrap();
        let applied = adapter
            .native_stack_reshape_record_existing(
                &repository(),
                &unstacked,
                &existing_receipt("split"),
            )
            .unwrap();
        let rebuilt = adapter
            .native_stack_reshape_rebuild_next(&repository(), &applied)
            .unwrap();
        assert_eq!(rebuilt.phase, GitHubStackReshapePhase::Rebuilt);
        assert!(rebuilt.replacement_receipts.is_empty());
        let verified = adapter
            .native_stack_reshape_verify_final(&repository(), &rebuilt)
            .unwrap();
        assert!(verified.final_stacks.is_empty());
        assert_eq!(adapter.runner.write_count(), 0);
        adapter.runner.assert_exhausted();
    }
}
