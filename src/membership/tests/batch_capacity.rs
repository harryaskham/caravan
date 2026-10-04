//! bd-0405b1: immutable explicit joins share the configured batch fence.
use super::*;

fn fixture(
    members: u64,
    operation: MembershipOperation,
) -> (StatusOutput, FakeProvider, MembershipRequest, AppContext) {
    let mut chain = Vec::new();
    for number in 1..=members {
        chain.push(pull_request(
            number,
            &format!("member-{number}"),
            &if number == 1 {
                "main".to_owned()
            } else {
                format!("member-{}", number - 1)
            },
            &[ACTIVE_LABEL],
        ));
    }
    let candidate = pull_request(
        60,
        "candidate",
        "main",
        if operation == MembershipOperation::Rejoin {
            &[EVICTED_LABEL]
        } else {
            &[]
        },
    );
    let mut before = status(candidate.clone(), chain);
    before.stack_backend.configured = crate::config::StackType::Github;
    let mut identity = stale_native_identity(&candidate);
    identity.synthetic.as_mut().unwrap().parents[0] = branch("main").oid;
    identity.stale_head = false;
    identity.stale_base = false;
    identity.stale_reasons.clear();
    identity.freshness = crate::model::MergeCandidateFreshness::Fresh;
    before.merge_candidates = vec![identity];
    let provider =
        FakeProvider::with_pull_requests(before.analysis.pull_requests.values().cloned().collect());
    let request = MembershipRequest {
        expected_admission: None,
        operation,
        create_pr: false,
        tail_pr: Some(members),
        head_pr: None,
        reason: Some("immutable explicit membership batch fixture".to_owned()),
        priority_label: None,
        agent_priority_labels: Vec::new(),
    };
    let mut context = AppContext::default();
    context.config.stack_type = crate::config::StackType::Github;
    context.config.rebase_on_join = false;
    context.config.max_caravan_length = Some(8);
    (before, provider, request, context)
}

fn apply(
    before: StatusOutput,
    provider: &FakeProvider,
    request: MembershipRequest,
    context: &AppContext,
) -> Result<MembershipOutput, AppError> {
    execute_with_rebase_guard_and_config(
        before,
        &exact_native_clean,
        provider,
        request,
        None,
        false,
        Some(context),
        None,
    )
}

#[test]
fn immutable_native_explicit_join_and_rejoin_refuse_full_or_oversize_batches_before_writes() {
    for operation in [MembershipOperation::Join, MembershipOperation::Rejoin] {
        for members in [8, 9] {
            let (before, provider, request, context) = fixture(members, operation);
            let original = provider.pull_requests.borrow().clone();
            let error = apply(before, &provider, request, &context)
                .expect_err("the real membership boundary must refuse another row");
            assert_eq!(error.code(), "caravan_batch_capacity_exhausted");
            assert_eq!(error.details().unwrap()["caravan_members"], members);
            assert_eq!(error.details().unwrap()["mutated"], false);
            assert_eq!(*provider.pull_requests.borrow(), original);
            assert!(provider.effects.borrow().is_empty());
            assert!(provider.audits.borrow().is_empty());
        }
    }
}

#[test]
fn immutable_native_explicit_join_below_batch_bound_keeps_normal_membership_policy() {
    let (before, provider, request, context) = fixture(7, MembershipOperation::Join);
    let source = before.analysis.pull_requests[&PrNumber(60)].head.clone();
    let output = apply(before, &provider, request, &context)
        .expect("one remaining slot permits the usual guarded membership operation");
    assert!(output.pull_request.has_label(ACTIVE_LABEL));
    assert_eq!(output.pull_request.base.name, "member-7");
    assert_eq!(output.pull_request.head, source);
    assert!(!provider.effects.borrow().is_empty());
}

#[test]
fn immutable_legacy_join_without_batch_bound_keeps_existing_behavior() {
    let (mut before, provider, request, mut context) = fixture(8, MembershipOperation::Join);
    before.stack_backend.configured = crate::config::StackType::Caravan;
    context.config.stack_type = crate::config::StackType::Caravan;
    context.config.max_caravan_length = None;
    let output = apply(before, &provider, request, &context)
        .expect("an absent legacy bound must not become an implicit native limit");
    assert!(output.pull_request.has_label(ACTIVE_LABEL));
    assert_eq!(output.pull_request.base.name, "member-8");
}

#[test]
fn already_enrolled_retry_is_not_a_new_batch_row() {
    let (mut before, mut provider, mut request, context) = fixture(8, MembershipOperation::Join);
    let member = before.analysis.pull_requests[&PrNumber(8)].clone();
    let chain = before
        .analysis
        .pull_requests
        .values()
        .filter(|pull| pull.number != PrNumber(60))
        .cloned()
        .collect();
    before = status(member, chain);
    before.stack_backend.configured = crate::config::StackType::Github;
    provider.pull_requests = RefCell::new(before.analysis.pull_requests.clone());
    request.tail_pr = Some(7);
    let output = apply(before, &provider, request, &context)
        .expect("an exact enrolled retry preserves the existing full batch");
    assert!(output.pull_request.has_label(ACTIVE_LABEL));
    assert_eq!(output.pull_request.base.name, "member-7");
    assert!(provider.effects.borrow().is_empty());
}
