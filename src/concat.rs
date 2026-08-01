//! Exact, no-write planning for concatenating two live caravans.
//!
//! Planning is a hard boundary: later execution may only consume this complete
//! topology receipt. It never sequences eviction and rejoin, never mutates the
//! provider, and never guesses which root/tail the operator meant.

use clap::Args;
use mcp_cli::{ErrorCategory, StructuredError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::graph::{CompatibilityChecker, GitCompatibilityChecker};
use crate::model::{BranchSnapshot, CommitOid, CompatibilityOutcome, PrNumber, RepositoryId};
use crate::read::{self, StatusOutput};
use crate::{AppContext, AppError};

/// Operator-reviewed intent to append one entire live caravan after another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Args)]
pub struct ConcatInput {
    /// Exact head/root of the source caravan to append.
    #[arg(long)]
    pub source_head_pr: u64,
    /// Exact current tail of the target caravan.
    #[arg(long)]
    pub target_tail_pr: u64,
    /// Non-secret audited actor identity.
    #[arg(long)]
    pub actor: String,
    /// Bounded operator rationale.
    #[arg(long)]
    pub reason: String,
}

/// Immutable member generation and symbolic rewrite target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConcatMemberPlan {
    pub pr: PrNumber,
    pub branch: String,
    pub old_head_oid: CommitOid,
    pub old_base: BranchSnapshot,
    /// Predecessor PR after concatenation. This is the target tail for the
    /// source root, then the preceding source member for every descendant.
    pub target_pr: PrNumber,
    pub target_branch: String,
    pub target_head_oid: CommitOid,
    /// Every source generation is prepared. Descendants retain their branch
    /// base name but must follow the rewritten parent head generation.
    pub requires_rewrite: bool,
}

/// Complete no-write receipt reviewed before any concat transaction begins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConcatPlan {
    pub schema_version: u32,
    pub repository: RepositoryId,
    pub source_caravan: PrNumber,
    pub target_caravan: PrNumber,
    pub source_members: Vec<PrNumber>,
    pub target_members: Vec<PrNumber>,
    pub old_ordering: Vec<Vec<PrNumber>>,
    pub new_ordering: Vec<PrNumber>,
    pub members: Vec<ConcatMemberPlan>,
    pub actor: String,
    pub reason: String,
    /// Stable hash over every preceding field with this field blank.
    pub plan_hash: String,
    /// Exact branches restored if execution must roll back before membership
    /// commit. Later slices bind the corresponding new-head leases.
    pub rollback_heads: Vec<BranchSnapshot>,
}

/// Read provider state once and produce a no-write concat plan.
pub fn plan(context: &AppContext, input: &ConcatInput) -> Result<ConcatPlan, AppError> {
    let _planning = context.acquire_planning_operation("plan-concat")?;
    let status = read::status(context)?;
    let checker = GitCompatibilityChecker::new(&context.repository_path, "origin").with_timeout(
        std::time::Duration::from_secs(context.config.command_timeout_secs),
    );
    plan_from_status(&status, input, &checker)
}

/// Pure planner seam used by the engine and hermetic tests. Keep the complete
/// refusal order visible: later execution relies on this being one reviewed
/// no-write barrier rather than partially initialized helper state.
#[allow(clippy::too_many_lines)]
pub(crate) fn plan_from_status(
    status: &StatusOutput,
    input: &ConcatInput,
    checker: &impl CompatibilityChecker,
) -> Result<ConcatPlan, AppError> {
    let actor = input.actor.trim();
    let reason = input.reason.trim();
    if actor.is_empty()
        || actor.len() > 256
        || actor != input.actor
        || actor.chars().any(char::is_control)
    {
        return Err(AppError::validation(
            "concat_actor_required",
            "concat requires a bounded non-empty audited actor without surrounding whitespace",
        ));
    }
    if reason.is_empty()
        || reason.len() > 512
        || reason != input.reason
        || reason.chars().any(char::is_control)
    {
        return Err(AppError::validation(
            "concat_reason_required",
            "concat requires a bounded non-empty rationale without surrounding whitespace",
        ));
    }
    if !status.healthy {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "concat_fleet_unhealthy",
            "concat requires a healthy complete fleet snapshot",
            Some(json!({"problems": status.analysis.fleet.problems})),
        ));
    }

    let source_head = PrNumber(input.source_head_pr);
    let target_tail = PrNumber(input.target_tail_pr);
    let source = status
        .analysis
        .fleet
        .caravans
        .iter()
        .find(|caravan| caravan.id == source_head)
        .ok_or_else(|| {
            AppError::validation(
                "concat_source_head_not_found",
                format!("PR #{source_head} is not a current caravan head"),
            )
        })?;
    let target = status
        .analysis
        .fleet
        .caravans
        .iter()
        .find(|caravan| caravan.tail() == Some(target_tail))
        .ok_or_else(|| {
            AppError::validation(
                "concat_target_tail_not_found",
                format!("PR #{target_tail} is not a current caravan tail"),
            )
        })?;
    if source.id == target.id {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "concat_cycle_refused",
            "source and target name the same caravan; concatenation would create a cycle",
            Some(json!({"caravan": source.id, "members": source.members})),
        ));
    }
    for caravan in [source, target] {
        if let Some(pause) = status
            .pauses
            .iter()
            .find(|pause| pause.state.is_effective() && pause.record.caravan_head == caravan.id)
        {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "concat_caravan_held",
                "concat refuses an intentionally held source or target caravan",
                Some(json!({"caravan": caravan.id, "pause": pause})),
            ));
        }
    }

    let target_snapshot = status
        .analysis
        .pull_requests
        .get(&target_tail)
        .ok_or_else(|| {
            AppError::validation("concat_target_incomplete", "target tail facts are missing")
        })?;
    let source_root = status
        .analysis
        .pull_requests
        .get(&source_head)
        .ok_or_else(|| {
            AppError::validation("concat_source_incomplete", "source root facts are missing")
        })?;
    if source_root.cross_repository
        || target_snapshot.cross_repository
        || source_root.head.repository != status.repository
        || target_snapshot.head.repository != status.repository
    {
        return Err(AppError::validation(
            "concat_fork_refused",
            "concat supports only same-repository owned caravan branches",
        ));
    }
    checker.prepare(&[source_root.head.clone(), target_snapshot.head.clone()])?;
    let compatibility = checker.check(&source_root.head, &target_snapshot.head)?;
    if compatibility.outcome != CompatibilityOutcome::Clean {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "concat_incompatible",
            "source root is not mechanically compatible with the exact target tail",
            Some(json!({"compatibility": compatibility, "mutated": false})),
        ));
    }

    let mut predecessor = target_snapshot;
    let mut members = Vec::with_capacity(source.members.len());
    let mut rollback_heads = Vec::with_capacity(source.members.len());
    for number in &source.members {
        let candidate = status.analysis.pull_requests.get(number).ok_or_else(|| {
            AppError::structured(
                ErrorCategory::Validation,
                "concat_source_incomplete",
                "source caravan member facts are missing",
                Some(json!({"source": source.id, "missing_pr": number})),
            )
        })?;
        if candidate.cross_repository || candidate.head.repository != status.repository {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "concat_fork_refused",
                "concat supports only same-repository owned caravan branches",
                Some(json!({"pr": number})),
            ));
        }
        members.push(ConcatMemberPlan {
            pr: *number,
            branch: candidate.head.name.clone(),
            old_head_oid: candidate.head.oid.clone(),
            old_base: candidate.base.clone(),
            target_pr: predecessor.number,
            target_branch: predecessor.head.name.clone(),
            target_head_oid: predecessor.head.oid.clone(),
            requires_rewrite: true,
        });
        rollback_heads.push(candidate.head.clone());
        predecessor = candidate;
    }

    let source_members = source.members.clone();
    let target_members = target.members.clone();
    let mut new_ordering = target_members.clone();
    new_ordering.extend(source_members.iter().copied());
    let mut plan = ConcatPlan {
        schema_version: 1,
        repository: status.repository.clone(),
        source_caravan: source.id,
        target_caravan: target.id,
        source_members: source_members.clone(),
        target_members: target_members.clone(),
        old_ordering: vec![target_members, source_members],
        new_ordering,
        members,
        actor: actor.to_owned(),
        reason: reason.to_owned(),
        plan_hash: String::new(),
        rollback_heads,
    };
    plan.plan_hash = crate::membership::fnv1a64(
        &serde_json::to_vec(&plan).expect("validated concat plan serializes"),
    );
    Ok(plan)
}

/// Execution request bound to one reviewed no-write plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Args)]
pub struct ConcatExecuteInput {
    #[serde(flatten)]
    #[command(flatten)]
    pub intent: ConcatInput,
    #[arg(long)]
    pub expected_plan_hash: String,
}

/// Successful atomic concat receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConcatOutput {
    /// True when an exact durable journal receipt was returned without writes.
    pub idempotent: bool,
    pub plan: ConcatPlan,
    pub physical: crate::physical_rebase::AtomicRebaseReceipt,
    pub membership: crate::github::GitHubMutationReceipt,
    pub resulting_ordering: Vec<PrNumber>,
    pub receipt: crate::model::OperationReceipt,
    #[serde(default)]
    pub events: Vec<crate::model::CaravanEvent>,
    #[serde(default)]
    pub hook_deliveries: Vec<crate::hooks::HookDelivery>,
}

trait ConcatProvider {
    fn set_base(
        &self,
        repository: &RepositoryId,
        expected: &crate::model::PullRequestPrecondition,
        base: &str,
    ) -> Result<crate::github::GitHubMutationReceipt, crate::github::MutationError>;
}

impl<R: crate::command::CommandRunner> ConcatProvider for crate::github::GitHubMutationAdapter<R> {
    fn set_base(
        &self,
        repository: &RepositoryId,
        expected: &crate::model::PullRequestPrecondition,
        base: &str,
    ) -> Result<crate::github::GitHubMutationReceipt, crate::github::MutationError> {
        crate::membership::MembershipProvider::set_base(self, repository, expected, base)
    }
}

/// Execute one reviewed concat plan under one writer operation.
#[allow(clippy::too_many_lines)]
pub fn execute(context: &AppContext, input: &ConcatExecuteInput) -> Result<ConcatOutput, AppError> {
    // Exact retry is a local durable read: do not acquire a remote writer lease
    // or touch provider state when the original receipt already exists.
    if let Some(output) = existing_receipt(context, &input.expected_plan_hash)? {
        return Ok(output);
    }
    let writer = context.acquire_writer_operation("concat")?;
    let operation_deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(context.config.sync.max_duration_secs);
    let status = read::status_with_deadline(context, operation_deadline)?;
    let timeout = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let checker = GitCompatibilityChecker::new(&context.repository_path, "origin")
        .with_timeout(timeout)
        .with_operation_deadline(operation_deadline);
    let plan = plan_from_status(&status, &input.intent, &checker)?;
    if input.expected_plan_hash != plan.plan_hash {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "concat_plan_stale",
            "concat facts changed after the reviewed plan; refusing before mutation",
            Some(
                json!({"expected_plan_hash": input.expected_plan_hash, "actual_plan": plan, "mutated": false}),
            ),
        ));
    }

    let source = status
        .analysis
        .fleet
        .caravan(plan.source_caravan)
        .expect("concat plan source is a live caravan");
    let target_tail = status
        .analysis
        .pull_requests
        .get(&plan.members[0].target_pr)
        .expect("concat plan target tail has facts");
    let mut prepared = Vec::with_capacity(source.members.len());
    let mut target = crate::physical_rebase::PlannedBase::Remote(target_tail.head.clone());
    for (index, number) in source.members.iter().enumerate() {
        let candidate = status
            .analysis
            .pull_requests
            .get(number)
            .expect("concat plan member has facts");
        let range = if index == 0 {
            crate::physical_rebase::range_base_for_remote_target(candidate, &target_tail.head)
        } else {
            let parent = status
                .analysis
                .pull_requests
                .get(&source.members[index - 1])
                .expect("concat source parent has facts");
            crate::physical_rebase::range_base_for_rewritten_parent(candidate, &parent.head)
        };
        let item = crate::physical_rebase::prepare_candidate(
            &context.repository_path,
            &status.repository,
            candidate,
            range,
            target,
            &status.analysis.fleet.default_branch,
            crate::physical_rebase::RebaseExecutionBudget::new(timeout)
                .with_deadline(operation_deadline)
                .with_writer_fence(writer.remote_fence())
                .because(
                    crate::physical_rebase::BranchRewriteReason::ParentAdvanced {
                        parent_pr: plan.members[index].target_pr,
                    },
                ),
        )?;
        target = crate::physical_rebase::PlannedBase::Simulated(BranchSnapshot {
            repository: status.repository.clone(),
            name: candidate.head.name.clone(),
            oid: item.plan.new_head_oid.clone(),
        });
        prepared.push(item);
    }
    let prepared_refs = prepared.iter().collect::<Vec<_>>();
    let physical = crate::physical_rebase::apply_prepared_atomically(&prepared_refs)?;

    let post_rewrite = match read::status_with_deadline(context, operation_deadline) {
        Ok(status) => status,
        Err(error) => {
            return rollback_physical_error(
                &error,
                &prepared_refs,
                &plan,
                "concat_post_rewrite_rediscovery_failed",
            );
        }
    };
    for receipt in &physical.receipts {
        let observed = post_rewrite.analysis.pull_requests.get(&receipt.pr);
        if observed.is_none_or(|pr| pr.head.oid != receipt.new_head_oid) {
            return rollback_physical_error(
                &AppError::validation(
                    "concat_rewrite_head_stale",
                    "provider did not expose every exact atomically rewritten head",
                ),
                &prepared_refs,
                &plan,
                "concat_post_rewrite_rediscovery_failed",
            );
        }
    }

    let runner = crate::command::ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(timeout)
        .with_operation_deadline(operation_deadline);
    let provider = crate::github::GitHubMutationAdapter::new(writer.runner(runner));
    let source_root = post_rewrite
        .analysis
        .pull_requests
        .get(&plan.source_caravan)
        .expect("rewritten source root remains open");
    let expected = crate::model::PullRequestPrecondition::from(source_root);
    let membership = match commit_source_root(
        &provider,
        &status.repository,
        &expected,
        &plan.members[0].target_branch,
    ) {
        Ok(receipt) => receipt,
        Err(error) => {
            let source = concat_mutation_error(&error, &plan, false);
            return rollback_physical_error(
                &source,
                &prepared_refs,
                &plan,
                "concat_membership_commit_failed",
            );
        }
    };

    let final_status = match read::status_with_deadline(context, operation_deadline) {
        Ok(status) => status,
        Err(error) => {
            return rollback_complete_transaction(
                &provider,
                &status.repository,
                &membership,
                &prepared_refs,
                &plan,
                &error,
                "concat_final_rediscovery_failed",
            );
        }
    };
    let Some(concatenated) = final_status
        .analysis
        .fleet
        .caravans
        .iter()
        .find(|caravan| caravan.members == plan.new_ordering)
    else {
        let source = AppError::structured(
            ErrorCategory::Validation,
            "concat_final_topology_unexpected",
            "provider writes completed but exact concatenated ordering was not observed",
            Some(json!({"fleet": final_status.analysis.fleet})),
        );
        return rollback_complete_transaction(
            &provider,
            &status.repository,
            &membership,
            &prepared_refs,
            &plan,
            &source,
            "concat_final_topology_rollback",
        );
    };

    let operation_id = crate::model::OperationId::new();
    let receipt = crate::model::OperationReceipt {
        operation_id: operation_id.clone(),
        operation: "concat".to_owned(),
        completed_steps: vec![crate::model::MutationStep {
            kind: crate::model::MutationKind::SetBase,
            state: crate::model::MutationStepState::Completed,
            pr: Some(plan.source_caravan),
            summary: format!(
                "concatenated caravan #{} after tail #{}",
                plan.source_caravan, input.intent.target_tail_pr
            ),
        }],
        changed: true,
    };
    let event = crate::hooks::event(
        crate::model::EventKind::CaravansConcatenated,
        operation_id,
        status.repository,
        Some(concatenated.id),
        plan.new_ordering.clone(),
        Some(final_status.analysis.fleet),
        Some(plan.reason.clone()),
        std::collections::BTreeMap::from([
            ("plan".to_owned(), json!(plan)),
            ("physical".to_owned(), json!(physical)),
            ("membership".to_owned(), json!(membership)),
            ("receipt".to_owned(), json!(receipt)),
            ("resulting_ordering".to_owned(), json!(plan.new_ordering)),
        ]),
    );
    let hook_deliveries = crate::hooks::dispatch_events(context, std::slice::from_ref(&event))?;
    Ok(ConcatOutput {
        idempotent: false,
        resulting_ordering: plan.new_ordering.clone(),
        plan,
        physical,
        membership,
        receipt,
        events: vec![event],
        hook_deliveries,
    })
}

fn rollback_complete_transaction(
    provider: &impl ConcatProvider,
    repository: &RepositoryId,
    membership: &crate::github::GitHubMutationReceipt,
    prepared: &[&crate::physical_rebase::PreparedRebase],
    plan: &ConcatPlan,
    source: &AppError,
    code: &str,
) -> Result<ConcatOutput, AppError> {
    let expected = crate::model::PullRequestPrecondition::from(&membership.after);
    let original_base = plan.members[0].old_base.name.as_str();
    let membership_rollback = match commit_source_root(
        provider,
        repository,
        &expected,
        original_base,
    ) {
        Ok(receipt) => receipt,
        Err(error) => {
            return Err(AppError::structured(
                ErrorCategory::ExecutionFailure,
                "concat_membership_rollback_indeterminate",
                "concat could not restore the source-root base after final verification failed",
                Some(
                    json!({"plan": plan, "membership": membership, "source": source.details(), "rollback_error": error.to_string(), "mutated": true, "safe_next_action": "inspect the source-root base and every source branch; do not retry until all generations are classified"}),
                ),
            ));
        }
    };
    match crate::physical_rebase::rollback_prepared_atomically(prepared) {
        Ok(physical_rollback) => Err(AppError::structured(
            source.category(),
            code,
            format!(
                "concat final verification failed and the complete original topology was restored: {source}"
            ),
            Some(
                json!({"plan": plan, "membership": membership, "membership_rollback": membership_rollback, "physical_rollback": physical_rollback, "source": source.details(), "mutated": false, "resumable": true}),
            ),
        )),
        Err(rollback) => Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "concat_physical_rollback_indeterminate",
            "source-root base was restored but original source branch heads could not be proven restored",
            Some(
                json!({"plan": plan, "membership": membership, "membership_rollback": membership_rollback, "source": source.details(), "rollback": rollback.details(), "mutated": true, "safe_next_action": "inspect every source branch; do not retry membership until all heads equal the plan rollback generations"}),
            ),
        )),
    }
}

fn commit_source_root(
    provider: &impl ConcatProvider,
    repository: &RepositoryId,
    expected: &crate::model::PullRequestPrecondition,
    base: &str,
) -> Result<crate::github::GitHubMutationReceipt, crate::github::MutationError> {
    provider.set_base(repository, expected, base)
}

fn existing_receipt(
    context: &AppContext,
    plan_hash: &str,
) -> Result<Option<ConcatOutput>, AppError> {
    let log = crate::journal::snapshot(
        context,
        &crate::journal::LogInput {
            limit: 100,
            kind: Some("caravans_concatenated".to_owned()),
            ..crate::journal::LogInput::default()
        },
    )?;
    for record in log.records.into_iter().rev() {
        let crate::journal::JournalRecord::Event { event, .. } = record else {
            continue;
        };
        let Some(plan_value) = event.metadata.get("plan") else {
            continue;
        };
        let plan: ConcatPlan = serde_json::from_value(plan_value.clone()).map_err(|error| {
            AppError::structured(
                ErrorCategory::ExecutionFailure,
                "concat_receipt_invalid",
                "durable concat event contains an unreadable plan",
                Some(json!({"event_id": event.event_id, "source": error.to_string()})),
            )
        })?;
        if plan.plan_hash != plan_hash {
            continue;
        }
        let physical = receipt_metadata(&event, "physical")?;
        let membership = receipt_metadata(&event, "membership")?;
        let receipt = receipt_metadata(&event, "receipt")?;
        let resulting_ordering = receipt_metadata(&event, "resulting_ordering")?;
        return Ok(Some(ConcatOutput {
            idempotent: true,
            plan,
            physical,
            membership,
            resulting_ordering,
            receipt,
            events: vec![event],
            hook_deliveries: Vec::new(),
        }));
    }
    Ok(None)
}

fn receipt_metadata<T: serde::de::DeserializeOwned>(
    event: &crate::model::CaravanEvent,
    key: &str,
) -> Result<T, AppError> {
    let value = event.metadata.get(key).ok_or_else(|| {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "concat_receipt_invalid",
            "durable concat event is missing required receipt evidence",
            Some(json!({"event_id": event.event_id, "missing": key})),
        )
    })?;
    serde_json::from_value(value.clone()).map_err(|error| {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "concat_receipt_invalid",
            "durable concat event contains unreadable receipt evidence",
            Some(json!({"event_id": event.event_id, "field": key, "source": error.to_string()})),
        )
    })
}

fn rollback_physical_error(
    source: &AppError,
    prepared: &[&crate::physical_rebase::PreparedRebase],
    plan: &ConcatPlan,
    code: &str,
) -> Result<ConcatOutput, AppError> {
    match crate::physical_rebase::rollback_prepared_atomically(prepared) {
        Ok(rollback) => Err(AppError::structured(
            source.category(),
            code,
            format!("concat stopped and restored every original source head: {source}"),
            Some(
                json!({"plan": plan, "rollback": rollback, "source": source.details(), "mutated": false, "resumable": true}),
            ),
        )),
        Err(rollback) => Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "concat_rollback_indeterminate",
            "concat failed after physical rewrite and original source heads could not be proven restored",
            Some(
                json!({"plan": plan, "source": source.details(), "rollback": rollback.details(), "mutated": true, "safe_next_action": "inspect every source branch and preserve both error receipts before any retry"}),
            ),
        )),
    }
}

fn concat_mutation_error(
    error: &crate::github::MutationError,
    plan: &ConcatPlan,
    mutated: bool,
) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "concat_provider_mutation_failed",
        format!("concat provider membership commit failed: {error}"),
        Some(json!({"plan": plan, "mutated": mutated})),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::analyze;
    use crate::model::{
        AutoMergeState, BranchSnapshot, Caravan, CompatibilityReport, PullRequestSnapshot,
        PullRequestState, RepositorySnapshot,
    };
    use mcp_cli::StructuredError;

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        }
    }

    fn pr(number: u64, head: &str, base: &str) -> PullRequestSnapshot {
        PullRequestSnapshot {
            number: PrNumber(number),
            title: format!("PR {number}"),
            url: format!("https://example.invalid/{number}"),
            state: PullRequestState::Open,
            draft: false,
            head: BranchSnapshot {
                repository: repository(),
                name: head.to_owned(),
                oid: CommitOid(format!("{head}-oid")),
            },
            base: BranchSnapshot {
                repository: repository(),
                name: base.to_owned(),
                oid: CommitOid(format!("{base}-oid")),
            },
            cross_repository: false,
            labels: std::collections::BTreeSet::from(["caravan".to_owned()]),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
            merge_state_status: None,
        }
    }

    fn status() -> StatusOutput {
        let pull_requests = vec![
            pr(1, "target-root", "main"),
            pr(2, "target-tail", "target-root"),
            pr(3, "source-root", "main"),
            pr(4, "source-tail", "source-root"),
        ];
        let snapshot = RepositorySnapshot {
            repository: repository(),
            default_branch: BranchSnapshot {
                repository: repository(),
                name: "main".to_owned(),
                oid: CommitOid("main-oid".to_owned()),
            },
            current_branch: None,
            current_pr: None,
            pull_requests,
            generation_facts: Vec::new(),
            observed_at: None,
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
        };
        let mut analysis = analyze(&snapshot, &Clean).unwrap();
        analysis.fleet.caravans = vec![
            Caravan::new(vec![PrNumber(1), PrNumber(2)]).unwrap(),
            Caravan::new(vec![PrNumber(3), PrNumber(4)]).unwrap(),
        ];
        let admission = crate::read::resolve_admission(
            &analysis,
            &crate::config::CaravanConfig::default().agent_priority_labels,
        );
        StatusOutput {
            runtime: crate::read::RuntimeProvenance::default(),
            config_provenance: None,
            provider_api: crate::model::GitHubApiTelemetry::default(),
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            timing: None,
            repository: repository(),
            rebase_on_join: crate::read::RebaseOnJoinStatus::default(),
            stack_backend: crate::read::StackBackendStatus::default(),
            head_merge: crate::read::HeadMergeStatus::default(),
            auto_admission: crate::read::AutoAdmissionStatus::default(),
            default_branch: "main".to_owned(),
            current_branch: None,
            current_pr: None,
            healthy: true,
            initialization: crate::initialization::InitializationStatus::default(),
            sync_budget: crate::sync::SyncBudgetStatus::default(),
            analysis,
            pauses: Vec::new(),
            admission,
        }
    }

    struct Clean;
    impl CompatibilityChecker for Clean {
        fn check(
            &self,
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
    }

    struct FakeConcatProvider {
        current: std::cell::RefCell<PullRequestSnapshot>,
    }

    impl ConcatProvider for FakeConcatProvider {
        fn set_base(
            &self,
            _repository: &RepositoryId,
            expected: &crate::model::PullRequestPrecondition,
            base: &str,
        ) -> Result<crate::github::GitHubMutationReceipt, crate::github::MutationError> {
            let mut current = self.current.borrow_mut();
            let actual = crate::model::PullRequestPrecondition::from(&*current);
            if !actual.mutation_identity_eq(expected) {
                return Err(crate::github::MutationError::StalePrecondition {
                    expected: Box::new(expected.clone()),
                    actual: Box::new(actual),
                    changed_fields: vec!["fixture_drift".to_owned()],
                });
            }
            let before = current.clone();
            current.base.name = base.to_owned();
            current.base.oid = CommitOid(format!("{base}-oid"));
            Ok(crate::github::GitHubMutationReceipt {
                kind: crate::model::MutationKind::SetBase,
                before: Some(before),
                after: current.clone(),
                provider_output: None,
            })
        }
    }

    struct Conflicting;
    impl CompatibilityChecker for Conflicting {
        fn check(
            &self,
            candidate: &BranchSnapshot,
            target: &BranchSnapshot,
        ) -> Result<CompatibilityReport, AppError> {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: CompatibilityOutcome::Conflict,
                conflicting_paths: vec!["shared.txt".to_owned()],
                diagnostic: Some("fixture conflict".to_owned()),
            })
        }
    }

    #[test]
    fn plan_preserves_both_orders_and_appends_the_complete_source() {
        let plan = plan_from_status(
            &status(),
            &ConcatInput {
                source_head_pr: 3,
                target_tail_pr: 2,
                actor: "operator".to_owned(),
                reason: "recover split roots".to_owned(),
            },
            &Clean,
        )
        .unwrap();
        assert_eq!(
            plan.old_ordering,
            vec![
                vec![PrNumber(1), PrNumber(2)],
                vec![PrNumber(3), PrNumber(4)]
            ]
        );
        assert_eq!(
            plan.new_ordering,
            vec![PrNumber(1), PrNumber(2), PrNumber(3), PrNumber(4)]
        );
        assert_eq!(
            plan.members
                .iter()
                .map(|member| (member.pr, member.target_pr))
                .collect::<Vec<_>>(),
            vec![(PrNumber(3), PrNumber(2)), (PrNumber(4), PrNumber(3))]
        );
        assert_eq!(plan.rollback_heads.len(), 2);
        assert!(plan.plan_hash.starts_with("fnv1a64:"));
        let mut unhashed = plan.clone();
        let expected = unhashed.plan_hash.clone();
        unhashed.plan_hash.clear();
        assert_eq!(
            crate::membership::fnv1a64(&serde_json::to_vec(&unhashed).unwrap()),
            expected
        );
    }

    #[test]
    fn plan_refuses_cycles_missing_heads_and_empty_audit() {
        let status = status();
        for (input, code) in [
            (
                ConcatInput {
                    source_head_pr: 1,
                    target_tail_pr: 2,
                    actor: "operator".to_owned(),
                    reason: "cycle".to_owned(),
                },
                "concat_cycle_refused",
            ),
            (
                ConcatInput {
                    source_head_pr: 99,
                    target_tail_pr: 2,
                    actor: "operator".to_owned(),
                    reason: "missing".to_owned(),
                },
                "concat_source_head_not_found",
            ),
            (
                ConcatInput {
                    source_head_pr: 3,
                    target_tail_pr: 99,
                    actor: "operator".to_owned(),
                    reason: "missing".to_owned(),
                },
                "concat_target_tail_not_found",
            ),
            (
                ConcatInput {
                    source_head_pr: 3,
                    target_tail_pr: 2,
                    actor: String::new(),
                    reason: "missing actor".to_owned(),
                },
                "concat_actor_required",
            ),
            (
                ConcatInput {
                    source_head_pr: 3,
                    target_tail_pr: 2,
                    actor: " operator".to_owned(),
                    reason: "space".to_owned(),
                },
                "concat_actor_required",
            ),
        ] {
            assert_eq!(
                plan_from_status(&status, &input, &Clean)
                    .unwrap_err()
                    .code(),
                code
            );
        }
    }

    #[test]
    fn membership_commit_and_rollback_use_exact_after_state_leases() {
        let source = pr(3, "source-root", "main");
        let provider = FakeConcatProvider {
            current: std::cell::RefCell::new(source.clone()),
        };
        let expected = crate::model::PullRequestPrecondition::from(&source);
        let committed = commit_source_root(&provider, &repository(), &expected, "target-tail")
            .expect("single membership commit");
        assert_eq!(provider.current.borrow().base.name, "target-tail");

        let rollback_expected = crate::model::PullRequestPrecondition::from(&committed.after);
        let rolled_back = commit_source_root(&provider, &repository(), &rollback_expected, "main")
            .expect("exact rollback restores original base");
        assert_eq!(rolled_back.after.base.name, "main");

        let committed = commit_source_root(
            &provider,
            &repository(),
            &crate::model::PullRequestPrecondition::from(&rolled_back.after),
            "target-tail",
        )
        .unwrap();
        provider.current.borrow_mut().head.oid = CommitOid("external-drift".to_owned());
        let error = commit_source_root(
            &provider,
            &repository(),
            &crate::model::PullRequestPrecondition::from(&committed.after),
            "main",
        )
        .expect_err("drift refuses rollback rather than overwriting");
        assert!(matches!(
            error,
            crate::github::MutationError::StalePrecondition { .. }
        ));
        assert_eq!(provider.current.borrow().base.name, "target-tail");
    }

    #[test]
    fn durable_receipt_makes_exact_retry_no_write_and_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(directory.path())
            .status()
            .unwrap();
        let context = AppContext {
            repository_path: directory.path().to_path_buf(),
            ..AppContext::default()
        };
        let status = status();
        let plan = plan_from_status(
            &status,
            &ConcatInput {
                source_head_pr: 3,
                target_tail_pr: 2,
                actor: "operator".to_owned(),
                reason: "recover".to_owned(),
            },
            &Clean,
        )
        .unwrap();
        let source = status.analysis.pull_requests[&PrNumber(3)].clone();
        let mut after = source.clone();
        after.base = status.analysis.pull_requests[&PrNumber(2)].head.clone();
        let membership = crate::github::GitHubMutationReceipt {
            kind: crate::model::MutationKind::SetBase,
            before: Some(source),
            after,
            provider_output: None,
        };
        let physical = crate::physical_rebase::AtomicRebaseReceipt {
            atomic: true,
            receipts: Vec::new(),
            old_heads: plan.rollback_heads.clone(),
            new_heads: plan.rollback_heads.clone(),
            recovered_ambiguous_success: false,
        };
        let receipt = crate::model::OperationReceipt {
            operation_id: crate::model::OperationId::new(),
            operation: "concat".to_owned(),
            completed_steps: Vec::new(),
            changed: true,
        };
        let event = crate::hooks::event(
            crate::model::EventKind::CaravansConcatenated,
            receipt.operation_id.clone(),
            repository(),
            Some(PrNumber(1)),
            plan.new_ordering.clone(),
            Some(status.analysis.fleet),
            Some(plan.reason.clone()),
            std::collections::BTreeMap::from([
                ("plan".to_owned(), json!(plan)),
                ("physical".to_owned(), json!(physical)),
                ("membership".to_owned(), json!(membership)),
                ("receipt".to_owned(), json!(receipt)),
                (
                    "resulting_ordering".to_owned(),
                    json!([PrNumber(1), PrNumber(2), PrNumber(3), PrNumber(4)]),
                ),
            ]),
        );
        crate::journal::append_event(&context, &event).unwrap();

        let retry = existing_receipt(&context, &plan.plan_hash)
            .unwrap()
            .expect("exact durable receipt");
        assert!(retry.idempotent);
        assert_eq!(retry.plan, plan);
        assert_eq!(retry.physical, physical);
        assert_eq!(retry.membership, membership);
        assert_eq!(retry.receipt, receipt);
        assert_eq!(retry.events, vec![event]);
    }

    #[test]
    fn plan_refuses_conflicts_forks_holds_and_incomplete_source() {
        let input = ConcatInput {
            source_head_pr: 3,
            target_tail_pr: 2,
            actor: "operator".to_owned(),
            reason: "recovery".to_owned(),
        };
        assert_eq!(
            plan_from_status(&status(), &input, &Conflicting)
                .unwrap_err()
                .code(),
            "concat_incompatible"
        );

        let mut forked = status();
        forked
            .analysis
            .pull_requests
            .get_mut(&PrNumber(3))
            .unwrap()
            .cross_repository = true;
        assert_eq!(
            plan_from_status(&forked, &input, &Clean)
                .unwrap_err()
                .code(),
            "concat_fork_refused"
        );

        let mut incomplete = status();
        incomplete.analysis.pull_requests.remove(&PrNumber(4));
        assert_eq!(
            plan_from_status(&incomplete, &input, &Clean)
                .unwrap_err()
                .code(),
            "concat_source_incomplete"
        );

        let mut held = status();
        let source_root = held.analysis.pull_requests[&PrNumber(3)].clone();
        held.pauses.push(crate::pause::PauseStatus {
            record: crate::pause::PauseRecord {
                version: 1,
                caravan_head: PrNumber(3),
                members: vec![PrNumber(3), PrNumber(4)],
                expected_head: crate::model::PullRequestPrecondition::from(&source_root),
                expected_checks: Vec::new(),
                actor: "operator".to_owned(),
                reason: "incident".to_owned(),
                paused_unix_secs: 1,
                expires_unix_secs: None,
                external_reference: None,
                resume_authorized_by: None,
            },
            state: crate::pause::PauseState::Active,
            auto_merge_suspended: true,
            retired_state: None,
            safe_next_action: "resume".to_owned(),
        });
        assert_eq!(
            plan_from_status(&held, &input, &Clean).unwrap_err().code(),
            "concat_caravan_held"
        );
    }
}
