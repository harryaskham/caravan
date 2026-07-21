//! Live read-only command implementations: status, show, and check.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use mcp_cli::{ErrorCategory, StructuredError};
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

/// Explicit repository policy and rollout action for physical chain rebuilding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RebaseOnJoinStatus {
    pub enabled: bool,
    pub state: String,
    pub config_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_action: Option<String>,
}

impl Default for RebaseOnJoinStatus {
    fn default() -> Self {
        Self {
            enabled: false,
            state: "disabled".to_owned(),
            config_path: crate::config::DEFAULT_CONFIG_PATH.to_owned(),
            required_action: Some(format!(
                "set `rebase_on_join: true` in {} after canary review",
                crate::config::DEFAULT_CONFIG_PATH
            )),
        }
    }
}

fn rebase_on_join_status(context: &AppContext) -> RebaseOnJoinStatus {
    let config_path = context.config_path.display().to_string();
    RebaseOnJoinStatus {
        enabled: context.config.rebase_on_join,
        state: if context.config.rebase_on_join {
            "enabled"
        } else {
            "disabled"
        }
        .to_owned(),
        required_action: (!context.config.rebase_on_join).then(|| {
            format!(
                "set `rebase_on_join: true` in {config_path}, commit it, then run `cara check` and `cara sync --all`"
            )
        }),
        config_path,
    }
}

/// Repository-wide live Caravan status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusOutput {
    /// Authenticated provider-call counts and latest rate-limit evidence.
    #[serde(default)]
    pub provider_api: crate::model::GitHubApiTelemetry,
    /// Bounded, secret-free provider identity and lineage for active members.
    #[serde(default)]
    pub merge_candidates: Vec<crate::model::MergeCandidateIdentity>,
    #[serde(default)]
    pub merge_candidates_truncated: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_default_oid: Option<crate::model::CommitOid>,
    #[serde(default)]
    pub default_branch_movements: Vec<crate::model::DefaultBranchMovement>,
    /// Successful phase timings make large-repository regressions diagnosable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<StatusTiming>,
    pub repository: RepositoryId,
    /// Explicitly reports enabled/disabled physical chain-rebuild policy.
    #[serde(default)]
    pub rebase_on_join: RebaseOnJoinStatus,
    /// Exact sync-owned automatic-admission policy and safety bounds.
    #[serde(default)]
    pub auto_admission: AutoAdmissionStatus,
    pub default_branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_pr: Option<PrNumber>,
    pub healthy: bool,
    /// Read-only first-use readiness. Status never creates or edits resources.
    pub initialization: crate::initialization::InitializationStatus,
    pub analysis: GraphAnalysis,
    /// Explicit caravan holds. Active and expired holds intentionally suspend
    /// only their exact head auto-merge invariant; stale holds fail closed.
    #[serde(default)]
    pub pauses: Vec<crate::pause::PauseStatus>,
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
pub struct AutoAdmissionStatus {
    pub enabled: bool,
    pub heuristic_version: String,
    pub max_candidates_per_tick: u32,
    pub max_mutations_per_tick: u32,
    pub max_github_requests_per_tick: u32,
    pub max_duration_secs: u64,
}

impl Default for AutoAdmissionStatus {
    fn default() -> Self {
        Self {
            enabled: false,
            heuristic_version: crate::sync::AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
            max_candidates_per_tick: 0,
            max_mutations_per_tick: 0,
            max_github_requests_per_tick: 0,
            max_duration_secs: 0,
        }
    }
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
pub struct SkippedAdmissionCandidate {
    pub pr: PrNumber,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_rank: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    pub reason: String,
}

/// One ready-looking PR excluded from automation because priority metadata is unsafe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RejectedAdmissionCandidate {
    pub pr: PrNumber,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_rank: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    pub reason: String,
}

/// Resolved GitHub-visible automatic-admission policy and result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdmissionStatus {
    pub policy: String,
    pub priority_labels: Vec<String>,
    /// Ordered highest priority first, then immutable provider creation time.
    pub candidates: Vec<AdmissionCandidate>,
    /// Exact generations carrying the sync-owned best-effort skip label.
    #[serde(default)]
    pub skipped: Vec<SkippedAdmissionCandidate>,
    #[serde(default)]
    pub rejected: Vec<RejectedAdmissionCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_candidate: Option<PrNumber>,
}

/// Dedicated read-only result for deterministic admission coordination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NextCandidateOutput {
    #[serde(default)]
    pub provider_api: crate::model::GitHubApiTelemetry,
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
    /// Merged PR named by the checked-out branch, when rolling context was recovered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub historical_predecessor: Option<PrNumber>,
    /// Exact bounded-history facts for the merged branch-local predecessor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub historical_pull_request: Option<PullRequestSnapshot>,
    /// Effective active successor used for chain position and navigation.
    pub current_pr: PrNumber,
    pub caravan: Caravan,
    /// Zero-based head-to-tail position.
    pub position: usize,
    pub pull_requests: Vec<PullRequestSnapshot>,
    /// Exact synthetic lineage for these caravan members.
    #[serde(default)]
    pub merge_candidates: Vec<crate::model::MergeCandidateIdentity>,
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

/// Mechanical continuation after an exact read-only candidate preflight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CandidateNextAction {
    New,
    Join,
    Repair,
    Wait,
    Reject,
}

/// Successful read-only eligibility/health result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckOutput {
    #[serde(default)]
    pub provider_api: crate::model::GitHubApiTelemetry,
    #[serde(default)]
    pub rebase_on_join: RebaseOnJoinStatus,
    pub mode: CheckMode,
    /// Candidate PR (named `current_pr` for backwards-compatible JSON).
    pub current_pr: PrNumber,
    /// Exact provider candidate facts used by this receipt.
    pub candidate: PullRequestSnapshot,
    /// Provider-visible head repository owner (branch ownership, not PR authorship).
    pub head_repository_owner: String,
    /// Canonical provider merge-candidate identity and freshness from status/show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_candidate: Option<crate::model::MergeCandidateIdentity>,
    /// Whether the candidate was already in the discovered active fleet.
    pub enrolled: bool,
    /// Whether this is the canonical first priority/FIFO admission attempt.
    pub canonical_candidate: bool,
    pub next_action: CandidateNextAction,
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

struct CachedStatus {
    inserted: Instant,
    output: StatusOutput,
}

static STATUS_CACHE: OnceLock<Mutex<HashMap<String, CachedStatus>>> = OnceLock::new();
const MAX_STATUS_CACHE_ENTRIES: usize = 64;
const STATUS_CACHE_RETENTION: Duration = Duration::from_secs(3_600);

struct CachedLabels {
    inserted: Instant,
    labels: Vec<crate::github::RepositoryLabel>,
}

static LABEL_CACHE: OnceLock<Mutex<HashMap<String, CachedLabels>>> = OnceLock::new();
const LABEL_CACHE_MAX_AGE: Duration = Duration::from_secs(600);

fn status_cache_key(context: &AppContext) -> String {
    let config = serde_json::to_vec(&context.config).unwrap_or_default();
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in config {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{}:{hash:016x}", context.repository_path.display())
}

/// Return a short-lived read-only status snapshot for polling surfaces.
///
/// Mutating operations never call this function: exact preflight and provider
/// rereads remain authoritative. The cache only coalesces duplicate dashboard
/// or controller refreshes inside one long-lived Cara process.
pub(crate) fn status_cached(
    context: &AppContext,
    max_age: Duration,
) -> Result<StatusOutput, AppError> {
    let key = status_cache_key(context);
    let cache = STATUS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let guard = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = guard.get(&key) {
            let age = cached.inserted.elapsed();
            if age <= max_age {
                let mut output = cached.output.clone();
                output.provider_api.cache_hits = output.provider_api.cache_hits.saturating_add(1);
                output.provider_api.cache_age_ms =
                    Some(u64::try_from(age.as_millis()).unwrap_or(u64::MAX));
                return Ok(output);
            }
        }
    }
    let output = status(context)?;
    let mut guard = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.retain(|_, cached| cached.inserted.elapsed() <= STATUS_CACHE_RETENTION);
    if guard.len() >= MAX_STATUS_CACHE_ENTRIES {
        if let Some(oldest) = guard
            .iter()
            .min_by_key(|(_, cached)| cached.inserted)
            .map(|(key, _)| key.clone())
        {
            guard.remove(&oldest);
        }
    }
    guard.insert(
        key,
        CachedStatus {
            inserted: Instant::now(),
            output: output.clone(),
        },
    );
    Ok(output)
}

/// Invalidate one repository's read-only polling cache after an explicit action.
pub(crate) fn invalidate_status_cache(context: &AppContext) {
    if let Some(cache) = STATUS_CACHE.get() {
        cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&status_cache_key(context));
    }
}

fn repository_labels_cached<R: crate::command::CommandRunner>(
    provider: &crate::github::GitHubMutationAdapter<R>,
    repository: &RepositoryId,
) -> Result<(Vec<crate::github::RepositoryLabel>, bool), crate::github::MutationError> {
    let key = repository.slug();
    let cache = LABEL_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let guard = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = guard.get(&key)
            && cached.inserted.elapsed() <= LABEL_CACHE_MAX_AGE
        {
            return Ok((cached.labels.clone(), true));
        }
    }
    let labels = provider.repository_label_definitions(repository)?;
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            key,
            CachedLabels {
                inserted: Instant::now(),
                labels: labels.clone(),
            },
        );
    Ok((labels, false))
}

fn attach_provider_api(
    error: &AppError,
    provider_api: &crate::model::GitHubApiTelemetry,
) -> AppError {
    let mut details = error.details().unwrap_or_else(|| json!({}));
    if let Some(object) = details.as_object_mut() {
        object.insert("provider_api".to_owned(), json!(provider_api));
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
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
    status_with_deadline_and_budget(context, operation_deadline, None)
}

/// Status with one shared exact authenticated GitHub request budget.
pub(crate) fn status_with_deadline_and_budget(
    context: &AppContext,
    operation_deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<StatusOutput, AppError> {
    status_with_discovery_options(context, operation_deadline, github_budget, false)
}

/// Explicit PR-creation discovery permits one safe, advanced, unlabelled
/// historical branch generation to be treated as ancestry rather than current
/// membership. Ordinary status/navigation keeps the strict historical rule.
pub(crate) fn status_for_pr_creation(
    context: &AppContext,
    operation_deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<StatusOutput, AppError> {
    status_with_discovery_options(context, operation_deadline, github_budget, true)
}

#[allow(clippy::too_many_lines)]
fn status_with_discovery_options(
    context: &AppContext,
    operation_deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
    allow_unlabelled_historical_pr_creation: bool,
) -> Result<StatusOutput, AppError> {
    // Sharing one absolute deadline prevents a large repository from
    // multiplying its budget by provider and compatibility subprocess count.
    let started = std::time::Instant::now();
    let operation_budget = operation_deadline.saturating_duration_since(started);
    let child_timeout = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let provider_runner = crate::command::ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(child_timeout)
        .with_operation_deadline(operation_deadline);
    let provider_runner = github_budget.map_or(provider_runner.clone(), |budget| {
        provider_runner.with_github_request_budget(budget.clone())
    });
    let discovery = GitHubDiscovery::new(provider_runner.clone()).with_options(
        crate::github::DiscoveryOptions {
            allow_unlabelled_historical_pr_creation,
            ..crate::github::DiscoveryOptions::default()
        },
    );
    let snapshot = discovery.discover().map_err(|error| {
        let mapped =
            if let DiscoveryError::Runner(CommandRunError::Timeout { command, .. }) = &error {
                discovery_timeout_error(
                    &error,
                    discovery_phase(command),
                    started.elapsed(),
                    operation_budget,
                )
            } else {
                discovery_error(&error)
            };
        attach_provider_api(&mapped, &provider_runner.github_api_telemetry())
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
    let label_provider = crate::github::GitHubMutationAdapter::new(provider_runner.clone());
    let (labels, label_cache_hit) = repository_labels_cached(&label_provider, &snapshot.repository)
        .map_err(|error| {
            let mapped = if let crate::github::MutationError::Provider(provider) = &error
                && matches!(
                    provider,
                    DiscoveryError::Runner(CommandRunError::Timeout { .. })
                ) {
                discovery_timeout_error(
                    provider,
                    "repository_label_inventory",
                    started.elapsed(),
                    operation_budget,
                )
            } else {
                AppError::structured(
                    mcp_cli::ErrorCategory::ExecutionFailure,
                    "repository_initialization_inventory_failed",
                    error.to_string(),
                    Some(json!({"next": "repair GitHub read access and rerun `cara status`"})),
                )
            };
            attach_provider_api(&mapped, &provider_runner.github_api_telemetry())
        })?;
    let mut initialization = crate::initialization::inspect_labels(
        &labels,
        &context.config.agent_priority_labels,
        context.config.sync.actions.join_unlabelled_prs,
    );
    if !context.config_existed {
        initialization.ready = false;
        initialization.next = Some("run `cara init` to atomically create .caravan/config.yaml and verify repository readiness".to_owned());
    }
    let admission = resolve_admission(&analysis, &context.config.agent_priority_labels);
    let labels_elapsed = started.elapsed();
    let mut provider_api = provider_runner.github_api_telemetry();
    if label_cache_hit {
        provider_api.cache_hits = provider_api.cache_hits.saturating_add(1);
        provider_api.cache_age_ms = LABEL_CACHE
            .get()
            .and_then(|cache| {
                cache.lock().ok().and_then(|cache| {
                    cache
                        .get(&snapshot.repository.slug())
                        .map(|entry| entry.inserted.elapsed())
                })
            })
            .map(|age| u64::try_from(age.as_millis()).unwrap_or(u64::MAX));
    }
    let mut output = StatusOutput {
        provider_api,
        merge_candidates: snapshot.merge_candidates,
        merge_candidates_truncated: snapshot.merge_candidates_truncated,
        previous_default_oid: snapshot.previous_default_oid,
        default_branch_movements: snapshot.default_branch_movements,
        timing: None,
        repository: snapshot.repository,
        rebase_on_join: rebase_on_join_status(context),
        auto_admission: AutoAdmissionStatus {
            enabled: context.config.sync.actions.join_unlabelled_prs,
            heuristic_version: crate::sync::AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
            max_candidates_per_tick: context.config.sync.max_candidates_per_tick,
            max_mutations_per_tick: context.config.sync.max_mutations_per_tick,
            max_github_requests_per_tick: context.config.sync.max_github_requests_per_tick,
            max_duration_secs: context.config.sync.max_duration_secs,
        },
        default_branch: snapshot.default_branch.name,
        current_branch: snapshot.current_branch,
        current_pr: snapshot.current_pr,
        healthy: false,
        initialization,
        analysis,
        pauses: Vec::new(),
        admission,
    };
    crate::pause::apply_to_status(&context.repository_path, &mut output)?;
    output.healthy = output.analysis.healthy() && output.initialization.ready;

    let total = started.elapsed();
    if std::time::Instant::now() >= operation_deadline {
        return Err(AppError::structured(
            ErrorCategory::Timeout,
            "github_discovery_timeout",
            "status deadline expired while finalizing status and paused-caravan projection",
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
    output.timing = Some(StatusTiming {
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
                millis(labels_elapsed.saturating_sub(analysis_elapsed)),
            ),
            (
                "paused_caravan_projection".to_owned(),
                millis(total.saturating_sub(labels_elapsed)),
            ),
        ]),
    });
    Ok(output)
}

/// Return the canonical first automatic-admission candidate without mutation.
pub fn next_candidate(context: &AppContext) -> Result<NextCandidateOutput, AppError> {
    let status = status(context)?;
    Ok(NextCandidateOutput {
        provider_api: status.provider_api,
        repository: status.repository,
        attempt_contract: "ordered manual admission attempt only; run `cara check --pr N` for this exact first candidate; on rejection fail closed rather than leapfrogging. The separate opt-in sync-owned greedy policy may persist an exact generation-bound mechanical skip before considering a later candidate".to_owned(),
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
    let mut skipped = Vec::new();
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
        } else if pull_request.draft {
            Some("fail closed: draft PR must wait until marked ready".to_owned())
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
                // Unknown priority metadata blocks before known attempts: its
                // rank cannot safely be guessed. Otherwise preserve the same
                // configured rank/FIFO key as selectable candidates.
                priority_rank: (configured.len() == 1).then(|| configured[0].1 + 1),
                created_at: pull_request.created_at.clone(),
                reason,
            });
            continue;
        }

        let created_at = pull_request.created_at.clone();
        if pull_request.has_label("caravan-join-skipped") {
            skipped.push(SkippedAdmissionCandidate {
                pr: *number,
                priority_label: configured.first().map(|(label, _)| (*label).clone()),
                priority_rank: configured.first().map(|(_, rank)| rank + 1),
                created_at,
                reason: "generation-bound automatic admission skip; sync revalidates exact evidence before retrying".to_owned(),
            });
            continue;
        }
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
    skipped.sort_by_key(|candidate| {
        (
            candidate.priority_rank.unwrap_or(priority_labels.len() + 1),
            candidate.created_at.is_none(),
            candidate.created_at.clone().unwrap_or_default(),
            candidate.pr,
        )
    });
    rejected.sort_by_key(|candidate| {
        (
            candidate.priority_rank.unwrap_or(priority_labels.len() + 1),
            candidate.created_at.is_none(),
            candidate.created_at.clone().unwrap_or_default(),
            candidate.pr,
        )
    });
    // Select from candidates and rejected attempts together. A rejected first
    // attempt remains canonical and therefore cannot be silently leapfrogged.
    let next_candidate = candidates
        .iter()
        .map(|candidate| {
            (
                candidate.priority_rank.unwrap_or(priority_labels.len() + 1),
                candidate.created_at.is_none(),
                candidate.created_at.clone().unwrap_or_default(),
                candidate.pr,
            )
        })
        .chain(rejected.iter().map(|candidate| {
            (
                candidate.priority_rank.unwrap_or(priority_labels.len() + 1),
                candidate.created_at.is_none(),
                candidate.created_at.clone().unwrap_or_default(),
                candidate.pr,
            )
        }))
        .min()
        .map(|(_, _, _, pr)| pr);
    AdmissionStatus {
        policy: "ordered admission attempts: explicit agent priority label (configured high to low), then FIFO by immutable provider created_at ascending with PR number ascending as equal-time tie-break; missing created_at falls back deterministically to PR number after timestamped peers; never LIFO; check/new preflight required and rejection never causes automatic leapfrogging".to_owned(),
        priority_labels: priority_labels.to_vec(),
        candidates,
        skipped,
        rejected,
        next_candidate,
    }
}

/// Return the unique merged Caravan PR named by the local branch when the
/// effective current PR is its active rolling successor.
#[must_use]
pub fn historical_predecessor(status: &StatusOutput) -> Option<PrNumber> {
    let branch = status.current_branch.as_deref()?;
    let mut matches = status.analysis.pull_requests.values().filter(|pull| {
        pull.state == PullRequestState::Merged
            && pull.has_label("caravan")
            && pull.head.repository == status.repository
            && !pull.cross_repository
            && pull.head.name == branch
    });
    let predecessor = matches.next()?.number;
    matches.next().is_none().then_some(predecessor)
}

/// Show the current branch's active caravan and position.
pub fn show(context: &AppContext) -> Result<ShowOutput, AppError> {
    let status = status(context)?;
    let current_pr = status.current_pr.ok_or_else(|| {
        if let Some(predecessor) = historical_predecessor(&status) {
            AppError::structured(
                ErrorCategory::TargetNotFound,
                "historical_successor_not_found",
                format!("merged Caravan PR #{predecessor} has no unique active rolling successor"),
                Some(json!({
                    "historical_predecessor": predecessor,
                    "current_branch": status.current_branch,
                    "fail_closed": true,
                })),
            )
        } else {
            AppError::validation(
                "current_pr_not_found",
                "the current branch has no unique open GitHub pull request",
            )
        }
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
    let merge_candidates = status
        .merge_candidates
        .iter()
        .filter(|candidate| caravan.members.contains(&candidate.pr))
        .cloned()
        .collect();
    let historical_predecessor = historical_predecessor(&status);
    let historical_pull_request = historical_predecessor
        .and_then(|number| status.analysis.pull_requests.get(&number).cloned());
    Ok(ShowOutput {
        historical_predecessor,
        historical_pull_request,
        repository: status.repository,
        current_pr,
        caravan,
        position,
        pull_requests,
        merge_candidates,
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
    let status = input.pr.map_or_else(
        || status(context),
        |number| status_for_remote_candidate(context, PrNumber(number)),
    )?;
    let checker = GitCompatibilityChecker::new(&context.repository_path, "origin").with_timeout(
        std::time::Duration::from_secs(context.config.command_timeout_secs),
    );
    check_analysis(&status, input, &checker)
}

/// Discover the fleet and bind one exact remote candidate without checkout mutation.
pub(crate) fn status_for_remote_candidate(
    context: &AppContext,
    number: PrNumber,
) -> Result<StatusOutput, AppError> {
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(context.config.command_timeout_secs);
    status_for_remote_candidate_with_deadline(context, number, deadline, None)
}

/// Bind one exact remote candidate under a caller-owned deadline and GitHub budget.
pub(crate) fn status_for_remote_candidate_with_deadline(
    context: &AppContext,
    number: PrNumber,
    operation_deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<StatusOutput, AppError> {
    let mut status = status_with_deadline_and_budget(context, operation_deadline, github_budget)?;
    // Re-read the selected provider PR after fleet discovery. Comparing the
    // complete snapshot closes the discovery/refetch race before exact Git
    // revision checks; merge compatibility subsequently verifies refs both
    // before and after its fetch.
    let provider_runner = crate::command::ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(std::time::Duration::from_secs(
            context.config.command_timeout_secs,
        ))
        .with_operation_deadline(operation_deadline);
    let provider_runner = github_budget.map_or(provider_runner.clone(), |budget| {
        provider_runner.with_github_request_budget(budget.clone())
    });
    let provider = crate::github::GitHubMutationAdapter::new(provider_runner.clone());
    let fresh = provider
        .refetch_pull_request(&status.repository, number)
        .map_err(|error| {
            AppError::structured(
                ErrorCategory::ExecutionFailure,
                "candidate_refetch_failed",
                error.to_string(),
                Some(json!({"pr": number, "safe_next_action": "retry the read-only preflight"})),
            )
        })?;
    let discovered = status.analysis.pull_requests.get(&number).ok_or_else(|| {
        AppError::validation(
            "candidate_not_found",
            format!("PR #{number} is not an open provider candidate"),
        )
    })?;
    require_fresh_candidate(discovered, &fresh)?;
    let identity = GitHubDiscovery::new(provider_runner.clone())
        .merge_candidate_identity(&status.repository, &fresh)
        .map_err(|error| discovery_error(&error))?;
    status
        .merge_candidates
        .retain(|candidate| candidate.pr != number);
    status.merge_candidates.push(identity);
    status.current_pr = Some(number);
    status
        .provider_api
        .merge(provider_runner.github_api_telemetry());
    Ok(status)
}

fn require_fresh_candidate(
    discovered: &PullRequestSnapshot,
    fresh: &PullRequestSnapshot,
) -> Result<(), AppError> {
    // Check churn, provider timestamps, title, and URL do not invalidate an
    // identity preflight. Every operation-shaping fact does.
    let identity_matches = discovered.number == fresh.number
        && discovered.state == fresh.state
        && discovered.draft == fresh.draft
        && discovered.head == fresh.head
        && discovered.base == fresh.base
        && discovered.cross_repository == fresh.cross_repository
        && discovered.labels == fresh.labels
        && discovered.auto_merge == fresh.auto_merge;
    if identity_matches {
        return Ok(());
    }
    Err(AppError::structured(
        ErrorCategory::Validation,
        "stale_candidate_snapshot",
        format!("PR #{} changed during remote preflight", discovered.number),
        Some(json!({
            "pr": discovered.number,
            "expected_head_oid": discovered.head.oid,
            "actual_head_oid": fresh.head.oid,
            "expected": discovered,
            "actual": fresh,
            "safe_next_action": "retry `cara check --pr` to preflight the new exact provider state",
            "mutated": false,
        })),
    ))
}

/// Pure/injectable check policy used by live commands and fixture tests.
#[allow(clippy::too_many_lines)]
pub fn check_analysis(
    status: &StatusOutput,
    input: &CheckInput,
    checker: &impl CompatibilityChecker,
) -> Result<CheckOutput, AppError> {
    crate::initialization::require_ready(&status.initialization)?;
    let current_pr = input.pr.map(PrNumber).or(status.current_pr).ok_or_else(|| {
        AppError::validation(
            "current_pr_not_found",
            "the selected remote PR was not found and the current branch has no unique open GitHub pull request",
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
    let canonical_candidate = status.admission.next_candidate == Some(current_pr)
        || (input.pr.is_some() && pull_request.draft && status.admission.next_candidate.is_none());
    let admission_rejection = status
        .admission
        .rejected
        .iter()
        .find(|candidate| candidate.pr == current_pr);
    let merge_candidate = status
        .merge_candidates
        .iter()
        .find(|candidate| candidate.pr == current_pr)
        .cloned();
    let candidate_stale = merge_candidate.as_ref().is_some_and(|candidate| {
        candidate.freshness != crate::model::MergeCandidateFreshness::Fresh
    });
    let remote = input.pr.is_some();

    if let Some(caravan) = status.analysis.fleet.containing(current_pr) {
        if (input.tail_pr.is_some() || input.head_pr.is_some()) && !remote {
            return Err(AppError::validation(
                "active_pr_cannot_join",
                format!("PR #{current_pr} is already in caravan #{}", caravan.id),
            ));
        }
        let mut active_problems = status.analysis.fleet.problems.clone();
        if input.tail_pr.is_some() || input.head_pr.is_some() {
            active_problems.push(GraphProblem {
                kind: GraphProblemKind::Unknown,
                prs: vec![current_pr],
                message: format!("candidate is already enrolled in caravan #{}", caravan.id),
            });
        }
        if candidate_stale {
            active_problems.push(GraphProblem {
                kind: GraphProblemKind::Unknown,
                prs: vec![current_pr],
                message: "provider merge-candidate identity is stale or incomplete; wait for the current generation".to_owned(),
            });
        }
        let eligible = status.healthy && active_problems.is_empty();
        let output = CheckOutput {
            provider_api: status.provider_api.clone(),
            rebase_on_join: status.rebase_on_join.clone(),
            mode: CheckMode::ActiveCaravan,
            current_pr,
            candidate: pull_request.clone(),
            head_repository_owner: pull_request.head.repository.owner.clone(),
            merge_candidate,
            enrolled: true,
            canonical_candidate,
            next_action: if input.tail_pr.is_some() || input.head_pr.is_some() {
                CandidateNextAction::Reject
            } else if candidate_stale || eligible {
                CandidateNextAction::Wait
            } else {
                CandidateNextAction::Repair
            },
            caravan_id: Some(caravan.id),
            target_pr: None,
            eligible,
            compatibility: status.analysis.compatibility.clone(),
            problems: active_problems,
            initialization: status.initialization.clone(),
        };
        return eligible_or_error(output, remote);
    }

    let mut problems = status.analysis.fleet.problems.clone();
    validate_candidate(pull_request, &mut problems);
    if candidate_stale {
        problems.push(GraphProblem {
            kind: GraphProblemKind::Unknown,
            prs: vec![current_pr],
            message: "provider merge-candidate identity is stale or incomplete; wait for the current generation".to_owned(),
        });
    }
    if let Some(rejected) = admission_rejection {
        problems.push(GraphProblem {
            kind: GraphProblemKind::Unknown,
            prs: vec![current_pr],
            message: rejected.reason.clone(),
        });
    }
    if remote && !canonical_candidate {
        problems.push(GraphProblem {
            kind: GraphProblemKind::Unknown,
            prs: vec![current_pr],
            message: status.admission.next_candidate.map_or_else(
                || "candidate is not selectable because no canonical admission attempt exists".to_owned(),
                |first| format!("candidate is not canonical first admission attempt; fail closed on PR #{first}"),
            ),
        });
    }
    let mut reports = Vec::new();

    let explicit_join = input.tail_pr.is_some() || input.head_pr.is_some();
    if !explicit_join {
        if !pull_request.cross_repository {
            check_new(status, pull_request, checker, &mut reports, &mut problems)?;
        }
        let eligible = problems.is_empty();
        let next_action = candidate_action(
            pull_request,
            &reports,
            &CandidateActionContext {
                eligibility: if eligible {
                    ActionEligibility::Eligible
                } else {
                    ActionEligibility::Ineligible
                },
                target: ActionTarget::New,
                order: if canonical_candidate || !remote {
                    ActionOrder::Canonical
                } else {
                    ActionOrder::NonCanonical
                },
                admission: if admission_rejection.is_some() {
                    AdmissionDecision::Rejected
                } else {
                    AdmissionDecision::Accepted
                },
                freshness: if candidate_stale {
                    CandidateFreshness::Stale
                } else {
                    CandidateFreshness::Fresh
                },
            },
        );
        return eligible_or_error(
            CheckOutput {
                provider_api: status.provider_api.clone(),
                rebase_on_join: status.rebase_on_join.clone(),
                mode: CheckMode::NewCaravan,
                current_pr,
                candidate: pull_request.clone(),
                head_repository_owner: pull_request.head.repository.owner.clone(),
                merge_candidate,
                enrolled: false,
                canonical_candidate,
                next_action,
                caravan_id: Some(current_pr),
                target_pr: None,
                eligible,
                compatibility: reports,
                problems,
                initialization: status.initialization.clone(),
            },
            remote,
        );
    }

    let target_caravan = resolve_target_caravan(status, input)?;
    let tail_number = target_caravan.tail().expect("caravans are non-empty");
    let tail = status
        .analysis
        .pull_requests
        .get(&tail_number)
        .expect("derived tail has a snapshot");
    if !pull_request.cross_repository {
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
    }

    let eligible = problems.is_empty();
    let next_action = candidate_action(
        pull_request,
        &reports,
        &CandidateActionContext {
            eligibility: if eligible {
                ActionEligibility::Eligible
            } else {
                ActionEligibility::Ineligible
            },
            target: ActionTarget::Join,
            order: if canonical_candidate || !remote {
                ActionOrder::Canonical
            } else {
                ActionOrder::NonCanonical
            },
            admission: if admission_rejection.is_some() {
                AdmissionDecision::Rejected
            } else {
                AdmissionDecision::Accepted
            },
            freshness: if candidate_stale {
                CandidateFreshness::Stale
            } else {
                CandidateFreshness::Fresh
            },
        },
    );
    eligible_or_error(
        CheckOutput {
            provider_api: status.provider_api.clone(),
            rebase_on_join: status.rebase_on_join.clone(),
            mode: CheckMode::JoinTail,
            current_pr,
            candidate: pull_request.clone(),
            head_repository_owner: pull_request.head.repository.owner.clone(),
            merge_candidate,
            enrolled: false,
            canonical_candidate,
            next_action,
            caravan_id: Some(target_caravan.id),
            target_pr: Some(tail_number),
            eligible,
            compatibility: reports,
            problems,
            initialization: status.initialization.clone(),
        },
        remote,
    )
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionEligibility {
    Eligible,
    Ineligible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionTarget {
    New,
    Join,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionOrder {
    Canonical,
    NonCanonical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionDecision {
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateFreshness {
    Fresh,
    Stale,
}

struct CandidateActionContext {
    eligibility: ActionEligibility,
    target: ActionTarget,
    order: ActionOrder,
    admission: AdmissionDecision,
    freshness: CandidateFreshness,
}

fn candidate_action(
    candidate: &PullRequestSnapshot,
    reports: &[CompatibilityReport],
    context: &CandidateActionContext,
) -> CandidateNextAction {
    if context.order == ActionOrder::NonCanonical {
        return CandidateNextAction::Reject;
    }
    if candidate.draft || context.freshness == CandidateFreshness::Stale {
        return CandidateNextAction::Wait;
    }
    if context.admission == AdmissionDecision::Rejected {
        return CandidateNextAction::Reject;
    }
    if context.eligibility == ActionEligibility::Eligible {
        return match context.target {
            ActionTarget::Join => CandidateNextAction::Join,
            ActionTarget::New => CandidateNextAction::New,
        };
    }
    if candidate.state != PullRequestState::Open
        || candidate.cross_repository
        || candidate.has_label("caravan-evicted")
    {
        return CandidateNextAction::Reject;
    }
    if reports
        .iter()
        .any(|report| report.outcome != CompatibilityOutcome::Clean)
    {
        return CandidateNextAction::Repair;
    }
    CandidateNextAction::Repair
}

fn eligible_or_error(
    output: CheckOutput,
    return_rejection_receipt: bool,
) -> Result<CheckOutput, AppError> {
    if output.eligible || return_rejection_receipt {
        return Ok(output);
    }
    Err(AppError::structured(
        ErrorCategory::Validation,
        "check_failed",
        "the requested Caravan operation is not currently valid",
        Some(serde_json::to_value(&output).unwrap_or_else(|_| json!({}))),
    ))
}

#[allow(clippy::too_many_lines)]
fn discovery_error(error: &DiscoveryError) -> AppError {
    if let DiscoveryError::Runner(CommandRunError::OutputLimit {
        command,
        code,
        stdout,
        stderr,
    }) = error
    {
        return AppError::structured(
            ErrorCategory::ExecutionFailure,
            "command_output_limit",
            error.to_string(),
            Some(json!({
                "stage": "github_command_output",
                "command": command.display(),
                "exit_code": code,
                "stdout": stdout,
                "stderr": stderr,
                "streams_combined": false,
                "mutated": false,
                "resumable": true,
                "safe_next_action": "reduce the bounded provider query/output, then retry; do not parse truncated JSON",
            })),
        );
    }
    if let DiscoveryError::Runner(CommandRunError::GithubRequestBudgetExceeded {
        command,
        limit,
        used,
    }) = error
    {
        return AppError::structured(
            ErrorCategory::ExecutionFailure,
            "github_request_budget_exhausted",
            error.to_string(),
            Some(json!({
                "command": command.display(),
                "limit": limit,
                "used": used,
                "retryable": true,
                "mutated": false,
                "safe_next_action": "rerun the same bounded sync tick to continue from fresh provider state",
            })),
        );
    }
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
    if let DiscoveryError::HistoricalCurrentPullRequest {
        reason,
        branch,
        candidates,
    } = error
    {
        return AppError::structured(
            ErrorCategory::Validation,
            format!("historical_current_pr_{reason}"),
            error.to_string(),
            Some(json!({
                "historical_branch": branch,
                "candidates": candidates,
                "reason": reason,
                "fail_closed": true,
                "next": "inspect bounded same-repository PR history and restore an exact, unique retained Caravan branch before retrying",
            })),
        );
    }
    let category = match error {
        DiscoveryError::AmbiguousCurrentPullRequest { .. }
        | DiscoveryError::HistoricalCurrentPullRequest { .. }
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Clone)]
    struct LabelRunner(Arc<AtomicUsize>);

    impl crate::command::CommandRunner for LabelRunner {
        fn run(
            &self,
            _command: &crate::command::CommandSpec,
        ) -> Result<crate::command::CommandOutput, crate::command::CommandRunError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(crate::command::CommandOutput {
                code: Some(0),
                stdout: r#"[{"name":"caravan","color":"5319E7","description":"Active"}]"#
                    .to_owned(),
                stderr: String::new(),
            })
        }
    }
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
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
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
            provider_api: crate::model::GitHubApiTelemetry::default(),
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            timing: None,
            repository: repository(),
            rebase_on_join: RebaseOnJoinStatus::default(),
            auto_admission: AutoAdmissionStatus::default(),
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
            pauses: Vec::new(),
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
                pr: None,
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
    fn provider_output_limit_is_typed_before_json_decode() {
        let error = discovery_error(&DiscoveryError::Runner(CommandRunError::OutputLimit {
            command: crate::command::CommandSpec::new("gh").args(["pr", "list"]),
            code: Some(0),
            stdout: Box::new(crate::command::StreamCaptureEvidence {
                limit_bytes: 32,
                total_bytes: 100,
                truncated: true,
                prefix: "[{\"number\":".to_owned(),
                suffix: "}]".to_owned(),
            }),
            stderr: Box::new(crate::command::StreamCaptureEvidence {
                limit_bytes: 16,
                total_bytes: 0,
                truncated: false,
                prefix: String::new(),
                suffix: String::new(),
            }),
        }));

        assert_eq!(error.code(), "command_output_limit");
        let details = error.details().unwrap();
        assert_eq!(details["stage"], "github_command_output");
        assert_eq!(details["stdout"]["total_bytes"], 100);
        assert_eq!(details["stdout"]["limit_bytes"], 32);
        assert_eq!(details["streams_combined"], false);
        assert_eq!(details["mutated"], false);
    }

    #[test]
    fn discovery_timeout_preserves_timeout_category_and_evidence() {
        let error = discovery_error(&DiscoveryError::Runner(CommandRunError::Timeout {
            command: crate::command::CommandSpec::new("gh").args(["pr", "list"]),
            process_group_id: None,
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
            process_group_id: None,
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
    fn generation_bound_skip_is_excluded_without_blocking_later_candidates() {
        let mut skipped = pr(10, "skipped", "main", false);
        skipped.labels.insert("caravan-join-skipped".to_owned());
        let later = pr(20, "later", "main", false);
        let status = status(skipped, vec![later]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;
        let admission = resolve_admission(&status.analysis, &labels);

        assert_eq!(admission.skipped.len(), 1);
        assert_eq!(admission.skipped[0].pr, PrNumber(10));
        assert_eq!(admission.next_candidate, Some(PrNumber(20)));
        assert_eq!(
            admission
                .candidates
                .iter()
                .map(|candidate| candidate.pr)
                .collect::<Vec<_>>(),
            [PrNumber(20)]
        );
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
        assert_eq!(
            admission.next_candidate,
            Some(PrNumber(10)),
            "the rejected first attempt must block rather than leapfrog to #30"
        );
        assert_eq!(admission.rejected.len(), 2);
        assert!(
            admission
                .rejected
                .iter()
                .all(|candidate| candidate.reason.contains("fail closed"))
        );
    }

    #[test]
    fn unrelated_check_churn_does_not_stale_candidate_identity() {
        let discovered = pr(9, "nine", "main", false);
        let mut fresh = discovered.clone();
        fresh.checks.push(crate::model::CheckSnapshot {
            name: "ci".to_owned(),
            state: crate::model::CheckState::InProgress,
            provider_state: Some("IN_PROGRESS".to_owned()),
            details_url: None,
        });
        fresh.updated_at = Some("2026-01-02T00:00:00Z".to_owned());
        require_fresh_candidate(&discovered, &fresh)
            .expect("check/timestamp churn does not change candidate identity");
    }

    #[test]
    fn stale_head_between_discovery_and_refetch_fails_closed() {
        let discovered = pr(9, "nine", "main", false);
        let mut fresh = discovered.clone();
        fresh.head.oid = crate::model::CommitOid("f".repeat(40));
        let error = require_fresh_candidate(&discovered, &fresh)
            .expect_err("a moved provider head must invalidate the receipt");
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "stale_candidate_snapshot"
        );
        let details = mcp_cli::StructuredError::details(&error).unwrap();
        assert_eq!(details["expected_head_oid"], discovered.head.oid.0);
        assert_eq!(details["actual_head_oid"], fresh.head.oid.0);
        assert_eq!(details["mutated"], false);
    }

    #[test]
    fn remote_candidate_receipt_rejects_a_noncanonical_pr_without_leapfrogging() {
        let first = pr(10, "first", "main", false);
        let second = pr(20, "second", "main", false);
        let status = status(first, vec![second]);
        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(20),
                tail_pr: None,
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("remote rejection is an inspectable receipt");
        assert!(!output.eligible);
        assert!(!output.canonical_candidate);
        assert_eq!(output.next_action, CandidateNextAction::Reject);
        assert!(
            output
                .problems
                .iter()
                .any(|problem| problem.message.contains("fail closed on PR #10"))
        );
        let json = serde_json::to_value(&output).expect("remote receipt serializes");
        assert_eq!(json["next_action"], "reject");
        assert_eq!(json["candidate"]["number"], 20);
        assert!(json.get("merge_candidate").is_none());
    }

    #[test]
    fn canonical_provider_staleness_returns_wait_receipt() {
        let candidate = pr(9, "nine", "main", false);
        let mut status = status(candidate.clone(), Vec::new());
        status
            .merge_candidates
            .push(crate::model::MergeCandidateIdentity {
                pr: candidate.number,
                provider_updated_at: "2026-01-01T00:00:00Z".to_owned(),
                observed_at: "2026-01-01T00:00:01Z".to_owned(),
                base: candidate.base.clone(),
                head: candidate.head.clone(),
                synthetic: None,
                auto_merge: crate::model::NativeAutoMergeState {
                    enabled: false,
                    merge_method: None,
                    actor: None,
                },
                freshness: crate::model::MergeCandidateFreshness::StaleHead,
                stale_base: false,
                stale_head: true,
                stale_reasons: vec!["synthetic head parent is stale".to_owned()],
            });
        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(9),
                tail_pr: None,
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("provider staleness is an inspectable remote receipt");
        assert!(!output.eligible);
        assert_eq!(output.next_action, CandidateNextAction::Wait);
        assert_eq!(
            output
                .merge_candidate
                .as_ref()
                .map(|identity| identity.freshness),
            Some(crate::model::MergeCandidateFreshness::StaleHead)
        );
    }

    #[test]
    fn remote_draft_receipt_waits_on_the_canonical_candidate() {
        let mut candidate = pr(9, "nine", "main", false);
        candidate.draft = true;
        let status = status(candidate, Vec::new());
        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(9),
                tail_pr: None,
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("remote draft rejection is an inspectable receipt");
        assert!(!output.eligible);
        assert_eq!(output.next_action, CandidateNextAction::Wait);
        assert_eq!(output.candidate.number, PrNumber(9));
    }

    #[test]
    fn repository_label_inventory_is_cached_without_becoming_mutation_state() {
        let repository = repository();
        if let Some(cache) = LABEL_CACHE.get() {
            cache.lock().unwrap().remove(&repository.slug());
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = crate::github::GitHubMutationAdapter::new(LabelRunner(Arc::clone(&calls)));
        let (first, first_hit) = repository_labels_cached(&provider, &repository).unwrap();
        let (second, second_hit) = repository_labels_cached(&provider, &repository).unwrap();
        assert!(!first_hit);
        assert!(second_hit);
        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn short_lived_status_cache_reports_hits_and_invalidates_exact_repository() {
        let directory = tempfile::tempdir().unwrap();
        let context = crate::AppContext {
            repository_path: directory.path().to_path_buf(),
            config_path: directory.path().join(".caravan/config.yaml"),
            config_existed: false,
            config: crate::config::CaravanConfig::default(),
        };
        let key = status_cache_key(&context);
        let expected = status(pr(9, "nine", "main", false), Vec::new());
        STATUS_CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .insert(
                key.clone(),
                CachedStatus {
                    inserted: Instant::now(),
                    output: expected,
                },
            );
        let cached = status_cached(&context, Duration::from_secs(5)).unwrap();
        assert_eq!(cached.provider_api.cache_hits, 1);
        assert!(cached.provider_api.cache_age_ms.is_some());
        invalidate_status_cache(&context);
        assert!(
            !STATUS_CACHE
                .get()
                .unwrap()
                .lock()
                .unwrap()
                .contains_key(&key)
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
