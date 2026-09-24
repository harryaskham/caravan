use super::*;
use mcp_cli::StructuredError;

fn dispatch(
    repository_path: &Path,
    binding: &DispatchBinding,
    selected: &CiExecution,
    operation_id: &OperationId,
    request: impl FnOnce() -> Result<GitHubMutationReceipt, AppError>,
) -> Result<DispatchResult, AppError> {
    super::dispatch(
        repository_path,
        binding,
        selected,
        operation_id,
        || Ok(()),
        request,
    )
}
use crate::{
    model::{AutoMergeState, BranchSnapshot, CheckState, CommitOid, PullRequestState},
    required_runs::*,
};

pub(crate) fn evidence(
    pull: &mut PullRequestSnapshot,
    attempt: u64,
    status: &str,
    conclusion: &str,
) -> (RequiredContextsRead, HeadRunLineage) {
    // Historical PR226 shape: Cursor's queued suite sorts before live Actions.
    let suite_id = 97_044_911_496;
    let run_id = 35_840_774_774;
    pull.checks = vec![CheckSnapshot {
        name: "gate".into(),
        state: if conclusion == "failure" {
            CheckState::Failure
        } else {
            CheckState::Success
        },
        provider_kind: Some("CheckRun".into()),
        workflow_name: Some("CI".into()),
        app_id: Some(77),
        check_suite_id: Some(suite_id),
        head_oid: Some(pull.head.oid.clone()),
        details_url: Some(format!(
            "https://github.com/{}/actions/runs/{run_id}/job/1",
            pull.head.repository.slug()
        )),
        ..Default::default()
    }];
    let policy = RequiredContextsRead {
        branch: "main".into(),
        protected: true,
        contexts: vec![],
        checks: vec![RequiredCheck {
            context: "gate".into(),
            app_id: Some(77),
        }],
        complete: true,
    }
    .normalized();
    let binding = WorkflowPullRequestBinding {
        number: pull.number,
        head_sha: pull.head.oid.0.clone(),
        head_ref: pull.head.name.clone(),
        base_sha: pull.base.oid.0.clone(),
        base_ref: pull.base.name.clone(),
    };
    let lineage = HeadRunLineage {
        head_sha: pull.head.oid.0.clone(),
        head_committed_at: Some("2026-09-23T09:00:00Z".into()),
        complete: true,
        check_suites: vec![
            CheckSuiteLineage {
                id: 97_044_895_885,
                head_sha: pull.head.oid.0.clone(),
                status: "queued".into(),
                conclusion: String::new(),
                app_slug: "cursor".into(),
                rerequestable: true,
            },
            CheckSuiteLineage {
                id: suite_id,
                head_sha: pull.head.oid.0.clone(),
                status: status.into(),
                conclusion: conclusion.into(),
                app_slug: "github-actions".into(),
                rerequestable: true,
            },
        ],
        workflow_runs: vec![WorkflowRunLineage {
            run_id,
            check_suite_id: suite_id,
            workflow_name: "CI".into(),
            head_sha: pull.head.oid.0.clone(),
            status: status.into(),
            conclusion: conclusion.into(),
            event: "pull_request".into(),
            execution: Some(WorkflowExecutionIdentity {
                workflow_id: 12,
                run_attempt: attempt,
                pull_requests: vec![binding],
            }),
        }],
    };
    (policy, lineage)
}

pub(crate) fn repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(directory.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    directory
}

fn pull() -> PullRequestSnapshot {
    let repository = RepositoryId {
        owner: "fixture".into(),
        name: "project".into(),
    };
    PullRequestSnapshot {
        number: PrNumber(226),
        title: "fixture".into(),
        url: "https://github.com/fixture/project/pull/226".into(),
        state: PullRequestState::Open,
        draft: false,
        cross_repository: false,
        labels: BTreeSet::from(["caravan".into()]),
        head: BranchSnapshot {
            repository: repository.clone(),
            name: "topic".into(),
            oid: CommitOid("a".repeat(40)),
        },
        base: BranchSnapshot {
            repository,
            name: "main".into(),
            oid: CommitOid("b".repeat(40)),
        },
        checks: vec![],
        auto_merge: AutoMergeState::disabled(),
        created_at: None,
        merged_at: None,
        updated_at: None,
        merge_state_status: None,
    }
}

#[test]
fn ci_dispatch_reuses_active_first_and_replacement_attempt_without_cursor_request() {
    for attempt in [1, 2] {
        let mut pull = pull();
        let (policy, lineage) = evidence(&mut pull, attempt, "in_progress", "");
        let selected = select(&pull, &policy, Some("gate"), &lineage).unwrap();
        assert_eq!(selected.len(), 1);
        let execution = &selected[0];
        assert!(execution.reusable());
        assert_eq!(execution.run_attempt, attempt);
        assert_eq!(execution.check_suite_id, 97_044_911_496);
        let directory = repository();
        let result = dispatch(
            directory.path(),
            &binding(execution, &pull),
            execution,
            &OperationId("op".into()),
            || panic!("active CI must not be started again"),
        )
        .unwrap();
        assert_eq!(result.disposition, DispatchDisposition::Reused);
        let keys = crate::stack_checkpoint::list_keys(directory.path(), "ci-start-").unwrap();
        assert_eq!(
            keys.len(),
            1,
            "reuse is durable even when no write is needed"
        );
        let saved: DispatchCheckpoint = crate::stack_checkpoint::load(directory.path(), &keys[0])
            .unwrap()
            .unwrap();
        assert_eq!(saved.disposition, DispatchDisposition::Reused);
        assert!(saved.provider_receipt.is_none());
    }
}

#[test]
fn ci_dispatch_refuses_wrong_app_suite_workflow_attempt_base_head_and_partial_evidence() {
    for case in [
        "app",
        "aggregate",
        "suite",
        "workflow",
        "attempt",
        "base",
        "head",
        "pr",
        "duplicate",
        "partial",
        "policy",
    ] {
        let mut pull = pull();
        let (mut policy, mut lineage) = evidence(&mut pull, 1, "in_progress", "");
        match case {
            "app" => pull.checks[0].app_id = Some(88),
            "aggregate" => pull.checks[0].provider_kind = Some("WorkflowRunLineage".into()),
            "suite" => pull.checks[0].check_suite_id = Some(42),
            "workflow" => pull.checks[0].workflow_name = Some("Other".into()),
            "attempt" => {
                lineage.workflow_runs[0]
                    .execution
                    .as_mut()
                    .unwrap()
                    .run_attempt = 0
            }
            "base" => {
                lineage.workflow_runs[0]
                    .execution
                    .as_mut()
                    .unwrap()
                    .pull_requests[0]
                    .base_sha = "c".repeat(40)
            }
            "head" => lineage.workflow_runs[0].head_sha = "c".repeat(40),
            "pr" => {
                lineage.workflow_runs[0]
                    .execution
                    .as_mut()
                    .unwrap()
                    .pull_requests[0]
                    .number = PrNumber(227)
            }
            "duplicate" => lineage.workflow_runs.push(lineage.workflow_runs[0].clone()),
            "partial" => lineage.complete = false,
            "policy" => policy.complete = false,
            _ => unreachable!(),
        }
        assert!(
            select(&pull, &policy, Some("gate"), &lineage).is_err(),
            "{case}"
        );
    }
}

#[test]
fn ci_dispatch_preserves_required_third_party_failure_and_does_not_assume_actions_required() {
    let mut pull = pull();
    let (mut policy, lineage) = evidence(&mut pull, 1, "in_progress", "");
    pull.checks.push(CheckSnapshot {
        name: "external".into(),
        state: CheckState::Failure,
        app_id: Some(88),
        head_oid: Some(pull.head.oid.clone()),
        check_suite_id: Some(lineage.check_suites[0].id),
        provider_kind: Some("CheckRun".into()),
        ..Default::default()
    });
    policy.checks.push(RequiredCheck {
        context: "external".into(),
        app_id: Some(88),
    });
    assert_eq!(select(&pull, &policy, None, &lineage).unwrap().len(), 1);
    let assessment = assess(&RequiredRunsInput {
        pr: pull.number,
        head: &pull.head,
        base: &pull.base,
        contexts: &policy,
        lineage: Some(&lineage),
        checks: &pull.checks,
        head_published_at: None,
        clock: RequiredRunsClock {
            now_unix: 1000,
            grace_secs: 0,
        },
    });
    assert_eq!(assessment.status, RequiredRunsStatus::Failing);
    policy.contexts.clear();
    policy
        .checks
        .retain(|required| required.context == "external");
    assert!(select(&pull, &policy, None, &lineage).unwrap().is_empty());
    assert_eq!(pull.checks[1].state, CheckState::Failure);
}

#[test]
fn ci_dispatch_optional_workflow_failure_cannot_authorize_a_restart() {
    let mut pull = pull();
    let (policy, lineage) = evidence(&mut pull, 1, "completed", "failure");
    pull.checks[0].state = CheckState::Success;
    let execution = select(&pull, &policy, None, &lineage).unwrap().remove(0);
    assert!(execution.reusable());
    assert!(!execution.requestable());
}

#[test]
fn ci_dispatch_newest_workflow_is_not_an_older_valid_run_fallback() {
    let mut pull = pull();
    let (policy, mut lineage) = evidence(&mut pull, 1, "completed", "failure");
    let mut newer = lineage.workflow_runs[0].clone();
    newer.run_id += 1;
    newer.status = "in_progress".into();
    newer.conclusion.clear();
    lineage.workflow_runs.push(newer);
    lineage.workflow_runs[0]
        .execution
        .as_mut()
        .unwrap()
        .pull_requests[0]
        .base_sha = "old-base".into();
    assert_eq!(
        select(&pull, &policy, None, &lineage).unwrap()[0].run_id,
        lineage.workflow_runs[1].run_id
    );
    lineage.workflow_runs[1]
        .execution
        .as_mut()
        .unwrap()
        .pull_requests[0]
        .base_ref = "other-parent".into();
    assert!(select(&pull, &policy, None, &lineage).is_err());
}

fn binding(selected: &CiExecution, pull: &PullRequestSnapshot) -> DispatchBinding {
    DispatchBinding {
        repository: pull.head.repository.clone(),
        pull_request: selected.pull_request.clone(),
        caravan_members: vec![pull.number],
        workflow_id: selected.workflow_id,
    }
}

#[test]
fn ci_dispatch_response_loss_is_fenced_across_restart_until_successor_is_observed() {
    let mut pull = pull();
    let (policy, lineage) = evidence(&mut pull, 1, "completed", "failure");
    let execution = select(&pull, &policy, None, &lineage).unwrap().remove(0);
    let binding = binding(&execution, &pull);
    let directory = repository();
    let calls = std::cell::Cell::new(0);
    let error = dispatch(
        directory.path(),
        &binding,
        &execution,
        &OperationId("original".into()),
        || {
            calls.set(calls.get() + 1);
            Err(AppError::validation(
                "lost_response",
                "provider may have accepted",
            ))
        },
    )
    .err()
    .unwrap();
    assert_eq!(error.code(), "lost_response");
    let retry = dispatch(
        directory.path(),
        &binding,
        &execution,
        &OperationId("restart".into()),
        || panic!("indeterminate write must not replay"),
    );
    assert_eq!(retry.err().unwrap().code(), "ci_dispatch_indeterminate");
    let mut successor = execution.clone();
    successor.run_attempt = 2;
    successor.status = "in_progress".into();
    successor.conclusion.clear();
    let result = dispatch(
        directory.path(),
        &binding,
        &successor,
        &OperationId("restart".into()),
        || panic!("successor already exists"),
    )
    .unwrap();
    assert_eq!(result.disposition, DispatchDisposition::SuccessorObserved);
    assert_eq!(result.operation_id.0, "original");
    let key = crate::stack_checkpoint::list_keys(directory.path(), "ci-start-")
        .unwrap()
        .remove(0);
    let checkpoint: DispatchCheckpoint = crate::stack_checkpoint::load(directory.path(), &key)
        .unwrap()
        .unwrap();
    assert!(
        checkpoint.provider_receipt.is_none(),
        "observation is not an invented acknowledgement"
    );
    assert_eq!(checkpoint.observed_successor, Some(successor));
    assert_eq!(calls.get(), 1);
    assert_eq!(pull.labels, BTreeSet::from(["caravan".into()]));
}

#[test]
fn ci_dispatch_accepted_completed_restart_has_one_durable_request() {
    let mut pull = pull();
    let (policy, lineage) = evidence(&mut pull, 1, "completed", "failure");
    let execution = select(&pull, &policy, None, &lineage).unwrap().remove(0);
    let binding = binding(&execution, &pull);
    let directory = repository();
    let receipt = GitHubMutationReceipt {
        kind: crate::model::MutationKind::RerunChecks,
        before: Some(pull.clone()),
        after: pull,
        provider_output: None,
    };
    let first = dispatch(
        directory.path(),
        &binding,
        &execution,
        &OperationId("original".into()),
        || Ok(receipt),
    )
    .unwrap();
    assert!(first.provider_receipt.is_some());
    let retry = dispatch(
        directory.path(),
        &binding,
        &execution,
        &OperationId("retry".into()),
        || panic!("accepted request must not replay"),
    )
    .unwrap();
    assert_eq!(retry.disposition, DispatchDisposition::Requested);
    assert_eq!(retry.operation_id.0, "original");
    assert!(retry.provider_receipt.is_none());
}

#[test]
fn ci_dispatch_reuse_stays_zero_write_after_that_execution_fails() {
    let mut pull = pull();
    let (policy, lineage) = evidence(&mut pull, 1, "in_progress", "");
    let mut execution = select(&pull, &policy, None, &lineage).unwrap().remove(0);
    let binding = binding(&execution, &pull);
    let directory = repository();
    dispatch(
        directory.path(),
        &binding,
        &execution,
        &OperationId("first".into()),
        || panic!("already running"),
    )
    .unwrap();
    execution.status = "completed".into();
    execution.conclusion = "failure".into();
    execution.restart_required = true;
    let replay = dispatch(
        directory.path(),
        &binding,
        &execution,
        &OperationId("restart".into()),
        || panic!("reuse is not an automatic failure rerun"),
    )
    .unwrap();
    assert_eq!(replay.disposition, DispatchDisposition::Reused);
    assert_eq!(
        execution.conclusion, "failure",
        "no green verdict was manufactured"
    );
}

#[test]
fn ci_dispatch_definite_prewrite_rejection_allows_fresh_retry_with_archived_evidence() {
    let mut pull = pull();
    let (policy, lineage) = evidence(&mut pull, 1, "completed", "failure");
    let execution = select(&pull, &policy, None, &lineage).unwrap().remove(0);
    let binding = binding(&execution, &pull);
    let directory = repository();
    let first = dispatch(
        directory.path(),
        &binding,
        &execution,
        &OperationId("first".into()),
        || {
            Err(AppError::structured(
                ErrorCategory::Validation,
                "prewrite_refusal",
                "no POST was attempted",
                Some(serde_json::json!({"provider_write_attempted":false})),
            ))
        },
    );
    assert!(first.is_err());
    let receipt = GitHubMutationReceipt {
        kind: crate::model::MutationKind::RerunChecks,
        before: Some(pull.clone()),
        after: pull,
        provider_output: None,
    };
    assert!(
        dispatch(
            directory.path(),
            &binding,
            &execution,
            &OperationId("retry".into()),
            || Ok(receipt)
        )
        .unwrap()
        .provider_receipt
        .is_some()
    );
    let keys = crate::stack_checkpoint::list_keys(directory.path(), "ci-refused-").unwrap();
    assert_eq!(keys.len(), 1);
    let rejected: DispatchCheckpoint = crate::stack_checkpoint::load(directory.path(), &keys[0])
        .unwrap()
        .unwrap();
    assert_eq!(
        rejected.rejected_before_write.as_deref(),
        Some("prewrite_refusal")
    );
    assert!(rejected.provider_receipt.is_none());
}

#[test]
fn ci_dispatch_budget_refusal_precedes_intent_and_old_gate_rows_do_not_restart_new_attempts() {
    let mut pull = pull();
    let (policy, lineage) = evidence(&mut pull, 2, "completed", "failure");
    let execution = select(&pull, &policy, None, &lineage).unwrap().remove(0);
    let directory = repository();
    let blocked = super::dispatch(
        directory.path(),
        &binding(&execution, &pull),
        &execution,
        &OperationId("op".into()),
        || Err(AppError::validation("budget", "no budget")),
        || panic!("budget exhausted"),
    );
    assert!(blocked.is_err());
    assert!(
        crate::stack_checkpoint::list_keys(directory.path(), "ci-start-")
            .unwrap()
            .is_empty()
    );
    pull.checks[0].started_at = Some("2026-09-23T09:00:00Z".into());
    let mut current = pull.checks[0].clone();
    current.state = CheckState::Success;
    current.started_at = Some("2026-09-23T09:05:00Z".into());
    pull.checks.push(current);
    let current = select(&pull, &policy, None, &lineage).unwrap().remove(0);
    assert!(
        !current.restart_required,
        "old attempt's failed gate is superseded"
    );
    assert!(
        current.reusable(),
        "optional workflow failure does not request another attempt"
    );
}

#[test]
fn ci_dispatch_successor_status_progresses_but_attempt_observation_cannot_regress() {
    let mut pull = pull();
    let (policy, lineage) = evidence(&mut pull, 1, "completed", "failure");
    let execution = select(&pull, &policy, None, &lineage).unwrap().remove(0);
    let binding = binding(&execution, &pull);
    let directory = repository();
    assert!(
        dispatch(
            directory.path(),
            &binding,
            &execution,
            &OperationId("op".into()),
            || Err(unavailable("response lost"))
        )
        .is_err()
    );
    let mut successor = execution.clone();
    successor.run_attempt = 2;
    successor.status = "in_progress".into();
    successor.conclusion.clear();
    dispatch(
        directory.path(),
        &binding,
        &successor,
        &OperationId("retry".into()),
        || panic!("already running"),
    )
    .unwrap();
    successor.status = "completed".into();
    successor.conclusion = "failure".into();
    let result = dispatch(
        directory.path(),
        &binding,
        &successor,
        &OperationId("retry".into()),
        || panic!("do not rerun finished successor"),
    )
    .unwrap();
    assert_eq!(result.disposition, DispatchDisposition::SuccessorObserved);
    assert!(
        dispatch(
            directory.path(),
            &binding,
            &execution,
            &OperationId("stale".into()),
            || panic!("stale observation")
        )
        .is_err()
    );
}

#[test]
fn ci_dispatch_storage_failure_prevents_provider_write_and_truncation_is_not_complete() {
    let mut pull = pull();
    let (policy, mut lineage) = evidence(&mut pull, 1, "completed", "failure");
    let execution = select(&pull, &policy, None, &lineage).unwrap().remove(0);
    let non_repository = tempfile::tempdir().unwrap();
    assert!(
        dispatch(
            non_repository.path(),
            &binding(&execution, &pull),
            &execution,
            &OperationId("op".into()),
            || panic!("no durable intent, no write")
        )
        .is_err()
    );
    lineage.workflow_runs = (0..(MAX_REPORTED_LINEAGE + 1))
        .map(|offset| {
            let mut run = lineage.workflow_runs[0].clone();
            run.run_id += u64::try_from(offset).unwrap();
            run
        })
        .collect();
    assert!(!lineage.bounded().complete);
}
