//! Retrospective two-job admission refresh shape, not a live incident replay.
//! A successful observer is an old observation, not proof of current admission.
use super::*;
use crate::required_runs::{HeadRunLineage, RequiredCheck, RequiredContextsRead};

const GATE: &str = "Caravan admission gate";
const APP: u64 = 15_368;

fn admission_fixture() -> (Value, Value, Value, model::PullRequestSnapshot, CiExecution) {
    let (mut pull, mut suite, mut run, _, _) = fixture();
    let head = "a".repeat(40);
    let base = "b".repeat(40);
    pull["headRefOid"] = json!(head);
    pull["baseRefOid"] = json!(base);
    pull["updatedAt"] = json!("2026-09-29T08:41:37Z");
    pull["statusCheckRollup"] = json!([
        {"__typename":"CheckRun", "name":GATE, "workflowName":"Admission readiness",
         "status":"COMPLETED", "conclusion":"FAILURE", "appId":APP,
         "checkSuiteId":4242, "headOid":head,
         "detailsUrl":"https://github.com/acme/widgets/actions/runs/99/job/2"},
        {"__typename":"CheckRun", "name":"Observe membership", "workflowName":"Admission readiness",
         "status":"COMPLETED", "conclusion":"SUCCESS", "appId":APP,
         "checkSuiteId":4242, "headOid":head,
         "startedAt":"2026-09-29T08:40:00Z", "completedAt":"2026-09-29T08:41:04Z",
         "detailsUrl":"https://github.com/acme/widgets/actions/runs/99/job/1"},
        {"__typename":"CheckRun", "name":"Compile", "workflowName":"Source CI",
         "status":"COMPLETED", "conclusion":"SUCCESS", "appId":APP,
         "checkSuiteId":5252, "headOid":head,
         "detailsUrl":"https://github.com/acme/widgets/actions/runs/100/job/3"}
    ]);
    suite["head_sha"] = json!(head);
    suite["app"]["id"] = json!(APP);
    run["name"] = json!("Admission readiness");
    run["head_sha"] = json!(head);
    run["run_attempt"] = json!(2);
    run["pull_requests"][0]["head"]["sha"] = json!(head);
    run["pull_requests"][0]["base"]["sha"] = json!(base);
    let snapshot = serde_json::from_value::<PullRequestJson>(pull.clone())
        .unwrap()
        .into_snapshot(&repository())
        .unwrap();
    let mut source_run = run.clone();
    source_run["id"] = json!(100);
    source_run["workflow_id"] = json!(124);
    source_run["check_suite_id"] = json!(5252);
    source_run["name"] = json!("Source CI");
    source_run["conclusion"] = json!("success");
    let mut source_suite = suite.clone();
    source_suite["id"] = json!(5252);
    source_suite["conclusion"] = json!("success");
    let lineage = HeadRunLineage {
        head_sha: head,
        head_committed_at: Some("2026-09-29T08:00:00Z".into()),
        complete: true,
        check_suites: [suite.clone(), source_suite]
            .into_iter()
            .map(|value| {
                serde_json::from_value::<CheckSuiteJson>(value)
                    .unwrap()
                    .into()
            })
            .collect(),
        workflow_runs: [run.clone(), source_run]
            .into_iter()
            .map(|value| {
                serde_json::from_value::<HeadWorkflowRunJson>(value)
                    .unwrap()
                    .into()
            })
            .collect(),
    };
    let policy = RequiredContextsRead {
        branch: "main".into(),
        protected: true,
        complete: true,
        contexts: vec![],
        checks: [GATE, "Compile"]
            .map(|context| RequiredCheck {
                context: context.into(),
                app_id: Some(APP),
            })
            .into(),
    }
    .normalized();
    let mut selected =
        crate::ci_dispatch::select(&snapshot, &policy, Some(GATE), &lineage).unwrap();
    assert_eq!(
        selected.len(),
        1,
        "do not select the independent source suite for an admission refresh"
    );
    (pull, suite, run, snapshot, selected.remove(0))
}

#[test]
fn admission_refresh_selects_full_workflow_not_only_failed_gate() {
    let (pull, suite, run, snapshot, execution) = admission_fixture();
    let observer = snapshot
        .checks
        .iter()
        .find(|check| check.name == "Observe membership")
        .unwrap();
    assert_eq!(observer.state, model::CheckState::Success);
    assert_eq!(
        observer.completed_at.as_deref(),
        Some("2026-09-29T08:41:04Z")
    );
    assert!(snapshot.has_label("caravan"));
    assert!(
        execution.restart_required,
        "observer success must not reuse the failed gate"
    );
    assert_eq!(execution.app_id, APP);
    assert_eq!(
        (
            execution.workflow_id,
            execution.run_id,
            execution.run_attempt
        ),
        (123, 99, 2)
    );
    assert_eq!(execution.contexts, [GATE]);
    assert_eq!(execution.pull_request.head_sha, "a".repeat(40));
    assert_eq!(execution.pull_request.base_sha, "b".repeat(40));
    let mut calls = reads(&pull, &suite, &run);
    // Independent literal expectation: using the production command builder here
    // would silently accept a regression to rerun-failed-jobs or --failed.
    calls.push((
        CommandSpec::new("gh")
            .args([
                "api",
                "--method",
                "POST",
                "repos/acme/widgets/actions/runs/99/rerun",
            ])
            .provider_write(),
        CommandOutput::success(""),
    ));
    calls.push((
        pull_request_command(&repository(), "12"),
        CommandOutput::success(pull.to_string()),
    ));
    let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));
    let receipt = adapter
        .restart_ci_execution(
            &repository(),
            &PullRequestPrecondition::from(&snapshot),
            &execution,
        )
        .unwrap();
    assert_eq!(
        receipt.after, snapshot,
        "requesting a fresh observer is not a green CI verdict"
    );
    adapter.runner.assert_exhausted();
}

#[test]
fn admission_refresh_identity_drift_refuses_before_any_workflow_write() {
    for case in ["app", "attempt", "workflow", "base", "head"] {
        let (pull, mut suite, mut run, snapshot, execution) = admission_fixture();
        match case {
            "app" => suite["app"]["id"] = json!(APP + 1),
            "attempt" => run["run_attempt"] = json!(3),
            "workflow" => run["workflow_id"] = json!(124),
            "base" => run["pull_requests"][0]["base"]["sha"] = json!("c".repeat(40)),
            "head" => run["head_sha"] = json!("c".repeat(40)),
            _ => unreachable!(),
        }
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(reads(&pull, &suite, &run)));
        let error = adapter
            .restart_ci_execution(
                &repository(),
                &PullRequestPrecondition::from(&snapshot),
                &execution,
            )
            .unwrap_err();
        assert_eq!(
            error.details().unwrap()["provider_write_attempted"],
            json!(false),
            "{case}"
        );
        adapter.runner.assert_exhausted();
    }
}

#[test]
fn admission_refresh_does_not_ignore_source_check_drift() {
    let (mut pull, _, _, snapshot, execution) = admission_fixture();
    pull["statusCheckRollup"][2]["conclusion"] = json!("FAILURE");
    let adapter = GitHubMutationAdapter::new(FakeRunner::new(vec![(
        pull_request_command(&repository(), "12"),
        CommandOutput::success(pull.to_string()),
    )]));
    let error = adapter
        .restart_ci_execution(
            &repository(),
            &PullRequestPrecondition::from(&snapshot),
            &execution,
        )
        .unwrap_err();
    assert_eq!(
        error.details().unwrap()["provider_write_attempted"],
        json!(false)
    );
    adapter.runner.assert_exhausted();
}
