//! bd-7efe6a: the queue repairs a drifted native Stack that still holds closed
//! history instead of refusing every tick with
//! `github_stack_closed_history_requires_owner`.
//!
//! On 2026-10-21 Cacophony Stack #4597 kept merged #4595, #4596, #4598 and
//! #4601 plus a closed stale flake bump (#4600) ahead of its only open member
//! (#4603, which also conflicted with main). Every tick exited 1 while that
//! member held the only `max_caravans` slot, so #4604-#4606 waited until an
//! operator rebased and merged by hand.

use super::*;
use crate::native_stack_rebase::{
    NativeStackConflictEviction, NativeStackRebaseOutput, NativeStackRepairRequest, SyncRepairKind,
};

fn row(
    number: u64,
    state: &str,
    merged: bool,
    head: &str,
) -> crate::github::GitHubStackPullRequest {
    crate::github::GitHubStackPullRequest {
        number,
        state: state.to_owned(),
        draft: false,
        merged_at: merged.then(|| "2026-10-21T11:30:10Z".to_owned()),
        head: crate::github::GitHubStackPullRequestHead {
            ref_name: head.to_owned(),
            sha: CommitOid(format!("{head:0<40}")),
        },
    }
}

fn open_row(status: &StatusOutput, number: u64) -> crate::github::GitHubStackPullRequest {
    let pull = &status.analysis.pull_requests[&PrNumber(number)];
    crate::github::GitHubStackPullRequest {
        number,
        state: "open".to_owned(),
        draft: false,
        merged_at: None,
        head: crate::github::GitHubStackPullRequestHead {
            ref_name: pull.head.name.clone(),
            sha: pull.head.oid.clone(),
        },
    }
}

fn problem(code: &str) -> crate::read::StackBackendProblem {
    crate::read::StackBackendProblem {
        code: code.to_owned(),
        message: format!("Stack #4597 {code}"),
    }
}

/// Map one drifted provider Stack holding `rows` onto caravan `caravan_id`.
fn with_stack(
    status: &mut StatusOutput,
    caravan_id: u64,
    rows: Vec<crate::github::GitHubStackPullRequest>,
    codes: &[&str],
) {
    enable_native_backend(status);
    let native = crate::read::NativeStackStatus {
        stack: crate::github::GitHubStackSnapshot {
            id: 2_397_539,
            number: 4597,
            node_id: "PRS_kwDORokKA84AJJVj".to_owned(),
            base: crate::github::GitHubStackBase {
                ref_name: "main".to_owned(),
            },
            open: true,
            created_at: "2026-10-21T03:23:13Z".to_owned(),
            pull_requests: rows,
        },
        caravan_id: Some(PrNumber(caravan_id)),
        consistency: crate::read::StackConsistency::Drifted,
        ancestry: Vec::new(),
        problems: codes.iter().map(|code| problem(code)).collect(),
    };
    status.stack_backend.problems = native.problems.clone();
    status.stack_backend.native_stacks = vec![native];
}

/// Two active members behind four merged rows and one closed-unmerged row.
fn closed_history_status() -> StatusOutput {
    let pulls = vec![
        caravan_member(1, "root", "main"),
        caravan_member(2, "child", "root"),
    ];
    let mut status = caravan_status(pulls, Some(PrNumber(1)), true);
    let rows = vec![
        row(10, "closed", true, "merged-a"),
        row(11, "closed", true, "merged-b"),
        row(12, "closed", false, "stale-flake"),
        open_row(&status, 1),
        open_row(&status, 2),
    ];
    with_stack(
        &mut status,
        1,
        rows,
        &[
            "github_stack_merged_prefix_invalid",
            "github_stack_pr_base_drift",
        ],
    );
    status
}

fn repair_context(repository_path: Option<&std::path::Path>) -> AppContext {
    let mut context = AppContext::default();
    if let Some(path) = repository_path {
        context.repository_path = path.to_path_buf();
    }
    context.config.stack_type = crate::config::StackType::Github;
    context.config.stack_rollout.mutations_opt_in = true;
    context.config.stack_rollout.reviewed_by = "operator".to_owned();
    context
}

fn git_repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(directory.path())
            .status()
            .unwrap()
            .success()
    );
    directory
}

/// Closed rows are pruned from the rebuilt representation, never rewritten:
/// every active member is replayed linearly from the exact default branch and
/// the sealed plan retains the raw provider history byte-for-byte.
#[test]
fn closed_history_stack_is_rebuilt_linearly_by_the_queue() {
    let status = closed_history_status();
    let native = &status.stack_backend.native_stacks[0];
    assert!(crate::stack_recovery::closed_history_requires_rebuild(
        native
    ));
    assert!(
        !crate::read::native_stack_quarantinable(native),
        "before bd-7efe6a this Stack failed the whole backend"
    );

    let plans =
        crate::native_stack_rebase::closed_history_repair_plans(&repair_context(None), &status);
    assert_eq!(plans.len(), 1);
    let plan = &plans[0].1;
    assert!(plan.verify());
    assert_eq!(plan.stack, 4597);
    assert_eq!(plan.root_pr, PrNumber(1));
    assert_eq!(
        plan.actor,
        crate::native_stack_rebase::SYNC_DRIFT_REPAIR_ACTOR
    );
    assert!(plan.reason.contains("bd-7efe6a"));
    // Raw history, including the closed-unmerged row, stays in the plan.
    assert_eq!(plan.provider_before, native.stack);
    assert_eq!(plan.provider_before.pull_requests.len(), 5);
    // Linear replay of every active member from the default branch.
    assert_eq!(
        plan.members
            .iter()
            .map(|member| member.pr)
            .collect::<Vec<_>>(),
        vec![PrNumber(1), PrNumber(2)]
    );
    assert_eq!(plan.members[0].parent_pr, None);
    assert_eq!(plan.members[0].parent_branch, "main");
    assert_eq!(
        plan.members[0].parent_head,
        status.analysis.fleet.default_branch.oid
    );
    assert_eq!(plan.members[1].parent_pr, Some(PrNumber(1)));
    assert_eq!(plan.members[1].parent_branch, "root");
    assert_eq!(
        plan.members[1].parent_head,
        status.analysis.pull_requests[&PrNumber(1)].head.oid
    );

    // Routine drift repair and closed-history rebuild never select the same
    // Stack.
    assert!(
        crate::native_stack_rebase::drift_repair_plans(&repair_context(None), &status).is_empty()
    );
}

/// The replacement shape is validated before any Stack write against each
/// survivor's linear predecessor, so a root still based on a closed branch is
/// rebuilt onto the default branch rather than refused after the clear.
#[test]
fn survivors_are_projected_onto_their_linear_predecessors() {
    let pulls = vec![
        caravan_member(1, "root", "stale-flake"),
        caravan_member(2, "child", "root"),
        caravan_member(3, "grandchild", "child"),
    ];
    let status = caravan_status(pulls, Some(PrNumber(1)), true);
    // #3 was evicted; the surviving prefix is projected linearly.
    let projected =
        crate::native_stack_rebase::project_linear_bases(&status, &[PrNumber(1), PrNumber(2)])
            .unwrap();
    assert_eq!(projected.len(), 2);
    assert_eq!(projected[0].base, status.analysis.fleet.default_branch);
    assert_eq!(
        projected[0].head,
        status.analysis.pull_requests[&PrNumber(1)].head
    );
    assert_eq!(projected[1].base, projected[0].head);
    let topology = crate::stack_membership::topology_from_members(
        &status.analysis.fleet.default_branch,
        projected.iter(),
    )
    .expect("the projected survivors form an exact linear Stack");
    assert_eq!(topology.entries[0].base.name, "main");
    assert!(
        crate::native_stack_rebase::project_linear_bases(&status, &[])
            .unwrap()
            .is_empty()
    );
    assert!(crate::native_stack_rebase::project_linear_bases(&status, &[PrNumber(9)]).is_err());
}

/// The exact Cacophony shape: four merged rows, a closed flake bump, and one
/// open member whose base is still the closed PR's branch.
#[test]
fn cacophony_stack_4597_singleton_is_replayed_onto_main() {
    let pulls = vec![caravan_member(4603, "agent-4603", "update-flake")];
    let mut status = caravan_status(pulls, Some(PrNumber(4603)), true);
    assert_eq!(status.analysis.fleet.caravans.len(), 1);
    let rows = vec![
        row(4595, "closed", true, "pd1-retire-pi-plugins"),
        row(4596, "closed", true, "fix/standalone-launch-pane"),
        row(4598, "closed", true, "agent-4598"),
        row(4601, "closed", true, "agent-4601"),
        row(4600, "closed", false, "update-flake"),
        open_row(&status, 4603),
    ];
    with_stack(
        &mut status,
        4603,
        rows,
        &[
            "github_stack_merged_prefix_invalid",
            "github_stack_pr_base_drift",
            "github_stack_pr_base_drift",
        ],
    );
    let plans =
        crate::native_stack_rebase::closed_history_repair_plans(&repair_context(None), &status);
    assert_eq!(plans.len(), 1);
    let plan = &plans[0].1;
    assert_eq!(plan.members.len(), 1);
    assert_eq!(plan.members[0].pr, PrNumber(4603));
    assert_eq!(plan.members[0].old_base.name, "update-flake");
    assert_eq!(plan.members[0].parent_pr, None);
    assert_eq!(plan.members[0].parent_branch, "main");
}

#[test]
fn closed_history_rebuild_is_gated_by_policy_and_exact_facts() {
    let status = closed_history_status();
    let plans = |context: &AppContext, status: &StatusOutput| {
        crate::native_stack_rebase::closed_history_repair_plans(context, status).len()
    };
    assert_eq!(plans(&repair_context(None), &status), 1);

    // Owner-only policy, no reviewed opt-in, or a truncated inventory.
    let mut manual = repair_context(None);
    manual.config.stack_rollout.manual_drift_repair = true;
    let mut unreviewed = repair_context(None);
    unreviewed.config.stack_rollout.mutations_opt_in = false;
    assert_eq!(plans(&manual, &status), 0);
    assert_eq!(plans(&unreviewed, &status), 0);
    let mut truncated = status.clone();
    truncated.stack_backend.provider_stacks_truncated = true;
    assert_eq!(plans(&repair_context(None), &truncated), 0);

    // A parked caravan, an orphaned Stack, or a Stack no longer open.
    let mut parked = status.clone();
    parked.analysis.fleet.caravans[0].parked = true;
    assert_eq!(plans(&repair_context(None), &parked), 0);
    let mut orphaned = status.clone();
    orphaned.stack_backend.native_stacks[0].caravan_id = None;
    assert_eq!(plans(&repair_context(None), &orphaned), 0);
    let mut closed = status.clone();
    closed.stack_backend.native_stacks[0].stack.open = false;
    assert_eq!(plans(&repair_context(None), &closed), 0);

    // Unknown ancestry or any problem the rebuild cannot resolve.
    for code in [
        "github_stack_ancestry_unknown",
        "github_stack_base_drift",
        "github_stack_orphaned",
        "github_stack_terminal_member_not_merged",
    ] {
        let mut unknown = status.clone();
        unknown.stack_backend.native_stacks[0]
            .problems
            .push(problem(code));
        assert_eq!(plans(&repair_context(None), &unknown), 0, "{code}");
    }

    // An open row outside this caravan, a contradictory or unknown row state.
    let mut foreign = status.clone();
    foreign.stack_backend.native_stacks[0]
        .stack
        .pull_requests
        .push(row(99, "open", false, "foreign"));
    assert_eq!(plans(&repair_context(None), &foreign), 0);
    let mut reopened = status.clone();
    reopened.stack_backend.native_stacks[0].stack.pull_requests[3].merged_at =
        Some("2026-10-21T00:00:00Z".to_owned());
    assert_eq!(plans(&repair_context(None), &reopened), 0);
    let mut future = status.clone();
    future.stack_backend.native_stacks[0].stack.pull_requests[2].state = "future".to_owned();
    assert_eq!(plans(&repair_context(None), &future), 0);

    // A clean merged prefix ahead of a linear open suffix is automatic
    // recovery's or routine drift's, never this rebuild's.
    let mut supported = status.clone();
    supported.stack_backend.native_stacks[0]
        .stack
        .pull_requests
        .remove(2);
    supported.stack_backend.native_stacks[0].problems = vec![problem("github_stack_head_drift")];
    assert!(!crate::stack_recovery::closed_history_requires_rebuild(
        &supported.stack_backend.native_stacks[0]
    ));
    assert_eq!(plans(&repair_context(None), &supported), 0);

    // Merged history interleaved after the open suffix began is pruned too.
    let mut interleaved = status;
    let rows = &mut interleaved.stack_backend.native_stacks[0]
        .stack
        .pull_requests;
    rows.remove(2);
    rows.swap(1, 2);
    interleaved.stack_backend.native_stacks[0].problems = vec![
        problem("github_stack_state_order_invalid"),
        problem("github_stack_pr_missing"),
    ];
    assert_eq!(plans(&repair_context(None), &interleaved), 1);
}

/// An applied plan hash is never replayed; only changed provider truth yields
/// a new, pending plan.
#[test]
fn applied_closed_history_plan_is_never_replayed() {
    let directory = git_repository();
    let context = repair_context(Some(directory.path()));
    let status = closed_history_status();
    let pending = crate::native_stack_rebase::pending_sync_repair_plans(
        &context,
        &status,
        SyncRepairKind::ClosedHistory,
    )
    .unwrap();
    assert_eq!(pending.len(), 1);
    let plan = pending[0].1.clone();
    let key = crate::native_stack_rebase::receipt_key(&plan.plan_hash).unwrap();
    crate::stack_checkpoint::write(
        directory.path(),
        &key,
        &NativeStackRebaseOutput {
            plan: plan.clone(),
            mutated: true,
            physical: None,
            drift_clear: None,
            replacement: None,
            eviction: None,
            retargets: Vec::new(),
            fresh_ci_required: false,
            next: "rerun cara sync".to_owned(),
        },
    )
    .unwrap();
    for _ in 0..2 {
        assert!(
            crate::native_stack_rebase::pending_sync_repair_plans(
                &context,
                &status,
                SyncRepairKind::ClosedHistory,
            )
            .unwrap()
            .is_empty(),
            "the same generation is never rewritten twice"
        );
    }

    let mut moved = status;
    let child = moved.analysis.pull_requests.get_mut(&PrNumber(2)).unwrap();
    child.head.oid = CommitOid("c".repeat(40));
    moved.stack_backend.native_stacks[0].stack.pull_requests[4]
        .head
        .sha = CommitOid("c".repeat(40));
    let fresh = crate::native_stack_rebase::pending_sync_repair_plans(
        &context,
        &moved,
        SyncRepairKind::ClosedHistory,
    )
    .unwrap();
    assert_eq!(fresh.len(), 1);
    assert_ne!(fresh[0].1.plan_hash, plan.plan_hash);
}

/// The dry-run names exactly the sealed plan the tick applies, and writes
/// nothing.
#[test]
fn dry_run_previews_the_exact_closed_history_plan_sync_applies() {
    let directory = git_repository();
    let context = repair_context(Some(directory.path()));
    let status = closed_history_status();
    let input = SyncInput {
        all: true,
        dry_run: true,
        ..SyncInput::default()
    };
    let applied = crate::native_stack_rebase::pending_sync_repair_plans(
        &context,
        &status,
        SyncRepairKind::ClosedHistory,
    )
    .unwrap()
    .remove(0)
    .1;
    let preview = plan::native_repair_plan(&context, &status, &input, 0)
        .unwrap()
        .expect("a repairable Stack is previewed");
    assert!(!preview.mutated);
    assert_eq!(preview.provider_writes, 0);
    assert!(!preview.plan_hash.is_empty());
    assert_eq!(preview.selected_caravans, vec![PrNumber(1)]);
    let action = &preview.actions[0];
    assert_eq!(action.kind, "native_stack_repair");
    assert_eq!(action.state, SyncPlanActionState::WouldMutate);
    let target = action.target.as_ref().unwrap();
    assert_eq!(target["repair"], "closed_history");
    assert_eq!(target["plan_hash"], applied.plan_hash);
    assert_eq!(target["stack"], 4597);
    assert_eq!(target["pruned_closed_rows"], json!([10, 11, 12]));
    assert_eq!(target["members"], json!(applied.members));
    assert_eq!(
        preview.actions[1].state,
        SyncPlanActionState::DeferredUntilRediscovery
    );
    // Same inputs, same preview.
    let again = plan::native_repair_plan(&context, &status, &input, 0)
        .unwrap()
        .unwrap();
    assert_eq!(again.plan_hash, preview.plan_hash);

    let mut manual = context;
    manual.config.stack_rollout.manual_drift_repair = true;
    assert!(
        plan::native_repair_plan(&manual, &status, &input, 0)
            .unwrap()
            .is_none()
    );
}

fn eviction_output(status: &StatusOutput) -> NativeStackRebaseOutput {
    let plan =
        crate::native_stack_rebase::closed_history_repair_plans(&repair_context(None), status)
            .remove(0)
            .1;
    let member = &plan.members[0];
    let request = NativeStackRepairRequest {
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
        target: status.analysis.fleet.default_branch.clone(),
        target_pr: None,
        refusal_code: "rebase_conflict".to_owned(),
        refusal: "replay conflicts".to_owned(),
        conflicting_paths: vec!["flake.lock".to_owned(), "src/daemon.rs".to_owned()],
        evicted: vec![PrNumber(1), PrNumber(2)],
        suggested_commands: vec![
            "cara repair start --pr 1 --non-force --actor <actor> --reason \"x\"".to_owned(),
            "cara rejoin --pr 1".to_owned(),
        ],
    };
    NativeStackRebaseOutput {
        eviction: Some(NativeStackConflictEviction {
            evicted: request.evicted.clone(),
            reason: "rebase_conflict: queue-owned native Stack #4597 repair could not replay PR #1"
                .to_owned(),
            repair_request: request,
            receipt: OperationReceipt {
                operation_id: OperationId::new(),
                operation: "evict".to_owned(),
                completed_steps: Vec::new(),
                changed: true,
            },
            provider_receipts: Vec::new(),
        }),
        plan,
        mutated: true,
        physical: None,
        drift_clear: None,
        replacement: None,
        retargets: Vec::new(),
        fresh_ci_required: false,
        next: "rerun cara sync".to_owned(),
    }
}

/// A conflicting member is evicted with a typed repair request on the
/// existing `sync_failed` hook, and the freed slot admits the next candidate.
#[test]
fn conflicting_member_eviction_emits_repair_request_and_frees_the_slot() {
    let status = closed_history_status();
    let output = eviction_output(&status);
    let operation = OperationId::new();
    let events = native_drift_repair_events(&operation, &output, &status.analysis.fleet);
    assert_eq!(events.len(), 2);
    let evicted = &events[0];
    assert_eq!(evicted.kind, EventKind::Evicted);
    assert_eq!(evicted.prs, vec![PrNumber(1), PrNumber(2)]);
    assert_eq!(evicted.caravan_id, Some(PrNumber(1)));
    assert_eq!(
        evicted.metadata["provider_before"],
        json!(output.plan.provider_before),
        "raw closed history is journalled with the eviction"
    );
    let failed = &events[1];
    assert_eq!(failed.kind, EventKind::SyncFailed);
    assert_eq!(failed.operation_id, operation);
    assert_eq!(
        failed.metadata["error_code"],
        "native_stack_member_unreplayable"
    );
    assert_eq!(failed.metadata["queue_continues"], true);
    assert_eq!(failed.metadata["phase"], "native_stack_repair_eviction");
    let request = &failed.metadata["repair_request"];
    assert_eq!(request["pr"], 1);
    assert_eq!(request["refusal_code"], "rebase_conflict");
    assert_eq!(
        request["conflicting_paths"],
        json!(["flake.lock", "src/daemon.rs"])
    );
    assert_eq!(request["evicted"], json!([1, 2]));
    assert_eq!(request["plan_hash"], output.plan.plan_hash);
    assert!(
        request["suggested_commands"][0]
            .as_str()
            .unwrap()
            .starts_with("cara repair start --pr 1")
    );
    // The existing Cacophony hook reads head/base from metadata.pull_request.
    assert_eq!(
        failed.metadata["pull_request"]["head"]["oid"],
        json!(status.analysis.pull_requests[&PrNumber(1)].head.oid)
    );
    assert_eq!(failed.metadata["pull_request"]["base"]["name"], "main");
    for event in &events {
        assert!(serde_json::to_vec(event).unwrap().len() < 1024 * 1024);
    }

    // A prune/rebase without eviction emits nothing new.
    let mut quiet = output;
    quiet.eviction = None;
    assert!(native_drift_repair_events(&operation, &quiet, &status.analysis.fleet).is_empty());

    // The drifted caravan held the only slot; once its unreplayable members
    // are released the next candidate can form a caravan.
    let mut context = AppContext::default();
    context.config.sync.max_caravans = 1;
    assert!(caravan_fleet_capacity_refusal(&context, &status, PrNumber(9)).is_some());
    let mut pulls = status
        .analysis
        .pull_requests
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for pull in &mut pulls {
        pull.labels.remove("caravan");
        pull.labels.insert("caravan-evicted".to_owned());
    }
    let released = caravan_status(pulls, None, true);
    assert!(released.analysis.fleet.caravans.is_empty());
    assert!(caravan_fleet_capacity_refusal(&context, &released, PrNumber(9)).is_none());
}
