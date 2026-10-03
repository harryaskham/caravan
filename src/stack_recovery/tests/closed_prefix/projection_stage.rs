use super::*;

fn eight_member_fixture() -> Fixture {
    let mut f = fixture(0, 2);
    for number in 104..=108 {
        let predecessor = f.provider.inner.pulls[&PrNumber(number - 1)].head.clone();
        let member = pull(number, predecessor);
        f.provider.inner.pulls.insert(member.number, member);
        f.caravans[0].members.push(PrNumber(number));
    }
    let root = f.provider.inner.pulls.get_mut(&PrNumber(101)).unwrap();
    root.merge_state_status = Some("UNSTABLE".to_owned());
    root.checks = vec![
        CheckSnapshot {
            name: "Caravan admission gate".to_owned(),
            state: CheckState::Failure,
            ..CheckSnapshot::default()
        },
        CheckSnapshot {
            name: "source tests".to_owned(),
            state: CheckState::Skipped,
            ..CheckSnapshot::default()
        },
    ];
    f
}

fn pending_add(f: &Fixture) -> NativeMembershipPlan {
    NativeMembershipPlan::Add {
        repository: repository(),
        operation_id: "original-membership".to_owned(),
        actor: "existing-actor".to_owned(),
        stack_number: f.provider.rows.number,
        expected_members: vec![PrNumber(101), PrNumber(102)],
        candidate: Box::new(f.provider.inner.pulls[&PrNumber(103)].clone()),
    }
}

#[test]
fn native_two_logical_eight_recovers_original_checkpoint_without_ci_qualification() {
    let directory = tempfile::tempdir().unwrap();
    let context = test_context(directory.path());
    assert!(!context.config.physical_branch_rewrites_enabled());
    let mut f = eight_member_fixture();
    let original = crate::stack_membership::persist_pending(directory.path(), &pending_add(&f))
        .unwrap()
        .unwrap();
    let source_before = f.provider.inner.pulls.clone();
    let result = auto_recover_from_facts(&context, &evidence(&f), &f.provider)
        .unwrap()
        .expect("the all-open incomplete prefix is a projection recovery");
    assert_eq!(
        result.plan.pending_membership_checkpoint_hash,
        Some(original.evidence_hash)
    );
    assert!(result.source_heads_unchanged);
    assert_eq!(f.provider.inner.pulls, source_before);
    let NativeMembershipPlan::RecoveryAdd { plan, accepted } = &result.plan.action else {
        panic!("must append to the same raw provider generation, not create or re-admit")
    };
    assert_eq!(plan.before, f.provider.rows);
    assert_eq!(plan.desired.entries.len(), 8);
    assert_eq!(accepted.entries.len(), 8);
    assert_eq!(
        &plan.desired.entries[..2],
        f.provider.rows.topology.entries.as_slice()
    );
    assert_eq!(f.provider.inner.creates.get(), 1);
    assert_eq!(
        result.plan.members[0].merge_state_status.as_deref(),
        Some("UNSTABLE")
    );
    assert_eq!(
        result.plan.members[0].authoritative_checks[0].state,
        CheckState::Failure
    );
    assert_eq!(
        result.plan.members[0].authoritative_checks[1].state,
        CheckState::Skipped
    );
    assert!(
        crate::stack_membership::load_pending(directory.path(), PrNumber(101))
            .unwrap()
            .is_none()
    );
    // Complete provider truth does not cause another append or ordinary join.
    f.provider.rows.topology.clone_from(&plan.desired);
    f.backend.native_stacks = vec![project(&f.provider.rows)];
    f.backend.native_stacks[0].consistency = StackConsistency::Exact;
    assert!(
        auto_recover_from_facts(&context, &evidence(&f), &f.provider)
            .unwrap()
            .is_none()
    );
    assert_eq!(f.provider.inner.creates.get(), 1);
}

#[test]
fn public_projection_preview_keeps_failed_and_skipped_checks_without_landing_grant() {
    let f = eight_member_fixture();
    let rows =
        crate::stack_recovery::closed_prefix::observe(&evidence(&f), &f.caravans[0], &f.provider)
            .unwrap()
            .unwrap();
    let mut current = evidence(&f);
    current.recovery_rows = Some(&rows);
    let (plan, observation) = build_plan(
        &config(),
        &current,
        PrNumber(101),
        "existing-actor",
        "projection only",
        None,
    )
    .expect("UNSTABLE solely from CI is not a conflicting projection");
    assert_eq!(
        observation,
        NativeStackRecoveryObservation::ExactPrefixPendingAdd
    );
    assert!(plan.verify());
    assert_eq!(plan.members[0].authoritative_checks.len(), 2);
    assert!(
        plan.members[0]
            .authoritative_checks
            .iter()
            .any(|check| check.state == CheckState::Failure)
    );
    assert!(
        plan.members[0]
            .authoritative_checks
            .iter()
            .any(|check| check.state == CheckState::Skipped)
    );
    assert_eq!(f.provider.inner.creates.get(), 0);
}

#[test]
fn pending_prefix_identity_or_stack_lease_drift_refuses_before_projection() {
    for changed_stack in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let context = test_context(directory.path());
        let mut f = eight_member_fixture();
        let mut action = pending_add(&f);
        if changed_stack {
            let NativeMembershipPlan::Add { stack_number, .. } = &mut action else {
                unreachable!()
            };
            *stack_number += 1;
        }
        let original = crate::stack_membership::persist_pending(directory.path(), &action)
            .unwrap()
            .unwrap();
        if !changed_stack {
            f.provider
                .inner
                .pulls
                .get_mut(&PrNumber(103))
                .unwrap()
                .head
                .oid = CommitOid("changed".to_owned());
            // Keep the current logical base chain internally linear, so the
            // original candidate lease, not an unrelated graph error, refuses.
            f.provider.inner.pulls.get_mut(&PrNumber(104)).unwrap().base =
                f.provider.inner.pulls[&PrNumber(103)].head.clone();
        }
        let error = auto_recover_from_facts(&context, &evidence(&f), &f.provider).unwrap_err();
        assert_eq!(error.code(), "github_stack_recovery_checkpoint_drifted");
        assert_eq!(f.provider.inner.creates.get(), 0);
        assert_eq!(
            crate::stack_membership::load_pending(directory.path(), PrNumber(101))
                .unwrap()
                .unwrap(),
            original
        );
    }
}

#[test]
fn lost_response_reuses_original_sealed_plan_after_clearance_and_check_churn() {
    let directory = tempfile::tempdir().unwrap();
    let context = test_context(directory.path());
    let mut f = eight_member_fixture();
    let pending = crate::stack_membership::persist_pending(directory.path(), &pending_add(&f))
        .unwrap()
        .unwrap();
    let original_rows = f.provider.rows.clone();
    let mut initial = evidence(&f);
    initial.recovery_rows = Some(&original_rows);
    let (saved, _) = build_plan(
        &config(),
        &initial,
        PrNumber(101),
        "existing-actor",
        "same intent",
        Some(&pending),
    )
    .unwrap();
    persist_reviewed_plan(&context, &saved).unwrap();
    // The native request completed and local clearance completed, but the
    // response was lost. Do not reconstruct an admission or another intent.
    f.provider.rows.topology.clone_from(&saved.desired);
    f.backend.native_stacks = vec![project(&f.provider.rows)];
    f.backend.native_stacks[0].consistency = StackConsistency::Exact;
    crate::stack_membership::clear_pending(directory.path(), PrNumber(101)).unwrap();
    let root = f.provider.inner.pulls.get_mut(&PrNumber(101)).unwrap();
    root.merge_state_status = Some("CLEAN".to_owned());
    for check in &mut root.checks {
        check.state = CheckState::Success;
    }
    let mut current = evidence(&f);
    current.recovery_rows = Some(&original_rows);
    let (fresh, observation) = build_plan(
        &config(),
        &current,
        PrNumber(101),
        "existing-actor",
        "same intent",
        None,
    )
    .unwrap();
    assert_eq!(
        observation,
        NativeStackRecoveryObservation::ExactAlreadySatisfied
    );
    assert_ne!(fresh.plan_hash, saved.plan_hash);
    let resumed = resume_reviewed_projection(fresh.clone(), Some(&saved), observation, true);
    assert_eq!(resumed, saved);
    assert_eq!(
        resumed.members[0].authoritative_checks[0].state,
        CheckState::Failure
    );
    // A changed source generation is never normalized as harmless CI churn.
    let mut changed = fresh;
    changed.desired.entries[0].head.oid = CommitOid("new-source".to_owned());
    let refused = resume_reviewed_projection(changed, Some(&saved), observation, true);
    assert_ne!(refused.plan_hash, saved.plan_hash);
    assert_eq!(f.provider.inner.creates.get(), 0);
}

#[test]
fn dirty_topology_remains_refused_even_with_a_deferred_admission_check() {
    let directory = tempfile::tempdir().unwrap();
    let context = test_context(directory.path());
    let mut f = eight_member_fixture();
    f.provider
        .inner
        .pulls
        .get_mut(&PrNumber(101))
        .unwrap()
        .merge_state_status = Some("DIRTY".to_owned());
    let error = auto_recover_from_facts(&context, &evidence(&f), &f.provider).unwrap_err();
    assert_eq!(error.code(), "github_stack_recovery_member_not_clean");
    assert_eq!(f.provider.inner.creates.get(), 0);
}
