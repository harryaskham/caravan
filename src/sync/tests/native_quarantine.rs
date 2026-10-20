//! bd-4d4621: one stuck native Stack quarantines only its own caravan.
//!
//! On 2026-10-12 the sole active Cacophony caravan (#4444 -> #4445) failed
//! every 20-minute tick for 4.5 hours on `github_stack_recovery_*`, retrying
//! silently while three independent ready PRs waited on `max_caravans`.

use super::*;

fn drifted_stack(status: &StatusOutput, codes: &[&str]) -> crate::read::NativeStackStatus {
    let generation = native_generation(status, 42, &[PrNumber(1), PrNumber(2)]);
    crate::read::NativeStackStatus {
        stack: crate::github::GitHubStackSnapshot {
            id: generation.id,
            number: generation.number,
            node_id: generation.node_id,
            open: generation.open,
            created_at: generation.created_at,
            base: crate::github::GitHubStackBase {
                ref_name: generation.topology.base.name.clone(),
            },
            pull_requests: generation
                .topology
                .entries
                .iter()
                .map(|entry| crate::github::GitHubStackPullRequest {
                    number: entry.pr.0,
                    state: "open".to_owned(),
                    draft: entry.draft,
                    merged_at: entry.merged_at.clone(),
                    head: crate::github::GitHubStackPullRequestHead {
                        ref_name: entry.head.name.clone(),
                        sha: entry.head.oid.clone(),
                    },
                })
                .collect(),
        },
        caravan_id: Some(PrNumber(1)),
        consistency: crate::read::StackConsistency::Drifted,
        ancestry: Vec::new(),
        problems: codes
            .iter()
            .map(|code| crate::read::StackBackendProblem {
                code: (*code).to_owned(),
                message: "Stack #42 PR #2 does not contain predecessor PR #1".to_owned(),
            })
            .collect(),
    }
}

fn quarantine(status: &mut StatusOutput, codes: &[&str]) {
    let native = drifted_stack(status, codes);
    status.stack_backend.problems = native.problems.clone();
    if crate::read::native_stack_quarantinable(&native) {
        status.stack_backend.quarantined_caravans = vec![crate::read::QuarantinedCaravan {
            caravan_id: PrNumber(1),
            code: codes[0].to_owned(),
            recovery: "cara native-stack rebase-preview --stack 42".to_owned(),
        }];
    }
    status.stack_backend.native_stacks = vec![native];
}

#[test]
fn diverged_stack_quarantines_only_its_caravan_and_independent_root_lands() {
    for codes in [
        &["native_stack_rebase_required"][..],
        &[
            "native_stack_rebase_required",
            "github_stack_member_order_drift",
        ][..],
        &["github_stack_head_drift"][..],
    ] {
        let pulls = vec![
            caravan_member(1, "root", "main"),
            caravan_member(2, "child", "root"),
            caravan_member(4, "independent", "main"),
        ];
        let mut status = caravan_status(pulls.clone(), Some(PrNumber(1)), true);
        enable_native_backend(&mut status);
        quarantine(&mut status, codes);
        require_native_stack_backend_healthy(&status)
            .expect("caravan-local drift no longer fails the whole backend");
        let provider = FakeProvider::with_pull_requests(pulls);
        let (_directory, config, native) = github_native_fixture();
        let progress = execute_bounded_with_native(
            &status,
            &provider,
            true,
            false,
            false,
            64,
            &BTreeMap::new(),
            RequiredRunsPolicy::from_config(&config.sync),
            Some(native),
        )
        .expect("one quarantined Stack never stops independent caravans");
        assert_eq!(
            progress.synchronized_caravans,
            vec![PrNumber(4)],
            "{codes:?}"
        );
        assert_eq!(progress.root_merge[0].pr, PrNumber(4), "{codes:?}");
        // The quarantined caravan's source heads are untouched.
        assert_eq!(
            provider.pulls.borrow()[&PrNumber(2)].head.oid,
            status.analysis.pull_requests[&PrNumber(2)].head.oid
        );
    }
}

#[test]
fn provider_unknown_ancestry_still_fails_closed() {
    let pulls = vec![
        caravan_member(1, "root", "main"),
        caravan_member(2, "child", "root"),
    ];
    let mut status = caravan_status(pulls, Some(PrNumber(1)), true);
    enable_native_backend(&mut status);
    quarantine(&mut status, &["github_stack_ancestry_unknown"]);
    assert!(status.stack_backend.quarantined_caravans.is_empty());
    let error = require_native_stack_backend_healthy(&status)
        .expect_err("unknown provider evidence is not caravan-local drift");
    assert_eq!(error.code(), "github_stack_backend_unhealthy");
}

#[test]
fn quarantined_sole_caravan_releases_max_caravans_capacity() {
    let pulls = vec![
        caravan_member(1, "root", "main"),
        caravan_member(2, "child", "root"),
    ];
    let mut status = caravan_status(pulls, Some(PrNumber(1)), true);
    enable_native_backend(&mut status);
    let mut context = AppContext::default();
    context.config.sync.max_caravans = 1;
    let refused = caravan_fleet_capacity_refusal(&context, &status, PrNumber(9))
        .expect("a healthy sole caravan fills max_caravans=1");
    assert_eq!(refused.active_caravan_ids, vec![PrNumber(1)]);

    quarantine(&mut status, &["native_stack_rebase_required"]);
    assert!(
        caravan_fleet_capacity_refusal(&context, &status, PrNumber(9)).is_none(),
        "an independent root may form a caravan while #1 awaits owner repair"
    );
}

#[test]
fn quarantine_escalates_with_the_exact_recovery_command() {
    let pulls = vec![
        caravan_member(1, "root", "main"),
        caravan_member(2, "child", "root"),
    ];
    let mut status = caravan_status(pulls, Some(PrNumber(1)), true);
    enable_native_backend(&mut status);
    quarantine(&mut status, &["native_stack_rebase_required"]);
    let scheduler = successful_scheduler_status(&status, &[], &[], false, &[], &[]);
    assert_eq!(
        scheduler.disposition,
        SchedulerDisposition::ExternalDecision
    );
    assert_eq!(scheduler.wake_class, SchedulerWakeClass::ExternalDecision);
    assert_eq!(scheduler.quarantined_caravans.len(), 1);
    assert!(
        scheduler.quarantined_caravans[0]
            .recovery
            .contains("rebase-preview")
    );

    // Refusals that still abort a tick escalate instead of retrying forever.
    for code in [
        "github_stack_recovery_source_rewrite_required",
        "github_stack_recovery_backend_unhealthy",
        "github_stack_auto_recovery_ambiguous",
    ] {
        let failure = scheduler_failure_status(&AppError::validation(code, "refused"));
        assert_eq!(
            failure.wake_class,
            SchedulerWakeClass::ExternalDecision,
            "{code}"
        );
        assert!(!failure.retryable, "{code}");
    }
}

#[test]
fn zero_write_recovery_refusal_is_contained_only_for_its_quarantined_caravan() {
    let pulls = vec![
        caravan_member(1, "root", "main"),
        caravan_member(2, "child", "root"),
    ];
    let mut status = caravan_status(pulls, Some(PrNumber(1)), true);
    enable_native_backend(&mut status);
    quarantine(&mut status, &["github_stack_member_order_drift"]);
    let refusal = |details: Value| {
        AppError::structured(
            ErrorCategory::Validation,
            "github_stack_recovery_provider_refused",
            "github_stack_recovery_source_rewrite_required: PR #2 does not provably contain predecessor #1",
            Some(details),
        )
    };
    assert!(contained_recovery_refusal(
        &status,
        &refusal(json!({"mutated": false, "mutation_outcome": "none", "recovery_caravan_id": 1}))
    ));
    assert!(!contained_recovery_refusal(
        &status,
        &refusal(
            json!({"mutated": null, "mutation_outcome": "indeterminate", "recovery_caravan_id": 1})
        )
    ));
    assert!(!contained_recovery_refusal(
        &status,
        &refusal(json!({"mutated": false, "recovery_caravan_id": 7}))
    ));
    assert!(!contained_recovery_refusal(
        &status,
        &refusal(json!({"mutated": false}))
    ));
}

#[test]
fn status_derives_quarantine_and_recovery_command_from_provider_truth() {
    let pulls = vec![
        caravan_member(1, "root", "main"),
        caravan_member(2, "child", "root"),
    ];
    let status = caravan_status(pulls, Some(PrNumber(1)), true);
    let diverged = crate::read::quarantined_caravan(&drifted_stack(
        &status,
        &["native_stack_rebase_required"],
    ))
    .expect("ancestry divergence is caravan-local");
    assert_eq!(diverged.caravan_id, PrNumber(1));
    assert_eq!(diverged.code, "native_stack_rebase_required");
    assert!(diverged.recovery.contains("rebase-preview --stack 42"));
    let order = crate::read::quarantined_caravan(&drifted_stack(
        &status,
        &["github_stack_member_order_drift"],
    ))
    .unwrap();
    assert!(order.recovery.contains("recovery-preview --root 1"));
    assert!(
        crate::read::quarantined_caravan(&drifted_stack(
            &status,
            &[
                "native_stack_rebase_required",
                "github_stack_inventory_truncated"
            ],
        ))
        .is_none(),
        "any non-local problem keeps the whole backend fail-closed"
    );
    let mut closed = drifted_stack(&status, &["native_stack_rebase_required"]);
    closed.stack.open = false;
    assert!(crate::read::quarantined_caravan(&closed).is_none());
}
