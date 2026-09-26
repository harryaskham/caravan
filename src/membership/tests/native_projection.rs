//! bd-7f24b2: current synthetic parents do not refresh the recorded PR base.
//! Exercise real read/apply policy with isolated providers, never a live queue.
use super::*;
use crate::read::{AdmissionCompatibilityAuthorization, CandidateNextAction};

fn fixture(join: bool) -> (StatusOutput, FakeProvider, MembershipRequest) {
    let mut candidate = pull_request(60, "candidate", "main", &[]);
    candidate.base.oid = branch("old-main").oid;
    let others = if join {
        vec![pull_request(1, "tail", "main", &[ACTIVE_LABEL])]
    } else {
        Vec::new()
    };
    let mut before = status(candidate.clone(), others);
    before.stack_backend.configured = crate::config::StackType::Github;
    let mut identity = stale_native_identity(&candidate);
    identity.synthetic.as_mut().unwrap().parents[0] = branch("main").oid;
    identity.stale_reasons = vec!["recorded PR base is superseded by current main".to_owned()];
    before.merge_candidates = vec![identity];
    let provider =
        FakeProvider::with_pull_requests(before.analysis.pull_requests.values().cloned().collect());
    provider.branch_heads.borrow_mut().extend(
        before
            .analysis
            .pull_requests
            .values()
            .map(|pr| (pr.head.name.clone(), pr.head.oid.clone()))
            .chain([("main".to_owned(), branch("main").oid)]),
    );
    let request = MembershipRequest {
        expected_admission: None,
        operation: if join {
            MembershipOperation::Join
        } else {
            MembershipOperation::New
        },
        create_pr: false,
        tail_pr: join.then_some(1),
        head_pr: None,
        reason: Some("projection-only stale base".to_owned()),
        priority_label: None,
        agent_priority_labels: Vec::new(),
    };
    (before, provider, request)
}

#[test]
fn root_and_join_keep_stale_diagnostics_and_use_exact_proof_in_check_and_apply() {
    for join in [false, true] {
        let (before, provider, request) = fixture(join);
        let identity = before.merge_candidates[0].clone();
        let source = identity.head.clone();
        let target = branch(if join { "tail" } else { "main" });
        let checked = read::check_analysis(
            &before,
            &CheckInput {
                pr: Some(60),
                tail_pr: request.tail_pr,
                ..CheckInput::default()
            },
            &exact_native_clean,
        )
        .expect("current synthetic plus stale projection is an exact-Git admission");
        assert!(checked.eligible);
        assert_eq!(
            checked.next_action,
            if join {
                CandidateNextAction::Join
            } else {
                CandidateNextAction::New
            }
        );
        assert_eq!(checked.merge_candidate.as_ref(), Some(&identity));
        assert!(matches!(
            &checked.admission_compatibility_authorization,
            Some(AdmissionCompatibilityAuthorization::ExactGitProof { stale_identity, compatibility })
                if **stale_identity == identity && compatibility.target == target
        ));

        let output = execute(before, &exact_native_clean, &provider, request).unwrap();
        assert_eq!(
            output.pull_request.head, source,
            "immutable source is not rewritten"
        );
        assert_eq!(output.pull_request.base.name, target.name);
        assert!(output.pull_request.has_label(ACTIVE_LABEL));
        assert_eq!(
            output.admission_compatibility_authorization,
            checked.admission_compatibility_authorization
        );
        assert!(
            provider.audits.borrow()[0]
                .compatibility_evidence
                .starts_with("authority=exact_git_proof;")
        );
        assert!(
            !provider.effects.borrow().is_empty(),
            "the actual membership apply seam ran"
        );
    }
}

#[test]
fn projection_still_requires_complete_exact_current_target_proof() {
    for join in [false, true] {
        for defect in [
            "missing-proof",
            "partial-objects",
            "wrong-target",
            "conflict",
            "stale-head",
            "missing-lineage",
            "foreign-identity",
            "moved-default",
            "required-failure",
        ] {
            let (mut before, provider, request) = fixture(join);
            let identity = &mut before.merge_candidates[0];
            match defect {
                "stale-head" => {
                    identity.stale_head = true;
                    identity.freshness = crate::model::MergeCandidateFreshness::StaleHead;
                }
                "missing-lineage" => {
                    identity.synthetic = None;
                }
                "foreign-identity" => {
                    identity.head.repository.owner = "foreign".to_owned();
                }
                "moved-default" => {
                    identity.compared_base.as_mut().unwrap().oid = branch("moved").oid;
                }
                "required-failure" => {
                    before
                        .analysis
                        .pull_requests
                        .get_mut(&PrNumber(60))
                        .unwrap()
                        .checks
                        .push(crate::model::CheckSnapshot {
                            name: "build-test".to_owned(),
                            state: crate::model::CheckState::Failure,
                            ..crate::model::CheckSnapshot::default()
                        });
                }
                _ => {}
            }
            let checker = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
                let mut proof = exact_native_clean(candidate, target)?;
                match defect {
                    "missing-proof" => {
                        proof.diagnostic = None;
                    }
                    "partial-objects" => {
                        proof.diagnostic =
                            Some("objects_present=false shallow=true filter=blob:none".to_owned());
                    }
                    "wrong-target" => {
                        proof.target = branch("wrong-target");
                    }
                    "conflict" => {
                        proof.outcome = CompatibilityOutcome::Conflict;
                    }
                    _ => {}
                }
                Ok(proof)
            };
            assert!(
                execute(before, &checker, &provider, request).is_err(),
                "{defect}, join={join}"
            );
            assert!(
                provider.effects.borrow().is_empty(),
                "{defect} must refuse before writes"
            );
            assert!(provider.audits.borrow().is_empty());
        }
    }
}

#[test]
fn projection_proof_config_drift_is_fenced_before_apply() {
    for join in [false, true] {
        let (before, provider, request) = fixture(join);
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join(".caravan/config.yaml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let config = crate::config::CaravanConfig::parse(
            "version: 1\nmin_cara_version: '0.0.65'\nstack_type: github\nsync:\n  head_merge_actor: caravan\n",
        ).expect("valid native policy before the deliberate generation change");
        std::fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
        let context = AppContext {
            repository_path: directory.path().to_path_buf(),
            config_path: std::path::PathBuf::from(".caravan/config.yaml"),
            config_existed: true,
            config: config.clone(),
        };
        let mut changed = config;
        changed.force_merge = !changed.force_merge;
        let checker = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
            std::fs::write(&config_path, serde_yaml::to_string(&changed).unwrap()).unwrap();
            exact_native_clean(candidate, target)
        };
        let error = execute_with_rebase_guard_and_config(
            before,
            &checker,
            &provider,
            request,
            None,
            false,
            Some(&context),
            None,
        )
        .unwrap_err();
        assert_eq!(error.code(), "membership_config_generation_changed");
        assert!(provider.effects.borrow().is_empty());
        assert!(provider.audits.borrow().is_empty());
    }
}

#[test]
fn projection_proof_does_not_bypass_candidate_default_or_tail_races() {
    for join in [false, true] {
        for drift in ["source", "base", "labels", "default", "tail"] {
            if drift == "tail" && !join {
                continue;
            }
            let (before, provider, request) = fixture(join);
            let checker = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
                match drift {
                    "default" => {
                        provider
                            .branch_heads
                            .borrow_mut()
                            .insert("main".to_owned(), branch("moved").oid);
                    }
                    "tail" => {
                        provider
                            .pull_requests
                            .borrow_mut()
                            .get_mut(&PrNumber(1))
                            .unwrap()
                            .head
                            .oid = branch("moved").oid;
                    }
                    _ => {
                        let mut prs = provider.pull_requests.borrow_mut();
                        let pr = prs.get_mut(&PrNumber(60)).unwrap();
                        match drift {
                            "source" => {
                                pr.head.oid = branch("moved").oid;
                            }
                            "base" => {
                                pr.base.oid = branch("moved").oid;
                            }
                            "labels" => {
                                pr.labels.insert("caravan-parked".to_owned());
                            }
                            _ => unreachable!(),
                        }
                    }
                }
                exact_native_clean(candidate, target)
            };
            let error = execute(before, &checker, &provider, request).unwrap_err();
            let expected = match drift {
                "default" => "native_admission_default_generation_changed",
                "tail" => "join_root_moved_before_apply",
                _ => "native_admission_candidate_generation_changed",
            };
            assert_eq!(error.code(), expected, "{drift}, join={join}");
            assert!(provider.effects.borrow().is_empty());
            assert!(provider.audits.borrow().is_empty());
        }
    }
}
