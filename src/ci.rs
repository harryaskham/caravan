//! Bounded, structured GitHub Actions failure evidence.
//!
//! This layer deliberately consumes provider JSON only. It never reads full job
//! logs, never classifies policy, and never decides whether to rerun a run.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::command::{CommandRunner, CommandSpec};
use crate::github::{DiscoveryError, JsonDecodeEvidence};
use crate::model::{CommitOid, PrNumber, PullRequestPrecondition, RepositoryId};

const MAX_DIAGNOSTIC_RUNS: usize = 10;
const MAX_FAILED_JOBS: usize = 25;
const MAX_FAILED_STEPS: usize = 25;
const MAX_EVIDENCE_EDGE_BYTES: usize = 4 * 1024;

/// One PR generation associated with an immutable Actions run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowRunPullRequestAssociation {
    pub pr: PrNumber,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<CommitOid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_oid: Option<CommitOid>,
}

/// One failed or otherwise non-successful Actions step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowStepFailureDiagnostic {
    pub number: u64,
    pub name: String,
    pub status: String,
    pub conclusion: String,
}

/// One failed/cancelled/timed-out Actions job and its bounded failed steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowJobFailureDiagnostic {
    pub job_id: u64,
    pub name: String,
    pub status: String,
    pub conclusion: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_name: Option<String>,
    #[serde(default)]
    pub runner_labels: Vec<String>,
    #[serde(default)]
    pub failed_steps: Vec<WorkflowStepFailureDiagnostic>,
    pub steps_truncated: bool,
}

/// Bounded provider evidence for one immutable Actions run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowRunFailureDiagnostic {
    pub run_id: u64,
    pub attempt: u64,
    pub workflow_id: u64,
    pub check_suite_id: u64,
    pub workflow_name: String,
    pub event: String,
    pub status: String,
    pub conclusion: String,
    pub head_branch: String,
    pub head_sha: CommitOid,
    pub expected_pr: PrNumber,
    pub expected_head_oid: CommitOid,
    pub expected_base_oid: CommitOid,
    #[serde(default)]
    pub pull_requests: Vec<WorkflowRunPullRequestAssociation>,
    #[serde(default)]
    pub failed_jobs: Vec<WorkflowJobFailureDiagnostic>,
    pub jobs_total: usize,
    pub jobs_truncated: bool,
}

/// Complete bounded response for requested failed run IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowFailureDiagnostics {
    #[serde(default)]
    pub requested_run_ids: Vec<u64>,
    #[serde(default)]
    pub runs: Vec<WorkflowRunFailureDiagnostic>,
    pub runs_truncated: bool,
}

/// Fetch structured run/job/step evidence without downloading raw logs.
pub fn diagnose_failed_runs(
    runner: &impl CommandRunner,
    repository: &RepositoryId,
    expected: &PullRequestPrecondition,
    run_ids: &[u64],
) -> Result<WorkflowFailureDiagnostics, DiscoveryError> {
    let requested_run_ids = run_ids.iter().copied().collect::<BTreeSet<_>>();
    let runs_truncated = requested_run_ids.len() > MAX_DIAGNOSTIC_RUNS;
    let selected = requested_run_ids
        .iter()
        .copied()
        .take(MAX_DIAGNOSTIC_RUNS)
        .collect::<Vec<_>>();
    let mut runs = Vec::with_capacity(selected.len());
    for run_id in &selected {
        let run: WorkflowRunApiJson =
            checked_json(runner, workflow_run_command(repository, *run_id))?;
        let jobs: WorkflowJobsApiJson =
            checked_json(runner, workflow_jobs_command(repository, *run_id))?;
        runs.push(run.into_diagnostic(expected, jobs));
    }
    Ok(WorkflowFailureDiagnostics {
        requested_run_ids: requested_run_ids.into_iter().collect(),
        runs,
        runs_truncated,
    })
}

fn checked_json<T: DeserializeOwned>(
    runner: &impl CommandRunner,
    command: CommandSpec,
) -> Result<T, DiscoveryError> {
    let output = runner.run(&command)?;
    if !output.is_success() {
        return Err(DiscoveryError::CommandFailed {
            command,
            code: output.code,
            stderr: evidence_excerpt(&output.stderr),
        });
    }
    serde_json::from_str(&output.stdout).map_err(|error| DiscoveryError::InvalidJson {
        command,
        message: error.to_string(),
        evidence: Box::new(JsonDecodeEvidence {
            stdout: evidence_excerpt(&output.stdout),
            stderr: evidence_excerpt(&output.stderr),
        }),
    })
}

fn evidence_excerpt(value: &str) -> String {
    if value.len() <= MAX_EVIDENCE_EDGE_BYTES * 2 {
        return value.to_owned();
    }
    let mut prefix_end = MAX_EVIDENCE_EDGE_BYTES;
    while !value.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }
    let mut suffix_start = value.len() - MAX_EVIDENCE_EDGE_BYTES;
    while !value.is_char_boundary(suffix_start) {
        suffix_start += 1;
    }
    format!(
        "{}\n...[{} bytes omitted]...\n{}",
        &value[..prefix_end],
        suffix_start.saturating_sub(prefix_end),
        &value[suffix_start..]
    )
}

fn workflow_run_command(repository: &RepositoryId, run_id: u64) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        format!("repos/{}/actions/runs/{run_id}", repository.slug()),
    ])
}

fn workflow_jobs_command(repository: &RepositoryId, run_id: u64) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        "--method".to_owned(),
        "GET".to_owned(),
        format!("repos/{}/actions/runs/{run_id}/jobs", repository.slug()),
        "-f".to_owned(),
        "filter=latest".to_owned(),
        "-f".to_owned(),
        "per_page=100".to_owned(),
    ])
}

fn failure_conclusion(conclusion: &str) -> bool {
    !matches!(
        conclusion.to_ascii_lowercase().as_str(),
        "success" | "neutral" | "skipped"
    )
}

#[derive(Debug, Deserialize)]
struct WorkflowRunApiJson {
    id: u64,
    #[serde(default = "one")]
    run_attempt: u64,
    workflow_id: u64,
    check_suite_id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    status: String,
    conclusion: Option<String>,
    #[serde(default)]
    head_branch: String,
    head_sha: String,
    #[serde(default)]
    pull_requests: Vec<WorkflowRunPullRequestJson>,
}

const fn one() -> u64 {
    1
}

impl WorkflowRunApiJson {
    fn into_diagnostic(
        self,
        expected: &PullRequestPrecondition,
        jobs: WorkflowJobsApiJson,
    ) -> WorkflowRunFailureDiagnostic {
        let jobs_total = usize::try_from(jobs.total_count).unwrap_or(usize::MAX);
        let mut failed_jobs = jobs
            .jobs
            .into_iter()
            .filter(|job| failure_conclusion(job.conclusion.as_deref().unwrap_or("")))
            .map(WorkflowJobJson::into_diagnostic)
            .collect::<Vec<_>>();
        let jobs_truncated = failed_jobs.len() > MAX_FAILED_JOBS || jobs_total > 100;
        failed_jobs.truncate(MAX_FAILED_JOBS);
        WorkflowRunFailureDiagnostic {
            run_id: self.id,
            attempt: self.run_attempt,
            workflow_id: self.workflow_id,
            check_suite_id: self.check_suite_id,
            workflow_name: self.name,
            event: self.event,
            status: self.status,
            conclusion: self.conclusion.unwrap_or_default(),
            head_branch: self.head_branch,
            head_sha: CommitOid(self.head_sha),
            expected_pr: expected.number,
            expected_head_oid: expected.head_oid.clone(),
            expected_base_oid: expected.base_oid.clone(),
            pull_requests: self
                .pull_requests
                .into_iter()
                .map(WorkflowRunPullRequestJson::into_association)
                .collect(),
            failed_jobs,
            jobs_total,
            jobs_truncated,
        }
    }
}

#[derive(Debug, Deserialize)]
struct WorkflowRunPullRequestJson {
    number: u64,
    head: Option<WorkflowRefJson>,
    base: Option<WorkflowRefJson>,
}

impl WorkflowRunPullRequestJson {
    fn into_association(self) -> WorkflowRunPullRequestAssociation {
        WorkflowRunPullRequestAssociation {
            pr: PrNumber(self.number),
            head_oid: self.head.map(|value| CommitOid(value.sha)),
            base_oid: self.base.map(|value| CommitOid(value.sha)),
        }
    }
}

#[derive(Debug, Deserialize)]
struct WorkflowRefJson {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct WorkflowJobsApiJson {
    total_count: u64,
    #[serde(default)]
    jobs: Vec<WorkflowJobJson>,
}

#[derive(Debug, Deserialize)]
struct WorkflowJobJson {
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    conclusion: Option<String>,
    #[serde(default)]
    html_url: String,
    runner_name: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    steps: Vec<WorkflowStepJson>,
}

impl WorkflowJobJson {
    fn into_diagnostic(self) -> WorkflowJobFailureDiagnostic {
        let mut failed_steps = self
            .steps
            .into_iter()
            .filter(|step| failure_conclusion(step.conclusion.as_deref().unwrap_or("")))
            .map(WorkflowStepJson::into_diagnostic)
            .collect::<Vec<_>>();
        let steps_truncated = failed_steps.len() > MAX_FAILED_STEPS;
        failed_steps.truncate(MAX_FAILED_STEPS);
        WorkflowJobFailureDiagnostic {
            job_id: self.id,
            name: self.name,
            status: self.status,
            conclusion: self.conclusion.unwrap_or_default(),
            url: self.html_url,
            runner_name: self.runner_name,
            runner_labels: self.labels,
            failed_steps,
            steps_truncated,
        }
    }
}

#[derive(Debug, Deserialize)]
struct WorkflowStepJson {
    number: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    conclusion: Option<String>,
}

impl WorkflowStepJson {
    fn into_diagnostic(self) -> WorkflowStepFailureDiagnostic {
        WorkflowStepFailureDiagnostic {
            number: self.number,
            name: self.name,
            status: self.status,
            conclusion: self.conclusion.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeSet, VecDeque};

    use super::*;
    use crate::command::{CommandOutput, CommandRunError, CommandRunner};
    use crate::model::{AutoMergeState, PullRequestState};

    struct FakeRunner {
        calls: RefCell<VecDeque<(CommandSpec, CommandOutput)>>,
    }

    impl FakeRunner {
        fn new(calls: Vec<(CommandSpec, CommandOutput)>) -> Self {
            Self {
                calls: RefCell::new(calls.into()),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
            let (expected, output) = self
                .calls
                .borrow_mut()
                .pop_front()
                .expect("unexpected call");
            assert_eq!(&expected, command);
            Ok(output)
        }
    }

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "harryaskham".to_owned(),
            name: "caravan".to_owned(),
        }
    }

    fn precondition() -> PullRequestPrecondition {
        PullRequestPrecondition {
            number: PrNumber(12),
            state: PullRequestState::Open,
            head_oid: CommitOid("current-head".to_owned()),
            base_ref: "main".to_owned(),
            base_oid: CommitOid("current-base".to_owned()),
            labels: BTreeSet::from(["caravan".to_owned()]),
            checks: Vec::new(),
            auto_merge: AutoMergeState::disabled(),
        }
    }

    #[test]
    fn diagnostics_are_structured_bounded_and_never_fetch_logs() {
        let repository = repository();
        let run_id = 99;
        let run = serde_json::json!({
            "id": run_id,
            "run_attempt": 2,
            "workflow_id": 7,
            "check_suite_id": 8,
            "name": "CI",
            "event": "pull_request",
            "status": "completed",
            "conclusion": "failure",
            "head_branch": "feature",
            "head_sha": "current-head",
            "pull_requests": [{
                "number": 12,
                "head": {"sha": "stale-head"},
                "base": {"sha": "current-base"}
            }]
        });
        let jobs = serde_json::json!({
            "total_count": 3,
            "jobs": [
                {
                    "id": 101,
                    "name": "check",
                    "status": "completed",
                    "conclusion": "failure",
                    "html_url": "https://example.test/job/101",
                    "runner_name": "runner-1",
                    "labels": ["self-hosted"],
                    "steps": [
                        {"number": 1, "name": "setup", "status": "completed", "conclusion": "success"},
                        {"number": 2, "name": "test", "status": "completed", "conclusion": "failure"}
                    ]
                },
                {"id": 102, "name": "pass", "status": "completed", "conclusion": "success", "steps": []},
                {"id": 103, "name": "skip", "status": "completed", "conclusion": "skipped", "steps": []}
            ]
        });
        let runner = FakeRunner::new(vec![
            (
                workflow_run_command(&repository, run_id),
                CommandOutput::success(run.to_string()),
            ),
            (
                workflow_jobs_command(&repository, run_id),
                CommandOutput::success(jobs.to_string()),
            ),
        ]);

        let output =
            diagnose_failed_runs(&runner, &repository, &precondition(), &[run_id]).unwrap();

        assert_eq!(output.runs.len(), 1);
        let diagnostic = &output.runs[0];
        assert_eq!(diagnostic.attempt, 2);
        assert_eq!(
            diagnostic.pull_requests[0].head_oid,
            Some(CommitOid("stale-head".to_owned()))
        );
        assert_eq!(diagnostic.failed_jobs.len(), 1);
        assert_eq!(diagnostic.failed_jobs[0].failed_steps[0].name, "test");
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn run_ids_are_deduplicated_sorted_and_capped_before_provider_calls() {
        let repository = repository();
        let ids = (1..=12).rev().chain([1, 2]).collect::<Vec<_>>();
        let calls = (1..=10)
            .flat_map(|run_id| {
                let run = serde_json::json!({
                    "id": run_id,
                    "workflow_id": 7,
                    "check_suite_id": 8,
                    "head_sha": "current-head"
                });
                let jobs = serde_json::json!({"total_count": 0, "jobs": []});
                [
                    (
                        workflow_run_command(&repository, run_id),
                        CommandOutput::success(run.to_string()),
                    ),
                    (
                        workflow_jobs_command(&repository, run_id),
                        CommandOutput::success(jobs.to_string()),
                    ),
                ]
            })
            .collect::<Vec<_>>();
        let runner = FakeRunner::new(calls);

        let output = diagnose_failed_runs(&runner, &repository, &precondition(), &ids).unwrap();

        assert!(output.runs_truncated);
        assert_eq!(output.runs.len(), 10);
        assert_eq!(output.runs[0].run_id, 1);
        assert_eq!(output.runs[9].run_id, 10);
        assert!(runner.calls.borrow().is_empty());
    }
}
