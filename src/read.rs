//! Live read-only command implementations: status, show, and check.

use std::collections::BTreeSet;

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::command::CommandRunError;
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
    /// Successful phase timings make large-repository regressions diagnosable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<StatusTiming>,
    pub repository: RepositoryId,
    pub default_branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_pr: Option<PrNumber>,
    pub healthy: bool,
    /// Read-only first-use readiness. Status never creates or edits resources.
    pub initialization: crate::initialization::InitializationStatus,
    pub analysis: GraphAnalysis,
    /// Canonical, nonmutating automatic-admission order derived from GitHub.
    pub admission: AdmissionStatus,
}

/// Timing evidence for one complete read-only status operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusTiming {
    pub deadline_ms: u64,
    pub total_ms: u64,
    pub phases_ms: std::collections::BTreeMap<String, u64>,
}

/// One selectable PR in canonical priority-then-FIFO order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdmissionCandidate {
    pub pr: PrNumber,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_rank: Option<usize>,
    /// Immutable GitHub creation timestamp used as the FIFO key. Legacy or
    /// synthetic snapshots may omit it and deterministically fall back to PR number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    pub reason: String,
}

/// One ready-looking PR excluded from automation because priority metadata is unsafe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RejectedAdmissionCandidate {
    pub pr: PrNumber,
    pub reason: String,
}

/// Resolved GitHub-visible automatic-admission policy and result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdmissionStatus {
    pub policy: String,
    pub priority_labels: Vec<String>,
    /// Ordered highest priority first, then immutable provider creation time.
    pub candidates: Vec<AdmissionCandidate>,
    #[serde(default)]
    pub rejected: Vec<RejectedAdmissionCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_candidate: Option<PrNumber>,
}

/// Dedicated read-only result for deterministic admission coordination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NextCandidateOutput {
    pub repository: RepositoryId,
    /// Ordering is selection-only: the chosen PR must still pass `check`/`new`
    /// preflight and a failure must not cause an automatic leapfrog.
    pub attempt_contract: String,
    pub admission: AdmissionStatus,
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
    pub initialization: crate::initialization::InitializationStatus,
}

/// Discover and validate the real current repository without mutation.
pub fn status(context: &AppContext) -> Result<StatusOutput, AppError> {
    let budget = std::time::Duration::from_secs(context.config.command_timeout_secs);
    status_with_deadline(context, std::time::Instant::now() + budget)
}

/// Run status under a caller-supplied absolute deadline. This narrow seam lets
/// orchestration share a future whole-operation budget without changing child APIs.
#[allow(clippy::too_many_lines)]
pub(crate) fn status_with_deadline(
    context: &AppContext,
    operation_deadline: std::time::Instant,
) -> Result<StatusOutput, AppError> {
    // Sharing one absolute deadline prevents a large repository from
    // multiplying its budget by provider and compatibility subprocess count.
    let started = std::time::Instant::now();
    let operation_budget = operation_deadline.saturating_duration_since(started);
    let child_timeout = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let discovery = GitHubDiscovery::new(
        crate::command::ProcessRunner::in_directory(&context.repository_path)
            .with_timeout(child_timeout)
            .with_operation_deadline(operation_deadline),
    );
    let snapshot = discovery.discover().map_err(|error| {
        if let DiscoveryError::Runner(CommandRunError::Timeout { command, .. }) = &error {
            discovery_timeout_error(
                &error,
                discovery_phase(command),
                started.elapsed(),
                operation_budget,
            )
        } else {
            discovery_error(&error)
        }
    })?;
    let discovery_elapsed = started.elapsed();
    let checker = GitCompatibilityChecker::new(&context.repository_path, "origin")
        .with_timeout(child_timeout)
        .with_operation_deadline(operation_deadline);
    let analysis = analyze(&snapshot, &checker).map_err(|error| {
        if mcp_cli::StructuredError::category(&error) == ErrorCategory::Timeout {
            AppError::structured(
                ErrorCategory::Timeout,
                "github_discovery_timeout",
                "compatibility analysis exceeded the status deadline",
                Some(json!({
                    "stage": "github_discovery",
                    "phase": "compatibility_analysis",
                    "elapsed_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "deadline_ms": u64::try_from(operation_budget.as_millis()).unwrap_or(u64::MAX),
                    "retryable": true,
                    "safe_next_action": "retry `cara status` after restoring Git transport health; status made no mutations",
                    "source": mcp_cli::StructuredError::details(&error),
                })),
            )
        } else {
            error
        }
    })?;
    let analysis_elapsed = started.elapsed();
    let label_provider = crate::github::GitHubMutationAdapter::new(
        crate::command::ProcessRunner::in_directory(&context.repository_path)
            .with_timeout(child_timeout)
            .with_operation_deadline(operation_deadline),
    );
    let labels = label_provider
        .repository_label_definitions(&snapshot.repository)
        .map_err(|error| {
            if let crate::github::MutationError::Provider(provider) = &error {
                if matches!(
                    provider,
                    DiscoveryError::Runner(CommandRunError::Timeout { .. })
                ) {
                    return discovery_timeout_error(
                        provider,
                        "repository_label_inventory",
                        started.elapsed(),
                        operation_budget,
                    );
                }
            }
            AppError::structured(
                mcp_cli::ErrorCategory::ExecutionFailure,
                "repository_initialization_inventory_failed",
                error.to_string(),
                Some(json!({"next": "repair GitHub read access and rerun `cara status`"})),
            )
        })?;
    let mut initialization =
        crate::initialization::inspect_labels(&labels, &context.config.agent_priority_labels);
    if !context.config_existed {
        initialization.ready = false;
        initialization.next = Some("run `cara init` to atomically create .caravan/config.yaml and verify repository readiness".to_owned());
    }
    let admission = resolve_admission(&analysis, &context.config.agent_priority_labels);
    let total = started.elapsed();
    if std::time::Instant::now() >= operation_deadline {
        return Err(AppError::structured(
            ErrorCategory::Timeout,
            "github_discovery_timeout",
            "status deadline expired after repository label inventory",
            Some(json!({
                "stage": "github_discovery",
                "phase": "finalize_status",
                "elapsed_ms": u64::try_from(total.as_millis()).unwrap_or(u64::MAX),
                "deadline_ms": u64::try_from(operation_budget.as_millis()).unwrap_or(u64::MAX),
                "retryable": true,
                "safe_next_action": "retry `cara status`; status made no mutations",
            })),
        ));
    }
    let millis =
        |duration: std::time::Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    let timing = StatusTiming {
        deadline_ms: millis(operation_budget),
        total_ms: millis(total),
        phases_ms: std::collections::BTreeMap::from([
            ("github_discovery".to_owned(), millis(discovery_elapsed)),
            (
                "compatibility_analysis".to_owned(),
                millis(analysis_elapsed.saturating_sub(discovery_elapsed)),
            ),
            (
                "repository_label_inventory".to_owned(),
                millis(total.saturating_sub(analysis_elapsed)),
            ),
        ]),
    };
    Ok(StatusOutput {
        timing: Some(timing),
        repository: snapshot.repository,
        default_branch: snapshot.default_branch.name,
        current_branch: snapshot.current_branch,
        current_pr: snapshot.current_pr,
        healthy: analysis.healthy() && initialization.ready,
        initialization,
        analysis,
        admission,
    })
}

/// Return the canonical first automatic-admission candidate without mutation.
pub fn next_candidate(context: &AppContext) -> Result<NextCandidateOutput, AppError> {
    let status = status(context)?;
    Ok(NextCandidateOutput {
        repository: status.repository,
        attempt_contract: "ordered admission attempt only; run check/new preflight for the first candidate; on rejection fail closed and retry after GitHub state changes rather than leapfrogging".to_owned(),
        admission: status.admission,
    })
}

/// Resolve configured explicit priority and FIFO from one GitHub snapshot.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn resolve_admission(analysis: &GraphAnalysis, priority_labels: &[String]) -> AdmissionStatus {
    let ranks: std::collections::BTreeMap<&str, usize> = priority_labels
        .iter()
        .enumerate()
        .map(|(rank, label)| (label.as_str(), rank))
        .collect();
    let mut candidates = Vec::new();
    let mut rejected = Vec::new();

    for number in &analysis.fleet.unqueued {
        let Some(pull_request) = analysis.pull_requests.get(number) else {
            continue;
        };
        let priority_namespace: Vec<&String> = pull_request
            .labels
            .iter()
            .filter(|label| label.starts_with("caravan-priority:"))
            .collect();
        let configured: Vec<(&String, usize)> = priority_namespace
            .iter()
            .filter_map(|label| ranks.get(label.as_str()).map(|rank| (*label, *rank)))
            .collect();
        let invalid: Vec<&String> = priority_namespace
            .iter()
            .copied()
            .filter(|label| !ranks.contains_key(label.as_str()))
            .collect();

        let rejection = if !invalid.is_empty() {
            Some(format!(
                "fail closed: unknown priority label(s): {}",
                invalid
                    .iter()
                    .map(|label| label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        } else if configured.len() > 1 {
            Some(format!(
                "fail closed: conflicting priority labels: {}",
                configured
                    .iter()
                    .map(|(label, _)| label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        } else if pull_request.cross_repository {
            Some("fail closed: fork-only PR cannot be admitted to a caravan".to_owned())
        } else if pull_request.auto_merge.enabled {
            Some("fail closed: candidate already has auto-merge enabled".to_owned())
        } else {
            None
        };
        if let Some(reason) = rejection {
            rejected.push(RejectedAdmissionCandidate {
                pr: *number,
                reason,
            });
            continue;
        }

        let created_at = pull_request.created_at.clone();
        let fifo_reason = created_at.as_ref().map_or_else(
            || format!("provider created_at missing; deterministic PR number #{number} fallback"),
            |created_at| {
                format!("immutable provider created_at {created_at}, PR number #{number} tie-break")
            },
        );
        let (priority_label, priority_rank, reason) = configured.first().map_or_else(
            || {
                (
                    None,
                    None,
                    format!(
                        "no explicit agent priority; FIFO by {fifo_reason}; selection only, check/new preflight required"
                    ),
                )
            },
            |(label, rank)| {
                (
                    Some((*label).clone()),
                    Some(rank + 1),
                    format!(
                        "explicit agent priority `{label}` (rank {}); FIFO by {fifo_reason} within this priority; selection only, check/new preflight required",
                        rank + 1
                    ),
                )
            },
        );
        candidates.push(AdmissionCandidate {
            pr: *number,
            priority_label,
            priority_rank,
            created_at,
            reason,
        });
    }

    // An absent explicit priority sorts after every configured rank. GitHub's
    // immutable creation timestamp is FIFO; PR number deterministically breaks
    // equal timestamps. Missing timestamps form a deterministic fallback group
    // after provider-timestamped candidates, ordered by PR number.
    candidates.sort_by_key(|candidate| {
        (
            candidate.priority_rank.unwrap_or(priority_labels.len() + 1),
            candidate.created_at.is_none(),
            candidate.created_at.clone().unwrap_or_default(),
            candidate.pr,
        )
    });
    rejected.sort_by_key(|candidate| candidate.pr);
    let next_candidate = candidates.first().map(|candidate| candidate.pr);
    AdmissionStatus {
        policy: "ordered admission attempts: explicit agent priority label (configured high to low), then FIFO by immutable provider created_at ascending with PR number ascending as equal-time tie-break; missing created_at falls back deterministically to PR number after timestamped peers; never LIFO; check/new preflight required and rejection never causes automatic leapfrogging".to_owned(),
        priority_labels: priority_labels.to_vec(),
        candidates,
        rejected,
        next_candidate,
    }
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
    let checker = GitCompatibilityChecker::new(&context.repository_path, "origin").with_timeout(
        std::time::Duration::from_secs(context.config.command_timeout_secs),
    );
    check_analysis(&status, input, &checker)
}

/// Pure/injectable check policy used by live commands and fixture tests.
pub fn check_analysis(
    status: &StatusOutput,
    input: &CheckInput,
    checker: &impl CompatibilityChecker,
) -> Result<CheckOutput, AppError> {
    crate::initialization::require_ready(&status.initialization)?;
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
            initialization: status.initialization.clone(),
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
            initialization: status.initialization.clone(),
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
        initialization: status.initialization.clone(),
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
    if let DiscoveryError::Runner(CommandRunError::Timeout {
        command,
        timeout_ms,
        ..
    }) = error
    {
        return discovery_timeout_error(
            error,
            discovery_phase(command),
            std::time::Duration::from_millis(*timeout_ms),
            std::time::Duration::from_millis(*timeout_ms),
        );
    }
    if let DiscoveryError::InvalidJson {
        command,
        message,
        evidence,
    } = error
    {
        return AppError::structured(
            ErrorCategory::ExecutionFailure,
            "github_discovery_failed",
            error.to_string(),
            Some(json!({
                "stage": "github_json_decode",
                "command": command.display(),
                "message": message,
                "stdout": evidence.stdout,
                "stderr": evidence.stderr,
                "streams_combined": false,
                "resumable": true,
                "next": "inspect the separate stdout/stderr excerpts, repair malformed provider stdout, and retry",
            })),
        );
    }
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

fn discovery_phase(command: &crate::command::CommandSpec) -> &'static str {
    let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
    if command.program == "git" {
        "current_branch"
    } else if args.starts_with(&["repo", "view"]) {
        "repository_identity"
    } else if args.starts_with(&["api"]) {
        "default_branch_revision"
    } else if args.contains(&"--head") {
        "current_pull_request"
    } else if args.windows(2).any(|pair| pair == ["--state", "merged"]) {
        "historical_caravan_members"
    } else if args.contains(&"--label") {
        "active_caravan_members"
    } else {
        "open_pull_requests_and_checks"
    }
}

fn discovery_timeout_error(
    error: &DiscoveryError,
    phase: &str,
    elapsed: std::time::Duration,
    deadline: std::time::Duration,
) -> AppError {
    let (command, stdout, stderr) = match error {
        DiscoveryError::Runner(CommandRunError::Timeout {
            command,
            stdout,
            stderr,
            ..
        }) => (command.display(), stdout.as_str(), stderr.as_str()),
        _ => ("unknown".to_owned(), "", ""),
    };
    AppError::structured(
        ErrorCategory::Timeout,
        "github_discovery_timeout",
        format!("GitHub discovery phase `{phase}` exceeded the status deadline"),
        Some(json!({
            "stage": "github_discovery",
            "phase": phase,
            "command": command,
            "elapsed_ms": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            "deadline_ms": u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
            "timeout_ms": u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
            "stdout": stdout,
            "stderr": stderr,
            "retryable": true,
            "resumable": true,
            "safe_next_action": "retry `cara status` after restoring Git/GitHub transport health; status made no mutations",
            "next": "retry `cara status` after restoring Git/GitHub transport health; status made no mutations",
        })),
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
            created_at: Some(format!("2026-01-01T00:00:{number:02}Z")),
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
            timing: None,
            repository: repository(),
            default_branch: "main".to_owned(),
            current_branch: snapshot.current_branch,
            current_pr: snapshot.current_pr,
            healthy: analysis.healthy(),
            initialization: crate::initialization::InitializationStatus::default(),
            admission: resolve_admission(
                &analysis,
                &crate::config::CaravanConfig::default().agent_priority_labels,
            ),
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
    fn invalid_provider_json_preserves_separate_stream_evidence() {
        let error = discovery_error(&DiscoveryError::InvalidJson {
            command: crate::command::CommandSpec::new("gh").args(["pr", "list"]),
            message: "control character at line 1".to_owned(),
            evidence: Box::new(crate::github::JsonDecodeEvidence {
                stdout: "{\"bad\":\"\u{1}\"}".to_owned(),
                stderr: "wrapper diagnostic\u{1}".to_owned(),
            }),
        });

        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "github_discovery_failed"
        );
        let details = mcp_cli::StructuredError::details(&error).unwrap();
        assert_eq!(details["stage"], "github_json_decode");
        assert_eq!(details["streams_combined"], false);
        assert_eq!(details["stderr"], "wrapper diagnostic\u{1}");
        assert!(details["stdout"].as_str().unwrap().contains("bad"));
    }

    #[test]
    fn discovery_timeout_preserves_timeout_category_and_evidence() {
        let error = discovery_error(&DiscoveryError::Runner(CommandRunError::Timeout {
            command: crate::command::CommandSpec::new("gh").args(["pr", "list"]),
            timeout_ms: 500,
            stdout: "partial".to_owned(),
            stderr: "stalled".to_owned(),
        }));

        assert_eq!(
            mcp_cli::StructuredError::category(&error),
            ErrorCategory::Timeout
        );
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "github_discovery_timeout"
        );
        let details = mcp_cli::StructuredError::details(&error).unwrap();
        assert_eq!(details["stage"], "github_discovery");
        assert_eq!(details["phase"], "open_pull_requests_and_checks");
        assert_eq!(details["timeout_ms"], 500);
        assert_eq!(details["stdout"], "partial");
        assert_eq!(details["retryable"], true);
        assert!(
            details["safe_next_action"]
                .as_str()
                .unwrap()
                .contains("no mutations")
        );
    }

    #[test]
    fn status_deadline_error_reports_total_elapsed_and_phase() {
        let provider = DiscoveryError::Runner(CommandRunError::Timeout {
            command: crate::command::CommandSpec::new("gh").args(["pr", "list"]),
            timeout_ms: 250,
            stdout: String::new(),
            stderr: "stalled".to_owned(),
        });
        let error = discovery_timeout_error(
            &provider,
            "compatibility_prepare",
            std::time::Duration::from_millis(875),
            std::time::Duration::from_secs(1),
        );
        let details = mcp_cli::StructuredError::details(&error).unwrap();
        assert_eq!(details["phase"], "compatibility_prepare");
        assert_eq!(details["elapsed_ms"], 875);
        assert_eq!(details["deadline_ms"], 1_000);
    }

    #[test]
    fn admission_is_fifo_for_equal_and_absent_priority() {
        let mut older = pr(20, "older", "main", false);
        older.created_at = Some("2026-01-01T00:00:01Z".to_owned());
        older.labels.insert("caravan-priority:normal".to_owned());
        let mut newer = pr(10, "newer", "main", false);
        newer.created_at = Some("2026-01-01T00:00:02Z".to_owned());
        newer.labels.insert("caravan-priority:normal".to_owned());
        let no_priority = pr(5, "unprioritized", "main", false);
        let status = status(older, vec![newer, no_priority]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;
        let admission = resolve_admission(&status.analysis, &labels);
        assert_eq!(
            admission
                .candidates
                .iter()
                .map(|candidate| candidate.pr)
                .collect::<Vec<_>>(),
            [PrNumber(20), PrNumber(10), PrNumber(5)]
        );
        assert_eq!(admission.next_candidate, Some(PrNumber(20)));
        assert!(admission.candidates[0].reason.contains("FIFO"));
        assert!(
            admission.candidates[0]
                .reason
                .contains("preflight required")
        );
        assert!(admission.policy.contains("never LIFO"));
        assert!(
            admission
                .policy
                .contains("never causes automatic leapfrogging")
        );
    }

    #[test]
    fn equal_and_missing_created_at_use_pr_number_deterministically() {
        let mut equal_high = pr(20, "equal-high", "main", false);
        equal_high.created_at = Some("2026-01-01T00:00:01Z".to_owned());
        let mut equal_low = pr(10, "equal-low", "main", false);
        equal_low.created_at = Some("2026-01-01T00:00:01Z".to_owned());
        let mut missing_high = pr(40, "missing-high", "main", false);
        missing_high.created_at = None;
        let mut missing_low = pr(30, "missing-low", "main", false);
        missing_low.created_at = None;
        let status = status(equal_high, vec![missing_high, equal_low, missing_low]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;
        let admission = resolve_admission(&status.analysis, &labels);
        assert_eq!(
            admission
                .candidates
                .iter()
                .map(|candidate| candidate.pr)
                .collect::<Vec<_>>(),
            [PrNumber(10), PrNumber(20), PrNumber(30), PrNumber(40)]
        );
        assert!(admission.candidates[2].reason.contains("fallback"));
    }

    #[test]
    fn explicit_priority_deliberately_overrides_fifo() {
        let older = pr(10, "older", "main", false);
        let mut newer = pr(20, "newer", "main", false);
        newer.labels.insert("caravan-priority:high".to_owned());
        let status = status(older, vec![newer]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;
        let admission = resolve_admission(&status.analysis, &labels);
        assert_eq!(admission.next_candidate, Some(PrNumber(20)));
        assert_eq!(admission.candidates[0].priority_rank, Some(1));
    }

    #[test]
    fn invalid_and_conflicting_priority_labels_fail_closed() {
        let mut unknown = pr(10, "unknown", "main", false);
        unknown
            .labels
            .insert("caravan-priority:surprise".to_owned());
        let mut conflicting = pr(20, "conflicting", "main", false);
        conflicting.labels.extend([
            "caravan-priority:high".to_owned(),
            "caravan-priority:low".to_owned(),
        ]);
        let safe = pr(30, "safe", "main", false);
        let status = status(unknown, vec![conflicting, safe]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;
        let admission = resolve_admission(&status.analysis, &labels);
        assert_eq!(admission.next_candidate, Some(PrNumber(30)));
        assert_eq!(admission.rejected.len(), 2);
        assert!(
            admission
                .rejected
                .iter()
                .all(|candidate| candidate.reason.contains("fail closed"))
        );
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
