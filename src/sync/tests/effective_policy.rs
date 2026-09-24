//! bd-e81848: exercise the actual ordinary/native/parking/admission seams.
use super::*;
use crate::required_runs::RequiredCheck;

fn policy() -> RequiredContextsRead {
    RequiredContextsRead {
        branch: "main".into(),
        protected: true,
        complete: true,
        contexts: vec!["gate".into()],
        checks: vec![RequiredCheck {
            context: "gate".into(),
            app_id: Some(77),
        }],
    }
    .normalized()
}
fn app_check(pull: &PullRequestSnapshot, name: &str, app: u64, state: CheckState) -> CheckSnapshot {
    CheckSnapshot {
        app_id: Some(app),
        head_oid: Some(pull.head.oid.clone()),
        provider_kind: Some("CheckRun".into()),
        ..check(name, state, None)
    }
}
fn root() -> PullRequestSnapshot {
    let mut pull = caravan_member(1, "one", "main");
    pull.checks = vec![
        app_check(&pull, "gate", 77, CheckState::Success),
        app_check(&pull, "gate", 88, CheckState::Failure),
        app_check(&pull, "optional", 77, CheckState::Failure),
        app_check(&pull, "optional-pending", 88, CheckState::InProgress),
    ];
    pull
}
fn setup(pulls: Vec<PullRequestSnapshot>) -> (StatusOutput, FakeProvider) {
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider
        .required_contexts
        .borrow_mut()
        .insert("main".into(), policy());
    let mut status = caravan_status(pulls, Some(PrNumber(1)), true);
    status.analysis.required_policy = Some(policy());
    (status, provider)
}

#[test]
fn effective_policy_ordinary_merge_ignores_optional_checks_but_preserves_diagnostics() {
    let (status, provider) = setup(vec![root()]);
    let progress = execute(&status, &provider, false, false, false).unwrap();
    assert!(provider.calls.borrow().contains(&MutationKind::SquashMerge));
    assert_eq!(progress.ci[0].checks.len(), 4);
    assert_eq!(progress.ci[0].checks[2].state, CheckState::Failure);
    assert!(progress.required_runs.iter().all(|receipt| {
        receipt.assessment.required_policy.as_ref().unwrap().checks[0].app_id == Some(77)
    }));
}

#[test]
fn effective_policy_ordinary_merge_refuses_required_third_party_missing_unknown_and_stale() {
    for state in [
        CheckState::Failure,
        CheckState::InProgress,
        CheckState::Unknown,
        CheckState::Expected,
        CheckState::Cancelled,
    ] {
        let mut pull = root();
        pull.checks[0].state = state;
        let (status, provider) = setup(vec![pull]);
        let _result = execute(&status, &provider, false, false, false);
        assert!(
            !provider.calls.borrow().contains(&MutationKind::SquashMerge),
            "required state {state:?}"
        );
    }
    for bad_identity in [0, 1, 2] {
        let mut pull = root();
        match bad_identity {
            0 => pull.checks[0].app_id = Some(99),
            1 => pull.checks[0].head_oid = Some(CommitOid("stale-head".into())),
            _ => {
                pull.checks.remove(0);
            }
        }
        let (status, provider) = setup(vec![pull]);
        let _result = execute(&status, &provider, false, false, false);
        assert!(!provider.calls.borrow().contains(&MutationKind::SquashMerge));
    }
    let (status, provider) = setup(vec![root()]);
    provider.partial_contexts("main");
    let _result = execute(&status, &provider, false, false, false);
    assert!(!provider.calls.borrow().contains(&MutationKind::SquashMerge));
}

#[test]
fn effective_policy_ordinary_merge_fences_policy_change_after_audit() {
    let (status, provider) = setup(vec![root()]);
    let mut changed = policy();
    changed.checks[0].app_id = Some(88);
    // Tick observation, fresh root preflight, then post-audit pre-submit read.
    *provider.policy_overrides.borrow_mut() = VecDeque::from([policy(), policy(), changed]);
    let progress = execute(&status, &provider, false, false, false).unwrap();
    assert!(provider.calls.borrow().contains(&MutationKind::Comment));
    assert!(!provider.calls.borrow().contains(&MutationKind::SquashMerge));
    assert!(
        progress
            .steps
            .iter()
            .any(|step| step.summary.contains("policy changed"))
    );
}

#[test]
fn effective_policy_native_parent_policy_cannot_hide_landing_requirements() {
    let mut child = caravan_member(2, "two", "one");
    child.checks = vec![app_check(&child, "gate", 88, CheckState::Success)];
    let (status, provider) = setup(vec![root(), child]);
    provider
        .required_contexts
        .borrow_mut()
        .insert("one".into(), RequiredContextsRead::unprotected("one"));
    let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], 64);
    let required = progress
        .observe_required_runs(&provider, &status.repository, PrNumber(2))
        .unwrap();
    assert!(!matches!(
        required.status,
        RequiredRunsStatus::Satisfied | RequiredRunsStatus::NotRequired
    ));
    assert_eq!(required.required_policy.unwrap().branch, "main");
    assert!(
        provider
            .policy_reads
            .borrow()
            .iter()
            .all(|branch| branch == "main")
    );
}

fn native_checkpoint(status: &StatusOutput) -> crate::github::GitHubStackLandCheckpoint {
    let generation = native_generation(status, 42, &[PrNumber(1)]);
    let prefix = crate::github::GitHubStackReadyPrefix {
        selected: generation.topology.entries.clone(),
        stack: generation,
        first_blocked: None,
    };
    let plan = prefix
        .direct_squash_plan("effective-policy", "test")
        .unwrap();
    GitHubMutationAdapter::<crate::command::ProcessRunner>::native_stack_land_begin(
        &status.repository,
        &plan,
    )
}

#[test]
fn effective_policy_native_revalidates_live_policy_checks_and_generation() {
    let (status, provider) = setup(vec![root()]);
    let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], 64);
    let checkpoint = native_checkpoint(&status);
    assert!(
        progress
            .native_required_ready(&provider, &status, &checkpoint)
            .unwrap()
    );
    let mut changed = policy();
    changed.checks[0].app_id = Some(88);
    provider
        .required_contexts
        .borrow_mut()
        .insert("main".into(), changed);
    assert!(
        !progress
            .native_required_ready(&provider, &status, &checkpoint)
            .unwrap()
    );
    provider
        .required_contexts
        .borrow_mut()
        .insert("main".into(), policy());
    provider
        .pulls
        .borrow_mut()
        .get_mut(&PrNumber(1))
        .unwrap()
        .checks[0]
        .state = CheckState::InProgress;
    assert!(
        !progress
            .native_required_ready(&provider, &status, &checkpoint)
            .unwrap()
    );
    assert!(provider.calls.borrow().is_empty(), "readiness never writes");
}

#[test]
fn effective_policy_native_ready_prefix_reaches_lock_only_with_required_evidence() {
    let (mut status, provider) = setup(vec![root()]);
    let pull = root();
    status.merge_candidates = vec![crate::model::MergeCandidateIdentity {
        pr: pull.number,
        provider_updated_at: "2026-09-24T08:00:00Z".into(),
        observed_at: "2026-09-24T08:00:01Z".into(),
        base: pull.base.clone(),
        head: pull.head.clone(),
        synthetic: Some(crate::model::SyntheticMergeCandidate {
            git_ref: "refs/pull/1/merge".into(),
            oid: CommitOid("candidate".into()),
            tree_oid: CommitOid("tree".into()),
            parents: vec![pull.base.oid.clone(), pull.head.oid.clone()],
        }),
        auto_merge: crate::model::NativeAutoMergeState {
            enabled: false,
            merge_method: None,
            actor: None,
        },
        freshness: crate::model::MergeCandidateFreshness::Fresh,
        compared_base: Some(pull.base.clone()),
        stale_base: false,
        stale_head: false,
        stale_reasons: Vec::new(),
    }];
    let caravan = &status.analysis.fleet.caravans[0];
    *provider.native_stack.borrow_mut() = Some(native_generation(&status, 42, &[PrNumber(1)]));
    let (_directory, _config, native) = github_native_fixture();
    let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], 64);
    progress
        .verify_required_runs(&provider, &status.repository, caravan.id, PrNumber(1))
        .unwrap();
    // The fake deliberately has no native lock implementation. Reaching that
    // boundary proves optional failures no longer shorten the ready prefix.
    let error = progress
        .drain_native_stack(&provider, &status, caravan, 42, &native)
        .unwrap_err();
    assert_eq!(error.code(), "github_stack_sync_provider_unavailable");
    let checkpoint = crate::stack_checkpoint::load::<crate::github::GitHubStackLandCheckpoint>(
        &native.repository_path,
        "land-42",
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        checkpoint.phase,
        crate::github::GitHubStackLandPhase::Planned
    );
    assert!(provider.calls.borrow().is_empty());
}

#[test]
fn effective_policy_parking_ignores_optional_red_and_preserves_required_failure() {
    let (status, provider) = setup(vec![root()]);
    let mut context = AppContext::default();
    context.config.sync.terminal_red.action = crate::config::TerminalRedAction::Park;
    let result = reconcile_terminal_red_parking(&context, &status, &provider).unwrap();
    assert!(!result.changed && provider.calls.borrow().is_empty());
    let mut failed = root();
    failed.checks[0].state = CheckState::Failure;
    let (status, provider) = setup(vec![failed]);
    let result = reconcile_terminal_red_parking(&context, &status, &provider).unwrap();
    assert!(result.changed);
    assert!(provider.pulls.borrow()[&PrNumber(1)].has_label(PARKED_LABEL));
}

#[test]
fn effective_policy_admission_and_read_projection_ignore_only_optional_failures() {
    let mut candidate = root();
    candidate.labels.clear();
    let (mut status, provider) = setup(vec![candidate.clone()]);
    status.admission = read::resolve_admission(&status.analysis, &[]);
    assert_eq!(status.admission.next_candidate, Some(candidate.number));
    let mut progress = SyncProgress::new(&status, vec![], 64);
    assert!(
        candidate_local_admission_refusal(
            &provider,
            &mut progress,
            &status.repository,
            candidate.number,
            None
        )
        .unwrap()
        .is_none()
    );
    provider
        .required_contexts
        .borrow_mut()
        .insert("main".into(), RequiredContextsRead::partial("main"));
    let mut fresh = SyncProgress::new(&status, vec![], 64);
    let error = candidate_local_admission_refusal(
        &provider,
        &mut fresh,
        &status.repository,
        candidate.number,
        None,
    )
    .unwrap_err();
    assert_eq!(error.code(), "auto_admission_provider_state_unknown");
}
