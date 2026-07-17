//! Live read-only command implementations: status, show, and check.

use std::collections::BTreeSet;

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::github::{DiscoveryError, GitHubDiscovery};
use crate::graph::{CompatibilityChecker, GitCompatibilityChecker, GraphAnalysis, analyze};
use crate::model::{
    Caravan, CompatibilityOutcome, CompatibilityReport, GraphProblem, GraphProblemKind, PrNumber,
    PullRequestSnapshot, PullRequestState, RepositoryId,
};
use crate::{AppContext, AppError, CheckInput};

/// Repository-wide live Caravan status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusOutput {
    pub repository: RepositoryId,
    pub default_branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_pr: Option<PrNumber>,
    pub healthy: bool,
    pub analysis: GraphAnalysis,
}

/// Current PR's ordered caravan view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ShowOutput {
    pub repository: RepositoryId,
    pub current_pr: PrNumber,
    pub caravan: Caravan,
    /// Zero-based head-to-tail position.
    pub position: usize,
    pub pull_requests: Vec<PullRequestSnapshot>,
    pub healthy: bool,
    #[serde(default)]
    pub problems: Vec<GraphProblem>,
}

/// Which eligibility contract `cara check` evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckMode {
    ActiveCaravan,
    NewCaravan,
    JoinTail,
}

/// Successful read-only eligibility/health result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckOutput {
    pub mode: CheckMode,
    pub current_pr: PrNumber,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caravan_id: Option<PrNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_pr: Option<PrNumber>,
    pub eligible: bool,
    #[serde(default)]
    pub compatibility: Vec<CompatibilityReport>,
    #[serde(default)]
    pub problems: Vec<GraphProblem>,
}

/// Discover and validate the real current repository without mutation.
pub fn status(context: &AppContext) -> Result<StatusOutput, AppError> {
    let discovery = GitHubDiscovery::new(crate::command::ProcessRunner::in_directory(
        &context.repository_path,
    ));
    let snapshot = discovery
        .discover()
        .map_err(|error| discovery_error(&error))?;
    let checker = GitCompatibilityChecker::new(&context.repository_path, "origin");
    let analysis = analyze(&snapshot, &checker)?;
    Ok(StatusOutput {
        repository: snapshot.repository,
        default_branch: snapshot.default_branch.name,
        current_branch: snapshot.current_branch,
        current_pr: snapshot.current_pr,
        healthy: analysis.healthy(),
        analysis,
    })
}

/// Show the current branch's active caravan and position.
pub fn show(context: &AppContext) -> Result<ShowOutput, AppError> {
    let status = status(context)?;
    let current_pr = status.current_pr.ok_or_else(|| {
        AppError::validation(
            "current_pr_not_found",
            "the current branch has no unique open GitHub pull request",
        )
    })?;
    let caravan = status
        .analysis
        .fleet
        .containing(current_pr)
        .cloned()
        .ok_or_else(|| {
            AppError::validation(
                "current_pr_not_in_caravan",
                format!("PR #{current_pr} is not an active caravan member"),
            )
        })?;
    let position = caravan
        .position(current_pr)
        .expect("containing caravan includes current PR");
    let pull_requests = caravan
        .members
        .iter()
        .filter_map(|number| status.analysis.pull_requests.get(number).cloned())
        .collect();
    Ok(ShowOutput {
        repository: status.repository,
        current_pr,
        caravan,
        position,
        pull_requests,
        healthy: status.healthy,
        problems: status.analysis.fleet.problems,
    })
}

/// Check active health or proposed new/join eligibility without mutation.
pub fn check(context: &AppContext, input: &CheckInput) -> Result<CheckOutput, AppError> {
    if input.tail_pr.is_some() && input.head_pr.is_some() {
        return Err(AppError::validation(
            "ambiguous_target",
            "--tail-pr and --head-pr are mutually exclusive",
        ));
    }
    let status = status(context)?;
    let checker = GitCompatibilityChecker::new(&context.repository_path, "origin");
    check_analysis(&status, input, &checker)
}

/// Pure/injectable check policy used by live commands and fixture tests.
pub fn check_analysis(
    status: &StatusOutput,
    input: &CheckInput,
    checker: &impl CompatibilityChecker,
) -> Result<CheckOutput, AppError> {
    let current_pr = status.current_pr.ok_or_else(|| {
        AppError::validation(
            "current_pr_not_found",
            "the current branch has no unique open GitHub pull request",
        )
    })?;
    let pull_request = status
        .analysis
        .pull_requests
        .get(&current_pr)
        .ok_or_else(|| {
            AppError::validation(
                "current_pr_missing_from_snapshot",
                format!("PR #{current_pr} was not included in discovery"),
            )
        })?;

    if let Some(caravan) = status.analysis.fleet.containing(current_pr) {
        if input.tail_pr.is_some() || input.head_pr.is_some() {
            return Err(AppError::validation(
                "active_pr_cannot_join",
                format!("PR #{current_pr} is already in caravan #{}", caravan.id),
            ));
        }
        let output = CheckOutput {
            mode: CheckMode::ActiveCaravan,
            current_pr,
            caravan_id: Some(caravan.id),
            target_pr: None,
            eligible: status.healthy,
            compatibility: status.analysis.compatibility.clone(),
            problems: status.analysis.fleet.problems.clone(),
        };
        return eligible_or_error(output);
    }

    let mut problems = status.analysis.fleet.problems.clone();
    validate_candidate(pull_request, &mut problems);
    let mut reports = Vec::new();

    let explicit_join = input.tail_pr.is_some() || input.head_pr.is_some();
    if !explicit_join {
        check_new(status, pull_request, checker, &mut reports, &mut problems)?;
        return eligible_or_error(CheckOutput {
            mode: CheckMode::NewCaravan,
            current_pr,
            caravan_id: Some(current_pr),
            target_pr: None,
            eligible: problems.is_empty(),
            compatibility: reports,
            problems,
        });
    }

    let target_caravan = resolve_target_caravan(status, input)?;
    let tail_number = target_caravan.tail().expect("caravans are non-empty");
    let tail = status
        .analysis
        .pull_requests
        .get(&tail_number)
        .expect("derived tail has a snapshot");
    record_report(
        checker.check(&pull_request.head, &tail.head)?,
        vec![tail_number, current_pr],
        "candidate does not merge cleanly after the selected tail",
        &mut reports,
        &mut problems,
    );
    for caravan in &status.analysis.fleet.caravans {
        if caravan.id == target_caravan.id {
            continue;
        }
        let head_number = caravan.head().expect("caravans are non-empty");
        let head = status
            .analysis
            .pull_requests
            .get(&head_number)
            .expect("derived head has a snapshot");
        record_report(
            checker.check(&head.head, &pull_request.head)?,
            vec![head_number, current_pr],
            "another caravan head cannot attach after the proposed new tail",
            &mut reports,
            &mut problems,
        );
    }

    eligible_or_error(CheckOutput {
        mode: CheckMode::JoinTail,
        current_pr,
        caravan_id: Some(target_caravan.id),
        target_pr: Some(tail_number),
        eligible: problems.is_empty(),
        compatibility: reports,
        problems,
    })
}

fn check_new(
    status: &StatusOutput,
    pull_request: &PullRequestSnapshot,
    checker: &impl CompatibilityChecker,
    reports: &mut Vec<CompatibilityReport>,
    problems: &mut Vec<GraphProblem>,
) -> Result<(), AppError> {
    record_report(
        checker.check(&pull_request.head, &status.analysis.fleet.default_branch)?,
        vec![pull_request.number],
        "candidate new head does not merge cleanly into the default branch",
        reports,
        problems,
    );
    for caravan in &status.analysis.fleet.caravans {
        let head_number = caravan.head().expect("caravans are non-empty");
        let tail_number = caravan.tail().expect("caravans are non-empty");
        let head = status
            .analysis
            .pull_requests
            .get(&head_number)
            .expect("derived head has a snapshot");
        let tail = status
            .analysis
            .pull_requests
            .get(&tail_number)
            .expect("derived tail has a snapshot");
        record_report(
            checker.check(&pull_request.head, &tail.head)?,
            vec![pull_request.number, tail_number],
            "candidate head cannot attach after an existing caravan tail",
            reports,
            problems,
        );
        record_report(
            checker.check(&head.head, &pull_request.head)?,
            vec![head_number, pull_request.number],
            "existing caravan head cannot attach after the candidate tail",
            reports,
            problems,
        );
    }
    Ok(())
}

fn resolve_target_caravan<'a>(
    status: &'a StatusOutput,
    input: &CheckInput,
) -> Result<&'a Caravan, AppError> {
    if let Some(head) = input.head_pr.map(PrNumber) {
        return status.analysis.fleet.caravan(head).ok_or_else(|| {
            AppError::validation(
                "caravan_head_not_found",
                format!("PR #{head} is not a current caravan head"),
            )
        });
    }
    if let Some(tail) = input.tail_pr.map(PrNumber) {
        return status
            .analysis
            .fleet
            .caravans
            .iter()
            .find(|caravan| caravan.tail() == Some(tail))
            .ok_or_else(|| {
                AppError::validation(
                    "caravan_tail_not_found",
                    format!("PR #{tail} is not a current caravan tail"),
                )
            });
    }
    match status.analysis.fleet.caravans.as_slice() {
        [caravan] => Ok(caravan),
        [] => Err(AppError::validation(
            "caravan_tail_not_found",
            "there is no caravan to join; use `cara new`",
        )),
        caravans => Err(AppError::structured(
            ErrorCategory::Validation,
            "ambiguous_caravan_tail",
            "multiple caravan tails exist; pass --tail-pr or --head-pr",
            Some(json!({
                "candidate_tails": caravans.iter().filter_map(Caravan::tail).collect::<Vec<_>>(),
            })),
        )),
    }
}

fn validate_candidate(pull_request: &PullRequestSnapshot, problems: &mut Vec<GraphProblem>) {
    let mut messages = BTreeSet::new();
    if pull_request.state != PullRequestState::Open {
        messages.insert("candidate PR is not open");
    }
    if pull_request.draft {
        messages.insert("candidate PR is still a draft");
    }
    if pull_request.has_label("caravan") {
        messages.insert("candidate PR already has the caravan label");
    }
    if pull_request.has_label("caravan-evicted") {
        messages.insert("candidate PR is evicted; use renew or rejoin");
    }
    if pull_request.auto_merge.enabled {
        messages.insert("candidate PR already has auto-merge enabled");
    }
    if pull_request.cross_repository {
        messages.insert("candidate PR uses a fork-only head branch");
    }
    for message in messages {
        problems.push(GraphProblem {
            kind: GraphProblemKind::Unknown,
            prs: vec![pull_request.number],
            message: message.to_owned(),
        });
    }
}

fn record_report(
    report: CompatibilityReport,
    prs: Vec<PrNumber>,
    message: &str,
    reports: &mut Vec<CompatibilityReport>,
    problems: &mut Vec<GraphProblem>,
) {
    if report.outcome != CompatibilityOutcome::Clean {
        problems.push(GraphProblem {
            kind: GraphProblemKind::Incompatible,
            prs,
            message: message.to_owned(),
        });
    }
    reports.push(report);
}

fn eligible_or_error(output: CheckOutput) -> Result<CheckOutput, AppError> {
    if output.eligible {
        return Ok(output);
    }
    Err(AppError::structured(
        ErrorCategory::Validation,
        "check_failed",
        "the requested Caravan operation is not currently valid",
        Some(serde_json::to_value(&output).unwrap_or_else(|_| json!({}))),
    ))
}

fn discovery_error(error: &DiscoveryError) -> AppError {
    let category = match error {
        DiscoveryError::AmbiguousCurrentPullRequest { .. }
        | DiscoveryError::ForkOnlyHead { .. }
        | DiscoveryError::InvalidLimit(_)
        | DiscoveryError::InvalidRepositorySlug(_)
        | DiscoveryError::MissingDefaultBranch
        | DiscoveryError::MissingHeadRepository { .. } => ErrorCategory::Validation,
        DiscoveryError::Runner(_)
        | DiscoveryError::CommandFailed { .. }
        | DiscoveryError::InvalidJson { .. } => ErrorCategory::ExecutionFailure,
    };
    AppError::structured(
        category,
        "github_discovery_failed",
        error.to_string(),
        Some(json!({ "error": format!("{error:?}") })),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::model::{AutoMergeState, BranchSnapshot, CommitOid, PullRequestState};

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "harryaskham".to_owned(),
            name: "caravan".to_owned(),
        }
    }

    fn branch(name: &str) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository(),
            name: name.to_owned(),
            oid: CommitOid(format!("{name:0<40}")),
        }
    }

    fn pr(number: u64, head: &str, base: &str, active: bool) -> PullRequestSnapshot {
        PullRequestSnapshot {
            number: PrNumber(number),
            title: format!("PR {number}"),
            url: format!("https://example.invalid/{number}"),
            state: PullRequestState::Open,
            draft: false,
            head: branch(head),
            base: branch(base),
            cross_repository: false,
            labels: if active {
                BTreeSet::from(["caravan".to_owned()])
            } else {
                BTreeSet::new()
            },
            auto_merge: if active && base == "main" {
                AutoMergeState::squash()
            } else {
                AutoMergeState::disabled()
            },
            checks: Vec::new(),
            merged_at: None,
            updated_at: None,
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn status(current: PullRequestSnapshot, active: Vec<PullRequestSnapshot>) -> StatusOutput {
        let current_number = current.number;
        let mut pull_requests = active.clone();
        if !pull_requests
            .iter()
            .any(|pull_request| pull_request.number == current_number)
        {
            pull_requests.push(current);
        }
        let snapshot = crate::model::RepositorySnapshot {
            repository: repository(),
            default_branch: branch("main"),
            current_branch: Some("current".to_owned()),
            current_pr: Some(current_number),
            pull_requests,
            observed_at: None,
        };
        let checker = clean_checker;
        let analysis = analyze(&snapshot, &checker).unwrap();
        StatusOutput {
            repository: repository(),
            default_branch: "main".to_owned(),
            current_branch: snapshot.current_branch,
            current_pr: snapshot.current_pr,
            healthy: analysis.healthy(),
            analysis,
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn clean_checker(
        candidate: &crate::model::BranchSnapshot,
        target: &crate::model::BranchSnapshot,
    ) -> Result<CompatibilityReport, AppError> {
        Ok(CompatibilityReport {
            candidate: candidate.clone(),
            target: target.clone(),
            outcome: CompatibilityOutcome::Clean,
            conflicting_paths: Vec::new(),
            diagnostic: None,
        })
    }

    #[test]
    fn active_check_reports_whole_caravan_health() {
        let active = vec![pr(1, "one", "main", true), pr(2, "two", "one", true)];
        let status = status(active[1].clone(), active);
        let output = check_analysis(&status, &CheckInput::default(), &clean_checker).unwrap();
        assert_eq!(output.mode, CheckMode::ActiveCaravan);
        assert_eq!(output.caravan_id, Some(PrNumber(1)));
        assert!(output.eligible);
    }

    #[test]
    fn new_check_proves_both_cross_caravan_attachment_orders() {
        let candidate = pr(9, "nine", "main", false);
        let status = status(candidate, vec![pr(1, "one", "main", true)]);
        // Explicitly avoid unique-tail inference: with >1 caravans default check
        // is new; add a second caravan to exercise all ordered directions.
        let mut status = status;
        status
            .analysis
            .fleet
            .caravans
            .push(Caravan::new(vec![PrNumber(3)]).expect("second caravan"));
        status
            .analysis
            .pull_requests
            .insert(PrNumber(3), pr(3, "three", "main", true));
        let calls = std::cell::RefCell::new(Vec::new());
        let checker = |candidate: &crate::model::BranchSnapshot,
                       target: &crate::model::BranchSnapshot| {
            calls
                .borrow_mut()
                .push((candidate.name.clone(), target.name.clone()));
            clean_checker(candidate, target)
        };
        let output = check_analysis(&status, &CheckInput::default(), &checker).unwrap();
        assert_eq!(output.mode, CheckMode::NewCaravan);
        let calls = calls.into_inner();
        assert!(calls.contains(&("nine".to_owned(), "one".to_owned())));
        assert!(calls.contains(&("one".to_owned(), "nine".to_owned())));
        assert!(calls.contains(&("nine".to_owned(), "three".to_owned())));
        assert!(calls.contains(&("three".to_owned(), "nine".to_owned())));
    }

    #[test]
    fn explicit_head_resolves_join_tail() {
        let candidate = pr(9, "nine", "main", false);
        let status = status(
            candidate,
            vec![pr(1, "one", "main", true), pr(2, "two", "one", true)],
        );
        let output = check_analysis(
            &status,
            &CheckInput {
                tail_pr: None,
                head_pr: Some(1),
            },
            &clean_checker,
        )
        .unwrap();
        assert_eq!(output.mode, CheckMode::JoinTail);
        assert_eq!(output.caravan_id, Some(PrNumber(1)));
        assert_eq!(output.target_pr, Some(PrNumber(2)));
    }

    #[test]
    fn failed_compatibility_is_a_nonzero_check_error() {
        let candidate = pr(9, "nine", "main", false);
        let status = status(candidate, Vec::new());
        let conflict = |candidate: &crate::model::BranchSnapshot,
                        target: &crate::model::BranchSnapshot| {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: CompatibilityOutcome::Conflict,
                conflicting_paths: vec!["src/lib.rs".to_owned()],
                diagnostic: None,
            })
        };
        let error = check_analysis(&status, &CheckInput::default(), &conflict)
            .expect_err("conflict must fail check");
        assert_eq!(mcp_cli::StructuredError::code(&error), "check_failed");
    }

    #[test]
    fn draft_candidate_fails_before_mutation() {
        let mut candidate = pr(9, "nine", "main", false);
        candidate.draft = true;
        let status = status(candidate, Vec::new());
        let error = check_analysis(&status, &CheckInput::default(), &clean_checker)
            .expect_err("drafts are not ready");
        let details = mcp_cli::StructuredError::details(&error).unwrap();
        assert_eq!(details["eligible"], false);
    }

    #[test]
    fn helper_status_keeps_all_pull_requests() {
        let candidate = pr(9, "nine", "main", false);
        let status = status(candidate, vec![pr(1, "one", "main", true)]);
        let numbers: BTreeMap<_, _> = status
            .analysis
            .pull_requests
            .iter()
            .map(|(number, pull_request)| (*number, pull_request.title.clone()))
            .collect();
        assert_eq!(numbers.len(), 2);
    }
}
