use super::*;

#[test]
fn closed_reformation_lifecycle_handles_root_middle_tail_multiple_and_merged_history() {
    for closed in [vec![1], vec![2], vec![3], vec![1, 3], vec![1, 2, 3]] {
        for state in [PullRequestState::Closed, PullRequestState::Merged] {
            let pulls = (1..=3)
                .map(|number| {
                    let base = if number == 1 {
                        "main".to_owned()
                    } else {
                        format!("head-{}", number - 1)
                    };
                    let mut pr = pull_request(
                        number,
                        &format!("head-{number}"),
                        &base,
                        if closed.contains(&number) {
                            state
                        } else {
                            PullRequestState::Open
                        },
                        AutoMergeState::disabled(),
                    );
                    pr.labels.insert("keep-me".to_owned());
                    if closed.contains(&number) && state == PullRequestState::Merged {
                        pr.merged_at = Some(PUBLISHED_AT.to_owned());
                        pr.labels.insert(CLOSED_LABEL.to_owned());
                    }
                    pr
                })
                .collect::<Vec<_>>();
            let mut snapshot = status(pulls.clone(), None, &clean);
            enable_native_backend(&mut snapshot);
            snapshot.stack_backend.problems = vec![crate::read::StackBackendProblem {
                code: "github_stack_member_order_drift".to_owned(),
                message: "closed row blocked native convergence".to_owned(),
            }];
            assert!(require_native_stack_backend_healthy(&snapshot).is_err());
            assert!(closed::has_pending(&snapshot));
            let input = SyncInput {
                all: true,
                ..SyncInput::default()
            };
            let plan = closed::plan(snapshot.clone(), &input, 0, None).unwrap();
            assert_eq!(plan.actions.len(), closed.len());
            assert!(!plan.auto_admission.enabled);
            assert!(plan.physical_rebase_plans.is_empty());
            let provider = FakeProvider::with_pull_requests(pulls.clone());
            let output = reconcile_closed_lifecycle(&snapshot, &provider).unwrap();
            assert_eq!(output.transitions.len(), closed.len());
            assert_eq!(
                *provider.calls.borrow(),
                vec![MutationKind::SetLabels; closed.len()]
            );
            for original in &pulls {
                let actual = provider.pulls.borrow()[&original.number].clone();
                assert_eq!(actual.head, original.head);
                assert_eq!(actual.base, original.base);
                assert!(actual.has_label("keep-me"));
                if closed.contains(&original.number.0) {
                    assert!(!actual.is_active_caravan_member());
                    assert_eq!(
                        actual.has_label("caravan"),
                        state == PullRequestState::Merged
                    );
                } else {
                    assert_eq!(actual, *original);
                }
            }
            let fresh = status(
                provider.pulls.borrow().values().cloned().collect(),
                None,
                &clean,
            );
            assert!(!closed::has_pending(&fresh));
            assert!(
                !reconcile_closed_lifecycle(&fresh, &provider)
                    .unwrap()
                    .changed
            );
            assert_eq!(provider.calls.borrow().len(), closed.len());
        }
    }
}

#[test]
fn closed_reformation_partial_failure_keeps_completed_receipts_and_resumes_without_replay() {
    let first = pull_request(
        41,
        "first",
        "main",
        PullRequestState::Closed,
        AutoMergeState::disabled(),
    );
    let second = pull_request(
        42,
        "second",
        "main",
        PullRequestState::Closed,
        AutoMergeState::disabled(),
    );
    let snapshot = status(vec![first.clone(), second.clone()], None, &clean);
    let provider = FakeProvider::with_pull_requests(vec![first.clone(), second.clone()]);
    let mut reopened = second;
    reopened.state = PullRequestState::Open;
    provider
        .refetch_overrides
        .borrow_mut()
        .insert(reopened.number, VecDeque::from([reopened]));
    let error = reconcile_closed_lifecycle(&snapshot, &provider).unwrap_err();
    assert_eq!(error.code(), "closed_member_partial");
    let details = error.details().unwrap();
    assert_eq!(details["mutated"], true);
    assert_eq!(details["provider_receipts"].as_array().unwrap().len(), 1);
    assert_eq!(details["closed_lifecycle_transitions"][0]["pr"], 41);
    assert_eq!(provider.calls.borrow().len(), 1);
    // A later complete observation proves the second member closed again.
    let fresh = status(
        provider.pulls.borrow().values().cloned().collect(),
        None,
        &clean,
    );
    let output = reconcile_closed_lifecycle(&fresh, &provider).unwrap();
    assert_eq!(output.transitions.len(), 1);
    assert_eq!(output.transitions[0].pr, PrNumber(42));
    assert_eq!(provider.calls.borrow().len(), 2);
    assert_eq!(provider.pulls.borrow()[&first.number].head, first.head);
}

#[test]
fn closed_reformation_routes_lifecycle_before_native_mutation_and_health_gates() {
    let source = include_str!("../../sync.rs");
    let lifecycle = source
        .find("let closed_lifecycle = reconcile_closed_lifecycle(&status, &provider)?;")
        .unwrap();
    let landing = source
        .find("if reconcile_pending_native_stack_landing_checkpoints(context, &status, &provider)?")
        .unwrap();
    assert!(lifecycle < landing);
    let plan = include_str!("../plan.rs");
    assert!(
        plan.find("closed::has_pending(&status)").unwrap()
            < plan
                .find("require_native_stack_backend_healthy(&status)?")
                .unwrap()
    );
}
