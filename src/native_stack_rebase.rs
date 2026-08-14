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
    pub parent_pr: PrNumber,
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
    let unexpected = native
        .problems
        .iter()
        .filter(|problem| problem.code != "native_stack_rebase_required")
        .collect::<Vec<_>>();
    if !unexpected.is_empty() {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "native_stack_rebase_topology_unhealthy",
            "native Stack rebase cannot repair membership, base, head, or provider-read drift",
            Some(json!({"problems": unexpected, "mutated": false})),
        ));
    }
    let first_diverged = native
        .ancestry
        .iter()
        .position(|edge| !edge.linear)
        .ok_or_else(|| {
            AppError::structured(
                ErrorCategory::Validation,
                "native_stack_rebase_not_required",
                "every adjacent Stack head already contains its predecessor",
                Some(json!({"stack": native.stack.number, "mutated": false})),
            )
        })?;
    if native.ancestry[first_diverged..].iter().any(|edge| {
        matches!(
            edge.relation,
            crate::generation::CommitRelation::Unknown { .. }
        )
    }) {
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "native_stack_rebase_ancestry_unknown",
            "an unknown adjacent commit relation cannot authorize branch rewrites",
            Some(json!({"ancestry": native.ancestry, "mutated": false})),
        ));
    }
    let open = native
        .stack
        .pull_requests
        .iter()
        .filter(|entry| entry.state.eq_ignore_ascii_case("open"))
        .collect::<Vec<_>>();
    let start = first_diverged + 1;
    let mut members = Vec::new();
    for position in start..open.len() {
        let entry = open[position];
        let parent = open[position - 1];
        let pull = status
            .analysis
            .pull_requests
            .get(&PrNumber(entry.number))
            .ok_or_else(|| {
                AppError::validation(
                    "native_stack_rebase_member_missing",
                    "one planned Stack member is absent from status",
                )
            })?;
        members.push(NativeStackRebaseMemberPlan {
            position: u32::try_from(position).unwrap_or(u32::MAX),
            pr: pull.number,
            branch: pull.head.name.clone(),
            old_head: pull.head.oid.clone(),
            old_base: pull.base.clone(),
            parent_pr: PrNumber(parent.number),
            parent_branch: parent.head.ref_name.clone(),
            parent_head: parent.head.sha.clone(),
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
        root_pr: native.caravan_id.unwrap_or(PrNumber(open[0].number)),
        stack_base_ref: native.stack.base.ref_name.clone(),
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

#[allow(clippy::too_many_lines)]
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
                .because(
                    crate::physical_rebase::BranchRewriteReason::ParentAdvanced {
                        parent_pr: member.parent_pr,
                    },
                ),
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
    let final_status = read::status_with_deadline(context, deadline).map_err(|error| {
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
    let final_native = final_status
        .stack_backend
        .native_stacks
        .iter()
        .find(|native| native.stack.number == input.stack)
        .ok_or_else(|| {
            AppError::structured(
                ErrorCategory::ExecutionFailure,
                "native_stack_rebase_postcondition_stack_missing",
                "atomic branch rewrite completed but Stack rediscovery is absent",
                Some(json!({"plan": plan, "physical": physical, "mutated": true})),
            )
        })?;
    if final_native.ancestry.iter().any(|edge| !edge.linear) {
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "native_stack_rebase_postcondition_diverged",
            "atomic branch rewrite completed but provider ancestry is still divergent",
            Some(json!({
                "plan": plan,
                "physical": physical,
                "ancestry": final_native.ancestry,
                "mutated": true,
            })),
        ));
    }
    let output = NativeStackRebaseOutput {
        plan,
        mutated: physical
            .receipts
            .iter()
            .any(|receipt| !receipt.already_satisfied),
        physical: Some(physical),
        fresh_ci_required: true,
        next: "wait for fresh CI on every rewritten head, then rerun cara sync".to_owned(),
    };
    crate::stack_checkpoint::write(&context.repository_path, &key, &output)?;
    Ok(output)
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

    #[test]
    fn plan_hash_binds_every_member_generation_and_intent() {
        let plan = NativeStackRebasePlan {
            schema_version: 1,
            repository: repository(),
            stack: 2818,
            root_pr: PrNumber(2738),
            stack_base_ref: "main".to_owned(),
            members: vec![NativeStackRebaseMemberPlan {
                position: 2,
                pr: PrNumber(2817),
                branch: "third".to_owned(),
                old_head: CommitOid("3333333333333333333333333333333333333333".to_owned()),
                old_base: branch("child", "2222222222222222222222222222222222222222"),
                parent_pr: PrNumber(2814),
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
