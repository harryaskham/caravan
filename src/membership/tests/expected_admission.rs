//! Exercise the real shared read policy, writer state machine and nested receipt.
use super::*;
use crate::expected_admission::ExpectedAdmission;

fn fixture() -> (StatusOutput, FakeProvider, ExpectedAdmission) {
    let mut candidate = pull_request(42, "source/ref", "main", &[]);
    candidate.head.oid = CommitOid("a".repeat(40));
    candidate.base.oid = CommitOid("b".repeat(40));
    let mut snapshot = status(candidate.clone(), vec![]);
    snapshot.analysis.fleet.default_branch.oid = candidate.base.oid.clone();
    let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    provider.branch_heads.borrow_mut().extend([
        ("main".to_owned(), candidate.base.oid.clone()),
        (candidate.head.name.clone(), candidate.head.oid.clone()),
    ]);
    provider.generation_relations.borrow_mut().insert(
        (candidate.base.oid.clone(), candidate.head.oid.clone()),
        crate::generation::CommitRelation::Ahead,
    );
    let binding = ExpectedAdmission {
        schema_version: 1,
        repository: repository().slug(),
        pr: 42,
        pr_url: candidate.url.clone(),
        head_ref: candidate.head.name,
        head_oid: "A".repeat(40),
        base_ref: candidate.base.name,
        base_oid: "B".repeat(40),
        default_ref: "main".to_owned(),
        default_oid: "B".repeat(40),
        unjoined: true,
    };
    (snapshot, provider, binding)
}

fn request(binding: ExpectedAdmission) -> MembershipRequest {
    MembershipRequest {
        expected_admission: Some(binding),
        operation: MembershipOperation::New,
        create_pr: false,
        tail_pr: None,
        head_pr: None,
        reason: None,
        priority_label: None,
        agent_priority_labels: vec![],
    }
}

#[test]
fn expected_admission_actual_wire_check_and_nested_receipt_preserve_oid_case() {
    let (snapshot, provider, binding) = fixture();
    let wire = serde_json::to_value(&binding).unwrap();
    let parsed: ExpectedAdmission = wire.to_string().parse().unwrap();
    let check = read::check_analysis(
        &snapshot,
        &CheckInput {
            pr: Some(42),
            expected_admission: Some(parsed.clone()),
            ..CheckInput::default()
        },
        &clean,
    )
    .unwrap();
    let checked = serde_json::to_value(check).unwrap();
    assert_eq!(checked["expected_admission"], wire);
    assert_eq!(checked["expected_admission_verified"], true);
    assert_eq!(checked["next_action"], "new");
    let mut output = execute(snapshot.clone(), &clean, &provider, request(parsed)).unwrap();
    output.join_receipt = Some(
        build_join_receipt(
            &AppContext::default(),
            &repository(),
            &snapshot,
            JoinReceiptEvidence {
                expected_admission: Some(&binding),
                immutable_ancestry_verified: true,
                predecessor: Some(JoinPredecessorReceipt {
                    pr: PrNumber(0),
                    branch: "main".to_owned(),
                    head_oid: CommitOid("b".repeat(40)),
                }),
                candidate_source_head_oid: Some(CommitOid("a".repeat(40))),
                source: None,
                default_branch_oid: CommitOid("b".repeat(40)),
                rebase_receipt: None,
            },
            &output,
        )
        .unwrap(),
    );
    let data = serde_json::to_value(output).unwrap();
    assert!(data.get("expected_admission").is_none());
    let receipt = &data["join_receipt"];
    assert_eq!(receipt["expected_admission"], wire);
    assert_eq!(receipt["expected_admission_verified"], true);
    assert_eq!(receipt["candidate_pr"], 42);
    assert_eq!(receipt["candidate_source_head_oid"], "a".repeat(40));
    assert_eq!(receipt["ancestry_verified"], true);
    assert_eq!(receipt["membership_durable"], true);
    assert_eq!(receipt["force_intent"], "none");
    assert_eq!(receipt["result"]["base_oid"], "b".repeat(40));
    assert_eq!(
        serde_json::from_value::<JoinForceIntent>(json!("absent")).unwrap(),
        JoinForceIntent::Absent
    );
}

#[test]
fn expected_admission_join_retains_original_lease_through_own_retarget_and_nested_receipt() {
    let (original, _, mut binding) = fixture();
    let candidate = original.analysis.pull_requests[&PrNumber(42)].clone();
    let mut root = pull_request(1, "tail", "main", &[ACTIVE_LABEL]);
    root.head.oid = CommitOid("c".repeat(40));
    root.base.oid = CommitOid("b".repeat(40));
    let mut snapshot = status(candidate.clone(), vec![root.clone()]);
    snapshot.analysis.fleet.default_branch.oid = root.base.oid.clone();
    let provider = FakeProvider::with_pull_requests(vec![candidate.clone(), root.clone()]);
    provider.branch_heads.borrow_mut().extend([
        ("main".to_owned(), root.base.oid.clone()),
        ("tail".to_owned(), root.head.oid.clone()),
        (candidate.head.name.clone(), candidate.head.oid.clone()),
    ]);
    provider.generation_relations.borrow_mut().insert(
        (root.head.oid.clone(), candidate.head.oid.clone()),
        crate::generation::CommitRelation::Ahead,
    );
    binding.base_ref = "main".to_owned();
    let check = read::check_analysis(
        &snapshot,
        &CheckInput {
            pr: Some(42),
            expected_admission: Some(binding.clone()),
            ..CheckInput::default()
        },
        &clean,
    )
    .unwrap();
    assert_eq!(check.next_action, crate::read::CandidateNextAction::Join);
    assert_eq!(check.target_pr, Some(root.number));
    let mut input = request(binding.clone());
    input.operation = MembershipOperation::Join;
    let mut output = execute(snapshot.clone(), &clean, &provider, input).unwrap();
    assert_eq!(provider.effects.borrow()[0], MutationKind::SetBase);
    assert_eq!(output.pull_request.base.name, "tail");
    output.join_receipt = Some(
        build_join_receipt(
            &AppContext::default(),
            &repository(),
            &snapshot,
            JoinReceiptEvidence {
                expected_admission: Some(&binding),
                immutable_ancestry_verified: true,
                predecessor: Some(JoinPredecessorReceipt {
                    pr: root.number,
                    branch: root.head.name,
                    head_oid: root.head.oid,
                }),
                candidate_source_head_oid: Some(candidate.head.oid),
                source: None,
                default_branch_oid: root.base.oid,
                rebase_receipt: None,
            },
            &output,
        )
        .unwrap(),
    );
    let data = serde_json::to_value(output).unwrap();
    assert_eq!(
        data["join_receipt"]["expected_admission"],
        serde_json::to_value(&binding).unwrap()
    );
    assert_eq!(data["join_receipt"]["expected_admission_verified"], true);
    assert_eq!(data["join_receipt"]["ancestry_verified"], true);
    assert_eq!(data["join_receipt"]["membership_durable"], true);
    assert_eq!(data["join_receipt"]["result"]["base_ref"], "tail");
    assert_eq!(data["join_receipt"]["result"]["base_oid"], "c".repeat(40));
}

#[test]
fn expected_admission_race_after_transaction_preflight_still_stops_first_write() {
    let (snapshot, provider, binding) = fixture();
    *provider.drift_on_identity_read.borrow_mut() = Some(2);
    let error = execute(snapshot, &clean, &provider, request(binding)).unwrap_err();
    assert_eq!(provider.admission_identity_reads.get(), 2);
    assert_eq!(error.code(), "expected_admission_drift");
    assert_eq!(error.details().unwrap()["provider_mutation"], "none");
    assert!(provider.effects.borrow().is_empty());
}

#[test]
fn expected_admission_malformed_or_stale_wire_refuses_read_and_write_without_effects() {
    let cases = [
        ("schema_version", json!(2)),
        ("repository", json!("other/repo")),
        ("pr", json!(43)),
        ("pr_url", json!("https://github.com/other/repo/pull/42")),
        ("head_ref", json!("other")),
        ("head_oid", json!("c".repeat(40))),
        ("base_ref", json!("other")),
        ("base_oid", json!("c".repeat(40))),
        ("default_ref", json!("other")),
        ("default_oid", json!("c".repeat(40))),
        ("unjoined", json!(false)),
        ("head_oid", json!("short")),
    ];
    for (field, value) in cases {
        let (snapshot, provider, binding) = fixture();
        let mut wire = serde_json::to_value(binding).unwrap();
        wire[field] = value;
        let binding: ExpectedAdmission = serde_json::from_value(wire).unwrap();
        assert!(
            read::check_analysis(
                &snapshot,
                &CheckInput {
                    pr: Some(42),
                    expected_admission: Some(binding.clone()),
                    ..CheckInput::default()
                },
                &clean
            )
            .is_err(),
            "{field}"
        );
        assert!(
            execute(snapshot, &clean, &provider, request(binding)).is_err(),
            "{field}"
        );
        assert!(provider.effects.borrow().is_empty(), "{field}");
        assert!(provider.audits.borrow().is_empty(), "{field}");
    }
    let (_, _, binding) = fixture();
    let mut wire = serde_json::to_value(binding).unwrap();
    wire["unrecognized"] = json!(true);
    assert!(serde_json::from_value::<ExpectedAdmission>(wire.clone()).is_err());
    wire.as_object_mut().unwrap().remove("unrecognized");
    for field in [
        "head_oid",
        "base_oid",
        "default_oid",
        "repository",
        "unjoined",
    ] {
        let mut missing = wire.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<ExpectedAdmission>(missing).is_err());
    }
}

#[test]
fn expected_admission_check_to_writer_source_base_default_and_membership_races_refuse() {
    for race in [
        "head",
        "base",
        "default",
        "default_ref",
        "repository",
        "unavailable",
        "joined",
        "force",
        "closed",
        "url",
        "native",
    ] {
        let (mut snapshot, provider, binding) = fixture();
        read::check_analysis(
            &snapshot,
            &CheckInput {
                pr: Some(42),
                expected_admission: Some(binding.clone()),
                ..CheckInput::default()
            },
            &clean,
        )
        .unwrap();
        match race {
            "default" => {
                provider
                    .branch_heads
                    .borrow_mut()
                    .insert("main".to_owned(), CommitOid("c".repeat(40)));
            }
            "default_ref" => {
                *provider.admission_identity.borrow_mut() =
                    Some((repository(), "other".to_owned()));
            }
            "repository" => {
                *provider.admission_identity.borrow_mut() = Some((
                    RepositoryId {
                        owner: "other".to_owned(),
                        name: "repo".to_owned(),
                    },
                    "main".to_owned(),
                ));
            }
            "unavailable" => *provider.admission_identity.borrow_mut() = None,
            "native" => {
                snapshot.stack_backend.configured = crate::config::StackType::Github;
                *provider.native_unjoined.borrow_mut() = false;
            }
            _ => {
                let mut prs = provider.pull_requests.borrow_mut();
                let candidate = prs.get_mut(&PrNumber(42)).unwrap();
                match race {
                    "head" => candidate.head.oid = CommitOid("c".repeat(40)),
                    "base" => candidate.base.oid = CommitOid("c".repeat(40)),
                    "joined" => {
                        candidate.labels.insert(ACTIVE_LABEL.to_owned());
                    }
                    "force" => {
                        candidate.labels.insert(FORCE_LABEL.to_owned());
                    }
                    "closed" => candidate.state = PullRequestState::Closed,
                    "url" => candidate.url.push_str("/other"),
                    _ => unreachable!(),
                }
            }
        }
        assert!(
            execute(snapshot, &clean, &provider, request(binding)).is_err(),
            "{race}"
        );
        assert!(provider.effects.borrow().is_empty(), "{race}");
    }
}

#[test]
fn expected_admission_partial_and_response_loss_never_roll_back_or_claim_zero_effects() {
    for response_lost in [false, true] {
        let (snapshot, provider, binding) = fixture();
        if response_lost {
            *provider.fail_after_effect.borrow_mut() = Some(MutationKind::AddLabel);
        } else {
            *provider.drift_default_after.borrow_mut() = Some(MutationKind::AddLabel);
        }
        let error = execute(
            snapshot.clone(),
            &clean,
            &provider,
            request(binding.clone()),
        )
        .unwrap_err();
        assert_eq!(error.code(), "expected_admission_partial");
        let details = error.details().unwrap();
        assert_eq!(details["provider_mutation"], "possible");
        assert_eq!(details["resumable"], false);
        assert_ne!(details["mutated"], false);
        assert!(provider.pull_requests.borrow()[&PrNumber(42)].has_label(ACTIVE_LABEL));
        assert_eq!(*provider.effects.borrow(), vec![MutationKind::AddLabel]);
        // Even a repeated old request cannot admit the already joined generation.
        assert!(execute(snapshot, &clean, &provider, request(binding)).is_err());
        assert_eq!(provider.effects.borrow().len(), 1);
    }
}

#[test]
fn expected_admission_provider_receipt_cannot_smuggle_an_unrequested_source_successor() {
    let (snapshot, provider, binding) = fixture();
    *provider.drift_source_after.borrow_mut() = Some(MutationKind::AddLabel);
    let error = execute(snapshot, &clean, &provider, request(binding)).unwrap_err();
    assert_eq!(error.code(), "expected_admission_partial");
    assert_eq!(error.details().unwrap()["provider_mutation"], "possible");
    assert_eq!(*provider.effects.borrow(), vec![MutationKind::AddLabel]);
    assert!(provider.audits.borrow().is_empty());
}

#[test]
fn expected_admission_unproved_ancestry_and_unsupported_modes_are_zero_write() {
    let (snapshot, provider, binding) = fixture();
    provider.generation_relations.borrow_mut().clear();
    assert_eq!(
        execute(
            snapshot.clone(),
            &clean,
            &provider,
            request(binding.clone())
        )
        .unwrap_err()
        .code(),
        "expected_admission_ancestry_unproved"
    );
    for operation in [MembershipOperation::Renew, MembershipOperation::Rejoin] {
        let mut input = request(binding.clone());
        input.operation = operation;
        assert_eq!(
            execute(snapshot.clone(), &clean, &provider, input)
                .unwrap_err()
                .code(),
            "expected_admission_operation_unsupported"
        );
    }
    let mut input = request(binding.clone());
    input.create_pr = true;
    assert!(execute(snapshot.clone(), &clean, &provider, input).is_err());
    assert!(
        read::check_analysis(
            &snapshot,
            &CheckInput {
                expected_admission: Some(binding),
                ..CheckInput::default()
            },
            &clean
        )
        .is_err()
    );
    assert!(provider.effects.borrow().is_empty());
}

#[test]
fn expected_admission_only_own_exact_rewrite_can_advance_source_binding() {
    let (mut snapshot, provider, binding) = fixture();
    let mut rewrite = crate::physical_rebase::RebaseReceipt {
        pr: PrNumber(42),
        branch: binding.head_ref.clone(),
        old_head_oid: CommitOid("a".repeat(40)),
        new_head_oid: CommitOid("d".repeat(40)),
        old_base_oid: CommitOid("b".repeat(40)),
        new_base_branch: "main".to_owned(),
        new_base_oid: CommitOid("b".repeat(40)),
        new_tree_oid: CommitOid("e".repeat(40)),
        commit_count: 1,
        merge_topology: None,
        squash_reconciliation: None,
        ci_trigger_workflows: vec![],
        lease: "exact-source-lease".to_owned(),
        already_satisfied: false,
        rewrite_reason: crate::physical_rebase::BranchRewriteReason::Unspecified,
    };
    snapshot
        .analysis
        .pull_requests
        .get_mut(&PrNumber(42))
        .unwrap()
        .head
        .oid = rewrite.new_head_oid.clone();
    assert!(binding.verify_snapshot(&snapshot, None).is_err());
    binding.verify_snapshot(&snapshot, Some(&rewrite)).unwrap();
    rewrite.old_head_oid = CommitOid("f".repeat(40));
    assert!(binding.verify_snapshot(&snapshot, Some(&rewrite)).is_err());
    let error = attach_rebase_receipt(
        crate::expected_admission::refusal(
            "expected_admission_drift",
            "default moved after own push",
        ),
        Some(&rewrite),
    );
    assert_eq!(error.details().unwrap()["provider_mutation"], "possible");
    assert_eq!(error.details().unwrap()["mutated"], true);
    assert!(provider.effects.borrow().is_empty());
}

#[test]
fn expected_admission_cli_and_mcp_share_the_typed_object_without_attestation_inputs() {
    use clap::Parser;
    #[derive(Parser)]
    struct Check {
        #[command(flatten)]
        input: CheckInput,
    }
    #[derive(Parser)]
    struct New {
        #[command(flatten)]
        input: CreateInput,
    }
    #[derive(Parser)]
    struct Join {
        #[command(flatten)]
        input: JoinInput,
    }
    let (_, _, binding) = fixture();
    let wire = serde_json::to_string(&binding).unwrap();
    let args = ["cara", "--pr", "42", "--expected-admission", wire.as_str()];
    assert_eq!(
        Check::try_parse_from(args)
            .unwrap()
            .input
            .expected_admission,
        Some(binding.clone())
    );
    assert_eq!(
        New::try_parse_from(args).unwrap().input.expected_admission,
        Some(binding.clone())
    );
    assert_eq!(
        Join::try_parse_from(args).unwrap().input.expected_admission,
        Some(binding.clone())
    );
    assert!(Check::try_parse_from(["cara", "--expected-admission", &wire]).is_err());
    let mcp: JoinInput =
        serde_json::from_value(json!({"pr":42,"expected_admission":binding})).unwrap();
    assert_eq!(mcp.expected_admission, Some(binding));
}
