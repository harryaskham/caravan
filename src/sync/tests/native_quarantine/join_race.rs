//! bd-28ae57: bounded discovery and repair across a native membership race.

use super::*;
use crate::native_stack_rebase::{NativeStackRebaseOutput, SyncDriftRepair};
use crate::sync::native_repair::{NativeConvergence, native_prefix_with_rediscovery};
use std::cell::Cell;

fn joined_status() -> StatusOutput {
    let mut status = caravan_status(
        vec![
            caravan_member(1, "root", "main"),
            caravan_member(2, "child", "root"),
        ],
        Some(PrNumber(1)),
        true,
    );
    enable_native_backend(&mut status);
    quarantine(&mut status, &["native_stack_rebase_required"]);
    status
}

fn repaired(context: &AppContext, status: &StatusOutput) -> SyncDriftRepair {
    let plan = crate::native_stack_rebase::drift_repair_plans(context, status)
        .remove(0)
        .1;
    // Physical replay and provider-truth discovery are covered with a real
    // bare Git remote in membership::tests::native_join_repair. Here the repair
    // result isolates orchestration: do not land before fresh CI can run.
    let mut fresh = status.clone();
    fresh.stack_backend.problems.clear();
    fresh.stack_backend.quarantined_caravans.clear();
    fresh.stack_backend.native_stacks[0].problems.clear();
    fresh.stack_backend.native_stacks[0].consistency = crate::read::StackConsistency::Exact;
    SyncDriftRepair {
        output: NativeStackRebaseOutput {
            plan,
            mutated: true,
            physical: None,
            drift_clear: None,
            replacement: None,
            eviction: None,
            retargets: Vec::new(),
            fresh_ci_required: true,
            next: "fresh CI then sync".to_owned(),
        },
        status: fresh,
    }
}

#[test]
fn native_join_visible_at_routing_is_rediscovered_and_repaired_in_tick() {
    let fresh = joined_status();
    let mut stale = caravan_status(
        vec![caravan_member(1, "root", "main")],
        Some(PrNumber(1)),
        true,
    );
    enable_native_backend(&mut stale);
    let provider =
        FakeProvider::with_pull_requests(fresh.analysis.pull_requests.values().cloned().collect());
    *provider.native_stack_intersections.borrow_mut() = Some(vec![native_generation(
        &fresh,
        42,
        &[PrNumber(1), PrNumber(2)],
    )]);
    let (_directory, config, native) = github_native_fixture();
    let context = drift_repair_context(false, true);
    let reads = Cell::new(0);
    let attempts = Cell::new(0);
    let outcome = native_prefix_with_rediscovery(
        &mut stale,
        |status| {
            attempts.set(attempts.get() + 1);
            execute_native_stack_prefix_first(
                status,
                &provider,
                true,
                false,
                false,
                64,
                RequiredRunsPolicy::from_config(&config.sync),
                Some(native.clone()),
            )
        },
        || {
            reads.set(reads.get() + 1);
            Ok(fresh.clone())
        },
        |status| Ok(Some(repaired(&context, status))),
    )
    .unwrap();
    let NativeConvergence::Repaired(repair) = outcome else {
        panic!("the same tick repairs the new child")
    };
    assert_eq!(reads.get(), 1);
    assert_eq!(
        attempts.get(),
        1,
        "no landing attempt on rewritten, untested heads"
    );
    assert_eq!(repair.output.plan.members[0].pr, PrNumber(2));
    assert!(repair.output.fresh_ci_required);
    assert!(repair.status.stack_backend.quarantined_caravans.is_empty());
    assert!(
        provider.calls.borrow().is_empty(),
        "routing mismatch made no provider mutation"
    );
}

#[test]
fn native_join_discovery_retry_is_bounded_and_never_retries_partial_or_unknown_writes() {
    for (code, mutated, expected_reads, expected_attempts) in [
        ("github_stack_generation_drifted", json!(false), 1, 2),
        ("github_stack_generation_drifted", json!(true), 0, 1),
        ("github_stack_generation_drifted", Value::Null, 0, 1),
        ("github_stack_caravan_mapping_ambiguous", json!(false), 0, 1),
        ("github_stack_sync_incomplete", json!(false), 0, 1),
    ] {
        let mut status = joined_status();
        let fresh = status.clone();
        let reads = Cell::new(0);
        let attempts = Cell::new(0);
        let repairs = Cell::new(0);
        let error = native_prefix_with_rediscovery::<()>(
            &mut status,
            |_| {
                attempts.set(attempts.get() + 1);
                Err(AppError::structured(
                    ErrorCategory::Validation,
                    code,
                    "refused",
                    Some(json!({"mutated": mutated})),
                ))
            },
            || {
                reads.set(reads.get() + 1);
                Ok(fresh)
            },
            |_| {
                repairs.set(repairs.get() + 1);
                Ok(None)
            },
        )
        .err()
        .expect("unchanged refusal is not suppressed");
        assert_eq!(error.code(), code);
        assert_eq!(reads.get(), expected_reads);
        assert_eq!(repairs.get(), expected_reads);
        assert_eq!(attempts.get(), expected_attempts);
    }
}

#[test]
fn native_join_strict_prefix_repair_keeps_policy_and_identity_gates() {
    let mut status = joined_status();
    quarantine(&mut status, &["github_stack_member_order_drift"]);
    status.stack_backend.native_stacks[0]
        .stack
        .pull_requests
        .truncate(1);
    let context = drift_repair_context(false, true);
    let plan = crate::native_stack_rebase::drift_repair_plans(&context, &status)
        .remove(0)
        .1;
    assert!(plan.verify());
    assert_eq!(
        plan.members.iter().map(|row| row.pr).collect::<Vec<_>>(),
        vec![PrNumber(1), PrNumber(2)]
    );
    for blocked in [
        drift_repair_context(true, true),
        drift_repair_context(false, false),
    ] {
        assert!(crate::native_stack_rebase::drift_repair_plans(&blocked, &status).is_empty());
    }
    for mutation in [
        "parked",
        "unknown",
        "truncated",
        "foreign",
        "head_drift",
        "draft",
    ] {
        let mut refused = status.clone();
        match mutation {
            "parked" => refused.analysis.fleet.caravans[0].parked = true,
            "unknown" => refused.stack_backend.native_stacks[0].problems.push(
                crate::read::StackBackendProblem {
                    code: "github_stack_ancestry_unknown".to_owned(),
                    message: "unknown".to_owned(),
                },
            ),
            "truncated" => refused.stack_backend.provider_stacks_truncated = true,
            "foreign" => refused.stack_backend.native_stacks[0].stack.pull_requests[0].number = 99,
            "head_drift" => {
                refused.stack_backend.native_stacks[0].stack.pull_requests[0]
                    .head
                    .sha = CommitOid("f".repeat(40));
            }
            "draft" => {
                refused
                    .analysis
                    .pull_requests
                    .get_mut(&PrNumber(2))
                    .unwrap()
                    .draft = true;
            }
            _ => unreachable!(),
        }
        assert!(
            crate::native_stack_rebase::drift_repair_plans(&context, &refused).is_empty(),
            "{mutation}"
        );
    }
}

#[test]
fn native_join_repair_preserves_prior_tick_receipts_and_requires_fresh_ci() {
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
    let status = joined_status();
    let mut writer = context.acquire_writer_operation("sync-test").unwrap();
    let start = Instant::now();
    let mut output = native_drift_repair_output(
        &context,
        &SyncInput::default(),
        start,
        start + Duration::from_secs(60),
        Duration::ZERO,
        repaired(&context, &status),
        None,
        &mut writer,
    )
    .unwrap();
    let mut prior = SyncProgress::new(&status, vec![PrNumber(1)], 64);
    prior.steps.push(MutationStep {
        kind: MutationKind::SetBase,
        state: MutationStepState::Completed,
        pr: Some(PrNumber(2)),
        summary: "joined child behind root".to_owned(),
    });
    let event = hooks::event(
        EventKind::PrJoined,
        prior.operation_id.clone(),
        status.repository.clone(),
        Some(PrNumber(1)),
        vec![PrNumber(2)],
        None,
        None,
        BTreeMap::new(),
    );
    prior.events.push(event.clone());
    let prior_steps = prior.steps.clone();
    crate::sync::native_repair::retain_prior_progress(
        &mut output,
        prior,
        AutoAdmissionOutput::disabled(&context, true),
    );
    assert_eq!(
        output.receipt.completed_steps[..prior_steps.len()],
        prior_steps,
    );
    assert_eq!(output.events[0], event);
    assert_eq!(
        output.scheduler_status.wake_class,
        SchedulerWakeClass::RetryTick
    );
    assert!(output.root_merge.is_empty());
    assert!(output.status.stack_backend.quarantined_caravans.is_empty());
}
