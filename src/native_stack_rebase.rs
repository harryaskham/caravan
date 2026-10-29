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
    /// bd-7efe6a: members a queue-owned repair released because they could not
    /// be replayed linearly, with their typed repair request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eviction: Option<NativeStackConflictEviction>,
    /// bd-7efe6a: survivor PR bases pointed at their linear predecessor (the
    /// root at the default branch) before the exact rebuild.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retargets: Vec<crate::github::GitHubMutationReceipt>,
    pub fresh_ci_required: bool,
    pub next: String,
}

const DRIFT_CLEAR_REDISCOVERY_ATTEMPTS: usize = 3;
const DRIFT_CLEAR_REDISCOVERY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

fn same_drift_clear_scope(before: &GitHubStackSnapshot, after: &GitHubStackSnapshot) -> bool {
    before.id == after.id
        && before.number == after.number
        && before.node_id == after.node_id
        && before.base == after.base
        && before
            .pull_requests
            .iter()
            .map(|entry| entry.number)
            .eq(after.pull_requests.iter().map(|entry| entry.number))
}

fn drift_clear_generation_changed(error: &crate::github::GitHubStackMutationError) -> bool {
    matches!(
        error,
        crate::github::GitHubStackMutationError::InconsistentProviderState { diagnostic }
            if diagnostic.contains("raw generation changed before mutation")
    )
}

fn clear_drifted_with_bounded_rediscovery<R: crate::command::CommandRunner>(
    provider: &GitHubMutationAdapter<R>,
    repository: &RepositoryId,
    operation_id: &str,
    actor: &str,
    expected: &GitHubStackSnapshot,
    deadline: std::time::Instant,
) -> Result<GitHubStackDriftClearReceipt, crate::github::GitHubStackMutationError> {
    let mut current = expected.clone();
    for attempt in 0..DRIFT_CLEAR_REDISCOVERY_ATTEMPTS {
        match provider.native_stack_clear_drifted(repository, operation_id, actor, &current) {
            Ok(receipt) => return Ok(receipt),
            Err(error)
                if drift_clear_generation_changed(&error)
                    && attempt + 1 < DRIFT_CLEAR_REDISCOVERY_ATTEMPTS
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(DRIFT_CLEAR_REDISCOVERY_DELAY);
                match provider.native_stack_snapshot_for_recovery(repository, current.number)? {
                    None => {
                        return Ok(GitHubStackDriftClearReceipt {
                            schema_version: 1,
                            operation_id: operation_id.to_owned(),
                            actor: actor.to_owned(),
                            before: current,
                            cleared: true,
                            recovered_after_ambiguous_response: true,
                        });
                    }
                    Some(observed) if same_drift_clear_scope(&current, &observed) => {
                        current = observed;
                    }
                    Some(observed) => {
                        return Err(
                            crate::github::GitHubStackMutationError::InconsistentProviderState {
                                diagnostic: format!(
                                    "drift-clear Stack #{} changed identity or ordered membership during bounded rediscovery: expected id {} node {} PRs {:?}, observed id {} node {} PRs {:?}",
                                    current.number,
                                    current.id,
                                    current.node_id,
                                    current
                                        .pull_requests
                                        .iter()
                                        .map(|entry| entry.number)
                                        .collect::<Vec<_>>(),
                                    observed.id,
                                    observed.node_id,
                                    observed
                                        .pull_requests
                                        .iter()
                                        .map(|entry| entry.number)
                                        .collect::<Vec<_>>(),
                                ),
                            },
                        );
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("bounded drift-clear loop always returns")
}

pub fn preview(
    context: &AppContext,
    input: &NativeStackRebasePreviewInput,
) -> Result<NativeStackRebasePlan, AppError> {
    let _planning = context.acquire_planning_operation("native-stack-rebase-preview")?;
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(context.config.sync.max_duration_secs);
    // The stack number is the complete subject identity. A targeted preview
    // must not depend on whichever historical branch the caller's checkout
    // happens to have selected; fleet discovery deliberately declines to
    // resolve that unrelated local branch while retaining all selected-Stack
    // validation below.
    let status = read::fleet_status(context, deadline, None)?;
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
    plan_from_native(context, status, native, actor, reason, PlanScope::Ordinary)
}

/// Which facts a sealed native Stack rebase plan may repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanScope {
    /// Explicit owner preview/apply and bd-7ff76d routine drift: only a proven
    /// merged prefix (or same-head duplicate) may be retained history.
    Ordinary,
    /// bd-7efe6a: queue-owned rebuild of a mapped Stack whose retained closed
    /// rows (merged or closed unmerged, possibly interleaved with the open
    /// suffix) defeat automatic merged-prefix recovery. Closed rows are pruned
    /// from the replacement representation, never rewritten, and every active
    /// member is replayed linearly from the root.
    ClosedHistory,
}

/// Problems a queue-owned closed-history rebuild resolves by construction:
/// representation and ancestry drift of the active members plus the closed
/// rows it prunes. Unknown ancestry, orphaned/terminal Stacks, Stack base drift
/// and inventory problems are never in this set.
const CLOSED_HISTORY_REPAIRABLE_PROBLEMS: [&str; 7] = [
    "native_stack_rebase_required",
    "github_stack_pr_base_drift",
    "github_stack_head_drift",
    "github_stack_member_order_drift",
    "github_stack_pr_missing",
    "github_stack_merged_prefix_invalid",
    "github_stack_state_order_invalid",
];

#[allow(clippy::too_many_lines)]
fn plan_from_native(
    context: &AppContext,
    status: &StatusOutput,
    native: &NativeStackStatus,
    actor: &str,
    reason: &str,
    scope: PlanScope,
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
    let closed_history = scope == PlanScope::ClosedHistory;
    let retained_closed_rows = proven_retained_closed_rows(native);
    let active_prefix = proven_active_fleet_prefix(status, native);
    // A provider-visible strict prefix can lag an ordinary join. Rebuilding
    // that representation must also replay the newly joined suffix; it is not
    // a membership-only repair merely because the absent rows have no
    // provider ancestry edges yet (bd-28ae57).
    let representation_only_membership_repair = !closed_history
        && !active_prefix
        && !native.problems.is_empty()
        && native
            .problems
            .iter()
            .all(|problem| problem.code == "github_stack_member_order_drift");
    let unexpected = native
        .problems
        .iter()
        .filter(|problem| {
            if closed_history {
                return !CLOSED_HISTORY_REPAIRABLE_PROBLEMS.contains(&problem.code.as_str());
            }
            !(repairable_problem(problem, retained_closed_rows, active_prefix)
                || (representation_only_membership_repair
                    && problem.code == "github_stack_member_order_drift"))
        })
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
    // A closed-history rebuild always replaces the whole representation, so
    // every active member is replayed linearly from the exact default branch.
    let representation_drift = closed_history
        || native.problems.iter().any(|problem| {
            matches!(
                problem.code.as_str(),
                "github_stack_pr_base_drift" | "github_stack_head_drift"
            ) || (retained_closed_rows
                && matches!(
                    problem.code.as_str(),
                    "github_stack_pr_missing" | "github_stack_merged_prefix_invalid"
                ))
                || (active_prefix && problem.code == "github_stack_member_order_drift")
        });
    let root_candidate_stale = status
        .merge_candidates
        .iter()
        .find(|candidate| candidate.pr == active[0].number)
        .is_some_and(|candidate| candidate.stale_base || candidate.stale_head);
    let root_stale = root_candidate_stale
        || active[0].base.repository != status.analysis.fleet.default_branch.repository
        || active[0].base.name != status.analysis.fleet.default_branch.name
        || active[0].base.oid != status.analysis.fleet.default_branch.oid;
    let first_diverged = if representation_only_membership_repair {
        None
    } else if root_stale || representation_drift {
        Some(0)
    } else {
        (1..active.len()).find(|position| {
            native
                .ancestry
                .iter()
                .find(|edge| edge.child_pr == active[*position].number)
                .is_none_or(|edge| !edge.linear)
        })
    };
    if !representation_only_membership_repair && first_diverged.is_none() {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "native_stack_rebase_not_required",
            "the active fleet root and every adjacent head already match current ancestry",
            Some(json!({"stack": native.stack.number, "mutated": false})),
        ));
    }
    let first_diverged = first_diverged.unwrap_or(active.len());
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
pub(crate) fn receipt_key(plan_hash: &str) -> Result<String, AppError> {
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
    read::fleet_status(context, deadline, github_budget)
}

fn owner_continuation_refusal(
    error: AppError,
    plan: &NativeStackRebasePlan,
    member: &NativeStackRebaseMemberPlan,
) -> AppError {
    if error.code() != "rebase_topology_changed" {
        return error;
    }
    AppError::structured(
        error.category(),
        error.code(),
        "source replay could not preserve topology; return this generation to its authorized source owner without relaxing the guard or retrying force publication",
        Some(json!({
            "source": error.details(),
            "repository": plan.repository,
            "stack": plan.stack,
            "plan_hash": plan.plan_hash,
            "member": member,
            "this_attempt_source_published": false,
            "prior_operation_outcomes": "unchanged; must be reconciled independently",
            "owner_continuation": {
                "tool": "repair_start_non_force",
                "input": {"pr": member.pr, "target_pr": member.parent_pr},
                "required_custody_fields": ["actor", "reason"],
                "continue_requires": ["same actor", "--no-sync"],
                "scope": "one authorized same-repository source owner; no source takeover or queue mutation",
                "next": "reconcile previous uncertainty and establish custody first; start from fresh provider facts, preserve the old head in a merge, then rediscover the child after its parent changes. This handoff is not an apply lease",
            },
            "force_boundary": "explicit native rebase and legacy repair use force-with-lease; neither is a non-force fallback, and ordinary sync rewrites only through its one gated queue-owned drift repair, which evicts rather than forces an unreplayable member",
            "queue_only_boundary": "there is no standalone sealed tail-eviction preview CLI; typed evict is a separate authorized mutation with its own preflight, not a command to inspect or a generic retry",
        })),
    )
}

/// Mark a refusal raised while preparing local rewrites, before any branch or
/// provider write, so a caller can tell it from a partial publication.
fn before_publication(error: AppError) -> AppError {
    let mut details = error.details().unwrap_or_else(|| json!({}));
    let Some(object) = details.as_object_mut() else {
        return error;
    };
    object.insert("publication_started".to_owned(), json!(false));
    object
        .entry("mutated".to_owned())
        .or_insert_with(|| json!(false));
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

/// Audited identity of queue-owned drift repair.
pub(crate) const SYNC_DRIFT_REPAIR_ACTOR: &str = "caravan-scheduler";
const SYNC_DRIFT_REPAIR_REASON: &str =
    "routine native Stack drift repaired by the queue (bd-7ff76d)";
const SYNC_CLOSED_HISTORY_REPAIR_REASON: &str =
    "native Stack holding closed history rebuilt by the queue (bd-7efe6a)";
/// Descriptive identity sealed into the plan; used only for refresh reporting.
pub(crate) const SYNC_STALE_BASE_REPAIR_REASON: &str =
    "refreshing stale base: replay the root and descendants onto current default (bd-c299b8)";

/// Outcome of one sync-owned drift repair attempt.
pub(crate) struct SyncDriftRepair {
    pub output: NativeStackRebaseOutput,
    pub status: StatusOutput,
}

/// Which queue-owned repair a sync tick is attempting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncRepairKind {
    /// bd-7ff76d: ancestry or PR base/head drift of a quarantinable Stack.
    RoutineDrift,
    /// bd-c299b8: an otherwise exact Stack whose root missed default-branch content.
    StaleBase,
    /// bd-7efe6a: a Stack whose retained closed rows defeat automatic
    /// merged-prefix recovery.
    ClosedHistory,
}

/// Whether the repository authorizes any queue-owned native Stack rewrite.
fn queue_repair_authorized(context: &AppContext, status: &StatusOutput) -> bool {
    context.config.stack_type == crate::config::StackType::Github
        && context.config.stack_rollout.mutations_opt_in
        && !context.config.stack_rollout.manual_drift_repair
        && !status.stack_backend.provider_stacks_truncated
}

/// The mapped caravan a queue-owned repair may act on: present, unparked and
/// not held by an explicit pause.
fn repairable_caravan<'a>(
    status: &'a StatusOutput,
    native: &NativeStackStatus,
    held: &std::collections::BTreeSet<PrNumber>,
) -> Option<&'a crate::model::Caravan> {
    let id = native.caravan_id?;
    if held.contains(&id) {
        return None;
    }
    status
        .analysis
        .fleet
        .caravans
        .iter()
        .find(|caravan| caravan.id == id && !caravan.parked)
}

/// Sealed queue-owned repair plans for every eligible drifted Stack, in
/// canonical caravan order. Pure: no provider, Git or filesystem access.
pub(crate) fn drift_repair_plans<'a>(
    context: &AppContext,
    status: &'a StatusOutput,
) -> Vec<(&'a NativeStackStatus, NativeStackRebasePlan)> {
    if !queue_repair_authorized(context, status) {
        return Vec::new();
    }
    let held = crate::stack_policy::held_caravan_members(status);
    let mut candidates = status
        .stack_backend
        .native_stacks
        .iter()
        .filter(|native| read::native_stack_quarantinable(native))
        .filter(|native| {
            native
                .problems
                .iter()
                .any(|problem| problem.code != "github_stack_member_order_drift")
                || proven_active_fleet_prefix(status, native)
        })
        .filter(|native| repairable_caravan(status, native, &held).is_some())
        .collect::<Vec<_>>();
    candidates.sort_by_key(|native| native.caravan_id);
    candidates
        .into_iter()
        .filter_map(|native| {
            plan_from_native(
                context,
                status,
                native,
                SYNC_DRIFT_REPAIR_ACTOR,
                SYNC_DRIFT_REPAIR_REASON,
                PlanScope::Ordinary,
            )
            .ok()
            .map(|plan| (native, plan))
        })
        .collect()
}

/// Whether an otherwise exact Stack needs a physical root refresh. A stale PR
/// base projection alone is not enough: GitHub can retain it after a successful
/// rewrite. Require exact evidence that current default adds unvalidated content
/// (or conflicts), never an absent/unknown comparison. Identical-tree divergence
/// stays with the retained-patch-set safety gate, not automatic replay.
fn stale_base_refresh_required(status: &StatusOutput, native: &NativeStackStatus) -> bool {
    if !native.stack.open
        || native.consistency != StackConsistency::Exact
        || !native.problems.is_empty()
    {
        return false;
    }
    let Some(root) = native
        .caravan_id
        .and_then(|id| status.analysis.pull_requests.get(&id))
    else {
        return false;
    };
    let default = &status.analysis.fleet.default_branch;
    if root.state != crate::model::PullRequestState::Open
        || root.draft
        || root.base.repository != default.repository
        || root.base.name != default.name
    {
        return false;
    }
    let stale = root.base.oid != default.oid
        || status.merge_candidates.iter().any(|candidate| {
            candidate.pr == root.number
                && candidate.base == root.base
                && candidate.head == root.head
                && candidate.compared_base.as_ref() == Some(default)
                && candidate.freshness == crate::model::MergeCandidateFreshness::StaleBase
                && candidate.stale_base
                && !candidate.stale_head
        });
    stale
        && (status.analysis.cumulative_trees.iter().any(|proof| {
            proof.candidate == root.head
                && proof.target == *default
                && !proof.identical
                && !proof.target_reachable_from_candidate
        }) || status.analysis.compatibility.iter().any(|report| {
            report.candidate == root.head
                && report.target == *default
                && report.outcome == crate::model::CompatibilityOutcome::Conflict
        }))
}

/// bd-c299b8: stale-base refresh uses the same sealed whole-chain replay,
/// atomic head leases, conflict handoff and fresh-CI boundary as drift repair.
/// A healthy provider representation must not make a stale root invisible.
pub(crate) fn stale_base_repair_plans<'a>(
    context: &AppContext,
    status: &'a StatusOutput,
) -> Vec<(&'a NativeStackStatus, NativeStackRebasePlan)> {
    if !queue_repair_authorized(context, status) {
        return Vec::new();
    }
    let held = crate::stack_policy::held_caravan_members(status);
    let mut candidates = status
        .stack_backend
        .native_stacks
        .iter()
        .filter(|native| repairable_caravan(status, native, &held).is_some())
        .filter(|native| stale_base_refresh_required(status, native))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|native| native.caravan_id);
    candidates
        .into_iter()
        .filter_map(|native| {
            plan_from_native(
                context,
                status,
                native,
                SYNC_DRIFT_REPAIR_ACTOR,
                SYNC_STALE_BASE_REPAIR_REASON,
                PlanScope::Ordinary,
            )
            .ok()
            .map(|plan| (native, plan))
        })
        .collect()
}

/// Whether one mapped Stack holds closed history the queue may prune itself.
///
/// Exactly the Stacks automatic merged-prefix recovery refuses with
/// `github_stack_closed_history_requires_owner` (bd-7efe6a): an open, drifted
/// Stack mapped to one repairable caravan that still holds closed rows, where
/// those rows are not one clean merged prefix ahead of a linear open suffix.
/// Every retained row must be provider-closed (merged or not), every open row
/// must be an active member of that same caravan, and every reported problem
/// must be one the rebuild resolves by construction.
fn closed_history_repairable(
    status: &StatusOutput,
    native: &NativeStackStatus,
    held: &std::collections::BTreeSet<PrNumber>,
) -> bool {
    let Some(caravan) = repairable_caravan(status, native, held) else {
        return false;
    };
    native.stack.open
        && native.consistency == StackConsistency::Drifted
        && !native.problems.is_empty()
        && native
            .problems
            .iter()
            .all(|problem| CLOSED_HISTORY_REPAIRABLE_PROBLEMS.contains(&problem.code.as_str()))
        && native
            .stack
            .pull_requests
            .iter()
            .any(|row| !row.state.eq_ignore_ascii_case("open"))
        && native.stack.pull_requests.iter().all(|row| {
            if row.state.eq_ignore_ascii_case("open") {
                row.merged_at.is_none() && caravan.members.contains(&PrNumber(row.number))
            } else {
                row.state.eq_ignore_ascii_case("closed") || row.state.eq_ignore_ascii_case("merged")
            }
        })
        && crate::stack_recovery::closed_history_requires_rebuild(native)
}

/// Sealed queue-owned rebuild plans for every Stack whose closed history
/// automatic recovery cannot use, in canonical caravan order (bd-7efe6a).
/// Pure: no provider, Git or filesystem access.
pub(crate) fn closed_history_repair_plans<'a>(
    context: &AppContext,
    status: &'a StatusOutput,
) -> Vec<(&'a NativeStackStatus, NativeStackRebasePlan)> {
    if !queue_repair_authorized(context, status) {
        return Vec::new();
    }
    let held = crate::stack_policy::held_caravan_members(status);
    let mut candidates = status
        .stack_backend
        .native_stacks
        .iter()
        .filter(|native| closed_history_repairable(status, native, &held))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|native| native.caravan_id);
    candidates
        .into_iter()
        .filter_map(|native| {
            plan_from_native(
                context,
                status,
                native,
                SYNC_DRIFT_REPAIR_ACTOR,
                SYNC_CLOSED_HISTORY_REPAIR_REASON,
                PlanScope::ClosedHistory,
            )
            .ok()
            .map(|plan| (native, plan))
        })
        .collect()
}

/// The sealed plans one sync tick would apply for `kind`, in order, with every
/// already-applied plan hash removed. Sync and its dry-run share this exact
/// selection, so a preview names the same plan hash the tick applies.
pub(crate) fn pending_sync_repair_plans<'a>(
    context: &AppContext,
    status: &'a StatusOutput,
    kind: SyncRepairKind,
) -> Result<Vec<(&'a NativeStackStatus, NativeStackRebasePlan)>, AppError> {
    let plans = match kind {
        SyncRepairKind::RoutineDrift => drift_repair_plans(context, status),
        SyncRepairKind::StaleBase => stale_base_repair_plans(context, status),
        SyncRepairKind::ClosedHistory => closed_history_repair_plans(context, status),
    };
    let mut pending = Vec::with_capacity(plans.len());
    for (native, plan) in plans {
        let key = receipt_key(&plan.plan_hash)?;
        if crate::stack_checkpoint::load::<NativeStackRebaseOutput>(&context.repository_path, &key)?
            .is_some_and(|done| done.plan.plan_hash == plan.plan_hash)
        {
            // This exact plan already completed; never rewrite the same
            // generation twice. Fresh provider truth must change first.
            continue;
        }
        pending.push((native, plan));
    }
    Ok(pending)
}

/// Repair one caravan-local drifted or stale-base native Stack as ordinary
/// queue behaviour (bd-7ff76d, bd-7efe6a, bd-c299b8).
///
/// On 2026-10-20 Cacophony Stack #4567 (child PR4566 no longer containing
/// root PR4565) could only be repaired by an operator-approved
/// `rebase-apply`, so the queue stalled until both PRs were merged by hand.
/// On 2026-10-21 Stack #4597 kept four merged PRs and a closed stale flake
/// bump ahead of its only open member, so every tick refused with
/// `github_stack_closed_history_requires_owner` while that member, which also
/// conflicted with main, held the only `max_caravans` slot.
///
/// Under the repository's reviewed Stack mutation opt-in, sync plans the same
/// sealed rebase from the same exact provider facts and applies it under its
/// writer guard: each member's final three-way tree is proven without replaying
/// its commit sequence, and an exact predecessor already in head ancestry needs
/// no rewrite. A new merge retains both the source head and predecessor; every
/// publication remains force-with-lease guarded against its observed generation.
/// Closed rows are pruned from the rebuilt Stack (the raw
/// generation stays in the sealed plan and checkpointed receipt), rewritten
/// heads need fresh CI, and an applied plan hash is never replayed.
///
/// A member that cannot be replayed linearly (a content conflict or an
/// unreplayable history shape) is evicted together with its descendants,
/// tail-first, with a typed reason and a machine-actionable repair request;
/// the members before it keep their rebase and the slot is freed. Only an
/// open mapped Stack of an unpaused, unparked caravan is eligible, never one
/// that is unknown or truncated. Membership-only drift belongs to automatic
/// recovery unless the provider rows are a proven strict active-fleet prefix
/// after a join; that suffix needs replay too (bd-28ae57). Any other zero-write
/// refusal leaves the caravan for its
/// owner; any later failure is returned for reconciliation.
pub(crate) fn repair_drift_for_sync(
    context: &AppContext,
    status: &StatusOutput,
    kind: SyncRepairKind,
    writer: &crate::writer_guard::WriterOperationGuard,
    deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<Option<SyncDriftRepair>, AppError> {
    for (native, plan) in pending_sync_repair_plans(context, status, kind)? {
        let key = receipt_key(&plan.plan_hash)?;
        match apply_plan(
            context,
            status,
            plan,
            native.stack.number,
            &key,
            ConflictPolicy::EvictSuffix,
            writer,
            deadline,
            github_budget,
        ) {
            Ok((output, status)) => return Ok(Some(SyncDriftRepair { output, status })),
            // Nothing was written: leave this caravan for its owner and try
            // the next eligible Stack.
            Err(error)
                if error
                    .details()
                    .and_then(|details| details.get("publication_started").cloned())
                    == Some(json!(false)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

/// What a rebase does with a member it cannot replay linearly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConflictPolicy {
    /// Explicit owner apply: refuse before any write.
    Refuse,
    /// Queue-owned repair: evict that member and its descendants (bd-7efe6a).
    EvictSuffix,
}

/// Preparation refusals that are deterministic facts about one exact member
/// generation rather than about the environment: its patch conflicts with the
/// target, or its history cannot be replayed linearly. Retrying the same
/// generation reproduces them; fetch, timeout, lease and object failures are
/// deliberately absent and never evict anybody.
const EVICTABLE_PREPARATION_REFUSALS: [&str; 8] = [
    "rebase_conflict",
    "rebase_merge_replay_conflict",
    "rebase_merge_tree_conflict",
    "rebase_resolution_conflict",
    "rebase_topology_changed",
    "rebase_cousin_history",
    "rebase_external_merge_parents",
    "rebase_unsupported_octopus",
];

fn evictable_preparation_refusal(error: &AppError) -> bool {
    EVICTABLE_PREPARATION_REFUSALS.contains(&error.code().as_str())
}

/// Machine-actionable repair request for one member the queue evicted because
/// it could not be replayed linearly (bd-7efe6a). Secret-free; it grants no
/// mutation authority and is delivered through the existing `sync_failed` hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NativeStackRepairRequest {
    pub schema_version: u32,
    pub kind: String,
    pub repository: RepositoryId,
    pub stack: u64,
    pub plan_hash: String,
    pub caravan_id: PrNumber,
    pub pr: PrNumber,
    /// Exact source generation the queue observed and preserved.
    pub head: BranchSnapshot,
    /// Exact provider base the PR targeted when it was evicted.
    pub base: BranchSnapshot,
    /// Exact generation it could not be replayed onto.
    pub target: BranchSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_pr: Option<PrNumber>,
    pub refusal_code: String,
    pub refusal: String,
    #[serde(default)]
    pub conflicting_paths: Vec<String>,
    /// Every member released, in caravan order: this one, then descendants.
    pub evicted: Vec<PrNumber>,
    pub suggested_commands: Vec<String>,
}

/// Members a queue-owned repair evicted, and the typed request to repair them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NativeStackConflictEviction {
    pub evicted: Vec<PrNumber>,
    pub reason: String,
    pub repair_request: NativeStackRepairRequest,
    pub receipt: crate::model::OperationReceipt,
    #[serde(default)]
    pub provider_receipts: Vec<crate::github::GitHubMutationReceipt>,
}

/// Build the typed repair request for the first member that could not be
/// replayed, naming exactly which suffix the queue releases. Pure.
fn conflict_repair_request(
    plan: &NativeStackRebasePlan,
    index: usize,
    target: &BranchSnapshot,
    error: &AppError,
) -> NativeStackRepairRequest {
    let member = &plan.members[index];
    let details = error.details().unwrap_or_else(|| json!({}));
    let conflicting_paths = details
        .get("conflicting_paths")
        .and_then(serde_json::Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let evicted = plan.members[index..]
        .iter()
        .map(|member| member.pr)
        .collect::<Vec<_>>();
    let target_flag = member
        .parent_pr
        .map_or_else(String::new, |parent| format!(" --target-pr {parent}"));
    NativeStackRepairRequest {
        schema_version: 1,
        kind: "native_stack_member_unreplayable".to_owned(),
        repository: plan.repository.clone(),
        stack: plan.stack,
        plan_hash: plan.plan_hash.clone(),
        caravan_id: plan.root_pr,
        pr: member.pr,
        head: BranchSnapshot {
            repository: plan.repository.clone(),
            name: member.branch.clone(),
            oid: member.old_head.clone(),
        },
        base: member.old_base.clone(),
        target: target.clone(),
        target_pr: member.parent_pr,
        refusal_code: error.code(),
        refusal: error.message(),
        conflicting_paths,
        suggested_commands: vec![
            format!(
                "cara repair start --pr {}{target_flag} --non-force --actor <actor> --reason \"resolve {} against {}@{}\"",
                member.pr,
                error.code(),
                target.name,
                target.oid,
            ),
            format!("cara rejoin --pr {}", member.pr),
        ],
        evicted,
    }
}

/// Typed, bounded eviction reason recorded on every released member.
fn conflict_eviction_reason(request: &NativeStackRepairRequest) -> String {
    let paths = if request.conflicting_paths.is_empty() {
        String::new()
    } else {
        let shown = request
            .conflicting_paths
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let more = request.conflicting_paths.len().saturating_sub(5);
        if more == 0 {
            format!(" in {shown}")
        } else {
            format!(" in {shown} (+{more} more)")
        }
    };
    let descendants = if request.evicted.len() > 1 {
        format!(
            "; descendant(s) {} carry its patch and were released with it",
            request.evicted[1..]
                .iter()
                .map(|pr| format!("#{pr}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        String::new()
    };
    format!(
        "{}: queue-owned native Stack #{} repair could not replay PR #{} {} onto {}@{}{paths}{descendants}; repair with `{}`, then `{}` (bd-7efe6a)",
        request.refusal_code,
        request.stack,
        request.pr,
        request.head.oid,
        request.target.name,
        request.target.oid,
        request.suggested_commands[0],
        request.suggested_commands[1],
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn apply_plan(
    context: &AppContext,
    status: &StatusOutput,
    plan: NativeStackRebasePlan,
    _stack: u64,
    key: &str,
    conflicts: ConflictPolicy,
    writer: &crate::writer_guard::WriterOperationGuard,
    deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<(NativeStackRebaseOutput, StatusOutput), AppError> {
    let timeout = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let mut prepared = Vec::with_capacity(plan.members.len());
    let mut target = plan.members.first().map(|member| {
        crate::physical_rebase::PlannedBase::Remote(BranchSnapshot {
            repository: plan.repository.clone(),
            name: member.parent_branch.clone(),
            oid: member.parent_head.clone(),
        })
    });
    let mut unreplayable = None;
    for (index, member) in plan.members.iter().enumerate() {
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
        let planned_target = target.clone().expect("non-empty rebase plan has a target");
        let item = match crate::physical_rebase::prepare_candidate(
            &context.repository_path,
            &plan.repository,
            candidate,
            member_range_source(&plan, member, candidate, &parent),
            planned_target.clone(),
            &status.analysis.fleet.default_branch,
            crate::physical_rebase::RebaseExecutionBudget::new(timeout)
                .with_cumulative_tree_replay(conflicts == ConflictPolicy::EvictSuffix)
                .with_deadline(deadline)
                .with_writer_fence(writer.remote_fence())
                .because(member.parent_pr.map_or(
                    crate::physical_rebase::BranchRewriteReason::CurrentDefaultAdvanced,
                    |parent_pr| crate::physical_rebase::BranchRewriteReason::ParentAdvanced {
                        parent_pr,
                    },
                )),
        ) {
            Ok(item) => item,
            // bd-7efe6a: a member the queue cannot replay linearly is released
            // with its descendants; the members before it keep their rebase.
            Err(error)
                if conflicts == ConflictPolicy::EvictSuffix
                    && evictable_preparation_refusal(&error) =>
            {
                let (crate::physical_rebase::PlannedBase::Remote(branch)
                | crate::physical_rebase::PlannedBase::Simulated(branch)) = &planned_target;
                unreplayable = Some(conflict_repair_request(&plan, index, branch, &error));
                break;
            }
            Err(error) => {
                return Err(before_publication(owner_continuation_refusal(
                    error, &plan, member,
                )));
            }
        };
        target = Some(crate::physical_rebase::PlannedBase::Simulated(
            BranchSnapshot {
                repository: plan.repository.clone(),
                name: member.branch.clone(),
                oid: item.plan.new_head_oid.clone(),
            },
        ));
        prepared.push(item);
    }
    let prepared_refs = prepared.iter().collect::<Vec<_>>();
    let physical = if prepared_refs.is_empty() {
        None
    } else {
        Some(crate::physical_rebase::apply_prepared_atomically(
            &prepared_refs,
        )?)
    };
    let rewritten_status = if physical.is_none() {
        status.clone()
    } else {
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
        })?
    };
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
    let evicted = unreplayable
        .as_ref()
        .map_or_else(Vec::new, |request| request.evicted.clone());
    // Before the first provider Stack write, a refusal with no physical
    // publication is still zero-write.
    let unpublished = |error: AppError| {
        if physical.is_none() {
            before_publication(error)
        } else {
            error
        }
    };
    if evicted
        .iter()
        .any(|number| !caravan.members.contains(number))
    {
        return Err(unpublished(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "native_stack_repair_eviction_scope_changed",
            "the unreplayable suffix is no longer part of the rewritten active caravan",
            Some(json!({
                "plan": plan,
                "physical": physical,
                "repair_request": unreplayable,
                "caravan": caravan,
                "mutated": physical.is_some(),
                "resumable": true,
            })),
        )));
    }
    let survivors = caravan
        .members
        .iter()
        .copied()
        .filter(|number| !evicted.contains(number))
        .collect::<Vec<_>>();
    // Validate the linear replacement shape before the first provider Stack
    // write: each survivor as it will be once retargeted onto its predecessor.
    let projected = project_linear_bases(&rewritten_status, &survivors).map_err(&unpublished)?;
    if !projected.is_empty() {
        crate::stack_membership::topology_from_members(
            &rewritten_status.analysis.fleet.default_branch,
            projected.iter(),
        )
        .map_err(|error| {
            unpublished(AppError::structured(
                ErrorCategory::ExecutionFailure,
                "native_stack_rebase_replacement_topology_invalid",
                error.to_string(),
                Some(json!({"plan": plan, "physical": physical, "mutated": physical.is_some()})),
            ))
        })?;
    }
    let provider_runner = ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(timeout)
        .with_operation_deadline(deadline);
    let provider = GitHubMutationAdapter::new(writer.runner(provider_runner));
    let clear_operation = format!("native-stack-rebase-clear-{}", plan.plan_hash);
    let drift_clear = clear_drifted_with_bounded_rediscovery(
        &provider,
        &plan.repository,
        &clear_operation,
        &plan.actor,
        &plan.provider_before,
        deadline,
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
            "recovery_class": "provider_stack_generation_race",
            "rediscovery_attempts": DRIFT_CLEAR_REDISCOVERY_ATTEMPTS,
            "safe_next_action": "rerun rebase-preview against fresh provider truth; never unstack or force-push manually",
        })),
    ))?;
    // The drifted representation is gone, so releasing the unreplayable
    // suffix re-links no surviving edge: it is a tail-first label transition
    // that preserves every evicted source head byte-for-byte.
    let eviction = match unreplayable {
        Some(repair_request) => {
            let reason = conflict_eviction_reason(&repair_request);
            let released = crate::reshape::queue_evict_suffix(
                &provider,
                &rewritten_status,
                &repair_request.evicted,
                &plan.actor,
                &reason,
                &format!(
                    "{} preparing PR #{} onto {}@{}; no source head was rewritten for the released suffix",
                    repair_request.refusal_code,
                    repair_request.pr,
                    repair_request.target.name,
                    repair_request.target.oid,
                ),
            )
            .map_err(|error| AppError::structured(
                ErrorCategory::ExecutionFailure,
                "native_stack_repair_eviction_failed",
                "the drifted Stack was cleared but releasing its unreplayable suffix failed",
                Some(json!({
                    "plan": plan,
                    "physical": physical,
                    "drift_clear": drift_clear,
                    "repair_request": repair_request,
                    "source": error.details(),
                    "source_code": error.code(),
                    "mutated": true,
                    "resumable": true,
                    "safe_next_action": "rerun cara sync: fresh provider truth re-plans the remaining eviction and rebuild; never force-push or unstack manually",
                })),
            ))?;
            Some(NativeStackConflictEviction {
                evicted: repair_request.evicted.clone(),
                reason,
                repair_request,
                receipt: released.receipt,
                provider_receipts: released.provider_receipts,
            })
        }
        None => None,
    };
    // A survivor whose predecessor closed without merging still targets that
    // closed branch; point every survivor at its linear predecessor before the
    // exact rebuild (the root at the exact default branch).
    let retargets = retarget_survivors(
        &provider,
        &plan.repository,
        &rewritten_status.analysis.fleet.default_branch.name,
        &survivors,
    )
    .map_err(|error| AppError::structured(
        ErrorCategory::ExecutionFailure,
        "native_stack_rebase_retarget_failed",
        format!("drifted Stack was cleared but linear survivor retargeting failed: {error}"),
        Some(json!({
            "plan": plan,
            "physical": physical,
            "drift_clear": drift_clear,
            "eviction": eviction,
            "mutated": true,
            "resumable": true,
            "safe_next_action": "rerun cara sync: fresh provider truth re-plans the remaining retarget and rebuild; never force-push or unstack manually",
        })),
    ))?;
    let replacement = if survivors.len() >= 2 {
        let rebuild_status = if retargets.is_empty() {
            rewritten_status
        } else {
            read_postcondition(context, deadline, github_budget)?
        };
        let pulls = survivors
            .iter()
            .map(|number| {
                rebuild_status
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
            &rebuild_status.analysis.fleet.default_branch,
            pulls.iter().copied(),
        )
        .map_err(|error| {
            AppError::structured(
                ErrorCategory::ExecutionFailure,
                "native_stack_rebase_replacement_topology_invalid",
                error.to_string(),
                Some(json!({"plan": plan, "physical": physical, "retargets": retargets, "mutated": true})),
            )
        })?;
        Some(provider
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
                    "eviction": eviction,
                    "retargets": retargets,
                    "source": error.to_string(),
                    "mutated": true,
                    "resumable": true,
                    "safe_next_action": format!("run native-stack recovery-preview --root {} to resume exact replacement creation", plan.root_pr),
                })),
            ))?)
    } else {
        None
    };
    let final_status = read_postcondition(context, deadline, github_budget)?;
    let final_native = final_status
        .stack_backend
        .native_stacks
        .iter()
        .find(|native| native.caravan_id == Some(plan.root_pr));
    if survivors.len() >= 2 {
        let final_native = final_native.ok_or_else(|| AppError::structured(
            ErrorCategory::ExecutionFailure,
            "native_stack_rebase_postcondition_stack_missing",
            "replacement Stack creation returned but exact active-fleet mapping is absent",
            Some(json!({"plan": plan, "physical": physical, "replacement": replacement, "eviction": eviction, "mutated": true})),
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
                    "eviction": eviction,
                    "native": final_native,
                    "mutated": true,
                })),
            ));
        }
    } else if final_native.is_some() {
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "native_stack_rebase_singleton_still_stacked",
            "terminal drift clearance left the singleton root in a provider Stack",
            Some(
                json!({"plan": plan, "physical": physical, "eviction": eviction, "mutated": true}),
            ),
        ));
    }
    let fresh_ci_required = physical.as_ref().is_some_and(|physical| {
        physical
            .receipts
            .iter()
            .any(|receipt| !receipt.already_satisfied)
    });
    let next = match (&eviction, fresh_ci_required) {
        (Some(_), true) => {
            "unreplayable members were evicted with a repair request; wait for fresh CI only on rewritten heads, then rerun cara sync"
        }
        (Some(_), false) => {
            "unreplayable members were evicted with a repair request and their slot is free; rerun cara sync"
        }
        (None, true) => "wait for fresh CI only on rewritten heads, then rerun cara sync",
        (None, false) => {
            "provider representation repaired without a source rewrite; rerun cara sync"
        }
    };
    let output = NativeStackRebaseOutput {
        plan,
        mutated: true,
        physical,
        drift_clear: Some(drift_clear),
        replacement,
        eviction,
        retargets,
        fresh_ci_required,
        next: next.to_owned(),
    };
    crate::stack_checkpoint::write(&context.repository_path, key, &output)?;
    Ok((output, final_status))
}

/// Exact lower range boundary for one planned member.
///
/// A root still based on a retained closed row's branch (a predecessor merged
/// or closed out of band) binds that row's immutable `refs/pull/N/head`, the
/// same boundary ordinary sync uses after a merged predecessor, rather than a
/// branch its owner may delete at any moment (bd-7efe6a).
fn member_range_source(
    plan: &NativeStackRebasePlan,
    member: &NativeStackRebaseMemberPlan,
    candidate: &crate::model::PullRequestSnapshot,
    parent: &BranchSnapshot,
) -> crate::physical_rebase::PlannedRangeBase {
    if member.parent_pr.is_none()
        && candidate.base.name != parent.name
        && let Some(row) = plan.provider_before.pull_requests.iter().find(|row| {
            !row.state.eq_ignore_ascii_case("open")
                && row.head.ref_name == candidate.base.name
                && row.head.sha == candidate.base.oid
        })
    {
        return crate::physical_rebase::PlannedRangeBase::PullRequestHead {
            pr: PrNumber(row.number),
            branch: candidate.base.clone(),
        };
    }
    crate::physical_rebase::range_base_for_rewritten_parent(candidate, parent)
}

/// Survivors as they will be once each targets its linear predecessor (the
/// root the exact default branch). Pure projection used to validate the
/// replacement shape before any provider Stack write.
pub(crate) fn project_linear_bases(
    status: &StatusOutput,
    survivors: &[PrNumber],
) -> Result<Vec<crate::model::PullRequestSnapshot>, AppError> {
    let mut projected: Vec<crate::model::PullRequestSnapshot> = Vec::with_capacity(survivors.len());
    for number in survivors {
        let mut pull = status
            .analysis
            .pull_requests
            .get(number)
            .cloned()
            .ok_or_else(|| {
                AppError::validation(
                    "native_stack_rebase_postcondition_member_missing",
                    "rewritten active member is absent before provider Stack replacement",
                )
            })?;
        pull.base = projected.last().map_or_else(
            || status.analysis.fleet.default_branch.clone(),
            |parent| parent.head.clone(),
        );
        projected.push(pull);
    }
    Ok(projected)
}

/// Point each survivor's PR base at its linear predecessor's branch (the root
/// at the default branch) from fresh provider facts. Already-linear bases are
/// left untouched, so a replay writes nothing.
fn retarget_survivors<R: crate::command::CommandRunner>(
    provider: &GitHubMutationAdapter<R>,
    repository: &RepositoryId,
    default_branch: &str,
    survivors: &[PrNumber],
) -> Result<Vec<crate::github::GitHubMutationReceipt>, crate::github::MutationError> {
    let mut receipts = Vec::new();
    let mut expected = default_branch.to_owned();
    for number in survivors {
        let current = provider.refetch_pull_request(repository, *number)?;
        if current.base.name != expected {
            receipts.push(provider.set_base(
                repository,
                &crate::model::PullRequestPrecondition::from(&current),
                &expected,
            )?);
        }
        expected = current.head.name;
    }
    Ok(receipts)
}

fn proven_active_fleet_prefix(status: &StatusOutput, native: &NativeStackStatus) -> bool {
    let Some(caravan_id) = native.caravan_id else {
        return false;
    };
    let Some(caravan) = status
        .analysis
        .fleet
        .caravans
        .iter()
        .find(|caravan| caravan.id == caravan_id)
    else {
        return false;
    };
    let open = native
        .stack
        .pull_requests
        .iter()
        .filter(|entry| entry.state.eq_ignore_ascii_case("open"))
        .collect::<Vec<_>>();
    if open.is_empty() || open.len() >= caravan.members.len() {
        return false;
    }
    if !open.iter().map(|entry| PrNumber(entry.number)).eq(caravan
        .members
        .iter()
        .copied()
        .take(open.len()))
    {
        return false;
    }
    let pulls = caravan
        .members
        .iter()
        .map(|number| status.analysis.pull_requests.get(number))
        .collect::<Option<Vec<_>>>();
    let Some(pulls) = pulls else {
        return false;
    };
    if open.iter().zip(&pulls).any(|(entry, pull)| {
        pull.head.name != entry.head.ref_name || pull.head.oid != entry.head.sha
    }) {
        return false;
    }
    pulls.iter().enumerate().all(|(position, pull)| {
        pull.state == crate::model::PullRequestState::Open
            && !pull.draft
            && pull.has_label("caravan")
            && if position == 0 {
                pull.base.repository == status.analysis.fleet.default_branch.repository
                    && pull.base.name == status.analysis.fleet.default_branch.name
            } else {
                pull.base.repository == pulls[position - 1].head.repository
                    && pull.base.name == pulls[position - 1].head.name
            }
    })
}

fn proven_retained_closed_rows(native: &NativeStackStatus) -> bool {
    let open_heads = native
        .stack
        .pull_requests
        .iter()
        .filter(|entry| entry.state.eq_ignore_ascii_case("open"))
        .map(|entry| &entry.head.sha)
        .collect::<Vec<_>>();
    let retained = native
        .stack
        .pull_requests
        .iter()
        .filter(|entry| !entry.state.eq_ignore_ascii_case("open"))
        .collect::<Vec<_>>();
    !retained.is_empty()
        && retained.iter().all(|entry| {
            entry.state.eq_ignore_ascii_case("closed")
                && !entry.draft
                && (entry.merged_at.is_some()
                    || open_heads.iter().any(|head| **head == entry.head.sha))
        })
}

fn repairable_problem(
    problem: &crate::read::StackBackendProblem,
    retained_closed_rows: bool,
    active_prefix: bool,
) -> bool {
    matches!(
        problem.code.as_str(),
        "native_stack_rebase_required" | "github_stack_pr_base_drift" | "github_stack_head_drift"
    ) || (retained_closed_rows
        && matches!(
            problem.code.as_str(),
            "github_stack_pr_missing" | "github_stack_merged_prefix_invalid"
        ))
        || (active_prefix && problem.code == "github_stack_member_order_drift")
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
    // Apply is fenced by an explicit Stack plus sealed plan hash, so local
    // current-branch resolution is unrelated state rather than authority.
    let status = read::fleet_status(context, deadline, None)?;
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
        ConflictPolicy::Refuse,
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

    #[test]
    fn drift_clear_rediscovery_accepts_only_same_stack_and_ordered_membership() {
        let mut before = divergent_stack(42).stack;
        before.pull_requests = vec![
            crate::github::GitHubStackPullRequest {
                number: 10,
                state: "open".to_owned(),
                draft: false,
                merged_at: None,
                head: crate::github::GitHubStackPullRequestHead {
                    ref_name: "root".to_owned(),
                    sha: CommitOid("old-root".to_owned()),
                },
            },
            crate::github::GitHubStackPullRequest {
                number: 11,
                state: "open".to_owned(),
                draft: false,
                merged_at: None,
                head: crate::github::GitHubStackPullRequestHead {
                    ref_name: "tail".to_owned(),
                    sha: CommitOid("old-tail".to_owned()),
                },
            },
        ];
        let mut refreshed = before.clone();
        refreshed.pull_requests[1].head.sha = CommitOid("new-tail".to_owned());
        assert!(same_drift_clear_scope(&before, &refreshed));

        let mut reordered = refreshed.clone();
        reordered.pull_requests.swap(0, 1);
        assert!(!same_drift_clear_scope(&before, &reordered));

        let mut replaced = refreshed;
        replaced.node_id = "replacement".to_owned();
        assert!(!same_drift_clear_scope(&before, &replaced));
    }

    #[test]
    fn drift_clear_retry_classifier_is_narrow() {
        let race = crate::github::GitHubStackMutationError::InconsistentProviderState {
            diagnostic: "drift-clear Stack #42 raw generation changed before mutation".to_owned(),
        };
        assert!(drift_clear_generation_changed(&race));
        let other = crate::github::GitHubStackMutationError::InconsistentProviderState {
            diagnostic: "drift-clear Stack #42 remained after provider response 500".to_owned(),
        };
        assert!(!drift_clear_generation_changed(&other));
    }

    #[test]
    fn retained_closed_row_requires_terminal_or_same_head_proof() {
        let mut native = divergent_stack(42);
        native.stack.pull_requests = vec![
            crate::github::GitHubStackPullRequest {
                number: 173,
                state: "open".to_owned(),
                draft: false,
                merged_at: None,
                head: crate::github::GitHubStackPullRequestHead {
                    ref_name: "release".to_owned(),
                    sha: CommitOid("same-head".to_owned()),
                },
            },
            crate::github::GitHubStackPullRequest {
                number: 174,
                state: "closed".to_owned(),
                draft: false,
                merged_at: None,
                head: crate::github::GitHubStackPullRequestHead {
                    ref_name: "duplicate".to_owned(),
                    sha: CommitOid("same-head".to_owned()),
                },
            },
        ];
        assert!(proven_retained_closed_rows(&native));
        native.stack.pull_requests[1].head.sha = CommitOid("unrelated".to_owned());
        assert!(!proven_retained_closed_rows(&native));
        native.stack.pull_requests[1].merged_at = Some("2026-08-19T00:00:00Z".to_owned());
        assert!(proven_retained_closed_rows(&native));
        native.stack.pull_requests[1].draft = true;
        assert!(!proven_retained_closed_rows(&native));
    }

    /// bd-7ff76d replaced "ordinary sync never rewrites" with one audited,
    /// gated entry. Sync reaches the publisher only through
    /// `repair_drift_for_sync`, which plans with the same sealed planner as the
    /// explicit command and is gated by the reviewed Stack opt-in and the
    /// repository's `manual_drift_repair` choice.
    #[test]
    fn sync_reaches_the_rebase_publisher_only_through_gated_drift_repair() {
        let sync = include_str!("sync.rs")
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        // One gated publisher, selected for closed-history, routine drift or
        // an otherwise healthy Stack with a stale base.
        assert_eq!(
            sync.matches("crate::native_stack_rebase::repair_drift_for_sync(")
                .count(),
            3
        );
        assert!(sync.contains("crate::native_stack_rebase::SyncRepairKind::StaleBase"));
        assert!(sync.contains("crate::native_stack_rebase::SyncRepairKind::ClosedHistory"));
        assert!(sync.contains("crate::native_stack_rebase::SyncRepairKind::RoutineDrift"));
        assert!(sync.contains("crate::native_stack_rebase::SyncDriftRepair"));
        assert!(sync.contains("require_native_stack_backend_healthy(&status)?"));
        let explicit = include_str!("native_stack_rebase.rs")
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        assert!(!explicit.contains("auto_apply_from_status"));
        // One private definition; one call from the explicit apply command,
        // which refuses unreplayable members, and one from the gated sync
        // repair, which evicts them.
        assert_eq!(explicit.matches("fn apply_plan(").count(), 1);
        assert_eq!(explicit.matches("        match apply_plan(\n").count(), 1);
        assert_eq!(explicit.matches("    apply_plan(\n").count(), 1);
        assert_eq!(explicit.matches("ConflictPolicy::Refuse,").count(), 1);
        assert_eq!(explicit.matches("ConflictPolicy::EvictSuffix,").count(), 1);
        assert!(
            explicit
                .contains(".with_cumulative_tree_replay(conflicts == ConflictPolicy::EvictSuffix)")
        );
        assert!(
            explicit.contains("context.acquire_writer_operation(\"native-stack-rebase-apply\")")
        );
        assert!(explicit.contains("plan.plan_hash != input.expected_plan_hash || !plan.verify()"));
        assert!(explicit.contains("|| !context.config.stack_rollout.mutations_opt_in"));
        assert!(explicit.contains("&& !context.config.stack_rollout.manual_drift_repair"));
        // All three repair selections share that single authorization gate.
        assert_eq!(
            explicit
                .matches("if !queue_repair_authorized(context, status) {")
                .count(),
            3
        );
    }

    #[test]
    fn preparation_refusals_are_marked_before_publication() {
        let marked = before_publication(AppError::structured(
            ErrorCategory::Validation,
            "rebase_merge_tree_conflict",
            "conflict",
            Some(json!({"conflicting_paths": ["src/a.rs"]})),
        ));
        let details = marked.details().unwrap();
        assert_eq!(details["publication_started"], false);
        assert_eq!(details["mutated"], false);
        assert_eq!(details["conflicting_paths"][0], "src/a.rs");
        // An explicit mutation verdict is never overwritten.
        let mutated = before_publication(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "x",
            "y",
            Some(json!({"mutated": true})),
        ));
        assert_eq!(mutated.details().unwrap()["mutated"], true);
    }

    fn sample_plan() -> NativeStackRebasePlan {
        NativeStackRebasePlan {
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
        .seal()
    }

    fn three_member_plan() -> NativeStackRebasePlan {
        let member = |position: u32, pr: u64, parent: Option<u64>| NativeStackRebaseMemberPlan {
            position,
            pr: PrNumber(pr),
            branch: format!("pr-{pr}"),
            old_head: CommitOid(format!("{pr:0>40}")),
            old_base: branch(
                &parent.map_or_else(|| "main".to_owned(), |parent| format!("pr-{parent}")),
                "1111111111111111111111111111111111111111",
            ),
            parent_pr: parent.map(PrNumber),
            parent_branch: parent
                .map_or_else(|| "main".to_owned(), |parent| format!("pr-{parent}")),
            parent_head: CommitOid(
                parent.map_or_else(|| "d".repeat(40), |parent| format!("{parent:0>40}")),
            ),
        };
        NativeStackRebasePlan {
            schema_version: 1,
            repository: repository(),
            stack: 4597,
            root_pr: PrNumber(4600),
            stack_base_ref: "main".to_owned(),
            provider_before: divergent_stack(4597).stack,
            members: vec![
                member(0, 4600, None),
                member(1, 4603, Some(4600)),
                member(2, 4604, Some(4603)),
            ],
            actor: SYNC_DRIFT_REPAIR_ACTOR.to_owned(),
            reason: SYNC_CLOSED_HISTORY_REPAIR_REASON.to_owned(),
            config_fingerprint: "fnv1a64:config".to_owned(),
            plan_hash: String::new(),
        }
        .seal()
    }

    /// bd-7efe6a: only deterministic facts about one exact member generation
    /// may evict it; the environment failing never evicts anybody.
    #[test]
    fn only_deterministic_replay_refusals_are_evictable() {
        for code in EVICTABLE_PREPARATION_REFUSALS {
            assert!(evictable_preparation_refusal(&AppError::validation(
                code, "x"
            )));
        }
        for code in [
            "rebase_push_preflight_failed",
            "rebase_metadata_unavailable",
            "rebase_simulated_parent_missing",
            "rebase_repository_not_owned",
            "rebase_range_ambiguous",
            "git_fetch_failed",
            "command_timeout",
            "operation_deadline_exceeded",
            "remote_lease_lost",
        ] {
            assert!(
                !evictable_preparation_refusal(&AppError::validation(code, "x")),
                "{code}"
            );
        }
    }

    /// bd-7efe6a: a conflicting member and its descendants are released with
    /// one exact, machine-actionable repair request naming its real conflict.
    #[test]
    fn conflicting_member_repair_request_releases_exactly_its_suffix() {
        let plan = three_member_plan();
        let target = branch("pr-4600", &"e".repeat(40));
        let error = AppError::structured(
            ErrorCategory::Validation,
            "rebase_conflict",
            "replaying PR #4603 conflicts",
            Some(json!({"conflicting_paths": ["flake.lock", "src/a.rs", "src/b.rs"]})),
        );
        let request = conflict_repair_request(&plan, 1, &target, &error);
        assert_eq!(request.schema_version, 1);
        assert_eq!(request.kind, "native_stack_member_unreplayable");
        assert_eq!(request.pr, PrNumber(4603));
        assert_eq!(request.caravan_id, PrNumber(4600));
        assert_eq!(request.stack, 4597);
        assert_eq!(request.plan_hash, plan.plan_hash);
        assert_eq!(request.head.name, "pr-4603");
        assert_eq!(request.head.oid, plan.members[1].old_head);
        assert_eq!(request.base, plan.members[1].old_base);
        assert_eq!(request.target, target);
        assert_eq!(request.target_pr, Some(PrNumber(4600)));
        assert_eq!(request.refusal_code, "rebase_conflict");
        assert_eq!(
            request.conflicting_paths,
            vec!["flake.lock", "src/a.rs", "src/b.rs"]
        );
        assert_eq!(request.evicted, vec![PrNumber(4603), PrNumber(4604)]);
        assert!(request.suggested_commands[0].starts_with(
            "cara repair start --pr 4603 --target-pr 4600 --non-force --actor <actor> --reason"
        ));
        assert_eq!(request.suggested_commands[1], "cara rejoin --pr 4603");

        let reason = conflict_eviction_reason(&request);
        assert!(reason.starts_with("rebase_conflict: queue-owned native Stack #4597"));
        assert!(reason.contains("in flake.lock, src/a.rs, src/b.rs"));
        assert!(reason.contains("descendant(s) #4604 carry its patch"));
        assert!(reason.contains("cara rejoin --pr 4603"));

        // A root that conflicts with the default branch releases the whole
        // caravan and suggests merging the current default branch.
        let root = conflict_repair_request(&plan, 0, &branch("main", &"d".repeat(40)), &error);
        assert_eq!(
            root.evicted,
            vec![PrNumber(4600), PrNumber(4603), PrNumber(4604)]
        );
        assert_eq!(root.target_pr, None);
        assert!(
            root.suggested_commands[0]
                .starts_with("cara repair start --pr 4600 --non-force --actor <actor>")
        );
        let tail = conflict_repair_request(&plan, 2, &target, &error);
        assert_eq!(tail.evicted, vec![PrNumber(4604)]);
        assert!(!conflict_eviction_reason(&tail).contains("descendant"));
    }

    fn snapshot(
        number: u64,
        head: BranchSnapshot,
        base: BranchSnapshot,
    ) -> crate::model::PullRequestSnapshot {
        crate::model::PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(number),
            title: format!("PR {number}"),
            url: format!("https://example.invalid/{number}"),
            state: crate::model::PullRequestState::Open,
            draft: false,
            head,
            base,
            cross_repository: false,
            labels: std::collections::BTreeSet::from(["caravan".to_owned()]),
            auto_merge: crate::model::AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        }
    }

    /// A root still based on a closed row's branch binds that row's immutable
    /// pull-request head, never a branch its owner may delete.
    #[test]
    fn root_based_on_a_closed_row_binds_its_immutable_pull_request_head() {
        let mut plan = three_member_plan();
        plan.provider_before.pull_requests = vec![crate::github::GitHubStackPullRequest {
            number: 4600,
            state: "closed".to_owned(),
            draft: false,
            merged_at: None,
            head: crate::github::GitHubStackPullRequestHead {
                ref_name: "update-flake".to_owned(),
                sha: CommitOid("6".repeat(40)),
            },
        }];
        let main = branch("main", &"d".repeat(40));
        let stale = branch("update-flake", &"6".repeat(40));
        let root = snapshot(4603, branch("pr-4603", &"c".repeat(40)), stale.clone());
        let mut member = plan.members[0].clone();
        member.pr = PrNumber(4603);
        assert_eq!(
            member_range_source(&plan, &member, &root, &main),
            crate::physical_rebase::PlannedRangeBase::PullRequestHead {
                pr: PrNumber(4600),
                branch: stale,
            }
        );
        // A different generation of that branch is not the closed row.
        let moved = snapshot(
            4603,
            branch("pr-4603", &"c".repeat(40)),
            branch("update-flake", &"7".repeat(40)),
        );
        assert!(matches!(
            member_range_source(&plan, &member, &moved, &main),
            crate::physical_rebase::PlannedRangeBase::RemoteBranch { .. }
        ));
        // Children keep the ordinary rewritten-parent boundary.
        let child = snapshot(
            4604,
            branch("pr-4604", &"4".repeat(40)),
            branch("pr-4603", &"c".repeat(40)),
        );
        assert!(matches!(
            member_range_source(
                &plan,
                &plan.members[1],
                &child,
                &branch("pr-4603", &"c".repeat(40))
            ),
            crate::physical_rebase::PlannedRangeBase::RemoteBranch { .. }
        ));
    }

    #[test]
    fn closed_history_eviction_output_round_trips_and_stays_optional() {
        let plan = three_member_plan();
        let legacy = json!({
            "plan": plan,
            "mutated": true,
            "fresh_ci_required": false,
            "next": "rerun cara sync",
        });
        let decoded: NativeStackRebaseOutput = serde_json::from_value(legacy.clone()).unwrap();
        assert!(decoded.eviction.is_none());
        assert_eq!(serde_json::to_value(&decoded).unwrap(), legacy);
    }

    #[test]
    fn topology_refusal_returns_non_force_owner_scope_without_replaying_history() {
        let plan = sample_plan();
        let original =
            json!({"old": "authored-two-parent", "new": "collapsed-one-parent", "resumable": true});
        let error = owner_continuation_refusal(
            AppError::structured(
                ErrorCategory::Validation,
                "rebase_topology_changed",
                "parent cardinality changed",
                Some(original.clone()),
            ),
            &plan,
            &plan.members[0],
        );
        assert_eq!(error.code(), "rebase_topology_changed");
        let details = error.details().unwrap();
        assert_eq!(details["source"], original);
        assert_eq!(details["plan_hash"], plan.plan_hash);
        assert_eq!(details["this_attempt_source_published"], false);
        assert_eq!(
            details["owner_continuation"]["tool"],
            "repair_start_non_force"
        );
        assert_eq!(details["owner_continuation"]["input"]["pr"], 2817);
        assert_eq!(details["owner_continuation"]["input"]["target_pr"], 2814);
        assert_eq!(
            details["owner_continuation"]["continue_requires"],
            json!(["same actor", "--no-sync"])
        );
        assert!(
            details["prior_operation_outcomes"]
                .as_str()
                .unwrap()
                .contains("unchanged")
        );
        let unrelated = owner_continuation_refusal(
            AppError::validation("different_guard", "leave this guard alone"),
            &plan,
            &plan.members[0],
        );
        assert_eq!(unrelated.code(), "different_guard");
    }

    #[test]
    fn plan_hash_binds_every_member_generation_and_intent() {
        let plan = sample_plan();
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
