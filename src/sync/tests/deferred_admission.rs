use super::*;

const LOG: &str = include_str!("../../../tests/fixtures/deferred-admission-4014.log");
const RUN: u64 = 35_526_976_658;

pub(super) fn fixture() -> (
    PullRequestSnapshot,
    WorkflowFailureDiagnostics,
    HeadRunLineage,
) {
    let data: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/deferred-admission-4014.json"
    ))
    .unwrap();
    let mut candidate = pull_request(
        4014,
        "candidate",
        "main",
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    candidate.labels.clear();
    candidate.head.oid = CommitOid("70688328bc9305c9de10f7122070b446f7bc51d4".to_owned());
    candidate.base.oid = CommitOid("8a22915bde7c8eb1d1721f8b932b427fa47f08c1".to_owned());
    candidate.checks = data["jobs"]["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|job| {
            let state = match job["conclusion"].as_str().unwrap() {
                "failure" => CheckState::Failure,
                "success" => CheckState::Success,
                "skipped" => CheckState::Skipped,
                _ => unreachable!(),
            };
            let mut row = check(job["name"].as_str().unwrap(), state, Some(RUN));
            row.provider_kind = Some("CheckRun".to_owned());
            row.workflow_name = Some("CI".to_owned());
            row
        })
        .collect();
    let response =
        crate::ci::tests::deferred_fixture(&PullRequestPrecondition::from(&candidate), LOG);
    assert!(crate::ci::proven_deferred_job(&response.runs[0]).is_some());
    let lineage = HeadRunLineage {
        head_sha: candidate.head.oid.0.clone(),
        complete: true,
        check_suites: vec![CheckSuiteLineage {
            id: 96_192_279_727,
            head_sha: candidate.head.oid.0.clone(),
            status: "completed".to_owned(),
            conclusion: "failure".to_owned(),
            app_slug: "github-actions".to_owned(),
            rerequestable: true,
        }],
        workflow_runs: vec![WorkflowRunLineage {
            run_id: RUN,
            check_suite_id: 96_192_279_727,
            workflow_name: "CI".to_owned(),
            head_sha: candidate.head.oid.0.clone(),
            status: "completed".to_owned(),
            conclusion: "failure".to_owned(),
            event: "pull_request".to_owned(),
        }],
        ..HeadRunLineage::default()
    };
    (candidate, response, lineage)
}

pub(super) fn prove_deferred(provider: &FakeProvider, candidate: &PullRequestSnapshot, gate: &str) {
    let (_, mut response, mut lineage) = fixture();
    let d = &mut response.runs[0];
    d.run_id = 10;
    d.check_suite_id = 77;
    d.expected_pr = candidate.number;
    d.head_sha = candidate.head.oid.clone();
    d.expected_head_oid = candidate.head.oid.clone();
    d.expected_base_oid = candidate.base.oid.clone();
    d.failed_jobs[0].name = gate.to_owned();
    d.failed_jobs[0].deferred_admission = Some(crate::ci::DeferredAdmissionReceipt {
        pr: candidate.number,
        head_oid: candidate.head.oid.clone(),
        base_oid: candidate.base.oid.clone(),
    });
    lineage.head_sha = candidate.head.oid.0.clone();
    lineage.head_committed_at = Some(PUBLISHED_AT.to_owned());
    lineage.workflow_runs[0].run_id = 10;
    lineage.workflow_runs[0].check_suite_id = 77;
    lineage.workflow_runs[0].head_sha = candidate.head.oid.0.clone();
    lineage.check_suites[0].id = 77;
    lineage.check_suites[0].head_sha = candidate.head.oid.0.clone();
    provider
        .failed_runs
        .borrow_mut()
        .insert(candidate.number, vec![failed_run(10, candidate)]);
    provider
        .diagnostic_overrides
        .borrow_mut()
        .insert(candidate.number, response);
    provider.serve_lineage(candidate.number, lineage);
}

fn setup(
    candidate: &PullRequestSnapshot,
    diagnostics: WorkflowFailureDiagnostics,
    lineage: HeadRunLineage,
) -> FakeProvider {
    let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    provider
        .failed_runs
        .borrow_mut()
        .insert(candidate.number, vec![failed_run(RUN, candidate)]);
    provider
        .diagnostic_overrides
        .borrow_mut()
        .insert(candidate.number, diagnostics);
    provider.serve_lineage(candidate.number, lineage);
    provider.require_contexts(
        "main",
        &["cara-admission", "Check & Lint", "Fast Tests (unit)"],
    );
    provider
}

fn gate() -> crate::config::CiAdmissionGateConfig {
    crate::config::CiAdmissionGateConfig {
        mode: crate::config::CiAdmissionGateMode::CaravanLabel,
        context: "cara-admission".to_owned(),
        member_label: "caravan".to_owned(),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn production_deferred_admission_new_and_join_verify_membership_before_one_ci_start() {
    for join in [false, true] {
        let (candidate, diagnostics, lineage) = fixture();
        // Regression witness: the old canonical reduction fabricated twelve
        // failures from one deferred sentinel, including successful/skipped jobs.
        let old = ci_generation_evidence(&candidate.checks, &candidate.head.oid.0, Some(&lineage));
        assert_eq!(old.effective_checks.len(), 12);
        assert!(!checks_have_exact_deferred_gate(
            &gate(),
            &old.effective_checks
        ));
        let provider = setup(&candidate, diagnostics, lineage.clone());
        let mut pulls = vec![candidate.clone()];
        let mut tail = caravan_member(1, "tail", "main");
        tail.auto_merge = AutoMergeState::squash();
        tail.base.oid = candidate.base.oid.clone();
        if join {
            pulls.push(tail.clone());
        }
        let mut observed = status(pulls, Some(candidate.number), &clean);
        observed.analysis.fleet.default_branch.oid = candidate.base.oid.clone();
        let mut progress = SyncProgress::new(&observed, Vec::new(), u32::MAX);
        assert!(
            candidate_local_admission_refusal(
                &provider,
                &mut progress,
                &repository(),
                candidate.number,
                Some(&gate())
            )
            .unwrap()
            .is_none()
        );
        let evaluation = evaluate_auto_candidate_bounded(
            &observed,
            &candidate,
            &clean,
            None,
            Some(&gate().context),
        )
        .unwrap();
        let target = if join { Some(tail.number) } else { None };
        assert_eq!(
            evaluation.target,
            if join {
                AutoCandidateTarget::Join(tail.number)
            } else {
                AutoCandidateTarget::New
            },
            "{:?}",
            evaluation.reasons
        );
        let trigger =
            admission_gate_retrigger(&provider, &repository(), &candidate, &gate()).unwrap();
        assert!(provider.workflow_reruns.borrow().is_empty());
        let membership =
            crate::membership::tests::apply_deferred_fixture(observed, target, &gate().context);
        assert!(membership.pull_request.is_active_caravan_member());
        assert_eq!(
            membership.pull_request.base.name,
            if join { "tail" } else { "main" }
        );
        append_membership_progress(&mut progress, &membership);
        provider
            .pulls
            .borrow_mut()
            .insert(candidate.number, membership.pull_request.clone());
        perform_admission_gate_retrigger(
            &provider,
            &mut progress,
            &repository(),
            candidate.number,
            trigger,
        )
        .unwrap();
        assert_eq!(
            provider.workflow_reruns.borrow().as_slice(),
            [(candidate.number, RUN)]
        );
        assert!(provider.rerequests.borrow().is_empty());
        let mut active = lineage;
        active.workflow_runs[0].status = "in_progress".to_owned();
        provider.head_lineage.borrow_mut().remove(&candidate.number);
        provider.serve_lineage(candidate.number, active);
        let reuse =
            admission_gate_retrigger(&provider, &repository(), &membership.pull_request, &gate())
                .unwrap();
        assert_eq!(reuse, AdmissionGateRetrigger::ReuseRun { run_id: RUN });
        // Simulate losing the local response/progress after the provider starts
        // CI: a new tick must reuse authoritative running work without a write.
        let retry_status = status(
            vec![membership.pull_request.clone()],
            Some(candidate.number),
            &clean,
        );
        let mut retry_progress = SyncProgress::new(&retry_status, Vec::new(), 0);
        perform_admission_gate_retrigger(
            &provider,
            &mut retry_progress,
            &repository(),
            candidate.number,
            reuse,
        )
        .unwrap();
        assert_eq!(provider.workflow_reruns.borrow().len(), 1);
        assert!(!root_checks_passing(&membership.pull_request));
    }
}

#[test]
fn production_deferred_admission_is_unevaluated_not_source_failure() {
    let (candidate, diagnostics, lineage) = fixture();
    let provider = setup(&candidate, diagnostics, lineage);
    let observed = status(vec![candidate.clone()], Some(candidate.number), &clean);
    let mut progress = SyncProgress::new(&observed, Vec::new(), u32::MAX);
    let ci = progress
        .observe_ci(&provider, &repository(), candidate.number)
        .unwrap();
    assert_eq!(ci.disposition, CiDisposition::Waiting);
    assert_eq!(ci.effective_checks.len(), 12);
    assert!(
        ci.effective_checks
            .iter()
            .all(|check| check.state == CheckState::InProgress)
    );
    assert_eq!(
        ci.failure_diagnostics[0].classification,
        WorkflowFailureClass::DeferredAdmission
    );
    assert!(!root_checks_passing(&candidate));
    assert!(
        candidate_local_admission_refusal(
            &provider,
            &mut progress,
            &repository(),
            candidate.number,
            Some(&gate())
        )
        .unwrap()
        .is_none()
    );
    assert!(
        progress
            .deferred_admission_gates
            .contains(&candidate.number)
    );
}

#[test]
fn deferred_admission_negative_evidence_never_grants_admission() {
    for case in [
        "source",
        "other_workflow",
        "truncated_jobs",
        "truncated_runs",
        "truncated_steps",
        "missing_log",
        "stale_base",
        "stale_head",
        "cancelled",
        "partial_lineage",
        "duplicate_diagnostic",
        "duplicate_suite",
        "duplicate_run",
        "superseded",
        "unknown",
        "wrong_failure_step",
    ] {
        let (mut candidate, mut diagnostics, mut lineage) = fixture();
        match case {
            "source" => candidate.checks.push(check(
                "independent source test",
                CheckState::Failure,
                Some(RUN),
            )),
            "other_workflow" => candidate.checks.push(check(
                "independent workflow",
                CheckState::Failure,
                Some(RUN + 1),
            )),
            "truncated_jobs" => diagnostics.runs[0].jobs_truncated = true,
            "truncated_runs" => diagnostics.runs_truncated = true,
            "truncated_steps" => diagnostics.runs[0].failed_jobs[0].steps_truncated = true,
            "missing_log" => diagnostics.runs[0].failed_jobs[0].deferred_admission = None,
            "stale_base" => {
                diagnostics.runs[0].expected_base_oid = CommitOid("new-base".to_owned())
            }
            "stale_head" => diagnostics.runs[0].head_sha = CommitOid("old-head".to_owned()),
            "cancelled" => lineage.workflow_runs[0].conclusion = "cancelled".to_owned(),
            "partial_lineage" => lineage.complete = false,
            "duplicate_diagnostic" => diagnostics.runs.push(diagnostics.runs[0].clone()),
            "duplicate_suite" => lineage.check_suites.push(lineage.check_suites[0].clone()),
            "duplicate_run" => lineage.workflow_runs.push(lineage.workflow_runs[0].clone()),
            "superseded" => {
                lineage.workflow_runs[0].run_id += 1;
                lineage.workflow_runs[0].conclusion = "cancelled".to_owned();
            }
            "unknown" => {
                candidate
                    .checks
                    .push(check("unknown job", CheckState::Unknown, Some(RUN)))
            }
            "wrong_failure_step" => {
                diagnostics.runs[0].failed_jobs[0].failed_steps[0].name =
                    "Compile source".to_owned()
            }
            _ => unreachable!(),
        }
        let provider = setup(&candidate, diagnostics, lineage);
        let observed = status(vec![candidate.clone()], Some(candidate.number), &clean);
        let mut progress = SyncProgress::new(&observed, Vec::new(), u32::MAX);
        assert!(
            !matches!(
                candidate_local_admission_refusal(
                    &provider,
                    &mut progress,
                    &repository(),
                    candidate.number,
                    Some(&gate())
                ),
                Ok(None)
            ),
            "{case}"
        );
        assert!(
            !progress
                .deferred_admission_gates
                .contains(&candidate.number),
            "{case}"
        );
    }
}

#[test]
fn deferred_admission_old_terminal_skip_is_invalidated_without_a_push() {
    let (mut candidate, diagnostics, lineage) = fixture();
    candidate
        .labels
        .insert(AUTO_ADMISSION_SKIP_LABEL.to_owned());
    let provider = setup(&candidate, diagnostics, lineage);
    let observed = status(vec![candidate.clone()], Some(candidate.number), &clean);
    let mut context = AppContext::default();
    context.config.sync.actions.join_unlabelled_prs = true;
    context.config.sync.max_mutations_per_tick = 2; // cleanup only; admission is a later tick
    let dir = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    );
    context.repository_path = dir.path().to_path_buf();
    let receipt = AutoJoinSkipReceipt {
        schema_version: 1,
        repository: observed.repository.clone(),
        candidate_pr: candidate.number,
        candidate_head: candidate.head.clone(),
        candidate_base: candidate.base.clone(),
        default_branch: observed.analysis.fleet.default_branch.clone(),
        tested_tails: Vec::new(),
        config_fingerprint: auto_admission_config_fingerprint(&context),
        heuristic_version: "priority_fifo_greedy_v1".to_owned(),
        refusal_kind: AutoAdmissionRefusalKind::TerminalCi,
        candidate_ci: None,
        required_runs: None,
        compatibility_reasons: vec!["historical false terminal_ci".to_owned()],
        actor: "cara sync automatic admission".to_owned(),
        observed_unix_secs: 1,
        evidence_hash: String::new(),
    }
    .finalize_hash();
    let original = receipt.comment_body();
    provider
        .comments
        .borrow_mut()
        .insert(candidate.number, vec![original.clone()]);
    assert!(!skip_receipt_matches(&context, &observed, &receipt));
    let guard = context
        .acquire_writer_operation("deferred-regression")
        .unwrap();
    assert!(context.acquire_writer_operation("second-writer").is_err());
    let mut progress = SyncProgress::new(&observed, Vec::new(), u32::MAX);
    let stale = observed.clone();
    run_auto_admission_with_refresh(
        &context,
        observed,
        &provider,
        &mut progress,
        Instant::now() + Duration::from_secs(30),
        &crate::command::GithubRequestBudget::new(100),
        &guard,
        |_| Ok(stale.clone()),
    )
    .unwrap();
    let after = &provider.pulls.borrow()[&candidate.number];
    assert_eq!(after.head.oid, candidate.head.oid);
    assert!(!after.has_label(AUTO_ADMISSION_SKIP_LABEL));
    assert!(provider.comments.borrow()[&candidate.number].contains(&original));
    assert_eq!(
        provider
            .calls
            .borrow()
            .iter()
            .filter(|kind| **kind == MutationKind::RemoveLabel)
            .count(),
        1
    );
}

#[test]
fn deferred_admission_log_parser_requires_complete_actual_output() {
    let (candidate, _, _) = fixture();
    for log in [
        LOG.replace("/1024", "/99999"),
        LOG.replace("exit code 78.", "exit code 1."),
        LOG.replace(
            "ADMISSION_DECISION: deferred_unjoined",
            "ADMISSION_DECISION: run_member",
        ),
        LOG.replace("CACO_CI_PR_NUMBER: 4014", "CACO_CI_PR_NUMBER: 4015"),
    ] {
        let response =
            crate::ci::tests::deferred_fixture(&PullRequestPrecondition::from(&candidate), &log);
        assert!(crate::ci::proven_deferred_job(&response.runs[0]).is_none());
    }
}
