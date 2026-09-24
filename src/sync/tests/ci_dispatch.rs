use super::*;
use crate::ci_dispatch::{
    DispatchDisposition,
    tests::{evidence, repository as test_repository},
};

#[test]
fn ci_dispatch_membership_and_active_actions_are_preserved_without_an_extra_start() {
    for attempt in [1, 2] {
        let mut pull = caravan_member(226, "feature", "main");
        let (policy, lineage) = evidence(&mut pull, attempt, "in_progress", "");
        let status = caravan_status(vec![pull.clone()], Some(pull.number), true);
        let provider = FakeProvider::with_pull_requests(vec![pull.clone()]);
        provider
            .required_contexts
            .borrow_mut()
            .insert("main".into(), policy);
        provider.serve_lineage(pull.number, lineage);
        let mut progress = SyncProgress::new(&status, vec![pull.number], 0); // reuse consumes no mutation budget
        let mut unjoined = pull.clone();
        unjoined.labels.remove("caravan");
        progress.provider_receipts.push(GitHubMutationReceipt {
            kind: MutationKind::AddLabel,
            before: Some(unjoined),
            after: pull.clone(),
            provider_output: None,
        });
        let directory = test_repository();
        dispatch_exact_ci_after_queue_mutations(
            directory.path(),
            &provider,
            &mut progress,
            &status,
        )
        .unwrap();
        assert!(provider.calls.borrow().is_empty());
        assert_eq!(progress.ci_generation_dispatches.len(), 1);
        assert_eq!(
            progress.ci_generation_dispatches[0].disposition,
            DispatchDisposition::Reused
        );
        assert_eq!(
            progress.ci_generation_dispatches[0]
                .execution
                .as_ref()
                .unwrap()
                .run_attempt,
            attempt
        );
        assert_eq!(provider.pulls.borrow()[&pull.number], pull);
    }
}

#[test]
fn ci_dispatch_respects_drafts_parking_and_inactive_membership() {
    for case in ["draft", "parked", "evicted", "closed"] {
        let mut pull = caravan_member(226, "feature", "main");
        let (policy, lineage) = evidence(&mut pull, 1, "completed", "failure");
        match case {
            "draft" => pull.draft = true,
            "parked" => {
                pull.labels.insert("caravan-parked".into());
            }
            "evicted" => {
                pull.labels.insert("caravan-evicted".into());
            }
            "closed" => pull.state = PullRequestState::Closed,
            _ => unreachable!(),
        }
        let status = caravan_status(vec![pull.clone()], Some(pull.number), true);
        let provider = FakeProvider::with_pull_requests(vec![pull.clone()]);
        provider
            .required_contexts
            .borrow_mut()
            .insert("main".into(), policy);
        provider.serve_lineage(pull.number, lineage);
        let mut progress = SyncProgress::new(&status, vec![pull.number], 10);
        progress.steps.push(MutationStep {
            kind: MutationKind::SetBase,
            state: MutationStepState::Completed,
            pr: Some(pull.number),
            summary: "earlier queue mutation".into(),
        });
        let directory = test_repository();
        dispatch_exact_ci_after_queue_mutations(
            directory.path(),
            &provider,
            &mut progress,
            &status,
        )
        .unwrap();
        assert!(provider.calls.borrow().is_empty(), "{case}");
        assert!(provider.lineage_reads.borrow().is_empty(), "{case}");
    }
}

#[test]
fn ci_dispatch_lost_response_retains_membership_receipt_and_restart_does_not_replay() {
    let mut pull = caravan_member(227, "feature", "main");
    let (policy, lineage) = evidence(&mut pull, 1, "completed", "failure");
    let mut unjoined = pull.clone();
    unjoined.labels.remove("caravan");
    let provider = FakeProvider::with_pull_requests(vec![unjoined.clone()]);
    let membership = provider
        .add_label(
            &repository(),
            &PullRequestPrecondition::from(&unjoined),
            "caravan",
        )
        .unwrap();
    provider
        .required_contexts
        .borrow_mut()
        .insert("main".into(), policy);
    provider.serve_lineage(pull.number, lineage.clone());
    *provider.ci_start_response_loss.borrow_mut() = true;
    let status = caravan_status(vec![pull.clone()], Some(pull.number), true);
    let directory = test_repository();
    let mut first = SyncProgress::new(&status, vec![pull.number], 10);
    first.record(membership.clone(), "membership accepted");
    let error =
        dispatch_exact_ci_after_queue_mutations(directory.path(), &provider, &mut first, &status)
            .unwrap_err();
    assert_eq!(error.code(), "fake_ci_response_loss");
    assert!(
        error
            .details()
            .unwrap()
            .get("provider_receipts")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|receipt| receipt.get("kind") == Some(&serde_json::json!(MutationKind::AddLabel)))
    );
    let mut restarted = SyncProgress::new(&status, vec![pull.number], 10);
    restarted.record(membership, "retained membership receipt");
    let error = dispatch_exact_ci_after_queue_mutations(
        directory.path(),
        &provider,
        &mut restarted,
        &status,
    )
    .unwrap_err();
    assert_eq!(error.code(), "ci_dispatch_indeterminate");
    let mut current = lineage;
    current.workflow_runs[0]
        .execution
        .as_mut()
        .unwrap()
        .run_attempt = 2;
    current.workflow_runs[0].status = "in_progress".into();
    current.workflow_runs[0].conclusion.clear();
    provider.serve_lineage(pull.number, current);
    // The fake serves its retained old snapshot once before the replacement.
    // Stale readback must stay fenced, not authorize a duplicate request.
    assert_eq!(
        dispatch_exact_ci_after_queue_mutations(
            directory.path(),
            &provider,
            &mut restarted,
            &status
        )
        .unwrap_err()
        .code(),
        "ci_dispatch_indeterminate"
    );
    dispatch_exact_ci_after_queue_mutations(directory.path(), &provider, &mut restarted, &status)
        .unwrap();
    assert_eq!(
        restarted.ci_generation_dispatches[0].disposition,
        DispatchDisposition::SuccessorObserved
    );
    assert_eq!(
        *provider.calls.borrow(),
        vec![MutationKind::AddLabel, MutationKind::RerunChecks]
    );
    assert_eq!(provider.workflow_reruns.borrow().len(), 1);
    assert!(
        provider.rerequests.borrow().is_empty(),
        "no Cursor suite request or suite fallback"
    );
    assert!(provider.pulls.borrow()[&pull.number].is_active_caravan_member());
}
