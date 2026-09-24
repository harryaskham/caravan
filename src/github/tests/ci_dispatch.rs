use super::*;
use crate::ci_dispatch::{CiExecution, DispatchBinding};
use mcp_cli::StructuredError;
use serde_json::{Value, json};

fn fixture() -> (Value, Value, Value, model::PullRequestSnapshot, CiExecution) {
    let mut pull: Value =
        serde_json::from_str(&pr_object_json(12, "feature/widget", "acme/widgets")).unwrap();
    pull["statusCheckRollup"] = json!([{"__typename":"CheckRun", "name":"gate", "workflowName":"CI", "status":"COMPLETED", "conclusion":"FAILURE",
        "appId":77, "checkSuiteId":4242, "headOid":"head-12", "detailsUrl":"https://github.com/acme/widgets/actions/runs/99/job/1"}]);
    let run = json!({"id":99,"check_suite_id":4242,"workflow_id":123,"run_attempt":1,"name":"CI","head_sha":"head-12","status":"completed","conclusion":"failure","event":"pull_request",
        "pull_requests":[{"number":12,"head":{"sha":"head-12","ref":"feature/widget"},"base":{"sha":"base-12","ref":"main"}}]});
    let suite = json!({"id":4242,"head_sha":"head-12","status":"completed","conclusion":"failure","app":{"id":77,"slug":"github-actions"}});
    let snapshot = serde_json::from_value::<PullRequestJson>(pull.clone())
        .unwrap()
        .into_snapshot(&repository())
        .unwrap();
    let execution = CiExecution::from_run(
        &snapshot,
        &serde_json::from_value::<HeadWorkflowRunJson>(run.clone())
            .unwrap()
            .into(),
        &serde_json::from_value::<CheckSuiteJson>(suite.clone())
            .unwrap()
            .into(),
        77,
        &["gate".into()],
    )
    .unwrap();
    (pull, suite, run, snapshot, execution)
}

fn reads(pull: &Value, suite: &Value, run: &Value) -> Vec<(CommandSpec, CommandOutput)> {
    vec![
        (
            pull_request_command(&repository(), "12"),
            CommandOutput::success(pull.to_string()),
        ),
        (
            check_suite_command(&repository(), 4242),
            CommandOutput::success(suite.to_string()),
        ),
        (
            workflow_run_command(&repository(), 99),
            CommandOutput::success(run.to_string()),
        ),
    ]
}

#[test]
fn ci_dispatch_provider_binds_execution_before_actions_only_restart_and_readback() {
    let (pull, suite, run, snapshot, execution) = fixture();
    assert_eq!(execution.run_attempt, 1);
    assert_eq!(execution.workflow_id, 123);
    assert_eq!(execution.pull_request.base_sha, "base-12");
    let mut calls = reads(&pull, &suite, &run);
    calls.push((
        rerun_workflow_command(&repository(), 99),
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
    assert_eq!(receipt.kind, MutationKind::RerunChecks);
    assert_eq!(receipt.after, snapshot);
    adapter.runner.assert_exhausted();
    let mut legacy = run;
    legacy.as_object_mut().unwrap().remove("run_attempt");
    let legacy: crate::required_runs::WorkflowRunLineage =
        serde_json::from_value::<HeadWorkflowRunJson>(legacy)
            .unwrap()
            .into();
    assert!(
        legacy.execution.is_none(),
        "never invent attempt one for old provider metadata"
    );
}

#[test]
fn ci_dispatch_provider_rechecks_app_workflow_attempt_suite_and_base_with_zero_writes() {
    for case in [
        "app", "slug", "workflow", "attempt", "suite", "base", "active",
    ] {
        let (pull, mut suite, mut run, snapshot, execution) = fixture();
        match case {
            "app" => suite["app"]["id"] = json!(88),
            "slug" => suite["app"]["slug"] = json!("cursor"),
            "workflow" => run["workflow_id"] = json!(456),
            "attempt" => run["run_attempt"] = json!(2),
            "suite" => run["check_suite_id"] = json!(999),
            "base" => run["pull_requests"][0]["base"]["sha"] = json!("new-base"),
            "active" => {
                run["status"] = json!("in_progress");
                run["conclusion"] = Value::Null;
            }
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
fn ci_dispatch_provider_response_or_readback_loss_never_repeats_a_write() {
    for post_readback in [false, true] {
        let (pull, suite, run, snapshot, execution) = fixture();
        let mut calls = reads(&pull, &suite, &run);
        let failed = CommandOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: "network response unavailable".into(),
        };
        if post_readback {
            calls.push((
                rerun_workflow_command(&repository(), 99),
                CommandOutput::success(""),
            ));
            calls.push((pull_request_command(&repository(), "12"), failed));
        } else {
            calls.push((rerun_workflow_command(&repository(), 99), failed));
        }
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));
        let directory = crate::ci_dispatch::tests::repository();
        let binding = DispatchBinding {
            repository: repository(),
            pull_request: execution.pull_request.clone(),
            caravan_members: vec![snapshot.number],
            workflow_id: execution.workflow_id,
        };
        let first = crate::ci_dispatch::dispatch(
            directory.path(),
            &binding,
            &execution,
            &model::OperationId("first".into()),
            || Ok(()),
            || {
                adapter.restart_ci_execution(
                    &repository(),
                    &PullRequestPrecondition::from(&snapshot),
                    &execution,
                )
            },
        );
        assert!(first.is_err());
        let second = crate::ci_dispatch::dispatch(
            directory.path(),
            &binding,
            &execution,
            &model::OperationId("restart".into()),
            || Ok(()),
            || {
                adapter.restart_ci_execution(
                    &repository(),
                    &PullRequestPrecondition::from(&snapshot),
                    &execution,
                )
            },
        );
        assert_eq!(second.err().unwrap().code(), "ci_dispatch_indeterminate");
        adapter.runner.assert_exhausted();
    }
}

#[test]
fn ci_dispatch_provider_truncated_lists_never_prove_execution_completeness() {
    let (pull, suite, run, snapshot, _) = fixture();
    for truncated_suites in [true, false] {
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(vec![
            (pull_request_command(&repository(), "12"), CommandOutput::success(pull.to_string())),
            (check_suites_command(&repository(), "head-12"), CommandOutput::success(json!({"total_count":if truncated_suites { 2 } else { 1 },"check_suites":[suite.clone()]}).to_string())),
            (head_runs_command(&repository(), "head-12"), CommandOutput::success(json!({"total_count":if truncated_suites { 1 } else { 2 },"workflow_runs":[run.clone()]}).to_string())),
            (commit_command(&repository(), "head-12"), CommandOutput::success(r#"{"commit":{"committer":{"date":"2026-09-23T09:00:00Z"}}}"#)),
        ]));
        assert!(
            !adapter
                .head_run_lineage(&repository(), &PullRequestPrecondition::from(&snapshot))
                .unwrap()
                .complete
        );
        adapter.runner.assert_exhausted();
    }
}
