//! bd-c299b8: healthy native topology must not strand a stale-base root.
use super::native_quarantine::{drift_repair_context, drifted_stack};
use super::*;
use crate::generation::CommitRelation;
use crate::native_stack_rebase::{
    NativeStackRebaseOutput, SyncDriftRepair, SyncRepairKind, pending_sync_repair_plans,
    stale_base_repair_plans,
};

fn stale_status() -> StatusOutput {
    let pulls = vec![
        caravan_member(1, "root", "main"),
        caravan_member(2, "child", "root"),
    ];
    let mut status = caravan_status_with_containment(pulls, Some(PrNumber(1)), false, false);
    enable_native_backend(&mut status);
    let mut native = drifted_stack(&status, &[]);
    native.consistency = crate::read::StackConsistency::Exact;
    native.ancestry = vec![crate::read::NativeStackAncestryEdge {
        parent_pr: PrNumber(1),
        parent_head: status.analysis.pull_requests[&PrNumber(1)].head.oid.clone(),
        child_pr: PrNumber(2),
        child_head: status.analysis.pull_requests[&PrNumber(2)].head.oid.clone(),
        relation: CommitRelation::Ahead,
        linear: true,
    }];
    status.stack_backend.native_stacks = vec![native];
    status.analysis.fleet.default_branch.oid = CommitOid("f".repeat(40));
    for proof in &mut status.analysis.cumulative_trees {
        proof.target = status.analysis.fleet.default_branch.clone();
    }
    status
}

fn active_pause(status: &StatusOutput) -> crate::pause::PauseStatus {
    let root = &status.analysis.pull_requests[&PrNumber(1)];
    crate::pause::PauseStatus {
        record: crate::pause::PauseRecord {
            version: 1,
            caravan_head: root.number,
            members: vec![PrNumber(1), PrNumber(2)],
            expected_head: PullRequestPrecondition::from(root),
            expected_checks: root.checks.clone(),
            actor: "operator".to_owned(),
            reason: "hold".to_owned(),
            paused_unix_secs: 1,
            expires_unix_secs: None,
            external_reference: None,
            resume_authorized_by: None,
            recovery: None,
        },
        state: crate::pause::PauseState::Active,
        auto_merge_suspended: true,
        retired_state: None,
        safe_next_action: "explicit resume".to_owned(),
    }
}

fn context() -> (tempfile::TempDir, AppContext) {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(directory.path())
            .status()
            .unwrap()
            .success()
    );
    let mut context = drift_repair_context(false, true);
    context.repository_path = directory.path().to_path_buf();
    (directory, context)
}

#[test]
fn healthy_stale_base_refreshes_root_and_entire_chain() {
    let status = stale_status();
    assert!(status.healthy);
    assert!(status.stack_backend.problems.is_empty());
    assert!(
        crate::native_stack_rebase::drift_repair_plans(&drift_repair_context(false, true), &status)
            .is_empty()
    );
    let plans = stale_base_repair_plans(&drift_repair_context(false, true), &status);
    assert_eq!(plans.len(), 1);
    let plan = &plans[0].1;
    assert!(plan.verify());
    assert_eq!(
        plan.provider_before,
        status.stack_backend.native_stacks[0].stack
    );
    assert_eq!(
        plan.members
            .iter()
            .map(|member| member.pr)
            .collect::<Vec<_>>(),
        vec![PrNumber(1), PrNumber(2)]
    );
    let root = &plan.members[0];
    assert_eq!(root.parent_pr, None);
    assert_eq!(root.parent_branch, "main");
    assert_eq!(root.parent_head, status.analysis.fleet.default_branch.oid);
    assert_eq!(
        root.old_head,
        status.analysis.pull_requests[&PrNumber(1)].head.oid
    );
    let child = &plan.members[1];
    assert_eq!(child.parent_pr, Some(PrNumber(1)));
    assert_eq!(child.parent_head, root.old_head);
    assert_eq!(
        child.old_head,
        status.analysis.pull_requests[&PrNumber(2)].head.oid
    );
    assert!(plan.reason.contains("refreshing stale base"));
}

#[test]
fn stale_base_refresh_preserves_policy_holds_and_exact_evidence() {
    let status = stale_status();
    let allowed = drift_repair_context(false, true);
    for context in [
        drift_repair_context(true, true),
        drift_repair_context(false, false),
        AppContext::default(),
    ] {
        assert!(stale_base_repair_plans(&context, &status).is_empty());
    }
    for defect in [
        "parked",
        "paused",
        "truncated",
        "closed",
        "orphan",
        "unknown",
        "problem",
        "unknown-ancestry",
        "draft",
        "root-closed",
        "other-base",
        "missing-proof",
        "old-head-proof",
        "old-default-proof",
        "identical-tree",
        "already-contains-default",
    ] {
        let mut other = status.clone();
        match defect {
            "parked" => other.analysis.fleet.caravans[0].parked = true,
            "paused" => other.pauses.push(active_pause(&status)),
            "truncated" => other.stack_backend.provider_stacks_truncated = true,
            "closed" => other.stack_backend.native_stacks[0].stack.open = false,
            "orphan" => other.stack_backend.native_stacks[0].caravan_id = None,
            "unknown" => {
                other.stack_backend.native_stacks[0].consistency =
                    crate::read::StackConsistency::Unknown
            }
            "problem" => other.stack_backend.native_stacks[0].problems.push(
                crate::read::StackBackendProblem {
                    code: "github_stack_base_drift".to_owned(),
                    message: "inexact".to_owned(),
                },
            ),
            "unknown-ancestry" => {
                other.stack_backend.native_stacks[0].ancestry[0].relation =
                    CommitRelation::Unknown {
                        reason: "unobserved".to_owned(),
                    }
            }
            "draft" => {
                other
                    .analysis
                    .pull_requests
                    .get_mut(&PrNumber(1))
                    .unwrap()
                    .draft = true
            }
            "root-closed" => {
                other
                    .analysis
                    .pull_requests
                    .get_mut(&PrNumber(1))
                    .unwrap()
                    .state = PullRequestState::Closed
            }
            "other-base" => {
                other
                    .analysis
                    .pull_requests
                    .get_mut(&PrNumber(1))
                    .unwrap()
                    .base
                    .name = "unmerged-parent".to_owned()
            }
            "missing-proof" => other.analysis.cumulative_trees.clear(),
            "old-head-proof" => {
                other.analysis.cumulative_trees[0].candidate.oid = CommitOid("old-head".to_owned())
            }
            "old-default-proof" => {
                other.analysis.cumulative_trees[0].target.oid = CommitOid("old-main".to_owned())
            }
            "identical-tree" => other.analysis.cumulative_trees[0].identical = true,
            "already-contains-default" => {
                other.analysis.cumulative_trees[0].target_reachable_from_candidate = true
            }
            _ => unreachable!(),
        }
        assert!(
            stale_base_repair_plans(&allowed, &other).is_empty(),
            "{defect}"
        );
    }
}

#[test]
fn stale_synthetic_with_current_pr_base_requires_exact_identity() {
    let mut status = stale_status();
    let root = status.analysis.pull_requests.get_mut(&PrNumber(1)).unwrap();
    root.base = status.analysis.fleet.default_branch.clone();
    status.merge_candidates = vec![crate::model::MergeCandidateIdentity {
        pr: root.number,
        provider_updated_at: "2026-10-25T03:00:00Z".to_owned(),
        observed_at: "2026-10-25T03:01:00Z".to_owned(),
        base: root.base.clone(),
        head: root.head.clone(),
        synthetic: None,
        auto_merge: crate::model::NativeAutoMergeState {
            enabled: false,
            merge_method: None,
            actor: None,
        },
        freshness: crate::model::MergeCandidateFreshness::StaleBase,
        compared_base: Some(root.base.clone()),
        stale_base: true,
        stale_head: false,
        stale_reasons: vec!["merge ref has not caught up with current main".to_owned()],
    }];
    let context = drift_repair_context(false, true);
    assert_eq!(
        stale_base_repair_plans(&context, &status)[0]
            .1
            .members
            .len(),
        2
    );
    for defect in [
        "old-head",
        "old-base",
        "old-comparison",
        "stale-head",
        "unknown",
        "fresh",
    ] {
        let mut other = status.clone();
        let candidate = &mut other.merge_candidates[0];
        match defect {
            "old-head" => candidate.head.oid = CommitOid("old-head".to_owned()),
            "old-base" => candidate.base.oid = CommitOid("old-base".to_owned()),
            "old-comparison" => candidate.compared_base = None,
            "stale-head" => candidate.stale_head = true,
            "unknown" => candidate.freshness = crate::model::MergeCandidateFreshness::Unknown,
            "fresh" => candidate.stale_base = false,
            _ => unreachable!(),
        }
        assert!(
            stale_base_repair_plans(&context, &other).is_empty(),
            "{defect}"
        );
    }
}

#[test]
fn exact_conflict_enters_existing_typed_repair_instead_of_idling() {
    let mut status = stale_status();
    status.analysis.cumulative_trees.clear();
    let root = &status.analysis.pull_requests[&PrNumber(1)];
    status.analysis.compatibility = vec![CompatibilityReport {
        candidate: root.head.clone(),
        target: status.analysis.fleet.default_branch.clone(),
        outcome: CompatibilityOutcome::Conflict,
        conflicting_paths: vec!["release.txt".to_owned()],
        diagnostic: None,
    }];
    let context = drift_repair_context(false, true);
    assert_eq!(stale_base_repair_plans(&context, &status).len(), 1);
    status.analysis.compatibility[0].outcome = CompatibilityOutcome::Unknown;
    assert!(stale_base_repair_plans(&context, &status).is_empty());
    status.analysis.compatibility[0].outcome = CompatibilityOutcome::Conflict;
    status.analysis.compatibility[0].target.oid = CommitOid("old-main".to_owned());
    assert!(stale_base_repair_plans(&context, &status).is_empty());
}

#[test]
fn repaired_heads_do_not_refresh_again_for_a_historical_pr_base() {
    let mut status = stale_status();
    let root = status.analysis.pull_requests.get_mut(&PrNumber(1)).unwrap();
    assert_ne!(root.base.oid, status.analysis.fleet.default_branch.oid);
    root.head.oid = CommitOid("rebased-root".to_owned());
    root.checks[0].state = CheckState::InProgress;
    status.analysis.cumulative_trees[0].candidate = root.head.clone();
    status.analysis.cumulative_trees[0].identical = true;
    status.analysis.cumulative_trees[0].target_reachable_from_candidate = true;
    assert!(stale_base_repair_plans(&drift_repair_context(false, true), &status).is_empty());
}

fn repair_output(status: &StatusOutput) -> NativeStackRebaseOutput {
    NativeStackRebaseOutput {
        plan: stale_base_repair_plans(&drift_repair_context(false, true), status)
            .remove(0)
            .1,
        mutated: true,
        physical: None,
        drift_clear: None,
        replacement: None,
        eviction: None,
        retargets: Vec::new(),
        fresh_ci_required: true,
        next: "wait for fresh CI".to_owned(),
    }
}

#[test]
fn stale_base_preview_and_sync_share_the_plan_and_deduplicate_completed_generations() {
    let (directory, context) = context();
    let status = stale_status();
    let input = SyncInput {
        all: true,
        dry_run: true,
        ..SyncInput::default()
    };
    let pending = pending_sync_repair_plans(&context, &status, SyncRepairKind::StaleBase).unwrap();
    let preview = plan::native_repair_plan(&context, &status, &input, 0)
        .unwrap()
        .unwrap();
    assert_eq!(preview.provider_writes, 0);
    assert!(!preview.mutated);
    assert_eq!(
        preview.actions[0].target.as_ref().unwrap()["repair"],
        "stale_base"
    );
    assert_eq!(
        preview.actions[0].target.as_ref().unwrap()["plan_hash"],
        pending[0].1.plan_hash
    );
    assert!(preview.actions[0].reason.contains("refreshing stale base"));
    assert_eq!(
        preview.actions[1].state,
        SyncPlanActionState::DeferredUntilRediscovery
    );
    let mut output = repair_output(&status);
    output.plan = pending[0].1.clone();
    let key = crate::native_stack_rebase::receipt_key(&output.plan.plan_hash).unwrap();
    crate::stack_checkpoint::write(directory.path(), &key, &output).unwrap();
    for _ in 0..2 {
        assert!(
            pending_sync_repair_plans(&context, &status, SyncRepairKind::StaleBase)
                .unwrap()
                .is_empty()
        );
        assert!(
            plan::native_repair_plan(&context, &status, &input, 0)
                .unwrap()
                .is_none()
        );
    }
    let mut moved = status;
    moved.analysis.fleet.default_branch.oid = CommitOid("new-default".to_owned());
    moved.analysis.cumulative_trees[0].target = moved.analysis.fleet.default_branch.clone();
    let fresh = pending_sync_repair_plans(&context, &moved, SyncRepairKind::StaleBase).unwrap();
    assert_eq!(fresh.len(), 1);
    assert_ne!(fresh[0].1.plan_hash, output.plan.plan_hash);
}

#[test]
fn stale_base_tick_reports_refresh_and_returns_before_landing_or_ci_dispatch() {
    let (_directory, context) = context();
    let status = stale_status();
    let repair = SyncDriftRepair {
        output: repair_output(&status),
        status,
        kind: SyncRepairKind::StaleBase,
    };
    let mut writer = context.acquire_writer_operation("stale-base-test").unwrap();
    let started = Instant::now();
    let output = native_drift_repair_output(
        &context,
        &SyncInput::default(),
        started,
        started + Duration::from_secs(30),
        Duration::ZERO,
        repair,
        None,
        &mut writer,
    )
    .unwrap();
    assert!(output.tick.changed);
    assert!(
        output.receipt.completed_steps[0]
            .summary
            .contains("refreshing stale base")
    );
    assert!(output.scheduler_status.reason.contains("fresh CI"));
    assert_eq!(
        output.scheduler_status.disposition,
        SchedulerDisposition::RetryTick
    );
    assert!(output.root_merge.is_empty());
    assert!(output.native_stack_land.is_empty());
    assert!(output.ci_generation_dispatches.is_empty());
    assert!(output.auto_admission.joins.is_empty());
}
