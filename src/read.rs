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
use crate::graph::{
    CompatibilityChecker, GitCompatibilityChecker, GraphAnalysis, analyze_for_actor,
};
use crate::model::{
    Caravan, CompatibilityOutcome, CompatibilityReport, GraphProblem, GraphProblemKind, PrNumber,
    PullRequestSnapshot, PullRequestState, RepositoryId,
};
use crate::squash_equivalence::SquashEquivalenceReport;
use crate::{AppContext, AppError, CheckInput};

/// Exact configured merge-actor policy shared by every merge-aware surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HeadMergeStatus {
    /// Who merges a caravan root into the default branch.
    pub actor: crate::model::HeadMergeActor,
    /// What a caravan-owned tick does about a foreign auto-merge request.
    pub external_auto_merge_policy: crate::root_merge::ExternalAutoMergePolicy,
    /// Bounded caravan-owned merges per tick.
    pub max_root_merges_per_tick: u32,
}

impl Default for HeadMergeStatus {
    fn default() -> Self {
        Self {
            actor: crate::model::HeadMergeActor::default(),
            external_auto_merge_policy: crate::root_merge::ExternalAutoMergePolicy::default(),
            max_root_merges_per_tick: crate::config::SyncConfig::default().max_root_merges_per_tick,
        }
    }
}

impl HeadMergeStatus {
    /// Derive the exact policy from configuration.
    #[must_use]
    pub fn from_config(config: &crate::config::SyncConfig) -> Self {
        Self {
            actor: config.resolved_head_merge_actor(),
            external_auto_merge_policy: config.external_auto_merge_policy,
            max_root_merges_per_tick: config.max_root_merges_per_tick,
        }
    }
}

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
    /// Exact binary that produced this receipt. Queue operators must be able to
    /// prove which build answered when several are installed (bd-0629ce).
    #[serde(default)]
    pub runtime: RuntimeProvenance,
    /// Whether the effective configuration is the repository's policy or one
    /// branch's proposal. Reported only; nothing depends on it yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_provenance: Option<crate::config_provenance::ConfigProvenance>,
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
    /// Exact configured merge-actor policy for caravan roots. Every surface
    /// that arms, refuses, or performs a merge reads this one fact so the
    /// auto-merge invariant, membership, reshape, pause, and sync can never
    /// disagree about who merges.
    #[serde(default)]
    pub head_merge: HeadMergeStatus,
    /// Exact sync-owned automatic-admission policy and safety bounds.
    #[serde(default)]
    pub auto_admission: AutoAdmissionStatus,
    /// Deterministic physical-apply reserve projection for the configured
    /// deadline: required budget, processable prefix, blocked candidate, and
    /// the safe next action, all visible before any sync refusal.
    #[serde(default)]
    pub sync_budget: crate::sync::SyncBudgetStatus,
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
    /// Superseded generations are excluded rather than allowed to block their
    /// proven canonical successor. Ambiguous/invalid candidates still block.
    #[serde(default = "default_true")]
    pub blocks_order: bool,
    pub reason: String,
}

const fn default_true() -> bool {
    true
}

/// Resolved GitHub-visible automatic-admission policy and result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdmissionStatus {
    pub policy: String,
    pub priority_labels: Vec<String>,
    /// Cacophony generation classification is provider-derived and remains
    /// visible even when a superseded PR is excluded from FIFO selection.
    #[serde(default)]
    pub generation_integrity: crate::generation::GenerationIntegrityStatus,
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
    /// Exact command to run next, so a correct FIFO rejection of a later PR is
    /// not mistaken for a queue fault (bd-d7aae7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_admission_command: Option<NextAdmissionCommand>,
    pub admission: AdmissionStatus,
    /// Typed automatic-selection decision for the canonical candidate. This is
    /// the FIFO-bound surface: it is emitted with
    /// `selection = automatic`, and automatic selection never bypasses an
    /// earlier ordered row for either `new` or `join` intent. Compare it with a
    /// `cara check --pr N` receipt's `admission_intent` to see explicit owner
    /// intent evaluated separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automatic_selection: Option<crate::admission::AdmissionIntentDecision>,
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
    /// Automatic selection follows this order; an explicit `--pr` selection is
    /// deliberate owner intent and is not blocked by it for either `new` or
    /// `join`.
    pub canonical_candidate: bool,
    /// Non-blocking evidence when explicit intent differs from the automatic
    /// priority/FIFO order. Derived from `admission_intent` so the human note,
    /// the typed decision, and the mutation gate can never disagree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_note: Option<String>,
    /// Typed selection/intent-aware admission-order decision and provenance.
    /// Explicit owner intent — `new` or `join` — is resolved here before FIFO
    /// canonical-candidate rejection; automatic selection stays FIFO-bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_intent: Option<crate::admission::AdmissionIntentDecision>,
    pub next_action: CandidateNextAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caravan_id: Option<PrNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_pr: Option<PrNumber>,
    pub eligible: bool,
    #[serde(default)]
    pub compatibility: Vec<CompatibilityReport>,
    /// Exact squash-equivalence evidence for every non-clean pair above.
    ///
    /// A stacked candidate whose earliest commits already landed as a squash
    /// conflicts against content identical to what it carries; this states,
    /// with exact per-path blob proof, whether that is the case and whether
    /// reconciling it would leave a clean replay. It is evidence only.
    #[serde(default)]
    pub squash_reconciliations: Vec<SquashEquivalenceReport>,
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
/// Exact identity of the running Cara binary.
///
/// Several Cara builds can be installed at once (a reviewed Nix store closure
/// and a cargo-installed binary), and PATH order alone decides which answers.
/// When one of them is broken, a silent crash is indistinguishable from a quiet
/// queue, so every status receipt records exactly which build produced it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeProvenance {
    /// Compiled package version.
    pub version: String,
    /// Fully resolved executable path, symlinks followed where possible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// SHA-256 of the exact running executable, when it is readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_sha256: Option<String>,
    /// True when the executable resolves inside a Nix store path.
    pub nix_store: bool,
}

impl RuntimeProvenance {
    #[must_use]
    pub fn detect() -> Self {
        let executable = std::env::current_exe()
            .ok()
            .map(|path| std::fs::canonicalize(&path).unwrap_or(path));
        let executable_sha256 = executable.as_ref().and_then(|path| {
            let bytes = std::fs::read(path).ok()?;
            Some(format!(
                "sha256:{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(&bytes)
            ))
        });
        let display = executable.as_ref().map(|path| path.display().to_string());
        Self {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            nix_store: display
                .as_deref()
                .is_some_and(|path| path.starts_with("/nix/store/")),
            executable: display,
            executable_sha256,
        }
    }
}

/// Prove an exact ancestry relation from objects already present locally.
///
/// The provider comparison is authoritative when it succeeds. When a ref has
/// been deleted or is otherwise unreachable, the same commits are frequently
/// still in the local object database, and an exact local proof is strictly
/// better than declaring an entire same-stream component unprovable.
fn local_commit_relation(
    repository_path: &std::path::Path,
    base: &crate::model::CommitOid,
    head: &crate::model::CommitOid,
) -> Option<crate::generation::CommitRelation> {
    use crate::command::{CommandRunner, CommandSpec, ProcessRunner};
    let runner = ProcessRunner::in_directory(repository_path);
    let known = |oid: &crate::model::CommitOid| {
        runner
            .run(&CommandSpec::new("git").args([
                "cat-file",
                "-e",
                &format!("{}^{{commit}}", oid.0),
            ]))
            .is_ok_and(|output| output.is_success())
    };
    if !known(base) || !known(head) {
        return None;
    }
    if base == head {
        return Some(crate::generation::CommitRelation::Identical);
    }
    let ancestor = |first: &crate::model::CommitOid, second: &crate::model::CommitOid| {
        runner
            .run(&CommandSpec::new("git").args([
                "merge-base",
                "--is-ancestor",
                first.0.as_str(),
                second.0.as_str(),
            ]))
            .is_ok_and(|output| output.is_success())
    };
    match (ancestor(base, head), ancestor(head, base)) {
        (true, true) => Some(crate::generation::CommitRelation::Identical),
        (true, false) => Some(crate::generation::CommitRelation::Ahead),
        (false, true) => Some(crate::generation::CommitRelation::Behind),
        (false, false) => Some(crate::generation::CommitRelation::Diverged),
    }
}

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
    status_with_discovery_options(context, operation_deadline, github_budget, false, true)
}

/// Status for a fleet-level read that must not depend on the local checkout.
///
/// Admission order and the next candidate are properties of the provider graph.
/// A checkout left on a merged or ambiguous branch — the state Cara itself
/// produces by retiring merged heads — must not stop the queue from advancing.
/// `current_pr` simply resolves to `None`, which every PR-scoped command already
/// treats as a refusal to act rather than a licence to guess.
pub(crate) fn fleet_status(
    context: &AppContext,
    operation_deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<StatusOutput, AppError> {
    status_with_discovery_options(context, operation_deadline, github_budget, false, false)
}

/// Explicit PR-creation discovery permits one safe, advanced, unlabelled
/// historical branch generation to be treated as ancestry rather than current
/// membership. Ordinary status/navigation keeps the strict historical rule.
pub(crate) fn status_for_pr_creation(
    context: &AppContext,
    operation_deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<StatusOutput, AppError> {
    status_with_discovery_options(context, operation_deadline, github_budget, true, true)
}

#[allow(clippy::too_many_lines)]
fn status_with_discovery_options(
    context: &AppContext,
    operation_deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
    allow_unlabelled_historical_pr_creation: bool,
    require_current_pr_resolution: bool,
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
            require_current_pr_resolution,
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
    // The auto-merge invariant is gated on the configured merge actor so a
    // repository that deliberately disabled native auto-merge never reports a
    // permanently unsatisfiable problem.
    let mut analysis = analyze_for_actor(
        &snapshot,
        &checker,
        context.config.sync.resolved_head_merge_actor(),
    )
    .map_err(|error| {
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
    let mut generation_facts = snapshot.generation_facts.clone();
    for pr in crate::generation::duplicate_stream_prs(&generation_facts)
        .into_iter()
        .take(32)
    {
        if let Ok(comments) = label_provider.pull_request_comment_bodies(&snapshot.repository, pr) {
            crate::generation::attach_reviewed_supersession_links(
                &mut generation_facts,
                pr,
                &comments,
            );
        }
    }
    let generation_integrity = crate::generation::analyze(&generation_facts, |base, head| {
        // bd-7546ea: exact local objects are an authoritative ancestry proof.
        // A provider comparison can be unreachable (deleted ref, 404) or simply
        // wrong/stale, and reporting a direct parent/child pair as `diverged`
        // dead-ends the owner. Prefer a local proof of ancestry whenever both
        // commits are present; otherwise keep the provider's answer.
        let provider_relation = label_provider.compare_commits(&snapshot.repository, base, head);
        let local = local_commit_relation(&context.repository_path, base, head);
        match (provider_relation, local) {
            // A local ancestry proof is authoritative and overrides an
            // unreachable or contradictory provider answer.
            (
                _,
                Some(
                    relation @ (crate::generation::CommitRelation::Ahead
                    | crate::generation::CommitRelation::Behind
                    | crate::generation::CommitRelation::Identical),
                ),
            )
            | (Err(_), Some(relation))
            | (Ok(relation), _) => relation,
            (Err(error), None) => crate::generation::CommitRelation::Unknown {
                reason: error.to_string(),
            },
        }
    });
    apply_generation_graph_problems(&mut analysis, &generation_integrity);
    let admission = resolve_admission_with_generation(
        &analysis,
        &context.config.agent_priority_labels,
        generation_integrity,
    );
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
        head_merge: HeadMergeStatus::from_config(&context.config.sync),
        runtime: RuntimeProvenance::detect(),
        // Reported only: measure how often a checkout is running one branch's
        // proposed policy before anything is allowed to depend on the answer.
        config_provenance: Some(crate::config_provenance::resolve(
            &context.repository_path,
            &context.config_path,
            context.config_path.is_absolute(),
        )),
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
        sync_budget: crate::sync::SyncBudgetStatus::default(),
    };
    crate::pause::apply_to_status(&context.repository_path, &mut output)?;
    output.sync_budget = crate::sync::project_status(context, &output);
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

/// One actionable admission instruction with the reason it was chosen.
///
/// bd-d7aae7: `next-candidate` already succeeds deterministically and the
/// attempt contract already says to preflight the exact first candidate, but
/// nothing surfaced a single command. Operators therefore read a correct FIFO
/// rejection of a later PR as a queue fault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NextAdmissionCommand {
    /// Exact `cara` invocation to run next.
    pub command: String,
    /// Why this is the next step.
    pub reason: String,
    /// Canonical candidate the command targets, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrNumber>,
}

fn next_admission_command(admission: &AdmissionStatus) -> NextAdmissionCommand {
    admission.next_candidate.map_or_else(
        || {
            let blocked = admission
                .rejected
                .iter()
                .find(|candidate| candidate.blocks_order);
            blocked.map_or_else(
                || NextAdmissionCommand {
                    command: "cara status".to_owned(),
                    reason: "no candidate is currently selectable for automatic admission"
                        .to_owned(),
                    pr: None,
                },
                |blocked| NextAdmissionCommand {
                    command: format!("cara check --pr {}", blocked.pr),
                    reason: format!(
                        "PR #{} blocks canonical order and must be resolved first: {}",
                        blocked.pr, blocked.reason
                    ),
                    pr: Some(blocked.pr),
                },
            )
        },
        |pr| NextAdmissionCommand {
            command: format!("cara check --pr {pr}"),
            reason: format!(
                "PR #{pr} is the canonical first automatic-admission attempt; preflight it before any membership mutation"
            ),
            pr: Some(pr),
        },
    )
}

/// Return the canonical first automatic-admission candidate without mutation.
pub fn next_candidate(context: &AppContext) -> Result<NextCandidateOutput, AppError> {
    // Which PR is next is a property of the provider graph, never of the local
    // checkout, so this read tolerates a merged or ambiguous current branch.
    let budget = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let status = fleet_status(context, std::time::Instant::now() + budget, None)?;
    // The automatic surface is FIFO-bound for both intents. Emitting the typed
    // decision here keeps it comparable with, and distinct from, the explicit
    // owner intent recorded on a `cara check --pr N` receipt.
    let automatic_selection = status
        .admission
        .next_candidate
        .and_then(|pr| status.analysis.pull_requests.get(&pr))
        .map(|candidate| {
            crate::admission::evaluate(
                &status.admission,
                &status.analysis,
                candidate,
                None,
                crate::admission::AdmissionSelection::Automatic,
            )
        });
    let next_admission_command = Some(next_admission_command(&status.admission));
    Ok(NextCandidateOutput {
        provider_api: status.provider_api,
        repository: status.repository,
        next_admission_command,
        attempt_contract: "ordered manual admission attempt only; run `cara check --pr N` for this exact first candidate; on rejection fail closed rather than leapfrogging. The separate opt-in sync-owned greedy policy may persist an exact generation-bound mechanical skip before considering a later candidate".to_owned(),
        admission: status.admission,
        automatic_selection,
    })
}

fn apply_generation_graph_problems(
    analysis: &mut GraphAnalysis,
    integrity: &crate::generation::GenerationIntegrityStatus,
) {
    for finding in &integrity.findings {
        if finding.disposition == crate::generation::GenerationDisposition::CurrentGeneration
            || !analysis
                .pull_requests
                .get(&finding.pr)
                .is_some_and(PullRequestSnapshot::is_active_caravan_member)
        {
            continue;
        }
        let kind = match finding.disposition {
            crate::generation::GenerationDisposition::CurrentGeneration => continue,
            crate::generation::GenerationDisposition::SupersededGeneration => {
                GraphProblemKind::SupersededGeneration
            }
            crate::generation::GenerationDisposition::AmbiguousGeneration => {
                GraphProblemKind::AmbiguousGeneration
            }
            crate::generation::GenerationDisposition::InvalidGenerationMetadata => {
                GraphProblemKind::InvalidGenerationMetadata
            }
        };
        analysis.fleet.problems.push(GraphProblem {
            kind,
            prs: finding.related_prs.clone(),
            message: finding.reason.clone(),
        });
    }
}

/// Resolve configured explicit priority and FIFO from one GitHub snapshot.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn resolve_admission(analysis: &GraphAnalysis, priority_labels: &[String]) -> AdmissionStatus {
    resolve_admission_with_generation(
        analysis,
        priority_labels,
        crate::generation::GenerationIntegrityStatus::default(),
    )
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn resolve_admission_with_generation(
    analysis: &GraphAnalysis,
    priority_labels: &[String],
    generation_integrity: crate::generation::GenerationIntegrityStatus,
) -> AdmissionStatus {
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

        let generation_finding = generation_integrity.finding(*number);
        // A structurally ineligible PR is not an admission attempt at all, so it
        // is reported with exact evidence but never becomes the canonical head
        // of the queue. Only rank-indeterminate priority metadata blocks order,
        // because canonical position genuinely cannot be computed. Eligible
        // candidates whose exact mechanical attempt fails still cannot be
        // leapfrogged: sync owns that generation-bound skip receipt.
        let generation_rejection = generation_finding.and_then(|finding| {
            (finding.disposition != crate::generation::GenerationDisposition::CurrentGeneration)
                .then(|| (finding.reason.clone(), false))
        });
        let rejection = if let Some((reason, blocks_order)) = generation_rejection {
            Some((format!("fail closed: {reason}"), blocks_order))
        } else if !invalid.is_empty() {
            Some((
                format!(
                    "fail closed: unknown priority label(s): {}",
                    invalid
                        .iter()
                        .map(|label| label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                true,
            ))
        } else if configured.len() > 1 {
            Some((
                format!(
                    "fail closed: conflicting priority labels: {}",
                    configured
                        .iter()
                        .map(|(label, _)| label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                true,
            ))
        } else if pull_request.draft {
            Some((
                "not an admission attempt: draft PR must be marked ready first".to_owned(),
                false,
            ))
        } else if pull_request.cross_repository {
            Some((
                "not an admission attempt: fork-only PR cannot be admitted to a caravan".to_owned(),
                false,
            ))
        } else if pull_request.auto_merge.enabled {
            Some((
                "not an admission attempt: candidate already has externally enabled auto-merge"
                    .to_owned(),
                false,
            ))
        } else {
            None
        };
        if let Some((reason, blocks_order)) = rejection {
            rejected.push(RejectedAdmissionCandidate {
                pr: *number,
                // Unknown priority metadata blocks before known attempts: its
                // rank cannot safely be guessed. Otherwise preserve the same
                // configured rank/FIFO key as selectable candidates.
                priority_rank: (configured.len() == 1).then(|| configured[0].1 + 1),
                created_at: pull_request.created_at.clone(),
                blocks_order,
                reason,
            });
            continue;
        }

        let created_at = pull_request.created_at.clone();
        // A red candidate is skipped, not elected, and specifically NOT treated as
        // an order-blocking rejection. Queueing behind red is guaranteed rework:
        // the failure must be fixed, fixing it rewrites the head, and everything
        // stacked behind is rebased anyway. So the queue advances past it and a
        // clean candidate can form the caravan instead of waiting on a red head.
        //
        // This mirrors `validate_candidate`, which refuses the same shape on the
        // `check` path. Both must agree: when only `check` refused, `cara status`
        // still elected the red PR as next_candidate while `cara check --pr N`
        // called it ineligible, and the two surfaces disagreed about the same PR
        // (observed on cacophony PR 2276).
        if !pull_request.has_label("caravan-force") && has_failing_check(pull_request) {
            skipped.push(SkippedAdmissionCandidate {
                pr: *number,
                priority_label: configured.first().map(|(label, _)| (*label).clone()),
                priority_rank: configured.first().map(|(_, rank)| rank + 1),
                created_at,
                reason: "candidate has a failing required check; queueing behind red only guarantees a later rebase, so the queue advances until it is fixed".to_owned(),
            });
            continue;
        }
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
        // A candidate Cara has already proven cannot merge into the exact
        // default branch is skipped rather than elected. Electing it starves
        // every clean candidate behind it, and no rerun resolves it: only its
        // owner can. This is deliberately NOT the same as the rejected-attempt
        // rule below, which keeps an owner's explicit `cara check --pr N`
        // rejection canonical. A mechanical conflict is not a decision anyone
        // made, so it must not inherit a decision's blocking authority.
        if analysis.fleet.problems.iter().any(|problem| {
            // Must match the kind graph analysis actually emits for an
            // unadmitted candidate. This guard silently died once already by
            // still naming `Incompatible` after the producer moved to
            // `CandidateIncompatible`, which reinstated the head-of-line stall
            // with every test still green.
            problem.kind.is_candidate_scoped() && problem.prs.contains(number)
        }) {
            skipped.push(SkippedAdmissionCandidate {
                pr: *number,
                priority_label: configured.first().map(|(label, _)| (*label).clone()),
                priority_rank: configured.first().map(|(_, rank)| rank + 1),
                created_at,
                reason: "proven mechanically incompatible with the exact current default branch; the owner must reconcile it, and no rerun changes that".to_owned(),
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
        .chain(
            rejected
                .iter()
                .filter(|candidate| candidate.blocks_order)
                .map(|candidate| {
                    (
                        candidate.priority_rank.unwrap_or(priority_labels.len() + 1),
                        candidate.created_at.is_none(),
                        candidate.created_at.clone().unwrap_or_default(),
                        candidate.pr,
                    )
                }),
        )
        .min()
        .map(|(_, _, _, pr)| pr);
    AdmissionStatus {
        policy: "Cacophony-shaped PRs first require unique current generation integrity. Structurally ineligible PRs (draft, fork-only, externally enabled auto-merge, superseded/ambiguous/invalid generation) are reported with exact reasons and excluded from ordering rather than wedging the queue, while unknown or conflicting configured priority labels block because canonical rank cannot be computed. Remaining attempts use explicit agent priority label (configured high to low), then FIFO by immutable provider created_at ascending with PR number ascending as equal-time tie-break; missing created_at falls back deterministically to PR number after timestamped peers; never LIFO; check/new preflight required and an eligible candidate whose exact mechanical attempt fails never causes automatic leapfrogging. This ordering binds automatic selection for both new-caravan and join intent; explicit owner intent naming one exact PR is evaluated separately and may attach ahead of unrelated unjoined rows without changing their canonical order".to_owned(),
        priority_labels: priority_labels.to_vec(),
        generation_integrity,
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

fn duration_millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// One fleet snapshot with a fresh deadline reserved for exact candidate work.
pub(crate) struct BoundRemoteCandidateStatus {
    pub status: StatusOutput,
    pub exact_deadline: std::time::Instant,
}

/// Discover the fleet and bind one exact remote candidate without checkout mutation.
pub(crate) fn status_for_remote_candidate(
    context: &AppContext,
    number: PrNumber,
) -> Result<StatusOutput, AppError> {
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(context.config.command_timeout_secs);
    Ok(status_for_remote_candidate_with_deadline(context, number, deadline, None)?.status)
}

/// Discover under the caller's fleet deadline, then reserve a fresh exact-candidate budget.
pub(crate) fn status_for_remote_candidate_with_deadline(
    context: &AppContext,
    number: PrNumber,
    discovery_deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<BoundRemoteCandidateStatus, AppError> {
    let status = status_with_deadline_and_budget(context, discovery_deadline, github_budget)?;
    bind_remote_candidate_from_status(context, status, number, github_budget)
}

/// Bind one selected PR from already-discovered fleet facts under a fresh budget.
/// This avoids repeating unrelated fleet compatibility during sync-owned admission.
pub(crate) fn bind_remote_candidate_from_status(
    context: &AppContext,
    mut status: StatusOutput,
    number: PrNumber,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<BoundRemoteCandidateStatus, AppError> {
    let exact_budget = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let exact_started = std::time::Instant::now();
    let exact_deadline = exact_started + exact_budget;
    // Re-read the selected provider PR after fleet discovery. Comparing the
    // complete snapshot closes the discovery/refetch race before exact Git
    // revision checks; merge compatibility subsequently verifies refs both
    // before and after its fetch.
    let provider_runner = crate::command::ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(exact_budget)
        .with_operation_deadline(exact_deadline);
    let provider_runner = github_budget.map_or(provider_runner.clone(), |budget| {
        provider_runner.with_github_request_budget(budget.clone())
    });
    let provider = crate::github::GitHubMutationAdapter::new(provider_runner.clone());
    let fresh = provider
        .refetch_pull_request(&status.repository, number)
        .map_err(|error| {
            let timeout = matches!(
                &error,
                crate::github::MutationError::Provider(DiscoveryError::Runner(
                    CommandRunError::Timeout { .. }
                ))
            );
            AppError::structured(
                if timeout {
                    ErrorCategory::Timeout
                } else {
                    ErrorCategory::ExecutionFailure
                },
                if timeout {
                    "candidate_refetch_timeout"
                } else {
                    "candidate_refetch_failed"
                },
                error.to_string(),
                Some(json!({
                    "stage": "exact_candidate",
                    "phase": "provider_refetch",
                    "pr": number,
                    "elapsed_ms": duration_millis(exact_started.elapsed()),
                    "deadline_ms": duration_millis(exact_budget),
                    "safe_next_action": "retry the read-only exact-candidate preflight",
                    "mutated": false,
                })),
            )
        })?;
    let refetch_elapsed = exact_started.elapsed();
    let discovered = status.analysis.pull_requests.get(&number).ok_or_else(|| {
        AppError::validation(
            "candidate_not_found",
            format!("PR #{number} is not an open provider candidate"),
        )
    })?;
    require_fresh_candidate(discovered, &fresh)?;
    let identity = GitHubDiscovery::new(provider_runner.clone())
        .merge_candidate_identity(
            &status.repository,
            &fresh,
            Some(&status.analysis.fleet.default_branch),
        )
        .map_err(|error| discovery_error(&error))?;
    status
        .merge_candidates
        .retain(|candidate| candidate.pr != number);
    status.merge_candidates.push(identity);
    status.current_pr = Some(number);
    status
        .provider_api
        .merge(provider_runner.github_api_telemetry());
    let exact_elapsed = exact_started.elapsed();
    let timing = status.timing.get_or_insert_with(|| StatusTiming {
        deadline_ms: 0,
        total_ms: 0,
        phases_ms: std::collections::BTreeMap::new(),
    });
    timing.deadline_ms = timing
        .deadline_ms
        .saturating_add(duration_millis(exact_budget));
    timing.total_ms = timing
        .total_ms
        .saturating_add(duration_millis(exact_elapsed));
    timing.phases_ms.insert(
        "exact_candidate_provider_refetch".to_owned(),
        duration_millis(refetch_elapsed),
    );
    timing.phases_ms.insert(
        "exact_candidate_merge_identity".to_owned(),
        duration_millis(exact_elapsed.saturating_sub(refetch_elapsed)),
    );
    Ok(BoundRemoteCandidateStatus {
        status,
        exact_deadline,
    })
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

/// Who chose the candidate a `check` receipt describes.
///
/// Naming an exact remote PR (`cara check --pr N`, with or without
/// `--tail-pr`/`--head-pr`) is deliberate owner intent for both `new` and
/// `join`. Operating on the checked-out PR is the owner's own PR, where
/// canonical position is evidence only. Neither is the automatic priority/FIFO
/// selection that binds sync and `next-candidate`.
const fn admission_selection(remote: bool) -> crate::admission::AdmissionSelection {
    if remote {
        crate::admission::AdmissionSelection::Explicit
    } else {
        crate::admission::AdmissionSelection::CheckedOut
    }
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
        let mut enrolled_intent = crate::admission::evaluate(
            &status.admission,
            &status.analysis,
            pull_request,
            None,
            admission_selection(remote),
        );
        enrolled_intent.record_preflight(true, eligible);
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
            admission_note: None,
            admission_intent: Some(enrolled_intent),
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
            squash_reconciliations: status.analysis.squash_reconciliations.clone(),
            problems: active_problems,
            initialization: status.initialization.clone(),
        };
        return eligible_or_error(output, remote);
    }

    // Seed from fleet problems, but never inherit another *unadmitted*
    // candidate's incompatibility. That is advisory evidence about a PR which is
    // in no caravan and blocks nothing, so letting it land here made one
    // conflicting unqueued PR mark every other candidate ineligible — including
    // the clean one that should have formed the first caravan.
    let mut problems = status
        .analysis
        .fleet
        .problems
        .iter()
        .filter(|problem| problem.kind.blocks_fleet() || problem.prs.contains(&current_pr))
        .cloned()
        .collect::<Vec<_>>();
    let mut ordering_note: Option<String> = None;
    validate_candidate(pull_request, &mut problems);
    // In physical membership mode the provider's synthetic merge ref is
    // advisory only: prepare/apply independently fetch and verify the exact PR
    // head plus current default/tail, prove merge-tree compatibility and range
    // topology, and push under force-with-lease. Requiring a fresh synthetic
    // parent here can deadlock the rewrite which would refresh that ref. Virtual
    // mode retains strict provider-candidate freshness.
    let candidate_stale_blocks_admission = candidate_stale && !status.rebase_on_join.enabled;
    if candidate_stale_blocks_admission {
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
    // Intent is resolved before FIFO canonical-candidate rejection. An
    // unresolved, missing, or ambiguous join target fails closed here and never
    // reaches the ordering relaxation below.
    let explicit_join = input.tail_pr.is_some() || input.head_pr.is_some();
    let target_caravan = explicit_join
        .then(|| resolve_target_caravan(status, input))
        .transpose()?;
    let mut admission_intent = crate::admission::evaluate(
        &status.admission,
        &status.analysis,
        pull_request,
        target_caravan,
        admission_selection(remote),
    );
    // Priority/FIFO binds automatic selection. Naming one exact remote PR is
    // deliberate owner intent for `new` and for `join` alike: canonical
    // position becomes evidence, and the candidate is admitted on its own
    // eligibility while every bypassed row is an unrelated unjoined
    // first-admission attempt that keeps its canonical order. A joined row, a
    // base-chain dependency, a rank-indeterminate row, or a candidate that is
    // not itself a current ordered attempt still fails closed.
    if remote && !canonical_candidate {
        let order = status.admission.next_candidate.map_or_else(
            || "no automatic priority/FIFO attempt exists".to_owned(),
            |first| format!("automatic priority/FIFO order would have selected PR #{first} first"),
        );
        // The note is derived from the same typed decision that gates the
        // receipt, so the CLI note, the decision, and mutation always agree.
        ordering_note = Some(format!(
            "explicit {} admission intent for PR #{current_pr}; {order}; {}",
            admission_intent.intent.name(),
            admission_intent.reason,
        ));
        if !admission_intent.order_permits_admission() {
            problems.push(GraphProblem {
                kind: GraphProblemKind::Unknown,
                prs: vec![current_pr],
                message: status.admission.next_candidate.map_or_else(
                    || format!("candidate is not selectable because no canonical admission attempt exists; {}", admission_intent.reason),
                    |first| format!("explicit admission intent fails closed on PR #{first}; {}", admission_intent.reason),
                ),
            });
        }
    }
    let order_admits = canonical_candidate || admission_intent.order_permits_admission();
    let mut reports = Vec::new();
    let mut reconciliations = Vec::new();

    if !explicit_join {
        if !pull_request.cross_repository {
            check_new(
                status,
                pull_request,
                checker,
                &mut reports,
                &mut problems,
                &mut reconciliations,
            )?;
        }
        let eligible = problems.is_empty();
        admission_intent.record_preflight(
            reports
                .iter()
                .all(|report| report.outcome == CompatibilityOutcome::Clean),
            eligible,
        );
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
                order: if order_admits {
                    ActionOrder::Canonical
                } else {
                    ActionOrder::NonCanonical
                },
                admission: if admission_rejection.is_some() {
                    AdmissionDecision::Rejected
                } else {
                    AdmissionDecision::Accepted
                },
                freshness: if candidate_stale_blocks_admission {
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
                admission_note: ordering_note.clone(),
                admission_intent: Some(admission_intent),
                next_action,
                caravan_id: Some(current_pr),
                target_pr: None,
                eligible,
                compatibility: reports,
                squash_reconciliations: reconciliations,
                problems,
                initialization: status.initialization.clone(),
            },
            remote,
        );
    }

    let target_caravan = target_caravan.expect("explicit join resolved its target");
    let tail_number = target_caravan.tail().expect("caravans are non-empty");
    let tail = status
        .analysis
        .pull_requests
        .get(&tail_number)
        .expect("derived tail has a snapshot");
    if !pull_request.cross_repository {
        record_report(
            checker,
            checker.check(&pull_request.head, &tail.head)?,
            vec![tail_number, current_pr],
            "candidate does not merge cleanly after the selected tail",
            &mut reports,
            &mut problems,
            &mut reconciliations,
        )?;
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
                checker,
                checker.check(&head.head, &pull_request.head)?,
                vec![head_number, current_pr],
                "another caravan head cannot attach after the proposed new tail",
                &mut reports,
                &mut problems,
                &mut reconciliations,
            )?;
        }
    }

    let eligible = problems.is_empty();
    admission_intent.record_preflight(
        reports
            .iter()
            .all(|report| report.outcome == CompatibilityOutcome::Clean),
        eligible,
    );
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
            order: if order_admits {
                ActionOrder::Canonical
            } else {
                ActionOrder::NonCanonical
            },
            admission: if admission_rejection.is_some() {
                AdmissionDecision::Rejected
            } else {
                AdmissionDecision::Accepted
            },
            freshness: if candidate_stale_blocks_admission {
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
            admission_note: ordering_note.clone(),
            admission_intent: Some(admission_intent),
            next_action,
            caravan_id: Some(target_caravan.id),
            target_pr: Some(tail_number),
            eligible,
            compatibility: reports,
            squash_reconciliations: reconciliations,
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
    reconciliations: &mut Vec<SquashEquivalenceReport>,
) -> Result<(), AppError> {
    record_report(
        checker,
        checker.check(&pull_request.head, &status.analysis.fleet.default_branch)?,
        vec![pull_request.number],
        "candidate new head does not merge cleanly into the default branch",
        reports,
        problems,
        reconciliations,
    )?;
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
            checker,
            checker.check(&pull_request.head, &tail.head)?,
            vec![pull_request.number, tail_number],
            "candidate head cannot attach after an existing caravan tail",
            reports,
            problems,
            reconciliations,
        )?;
        record_report(
            checker,
            checker.check(&head.head, &pull_request.head)?,
            vec![head_number, pull_request.number],
            "existing caravan head cannot attach after the candidate tail",
            reports,
            problems,
            reconciliations,
        )?;
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
    // Queueing behind red is guaranteed rework. A failing check has to be fixed,
    // fixing it rewrites the head, and every member stacked behind it is then
    // rebased anyway — so admitting a red candidate buys nothing and costs the
    // whole tail a re-stitch. `caravan-force` remains the audited override.
    if !pull_request.has_label("caravan-force") && has_failing_check(pull_request) {
        messages.insert("candidate PR has a failing required check; fix it before admission rather than stacking work behind it");
    }
    for message in messages {
        problems.push(GraphProblem {
            kind: GraphProblemKind::Unknown,
            prs: vec![pull_request.number],
            message: message.to_owned(),
        });
    }
}

/// Whether any check on this exact head is a hard failure.
///
/// Deliberately mirrors the sync-side classification: a missing or empty
/// conclusion is `Unknown`, and Unknown counts as failure rather than success,
/// because an absent result is not a passing result.
fn has_failing_check(pull_request: &PullRequestSnapshot) -> bool {
    pull_request.checks.iter().any(|check| {
        matches!(
            check.state,
            crate::model::CheckState::Failure
                | crate::model::CheckState::Cancelled
                | crate::model::CheckState::TimedOut
                | crate::model::CheckState::ActionRequired
        )
    })
}

/// Record one pairwise report plus, for a non-clean pair, the exact
/// squash-equivalence evidence for the same revisions.
///
/// Collecting it only on conflict keeps healthy preflights free of extra Git
/// work, and it never changes the outcome: a conflict stays a conflict until a
/// separately reviewed operation acts on the proof.
fn record_report(
    checker: &impl CompatibilityChecker,
    report: CompatibilityReport,
    prs: Vec<PrNumber>,
    message: &str,
    reports: &mut Vec<CompatibilityReport>,
    problems: &mut Vec<GraphProblem>,
    reconciliations: &mut Vec<SquashEquivalenceReport>,
) -> Result<(), AppError> {
    if report.outcome != CompatibilityOutcome::Clean {
        problems.push(GraphProblem {
            kind: GraphProblemKind::Incompatible,
            prs,
            message: message.to_owned(),
        });
        if let Some(reconciliation) =
            checker.squash_equivalence(&report.candidate, &report.target)?
        {
            reconciliations.push(reconciliation);
        }
    }
    reports.push(report);
    Ok(())
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
enum AdmissionDecision {
    Accepted,
    Rejected,
}

/// Whether intent-aware admission ordering permits this candidate now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionOrder {
    Canonical,
    NonCanonical,
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
            merge_state_status: None,
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
            // Absent configuration keeps the historical provider-native actor.
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
            generation_facts: Vec::new(),
            observed_at: None,
        };
        let checker = clean_checker;
        let analysis =
            analyze_for_actor(&snapshot, &checker, crate::model::HeadMergeActor::default())
                .unwrap();
        StatusOutput {
            config_provenance: None,
            head_merge: HeadMergeStatus::default(),
            runtime: crate::read::RuntimeProvenance::default(),
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
            sync_budget: crate::sync::SyncBudgetStatus::default(),
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
    fn superseded_generation_is_excluded_without_blocking_canonical_successor() {
        let labels = vec!["caravan-priority:high".to_owned()];
        let mut older = pr(2107, "old", "main", false);
        older.labels.insert(labels[0].clone());
        older.created_at = Some("2026-07-23T01:50:32Z".to_owned());
        let mut newer = pr(2123, "new", "main", false);
        newer.labels.insert(labels[0].clone());
        newer.created_at = Some("2026-07-23T18:34:41Z".to_owned());
        let status = status(older.clone(), vec![newer.clone()]);
        let generation = crate::generation::analyze(
            &[
                crate::model::PullRequestGenerationFact {
                    pr: older.number,
                    provider_head: older.head.oid.clone(),
                    created_at: older.created_at.clone(),
                    provenance: Some(crate::model::CacophonyGenerationProvenance {
                        generation: format!("old-pr-g{}", "a".repeat(40)),
                        agent: "android-agent".to_owned(),
                        source_head: crate::model::CommitOid("a".repeat(40)),
                        bead_ids: BTreeSet::from(["bd-c7440c".to_owned()]),
                        stack_base: "main".to_owned(),
                        stack_state: "root".to_owned(),
                    }),
                    metadata_error: None,
                    supersedes: BTreeSet::new(),
                },
                crate::model::PullRequestGenerationFact {
                    pr: newer.number,
                    provider_head: newer.head.oid.clone(),
                    created_at: newer.created_at.clone(),
                    provenance: Some(crate::model::CacophonyGenerationProvenance {
                        generation: format!("new-pr-g{}", "b".repeat(40)),
                        agent: "android-agent".to_owned(),
                        source_head: crate::model::CommitOid("b".repeat(40)),
                        bead_ids: BTreeSet::from(["bd-c7440c".to_owned(), "bd-4734d1".to_owned()]),
                        stack_base: "main".to_owned(),
                        stack_state: "root".to_owned(),
                    }),
                    metadata_error: None,
                    supersedes: BTreeSet::new(),
                },
            ],
            |_base, _head| crate::generation::CommitRelation::Ahead,
        );
        let admission = resolve_admission_with_generation(&status.analysis, &labels, generation);
        assert_eq!(admission.next_candidate, Some(newer.number));
        assert_eq!(admission.candidates[0].pr, newer.number);
        let excluded = admission
            .rejected
            .iter()
            .find(|candidate| candidate.pr == older.number)
            .unwrap();
        assert!(!excluded.blocks_order);
        assert!(excluded.reason.contains("superseded"));
    }

    #[test]
    fn active_superseded_generation_becomes_a_graph_stop() {
        let older = pr(2107, "old", "main", true);
        let newer = pr(2123, "new", "main", false);
        let mut status = status(older.clone(), vec![newer.clone()]);
        let old_fact = crate::model::PullRequestGenerationFact {
            pr: older.number,
            provider_head: older.head.oid.clone(),
            created_at: older.created_at.clone(),
            provenance: Some(crate::model::CacophonyGenerationProvenance {
                generation: format!("old-pr-g{}", "a".repeat(40)),
                agent: "android-agent".to_owned(),
                source_head: crate::model::CommitOid("a".repeat(40)),
                bead_ids: BTreeSet::from(["bd-c7440c".to_owned()]),
                stack_base: "main".to_owned(),
                stack_state: "root".to_owned(),
            }),
            metadata_error: None,
            supersedes: BTreeSet::new(),
        };
        let mut new_fact = old_fact.clone();
        new_fact.pr = newer.number;
        new_fact.provider_head = newer.head.oid.clone();
        new_fact.created_at = newer.created_at.clone();
        new_fact.provenance.as_mut().unwrap().generation = format!("new-pr-g{}", "b".repeat(40));
        new_fact.provenance.as_mut().unwrap().source_head = crate::model::CommitOid("b".repeat(40));
        new_fact.supersedes.insert(older.number);
        let generation = crate::generation::analyze(&[old_fact, new_fact], |_base, _head| {
            unreachable!("reviewed link is complete")
        });
        apply_generation_graph_problems(&mut status.analysis, &generation);
        assert!(status.analysis.fleet.problems.iter().any(|problem| {
            problem.kind == GraphProblemKind::SupersededGeneration
                && problem.prs.contains(&older.number)
        }));
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

    /// Measured on live cacophony: PR 2279 was CLEAN and correctly elected, yet
    /// the forge reported `mergeStateStatus=BLOCKED` for it exactly as it did
    /// for the red PR 2276. BLOCKED means "protection not yet satisfied", which
    /// includes required checks that are merely still running.
    ///
    /// So the forge verdict must NOT gate admission: doing so would refuse every
    /// candidate whose CI has not finished and stall the queue completely. It is
    /// consulted only at merge time, where the checks are already proven green.
    #[test]
    fn a_blocked_forge_verdict_does_not_bar_admission() {
        let mut blocked = pr(20, "clean-but-blocked", "main", false);
        blocked.merge_state_status = Some("BLOCKED".to_owned());
        let mut problems = Vec::new();

        validate_candidate(&blocked, &mut problems);

        assert!(
            problems.is_empty(),
            "a forge BLOCKED verdict must not bar admission, or no candidate with running CI could ever be admitted: {problems:?}"
        );
    }

    /// Two surfaces must not disagree about the same PR. On cacophony PR 2276,
    /// `check --pr 2276` reported ineligible while `status` still elected it as
    /// `next_candidate`, because the CI refusal lived only on the check path.
    /// Ordering now applies the same rule, and skips rather than blocks so the
    /// queue advances to a clean candidate instead of waiting on a red head.
    #[test]
    fn a_red_candidate_is_skipped_so_the_queue_advances() {
        let mut red = pr(10, "red", "main", false);
        red.checks.push(crate::model::CheckSnapshot {
            name: "Rust Check & Lint work".to_owned(),
            state: crate::model::CheckState::Failure,
            provider_state: Some("FAILURE".to_owned()),
            details_url: None,
        });
        let green = pr(20, "green", "main", false);
        let status = status(red, vec![green]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;

        let admission = resolve_admission(&status.analysis, &labels);

        assert_eq!(
            admission.next_candidate,
            Some(PrNumber(20)),
            "a red PR must not hold the admission front"
        );
        assert!(
            admission
                .skipped
                .iter()
                .any(|candidate| candidate.pr == PrNumber(10)),
            "the red candidate must be visibly skipped, not silently dropped: {:?}",
            admission.skipped
        );
    }

    /// Operator ruling from cacophony PR 2276: a failing check MUST bar
    /// admission, because queueing behind red is guaranteed rework. The failure
    /// has to be fixed, fixing it rewrites the head, and every member stacked
    /// behind it is rebased anyway, so admitting red buys nothing and costs the
    /// whole tail a re-stitch.
    #[test]
    fn a_candidate_with_a_failing_check_is_not_admissible() {
        let mut red = pr(10, "red", "main", false);
        red.checks.push(crate::model::CheckSnapshot {
            name: "Rust Check & Lint work".to_owned(),
            state: crate::model::CheckState::Failure,
            provider_state: Some("FAILURE".to_owned()),
            details_url: None,
        });
        let mut problems = Vec::new();

        validate_candidate(&red, &mut problems);

        assert!(
            problems
                .iter()
                .any(|problem| problem.message.contains("failing required check")),
            "a red candidate must be refused admission: {problems:?}"
        );
    }

    /// The audited override still works: `caravan-force` is how an operator
    /// takes responsibility for admitting a red head deliberately.
    #[test]
    fn caravan_force_still_admits_a_red_candidate() {
        let mut forced = pr(11, "forced", "main", false);
        forced.labels.insert("caravan-force".to_owned());
        forced.checks.push(crate::model::CheckSnapshot {
            name: "Rust Check & Lint work".to_owned(),
            state: crate::model::CheckState::Failure,
            provider_state: Some("FAILURE".to_owned()),
            details_url: None,
        });
        let mut problems = Vec::new();

        validate_candidate(&forced, &mut problems);

        assert!(
            !problems
                .iter()
                .any(|problem| problem.message.contains("failing required check")),
            "caravan-force must remain the audited override: {problems:?}"
        );
    }

    /// A check still running is not a failure. Refusing on pending would stall
    /// every candidate for the duration of its own CI.
    #[test]
    fn a_pending_check_does_not_bar_admission() {
        let mut pending = pr(12, "pending", "main", false);
        pending.checks.push(crate::model::CheckSnapshot {
            name: "Rust Fast Tests work".to_owned(),
            state: crate::model::CheckState::InProgress,
            provider_state: None,
            details_url: None,
        });
        let mut problems = Vec::new();

        validate_candidate(&pending, &mut problems);

        assert!(
            !problems
                .iter()
                .any(|problem| problem.message.contains("failing required check")),
            "a pending check is not a failure: {problems:?}"
        );
    }

    /// Live operator report (PR 2234): Cara proved the leading candidate could
    /// not merge into the default branch, reported it, and then elected it
    /// anyway, holding the whole queue behind a PR no rerun can fix. A
    /// mechanical conflict is not a decision anyone made, so unlike an owner's
    /// explicit rejection it must not inherit blocking authority.
    #[test]
    fn a_mechanically_incompatible_candidate_skips_instead_of_holding_the_front() {
        let conflicted = pr(10, "conflicted", "main", false);
        let later = pr(20, "later", "main", false);
        let mut status = status(conflicted, vec![later]);
        status.analysis.fleet.problems.push(GraphProblem {
            kind: GraphProblemKind::CandidateIncompatible,
            prs: vec![PrNumber(10)],
            message: "does not merge cleanly into the current default branch".to_owned(),
        });
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;

        let admission = resolve_admission(&status.analysis, &labels);

        assert_eq!(
            admission.next_candidate,
            Some(PrNumber(20)),
            "a clean later candidate must not be starved by a proven conflict"
        );
        assert!(
            admission
                .skipped
                .iter()
                .any(|candidate| candidate.pr == PrNumber(10)),
            "the conflicting candidate must be reported as skipped, not silently dropped: {:?}",
            admission.skipped
        );
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
    fn ineligible_candidates_are_reported_without_wedging_eligible_admission() {
        let mut draft = pr(10, "draft", "main", false);
        draft.draft = true;
        let mut fork = pr(20, "fork", "main", false);
        fork.cross_repository = true;
        let mut external = pr(30, "external", "main", false);
        external.auto_merge = AutoMergeState::squash();
        let eligible = pr(40, "eligible", "main", false);
        let status = status(draft, vec![fork, external, eligible]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;

        let admission = resolve_admission(&status.analysis, &labels);

        assert_eq!(
            admission.next_candidate,
            Some(PrNumber(40)),
            "structurally ineligible PRs must not starve an eligible candidate"
        );
        assert_eq!(admission.candidates.len(), 1);
        assert!(
            admission
                .rejected
                .iter()
                .all(|candidate| !candidate.blocks_order
                    && candidate.reason.contains("not an admission attempt"))
        );
        assert_eq!(
            admission
                .rejected
                .iter()
                .map(|candidate| candidate.pr)
                .collect::<Vec<_>>(),
            vec![PrNumber(20), PrNumber(30)],
            "drafts are excluded before admission ordering; fork/auto-merge PRs are reported"
        );
    }

    #[test]
    fn ambiguous_generation_reports_exactly_without_blocking_other_owners() {
        let ambiguous_first = pr(2107, "old", "main", false);
        let ambiguous_second = pr(2109, "newer", "main", false);
        let unrelated = pr(2115, "unrelated", "main", false);
        let status = status(
            ambiguous_first.clone(),
            vec![ambiguous_second.clone(), unrelated.clone()],
        );
        let fact = |pr: &crate::model::PullRequestSnapshot, source: char| {
            crate::model::PullRequestGenerationFact {
                pr: pr.number,
                provider_head: pr.head.oid.clone(),
                created_at: pr.created_at.clone(),
                provenance: Some(crate::model::CacophonyGenerationProvenance {
                    generation: format!("agent/x-pr-g{}", source.to_string().repeat(40)),
                    agent: "android-agent".to_owned(),
                    source_head: crate::model::CommitOid(source.to_string().repeat(40)),
                    bead_ids: BTreeSet::from(["bd-c7440c".to_owned()]),
                    stack_base: "main".to_owned(),
                    stack_state: "root".to_owned(),
                }),
                metadata_error: None,
                supersedes: BTreeSet::new(),
            }
        };
        let generation = crate::generation::analyze(
            &[fact(&ambiguous_first, 'a'), fact(&ambiguous_second, 'b')],
            |_base, _head| crate::generation::CommitRelation::Diverged,
        );
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;

        let admission = resolve_admission_with_generation(&status.analysis, &labels, generation);

        assert_eq!(admission.next_candidate, Some(unrelated.number));
        assert_eq!(admission.rejected.len(), 2);
        assert!(
            admission
                .rejected
                .iter()
                .all(|candidate| !candidate.blocks_order
                    && candidate.reason.contains("divergent or unproved"))
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

    /// bd-7546ea live pair: PR2228 head ccc3af5c is the direct parent of
    /// PR2235 head f0bae700, so an exact local proof must classify it as
    /// strict-prefix ancestry, never as divergence.
    #[test]
    fn local_ancestry_proves_direct_parent_child_instead_of_divergence() {
        let directory = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(directory.path())
                .args(args)
                .output()
                .expect("git fixture command");
            assert!(output.status.success(), "git {args:?} failed");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        run(&["init", "--quiet"]);
        run(&["config", "user.name", "Caravan Test"]);
        run(&["config", "user.email", "caravan@example.invalid"]);
        std::fs::write(directory.path().join("a"), "a\n").unwrap();
        run(&["add", "a"]);
        run(&["commit", "-m", "parent"]);
        let parent = crate::model::CommitOid(run(&["rev-parse", "HEAD"]));
        std::fs::write(directory.path().join("b"), "b\n").unwrap();
        run(&["add", "b"]);
        run(&["commit", "-m", "child"]);
        let child = crate::model::CommitOid(run(&["rev-parse", "HEAD"]));

        assert_eq!(
            local_commit_relation(directory.path(), &parent, &child),
            Some(crate::generation::CommitRelation::Ahead)
        );
        assert_eq!(
            local_commit_relation(directory.path(), &child, &parent),
            Some(crate::generation::CommitRelation::Behind)
        );
        assert_eq!(
            local_commit_relation(directory.path(), &parent, &parent),
            Some(crate::generation::CommitRelation::Identical)
        );
        // An absent object cannot be proven locally and must stay unproved.
        assert_eq!(
            local_commit_relation(
                directory.path(),
                &parent,
                &crate::model::CommitOid("0".repeat(40))
            ),
            None
        );
    }

    /// Reviewed operator resolution choice-019f9d34 (bd-afa02d) narrowed the
    /// relaxation to *intent* rather than blanket queue position. bd-7099e8
    /// restores the reviewed 0.0.9 shape it regressed: naming one exact remote
    /// PR is deliberate owner intent for `new` as well as `join`, so an
    /// unadmitted earlier unrelated row cannot wedge another owner.
    /// bd-d7aae7: next-candidate must name one exact command, and when nothing
    /// is selectable it must point at whatever blocks canonical order rather
    /// than leaving the operator to infer a fault.
    #[test]
    fn next_candidate_names_one_actionable_command() {
        let mut admission = AdmissionStatus {
            policy: String::new(),
            priority_labels: Vec::new(),
            generation_integrity: crate::generation::GenerationIntegrityStatus::default(),
            candidates: Vec::new(),
            skipped: Vec::new(),
            rejected: Vec::new(),
            next_candidate: Some(PrNumber(2113)),
        };

        let next = next_admission_command(&admission);
        assert_eq!(next.command, "cara check --pr 2113");
        assert_eq!(next.pr, Some(PrNumber(2113)));
        assert!(next.reason.contains("canonical first"));

        admission.next_candidate = None;
        admission.rejected.push(RejectedAdmissionCandidate {
            pr: PrNumber(2117),
            priority_rank: None,
            created_at: None,
            blocks_order: true,
            reason: "fail closed: conflicting priority labels".to_owned(),
        });

        let blocked = next_admission_command(&admission);
        assert_eq!(blocked.command, "cara check --pr 2117");
        assert_eq!(blocked.pr, Some(PrNumber(2117)));
        assert!(blocked.reason.contains("blocks canonical order"));

        admission.rejected.clear();
        let idle = next_admission_command(&admission);
        assert_eq!(idle.command, "cara status");
        assert_eq!(idle.pr, None);
    }

    #[test]
    fn explicit_remote_new_intent_is_admitted_with_ordering_evidence() {
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
        .expect("explicit remote new intent is admissible");
        assert!(output.eligible, "problems: {:?}", output.problems);
        assert!(output.problems.is_empty());
        assert!(!output.canonical_candidate);
        assert_eq!(output.next_action, CandidateNextAction::New);
        let note = output.admission_note.clone().expect("ordering evidence");
        assert!(note.contains("PR #20"));
        assert!(note.contains("PR #10"));
        let intent = output
            .admission_intent
            .clone()
            .expect("typed decision is emitted");
        assert_eq!(intent.intent, crate::admission::AdmissionIntent::New);
        assert_eq!(
            intent.selection,
            crate::admission::AdmissionSelection::Explicit
        );
        assert_eq!(
            intent.outcome,
            crate::admission::AdmissionOrderOutcome::ExplicitAheadOfUnjoined
        );
        assert_eq!(intent.bypassed_unjoined_prs, vec![PrNumber(10)]);
        assert!(
            note.contains(&intent.reason),
            "the human note is derived from the typed decision so they cannot disagree"
        );
        assert_eq!(
            status.admission.next_candidate,
            Some(PrNumber(10)),
            "the bypassed row keeps its canonical first-admission position"
        );
        let json = serde_json::to_value(&output).expect("remote receipt serializes");
        assert_eq!(json["next_action"], "new");
        assert_eq!(json["candidate"]["number"], 20);
        assert_eq!(json["canonical_candidate"], false);
        assert_eq!(json["admission_intent"]["selection"], "explicit");
        assert!(json.get("merge_candidate").is_none());
    }

    /// The same non-canonical PR is admissible the moment it declares explicit
    /// join intent to a valid live target, while the bypassed row stays ordered.
    #[test]
    fn explicit_remote_join_intent_admits_the_same_noncanonical_pr() {
        let root = pr(1, "root", "main", true);
        let first = pr(10, "first", "main", false);
        let second = pr(20, "second", "main", false);
        let status = status(second, vec![root, first]);
        assert_eq!(status.admission.next_candidate, Some(PrNumber(10)));

        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(20),
                tail_pr: Some(1),
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("explicit join intent is admissible");

        assert!(output.eligible, "problems: {:?}", output.problems);
        assert_eq!(output.next_action, CandidateNextAction::Join);
        assert!(!output.canonical_candidate);
        assert!(output.admission_note.is_some());
        let intent = output.admission_intent.expect("typed decision is emitted");
        assert_eq!(
            intent.outcome,
            crate::admission::AdmissionOrderOutcome::ExplicitAheadOfUnjoined
        );
        assert_eq!(intent.bypassed_unjoined_prs, vec![PrNumber(10)]);
        assert_eq!(
            status.admission.next_candidate,
            Some(PrNumber(10)),
            "the bypassed row keeps its canonical first-admission position"
        );
    }

    /// A local `check` on the owner's own checked-out PR is checked-out owner
    /// selection: canonical order is evidence, never a gate, exactly as it was
    /// in 0.0.8/0.0.9/0.0.10. The typed decision says so instead of claiming a
    /// block the receipt does not apply.
    #[test]
    fn local_checked_out_receipt_reports_order_as_evidence_only() {
        let candidate = pr(2179, "child", "parent", false);
        let status = status(
            candidate,
            vec![
                pr(2050, "unrelated", "main", false),
                pr(2100, "parent", "main", false),
            ],
        );

        let output = check_analysis(&status, &CheckInput::default(), &clean_checker)
            .expect("a local owner check is never gated by canonical order");

        assert!(output.eligible, "problems: {:?}", output.problems);
        assert_eq!(output.next_action, CandidateNextAction::New);
        assert!(
            output.admission_note.is_none(),
            "ordering evidence is a remote-selection note"
        );
        let intent = output.admission_intent.expect("typed decision is emitted");
        assert_eq!(
            intent.selection,
            crate::admission::AdmissionSelection::CheckedOut
        );
        assert_eq!(
            intent.outcome,
            crate::admission::AdmissionOrderOutcome::OwnerSelected
        );
        assert!(
            intent.order_permits_admission(),
            "the decision must agree with the receipt it is bound to"
        );
        assert_eq!(intent.blocking_prs, vec![PrNumber(2100)]);
        assert!(intent.reason.contains("evidence only"));
    }

    #[test]
    fn ineligible_noncanonical_pr_still_fails_closed() {
        let first = pr(10, "first", "main", false);
        let mut draft = pr(20, "second", "main", false);
        draft.draft = true;
        let status = status(first, vec![draft]);
        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(20),
                tail_pr: None,
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("rejection remains an inspectable receipt");
        assert!(!output.eligible);
        assert_ne!(output.next_action, CandidateNextAction::New);
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
                compared_base: None,
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
    fn physical_admission_uses_exact_git_proof_when_synthetic_base_is_stale() {
        let candidate = pr(9, "nine", "main", false);
        let mut status = status(candidate.clone(), Vec::new());
        status.rebase_on_join = RebaseOnJoinStatus {
            enabled: true,
            state: "enabled".to_owned(),
            config_path: ".caravan/config.yaml".to_owned(),
            required_action: None,
        };
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
                freshness: crate::model::MergeCandidateFreshness::StaleBase,
                compared_base: None,
                stale_base: true,
                stale_head: false,
                stale_reasons: vec!["synthetic base parent is stale".to_owned()],
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
        .expect("physical mode owns a fresh exact Git/lease preflight");

        assert!(output.eligible);
        assert_eq!(output.next_action, CandidateNextAction::New);
        assert!(output.problems.is_empty());
        assert_eq!(
            output
                .merge_candidate
                .as_ref()
                .map(|identity| identity.freshness),
            Some(crate::model::MergeCandidateFreshness::StaleBase)
        );
    }

    /// Explicit join intent attaches ahead of an older unrelated unjoined row
    /// while that row keeps its canonical first-admission position.
    #[test]
    fn explicit_join_is_admitted_ahead_of_older_unjoined_fifo_rows() {
        let candidate = pr(2179, "green", "main", false);
        let status = status(
            candidate,
            vec![
                pr(1, "root", "main", true),
                pr(2113, "old-unjoined", "main", false),
            ],
        );
        assert_eq!(status.admission.next_candidate, Some(PrNumber(2113)));

        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2179),
                tail_pr: Some(1),
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("explicit join intent is evaluated before FIFO rejection");

        assert!(output.eligible, "problems: {:?}", output.problems);
        assert_eq!(output.next_action, CandidateNextAction::Join);
        assert_eq!(output.mode, CheckMode::JoinTail);
        assert!(!output.canonical_candidate);
        let intent = output.admission_intent.expect("typed decision is emitted");
        assert_eq!(intent.intent, crate::admission::AdmissionIntent::Join);
        assert_eq!(
            intent.outcome,
            crate::admission::AdmissionOrderOutcome::ExplicitAheadOfUnjoined
        );
        assert_eq!(intent.target_caravan, Some(PrNumber(1)));
        assert_eq!(intent.bypassed_unjoined_prs, vec![PrNumber(2113)]);
        assert!(intent.blocking_prs.is_empty());
        assert!(intent.compatibility_clean && intent.preflight_clean);
        assert!(!intent.provider_mutated && !intent.idempotent);
        assert_eq!(
            status.admission.next_candidate,
            Some(PrNumber(2113)),
            "bypassed FIFO rows keep their canonical order"
        );
    }

    /// FIFO still governs *automatic* selection: the ordered attempt list and
    /// canonical candidate are unchanged by any explicit owner receipt.
    #[test]
    fn explicit_intent_never_reorders_the_automatic_fifo_queue() {
        let candidate = pr(2179, "green", "main", false);
        let status = status(candidate, vec![pr(2113, "old-unjoined", "main", false)]);
        let before = status.admission.clone();

        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2179),
                tail_pr: None,
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("explicit new intent is admissible");

        assert!(output.eligible, "problems: {:?}", output.problems);
        assert_eq!(output.next_action, CandidateNextAction::New);
        assert_eq!(status.admission, before, "admission ordering is read-only");
        assert_eq!(status.admission.next_candidate, Some(PrNumber(2113)));
        assert_eq!(
            status
                .admission
                .candidates
                .iter()
                .map(|row| row.pr)
                .collect::<Vec<_>>(),
            vec![PrNumber(2113), PrNumber(2179)],
            "the canonical order the automatic surface publishes is unchanged"
        );
        // The separate automatic-selection axis still fails closed on the same
        // non-canonical candidate.
        let automatic = crate::admission::evaluate(
            &status.admission,
            &status.analysis,
            &status.analysis.pull_requests[&PrNumber(2179)],
            None,
            crate::admission::AdmissionSelection::Automatic,
        );
        assert_eq!(
            automatic.outcome,
            crate::admission::AdmissionOrderOutcome::BlockedByOrder
        );
        assert!(!automatic.order_permits_admission());
    }

    /// Cacophony generation4 PR2213 A/B shape: zero caravans, one older
    /// unadmitted unrelated row, explicit `cara check --pr 2213`. Reviewed
    /// 0.0.9 returned eligible/new with no problems; 0.0.10 regressed it to
    /// reject. This pins the reviewed shape byte for byte.
    #[test]
    fn cacophony_generation4_pr2213_explicit_new_matches_the_reviewed_ab_shape() {
        let candidate = pr(2213, "generation4", "main", false);
        let status = status(candidate, vec![pr(2113, "old-unjoined", "main", false)]);
        assert_eq!(status.admission.next_candidate, Some(PrNumber(2113)));

        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2213),
                tail_pr: None,
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("reviewed 0.0.9 admitted this explicit intent");

        assert!(output.eligible);
        assert_eq!(output.next_action, CandidateNextAction::New);
        assert!(output.problems.is_empty());
        assert_eq!(output.mode, CheckMode::NewCaravan);
        assert!(!output.canonical_candidate);
        assert!(output.admission_note.is_some());
        let intent = output.admission_intent.expect("typed decision is emitted");
        assert_eq!(
            intent.outcome,
            crate::admission::AdmissionOrderOutcome::ExplicitAheadOfUnjoined
        );
        assert_eq!(intent.bypassed_unjoined_prs, vec![PrNumber(2113)]);
        assert!(intent.blocking_prs.is_empty());
    }

    /// Cacophony PR2215 A/B shape: the same front with a live caravan present.
    /// Explicit `new` and explicit `join --tail-pr` are both admitted, and both
    /// leave PR #2113 canonical.
    #[test]
    fn cacophony_pr2215_explicit_new_and_join_match_the_reviewed_ab_shape() {
        let candidate = pr(2215, "green", "main", false);
        let status = status(
            candidate,
            vec![
                pr(1, "root", "main", true),
                pr(2113, "old-unjoined", "main", false),
            ],
        );
        assert_eq!(status.admission.next_candidate, Some(PrNumber(2113)));

        for (input, expected_action, expected_mode) in [
            (
                CheckInput {
                    pr: Some(2215),
                    tail_pr: None,
                    head_pr: None,
                },
                CandidateNextAction::New,
                CheckMode::NewCaravan,
            ),
            (
                CheckInput {
                    pr: Some(2215),
                    tail_pr: Some(1),
                    head_pr: None,
                },
                CandidateNextAction::Join,
                CheckMode::JoinTail,
            ),
        ] {
            let output = check_analysis(&status, &input, &clean_checker)
                .expect("explicit owner intent is admissible");

            assert!(output.eligible, "problems: {:?}", output.problems);
            assert!(output.problems.is_empty());
            assert_eq!(output.next_action, expected_action);
            assert_eq!(output.mode, expected_mode);
            assert!(!output.canonical_candidate);
            let intent = output.admission_intent.expect("typed decision is emitted");
            assert_eq!(
                intent.outcome,
                crate::admission::AdmissionOrderOutcome::ExplicitAheadOfUnjoined
            );
            assert_eq!(intent.bypassed_unjoined_prs, vec![PrNumber(2113)]);
            assert_eq!(
                status.admission.next_candidate,
                Some(PrNumber(2113)),
                "bypassed FIFO rows keep their canonical order"
            );
        }
    }

    /// Explicit intent still fails closed on rows it may not pass: a base-chain
    /// dependency of the candidate is never bypassed for `new` or `join`.
    #[test]
    fn explicit_new_intent_still_fails_closed_on_a_dependency_row() {
        let candidate = pr(2179, "child", "parent", false);
        let status = status(
            candidate,
            vec![
                pr(2050, "unrelated", "main", false),
                pr(2100, "parent", "main", false),
            ],
        );

        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2179),
                tail_pr: None,
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("non-admissible order is an inspectable receipt");

        assert!(!output.eligible);
        assert_eq!(output.next_action, CandidateNextAction::Reject);
        let note = output.admission_note.clone().expect("ordering evidence");
        let intent = output.admission_intent.expect("typed decision is emitted");
        assert_eq!(
            intent.outcome,
            crate::admission::AdmissionOrderOutcome::BlockedByOrder
        );
        assert_eq!(intent.blocking_prs, vec![PrNumber(2100)]);
        assert_eq!(intent.bypassed_unjoined_prs, vec![PrNumber(2050)]);
        assert!(
            note.contains(&intent.reason),
            "the refusal note names the same blocking reason the decision carries"
        );
        assert!(
            output
                .problems
                .iter()
                .any(|problem| problem.message.contains("fails closed"))
        );
    }

    /// A conflicting explicit join is rejected even though ordering allowed it.
    #[test]
    fn conflicting_explicit_join_is_rejected_with_bound_evidence() {
        let candidate = pr(2179, "green", "main", false);
        let status = status(
            candidate,
            vec![
                pr(1, "root", "main", true),
                pr(2113, "old-unjoined", "main", false),
            ],
        );
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

        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2179),
                tail_pr: Some(1),
                head_pr: None,
            },
            &conflict,
        )
        .expect("remote rejection is an inspectable receipt");

        assert!(!output.eligible);
        assert_eq!(output.next_action, CandidateNextAction::Repair);
        let intent = output.admission_intent.expect("typed decision is emitted");
        assert_eq!(
            intent.outcome,
            crate::admission::AdmissionOrderOutcome::BlockedByPreflight
        );
        assert!(!intent.compatibility_clean);
        assert!(intent.reason.contains("exact preflight rejected"));
    }

    /// A conflicting attachment preflight carries the exact squash-equivalence
    /// evidence for the same revisions, and a clean one carries none.
    #[test]
    fn conflicting_check_carries_squash_equivalence_evidence() {
        struct ReconcilableChecker;
        impl CompatibilityChecker for ReconcilableChecker {
            fn check(
                &self,
                candidate: &crate::model::BranchSnapshot,
                target: &crate::model::BranchSnapshot,
            ) -> Result<CompatibilityReport, AppError> {
                Ok(CompatibilityReport {
                    candidate: candidate.clone(),
                    target: target.clone(),
                    outcome: CompatibilityOutcome::Conflict,
                    conflicting_paths: vec!["src/lib.rs".to_owned()],
                    diagnostic: None,
                })
            }

            fn squash_equivalence(
                &self,
                candidate: &crate::model::BranchSnapshot,
                target: &crate::model::BranchSnapshot,
            ) -> Result<Option<SquashEquivalenceReport>, AppError> {
                Ok(Some(SquashEquivalenceReport {
                    schema_version: 1,
                    candidate: candidate.clone(),
                    target: target.clone(),
                    candidate_oid: candidate.oid.clone(),
                    target_oid: target.oid.clone(),
                    merge_base: None,
                    outcome: crate::squash_equivalence::SquashEquivalenceOutcome::NoEquivalence,
                    before: None,
                    after: None,
                    proven_boundary: None,
                    boundary_tree: None,
                    target_tree: None,
                    commits: Vec::new(),
                    represented_paths: Vec::new(),
                    represented_paths_truncated: false,
                    candidate_commit_count: 2,
                    analyzed_prefix_complete: true,
                    evaluated_boundaries: 0,
                    evaluation_bounded: false,
                    reason: "ordinary three-way divergence".to_owned(),
                    policy: crate::squash_equivalence::SQUASH_EQUIVALENCE_POLICY.to_owned(),
                }))
            }
        }

        let candidate = pr(2227, "tail", "main", false);
        let status = status(candidate, vec![pr(1, "root", "main", true)]);

        let conflicting = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2227),
                tail_pr: Some(1),
                head_pr: None,
            },
            &ReconcilableChecker,
        )
        .expect("rejection is an inspectable receipt");

        assert!(!conflicting.eligible);
        assert!(!conflicting.squash_reconciliations.is_empty());
        assert!(
            conflicting
                .squash_reconciliations
                .iter()
                .all(|report| report.authorized_range_base().is_none()),
            "unproven evidence never authorizes a boundary"
        );

        let clean = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2227),
                tail_pr: Some(1),
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("clean receipt");
        assert!(clean.squash_reconciliations.is_empty());
    }

    /// An unresolved or ambiguous join target fails closed before ordering.
    #[test]
    fn ambiguous_or_missing_join_target_never_relaxes_order() {
        let candidate = pr(2179, "green", "main", false);
        let status = status(
            candidate,
            vec![
                pr(1, "root", "main", true),
                pr(3, "other-root", "main", true),
                pr(2113, "old-unjoined", "main", false),
            ],
        );

        let error = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2179),
                tail_pr: Some(2113),
                head_pr: None,
            },
            &clean_checker,
        )
        .expect_err("an unjoined PR is not a caravan tail");

        assert_eq!(error.code(), "caravan_tail_not_found");
    }

    /// A stale pinned provider candidate still blocks explicit join intent.
    #[test]
    fn stale_candidate_still_blocks_explicit_join_intent() {
        let candidate = pr(2179, "green", "main", false);
        let mut status = status(
            candidate.clone(),
            vec![
                pr(1, "root", "main", true),
                pr(2113, "old-unjoined", "main", false),
            ],
        );
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
                compared_base: None,
                stale_base: false,
                stale_head: true,
                stale_reasons: vec!["synthetic head parent is stale".to_owned()],
            });

        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2179),
                tail_pr: Some(1),
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("stale rejection is an inspectable receipt");

        assert!(!output.eligible);
        assert_eq!(output.next_action, CandidateNextAction::Wait);
        let intent = output.admission_intent.expect("typed decision is emitted");
        assert_eq!(
            intent.outcome,
            crate::admission::AdmissionOrderOutcome::BlockedByPreflight
        );
    }

    /// A provider/compatibility failure propagates instead of being bypassed.
    #[test]
    fn provider_failure_during_join_preflight_propagates() {
        let candidate = pr(2179, "green", "main", false);
        let status = status(
            candidate,
            vec![
                pr(1, "root", "main", true),
                pr(2113, "old-unjoined", "main", false),
            ],
        );
        let failing = |_candidate: &crate::model::BranchSnapshot,
                       _target: &crate::model::BranchSnapshot| {
            Err(AppError::structured(
                ErrorCategory::ExecutionFailure,
                "compatibility_provider_failed",
                "injected provider failure",
                None,
            ))
        };

        let error = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2179),
                tail_pr: Some(1),
                head_pr: None,
            },
            &failing,
        )
        .expect_err("provider failure is never an admission bypass");

        assert_eq!(error.code(), "compatibility_provider_failed");
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
