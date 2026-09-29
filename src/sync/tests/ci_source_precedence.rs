//! Required source failures must veto infrastructure recovery (bd-cd04c2).
use super::*;
use crate::ci::{
    WorkflowJobFailureDiagnostic, WorkflowRunPullRequestAssociation, WorkflowStepFailureDiagnostic,
};

fn job(name: &str, conclusion: &str) -> WorkflowJobFailureDiagnostic {
    WorkflowJobFailureDiagnostic {
        job_id: 1,
        name: name.into(),
        status: "completed".into(),
        conclusion: conclusion.into(),
        url: "https://github.com/harryaskham/caravan/actions/runs/10/job/1".into(),
        runner_name: None,
        runner_labels: Vec::new(),
        failed_steps: if conclusion == "failure" {
            vec![WorkflowStepFailureDiagnostic {
                number: 3,
                name: "Compile and test".into(),
                status: "completed".into(),
                conclusion: "failure".into(),
            }]
        } else {
            Vec::new()
        },
        steps_truncated: false,
        selected_lineage: None,
        lineage_evidence_status: crate::ci::LineageEvidenceStatus::NotRequested,
        deferred_admission: None,
    }
}

fn fixture(runs: Vec<(u64, Vec<WorkflowJobFailureDiagnostic>)>) -> (StatusOutput, FakeProvider) {
    let mut pull = healthy_chain().remove(0);
    let mut diagnostics = WorkflowFailureDiagnostics {
        requested_run_ids: Vec::new(),
        runs: Vec::new(),
        runs_truncated: false,
    };
    let mut workflows = Vec::new();
    pull.checks.clear();
    for (run_id, mut jobs) in runs {
        let mut run = failed_run(run_id, &pull);
        run.workflow_name = format!("CI-{run_id}");
        for (index, job) in jobs.iter_mut().enumerate() {
            job.job_id = run_id * 100 + u64::try_from(index).unwrap();
            job.url = format!("{}/job/{}", run.url, job.job_id);
            let state = match job.conclusion.as_str() {
                "cancelled" | "canceled" => CheckState::Cancelled,
                "timed_out" => CheckState::TimedOut,
                _ => CheckState::Failure,
            };
            pull.checks.push(CheckSnapshot {
                app_id: Some(77),
                head_oid: Some(pull.head.oid.clone()),
                provider_kind: Some("CheckRun".into()),
                workflow_name: Some(run.workflow_name.clone()),
                details_url: Some(job.url.clone()),
                ..check(&job.name, state, Some(run_id))
            });
        }
        diagnostics.requested_run_ids.push(run_id);
        diagnostics.runs.push(WorkflowRunFailureDiagnostic {
            run_id,
            attempt: 1,
            workflow_id: run_id,
            check_suite_id: run_id,
            workflow_name: run.workflow_name.clone(),
            event: "pull_request".into(),
            status: "completed".into(),
            conclusion: "failure".into(),
            head_branch: pull.head.name.clone(),
            head_sha: pull.head.oid.clone(),
            expected_pr: pull.number,
            expected_head_oid: pull.head.oid.clone(),
            expected_base_oid: pull.base.oid.clone(),
            pull_requests: vec![WorkflowRunPullRequestAssociation {
                pr: pull.number,
                head_oid: Some(pull.head.oid.clone()),
                base_oid: Some(pull.base.oid.clone()),
            }],
            jobs_total: jobs.len(),
            jobs_truncated: false,
            failed_jobs: jobs,
        });
        workflows.push(run);
    }
    let provider = FakeProvider::with_pull_requests(vec![pull.clone()]);
    provider
        .failed_runs
        .borrow_mut()
        .insert(pull.number, workflows);
    provider
        .diagnostic_overrides
        .borrow_mut()
        .insert(pull.number, diagnostics);
    (status(vec![pull], Some(PrNumber(1)), &clean), provider)
}

fn failed_ci(status: &StatusOutput, provider: &FakeProvider) -> serde_json::Value {
    let error = execute(status, provider, false, true, false)
        .expect_err("source or unproved evidence cannot authorize recovery");
    assert_eq!(error.code(), "ci_failure");
    assert!(provider.calls.borrow().is_empty(), "no provider mutations");
    assert!(provider.workflow_reruns.borrow().is_empty());
    assert!(provider.rerequests.borrow().is_empty());
    let details = mcp_cli::StructuredError::details(&error).unwrap();
    let ci = &details["decision"]["evidence"]["ci"];
    assert_eq!(ci["rerunnable_run_ids"], json!([]));
    ci.clone()
}

fn change_diagnostics(
    provider: &FakeProvider,
    change: impl FnOnce(&mut WorkflowFailureDiagnostics),
) {
    change(
        provider
            .diagnostic_overrides
            .borrow_mut()
            .get_mut(&PrNumber(1))
            .unwrap(),
    );
}

#[test]
fn same_run_source_failure_vetoes_infrastructure_sibling() {
    for conclusion in [
        "cancelled",
        "canceled",
        "timed_out",
        "startup_failure",
        "action_required",
        "stale",
    ] {
        let (status, provider) = fixture(vec![(
            10,
            vec![job("compile", "failure"), job("prepare", conclusion)],
        )]);
        let ci = failed_ci(&status, &provider);
        assert_eq!(
            ci["failure_diagnostics"][0]["classification"],
            "source_or_test_failure"
        );
        assert_eq!(ci["failure_diagnostics"][0]["action"], "repair_source");
    }
}

#[test]
fn required_source_workflow_vetoes_sibling_infrastructure_rerun() {
    let (status, provider) = fixture(vec![
        (10, vec![job("prepare", "timed_out")]),
        (20, vec![job("compile", "failure")]),
    ]);
    let ci = failed_ci(&status, &provider);
    assert_eq!(ci["failure_diagnostics"].as_array().unwrap().len(), 2);
    assert_eq!(
        ci["failure_diagnostics"][1]["classification"],
        "source_or_test_failure"
    );
    // A new sync observation of the unchanged generation is still zero-write.
    assert_eq!(failed_ci(&status, &provider), ci);
}

#[test]
fn source_failure_is_not_hidden_by_a_cancelled_run_conclusion() {
    let (status, provider) = fixture(vec![(
        10,
        vec![job("compile", "failure"), job("prepare", "cancelled")],
    )]);
    change_diagnostics(&provider, |response| {
        response.runs[0].conclusion = "cancelled".into()
    });
    provider
        .failed_runs
        .borrow_mut()
        .get_mut(&PrNumber(1))
        .unwrap()[0]
        .conclusion = "cancelled".into();
    let ci = failed_ci(&status, &provider);
    assert_eq!(
        ci["failure_diagnostics"][0]["classification"],
        "source_or_test_failure"
    );
}

#[test]
fn pure_infrastructure_keeps_exact_opt_in_rerun_path() {
    for conclusion in ["cancelled", "timed_out", "startup_failure"] {
        let (status, provider) = fixture(vec![(10, vec![job("prepare", conclusion)])]);
        let error = execute(&status, &provider, false, false, false).unwrap_err();
        let details = mcp_cli::StructuredError::details(&error).unwrap();
        assert_eq!(
            details["decision"]["evidence"]["ci"]["rerunnable_run_ids"],
            json!([10])
        );
        assert!(
            provider.calls.borrow().is_empty(),
            "recovery requires opt-in"
        );

        let progress = execute(&status, &provider, false, true, false).unwrap();
        assert_eq!(*provider.calls.borrow(), vec![MutationKind::RerunChecks]);
        assert_eq!(progress.provider_receipts.len(), 1);
        assert_eq!(progress.ci[0].disposition, CiDisposition::Waiting);
        assert_eq!(
            provider.pulls.borrow()[&PrNumber(1)].checks[0].state,
            CheckState::Queued
        );
    }
}

#[test]
fn incomplete_evidence_vetoes_infrastructure_in_its_run_and_required_siblings() {
    for separate in [false, true] {
        for incomplete in ["jobs", "steps", "runs", "unknown_count"] {
            let mut runs = vec![(10, vec![job("prepare", "timed_out")])];
            if separate {
                runs.push((20, vec![job("other", "timed_out")]));
            }
            let (status, provider) = fixture(runs);
            change_diagnostics(&provider, |response| {
                let run = response.runs.last_mut().unwrap();
                match incomplete {
                    "jobs" => run.jobs_truncated = true,
                    "steps" => run.failed_jobs[0].steps_truncated = true,
                    "runs" => response.runs_truncated = true,
                    "unknown_count" => run.jobs_total = 0,
                    _ => unreachable!(),
                }
            });
            let ci = failed_ci(&status, &provider);
            assert_eq!(
                ci["failure_diagnostics"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()["classification"],
                "unknown"
            );
        }
    }
}

#[test]
fn source_precedence_does_not_override_generation_or_lineage_refusals() {
    for invalid in ["head", "base", "association", "lineage"] {
        let (status, provider) = fixture(vec![(
            10,
            vec![job("compile", "failure"), job("prepare", "timed_out")],
        )]);
        change_diagnostics(&provider, |response| {
            let run = &mut response.runs[0];
            match invalid {
                "head" => run.pull_requests[0].head_oid = Some(CommitOid("old-head".into())),
                "base" => run.pull_requests[0].base_oid = Some(CommitOid("old-base".into())),
                "association" => run.pull_requests.clear(),
                "lineage" => {
                    run.failed_jobs[0].failed_steps[0].name = "Verify selected ref lineage".into()
                }
                _ => unreachable!(),
            }
        });
        let ci = failed_ci(&status, &provider);
        assert_eq!(
            ci["failure_diagnostics"][0]["action"],
            "fresh_candidate_trigger"
        );
        assert_eq!(
            ci["failure_diagnostics"][0]["classification"],
            if invalid == "lineage" {
                "unknown"
            } else {
                "stale_generation"
            }
        );
    }
}

#[test]
fn optional_or_wrong_app_source_workflow_is_not_a_required_recovery_veto() {
    for name in ["advisory", "prepare"] {
        let (mut status, provider) = fixture(vec![
            (10, vec![job("prepare", "timed_out")]),
            (20, vec![job("advisory", "failure")]),
        ]);
        {
            let mut pulls = provider.pulls.borrow_mut();
            let check = &mut pulls.get_mut(&PrNumber(1)).unwrap().checks[1];
            check.name = name.into();
            check.app_id = Some(88);
        }
        let policy = RequiredContextsRead {
            branch: "main".into(),
            protected: true,
            complete: true,
            contexts: vec!["prepare".into()],
            checks: vec![crate::required_runs::RequiredCheck {
                context: "prepare".into(),
                app_id: Some(77),
            }],
        }
        .normalized();
        status.analysis.required_policy = Some(policy.clone());
        provider
            .required_contexts
            .borrow_mut()
            .insert("main".into(), policy);
        // The real provider diagnoses only selected required workflow IDs.
        change_diagnostics(&provider, |response| {
            response.requested_run_ids = vec![10];
            response.runs.retain(|run| run.run_id == 10);
        });
        let progress = execute(&status, &provider, false, true, false).unwrap();
        assert_eq!(*provider.calls.borrow(), vec![MutationKind::RerunChecks]);
        assert_eq!(progress.ci[0].disposition, CiDisposition::Waiting);
        assert_eq!(
            provider.pulls.borrow()[&PrNumber(1)].checks[1].state,
            CheckState::Failure
        );
    }
}

#[test]
fn proven_deferred_admission_remains_unevaluated_not_source_failure() {
    let (candidate, diagnostics, lineage) = deferred_admission::fixture();
    let run_id = diagnostics.runs[0].run_id;
    let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    provider
        .failed_runs
        .borrow_mut()
        .insert(candidate.number, vec![failed_run(run_id, &candidate)]);
    provider
        .diagnostic_overrides
        .borrow_mut()
        .insert(candidate.number, diagnostics);
    provider.serve_lineage(candidate.number, lineage);
    provider.require_contexts(
        "main",
        &["cara-admission", "Check & Lint", "Fast Tests (unit)"],
    );
    let observed = status(vec![candidate.clone()], Some(candidate.number), &clean);
    let mut progress = SyncProgress::new(&observed, vec![candidate.number], 0);
    let ci = progress
        .observe_ci(&provider, &repository(), candidate.number)
        .unwrap();
    assert_eq!(ci.disposition, CiDisposition::Waiting);
    assert!(ci.rerunnable_run_ids.is_empty());
    assert_eq!(
        ci.failure_diagnostics[0].classification,
        WorkflowFailureClass::DeferredAdmission
    );
    assert!(provider.calls.borrow().is_empty());
}
