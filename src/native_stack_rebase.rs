//! Typed physical convergence for provider-native GitHub Stacks.
//!
//! Native Stack mode owns provider topology, but GitHub accepts base-linked PRs
//! whose commit histories are still divergent. This module consumes the exact
//! ancestry evidence from status, prepares the complete divergent suffix before
//! any write, then publishes every rewritten source ref in one atomic
//! force-with-lease transaction.

use clap::Args;
use mcp_cli::{ErrorCategory, StructuredError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::command::ProcessRunner;
use crate::github::{
    GitHubMutationAdapter, GitHubStackCreatePlan, GitHubStackDriftClearReceipt,
    GitHubStackMutationReceipt, GitHubStackSnapshot,
};
use crate::model::{BranchSnapshot, CommitOid, PrNumber, RepositoryId};
use crate::read::{self, NativeStackStatus, StackConsistency, StatusOutput};
use crate::{AppContext, AppError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Args)]
pub struct NativeStackRebasePreviewInput {
    /// Exact provider Stack number.
    #[arg(long)]
    pub stack: u64,
    /// Audited non-secret actor identity.
    #[arg(long)]
    pub actor: String,
    /// Bounded rationale retained by the plan and receipt.
    #[arg(long)]
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Args)]
pub struct NativeStackRebaseApplyInput {
    /// Exact provider Stack number.
    #[arg(long)]
    pub stack: u64,
    /// Audited non-secret actor identity.
    #[arg(long)]
    pub actor: String,
    /// Bounded rationale retained by the plan and receipt.
    #[arg(long)]
    pub reason: String,
    /// Exact reviewed preview hash.
    #[arg(long)]
    pub expected_plan_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NativeStackRebaseMemberPlan {
    pub position: u32,
    pub pr: PrNumber,
    pub branch: String,
    pub old_head: CommitOid,
    pub old_base: BranchSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_pr: Option<PrNumber>,
    pub parent_branch: String,
    pub parent_head: CommitOid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NativeStackRebasePlan {
    pub schema_version: u32,
    pub repository: RepositoryId,
    pub stack: u64,
    pub root_pr: PrNumber,
    pub stack_base_ref: String,
    /// Exact raw provider representation authorized for drift clearance after
    /// branch publication. This binds retained merged prefix and stale heads.
    pub provider_before: GitHubStackSnapshot,
    pub members: Vec<NativeStackRebaseMemberPlan>,
    pub actor: String,
    pub reason: String,
    pub config_fingerprint: String,
    pub plan_hash: String,
}

impl NativeStackRebasePlan {
    fn seal(mut self) -> Self {
        self.plan_hash.clear();
        self.plan_hash = crate::membership::fnv1a64(
            &serde_json::to_vec(&self).expect("native Stack rebase plan serializes"),
        );
        self
    }

    #[must_use]
    pub fn verify(&self) -> bool {
        let expected = self.plan_hash.clone();
        let mut material = self.clone();
        material.plan_hash.clear();
        self.schema_version == 1
            && !self.members.is_empty()
            && serde_json::to_vec(&material)
                .ok()
                .is_some_and(|bytes| crate::membership::fnv1a64(&bytes) == expected)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NativeStackRebaseOutput {
    pub plan: NativeStackRebasePlan,
    pub mutated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical: Option<crate::physical_rebase::AtomicRebaseReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drift_clear: Option<GitHubStackDriftClearReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<GitHubStackMutationReceipt>,
    pub fresh_ci_required: bool,
    pub next: String,
}

pub fn preview(
    context: &AppContext,
    input: &NativeStackRebasePreviewInput,
) -> Result<NativeStackRebasePlan, AppError> {
    let _planning = context.acquire_planning_operation("native-stack-rebase-preview")?;
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(context.config.sync.max_duration_secs);
    let status = read::status_with_deadline(context, deadline)?;
    plan_from_status(context, &status, input)
}

fn validated_text<'a>(value: &'a str, field: &str, max: usize) -> Result<&'a str, AppError> {
    if value.is_empty()
        || value.len() > max
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(AppError::validation(
            "native_stack_rebase_input_invalid",
            format!("native Stack rebase requires a trimmed bounded {field}"),
        ));
    }
    Ok(value)
}

pub(crate) fn plan_from_status(
    context: &AppContext,
    status: &StatusOutput,
    input: &NativeStackRebasePreviewInput,
) -> Result<NativeStackRebasePlan, AppError> {
    let actor = validated_text(&input.actor, "actor", 256)?;
    let reason = validated_text(&input.reason, "reason", 512)?;
    if context.config.stack_type != crate::config::StackType::Github
        || !context.config.stack_rollout.mutations_opt_in
    {
        return Err(AppError::validation(
            "native_stack_rebase_not_authorized",
            "native Stack rebase requires reviewed GitHub Stack mutation opt-in",
        ));
    }
    let native = status
        .stack_backend
        .native_stacks
        .iter()
        .find(|native| native.stack.number == input.stack)
        .ok_or_else(|| {
            AppError::validation(
                "native_stack_rebase_stack_missing",
                "requested provider Stack is absent from complete status",
            )
        })?;
    plan_from_native(context, status, native, actor, reason)
}

#[allow(clippy::too_many_lines)]
fn plan_from_native(
    context: &AppContext,
    status: &StatusOutput,
    native: &NativeStackStatus,
    actor: &str,
    reason: &str,
) -> Result<NativeStackRebasePlan, AppError> {
    if status.stack_backend.provider_stacks_truncated
        || matches!(
            native.consistency,
            StackConsistency::Unknown | StackConsistency::Orphaned
        )
    {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "native_stack_rebase_provider_incomplete",
            "complete exact provider Stack evidence is required before planning",
            Some(json!({"stack": native, "mutated": false})),
        ));
    }
    let repairable = [
        "native_stack_rebase_required",
        "github_stack_pr_base_drift",
        "github_stack_head_drift",
    ];
    let unexpected = native
        .problems
        .iter()
        .filter(|problem| !repairable.contains(&problem.code.as_str()))
        .collect::<Vec<_>>();
    if !unexpected.is_empty() {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "native_stack_rebase_topology_unhealthy",
            "native Stack rebase cannot repair incomplete, ambiguous, or membership drift",
            Some(json!({"problems": unexpected, "mutated": false})),
        ));
    }
    let caravan_id = native.caravan_id.ok_or_else(|| {
        AppError::validation(
            "native_stack_rebase_caravan_missing",
            "provider Stack does not map to one complete current Cara fleet",
        )
    })?;
    let caravan = status
        .analysis
        .fleet
        .caravans
        .iter()
        .find(|caravan| caravan.id == caravan_id)
        .ok_or_else(|| {
            AppError::validation(
                "native_stack_rebase_caravan_missing",
                "provider Stack mapping is absent from the current Cara fleet",
            )
        })?;
    let active = caravan
        .members
        .iter()
        .map(|number| {
            status.analysis.pull_requests.get(number).ok_or_else(|| {
                AppError::validation(
                    "native_stack_rebase_member_missing",
                    "one active fleet member is absent from complete status",
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let root_candidate_stale = status
        .merge_candidates
        .iter()
        .find(|candidate| candidate.pr == active[0].number)
        .is_some_and(|candidate| candidate.stale_base || candidate.stale_head);
    let root_stale = root_candidate_stale
        || active[0].base.repository != status.analysis.fleet.default_branch.repository
        || active[0].base.name != status.analysis.fleet.default_branch.name
        || active[0].base.oid != status.analysis.fleet.default_branch.oid;
    let first_diverged = if root_stale {
        Some(0)
    } else {
        (1..active.len()).find(|position| {
            native
                .ancestry
                .iter()
                .find(|edge| edge.child_pr == active[*position].number)
                .is_none_or(|edge| !edge.linear)
        })
    }
    .ok_or_else(|| {
        AppError::structured(
            ErrorCategory::Validation,
            "native_stack_rebase_not_required",
            "the active fleet root and every adjacent head already match current ancestry",
            Some(json!({"stack": native.stack.number, "mutated": false})),
        )
    })?;
    if native.ancestry.iter().any(|edge| {
        caravan.members[first_diverged..].contains(&edge.child_pr)
            && matches!(
                edge.relation,
                crate::generation::CommitRelation::Unknown { .. }
            )
    }) {
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "native_stack_rebase_ancestry_unknown",
            "an unknown active-suffix commit relation cannot authorize branch rewrites",
            Some(json!({"ancestry": native.ancestry, "mutated": false})),
        ));
    }
    let mut members = Vec::new();
    for position in first_diverged..active.len() {
        let pull = active[position];
        let (parent_pr, parent) = if position == 0 {
            (None, &status.analysis.fleet.default_branch)
        } else {
            (
                Some(active[position - 1].number),
                &active[position - 1].head,
            )
        };
        members.push(NativeStackRebaseMemberPlan {
            position: u32::try_from(position).unwrap_or(u32::MAX),
            pr: pull.number,
            branch: pull.head.name.clone(),
            old_head: pull.head.oid.clone(),
            old_base: pull.base.clone(),
            parent_pr,
            parent_branch: parent.name.clone(),
            parent_head: parent.oid.clone(),
        });
    }
    let config_fingerprint = crate::membership::fnv1a64(
        &serde_json::to_vec(&json!({
            "version": context.config.version,
            "stack_type": context.config.stack_type,
            "stack_rollout": context.config.stack_rollout,
            "writer": context.config.writer,
            "sync": context.config.sync,
        }))
        .expect("native Stack rebase config serializes"),
    );
    Ok(NativeStackRebasePlan {
        schema_version: 1,
        repository: status.repository.clone(),
        stack: native.stack.number,
        root_pr: caravan_id,
        stack_base_ref: native.stack.base.ref_name.clone(),
        provider_before: native.stack.clone(),
        members,
        actor: actor.to_owned(),
        reason: reason.to_owned(),
        config_fingerprint,
        plan_hash: String::new(),
    }
    .seal())
}

#[allow(clippy::too_many_lines)]
fn receipt_key(plan_hash: &str) -> Result<String, AppError> {
    if plan_hash.is_empty()
        || plan_hash.len() > 96
        || !plan_hash
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, ':' | '-'))
    {
        return Err(AppError::validation(
            "native_stack_rebase_plan_hash_invalid",
            "native Stack rebase plan hash is not a bounded Cara fingerprint",
        ));
    }
    Ok(format!("rebase-{}", plan_hash.replace(':', "-")))
}

fn read_postcondition(
    context: &AppContext,
    deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<StatusOutput, AppError> {
    github_budget.map_or_else(
        || read::status_with_deadline(context, deadline),
        |budget| read::status_with_deadline_and_budget(context, deadline, Some(budget)),
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn apply_plan(
    context: &AppContext,
    status: &StatusOutput,
    plan: NativeStackRebasePlan,
    _stack: u64,
    key: &str,
    writer: &crate::writer_guard::WriterOperationGuard,
    deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<(NativeStackRebaseOutput, StatusOutput), AppError> {
    let timeout = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let mut prepared = Vec::with_capacity(plan.members.len());
    let mut target = crate::physical_rebase::PlannedBase::Remote(BranchSnapshot {
        repository: plan.repository.clone(),
        name: plan.members[0].parent_branch.clone(),
        oid: plan.members[0].parent_head.clone(),
    });
    for member in &plan.members {
        let candidate = status
            .analysis
            .pull_requests
            .get(&member.pr)
            .expect("sealed member has status facts");
        let parent = BranchSnapshot {
            repository: plan.repository.clone(),
            name: member.parent_branch.clone(),
            oid: member.parent_head.clone(),
        };
        let item = crate::physical_rebase::prepare_candidate(
            &context.repository_path,
            &plan.repository,
            candidate,
            crate::physical_rebase::range_base_for_rewritten_parent(candidate, &parent),
            target,
            &status.analysis.fleet.default_branch,
            crate::physical_rebase::RebaseExecutionBudget::new(timeout)
                .with_deadline(deadline)
                .with_writer_fence(writer.remote_fence())
                .because(member.parent_pr.map_or(
                    crate::physical_rebase::BranchRewriteReason::CurrentDefaultAdvanced,
                    |parent_pr| crate::physical_rebase::BranchRewriteReason::ParentAdvanced {
                        parent_pr,
                    },
                )),
        )?;
        target = crate::physical_rebase::PlannedBase::Simulated(BranchSnapshot {
            repository: plan.repository.clone(),
            name: member.branch.clone(),
            oid: item.plan.new_head_oid.clone(),
        });
        prepared.push(item);
    }
    let prepared_refs = prepared.iter().collect::<Vec<_>>();
    let physical = crate::physical_rebase::apply_prepared_atomically(&prepared_refs)?;
    let rewritten_status =
        read_postcondition(context, deadline, github_budget).map_err(|error| {
            AppError::structured(
                ErrorCategory::ExecutionFailure,
                "native_stack_rebase_postcondition_read_failed",
                "atomic branch rewrite completed but provider rediscovery failed",
                Some(json!({
                    "plan": plan,
                    "physical": physical,
                    "source": error.details(),
                    "mutated": true,
                    "resumable": true,
                })),
            )
        })?;
    let caravan = rewritten_status
        .analysis
        .fleet
        .caravans
        .iter()
        .find(|caravan| caravan.id == plan.root_pr)
        .ok_or_else(|| {
            AppError::structured(
                ErrorCategory::ExecutionFailure,
                "native_stack_rebase_postcondition_caravan_missing",
                "rewritten active fleet is absent before provider Stack replacement",
                Some(json!({"plan": plan, "physical": physical, "mutated": true})),
            )
        })?;
    let pulls = caravan
        .members
        .iter()
        .map(|number| {
            rewritten_status
                .analysis
                .pull_requests
                .get(number)
                .ok_or_else(|| {
                    AppError::validation(
                        "native_stack_rebase_postcondition_member_missing",
                        "rewritten active member is absent before provider Stack replacement",
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let desired = crate::stack_membership::topology_from_members(
        &rewritten_status.analysis.fleet.default_branch,
        pulls.iter().copied(),
    )
    .map_err(|error| {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "native_stack_rebase_replacement_topology_invalid",
            error.to_string(),
            Some(json!({"plan": plan, "physical": physical, "mutated": true})),
        )
    })?;
    let provider_runner = ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(timeout)
        .with_operation_deadline(deadline);
    let provider = GitHubMutationAdapter::new(writer.runner(provider_runner));
    let clear_operation = format!("native-stack-rebase-clear-{}", plan.plan_hash);
    let drift_clear = provider
        .native_stack_clear_drifted(
            &plan.repository,
            &clear_operation,
            &plan.actor,
            &plan.provider_before,
        )
        .map_err(|error| AppError::structured(
            ErrorCategory::ExecutionFailure,
            "native_stack_rebase_drift_clear_failed",
            "branch rewrite completed but exact drifted provider Stack clearance failed",
            Some(json!({
                "plan": plan,
                "physical": physical,
                "source": error.to_string(),
                "mutated": true,
                "resumable": true,
                "safe_next_action": "rerun rebase-preview against fresh provider truth; never unstack or force-push manually",
            })),
        ))?;
    let replacement = provider
        .native_stack_create(
            &plan.repository,
            &GitHubStackCreatePlan {
                operation_id: format!("native-stack-rebase-rebuild-{}", plan.plan_hash),
                actor: plan.actor.clone(),
                desired,
            },
        )
        .map_err(|error| AppError::structured(
            ErrorCategory::ExecutionFailure,
            "native_stack_rebase_rebuild_failed",
            "drifted Stack was cleared but exact replacement creation failed",
            Some(json!({
                "plan": plan,
                "physical": physical,
                "drift_clear": drift_clear,
                "source": error.to_string(),
                "mutated": true,
                "resumable": true,
                "safe_next_action": format!("run native-stack recovery-preview --root {} to resume exact replacement creation", plan.root_pr),
            })),
        ))?;
    let final_status = read_postcondition(context, deadline, github_budget)?;
    let final_native = final_status
        .stack_backend
        .native_stacks
        .iter()
        .find(|native| native.caravan_id == Some(plan.root_pr))
        .ok_or_else(|| AppError::structured(
            ErrorCategory::ExecutionFailure,
            "native_stack_rebase_postcondition_stack_missing",
            "replacement Stack creation returned but exact active-fleet mapping is absent",
            Some(json!({"plan": plan, "physical": physical, "replacement": replacement, "mutated": true})),
        ))?;
    if final_native.consistency != StackConsistency::Exact
        || final_native.ancestry.iter().any(|edge| !edge.linear)
    {
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "native_stack_rebase_postcondition_diverged",
            "replacement provider Stack is not exact and linear",
            Some(json!({
                "plan": plan,
                "physical": physical,
                "drift_clear": drift_clear,
                "replacement": replacement,
                "native": final_native,
                "mutated": true,
            })),
        ));
    }
    let output = NativeStackRebaseOutput {
        plan,
        mutated: true,
        physical: Some(physical),
        drift_clear: Some(drift_clear),
        replacement: Some(replacement),
        fresh_ci_required: true,
        next: "wait for fresh CI only on rewritten heads, then rerun cara sync".to_owned(),
    };
    crate::stack_checkpoint::write(&context.repository_path, key, &output)?;
    Ok((output, final_status))
}

fn automatic_rebase_stack(
    backend: &crate::read::StackBackendStatus,
) -> Result<Option<u64>, AppError> {
    if backend.provider_stacks_truncated
        || backend.capability != crate::read::StackCapability::Available
        || backend.mutation_support != crate::read::StackMutationSupport::NativeStack
    {
        return Ok(None);
    }
    if backend.problems.iter().any(|problem| {
        ![
            "native_stack_rebase_required",
            "github_stack_pr_base_drift",
            "github_stack_head_drift",
        ]
        .contains(&problem.code.as_str())
    }) {
        return Ok(None);
    }
    let divergent = backend
        .native_stacks
        .iter()
        .filter(|native| {
            !native.problems.is_empty()
                && native
                    .problems
                    .iter()
                    .all(|problem| problem.code == "native_stack_rebase_required")
        })
        .map(|native| native.stack.number)
        .collect::<Vec<_>>();
    match divergent.as_slice() {
        [] => Ok(None),
        [stack] => Ok(Some(*stack)),
        _ => Err(AppError::structured(
            ErrorCategory::Validation,
            "native_stack_auto_rebase_ambiguous",
            "multiple divergent native Stacks cannot share one automatic rewrite tick",
            Some(json!({"stacks": divergent, "mutated": false})),
        )),
    }
}

pub(crate) fn auto_apply_from_status(
    context: &AppContext,
    status: &StatusOutput,
    writer: &crate::writer_guard::WriterOperationGuard,
    deadline: std::time::Instant,
    github_budget: &crate::command::GithubRequestBudget,
) -> Result<Option<(NativeStackRebaseOutput, StatusOutput)>, AppError> {
    let Some(stack) = automatic_rebase_stack(&status.stack_backend)? else {
        return Ok(None);
    };
    let intent = NativeStackRebasePreviewInput {
        stack,
        actor: "caravan-scheduler".to_owned(),
        reason: "automatic exact native Stack ancestry convergence".to_owned(),
    };
    let plan = plan_from_status(context, status, &intent)?;
    let key = receipt_key(&plan.plan_hash)?;
    if let Some(output) =
        crate::stack_checkpoint::load::<NativeStackRebaseOutput>(&context.repository_path, &key)?
    {
        let final_status = read_postcondition(context, deadline, Some(github_budget))?;
        return Ok(Some((output, final_status)));
    }
    apply_plan(
        context,
        status,
        plan,
        stack,
        &key,
        writer,
        deadline,
        Some(github_budget),
    )
    .map(Some)
}

pub fn apply(
    context: &AppContext,
    input: &NativeStackRebaseApplyInput,
) -> Result<NativeStackRebaseOutput, AppError> {
    let key = receipt_key(&input.expected_plan_hash)?;
    if let Some(output) =
        crate::stack_checkpoint::load::<NativeStackRebaseOutput>(&context.repository_path, &key)?
    {
        if output.plan.plan_hash == input.expected_plan_hash {
            return Ok(output);
        }
    }
    let writer = context.acquire_writer_operation("native-stack-rebase-apply")?;
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(context.config.sync.max_duration_secs);
    let status = read::status_with_deadline(context, deadline)?;
    let intent = NativeStackRebasePreviewInput {
        stack: input.stack,
        actor: input.actor.clone(),
        reason: input.reason.clone(),
    };
    let plan = plan_from_status(context, &status, &intent)?;
    if plan.plan_hash != input.expected_plan_hash || !plan.verify() {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "native_stack_rebase_plan_stale",
            "native Stack rebase facts changed after preview",
            Some(json!({
                "expected_plan_hash": input.expected_plan_hash,
                "actual_plan": plan,
                "mutated": false,
            })),
        ));
    }
    apply_plan(
        context,
        &status,
        plan,
        input.stack,
        &key,
        &writer,
        deadline,
        None,
    )
    .map(|(output, _)| output)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn divergent_stack(number: u64) -> NativeStackStatus {
        NativeStackStatus {
            stack: crate::github::GitHubStackSnapshot {
                id: number,
                number,
                node_id: format!("S_{number}"),
                base: crate::github::GitHubStackBase {
                    ref_name: "main".to_owned(),
                },
                open: true,
                created_at: String::new(),
                pull_requests: Vec::new(),
            },
            caravan_id: Some(PrNumber(number)),
            consistency: StackConsistency::Drifted,
            ancestry: Vec::new(),
            problems: vec![crate::read::StackBackendProblem {
                code: "native_stack_rebase_required".to_owned(),
                message: "diverged".to_owned(),
            }],
        }
    }

    fn backend(stacks: Vec<NativeStackStatus>) -> crate::read::StackBackendStatus {
        crate::read::StackBackendStatus {
            configured: crate::config::StackType::Github,
            capability: crate::read::StackCapability::Available,
            mutation_support: crate::read::StackMutationSupport::NativeStack,
            native_stacks: stacks,
            provider_stacks_truncated: false,
            missing_caravans: Vec::new(),
            problems: vec![crate::read::StackBackendProblem {
                code: "native_stack_rebase_required".to_owned(),
                message: "diverged".to_owned(),
            }],
        }
    }

    #[test]
    fn automatic_rebase_selects_only_one_pure_ancestry_drift() {
        assert_eq!(automatic_rebase_stack(&backend(Vec::new())).unwrap(), None);
        assert_eq!(
            automatic_rebase_stack(&backend(vec![divergent_stack(42)])).unwrap(),
            Some(42)
        );
        assert_eq!(
            automatic_rebase_stack(&backend(vec![divergent_stack(42), divergent_stack(43)]))
                .unwrap_err()
                .code(),
            "native_stack_auto_rebase_ambiguous"
        );
        let mut unavailable = backend(vec![divergent_stack(42)]);
        unavailable.provider_stacks_truncated = true;
        assert_eq!(automatic_rebase_stack(&unavailable).unwrap(), None);
        let mut mixed = backend(vec![divergent_stack(42)]);
        mixed.problems.push(crate::read::StackBackendProblem {
            code: "github_stack_member_order_drift".to_owned(),
            message: "mixed".to_owned(),
        });
        assert_eq!(automatic_rebase_stack(&mixed).unwrap(), None);
    }

    #[test]
    fn plan_hash_binds_every_member_generation_and_intent() {
        let plan = NativeStackRebasePlan {
            schema_version: 1,
            repository: repository(),
            stack: 2818,
            root_pr: PrNumber(2738),
            stack_base_ref: "main".to_owned(),
            provider_before: divergent_stack(2818).stack,
            members: vec![NativeStackRebaseMemberPlan {
                position: 2,
                pr: PrNumber(2817),
                branch: "third".to_owned(),
                old_head: CommitOid("3333333333333333333333333333333333333333".to_owned()),
                old_base: branch("child", "2222222222222222222222222222222222222222"),
                parent_pr: Some(PrNumber(2814)),
                parent_branch: "child".to_owned(),
                parent_head: CommitOid("2222222222222222222222222222222222222222".to_owned()),
            }],
            actor: "operator".to_owned(),
            reason: "linearize exact Stack".to_owned(),
            config_fingerprint: "fnv1a64:config".to_owned(),
            plan_hash: String::new(),
        }
        .seal();
        assert!(plan.verify());
        let mut drifted = plan.clone();
        drifted.members[0].old_head =
            CommitOid("4444444444444444444444444444444444444444".to_owned());
        assert!(!drifted.verify());
    }

    #[test]
    fn receipt_key_is_bounded_and_filesystem_safe() {
        assert_eq!(
            receipt_key("fnv1a64:0123456789abcdef").unwrap(),
            "rebase-fnv1a64-0123456789abcdef"
        );
        assert!(receipt_key("").is_err());
        assert!(receipt_key("not/a/hash").is_err());
    }

    #[test]
    fn actor_and_reason_are_bounded_and_trimmed() {
        assert_eq!(
            validated_text("operator", "actor", 256).unwrap(),
            "operator"
        );
        for invalid in ["", " operator", "operator ", "operator\n"] {
            assert!(validated_text(invalid, "actor", 256).is_err());
        }
    }
}
