//! Exact, no-write planning for concatenating two live caravans.
//!
//! Planning is a hard boundary: later execution may only consume this complete
//! topology receipt. It never sequences eviction and rejoin, never mutates the
//! provider, and never guesses which root/tail the operator meant.

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::graph::{CompatibilityChecker, GitCompatibilityChecker};
use crate::model::{BranchSnapshot, CommitOid, CompatibilityOutcome, PrNumber, RepositoryId};
use crate::read::{self, StatusOutput};
use crate::{AppContext, AppError};

/// Operator-reviewed intent to append one entire live caravan after another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConcatInput {
    /// Exact head/root of the source caravan to append.
    pub source_head_pr: u64,
    /// Exact current tail of the target caravan.
    pub target_tail_pr: u64,
    /// Non-secret audited actor identity.
    pub actor: String,
    /// Bounded operator rationale.
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
