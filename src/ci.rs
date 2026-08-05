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
const MAX_LINEAGE_LOG_JOBS: usize = 5;
const MAX_LINEAGE_LOG_BODY_BYTES: usize = 60 * 1024;
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

/// Strictly allowlisted machine receipt extracted from a bounded job-log range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SelectedRefLineageReceipt {
    pub event: String,
    pub head_ref: String,
    pub selected_ref: String,
    pub selected_commit: CommitOid,
    pub actual_head: CommitOid,
    pub expected_head: CommitOid,
    pub expected_base: CommitOid,
    #[serde(default)]
    pub parents: Vec<CommitOid>,
}

/// Result of the bounded lineage-receipt extraction attempt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LineageEvidenceStatus {
    #[default]
    NotRequested,
    Parsed,
    Missing,
    Truncated,
    Unavailable,
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
    /// Raw log text is never retained; only this strict receipt may escape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_lineage: Option<SelectedRefLineageReceipt>,
    #[serde(default)]
    pub lineage_evidence_status: LineageEvidenceStatus,
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
        let mut diagnostic = run.into_diagnostic(expected, jobs);
        enrich_lineage_receipts(runner, repository, &mut diagnostic);
        runs.push(diagnostic);
    }
    Ok(WorkflowFailureDiagnostics {
        requested_run_ids: requested_run_ids.into_iter().collect(),
        runs,
        runs_truncated,
    })
}

fn enrich_lineage_receipts(
    runner: &impl CommandRunner,
    repository: &RepositoryId,
    diagnostic: &mut WorkflowRunFailureDiagnostic,
) {
    let mut requested = 0;
    for job in &mut diagnostic.failed_jobs {
        let is_lineage_job = job.failed_steps.iter().any(|step| {
            step.name.to_ascii_lowercase().contains("lineage")
                || step.name.to_ascii_lowercase().contains("selected ref")
        });
        if !is_lineage_job {
            continue;
        }
        if requested >= MAX_LINEAGE_LOG_JOBS {
            job.lineage_evidence_status = LineageEvidenceStatus::Truncated;
            continue;
        }
        requested += 1;
        let command = workflow_job_log_command(repository, job.job_id);
        let Ok(output) = runner.run(&command) else {
            job.lineage_evidence_status = LineageEvidenceStatus::Unavailable;
            continue;
        };
        if !output.is_success() || !range_was_honored(&output.stdout) {
            job.lineage_evidence_status = LineageEvidenceStatus::Unavailable;
            continue;
        }
        job.selected_lineage = parse_selected_lineage_receipt(&output.stdout);
        job.lineage_evidence_status = if job.selected_lineage.is_some() {
            LineageEvidenceStatus::Parsed
        } else if response_range_is_truncated(&output.stdout) {
            LineageEvidenceStatus::Truncated
        } else {
            LineageEvidenceStatus::Missing
        };
    }
}

fn range_was_honored(output: &str) -> bool {
    output.lines().take(40).any(|line| {
        line.starts_with("HTTP/") && line.split_ascii_whitespace().nth(1) == Some("206")
    }) && output.lines().take(40).any(|line| {
        line.to_ascii_lowercase()
            .starts_with("content-range: bytes ")
    })
}

fn response_range_is_truncated(output: &str) -> bool {
    output.lines().take(40).any(|line| {
        let lower = line.to_ascii_lowercase();
        let Some(range) = lower.strip_prefix("content-range: bytes ") else {
            return false;
        };
        let Some((covered, total)) = range.trim().split_once('/') else {
            return false;
        };
        let Some((_, end)) = covered.split_once('-') else {
            return false;
        };
        end.parse::<usize>()
            .ok()
            .zip(total.parse::<usize>().ok())
            .is_some_and(|(end, total)| end.saturating_add(1) < total)
    }) || output.contains("...[truncated]")
}

fn parse_selected_lineage_receipt(output: &str) -> Option<SelectedRefLineageReceipt> {
    const MARKER: &str = "ci-selected-ref-receipt ";
    let body = output
        .lines()
        .find_map(|line| line.find(MARKER).map(|index| &line[index + MARKER.len()..]))?;
    let tokens = body.split_ascii_whitespace().collect::<Vec<_>>();
    let value = |key: &str| {
        tokens.iter().find_map(|token| {
            token
                .split_once('=')
                .filter(|(candidate, _)| *candidate == key)
                .map(|(_, value)| value)
        })
    };
    let parents_index = tokens
        .iter()
        .position(|token| token.starts_with("parents="))?;
    let first_parent = tokens[parents_index].split_once('=')?.1;
    let mut parents = vec![parse_oid(first_parent)?];
    for token in tokens.iter().skip(parents_index + 1).take(3) {
        if token.contains('=') {
            break;
        }
        parents.push(parse_oid(token)?);
    }
    if parents.len() != 2 {
        return None;
    }
    Some(SelectedRefLineageReceipt {
        event: parse_safe_text(value("event")?, 64)?,
        head_ref: parse_safe_text(value("head_ref")?, 512)?,
        selected_ref: parse_safe_text(value("selected_ref")?, 512)?,
        selected_commit: parse_oid(value("selected_commit")?)?,
        actual_head: parse_oid(value("actual_head")?)?,
        expected_head: parse_oid(value("expected_head")?)?,
        expected_base: parse_oid(value("expected_base")?)?,
        parents,
    })
}

fn parse_oid(value: &str) -> Option<CommitOid> {
    ((40..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| CommitOid(value.to_ascii_lowercase()))
}

fn parse_safe_text(value: &str, max_len: usize) -> Option<String> {
    (!value.is_empty()
        && value.len() <= max_len
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b':')
        }))
    .then(|| value.to_owned())
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

fn workflow_job_log_command(repository: &RepositoryId, job_id: u64) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        "--include".to_owned(),
        "-H".to_owned(),
        format!("Range: bytes=0-{}", MAX_LINEAGE_LOG_BODY_BYTES - 1),
        format!("repos/{}/actions/jobs/{job_id}/logs", repository.slug()),
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
            selected_lineage: None,
            lineage_evidence_status: LineageEvidenceStatus::NotRequested,
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
            merged_at: None,
            head_oid: CommitOid("current-head".to_owned()),
            base_ref: "main".to_owned(),
            base_oid: CommitOid("current-base".to_owned()),
            labels: BTreeSet::from(["caravan".to_owned()]),
            checks: Vec::new(),
            auto_merge: AutoMergeState::disabled(),
        }
    }

    fn bounded_lineage_log() -> String {
        format!(
            "HTTP/1.1 206 Partial Content\nContent-Range: bytes 0-2047/2048\n\nTOKEN=do-not-expose\n2026-07-18T13:53:23Z ci-selected-ref-receipt event=pull_request head_ref=feature selected_ref={selected} selected_commit={selected} actual_head={selected} expected_head={head} expected_base={base} parents={base} {prior}\n",
            selected = "a".repeat(40),
            head = "b".repeat(40),
            base = "c".repeat(40),
            prior = "d".repeat(40),
        )
    }

    #[test]
    fn diagnostics_are_structured_bounded_and_never_fetch_raw_logs() {
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
                        {"number": 2, "name": "Verify exact CI ref lineage", "status": "completed", "conclusion": "failure"}
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
            (
                workflow_job_log_command(&repository, 101),
                CommandOutput::success(bounded_lineage_log()),
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
        let job = &diagnostic.failed_jobs[0];
        assert_eq!(job.lineage_evidence_status, LineageEvidenceStatus::Parsed);
        assert_eq!(
            job.selected_lineage
                .as_ref()
                .map(|receipt| &receipt.selected_commit),
            Some(&CommitOid("a".repeat(40)))
        );
        let serialized = serde_json::to_string(diagnostic).unwrap();
        assert!(!serialized.contains("do-not-expose"));
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
