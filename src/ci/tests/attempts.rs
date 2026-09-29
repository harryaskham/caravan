//! Exact-attempt evidence collection; no producer-origin or rerun authority.
use super::*;
use serde_json::{Value, json};

const RUN: u64 = 99;
const ATTEMPT: u64 = 2;

fn run_json(expected: &PullRequestPrecondition) -> Value {
    json!({
        "id": RUN, "run_attempt": ATTEMPT, "workflow_id": 7, "check_suite_id": 8,
        "name": "CI", "event": "pull_request", "status": "completed", "conclusion": "failure",
        "head_branch": "feature", "head_sha": expected.head_oid.0,
        "pull_requests": [{"number": expected.number.0,
            "head": {"sha": expected.head_oid.0}, "base": {"sha": expected.base_oid.0}}]
    })
}

fn jobs_json(expected: &PullRequestPrecondition, id: u64, conclusion: &str) -> Value {
    json!({"total_count": 1, "jobs": [{
        "id": id, "run_id": RUN, "head_sha": expected.head_oid.0,
        "name": "compile", "status": "completed", "conclusion": conclusion,
        "html_url": format!("https://github.com/harryaskham/caravan/actions/runs/{RUN}/job/{id}"),
        "steps": if conclusion == "failure" {
            json!([{"number": 1, "name": "Compile and test", "status": "completed", "conclusion": "failure"}])
        } else { json!([]) }
    }]})
}

// Intentionally independent of the production command builder: assert the
// immutable endpoint, not a helper that could make the same latest-read mistake.
fn attempt_jobs_command() -> CommandSpec {
    CommandSpec::new("gh").args([
        "api",
        "--method",
        "GET",
        "repos/harryaskham/caravan/actions/runs/99/attempts/2/jobs",
        "-f",
        "per_page=100",
    ])
}

struct AdvancingRunner {
    expected: PullRequestPrecondition,
    calls: RefCell<Vec<CommandSpec>>,
}

impl CommandRunner for AdvancingRunner {
    fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
        self.calls.borrow_mut().push(command.clone());
        let response = if command == &workflow_run_command(&repository(), RUN) {
            assert_eq!(self.calls.borrow().len(), 1);
            run_json(&self.expected)
        } else {
            // Attempt 3 begins between metadata and jobs. Its cancelled job is
            // returned only by the moving latest endpoint; attempt 2 failed a
            // real compiler step and must remain bound to job 201.
            assert_eq!(self.calls.borrow().len(), 2);
            if command == &attempt_jobs_command() {
                jobs_json(&self.expected, 201, "failure")
            } else {
                assert!(
                    command
                        .args
                        .iter()
                        .any(|arg| arg == "repos/harryaskham/caravan/actions/runs/99/jobs")
                );
                assert!(command.args.iter().any(|arg| arg == "filter=latest"));
                jobs_json(&self.expected, 301, "cancelled")
            }
        };
        Ok(CommandOutput::success(response.to_string()))
    }
}

pub(crate) fn advancing_attempt_diagnostics(
    expected: &PullRequestPrecondition,
) -> WorkflowFailureDiagnostics {
    let runner = AdvancingRunner {
        expected: expected.clone(),
        calls: RefCell::new(Vec::new()),
    };
    diagnose_failed_runs(&runner, &repository(), expected, &[RUN]).unwrap()
}

pub(crate) fn exact_attempt_diagnostics(
    expected: &PullRequestPrecondition,
    conclusion: &str,
) -> WorkflowFailureDiagnostics {
    let runner = FakeRunner::new(vec![
        (
            workflow_run_command(&repository(), RUN),
            CommandOutput::success(run_json(expected).to_string()),
        ),
        (
            attempt_jobs_command(),
            CommandOutput::success(jobs_json(expected, 201, conclusion).to_string()),
        ),
    ]);
    let response = diagnose_failed_runs(&runner, &repository(), expected, &[RUN]).unwrap();
    assert!(runner.calls.borrow().is_empty());
    response
}

#[test]
fn interleaved_attempt_advance_cannot_splice_latest_jobs_into_old_metadata() {
    let runner = AdvancingRunner {
        expected: precondition(),
        calls: RefCell::new(Vec::new()),
    };
    let response = diagnose_failed_runs(&runner, &repository(), &runner.expected, &[RUN]).unwrap();
    assert_eq!(response.runs[0].attempt, ATTEMPT);
    assert_eq!(response.runs[0].failed_jobs[0].job_id, 201);
    assert_eq!(response.runs[0].failed_jobs[0].conclusion, "failure");
    assert_eq!(runner.calls.borrow()[1], attempt_jobs_command());
    assert_eq!(
        runner.calls.borrow().len(),
        2,
        "no unbounded retry or log reads"
    );
}

#[test]
fn missing_malformed_or_mismatched_run_identity_stops_before_jobs() {
    for invalid in [
        "missing_attempt",
        "null_attempt",
        "zero_attempt",
        "negative_attempt",
        "text_attempt",
        "wrong_run",
        "zero_run",
        "missing_run",
        "empty_head",
        "missing_head",
    ] {
        let mut run = run_json(&precondition());
        match invalid {
            "missing_attempt" => {
                run.as_object_mut().unwrap().remove("run_attempt");
            }
            "null_attempt" => run["run_attempt"] = Value::Null,
            "zero_attempt" => run["run_attempt"] = json!(0),
            "negative_attempt" => run["run_attempt"] = json!(-1),
            "text_attempt" => run["run_attempt"] = json!("2"),
            "wrong_run" => run["id"] = json!(100),
            "zero_run" => run["id"] = json!(0),
            "missing_run" => {
                run.as_object_mut().unwrap().remove("id");
            }
            "empty_head" => run["head_sha"] = json!(""),
            "missing_head" => {
                run.as_object_mut().unwrap().remove("head_sha");
            }
            _ => unreachable!(),
        }
        let runner = FakeRunner::new(vec![(
            workflow_run_command(&repository(), RUN),
            CommandOutput::success(run.to_string()),
        )]);
        assert!(
            diagnose_failed_runs(&runner, &repository(), &precondition(), &[RUN]).is_err(),
            "{invalid}"
        );
        assert!(runner.calls.borrow().is_empty());
    }
}

#[test]
fn missing_or_foreign_job_identity_is_not_interpreted_or_logged() {
    for invalid in [
        "run",
        "head",
        "missing_run",
        "missing_head",
        "zero_job",
        "duplicate_job",
    ] {
        let mut jobs = jobs_json(&precondition(), 201, "failure");
        match invalid {
            "run" => jobs["jobs"][0]["run_id"] = json!(100),
            "head" => jobs["jobs"][0]["head_sha"] = json!("other-head"),
            "missing_run" => {
                jobs["jobs"][0].as_object_mut().unwrap().remove("run_id");
            }
            "missing_head" => {
                jobs["jobs"][0].as_object_mut().unwrap().remove("head_sha");
            }
            "zero_job" => jobs["jobs"][0]["id"] = json!(0),
            "duplicate_job" => {
                jobs["total_count"] = json!(2);
                let duplicate = jobs["jobs"][0].clone();
                jobs["jobs"].as_array_mut().unwrap().push(duplicate);
            }
            _ => unreachable!(),
        }
        // A foreign lineage job must not even cause a raw-log request.
        jobs["jobs"][0]["steps"][0]["name"] = json!("Verify selected ref lineage");
        let runner = FakeRunner::new(vec![
            (
                workflow_run_command(&repository(), RUN),
                CommandOutput::success(run_json(&precondition()).to_string()),
            ),
            (
                attempt_jobs_command(),
                CommandOutput::success(jobs.to_string()),
            ),
        ]);
        assert!(
            diagnose_failed_runs(&runner, &repository(), &precondition(), &[RUN]).is_err(),
            "{invalid}"
        );
        assert!(runner.calls.borrow().is_empty());
    }
}

#[test]
fn attempt_scoped_pages_preserve_bounds_and_partial_inventory_flags() {
    for total in [0, 2, 101] {
        let mut jobs = jobs_json(&precondition(), 201, "cancelled");
        jobs["total_count"] = json!(total);
        let runner = FakeRunner::new(vec![
            (
                workflow_run_command(&repository(), RUN),
                CommandOutput::success(run_json(&precondition()).to_string()),
            ),
            (
                attempt_jobs_command(),
                CommandOutput::success(jobs.to_string()),
            ),
        ]);
        let response =
            diagnose_failed_runs(&runner, &repository(), &precondition(), &[RUN]).unwrap();
        assert!(
            response.runs[0].jobs_truncated,
            "unproved inventory count {total}"
        );
        assert_eq!(response.runs[0].attempt, ATTEMPT);
        assert!(
            runner.calls.borrow().is_empty(),
            "one bounded page, no latest fallback"
        );
    }
}

#[test]
fn missing_attempt_endpoint_fails_without_latest_fallback() {
    let runner = FakeRunner::new(vec![
        (
            workflow_run_command(&repository(), RUN),
            CommandOutput::success(run_json(&precondition()).to_string()),
        ),
        (
            attempt_jobs_command(),
            CommandOutput {
                code: Some(1),
                stdout: String::new(),
                stderr: "gh: Not Found (HTTP 404)".into(),
            },
        ),
    ]);
    assert!(matches!(
        diagnose_failed_runs(&runner, &repository(), &precondition(), &[RUN]),
        Err(DiscoveryError::CommandFailed { .. })
    ));
    assert!(runner.calls.borrow().is_empty());
}
