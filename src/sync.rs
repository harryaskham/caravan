//! Deterministic, idempotent caravan synchronization.
//!
//! GitHub remains the only durable cursor. Every tick starts from fresh graph
//! facts, proves all compatibility decisions before mutation, applies exact
//! optimistic primitives, and records enough completed work for a rerun to
//! resume after interruption.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mcp_cli::{ErrorCategory, StructuredError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ci::{WorkflowFailureDiagnostics, WorkflowRunFailureDiagnostic};
use crate::command::CommandRunError;
use crate::github::{
    ControlLabelAudit, DiscoveryError, GitHubMutationAdapter, GitHubMutationReceipt, MutationError,
    WorkflowRunSnapshot, control_label_marker,
};
use crate::hooks::{self, HookDelivery};
use crate::model::{
    Caravan, CaravanEvent, CheckSnapshot, CheckState, CompatibilityOutcome, DecisionKind,
    DecisionPoint, EventId, EventKind, GraphProblem, GraphProblemKind, MergeCandidateIdentity,
    MergeMethod, MutationKind, MutationStep, MutationStepState, OperationId, OperationReceipt,
    PrNumber, PullRequestPrecondition, PullRequestSnapshot, PullRequestState, RepositoryId,
};
use crate::operation_lock::{OperationLock, OperationLockRecovery};
use crate::read::{self, StatusOutput};
use crate::{AppContext, AppError, CheckInput, SyncInput};

const MAX_SYNC_OPERATION_SECS: u64 = 3_600;
const MAX_PARALLEL_REBASE_CHAINS: usize = 2;
const MAX_SYNC_PLAN_ACTIONS: usize = 512;
const AUTO_ADMISSION_SKIP_LABEL: &str = "caravan-join-skipped";
const AUTO_ADMISSION_SKIP_PREFIX: &str = "<!-- caravan-auto-join-skip-receipt:";
const MAX_AUTO_ADMISSION_COMMENT_BYTES: usize = 60 * 1024;
/// Evolvable deterministic best-effort queue heuristic exposed in receipts.
pub const AUTO_ADMISSION_HEURISTIC_VERSION: &str = "priority_fifo_greedy_v1";

/// One observed rolling-head transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HeadAdvancement {
    pub merged_predecessor: PrNumber,
    pub new_head: PrNumber,
    pub previous_caravan_id: PrNumber,
    pub new_caravan_id: PrNumber,
}

/// Normalized CI policy outcome observed for one PR during a sync tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CiDisposition {
    Passing,
    Waiting,
    Failed,
    Forced,
}

/// Run-to-current-PR generation relationship derived from immutable provider facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunGeneration {
    Current,
    StaleHead,
    StaleBase,
    StaleHeadAndBase,
    MissingAssociation,
}

/// Deterministic sync-owned failure class; raw logs are never required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowFailureClass {
    StaleGeneration,
    RetryableInfrastructure,
    SourceOrTestFailure,
    Cancelled,
    Unknown,
}

/// Safe sync action selected from generation and structured failure evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowFailureAction {
    FreshCandidateTrigger,
    RerunFailedJobs,
    RepairSource,
    WaitOrInspect,
}

/// Classified evidence consumed directly by the CI decision and hook event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClassifiedWorkflowRunFailure {
    pub diagnostic: WorkflowRunFailureDiagnostic,
    pub generation: WorkflowRunGeneration,
    pub classification: WorkflowFailureClass,
    pub action: WorkflowFailureAction,
    #[serde(default)]
    pub reasons: Vec<String>,
}

/// Exact check and workflow-run evidence for one selected PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CiObservation {
    pub pr: PrNumber,
    pub disposition: CiDisposition,
    #[serde(default)]
    pub checks: Vec<CheckSnapshot>,
    #[serde(default)]
    pub failed_runs: Vec<WorkflowRunSnapshot>,
    /// Bounded run/job/failed-step evidence with generation-aware policy.
    #[serde(default)]
    pub failure_diagnostics: Vec<ClassifiedWorkflowRunFailure>,
    /// Exact current-generation infrastructure runs safe to rerun.
    #[serde(default)]
    pub rerunnable_run_ids: Vec<u64>,
}

/// Bounded whole-sync phase timings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncTiming {
    pub deadline_ms: u64,
    pub total_ms: u64,
    pub initial_status_ms: u64,
    pub provider_convergence_ms: u64,
    pub final_status_ms: u64,
}

/// Scheduler-facing outcome of one bounded, idempotent sync tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerDisposition {
    Healthy,
    WaitingCi,
    Held,
    RetryTick,
    ExternalDecision,
    OperatorAction,
}

/// Whether an external coordinator should wake a repair actor for this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerWakeClass {
    None,
    RetryTick,
    ExternalDecision,
    OperatorAction,
}

/// Exact provider generation retained for one member after a successful tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncMemberGeneration {
    pub pr: PrNumber,
    pub head: crate::model::BranchSnapshot,
    pub base: crate::model::BranchSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<MergeCandidateIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci: Option<CiDisposition>,
}

/// Exact root-to-tail generation for one converged caravan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncCaravanGeneration {
    pub caravan_id: PrNumber,
    pub root: PrNumber,
    pub tail: PrNumber,
    pub members: Vec<SyncMemberGeneration>,
}

/// Stable status consumed by deterministic external tick schedulers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncSchedulerStatus {
    pub schema_version: u32,
    pub disposition: SchedulerDisposition,
    pub wake_class: SchedulerWakeClass,
    pub rebase_on_join: bool,
    pub default_branch: crate::model::BranchSnapshot,
    #[serde(default)]
    pub caravans: Vec<SyncCaravanGeneration>,
    #[serde(default)]
    pub waiting_prs: Vec<PrNumber>,
    #[serde(default)]
    pub held_caravans: Vec<PrNumber>,
    pub reason: String,
}

/// Scheduler classification attached to every failed tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncFailureSchedulerStatus {
    pub schema_version: u32,
    pub disposition: SchedulerDisposition,
    pub wake_class: SchedulerWakeClass,
    pub retryable: bool,
    pub error_code: String,
}

/// Why one bounded automatic-admission phase stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AutoAdmissionContinuation {
    Disabled,
    RequiresSyncAll,
    Complete,
    CandidateBudgetExhausted,
    MutationBudgetExhausted,
    GithubRequestBudgetExhausted,
    DeadlineExhausted,
    RejectedCanonicalCandidate,
}

/// Exact live tail generation considered by the greedy heuristic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AutoAdmissionTailGeneration {
    pub caravan_id: PrNumber,
    pub tail_pr: PrNumber,
    pub branch: String,
    pub head_oid: crate::model::CommitOid,
    /// Active or expired explicit hold; stale holds do not freeze admission.
    pub held: bool,
}

/// Durable generation-bound explanation for one best-effort skip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AutoJoinSkipReceipt {
    pub schema_version: u32,
    pub repository: RepositoryId,
    pub candidate_pr: PrNumber,
    pub candidate_head: crate::model::BranchSnapshot,
    pub candidate_base: crate::model::BranchSnapshot,
    pub default_branch: crate::model::BranchSnapshot,
    #[serde(default)]
    pub tested_tails: Vec<AutoAdmissionTailGeneration>,
    pub config_fingerprint: String,
    pub heuristic_version: String,
    #[serde(default)]
    pub compatibility_reasons: Vec<String>,
    pub actor: String,
    pub observed_unix_secs: u64,
    /// Deterministic hash with this field omitted.
    pub evidence_hash: String,
}

impl AutoJoinSkipReceipt {
    fn finalize_hash(mut self) -> Self {
        self.evidence_hash.clear();
        let material = serde_json::to_vec(&self).expect("skip receipt serializes");
        self.evidence_hash = crate::membership::fnv1a64(&material);
        self
    }

    fn hash_is_valid(&self) -> bool {
        let mut material = self.clone();
        let expected = material.evidence_hash.clone();
        material.evidence_hash.clear();
        serde_json::to_vec(&material)
            .ok()
            .is_some_and(|bytes| crate::membership::fnv1a64(&bytes) == expected)
    }

    fn marker(&self) -> String {
        let encoded =
            hex_encode(&serde_json::to_vec(self).expect("validated skip receipt serializes"));
        format!("{AUTO_ADMISSION_SKIP_PREFIX}{encoded} -->")
    }

    fn comment_body(&self) -> String {
        format!(
            "{}\n### Cara automatic admission skip\n\nPR #{} was not mechanically compatible with any deterministic target under `{}`. This evidence is bound to the exact candidate/default/tail/config generations and becomes stale automatically when any bound fact changes. Manual `cara new`, `join`, or `rejoin` remains authoritative.\n\n- **Evidence:** `{}`\n- **Exact compatibility findings:** {} (encoded in the receipt marker)\n",
            self.marker(),
            self.candidate_pr,
            self.heuristic_version,
            self.evidence_hash,
            self.compatibility_reasons.len(),
        )
    }

    fn from_comment(body: &str) -> Option<Self> {
        let start = body.find(AUTO_ADMISSION_SKIP_PREFIX)? + AUTO_ADMISSION_SKIP_PREFIX.len();
        let remainder = &body[start..];
        let end = remainder.find(" -->")?;
        let bytes = hex_decode(&remainder[..end])?;
        let receipt: Self = serde_json::from_slice(&bytes).ok()?;
        receipt.hash_is_valid().then_some(receipt)
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn hex_decode(input: &str) -> Option<Vec<u8>> {
    if input.len() % 2 != 0 {
        return None;
    }
    input
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(pair, 16).ok()
        })
        .collect()
}

/// One exact successful sync-owned membership mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AutoAdmissionJoinReceipt {
    pub candidate_pr: PrNumber,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_tail: Option<PrNumber>,
    pub membership: crate::membership::MembershipOutput,
}

/// Stable bounded result of the opt-in greedy auto-admission phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AutoAdmissionOutput {
    pub enabled: bool,
    pub heuristic_version: String,
    pub continuation: AutoAdmissionContinuation,
    pub candidates_considered: u32,
    pub mutations_used: u32,
    pub mutation_limit: u32,
    pub github_requests_used: u32,
    pub github_request_limit: u32,
    #[serde(default)]
    pub joins: Vec<AutoAdmissionJoinReceipt>,
    #[serde(default)]
    pub skips: Vec<AutoJoinSkipReceipt>,
    #[serde(default)]
    pub remaining_candidates: Vec<PrNumber>,
}

impl Default for AutoAdmissionOutput {
    fn default() -> Self {
        Self {
            enabled: false,
            heuristic_version: AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
            continuation: AutoAdmissionContinuation::Disabled,
            candidates_considered: 0,
            mutations_used: 0,
            mutation_limit: 0,
            github_requests_used: 0,
            github_request_limit: 0,
            joins: Vec::new(),
            skips: Vec::new(),
            remaining_candidates: Vec::new(),
        }
    }
}

impl AutoAdmissionOutput {
    fn disabled(context: &AppContext, all: bool) -> Self {
        let enabled = context.config.sync.actions.join_unlabelled_prs;
        Self {
            enabled,
            heuristic_version: AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
            continuation: if enabled && !all {
                AutoAdmissionContinuation::RequiresSyncAll
            } else {
                AutoAdmissionContinuation::Disabled
            },
            candidates_considered: 0,
            mutations_used: 0,
            mutation_limit: context.config.sync.max_mutations_per_tick,
            github_requests_used: 0,
            github_request_limit: context.config.sync.max_github_requests_per_tick,
            joins: Vec::new(),
            skips: Vec::new(),
            remaining_candidates: Vec::new(),
        }
    }
}

/// Phase in which a no-write sync plan action would occur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SyncPlanPhase {
    PhysicalPreflight,
    ProviderConvergence,
    AutoAdmission,
    Rediscovery,
}

/// Whether an exact planned action writes, is already satisfied, or requires
/// fresh facts after an earlier planned generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SyncPlanActionState {
    WouldMutate,
    AlreadySatisfied,
    ReadOnlyObservation,
    DeferredUntilRediscovery,
    WouldStop,
}

/// One ordered, bounded action in an exact no-provider-write sync plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncPlanAction {
    pub order: u32,
    pub phase: SyncPlanPhase,
    pub state: SyncPlanActionState,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caravan_id: Option<PrNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<PullRequestPrecondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Value>,
    pub reason: String,
}

/// Exact first auto-admission attempt that can be proven without applying an
/// earlier provider generation. Later candidates always require rediscovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncAutoAdmissionPlan {
    pub enabled: bool,
    pub heuristic_version: String,
    pub continuation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_pr: Option<PrNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_tail: Option<PrNumber>,
    #[serde(default)]
    pub tested_tails: Vec<AutoAdmissionTailGeneration>,
    #[serde(default)]
    pub compatibility_reasons: Vec<String>,
}

/// One deterministic no-write stop or decision surfaced by planning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncPlanDecision {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrNumber>,
    pub reason: String,
    pub next: String,
}

/// Stable exact sync plan. `mutated` and `provider_writes` are invariantly zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncPlanOutput {
    pub schema_version: u32,
    pub mutated: bool,
    pub provider_writes: u32,
    pub local_ephemeral_preflight: bool,
    pub repository: RepositoryId,
    pub default_branch: crate::model::BranchSnapshot,
    pub all: bool,
    pub plan_hash: String,
    #[serde(default)]
    pub selected_caravans: Vec<PrNumber>,
    #[serde(default)]
    pub physical_rebase_plans: Vec<crate::physical_rebase::RebasePlan>,
    #[serde(default)]
    pub ci: Vec<CiObservation>,
    #[serde(default)]
    pub actions: Vec<SyncPlanAction>,
    pub auto_admission: SyncAutoAdmissionPlan,
    #[serde(default)]
    pub decisions: Vec<SyncPlanDecision>,
    #[serde(default)]
    pub would_emit_events: Vec<EventKind>,
    pub github_requests_used: u32,
    pub status: StatusOutput,
}

impl SyncPlanOutput {
    fn finalize_hash(mut self) -> Self {
        let material = serde_json::to_vec(&json!({
            "schema_version": self.schema_version,
            "repository": &self.repository,
            "default_branch": &self.default_branch,
            "all": self.all,
            "selected_caravans": &self.selected_caravans,
            "physical_rebase_plans": &self.physical_rebase_plans,
            "ci": &self.ci,
            "actions": &self.actions,
            "auto_admission": &self.auto_admission,
            "decisions": &self.decisions,
            "would_emit_events": &self.would_emit_events,
        }))
        .expect("sync plan hash material serializes");
        self.plan_hash = crate::membership::fnv1a64(&material);
        self
    }
}

/// Stable result of one converged synchronization tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncOutput {
    pub receipt: OperationReceipt,
    /// Opt-in sync-owned best-effort admission receipts and continuation.
    #[serde(default)]
    pub auto_admission: AutoAdmissionOutput,
    /// Explicit no-wake/decision status for external deterministic schedulers.
    pub scheduler_status: SyncSchedulerStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<SyncTiming>,
    /// Exact dead-owner cleanup performed before this sync acquired its lock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_recovery: Option<OperationLockRecovery>,
    /// Exact provider before/after facts for completed remote mutations.
    #[serde(default)]
    pub provider_receipts: Vec<GitHubMutationReceipt>,
    /// Complete immutable physical-rebase plans approved before the write barrier.
    #[serde(default)]
    pub rebase_plans: Vec<crate::physical_rebase::RebasePlan>,
    /// Exact old/new head and lease facts for applied branch generations.
    #[serde(default)]
    pub rebase_receipts: Vec<crate::physical_rebase::RebaseReceipt>,
    /// Merged branch-local predecessor that selected the active caravan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub historical_predecessor: Option<PrNumber>,
    /// Caravan IDs actually selected from the initial snapshot.
    #[serde(default)]
    pub synchronized_caravans: Vec<PrNumber>,
    /// Intentional holds skipped without any provider mutation. Expired holds
    /// remain here until an explicit resume; they never resume by time alone.
    #[serde(default)]
    pub paused_caravans: Vec<crate::pause::PauseStatus>,
    #[serde(default)]
    pub head_advancements: Vec<HeadAdvancement>,
    /// CI policy facts in deterministic head-to-tail order.
    #[serde(default)]
    pub ci: Vec<CiObservation>,
    /// Canonical auditable events consumed by configured hooks.
    #[serde(default)]
    pub events: Vec<CaravanEvent>,
    /// Bounded status for configured hooks which consumed `events`.
    #[serde(default)]
    pub hook_deliveries: Vec<HookDelivery>,
    /// Fresh post-mutation discovery rather than a locally predicted graph.
    pub status: StatusOutput,
}

/// Provider facts and primitives required by sync policy.
pub trait SyncProvider {
    fn verify_pull_request(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<PullRequestSnapshot, MutationError>;

    fn refetch_pull_request(
        &self,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<PullRequestSnapshot, MutationError>;

    fn verify_branch_head(
        &self,
        repository: &RepositoryId,
        branch: &str,
        expected: &crate::model::CommitOid,
    ) -> Result<(), MutationError>;

    fn branch_is_protected(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<bool, MutationError>;

    fn repository_allows_auto_merge(
        &self,
        repository: &RepositoryId,
    ) -> Result<bool, MutationError>;

    fn set_base(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        base: &str,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn enable_squash_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn disable_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn add_label(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn remove_label(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn pull_request_comment_bodies(
        &self,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<Vec<String>, MutationError>;

    fn ensure_marked_comment(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        marker: &str,
        body: &str,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn failed_runs_for_pull_request(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<Vec<WorkflowRunSnapshot>, MutationError>;

    fn failed_run_diagnostics(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        run_ids: &[u64],
    ) -> Result<WorkflowFailureDiagnostics, MutationError>;

    fn rerun_failed_run(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        run_id: u64,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn viewer_permission(&self, repository: &RepositoryId) -> Result<String, MutationError>;

    fn ensure_control_label_comment(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        audit: &ControlLabelAudit,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn admin_squash_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError>;
}

impl<R: crate::command::CommandRunner> SyncProvider for GitHubMutationAdapter<R> {
    fn verify_pull_request(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<PullRequestSnapshot, MutationError> {
        self.verify_precondition(repository, expected)
    }

    fn refetch_pull_request(
        &self,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<PullRequestSnapshot, MutationError> {
        self.refetch_pull_request(repository, number)
    }

    fn verify_branch_head(
        &self,
        repository: &RepositoryId,
        branch: &str,
        expected: &crate::model::CommitOid,
    ) -> Result<(), MutationError> {
        self.verify_branch_head(repository, branch, expected)
    }

    fn branch_is_protected(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<bool, MutationError> {
        self.branch_is_protected(repository, branch)
    }

    fn repository_allows_auto_merge(
        &self,
        repository: &RepositoryId,
    ) -> Result<bool, MutationError> {
        self.repository_allows_auto_merge(repository)
    }

    fn set_base(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        base: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.set_base(repository, expected, base)
    }

    fn enable_squash_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.enable_squash_auto_merge(repository, expected)
    }

    fn disable_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.disable_auto_merge(repository, expected)
    }

    fn add_label(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.add_label(repository, expected, label)
    }

    fn remove_label(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.remove_label(repository, expected, label)
    }

    fn pull_request_comment_bodies(
        &self,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<Vec<String>, MutationError> {
        self.pull_request_comment_bodies(repository, number)
    }

    fn ensure_marked_comment(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        marker: &str,
        body: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.ensure_marked_comment(repository, expected, marker, body)
    }

    fn failed_runs_for_pull_request(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<Vec<WorkflowRunSnapshot>, MutationError> {
        self.failed_runs_for_pull_request(repository, expected)
    }

    fn failed_run_diagnostics(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        run_ids: &[u64],
    ) -> Result<WorkflowFailureDiagnostics, MutationError> {
        self.failed_run_diagnostics(repository, expected, run_ids)
    }

    fn rerun_failed_run(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        run_id: u64,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.rerun_failed_run(repository, expected, run_id)
    }

    fn viewer_permission(&self, repository: &RepositoryId) -> Result<String, MutationError> {
        self.viewer_permission(repository)
    }

    fn ensure_control_label_comment(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        audit: &ControlLabelAudit,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.ensure_control_label_comment(repository, expected, audit)
    }

    fn admin_squash_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.admin_squash_merge(repository, expected)
    }
}

/// Synchronize the current caravan or every caravan and dispatch its canonical events.
pub fn sync(context: &AppContext, input: &SyncInput) -> Result<SyncOutput, AppError> {
    let started = Instant::now();
    let budget = sync_operation_budget(context);
    let operation_deadline = started + budget;
    match sync_without_hooks(context, input, started, operation_deadline) {
        Ok(mut output) => {
            output.hook_deliveries = hooks::dispatch_events(context, &output.events)?;
            Ok(output)
        }
        Err(error) => {
            let error = checkout_for_decision(context, error, operation_deadline);
            let scheduler_status = scheduler_failure_status(&error);
            let error = attach_scheduler_failure(&error, &scheduler_status);
            let mut events = hooks::events_from_error(&error);
            let already_wakes_repair = events
                .iter()
                .any(|event| matches!(event.kind, EventKind::CiFailed | EventKind::SyncFailed));
            if !already_wakes_repair
                && scheduler_status.wake_class == SchedulerWakeClass::ExternalDecision
            {
                if let Some(event) = sync_failed_event(&error) {
                    events.push(event);
                }
            }
            let deliveries = hooks::dispatch_events(context, &events)?;
            Err(hooks::attach_deliveries(error, &deliveries))
        }
    }
}

/// Build an exact, bounded sync plan without invoking any provider mutation.
#[allow(clippy::too_many_lines)]
pub fn plan_sync(context: &AppContext, input: &SyncInput) -> Result<SyncPlanOutput, AppError> {
    let _lock = OperationLock::acquire(&context.repository_path, "plan-sync")?;
    let started = Instant::now();
    let operation_deadline = started + sync_operation_budget(context);
    let github_budget =
        crate::command::GithubRequestBudget::new(context.config.sync.max_github_requests_per_tick);
    let status =
        read::status_with_deadline_and_budget(context, operation_deadline, Some(&github_budget))?;
    crate::initialization::require_ready(&status.initialization)?;
    let timeout = Duration::from_secs(context.config.command_timeout_secs);
    let runner = crate::command::ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(timeout)
        .with_operation_deadline(operation_deadline)
        .with_github_request_budget(github_budget.clone());
    crate::navigation::ensure_safe_worktree(
        &context.repository_path,
        &context.config_path,
        &runner,
    )?;
    let provider = GitHubMutationAdapter::new(runner);
    let selected = selected_unpaused_caravans(&status, input.all)?;
    let selected_ids = selected
        .iter()
        .map(|caravan| caravan.id)
        .collect::<Vec<_>>();
    let (physical_rebase_plans, mut progress) = if context.config.rebase_on_join {
        let (prepared, progress) =
            prepare_physical_chains(context, &status, input.all, &provider, operation_deadline)?;
        let plans = prepared
            .iter()
            .flat_map(|chain| chain.members.iter().map(|item| item.plan.clone()))
            .collect::<Vec<_>>();
        drop(prepared);
        (plans, progress)
    } else {
        let mut progress = SyncProgress::new(
            &status,
            selected_ids.clone(),
            context.config.sync.max_mutations_per_tick,
        );
        progress.paused_caravans = status
            .pauses
            .iter()
            .filter(|pause| {
                pause.state != crate::pause::PauseState::Stale
                    && status
                        .analysis
                        .fleet
                        .caravans
                        .iter()
                        .any(|caravan| caravan.id == pause.record.caravan_head)
            })
            .cloned()
            .collect();
        if !selected.is_empty() {
            preflight_repository(&provider, &status, &progress)?;
            validate_graph(&status, &selected, &progress)?;
        }
        (Vec::new(), progress)
    };

    let mut actions = Vec::new();
    let mut decisions = Vec::new();
    let mut would_emit_events = Vec::new();
    for plan in &physical_rebase_plans {
        push_plan_action(
            &mut actions,
            SyncPlanAction {
                order: 0,
                phase: SyncPlanPhase::PhysicalPreflight,
                state: if plan.already_satisfied {
                    SyncPlanActionState::AlreadySatisfied
                } else {
                    SyncPlanActionState::WouldMutate
                },
                kind: "rebase_branch".to_owned(),
                pr: Some(plan.pr),
                caravan_id: selected
                    .iter()
                    .find(|caravan| caravan.members.contains(&plan.pr))
                    .map(|caravan| caravan.id),
                expected: status
                    .analysis
                    .pull_requests
                    .get(&plan.pr)
                    .map(PullRequestPrecondition::from),
                target: Some(json!({
                    "branch": planned_base_snapshot(&plan.new_base).name,
                    "oid": planned_base_snapshot(&plan.new_base).oid,
                    "new_head_oid": plan.new_head_oid,
                    "lease": plan.lease,
                })),
                reason: if plan.already_satisfied {
                    "exact cumulative ancestry is already satisfied".to_owned()
                } else {
                    "exact retained generation passed conflict and dry-run lease preflight"
                        .to_owned()
                },
            },
        )?;
    }
    let has_physical_write = physical_rebase_plans
        .iter()
        .any(|plan| !plan.already_satisfied);
    for pause in &status.pauses {
        if pause.state != crate::pause::PauseState::Stale {
            push_plan_action(
                &mut actions,
                SyncPlanAction {
                    order: 0,
                    phase: SyncPlanPhase::ProviderConvergence,
                    state: SyncPlanActionState::AlreadySatisfied,
                    kind: "hold_caravan".to_owned(),
                    pr: Some(pause.record.caravan_head),
                    caravan_id: Some(pause.record.caravan_head),
                    expected: None,
                    target: None,
                    reason: format!("explicit {:?} hold prevents sync mutation", pause.state),
                },
            )?;
        }
    }

    for caravan in &selected {
        plan_caravan_convergence(
            &status,
            &provider,
            caravan,
            input,
            context.config.force_merge,
            has_physical_write,
            &mut progress,
            &mut actions,
            &mut decisions,
            &mut would_emit_events,
        )?;
    }

    let auto_admission = plan_auto_admission(
        context,
        &status,
        input,
        has_physical_write || !decisions.is_empty(),
        operation_deadline,
        &mut actions,
        &mut would_emit_events,
    )?;
    would_emit_events.sort();
    would_emit_events.dedup();
    let output = SyncPlanOutput {
        schema_version: 1,
        mutated: false,
        provider_writes: 0,
        local_ephemeral_preflight: context.config.rebase_on_join,
        repository: status.repository.clone(),
        default_branch: status.analysis.fleet.default_branch.clone(),
        all: input.all,
        plan_hash: String::new(),
        selected_caravans: selected_ids,
        physical_rebase_plans,
        ci: progress.ci,
        actions,
        auto_admission,
        decisions,
        would_emit_events,
        github_requests_used: github_budget.used(),
        status,
    };
    Ok(output.finalize_hash())
}

fn push_plan_action(
    actions: &mut Vec<SyncPlanAction>,
    mut action: SyncPlanAction,
) -> Result<(), AppError> {
    if actions.len() >= MAX_SYNC_PLAN_ACTIONS {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "sync_plan_action_limit",
            "sync plan exceeded its bounded action limit",
            Some(json!({"limit": MAX_SYNC_PLAN_ACTIONS, "mutated": false})),
        ));
    }
    action.order = u32::try_from(actions.len() + 1).unwrap_or(u32::MAX);
    actions.push(action);
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn plan_caravan_convergence(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    caravan: &Caravan,
    input: &SyncInput,
    force_merge: bool,
    deferred: bool,
    progress: &mut SyncProgress,
    actions: &mut Vec<SyncPlanAction>,
    decisions: &mut Vec<SyncPlanDecision>,
    would_emit_events: &mut Vec<EventKind>,
) -> Result<(), AppError> {
    let head = caravan.head().expect("caravan head");
    let head_snapshot = status
        .analysis
        .pull_requests
        .get(&head)
        .expect("selected head has provider facts");
    let expected = Some(PullRequestPrecondition::from(head_snapshot));
    let base_satisfied = head_snapshot.base.name == status.default_branch;
    push_plan_action(
        actions,
        SyncPlanAction {
            order: 0,
            phase: SyncPlanPhase::ProviderConvergence,
            state: if base_satisfied {
                SyncPlanActionState::AlreadySatisfied
            } else if deferred {
                SyncPlanActionState::DeferredUntilRediscovery
            } else {
                SyncPlanActionState::WouldMutate
            },
            kind: "set_base".to_owned(),
            pr: Some(head),
            caravan_id: Some(caravan.id),
            expected: expected.clone(),
            target: Some(
                json!({"branch": status.default_branch, "oid": status.analysis.fleet.default_branch.oid}),
            ),
            reason: if base_satisfied {
                "caravan head already targets the current default branch".to_owned()
            } else {
                "caravan head must target the exact current default branch".to_owned()
            },
        },
    )?;
    if merged_predecessor(status, caravan).is_some() {
        would_emit_events.push(EventKind::HeadAdvanced);
    }
    if deferred {
        for number in caravan.members.iter().copied() {
            let current = &status.analysis.pull_requests[&number];
            push_plan_action(
                actions,
                SyncPlanAction {
                    order: 0,
                    phase: SyncPlanPhase::Rediscovery,
                    state: SyncPlanActionState::DeferredUntilRediscovery,
                    kind: "observe_ci".to_owned(),
                    pr: Some(number),
                    caravan_id: Some(caravan.id),
                    expected: Some(PullRequestPrecondition::from(current)),
                    target: None,
                    reason: "planned branch rewrite changes the CI generation; fresh checks must be observed after apply"
                        .to_owned(),
                },
            )?;
        }
        for number in caravan.members.iter().skip(1).copied() {
            let current = &status.analysis.pull_requests[&number];
            push_plan_action(
                actions,
                SyncPlanAction {
                    order: 0,
                    phase: SyncPlanPhase::Rediscovery,
                    state: SyncPlanActionState::DeferredUntilRediscovery,
                    kind: "disable_auto_merge".to_owned(),
                    pr: Some(number),
                    caravan_id: Some(caravan.id),
                    expected: Some(PullRequestPrecondition::from(current)),
                    target: Some(json!({"enabled": false})),
                    reason:
                        "revalidate the rewritten generation before repairing non-head auto-merge"
                            .to_owned(),
                },
            )?;
        }
        push_plan_action(
            actions,
            SyncPlanAction {
                order: 0,
                phase: SyncPlanPhase::Rediscovery,
                state: SyncPlanActionState::DeferredUntilRediscovery,
                kind: "enable_squash_auto_merge".to_owned(),
                pr: Some(head),
                caravan_id: Some(caravan.id),
                expected,
                target: Some(json!({"enabled": true, "merge_method": "squash"})),
                reason:
                    "revalidate rewritten head CI and provider facts before enabling auto-merge"
                        .to_owned(),
            },
        )?;
        return Ok(());
    }

    let mut forced_head = false;
    let mut stopped = false;
    for number in caravan.members.iter().copied() {
        let observation = progress.observe_ci(provider, &status.repository, number)?;
        let disposition = observation.disposition;
        progress.ci.push(observation.clone());
        push_plan_action(
            actions,
            SyncPlanAction {
                order: 0,
                phase: SyncPlanPhase::ProviderConvergence,
                state: SyncPlanActionState::ReadOnlyObservation,
                kind: "observe_ci".to_owned(),
                pr: Some(number),
                caravan_id: Some(caravan.id),
                expected: status
                    .analysis
                    .pull_requests
                    .get(&number)
                    .map(PullRequestPrecondition::from),
                target: Some(json!({
                    "disposition": disposition,
                    "rerunnable_run_ids": observation.rerunnable_run_ids,
                })),
                reason: "fresh checks and bounded workflow diagnostics are read without mutation"
                    .to_owned(),
            },
        )?;
        if disposition == CiDisposition::Failed {
            if input.rerun_failed && !observation.rerunnable_run_ids.is_empty() {
                push_plan_action(
                    actions,
                    SyncPlanAction {
                        order: 0,
                        phase: SyncPlanPhase::ProviderConvergence,
                        state: if deferred {
                            SyncPlanActionState::DeferredUntilRediscovery
                        } else {
                            SyncPlanActionState::WouldMutate
                        },
                        kind: "rerun_failed_jobs".to_owned(),
                        pr: Some(number),
                        caravan_id: Some(caravan.id),
                        expected: None,
                        target: Some(json!({"run_ids": observation.rerunnable_run_ids})),
                        reason:
                            "only exact current-generation infrastructure failures are rerunnable"
                                .to_owned(),
                    },
                )?;
            }
            decisions.push(SyncPlanDecision {
                code: "ci_failed".to_owned(),
                pr: Some(number),
                reason: "sync would stop at this exact failed CI generation".to_owned(),
                next: "repair source/test failures or rerun only listed infrastructure runs, then plan again"
                    .to_owned(),
            });
            stopped = true;
            break;
        }
        forced_head |= number == head && disposition == CiDisposition::Forced;
    }
    if stopped {
        return Ok(());
    }
    if forced_head {
        let mechanically_allowed = force_allowed(status, head_snapshot, force_merge);
        let permission = if mechanically_allowed {
            Some(
                provider
                    .viewer_permission(&status.repository)
                    .map_err(|error| mutation_error(&error, progress, Some(head)))?,
            )
        } else {
            None
        };
        let can_force = mechanically_allowed && permission.as_deref() == Some("ADMIN");
        push_plan_action(
            actions,
            SyncPlanAction {
                order: 0,
                phase: SyncPlanPhase::ProviderConvergence,
                state: if can_force && !deferred {
                    SyncPlanActionState::WouldMutate
                } else if deferred {
                    SyncPlanActionState::DeferredUntilRediscovery
                } else {
                    SyncPlanActionState::WouldStop
                },
                kind: "force_squash_merge".to_owned(),
                pr: Some(head),
                caravan_id: Some(caravan.id),
                expected,
                target: Some(json!({"merge_method": "squash", "permission": permission})),
                reason: "explicit exact-generation caravan-force intent requires configured policy and fresh ADMIN preflight"
                    .to_owned(),
            },
        )?;
        if can_force {
            would_emit_events.push(EventKind::ForceMergeAttempted);
            would_emit_events.push(EventKind::ForceMergeCompleted);
        } else {
            decisions.push(SyncPlanDecision {
                code: "force_merge_denied".to_owned(),
                pr: Some(head),
                reason: "force intent lacks configured policy, exact clean compatibility, or ADMIN permission"
                    .to_owned(),
                next: "repair the exact policy/permission evidence or remove stale force intent, then plan again"
                    .to_owned(),
            });
        }
        return Ok(());
    }

    for number in caravan.members.iter().skip(1).copied() {
        let current = &status.analysis.pull_requests[&number];
        push_plan_action(
            actions,
            SyncPlanAction {
                order: 0,
                phase: SyncPlanPhase::ProviderConvergence,
                state: if !current.auto_merge.enabled {
                    SyncPlanActionState::AlreadySatisfied
                } else if deferred {
                    SyncPlanActionState::DeferredUntilRediscovery
                } else {
                    SyncPlanActionState::WouldMutate
                },
                kind: "disable_auto_merge".to_owned(),
                pr: Some(number),
                caravan_id: Some(caravan.id),
                expected: Some(PullRequestPrecondition::from(current)),
                target: Some(json!({"enabled": false})),
                reason: "only the caravan head may have squash auto-merge enabled".to_owned(),
            },
        )?;
    }
    push_plan_action(
        actions,
        SyncPlanAction {
            order: 0,
            phase: SyncPlanPhase::ProviderConvergence,
            state: if head_snapshot.auto_merge.enabled {
                SyncPlanActionState::AlreadySatisfied
            } else if deferred {
                SyncPlanActionState::DeferredUntilRediscovery
            } else {
                SyncPlanActionState::WouldMutate
            },
            kind: "enable_squash_auto_merge".to_owned(),
            pr: Some(head),
            caravan_id: Some(caravan.id),
            expected: Some(PullRequestPrecondition::from(head_snapshot)),
            target: Some(json!({"enabled": true, "merge_method": "squash"})),
            reason: "healthy caravan head is the sole auto-merge candidate".to_owned(),
        },
    )?;
    Ok(())
}

fn force_allowed(status: &StatusOutput, head: &PullRequestSnapshot, force_merge: bool) -> bool {
    force_merge
        && head.state == PullRequestState::Open
        && !head.draft
        && head.has_label("caravan-force")
        && head_is_conflict_free_with_default(status, head)
}

#[allow(clippy::too_many_arguments)]
fn plan_auto_admission(
    context: &AppContext,
    status: &StatusOutput,
    input: &SyncInput,
    requires_rediscovery: bool,
    operation_deadline: Instant,
    actions: &mut Vec<SyncPlanAction>,
    would_emit_events: &mut Vec<EventKind>,
) -> Result<SyncAutoAdmissionPlan, AppError> {
    let checker = crate::graph::GitCompatibilityChecker::new(&context.repository_path, "origin")
        .with_timeout(Duration::from_secs(context.config.command_timeout_secs))
        .with_operation_deadline(operation_deadline);
    plan_auto_admission_with_checker(
        context,
        status,
        input,
        requires_rediscovery,
        actions,
        would_emit_events,
        &checker,
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn plan_auto_admission_with_checker(
    context: &AppContext,
    status: &StatusOutput,
    input: &SyncInput,
    requires_rediscovery: bool,
    actions: &mut Vec<SyncPlanAction>,
    would_emit_events: &mut Vec<EventKind>,
    checker: &impl crate::graph::CompatibilityChecker,
) -> Result<SyncAutoAdmissionPlan, AppError> {
    let enabled = context.config.sync.actions.join_unlabelled_prs;
    let mut output = SyncAutoAdmissionPlan {
        enabled,
        heuristic_version: AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
        continuation: if enabled && !input.all {
            "requires_sync_all".to_owned()
        } else if enabled {
            "complete".to_owned()
        } else {
            "disabled".to_owned()
        },
        candidate_pr: None,
        target_tail: None,
        tested_tails: Vec::new(),
        compatibility_reasons: Vec::new(),
    };
    if !enabled || !input.all {
        return Ok(output);
    }
    if requires_rediscovery {
        "replan_after_existing_fleet_convergence".clone_into(&mut output.continuation);
        push_plan_action(
            actions,
            SyncPlanAction {
                order: 0,
                phase: SyncPlanPhase::Rediscovery,
                state: SyncPlanActionState::DeferredUntilRediscovery,
                kind: "rediscover_before_auto_admission".to_owned(),
                pr: None,
                caravan_id: None,
                expected: None,
                target: None,
                reason: "auto-admission target generations are not guessed across earlier planned writes or decisions"
                    .to_owned(),
            },
        )?;
        return Ok(output);
    }
    let Some(candidate_pr) = status.admission.next_candidate else {
        if let Some(rejected) = status.admission.rejected.first()
            && let Some(candidate) = status.analysis.pull_requests.get(&rejected.pr)
        {
            output.candidate_pr = Some(rejected.pr);
            "rejected_canonical_candidate".clone_into(&mut output.continuation);
            output.compatibility_reasons = vec![rejected.reason.clone()];
            push_plan_action(
                actions,
                SyncPlanAction {
                    order: 0,
                    phase: SyncPlanPhase::AutoAdmission,
                    state: SyncPlanActionState::WouldStop,
                    kind: "reject_canonical_candidate".to_owned(),
                    pr: Some(rejected.pr),
                    caravan_id: None,
                    expected: Some(PullRequestPrecondition::from(candidate)),
                    target: None,
                    reason: rejected.reason.clone(),
                },
            )?;
        }
        return Ok(output);
    };
    let Some(candidate) = status.analysis.pull_requests.get(&candidate_pr) else {
        return Err(AppError::validation(
            "sync_plan_candidate_missing",
            format!("canonical candidate #{candidate_pr} disappeared from exact status"),
        ));
    };
    if !status
        .admission
        .candidates
        .iter()
        .any(|candidate| candidate.pr == candidate_pr)
    {
        output.candidate_pr = Some(candidate_pr);
        "rejected_canonical_candidate".clone_into(&mut output.continuation);
        output.compatibility_reasons = status
            .admission
            .rejected
            .iter()
            .find(|candidate| candidate.pr == candidate_pr)
            .map_or_else(Vec::new, |candidate| vec![candidate.reason.clone()]);
        push_plan_action(
            actions,
            SyncPlanAction {
                order: 0,
                phase: SyncPlanPhase::AutoAdmission,
                state: SyncPlanActionState::WouldStop,
                kind: "reject_canonical_candidate".to_owned(),
                pr: Some(candidate_pr),
                caravan_id: None,
                expected: Some(PullRequestPrecondition::from(candidate)),
                target: None,
                reason: output.compatibility_reasons.join(" · "),
            },
        )?;
        return Ok(output);
    }
    let evaluation = evaluate_auto_candidate(status, candidate, checker)?;
    output.candidate_pr = Some(candidate_pr);
    output.tested_tails.clone_from(&evaluation.tested_tails);
    output.compatibility_reasons.clone_from(&evaluation.reasons);
    let (kind, target_tail, reason, events) = match evaluation.target {
        AutoCandidateTarget::New => (
            "auto_admission_new",
            None,
            "canonical candidate would form a new caravan",
            vec![EventKind::CaravanCreated],
        ),
        AutoCandidateTarget::Join(tail) => (
            "auto_admission_join",
            Some(tail),
            "canonical candidate would join the first exact compatible tail",
            vec![EventKind::PrJoined],
        ),
        AutoCandidateTarget::Skip => (
            "persist_auto_admission_skip",
            None,
            "no deterministic compatible target; exact generation-bound skip would be recorded",
            Vec::new(),
        ),
    };
    output.target_tail = target_tail;
    "replan_after_first_admission".clone_into(&mut output.continuation);
    would_emit_events.extend(events);
    push_plan_action(
        actions,
        SyncPlanAction {
            order: 0,
            phase: SyncPlanPhase::AutoAdmission,
            state: SyncPlanActionState::WouldMutate,
            kind: kind.to_owned(),
            pr: Some(candidate_pr),
            caravan_id: target_tail.and_then(|tail| {
                status
                    .analysis
                    .fleet
                    .containing(tail)
                    .map(|caravan| caravan.id)
            }),
            expected: Some(PullRequestPrecondition::from(candidate)),
            target: Some(json!({
                "tail_pr": target_tail,
                "tested_tails": evaluation.tested_tails,
                "compatibility_reasons": evaluation.reasons,
            })),
            reason: reason.to_owned(),
        },
    )?;
    Ok(output)
}

fn planned_base_snapshot(
    base: &crate::physical_rebase::PlannedBase,
) -> &crate::model::BranchSnapshot {
    match base {
        crate::physical_rebase::PlannedBase::Remote(branch)
        | crate::physical_rebase::PlannedBase::Simulated(branch) => branch,
    }
}

fn sync_operation_budget(context: &AppContext) -> Duration {
    Duration::from_secs(
        context
            .config
            .sync
            .max_duration_secs
            .min(MAX_SYNC_OPERATION_SECS),
    )
}

struct PreparedChain {
    caravan: Caravan,
    members: Vec<crate::physical_rebase::PreparedRebase>,
}

#[derive(Default)]
struct PhysicalRebuildOutcome {
    repository: Option<RepositoryId>,
    caravan_id: Option<PrNumber>,
    affected_prs: Vec<PrNumber>,
    plans: Vec<crate::physical_rebase::RebasePlan>,
    receipts: Vec<crate::physical_rebase::RebaseReceipt>,
    provider_receipts: Vec<GitHubMutationReceipt>,
    steps: Vec<MutationStep>,
}

fn selected_unpaused_caravans(status: &StatusOutput, all: bool) -> Result<Vec<Caravan>, AppError> {
    let mut selected = select_caravans(status, all)?;
    selected.retain(|caravan| {
        !status.pauses.iter().any(|pause| {
            pause.state != crate::pause::PauseState::Stale
                && pause.record.caravan_head == caravan.id
        })
    });
    Ok(selected)
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[allow(clippy::too_many_lines)]
fn prepare_physical_chains(
    context: &AppContext,
    status: &StatusOutput,
    all: bool,
    provider: &impl SyncProvider,
    operation_deadline: Instant,
) -> Result<(Vec<PreparedChain>, SyncProgress), AppError> {
    let selected = selected_unpaused_caravans(status, all)?;
    let progress = SyncProgress::new(
        status,
        selected.iter().map(|caravan| caravan.id).collect(),
        context.config.sync.max_mutations_per_tick,
    );
    preflight_repository(provider, status, &progress)?;
    validate_rebase_preflight_graph(status, &selected, &progress)?;
    let timeout = Duration::from_secs(context.config.command_timeout_secs);
    let mut chains = Vec::with_capacity(selected.len());
    for caravan in selected {
        let mut target = crate::physical_rebase::PlannedBase::Remote(
            status.analysis.fleet.default_branch.clone(),
        );
        let predecessor = merged_predecessor(status, &caravan);
        let mut members = Vec::with_capacity(caravan.members.len());
        for (index, number) in caravan.members.iter().enumerate() {
            let candidate = status
                .analysis
                .pull_requests
                .get(number)
                .expect("selected caravan member has provider facts");
            let range_source = if index == 0 {
                predecessor.map_or_else(
                    || {
                        crate::physical_rebase::range_base_for_remote_target(
                            candidate,
                            &status.analysis.fleet.default_branch,
                        )
                    },
                    |merged| crate::physical_rebase::PlannedRangeBase::PullRequestHead {
                        pr: merged.number,
                        branch: candidate.base.clone(),
                    },
                )
            } else {
                crate::physical_rebase::PlannedRangeBase::RemoteBranch {
                    branch: candidate.base.clone(),
                }
            };
            let prepared = match crate::physical_rebase::prepare_candidate(
                &context.repository_path,
                &status.repository,
                candidate,
                range_source,
                target,
                &status.analysis.fleet.default_branch,
                crate::physical_rebase::RebaseExecutionBudget::new(timeout)
                    .with_deadline(operation_deadline),
            ) {
                Ok(prepared) => prepared,
                Err(error) => {
                    let plans =
                        chains
                            .iter()
                            .flat_map(|chain: &PreparedChain| {
                                chain.members.iter().map(|item| item.plan.clone())
                            })
                            .chain(members.iter().map(
                                |item: &crate::physical_rebase::PreparedRebase| item.plan.clone(),
                            ))
                            .collect();
                    return Err(attach_physical_rebuild(
                        error,
                        &PhysicalRebuildOutcome {
                            repository: Some(status.repository.clone()),
                            caravan_id: Some(caravan.id),
                            affected_prs: vec![*number],
                            plans,
                            ..PhysicalRebuildOutcome::default()
                        },
                    ));
                }
            };
            target = crate::physical_rebase::PlannedBase::Simulated(crate::model::BranchSnapshot {
                repository: status.repository.clone(),
                name: candidate.head.name.clone(),
                oid: prepared.plan.new_head_oid.clone(),
            });
            members.push(prepared);
        }
        chains.push(PreparedChain { caravan, members });
    }
    if let Err(error) =
        verify_physical_write_barrier(context, status, provider, &chains, operation_deadline)
    {
        let plans: Vec<crate::physical_rebase::RebasePlan> = chains
            .iter()
            .flat_map(|chain| chain.members.iter().map(|item| item.plan.clone()))
            .collect();
        return Err(attach_physical_rebuild(
            error,
            &PhysicalRebuildOutcome {
                repository: Some(status.repository.clone()),
                affected_prs: plans
                    .iter()
                    .map(|plan: &crate::physical_rebase::RebasePlan| plan.pr)
                    .collect(),
                plans,
                ..PhysicalRebuildOutcome::default()
            },
        ));
    }
    Ok((chains, progress))
}

fn validate_rebase_preflight_graph(
    status: &StatusOutput,
    selected: &[Caravan],
    progress: &SyncProgress,
) -> Result<(), AppError> {
    for problem in &status.analysis.fleet.problems {
        let auto_merge = problem.kind == GraphProblemKind::AutoMergeInvariant
            && problem.prs.iter().all(|number| {
                selected
                    .iter()
                    .any(|caravan| caravan.members.contains(number))
            });
        let advancement = problem.kind == GraphProblemKind::DanglingBase
            && recoverable_dangling_problem(status, selected, problem);
        let rebase = problem.kind == GraphProblemKind::Incompatible
            && selected.iter().any(|caravan| {
                problem
                    .prs
                    .iter()
                    .all(|number| caravan.members.contains(number))
                    && (problem.prs.len() == 1
                        || caravan.members.windows(2).any(|pair| {
                            problem.prs.as_slice() == pair
                                || problem.prs.as_slice() == [pair[1], pair[0]]
                        }))
            });
        if auto_merge || advancement || rebase {
            continue;
        }
        return Err(decision_error(
            &decision_for_problem(problem, status, progress),
            progress,
        ));
    }
    Ok(())
}

fn verify_physical_write_barrier(
    context: &AppContext,
    status: &StatusOutput,
    provider: &impl SyncProvider,
    chains: &[PreparedChain],
    _operation_deadline: Instant,
) -> Result<(), AppError> {
    let timeout = Duration::from_secs(context.config.command_timeout_secs);
    crate::physical_rebase::verify_branch_snapshot(
        &context.repository_path,
        &status.analysis.fleet.default_branch,
        timeout,
    )?;
    let mut branches = BTreeSet::new();
    for chain in chains {
        for prepared in &chain.members {
            if !branches.insert(prepared.plan.branch.clone()) {
                return Err(AppError::structured(
                    ErrorCategory::Validation,
                    "rebase_overlapping_branch_sets",
                    "selected caravans contain the same physical branch",
                    Some(
                        json!({"branch": prepared.plan.branch, "plans": chains.iter().flat_map(|chain| chain.members.iter().map(|item| &item.plan)).collect::<Vec<_>>() }),
                    ),
                ));
            }
            let expected = PullRequestPrecondition::from(
                status
                    .analysis
                    .pull_requests
                    .get(&prepared.plan.pr)
                    .expect("planned PR has initial provider facts"),
            );
            provider
                .verify_pull_request(&status.repository, &expected)
                .map_err(|error| {
                    mutation_error(
                        &error,
                        &SyncProgress::new(
                            status,
                            Vec::new(),
                            context.config.sync.max_mutations_per_tick,
                        ),
                        Some(prepared.plan.pr),
                    )
                })?;
            crate::physical_rebase::verify_prepared(prepared)?;
        }
    }
    Ok(())
}

fn invalidate_rewritten_force_intents(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    plans: &[crate::physical_rebase::RebasePlan],
    progress: &mut SyncProgress,
) -> Result<(), AppError> {
    for plan in plans.iter().filter(|plan| !plan.already_satisfied) {
        let current = progress
            .current
            .get(&plan.pr)
            .expect("planned PR has current provider facts")
            .clone();
        if !current.has_label("caravan-force") {
            continue;
        }

        let mut after_labels = current.labels.clone();
        after_labels.remove("caravan-force");
        let audit = ControlLabelAudit {
            operation: "force_invalidate_rewrite".to_owned(),
            marker: control_label_marker(
                "force_invalidate_rewrite",
                plan.pr,
                &plan.old_head_oid,
                &current.labels,
                &after_labels,
            ),
            before_labels: current.labels.clone(),
            after_labels,
            actor: "cara sync physical-rebase policy".to_owned(),
            reason: format!(
                "invalidated caravan-force intent bound to old head {} before Cara-owned rewrite to {}",
                plan.old_head_oid, plan.new_head_oid
            ),
            reason_source: "deterministic exact-generation safety policy".to_owned(),
            compatibility_evidence: format!(
                "physical rebase plan for PR #{} passed the global write barrier",
                plan.pr
            ),
            clean_squash_evidence:
                "not applicable: force intent is removed before branch history changes".to_owned(),
            admission_priority_basis: "not applicable: caravan order is unchanged".to_owned(),
        };
        progress.ensure_mutation_capacity(1)?;
        let receipt = provider
            .remove_label(
                &status.repository,
                &progress.precondition(plan.pr),
                "caravan-force",
            )
            .map_err(|error| mutation_error(&error, progress, Some(plan.pr)))?;
        progress.record(
            receipt,
            "removed caravan-force before rewriting its exact head generation",
        );
        progress.ensure_control_label_comment(provider, &status.repository, plan.pr, &audit)?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn apply_physical_chains(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    chains: &[PreparedChain],
    mut progress: SyncProgress,
) -> Result<PhysicalRebuildOutcome, AppError> {
    let plans = chains
        .iter()
        .flat_map(|chain| chain.members.iter().map(|prepared| prepared.plan.clone()))
        .collect::<Vec<_>>();
    let mut outcome = PhysicalRebuildOutcome {
        repository: Some(status.repository.clone()),
        caravan_id: (chains.len() == 1).then_some(chains[0].caravan.id),
        affected_prs: plans.iter().map(|plan| plan.pr).collect(),
        plans,
        ..PhysicalRebuildOutcome::default()
    };
    for chain in chains {
        for prepared in &chain.members {
            if let Err(error) =
                progress.ensure_auto_merge_disabled(provider, &status.repository, prepared.plan.pr)
            {
                outcome
                    .provider_receipts
                    .clone_from(&progress.provider_receipts);
                outcome.steps.clone_from(&progress.steps);
                return Err(attach_physical_rebuild(error, &outcome));
            }
        }
    }
    let planned_branch_writes = u32::try_from(
        outcome
            .plans
            .iter()
            .filter(|plan| !plan.already_satisfied)
            .count(),
    )
    .unwrap_or(u32::MAX);
    progress
        .ensure_mutation_capacity(planned_branch_writes)
        .map_err(|error| attach_physical_rebuild(error, &outcome))?;
    outcome
        .provider_receipts
        .clone_from(&progress.provider_receipts);
    outcome.steps.clone_from(&progress.steps);
    for batch in chains.chunks(MAX_PARALLEL_REBASE_CHAINS) {
        let results = std::thread::scope(|scope| {
            batch
                .iter()
                .map(|chain| scope.spawn(|| apply_prepared_chain(chain)))
                .collect::<Vec<_>>()
                .into_iter()
                .map(std::thread::ScopedJoinHandle::join)
                .collect::<Vec<_>>()
        });
        let mut first_error = None;
        for (chain, result) in batch.iter().zip(results) {
            match result {
                Ok((receipts, error)) => {
                    outcome.receipts.extend(receipts);
                    if first_error.is_none() {
                        first_error = error;
                    }
                }
                Err(_) if first_error.is_none() => {
                    first_error = Some(AppError::structured(
                        ErrorCategory::ExecutionFailure,
                        "rebase_worker_panicked",
                        "bounded independent-caravan rebase worker panicked",
                        Some(json!({"caravan": chain.caravan.id, "resumable": true})),
                    ));
                }
                Err(_) => {}
            }
        }
        if let Some(error) = first_error {
            return Err(attach_physical_rebuild(error, &outcome));
        }
    }
    for receipt in &outcome.receipts {
        let observed = provider
            .refetch_pull_request(&status.repository, receipt.pr)
            .map_err(|error| {
                attach_physical_rebuild(
                    mutation_error(&error, &progress, Some(receipt.pr)),
                    &outcome,
                )
            })?;
        if observed.head.oid != receipt.new_head_oid {
            return Err(attach_physical_rebuild(
                AppError::structured(
                    ErrorCategory::Validation,
                    "rebase_midpoint_head_stale",
                    "provider did not expose the exact applied branch generation",
                    Some(
                        json!({"receipt": receipt, "observed_head": observed.head.oid, "resumable": true}),
                    ),
                ),
                &outcome,
            ));
        }
        progress.current.insert(receipt.pr, observed);
        outcome.steps.push(MutationStep {
            kind: MutationKind::RebaseBranch,
            state: if receipt.already_satisfied {
                MutationStepState::AlreadySatisfied
            } else {
                MutationStepState::Completed
            },
            pr: Some(receipt.pr),
            summary: format!(
                "rebased branch {} from {} to {} onto {} under exact lease",
                receipt.branch, receipt.old_head_oid, receipt.new_head_oid, receipt.new_base_oid
            ),
        });
    }
    Ok(outcome)
}

fn apply_prepared_chain(
    chain: &PreparedChain,
) -> (Vec<crate::physical_rebase::RebaseReceipt>, Option<AppError>) {
    let mut receipts = Vec::with_capacity(chain.members.len());
    for prepared in &chain.members {
        match crate::physical_rebase::apply_prepared(prepared) {
            Ok(receipt) => receipts.push(receipt),
            Err(error) => return (receipts, Some(error)),
        }
    }
    (receipts, None)
}

#[allow(clippy::needless_pass_by_value)]
fn attach_physical_rebuild(error: AppError, outcome: &PhysicalRebuildOutcome) -> AppError {
    let mut details = error.details().unwrap_or_else(|| json!({}));
    if let Some(object) = details.as_object_mut() {
        object.insert("repository".to_owned(), json!(outcome.repository));
        object.insert("caravan_id".to_owned(), json!(outcome.caravan_id));
        object.insert("affected_prs".to_owned(), json!(outcome.affected_prs));
        object.insert("rebase_plans".to_owned(), json!(outcome.plans));
        object.insert("rebase_receipts".to_owned(), json!(outcome.receipts));
        object.insert(
            "provider_receipts".to_owned(),
            json!(outcome.provider_receipts),
        );
        object.insert("completed_steps".to_owned(), json!(outcome.steps));
        object.insert("resumable".to_owned(), json!(true));
        let deterministic_history_decision = matches!(
            error.code().as_str(),
            "rebase_nonlinear_range"
                | "rebase_range_ambiguous"
                | "rebase_empty_patch_range"
                | "rebase_target_history_changed"
                | "rebase_repository_not_owned"
                | "rebase_historical_target_mismatch"
                | "rebase_unsupported_octopus"
                | "rebase_topology_limit"
                | "rebase_external_merge_parents"
                | "rebase_cousin_history"
                | "rebase_merge_tree_conflict"
                | "rebase_merge_replay_conflict"
                | "rebase_merge_tree_mismatch"
                | "rebase_topology_changed"
        );
        object.insert(
            "next".to_owned(),
            json!(if deterministic_history_decision {
                "the unchanged exact generation cannot succeed by retry: inspect the reported topology and explicitly repair/reshape/evict, use an audited merge-preserving strategy, or change the candidate head before rerunning"
            } else {
                "rediscover provider state and rerun `cara sync --all`"
            }),
        );
        if deterministic_history_decision {
            object.insert("retryable".to_owned(), json!(false));
            object.insert(
                "suggested_actions".to_owned(),
                json!([
                    "inspect the exact candidate/base/default topology and merge OIDs",
                    "repair, reshape, or evict the affected PR through a reviewed first-party operation",
                    "use an explicit audited merge-preserving rewrite strategy when available",
                    "rerun only after the candidate head, target generation, config, or supported strategy changes"
                ]),
            );
        }
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

fn checkout_for_decision(
    context: &AppContext,
    error: AppError,
    operation_deadline: Instant,
) -> AppError {
    let Some(details) = error.details() else {
        return error;
    };
    let Some(decision_value) = details.get("decision") else {
        return error;
    };
    let Ok(decision) = serde_json::from_value::<DecisionPoint>(decision_value.clone()) else {
        return error;
    };
    let target = decision_checkout_target(&decision);
    let Some(target) = target else {
        return error;
    };
    let Some(pull_request) = decision_checkout_pull_request(&decision, target) else {
        return attach_checkout_evidence(
            &error,
            details,
            json!({
                "state": "skipped",
                "pr": target,
                "error": {
                    "category": "validation",
                    "code": "decision_checkout_snapshot_missing",
                    "message": "the decision did not preserve the affected PR snapshot",
                },
                "next": "rediscover status and check out the affected PR before repairing the decision",
            }),
        );
    };
    let checkout = match crate::navigation::checkout_decision_snapshot(
        context,
        &pull_request,
        operation_deadline,
    ) {
        Ok(lock_recovery) => json!({
            "state": "completed",
            "pr": target,
            "branch": pull_request.head.name,
            "oid": pull_request.head.oid,
            "lock_recovery": lock_recovery,
        }),
        Err(checkout_error) => json!({
            "state": "skipped",
            "pr": target,
            "error": {
                "category": checkout_error.category(),
                "code": checkout_error.code(),
                "message": checkout_error.message(),
                "details": checkout_error.details(),
            },
            "next": "make the local worktree safe, then check out the affected PR before repairing the decision",
        }),
    };
    attach_checkout_evidence(&error, details, checkout)
}

fn attach_checkout_evidence(error: &AppError, mut details: Value, checkout: Value) -> AppError {
    if let Some(object) = details.as_object_mut() {
        object.insert("checkout".to_owned(), checkout);
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

fn decision_checkout_pull_request(
    decision: &DecisionPoint,
    target: PrNumber,
) -> Option<PullRequestSnapshot> {
    decision
        .evidence
        .get("pull_request")
        .and_then(|value| serde_json::from_value::<PullRequestSnapshot>(value.clone()).ok())
        .filter(|pull_request| pull_request.number == target)
        .or_else(|| {
            decision
                .evidence
                .get("pull_requests")
                .and_then(|value| {
                    serde_json::from_value::<Vec<PullRequestSnapshot>>(value.clone()).ok()
                })
                .and_then(|pull_requests| {
                    pull_requests
                        .into_iter()
                        .find(|pull_request| pull_request.number == target)
                })
        })
}

fn decision_checkout_target(decision: &DecisionPoint) -> Option<PrNumber> {
    match decision.kind {
        DecisionKind::HeadConflict | DecisionKind::CiFailure => {
            decision.affected_prs.first().copied()
        }
        DecisionKind::LinkConflict => decision.affected_prs.last().copied(),
        _ => None,
    }
}

fn successful_scheduler_status(
    status: &StatusOutput,
    ci: &[CiObservation],
    paused: &[crate::pause::PauseStatus],
    rebase_on_join: bool,
) -> SyncSchedulerStatus {
    let ci_by_pr = ci
        .iter()
        .map(|observation| (observation.pr, observation.disposition))
        .collect::<BTreeMap<_, _>>();
    let candidates = status
        .merge_candidates
        .iter()
        .cloned()
        .map(|candidate| (candidate.pr, candidate))
        .collect::<BTreeMap<_, _>>();
    let caravans = status
        .analysis
        .fleet
        .caravans
        .iter()
        .map(|caravan| {
            let members = caravan
                .members
                .iter()
                .filter_map(|number| {
                    status
                        .analysis
                        .pull_requests
                        .get(number)
                        .map(|pull_request| SyncMemberGeneration {
                            pr: *number,
                            head: pull_request.head.clone(),
                            base: pull_request.base.clone(),
                            candidate: candidates.get(number).cloned(),
                            ci: ci_by_pr.get(number).copied(),
                        })
                })
                .collect::<Vec<_>>();
            SyncCaravanGeneration {
                caravan_id: caravan.id,
                root: caravan.head().expect("caravans are non-empty"),
                tail: caravan.tail().expect("caravans are non-empty"),
                members,
            }
        })
        .collect::<Vec<_>>();
    let waiting_prs = ci
        .iter()
        .filter(|observation| observation.disposition == CiDisposition::Waiting)
        .map(|observation| observation.pr)
        .collect::<Vec<_>>();
    let held_caravans = paused
        .iter()
        .map(|pause| pause.record.caravan_head)
        .collect::<Vec<_>>();
    let (disposition, reason) = if !waiting_prs.is_empty() {
        (
            SchedulerDisposition::WaitingCi,
            "fresh or pending CI is the only incomplete condition; do not wake a repair actor",
        )
    } else if !held_caravans.is_empty() {
        (
            SchedulerDisposition::Held,
            "one or more caravans are intentionally held; only explicit resume may release them",
        )
    } else {
        (
            SchedulerDisposition::Healthy,
            "the exact provider graph and selected root-to-tail generations are converged",
        )
    };
    SyncSchedulerStatus {
        schema_version: 1,
        disposition,
        wake_class: SchedulerWakeClass::None,
        rebase_on_join,
        default_branch: status.analysis.fleet.default_branch.clone(),
        caravans,
        waiting_prs,
        held_caravans,
        reason: reason.to_owned(),
    }
}

fn scheduler_failure_status(error: &AppError) -> SyncFailureSchedulerStatus {
    let error_code = error.code();
    let decision = error.details().and_then(|details| {
        serde_json::from_value::<DecisionPoint>(details.get("decision")?.clone()).ok()
    });
    let (disposition, wake_class, retryable) = match decision.map(|item| item.kind) {
        Some(DecisionKind::StalePrecondition) => (
            SchedulerDisposition::RetryTick,
            SchedulerWakeClass::RetryTick,
            true,
        ),
        Some(DecisionKind::UnsafeCheckout | DecisionKind::HookFailure) => (
            SchedulerDisposition::OperatorAction,
            SchedulerWakeClass::OperatorAction,
            false,
        ),
        Some(_) => (
            SchedulerDisposition::ExternalDecision,
            SchedulerWakeClass::ExternalDecision,
            false,
        ),
        None if matches!(
            error_code.as_str(),
            "rebase_conflict"
                | "rebase_nonlinear_range"
                | "rebase_range_ambiguous"
                | "rebase_empty_patch_range"
                | "rebase_target_history_changed"
                | "rebase_repository_not_owned"
                | "rebase_historical_target_mismatch"
                | "rebase_unsupported_octopus"
                | "rebase_topology_limit"
                | "rebase_external_merge_parents"
                | "rebase_cousin_history"
                | "rebase_merge_tree_conflict"
                | "rebase_merge_replay_conflict"
                | "rebase_merge_tree_mismatch"
                | "rebase_topology_changed"
                | "rebase_midpoint_head_stale"
                | "rebase_midpoint_pr_missing"
                | "rebase_prepared_object_changed"
                | "rebase_result_invalid"
                | "rebase_worker_panicked"
        ) =>
        {
            (
                SchedulerDisposition::ExternalDecision,
                SchedulerWakeClass::ExternalDecision,
                false,
            )
        }
        None if matches!(
            error_code.as_str(),
            "default_branch_not_protected"
                | "rebase_ci_trigger_missing"
                | "repository_not_initialized"
                | "unsafe_checkout"
        ) =>
        {
            (
                SchedulerDisposition::OperatorAction,
                SchedulerWakeClass::OperatorAction,
                false,
            )
        }
        None => (
            SchedulerDisposition::RetryTick,
            SchedulerWakeClass::RetryTick,
            true,
        ),
    };
    SyncFailureSchedulerStatus {
        schema_version: 1,
        disposition,
        wake_class,
        retryable,
        error_code,
    }
}

fn scheduler_decision_fingerprint(error: &AppError) -> String {
    let details = error.details().unwrap_or_else(|| json!({}));
    let material = serde_json::to_vec(&json!({
        "error_code": error.code(),
        "repository": details.get("repository"),
        "caravan_id": details.get("caravan_id"),
        "affected_prs": details.get("affected_prs"),
        "pr": details.get("pr"),
        "merge_oids": details.get("merge_oids"),
        "rebase_plans": details.get("rebase_plans"),
        "decision": details.get("decision"),
    }))
    .expect("scheduler fingerprint material serializes");
    crate::membership::fnv1a64(&material)
}

fn attach_scheduler_failure(
    error: &AppError,
    scheduler_status: &SyncFailureSchedulerStatus,
) -> AppError {
    let mut details = error.details().unwrap_or_else(|| json!({}));
    if let Some(object) = details.as_object_mut() {
        object.insert("scheduler_status".to_owned(), json!(scheduler_status));
        if scheduler_status.wake_class == SchedulerWakeClass::ExternalDecision {
            object.insert(
                "decision_fingerprint".to_owned(),
                json!(scheduler_decision_fingerprint(error)),
            );
        }
    } else {
        details = json!({
            "original_details": details,
            "scheduler_status": scheduler_status,
        });
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

fn sync_failed_event(error: &AppError) -> Option<CaravanEvent> {
    let details = error.details()?;
    if let Some(decision) = details
        .get("decision")
        .and_then(|value| serde_json::from_value::<DecisionPoint>(value.clone()).ok())
    {
        let fleet = decision
            .evidence
            .get("fleet")
            .and_then(|value| serde_json::from_value(value.clone()).ok());
        return Some(hooks::event(
            EventKind::SyncFailed,
            decision.operation_id,
            decision.repository,
            decision.caravan_id,
            decision.affected_prs,
            fleet,
            Some(decision.message),
            BTreeMap::from([
                ("error_code".to_owned(), json!(error.code())),
                (
                    "scheduler_status".to_owned(),
                    details.get("scheduler_status").cloned().unwrap_or_default(),
                ),
                (
                    "decision_fingerprint".to_owned(),
                    details
                        .get("decision_fingerprint")
                        .cloned()
                        .unwrap_or_default(),
                ),
            ]),
        ));
    }

    let scheduler_status = serde_json::from_value::<SyncFailureSchedulerStatus>(
        details.get("scheduler_status")?.clone(),
    )
    .ok()?;
    if scheduler_status.wake_class != SchedulerWakeClass::ExternalDecision {
        return None;
    }
    let repository =
        serde_json::from_value::<RepositoryId>(details.get("repository")?.clone()).ok()?;
    let mut prs = BTreeSet::new();
    if let Some(pr) = details
        .get("pr")
        .and_then(|value| serde_json::from_value::<PrNumber>(value.clone()).ok())
    {
        prs.insert(pr);
    }
    if let Some(affected) = details
        .get("affected_prs")
        .and_then(|value| serde_json::from_value::<Vec<PrNumber>>(value.clone()).ok())
    {
        prs.extend(affected);
    }
    if let Some(plans) = details.get("rebase_plans").and_then(|value| {
        serde_json::from_value::<Vec<crate::physical_rebase::RebasePlan>>(value.clone()).ok()
    }) {
        prs.extend(plans.into_iter().map(|plan| plan.pr));
    }
    if let Some(receipts) = details.get("rebase_receipts").and_then(|value| {
        serde_json::from_value::<Vec<crate::physical_rebase::RebaseReceipt>>(value.clone()).ok()
    }) {
        prs.extend(receipts.into_iter().map(|receipt| receipt.pr));
    }
    let caravan_id = details
        .get("caravan_id")
        .and_then(|value| serde_json::from_value::<PrNumber>(value.clone()).ok());
    Some(hooks::event(
        EventKind::SyncFailed,
        hooks::operation_id_from_error(error),
        repository,
        caravan_id,
        prs.into_iter().collect(),
        None,
        Some(error.message()),
        BTreeMap::from([
            ("error_code".to_owned(), json!(error.code())),
            ("scheduler_status".to_owned(), json!(scheduler_status)),
            (
                "provider_invariant".to_owned(),
                json!(details.get("rebase_receipts").is_some()),
            ),
            (
                "decision_fingerprint".to_owned(),
                details
                    .get("decision_fingerprint")
                    .cloned()
                    .unwrap_or_default(),
            ),
        ]),
    ))
}

fn sync_without_hooks(
    context: &AppContext,
    input: &SyncInput,
    started: Instant,
    operation_deadline: Instant,
) -> Result<SyncOutput, AppError> {
    let lock = OperationLock::acquire(&context.repository_path, "sync")?;
    let lock_recovery = lock.recovered_dead_owner().cloned();
    sync_with_lock(
        context,
        input,
        started,
        operation_deadline,
        lock,
        lock_recovery.clone(),
    )
    .map_err(|error| attach_lock_recovery(error, lock_recovery.as_ref()))
}

#[allow(clippy::too_many_lines)]
fn sync_with_lock(
    context: &AppContext,
    input: &SyncInput,
    started: Instant,
    operation_deadline: Instant,
    mut lock: OperationLock,
    lock_recovery: Option<OperationLockRecovery>,
) -> Result<SyncOutput, AppError> {
    lock.checkpoint(
        "initial_discovery_in_flight",
        json!({
            "operation": "sync",
            "all": input.all,
            "deadline_ms": duration_millis(operation_deadline.saturating_duration_since(started)),
        }),
        false,
    )?;
    let timeout = Duration::from_secs(context.config.command_timeout_secs);
    let github_budget =
        crate::command::GithubRequestBudget::new(context.config.sync.max_github_requests_per_tick);
    let initial_status_started = Instant::now();
    let mut status =
        read::status_with_deadline_and_budget(context, operation_deadline, Some(&github_budget))?;
    let initial_status_elapsed = initial_status_started.elapsed();
    crate::initialization::require_ready(&status.initialization)?;
    let runner = crate::command::ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(timeout)
        .with_operation_deadline(operation_deadline)
        .with_github_request_budget(github_budget.clone());
    // A decision can require an exact branch checkout. Prove checkout safety
    // before the first provider mutation so a dirty worktree can never turn a
    // partially-mutated sync into an unrepairable decision receipt.
    crate::navigation::ensure_safe_worktree(
        &context.repository_path,
        &context.config_path,
        &runner,
    )?;
    let provider = GitHubMutationAdapter::new(runner);
    let convergence_started = Instant::now();
    let mut physical_rebuild = PhysicalRebuildOutcome::default();
    if context.config.rebase_on_join {
        lock.checkpoint(
            "physical_rebase_planning_in_flight",
            json!({
                "operation": "sync",
                "all": input.all,
                "default_branch": status.analysis.fleet.default_branch,
                "write_barrier": "no provider or branch writes before all selected plans verify",
            }),
            false,
        )?;
        let (prepared, progress) =
            prepare_physical_chains(context, &status, input.all, &provider, operation_deadline)?;
        let plans = prepared
            .iter()
            .flat_map(|chain| chain.members.iter().map(|item| item.plan.clone()))
            .collect::<Vec<_>>();
        lock.checkpoint(
            "physical_rebase_global_preflight_complete",
            json!({
                "rebase_plans": checkpoint_rebase_plans(&plans),
                "provider_writes": 0,
                "branch_writes": 0
            }),
            false,
        )?;
        let mut progress = progress;
        invalidate_rewritten_force_intents(&status, &provider, &plans, &mut progress)?;
        physical_rebuild = apply_physical_chains(&status, &provider, &prepared, progress)?;
        lock.checkpoint(
            "physical_rebase_applied",
            json!({
                "rebase_plans": checkpoint_rebase_plans(&physical_rebuild.plans),
                "rebase_receipts": checkpoint_rebase_receipts(&physical_rebuild.receipts),
                "provider_receipts": checkpoint_provider_receipts(&physical_rebuild.provider_receipts),
            }),
            false,
        )?;
        let midpoint = read::status_with_deadline_and_budget(
            context,
            operation_deadline,
            Some(&github_budget),
        )
        .map_err(|error| attach_physical_rebuild(error, &physical_rebuild))?;
        for receipt in &physical_rebuild.receipts {
            let observed = midpoint
                .analysis
                .pull_requests
                .get(&receipt.pr)
                .ok_or_else(|| {
                    attach_physical_rebuild(
                        AppError::structured(
                            ErrorCategory::Validation,
                            "rebase_midpoint_pr_missing",
                            "rewritten PR disappeared during mandatory midpoint rediscovery",
                            Some(json!({"receipt": receipt, "resumable": true})),
                        ),
                        &physical_rebuild,
                    )
                })?;
            if observed.head.oid != receipt.new_head_oid {
                return Err(attach_physical_rebuild(
                    AppError::structured(
                        ErrorCategory::Validation,
                        "rebase_midpoint_head_stale",
                        "midpoint discovery did not contain the exact planned head",
                        Some(
                            json!({"receipt": receipt, "observed_head": observed.head.oid, "resumable": true}),
                        ),
                    ),
                    &physical_rebuild,
                ));
            }
        }
        status = midpoint;
    }
    lock.checkpoint(
        "provider_convergence_in_flight",
        json!({
            "operation": "sync",
            "repository": &status.repository,
            "default_branch": &status.analysis.fleet.default_branch,
            "initial_status_timing": &status.timing,
            "selected_caravans": if input.all {
                json!(status.analysis.fleet.caravans.iter().map(|caravan| caravan.id).collect::<Vec<_>>())
            } else {
                json!(status.current_pr.and_then(|pr| status.analysis.fleet.containing(pr).map(|caravan| caravan.id)))
            },
            "recovery": "rediscover provider state and replay the same idempotent sync",
        }),
        true,
    )?;
    let physical_mutations = u32::try_from(
        physical_rebuild
            .steps
            .iter()
            .filter(|step| step.state == MutationStepState::Completed)
            .count(),
    )
    .unwrap_or(u32::MAX);
    let mut progress = execute_bounded(
        &status,
        &provider,
        input.all,
        input.rerun_failed,
        context.config.force_merge,
        context
            .config
            .sync
            .max_mutations_per_tick
            .saturating_sub(physical_mutations),
    )?;
    if context.config.rebase_on_join {
        physical_rebuild.steps.append(&mut progress.steps);
        progress.steps = physical_rebuild.steps;
        physical_rebuild
            .provider_receipts
            .append(&mut progress.provider_receipts);
        progress.provider_receipts = physical_rebuild.provider_receipts;
        progress.rebase_plans = physical_rebuild.plans;
        progress.rebase_receipts = physical_rebuild.receipts;
        progress.mutation_limit = context.config.sync.max_mutations_per_tick;
    }
    let convergence_elapsed = convergence_started.elapsed();
    lock.checkpoint(
        "provider_converged",
        sync_checkpoint_evidence(&progress),
        false,
    )?;
    lock.checkpoint(
        "final_discovery_in_flight",
        sync_checkpoint_evidence(&progress),
        false,
    )?;

    // A fresh graph is the authoritative completion receipt. It detects a
    // default-branch or fleet change that raced after the preflight proof.
    let final_status_started = Instant::now();
    let mut final_status = read::status_with_deadline_and_budget(
        context,
        operation_deadline,
        Some(&github_budget),
    )
    .map_err(|error| {
        AppError::structured(
            error.category(),
            if error.category() == ErrorCategory::Timeout {
                "sync_operation_timeout"
            } else {
                "sync_rediscovery_failed"
            },
            error.to_string(),
            Some(json!({
                "operation_receipt": progress.operation_receipt(),
                "provider_receipts": progress.provider_receipts,
                "events": progress.events,
                "phase": "final_status",
                "elapsed_ms": duration_millis(started.elapsed()),
                "deadline_ms": duration_millis(operation_deadline.saturating_duration_since(started)),
                "source": error.details(),
                "resumable": true,
                "next": "rerun `cara sync` to rediscover GitHub state",
            })),
        )
    })?;
    if let Some(problem) = final_status.analysis.fleet.problems.first() {
        return Err(decision_error(
            &decision_for_problem(problem, &final_status, &progress),
            &progress,
        ));
    }

    let mut auto_admission = AutoAdmissionOutput::disabled(context, input.all);
    if context.config.sync.actions.join_unlabelled_prs && input.all {
        lock.checkpoint(
            "automatic_admission_in_flight",
            json!({
                "heuristic_version": AUTO_ADMISSION_HEURISTIC_VERSION,
                "candidate_limit": context.config.sync.max_candidates_per_tick,
                "mutation_limit": context.config.sync.max_mutations_per_tick,
                "github_request_limit": github_budget.limit(),
                "existing_fleet_converged": true,
            }),
            true,
        )?;
        let (admitted_status, admission) = run_auto_admission(
            context,
            final_status,
            &provider,
            &mut progress,
            operation_deadline,
            &github_budget,
        )
        .map_err(|error| {
            attach_auto_admission_progress(&error, context, &progress, &github_budget)
        })?;
        final_status = admitted_status;
        auto_admission = admission;

        // Membership operations leave exact provider state, but a final normal
        // convergence pass owns CI observation and rolling-head invariants for
        // newly admitted members under the same lock and deadline.
        if !auto_admission.joins.is_empty() {
            let post_admission = execute_bounded(
                &final_status,
                &provider,
                true,
                input.rerun_failed,
                context.config.force_merge,
                context
                    .config
                    .sync
                    .max_mutations_per_tick
                    .saturating_sub(completed_mutation_count(&progress)),
            )
            .map_err(|error| {
                attach_auto_admission_progress(&error, context, &progress, &github_budget)
            })?;
            merge_sync_progress(&mut progress, post_admission);
            final_status = read::status_with_deadline_and_budget(
                context,
                operation_deadline,
                Some(&github_budget),
            )?;
            if let Some(problem) = final_status.analysis.fleet.problems.first() {
                return Err(decision_error(
                    &decision_for_problem(problem, &final_status, &progress),
                    &progress,
                ));
            }
        }
        auto_admission.github_requests_used = github_budget.used();
        auto_admission.mutations_used = completed_mutation_count(&progress);
        lock.checkpoint(
            "automatic_admission_complete",
            json!({
                "auto_admission": checkpoint_auto_admission(&auto_admission),
                "provider_state": sync_checkpoint_evidence(&progress),
            }),
            false,
        )?;
    }

    auto_admission.github_requests_used = github_budget.used();
    auto_admission.mutations_used = completed_mutation_count(&progress);
    let final_status_elapsed = final_status_started.elapsed();
    lock.checkpoint("completed", sync_checkpoint_evidence(&progress), false)?;

    let scheduler_status = successful_scheduler_status(
        &final_status,
        &progress.ci,
        &progress.paused_caravans,
        context.config.rebase_on_join,
    );
    Ok(SyncOutput {
        receipt: progress.operation_receipt(),
        auto_admission,
        scheduler_status,
        timing: Some(SyncTiming {
            deadline_ms: duration_millis(operation_deadline.saturating_duration_since(started)),
            total_ms: duration_millis(started.elapsed()),
            initial_status_ms: duration_millis(initial_status_elapsed),
            provider_convergence_ms: duration_millis(convergence_elapsed),
            final_status_ms: duration_millis(final_status_elapsed),
        }),
        lock_recovery,
        provider_receipts: progress.provider_receipts,
        rebase_plans: progress.rebase_plans,
        rebase_receipts: progress.rebase_receipts,
        historical_predecessor: read::historical_predecessor(&status),
        synchronized_caravans: progress.synchronized_caravans,
        paused_caravans: progress.paused_caravans,
        head_advancements: progress.head_advancements,
        ci: progress.ci,
        events: progress.events,
        hook_deliveries: Vec::new(),
        status: final_status,
    })
}

fn attach_auto_admission_progress(
    error: &AppError,
    context: &AppContext,
    progress: &SyncProgress,
    github_budget: &crate::command::GithubRequestBudget,
) -> AppError {
    let mut details = error.details().unwrap_or_else(|| json!({}));
    if let Some(object) = details.as_object_mut() {
        object.insert(
            "auto_admission".to_owned(),
            json!({
                "enabled": true,
                "heuristic_version": AUTO_ADMISSION_HEURISTIC_VERSION,
                "operation_receipt": progress.operation_receipt(),
                "provider_receipts": progress.provider_receipts,
                "rebase_receipts": progress.rebase_receipts,
                "github_requests_used": github_budget.used(),
                "github_request_limit": github_budget.limit(),
                "mutations_used": completed_mutation_count(progress),
                "mutation_limit": context.config.sync.max_mutations_per_tick,
                "resumable": true,
                "next": "rerun the same bounded `cara sync --all` tick; fresh provider state is the cursor",
            }),
        );
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

fn completed_mutation_count(progress: &SyncProgress) -> u32 {
    u32::try_from(
        progress
            .steps
            .iter()
            .filter(|step| step.state == MutationStepState::Completed)
            .count(),
    )
    .unwrap_or(u32::MAX)
}

fn merge_sync_progress(target: &mut SyncProgress, mut source: SyncProgress) {
    target.steps.append(&mut source.steps);
    target
        .provider_receipts
        .append(&mut source.provider_receipts);
    target.rebase_plans.append(&mut source.rebase_plans);
    target.rebase_receipts.append(&mut source.rebase_receipts);
    target.paused_caravans.append(&mut source.paused_caravans);
    target
        .head_advancements
        .append(&mut source.head_advancements);
    target.events.append(&mut source.events);
    for observation in source.ci {
        target.ci.retain(|existing| existing.pr != observation.pr);
        target.ci.push(observation);
    }
    target
        .synchronized_caravans
        .append(&mut source.synchronized_caravans);
    target.synchronized_caravans.sort_unstable();
    target.synchronized_caravans.dedup();
    target.current = source.current;
    target.merge_candidates = source.merge_candidates;
}

#[allow(clippy::too_many_lines)]
fn run_auto_admission(
    context: &AppContext,
    mut status: StatusOutput,
    provider: &impl SyncProvider,
    progress: &mut SyncProgress,
    operation_deadline: Instant,
    github_budget: &crate::command::GithubRequestBudget,
) -> Result<(StatusOutput, AutoAdmissionOutput), AppError> {
    let mut output = AutoAdmissionOutput {
        enabled: true,
        heuristic_version: AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
        continuation: AutoAdmissionContinuation::Complete,
        candidates_considered: 0,
        mutations_used: completed_mutation_count(progress),
        mutation_limit: context.config.sync.max_mutations_per_tick,
        github_requests_used: github_budget.used(),
        github_request_limit: github_budget.limit(),
        joins: Vec::new(),
        skips: Vec::new(),
        remaining_candidates: Vec::new(),
    };
    progress.current = status.analysis.pull_requests.clone();
    progress.merge_candidates = status
        .merge_candidates
        .iter()
        .cloned()
        .map(|candidate| (candidate.pr, candidate))
        .collect();

    let checker = crate::graph::GitCompatibilityChecker::new(&context.repository_path, "origin")
        .with_timeout(Duration::from_secs(context.config.command_timeout_secs))
        .with_operation_deadline(operation_deadline);
    let mut validated_skips = BTreeSet::new();

    // Revalidate persisted skips without recomputing compatibility. Exact
    // candidate/default/tail/config generations are enough to prove whether the
    // old heuristic receipt remains current.
    loop {
        if Instant::now() >= operation_deadline {
            output.continuation = AutoAdmissionContinuation::DeadlineExhausted;
            break;
        }
        if github_budget.used() >= github_budget.limit() {
            output.continuation = AutoAdmissionContinuation::GithubRequestBudgetExhausted;
            break;
        }
        let next = status
            .admission
            .skipped
            .iter()
            .find(|candidate| !validated_skips.contains(&candidate.pr))
            .cloned();
        let Some(skipped) = next else {
            break;
        };
        let comments = provider
            .pull_request_comment_bodies(&status.repository, skipped.pr)
            .map_err(|error| mutation_error(&error, progress, Some(skipped.pr)))?;
        let retained = comments
            .iter()
            .rev()
            .find_map(|body| AutoJoinSkipReceipt::from_comment(body));
        if retained
            .as_ref()
            .is_some_and(|receipt| skip_receipt_matches(context, &status, receipt))
        {
            let receipt = retained.expect("checked as present");
            validated_skips.insert(skipped.pr);
            output.skips.push(receipt);
            progress.already(
                MutationKind::Comment,
                skipped.pr,
                "generation-bound automatic admission skip remains exact",
            );
            continue;
        }

        if !has_mutation_capacity(context, progress, 2) {
            output.continuation = AutoAdmissionContinuation::MutationBudgetExhausted;
            break;
        }
        let candidate = status
            .analysis
            .pull_requests
            .get(&skipped.pr)
            .cloned()
            .ok_or_else(|| {
                AppError::validation(
                    "auto_admission_candidate_missing",
                    format!("skipped candidate #{} disappeared", skipped.pr),
                )
            })?;
        let old_hash = retained
            .as_ref()
            .map_or("missing", |receipt| receipt.evidence_hash.as_str());
        let marker = format!(
            "<!-- caravan-auto-join-skip-invalidated:{}:{}:{} -->",
            skipped.pr, candidate.head.oid, old_hash,
        );
        let body = format!(
            "{marker}\n### Cara automatic admission skip invalidated\n\nThe prior skip evidence `{old_hash}` no longer matches the candidate, default, tail, config, or heuristic generation. The candidate is eligible for a fresh bounded retry.\n"
        );
        let comment = provider
            .ensure_marked_comment(
                &status.repository,
                &PullRequestPrecondition::from(&candidate),
                &marker,
                &body,
            )
            .map_err(|error| mutation_error(&error, progress, Some(skipped.pr)))?;
        record_marked_comment(
            progress,
            comment,
            skipped.pr,
            "posted stale-skip invalidation authorization",
        );
        let removed = provider
            .remove_label(
                &status.repository,
                &progress.precondition(skipped.pr),
                AUTO_ADMISSION_SKIP_LABEL,
            )
            .map_err(|error| mutation_error(&error, progress, Some(skipped.pr)))?;
        progress.record(
            removed,
            "removed stale generation-bound automatic admission skip",
        );
        status = read::status_with_deadline_and_budget(
            context,
            operation_deadline,
            Some(github_budget),
        )?;
        progress.current = status.analysis.pull_requests.clone();
        progress.merge_candidates = status
            .merge_candidates
            .iter()
            .cloned()
            .map(|candidate| (candidate.pr, candidate))
            .collect();
    }

    while output.continuation == AutoAdmissionContinuation::Complete {
        if Instant::now() >= operation_deadline {
            output.continuation = AutoAdmissionContinuation::DeadlineExhausted;
            break;
        }
        if github_budget.used() >= github_budget.limit() {
            output.continuation = AutoAdmissionContinuation::GithubRequestBudgetExhausted;
            break;
        }
        if output.candidates_considered >= context.config.sync.max_candidates_per_tick {
            if !status.admission.candidates.is_empty() || !status.admission.rejected.is_empty() {
                output.continuation = AutoAdmissionContinuation::CandidateBudgetExhausted;
            }
            break;
        }
        let Some(next_pr) = status.admission.next_candidate else {
            break;
        };
        let Some(candidate_order) = status
            .admission
            .candidates
            .iter()
            .find(|candidate| candidate.pr == next_pr)
            .cloned()
        else {
            output.continuation = AutoAdmissionContinuation::RejectedCanonicalCandidate;
            break;
        };
        let candidate = status
            .analysis
            .pull_requests
            .get(&next_pr)
            .cloned()
            .ok_or_else(|| {
                AppError::validation(
                    "auto_admission_candidate_missing",
                    format!("canonical candidate #{next_pr} disappeared"),
                )
            })?;
        output.candidates_considered += 1;
        let evaluation = evaluate_auto_candidate(&status, &candidate, &checker)?;

        if matches!(evaluation.target, AutoCandidateTarget::Skip) {
            if !has_mutation_capacity(context, progress, 2) {
                output.continuation = AutoAdmissionContinuation::MutationBudgetExhausted;
                break;
            }
            let receipt = AutoJoinSkipReceipt {
                schema_version: 1,
                repository: status.repository.clone(),
                candidate_pr: candidate.number,
                candidate_head: candidate.head.clone(),
                candidate_base: candidate.base.clone(),
                default_branch: status.analysis.fleet.default_branch.clone(),
                tested_tails: evaluation.tested_tails,
                config_fingerprint: auto_admission_config_fingerprint(context),
                heuristic_version: AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
                compatibility_reasons: evaluation.reasons,
                actor: "cara sync automatic admission".to_owned(),
                observed_unix_secs: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                evidence_hash: String::new(),
            }
            .finalize_hash();
            persist_auto_skip(provider, progress, &status.repository, &receipt)?;
            output.skips.push(receipt);
        } else {
            let target_tail = match evaluation.target {
                AutoCandidateTarget::New => None,
                AutoCandidateTarget::Join(tail) => Some(tail),
                AutoCandidateTarget::Skip => unreachable!("checked above"),
            };
            let conservative_membership_bound =
                u32::try_from(context.config.agent_priority_labels.len().saturating_mul(2) + 12)
                    .unwrap_or(u32::MAX);
            if !has_mutation_capacity(context, progress, conservative_membership_bound) {
                output.continuation = AutoAdmissionContinuation::MutationBudgetExhausted;
                break;
            }
            let membership = crate::membership::auto_admit_locked(
                context,
                candidate.number,
                target_tail,
                candidate_order.priority_label,
                operation_deadline,
                github_budget,
            )?;
            append_membership_progress(progress, &membership);
            output.joins.push(AutoAdmissionJoinReceipt {
                candidate_pr: candidate.number,
                target_tail,
                membership,
            });
        }

        status = read::status_with_deadline_and_budget(
            context,
            operation_deadline,
            Some(github_budget),
        )?;
        progress.current = status.analysis.pull_requests.clone();
        progress.merge_candidates = status
            .merge_candidates
            .iter()
            .cloned()
            .map(|candidate| (candidate.pr, candidate))
            .collect();
    }

    output.mutations_used = completed_mutation_count(progress);
    output.github_requests_used = github_budget.used();
    output.remaining_candidates = status
        .admission
        .candidates
        .iter()
        .map(|candidate| candidate.pr)
        .chain(
            status
                .admission
                .rejected
                .iter()
                .map(|candidate| candidate.pr),
        )
        .collect();
    Ok((status, output))
}

fn has_mutation_capacity(context: &AppContext, progress: &SyncProgress, reserve: u32) -> bool {
    completed_mutation_count(progress).saturating_add(reserve)
        <= context.config.sync.max_mutations_per_tick
}

fn append_membership_progress(
    progress: &mut SyncProgress,
    membership: &crate::membership::MembershipOutput,
) {
    progress
        .steps
        .extend(membership.receipt.completed_steps.iter().cloned());
    progress
        .provider_receipts
        .extend(membership.provider_receipts.iter().cloned());
    if let Some(receipt) = &membership.rebase_receipt {
        progress.steps.push(MutationStep {
            kind: MutationKind::RebaseBranch,
            state: if receipt.already_satisfied {
                MutationStepState::AlreadySatisfied
            } else {
                MutationStepState::Completed
            },
            pr: Some(receipt.pr),
            summary: if receipt.already_satisfied {
                "automatic admission candidate already had exact target ancestry".to_owned()
            } else {
                "automatic admission rebased candidate under exact force-with-lease".to_owned()
            },
        });
        progress.rebase_receipts.push(receipt.clone());
    }
    progress.events.extend(membership.events.iter().cloned());
    progress.current.insert(
        membership.pull_request.number,
        membership.pull_request.clone(),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoCandidateTarget {
    New,
    Join(PrNumber),
    Skip,
}

struct AutoCandidateEvaluation {
    target: AutoCandidateTarget,
    tested_tails: Vec<AutoAdmissionTailGeneration>,
    reasons: Vec<String>,
}

fn evaluate_auto_candidate(
    status: &StatusOutput,
    candidate: &PullRequestSnapshot,
    checker: &impl crate::graph::CompatibilityChecker,
) -> Result<AutoCandidateEvaluation, AppError> {
    let mut virtual_status = status.clone();
    virtual_status.current_pr = Some(candidate.number);
    if let Some(virtual_candidate) = virtual_status
        .analysis
        .pull_requests
        .get_mut(&candidate.number)
    {
        virtual_candidate.labels.remove(AUTO_ADMISSION_SKIP_LABEL);
    }
    let tails = current_tail_generations(status);
    if tails.is_empty() {
        let output = check_auto_target(&virtual_status, &CheckInput::default(), checker)?;
        if output.eligible {
            return Ok(AutoCandidateEvaluation {
                target: AutoCandidateTarget::New,
                tested_tails: tails,
                reasons: Vec::new(),
            });
        }
        return Ok(AutoCandidateEvaluation {
            target: AutoCandidateTarget::Skip,
            tested_tails: tails,
            reasons: check_reasons(&output),
        });
    }

    let mut reasons = Vec::new();
    for tail in &tails {
        if tail.held {
            reasons.push(format!(
                "tail #{}: caravan #{} is intentionally held",
                tail.tail_pr, tail.caravan_id
            ));
            continue;
        }
        let output = check_auto_target(
            &virtual_status,
            &CheckInput {
                pr: None,
                tail_pr: Some(tail.tail_pr.0),
                head_pr: None,
            },
            checker,
        )?;
        if output.eligible {
            return Ok(AutoCandidateEvaluation {
                target: AutoCandidateTarget::Join(tail.tail_pr),
                tested_tails: tails,
                reasons,
            });
        }
        reasons.extend(
            check_reasons(&output)
                .into_iter()
                .map(|reason| format!("tail #{}: {reason}", tail.tail_pr)),
        );
    }
    Ok(AutoCandidateEvaluation {
        target: AutoCandidateTarget::Skip,
        tested_tails: tails,
        reasons,
    })
}

fn check_auto_target(
    status: &StatusOutput,
    input: &CheckInput,
    checker: &impl crate::graph::CompatibilityChecker,
) -> Result<crate::read::CheckOutput, AppError> {
    match crate::read::check_analysis(status, input, checker) {
        Ok(output) => Ok(output),
        Err(error) if error.code() == "check_failed" => error
            .details()
            .and_then(|details| serde_json::from_value(details).ok())
            .ok_or(error),
        Err(error) => Err(error),
    }
}

fn check_reasons(output: &crate::read::CheckOutput) -> Vec<String> {
    let mut reasons = output
        .problems
        .iter()
        .map(|problem| problem.message.clone())
        .collect::<Vec<_>>();
    reasons.extend(
        output
            .compatibility
            .iter()
            .filter(|report| report.outcome != CompatibilityOutcome::Clean)
            .map(|report| {
                format!(
                    "{}@{} -> {}@{} = {:?}; paths=[{}]",
                    report.candidate.name,
                    report.candidate.oid,
                    report.target.name,
                    report.target.oid,
                    report.outcome,
                    report.conflicting_paths.join(","),
                )
            }),
    );
    if reasons.is_empty() {
        reasons.push(format!(
            "candidate preflight returned {:?}",
            output.next_action
        ));
    }
    reasons.sort();
    reasons.dedup();
    reasons
}

fn current_tail_generations(status: &StatusOutput) -> Vec<AutoAdmissionTailGeneration> {
    status
        .analysis
        .fleet
        .caravans
        .iter()
        .filter_map(|caravan| {
            let tail_pr = caravan.tail()?;
            let tail = status.analysis.pull_requests.get(&tail_pr)?;
            Some(AutoAdmissionTailGeneration {
                caravan_id: caravan.id,
                tail_pr,
                branch: tail.head.name.clone(),
                head_oid: tail.head.oid.clone(),
                held: status.pauses.iter().any(|pause| {
                    pause.state != crate::pause::PauseState::Stale
                        && pause.record.caravan_head == caravan.id
                }),
            })
        })
        .collect()
}

fn auto_admission_config_fingerprint(context: &AppContext) -> String {
    let material = serde_json::to_vec(&json!({
        "version": context.config.version,
        "rebase_on_join": context.config.rebase_on_join,
        "agent_priority_labels": &context.config.agent_priority_labels,
        "sync": &context.config.sync,
    }))
    .expect("validated config serializes");
    crate::membership::fnv1a64(&material)
}

fn skip_receipt_matches(
    context: &AppContext,
    status: &StatusOutput,
    receipt: &AutoJoinSkipReceipt,
) -> bool {
    let Some(candidate) = status.analysis.pull_requests.get(&receipt.candidate_pr) else {
        return false;
    };
    receipt.schema_version == 1
        && receipt.hash_is_valid()
        && receipt.repository == status.repository
        && receipt.candidate_head == candidate.head
        && receipt.candidate_base == candidate.base
        && receipt.default_branch == status.analysis.fleet.default_branch
        && receipt.tested_tails == current_tail_generations(status)
        && receipt.config_fingerprint == auto_admission_config_fingerprint(context)
        && receipt.heuristic_version == AUTO_ADMISSION_HEURISTIC_VERSION
}

fn persist_auto_skip(
    provider: &impl SyncProvider,
    progress: &mut SyncProgress,
    repository: &RepositoryId,
    receipt: &AutoJoinSkipReceipt,
) -> Result<(), AppError> {
    let marker = receipt.marker();
    let body = receipt.comment_body();
    if body.len() > MAX_AUTO_ADMISSION_COMMENT_BYTES {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "auto_admission_skip_receipt_too_large",
            "the exact generation-bound skip receipt exceeds GitHub's bounded comment budget",
            Some(json!({
                "candidate_pr": receipt.candidate_pr,
                "evidence_hash": receipt.evidence_hash,
                "comment_bytes": body.len(),
                "max_comment_bytes": MAX_AUTO_ADMISSION_COMMENT_BYTES,
                "mutated": false,
                "next": "reduce the active caravan/tail surface or admit/repair the candidate manually; Cara will not truncate skip authority",
            })),
        ));
    }
    let candidate = progress
        .current
        .get(&receipt.candidate_pr)
        .cloned()
        .ok_or_else(|| {
            AppError::validation(
                "auto_admission_candidate_missing",
                format!(
                    "candidate #{} disappeared before skip",
                    receipt.candidate_pr
                ),
            )
        })?;
    if !candidate.has_label(AUTO_ADMISSION_SKIP_LABEL) {
        let labelled = provider
            .add_label(
                repository,
                &PullRequestPrecondition::from(&candidate),
                AUTO_ADMISSION_SKIP_LABEL,
            )
            .map_err(|error| mutation_error(&error, progress, Some(receipt.candidate_pr)))?;
        progress.record(labelled, "added generation-bound automatic admission skip");
    }
    let comment = provider
        .ensure_marked_comment(
            repository,
            &progress.precondition(receipt.candidate_pr),
            &marker,
            &body,
        )
        .map_err(|error| mutation_error(&error, progress, Some(receipt.candidate_pr)))?;
    record_marked_comment(
        progress,
        comment,
        receipt.candidate_pr,
        "posted durable automatic admission skip receipt",
    );
    Ok(())
}

fn record_marked_comment(
    progress: &mut SyncProgress,
    receipt: GitHubMutationReceipt,
    pr: PrNumber,
    summary: &str,
) {
    let already = receipt
        .provider_output
        .as_deref()
        .is_some_and(|output| output.starts_with("existing GitHub comment"));
    if already {
        progress.already(MutationKind::Comment, pr, "marked comment already present");
        progress.current.insert(pr, receipt.after);
    } else {
        progress.record(receipt, summary);
    }
}

fn attach_lock_recovery(
    error: AppError,
    lock_recovery: Option<&OperationLockRecovery>,
) -> AppError {
    let Some(lock_recovery) = lock_recovery else {
        return error;
    };
    let mut details = error.details().unwrap_or_else(|| json!({}));
    if let Some(object) = details.as_object_mut() {
        object.insert("lock_recovery".to_owned(), json!(lock_recovery));
        object.insert(
            "lock_recovery_next".to_owned(),
            json!("provider state was rediscovered after exact dead-owner cleanup; rerun the same idempotent command after repairing this error"),
        );
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

#[cfg(test)]
fn execute(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    all: bool,
    rerun_failed: bool,
    force_merge: bool,
) -> Result<SyncProgress, AppError> {
    execute_bounded(status, provider, all, rerun_failed, force_merge, u32::MAX)
}

fn execute_bounded(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    all: bool,
    rerun_failed: bool,
    force_merge: bool,
    mutation_limit: u32,
) -> Result<SyncProgress, AppError> {
    let mut caravans = select_caravans(status, all)?;
    let paused_caravans = status
        .pauses
        .iter()
        .filter(|pause| {
            pause.state != crate::pause::PauseState::Stale
                && caravans
                    .iter()
                    .any(|caravan| caravan.id == pause.record.caravan_head)
        })
        .cloned()
        .collect::<Vec<_>>();
    caravans.retain(|caravan| {
        !paused_caravans
            .iter()
            .any(|pause| pause.record.caravan_head == caravan.id)
    });
    let synchronized_caravans = caravans.iter().map(|caravan| caravan.id).collect();
    let mut progress = SyncProgress::new(status, synchronized_caravans, mutation_limit);
    progress.paused_caravans = paused_caravans;
    for pause in &progress.paused_caravans {
        progress.steps.push(MutationStep {
            kind: MutationKind::DisableAutoMerge,
            state: MutationStepState::AlreadySatisfied,
            pr: Some(pause.record.caravan_head),
            summary: format!("caravan #{} intentionally paused ({:?}); no mutation; after recovery explicitly run `cara resume --head-pr {} --actor <actor>`", pause.record.caravan_head, pause.state, pause.record.caravan_head),
        });
    }
    if caravans.is_empty() {
        return Ok(progress);
    }

    preflight_repository(provider, status, &progress)?;
    validate_graph(status, &caravans, &progress)?;

    for caravan in &caravans {
        reconcile_caravan(
            status,
            provider,
            caravan,
            rerun_failed,
            force_merge,
            &mut progress,
        )?;
    }

    Ok(progress)
}

fn reconcile_caravan(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    caravan: &Caravan,
    rerun_failed: bool,
    force_merge: bool,
    progress: &mut SyncProgress,
) -> Result<(), AppError> {
    let head = caravan.head().expect("caravans are non-empty");
    if let Some(predecessor) = merged_predecessor(status, caravan) {
        progress.ensure_base(provider, &status.repository, head, &status.default_branch)?;
        progress.record_head_advancement(predecessor.number, head, status);
    } else {
        progress.ensure_base(provider, &status.repository, head, &status.default_branch)?;
    }

    let mut forced_head = false;
    for number in caravan.members.iter().copied() {
        let observation = progress.observe_ci(provider, &status.repository, number)?;
        let disposition = observation.disposition;
        progress.ci.push(observation.clone());
        if disposition == CiDisposition::Failed {
            if rerun_failed {
                progress.rerun_exact_failed_runs(
                    provider,
                    &status.repository,
                    number,
                    &observation.rerunnable_run_ids,
                )?;
            }
            return Err(ci_decision_error(status, caravan, &observation, progress));
        }
        forced_head |= number == head && disposition == CiDisposition::Forced;
    }

    if forced_head {
        return force_merge_head(status, provider, caravan, force_merge, progress);
    }

    // Repair externally enabled non-heads before enabling the head so sync
    // never creates a transient two-auto-merge window.
    for number in caravan.members.iter().skip(1).copied() {
        progress.ensure_auto_merge_disabled(provider, &status.repository, number)?;
    }
    progress.ensure_squash_auto_merge(provider, &status.repository, head)
}

#[allow(clippy::too_many_lines)]
fn force_merge_head(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    caravan: &Caravan,
    force_merge: bool,
    progress: &mut SyncProgress,
) -> Result<(), AppError> {
    let head = caravan.head().expect("caravan head");
    let current = progress
        .current
        .get(&head)
        .expect("current head facts")
        .clone();
    if !force_merge {
        return Err(force_merge_denied(
            status,
            caravan,
            progress,
            "repository policy has force_merge=false",
            None,
        ));
    }
    if current.state != PullRequestState::Open
        || current.draft
        || !current.has_label("caravan-force")
    {
        return Err(force_merge_denied(
            status,
            caravan,
            progress,
            "forced head must remain open, non-draft, and labelled caravan-force",
            None,
        ));
    }
    if !head_is_conflict_free_with_default(status, &current) {
        return Err(force_merge_denied(
            status,
            caravan,
            progress,
            "forced head is not proven conflict-free with the exact default-branch revision",
            None,
        ));
    }

    provider
        .verify_branch_head(
            &status.repository,
            &status.default_branch,
            &status.analysis.fleet.default_branch.oid,
        )
        .map_err(|error| mutation_error(&error, progress, Some(head)))?;

    progress.events.push(progress.event(
        EventKind::ForceMergeAttempted,
        Some(caravan.id),
        vec![head],
        Some("non-successful CI state accepted by explicit caravan-force policy".to_owned()),
        force_event_metadata(progress, head),
    ));

    let permission = provider
        .viewer_permission(&status.repository)
        .map_err(|error| mutation_error(&error, progress, Some(head)))?;
    if permission != "ADMIN" {
        let error = MutationError::PermissionDenied {
            required: "ADMIN".to_owned(),
            actual: permission,
        };
        return Err(force_merge_denied(
            status,
            caravan,
            progress,
            "authenticated actor cannot perform the required administrator squash merge",
            Some(&error),
        ));
    }

    let mut before_labels = current.labels.clone();
    before_labels.remove("caravan-force");
    let observation = progress
        .ci
        .iter()
        .find(|item| item.pr == head)
        .expect("forced head has CI observation");
    let compatibility = status
        .analysis
        .compatibility
        .iter()
        .find(|report| {
            report.candidate == current.head
                && report.target == status.analysis.fleet.default_branch
        })
        .expect("force policy requires exact compatibility proof");
    let audit = ControlLabelAudit {
        operation: "force_accept".to_owned(),
        marker: control_label_marker(
            "force_accept",
            head,
            &current.head.oid,
            &before_labels,
            &current.labels,
        ),
        before_labels,
        after_labels: current.labels.clone(),
        actor: "authenticated GitHub comment author; cara sync/loop force policy".to_owned(),
        reason: format!(
            "observed external `caravan-force`; force_merge=true; ADMIN permission confirmed; observed checks (including pending, running, failed, or empty): {}",
            serde_json::to_string(&observation.checks).expect("checks serialize")
        ),
        reason_source: "deterministic evidence from GitHub checks, external label, repository config, and permission preflight".to_owned(),
        compatibility_evidence: format!(
            "{}@{} -> {}@{} = {:?}",
            compatibility.candidate.name,
            compatibility.candidate.oid.0,
            compatibility.target.name,
            compatibility.target.oid.0,
            compatibility.outcome,
        ),
        clean_squash_evidence: "exact head/default compatibility is clean; ADMIN squash merge is the configured force action".to_owned(),
        admission_priority_basis: "not applicable: force acceptance preserves existing caravan order".to_owned(),
    };
    progress.ensure_control_label_comment(provider, &status.repository, head, &audit)?;

    // Non-heads are repaired before the exceptional merge so force cannot
    // create a transient second auto-merge candidate.
    for number in caravan.members.iter().skip(1).copied() {
        progress.ensure_auto_merge_disabled(provider, &status.repository, number)?;
    }
    progress.ensure_mutation_capacity(1)?;
    let receipt =
        match provider.admin_squash_merge(&status.repository, &progress.precondition(head)) {
            Ok(receipt) => receipt,
            Err(error @ MutationError::PermissionDenied { .. }) => {
                return Err(force_merge_denied(
                    status,
                    caravan,
                    progress,
                    "authenticated actor cannot perform the required administrator squash merge",
                    Some(&error),
                ));
            }
            Err(error) => return Err(mutation_error(&error, progress, Some(head))),
        };
    progress.record(receipt, "administrator squash-merged forced caravan head");
    progress.events.push(progress.event(
        EventKind::ForceMergeCompleted,
        Some(caravan.id),
        vec![head],
        Some("administrator squash merge completed".to_owned()),
        force_event_metadata(progress, head),
    ));

    // At most one exceptional merge is attempted per tick. If a child exists,
    // advance it normally and leave any forced child for the next fresh tick.
    if let Some(new_head) = caravan.members.get(1).copied() {
        progress.ensure_base(
            provider,
            &status.repository,
            new_head,
            &status.default_branch,
        )?;
        progress.record_head_advancement(head, new_head, status);
        for number in caravan.members.iter().skip(2).copied() {
            progress.ensure_auto_merge_disabled(provider, &status.repository, number)?;
        }
        progress.ensure_squash_auto_merge(provider, &status.repository, new_head)?;
    }
    Ok(())
}

fn head_is_conflict_free_with_default(status: &StatusOutput, head: &PullRequestSnapshot) -> bool {
    status.analysis.compatibility.iter().any(|report| {
        report.candidate == head.head
            && report.target == status.analysis.fleet.default_branch
            && report.outcome == CompatibilityOutcome::Clean
    })
}

const LOCK_CHECKPOINT_SAMPLE_LIMIT: usize = 4;

fn checkpoint_hash(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).expect("checkpoint evidence serializes");
    crate::membership::fnv1a64(&bytes)
}

fn bounded_checkpoint_sequence(values: Vec<Value>) -> Value {
    let count = values.len();
    let hash = checkpoint_hash(&values);
    let sample = if count <= LOCK_CHECKPOINT_SAMPLE_LIMIT {
        values
    } else {
        values
            .iter()
            .take(LOCK_CHECKPOINT_SAMPLE_LIMIT / 2)
            .chain(values.iter().skip(count - LOCK_CHECKPOINT_SAMPLE_LIMIT / 2))
            .cloned()
            .collect()
    };
    json!({
        "count": count,
        "hash": hash,
        "sample": sample,
        "truncated": count.saturating_sub(LOCK_CHECKPOINT_SAMPLE_LIMIT),
        "sample_policy": "first_two_last_two",
    })
}

fn compact_precondition(precondition: &PullRequestPrecondition) -> Value {
    json!({
        "number": precondition.number,
        "state": precondition.state,
        "head_oid": precondition.head_oid,
        "base_ref": precondition.base_ref,
        "base_oid": precondition.base_oid,
        "labels": {
            "count": precondition.labels.len(),
            "hash": checkpoint_hash(&precondition.labels),
        },
        "checks": {
            "count": precondition.checks.len(),
            "hash": checkpoint_hash(&precondition.checks),
        },
        "auto_merge": precondition.auto_merge,
    })
}

fn checkpoint_rebase_plans(plans: &[crate::physical_rebase::RebasePlan]) -> Value {
    bounded_checkpoint_sequence(
        plans
            .iter()
            .map(|plan| {
                json!({
                    "pr": plan.pr,
                    "branch": plan.branch,
                    "old_head_oid": plan.old_head_oid,
                    "old_base_oid": plan.old_base_oid,
                    "range_source": plan.range_source,
                    "new_base": plan.new_base,
                    "new_head_oid": plan.new_head_oid,
                    "new_tree_oid": plan.new_tree_oid,
                    "commit_count": plan.commit_count,
                    "ci_trigger_workflow_count": plan.ci_trigger_workflows.len(),
                    "ci_trigger_workflows_hash": checkpoint_hash(&plan.ci_trigger_workflows),
                    "lease": plan.lease,
                    "already_satisfied": plan.already_satisfied,
                })
            })
            .collect(),
    )
}

fn checkpoint_rebase_receipts(receipts: &[crate::physical_rebase::RebaseReceipt]) -> Value {
    bounded_checkpoint_sequence(
        receipts
            .iter()
            .map(|receipt| {
                json!({
                    "pr": receipt.pr,
                    "branch": receipt.branch,
                    "old_head_oid": receipt.old_head_oid,
                    "new_head_oid": receipt.new_head_oid,
                    "old_base_oid": receipt.old_base_oid,
                    "new_base_branch": receipt.new_base_branch,
                    "new_base_oid": receipt.new_base_oid,
                    "new_tree_oid": receipt.new_tree_oid,
                    "commit_count": receipt.commit_count,
                    "lease": receipt.lease,
                    "already_satisfied": receipt.already_satisfied,
                })
            })
            .collect(),
    )
}

fn checkpoint_provider_receipts(receipts: &[GitHubMutationReceipt]) -> Value {
    bounded_checkpoint_sequence(
        receipts
            .iter()
            .map(|receipt| {
                json!({
                    "kind": receipt.kind,
                    "before": receipt
                        .before
                        .as_ref()
                        .map(PullRequestPrecondition::from)
                        .as_ref()
                        .map(compact_precondition),
                    "after": compact_precondition(&PullRequestPrecondition::from(&receipt.after)),
                })
            })
            .collect(),
    )
}

fn checkpoint_steps(steps: &[MutationStep]) -> Value {
    bounded_checkpoint_sequence(
        steps
            .iter()
            .map(|step| {
                json!({
                    "kind": step.kind,
                    "state": step.state,
                    "pr": step.pr,
                    "summary_hash": checkpoint_hash(&step.summary),
                })
            })
            .collect(),
    )
}

fn checkpoint_events(events: &[CaravanEvent]) -> Value {
    bounded_checkpoint_sequence(
        events
            .iter()
            .map(|event| {
                json!({
                    "event_id": event.event_id,
                    "kind": event.kind,
                    "caravan_id": event.caravan_id,
                    "prs": bounded_checkpoint_sequence(
                        event.prs.iter().map(|pr| json!(pr)).collect()
                    ),
                })
            })
            .collect(),
    )
}

fn checkpoint_auto_admission(output: &AutoAdmissionOutput) -> Value {
    json!({
        "enabled": output.enabled,
        "heuristic_version": output.heuristic_version,
        "continuation": output.continuation,
        "candidates_considered": output.candidates_considered,
        "mutations_used": output.mutations_used,
        "mutation_limit": output.mutation_limit,
        "github_requests_used": output.github_requests_used,
        "github_request_limit": output.github_request_limit,
        "joins": bounded_checkpoint_sequence(output.joins.iter().map(|join| json!({
            "candidate_pr": join.candidate_pr,
            "target_tail": join.target_tail,
            "operation_id": join.membership.receipt.operation_id,
            "changed": join.membership.receipt.changed,
            "join_receipt_hash": join.membership.join_receipt.as_ref().map(|receipt| &receipt.receipt_hash),
        })).collect()),
        "skips": bounded_checkpoint_sequence(output.skips.iter().map(|skip| json!({
            "candidate_pr": skip.candidate_pr,
            "evidence_hash": skip.evidence_hash,
        })).collect()),
        "remaining_candidates": bounded_checkpoint_sequence(
            output.remaining_candidates.iter().map(|pr| json!(pr)).collect()
        ),
    })
}

fn sync_checkpoint_evidence(progress: &SyncProgress) -> Value {
    let affected_prs = progress
        .steps
        .iter()
        .filter_map(|step| step.pr)
        .chain(progress.rebase_plans.iter().map(|plan| plan.pr))
        .chain(progress.rebase_receipts.iter().map(|receipt| receipt.pr))
        .collect::<BTreeSet<_>>();
    json!({
        "schema_version": 2,
        "operation": {
            "operation_id": progress.operation_id,
            "name": "sync",
            "changed": progress.operation_receipt().changed,
        },
        "affected_prs": bounded_checkpoint_sequence(
            affected_prs.into_iter().map(|pr| json!(pr)).collect()
        ),
        "steps": checkpoint_steps(&progress.steps),
        "rebase_plans": checkpoint_rebase_plans(&progress.rebase_plans),
        "rebase_receipts": checkpoint_rebase_receipts(&progress.rebase_receipts),
        "provider_receipts": checkpoint_provider_receipts(&progress.provider_receipts),
        "events": checkpoint_events(&progress.events),
        "recovery": "rediscover provider state and replay the same idempotent sync; hashes bind complete omitted evidence",
    })
}

fn force_event_metadata(progress: &SyncProgress, head: PrNumber) -> BTreeMap<String, Value> {
    let mut metadata = BTreeMap::new();
    metadata.insert("head".to_owned(), json!(progress.current.get(&head)));
    metadata.insert(
        "ci".to_owned(),
        json!(progress.ci.iter().find(|item| item.pr == head)),
    );
    metadata.insert(
        "operation_receipt".to_owned(),
        json!(progress.operation_receipt()),
    );
    metadata
}

fn force_merge_denied(
    status: &StatusOutput,
    caravan: &Caravan,
    progress: &SyncProgress,
    message: &str,
    error: Option<&MutationError>,
) -> AppError {
    let head = caravan.head().expect("caravan head");
    let mut evidence = BTreeMap::new();
    evidence.insert(
        "default_branch".to_owned(),
        json!(status.analysis.fleet.default_branch),
    );
    evidence.insert("head".to_owned(), json!(progress.current.get(&head)));
    evidence.insert(
        "ci".to_owned(),
        json!(progress.ci.iter().find(|item| item.pr == head)),
    );
    evidence.insert("events".to_owned(), json!(progress.events));
    if let Some(error) = error {
        evidence.insert("provider_error".to_owned(), json!(format!("{error:?}")));
        if let MutationError::PermissionDenied { required, actual } = error {
            evidence.insert(
                "permission".to_owned(),
                json!({"required": required, "actual": actual}),
            );
        }
    }
    let decision = DecisionPoint {
        kind: DecisionKind::ForceMergeDenied,
        operation_id: progress.operation_id.clone(),
        repository: status.repository.clone(),
        caravan_id: Some(caravan.id),
        affected_prs: vec![head],
        message: message.to_owned(),
        evidence,
        completed_steps: progress.steps.clone(),
        resumable: true,
        suggested_actions: vec![
            "repair CI and remove caravan-force, or satisfy the complete force-merge policy"
                .to_owned(),
            "rerun the same `cara sync` command after GitHub facts change".to_owned(),
        ],
    };
    decision_error(&decision, progress)
}

fn ci_decision_error(
    status: &StatusOutput,
    caravan: &Caravan,
    observation: &CiObservation,
    progress: &mut SyncProgress,
) -> AppError {
    let event = progress.event(
        EventKind::CiFailed,
        Some(caravan.id),
        vec![observation.pr],
        Some("unforced terminal or unknown CI state requires a decision".to_owned()),
        BTreeMap::from([
            ("ci".to_owned(), json!(observation)),
            (
                "pull_request".to_owned(),
                json!(progress.current.get(&observation.pr)),
            ),
        ]),
    );
    progress.events.push(event.clone());
    let mut evidence = BTreeMap::new();
    evidence.insert(
        "default_branch".to_owned(),
        json!(status.analysis.fleet.default_branch),
    );
    evidence.insert("fleet".to_owned(), json!(status.analysis.fleet));
    evidence.insert("ci".to_owned(), json!(observation));
    evidence.insert(
        "pull_request".to_owned(),
        json!(progress.current.get(&observation.pr)),
    );
    evidence.insert("event".to_owned(), json!(event));
    let mut suggested_actions = vec![
        "repair and push the affected PR, then rerun `cara sync`".to_owned(),
        format!(
            "run `cara evict --pr {} --reason <text>` or split the caravan",
            observation.pr
        ),
        "apply caravan-force only for a known acceptable failure".to_owned(),
    ];
    if observation
        .failure_diagnostics
        .iter()
        .any(|diagnostic| diagnostic.action == WorkflowFailureAction::FreshCandidateTrigger)
    {
        suggested_actions.insert(
            0,
            "trigger a fresh exact-candidate workflow; do not rerun the listed stale generation"
                .to_owned(),
        );
    } else if !observation.rerunnable_run_ids.is_empty() {
        suggested_actions.insert(
            0,
            "rerun only the listed current-generation infrastructure runs with `cara sync --rerun-failed`"
                .to_owned(),
        );
    }
    if observation
        .failure_diagnostics
        .iter()
        .any(|diagnostic| diagnostic.action == WorkflowFailureAction::WaitOrInspect)
    {
        suggested_actions.insert(
            0,
            "wait for provider evidence or inspect the bounded run/job/step receipts before retrying"
                .to_owned(),
        );
    }
    let decision = DecisionPoint {
        kind: DecisionKind::CiFailure,
        operation_id: progress.operation_id.clone(),
        repository: status.repository.clone(),
        caravan_id: Some(caravan.id),
        affected_prs: vec![observation.pr],
        message: format!("PR #{} has unresolved CI failure", observation.pr),
        evidence,
        completed_steps: progress.steps.clone(),
        resumable: true,
        suggested_actions,
    };
    decision_error(&decision, progress)
}

fn classify_workflow_failure(
    diagnostic: WorkflowRunFailureDiagnostic,
    candidate: Option<&MergeCandidateIdentity>,
) -> ClassifiedWorkflowRunFailure {
    let (mut generation, mut reasons) = workflow_run_generation(&diagnostic);
    let mut candidate_lineage_unknown = false;
    if let Some(candidate) = candidate {
        if candidate.pr != diagnostic.expected_pr {
            generation = WorkflowRunGeneration::MissingAssociation;
            reasons.push(format!(
                "candidate identity belongs to PR {}, expected {}",
                candidate.pr, diagnostic.expected_pr
            ));
        } else if candidate.stale_head || candidate.stale_base {
            generation = match (candidate.stale_head, candidate.stale_base) {
                (true, true) => WorkflowRunGeneration::StaleHeadAndBase,
                (true, false) => WorkflowRunGeneration::StaleHead,
                (false, true) => WorkflowRunGeneration::StaleBase,
                (false, false) => WorkflowRunGeneration::Current,
            };
            reasons.extend(
                candidate
                    .stale_reasons
                    .iter()
                    .map(|reason| format!("current synthetic candidate is stale: {reason}")),
            );
        } else if matches!(
            candidate.freshness,
            crate::model::MergeCandidateFreshness::Missing
                | crate::model::MergeCandidateFreshness::Unknown
        ) {
            candidate_lineage_unknown = true;
            reasons.push("current synthetic candidate lineage is missing or unknown".to_owned());
        }
    }

    let failed_lineage_step = diagnostic.failed_jobs.iter().any(|job| {
        job.failed_steps.iter().any(|step| {
            let name = step.name.to_ascii_lowercase();
            name.contains("lineage") || name.contains("selected ref")
        })
    });
    let lineage_receipt_present =
        apply_selected_lineage_receipt(&diagnostic, &mut generation, &mut reasons);
    let (classification, action) = if generation != WorkflowRunGeneration::Current {
        (
            WorkflowFailureClass::StaleGeneration,
            WorkflowFailureAction::FreshCandidateTrigger,
        )
    } else if failed_lineage_step && !lineage_receipt_present {
        let cause = if candidate_lineage_unknown {
            "current candidate identity and bounded lineage receipt are unavailable"
        } else {
            "bounded lineage receipt is unavailable"
        };
        reasons.push(format!("lineage verification failed while {cause}"));
        (
            WorkflowFailureClass::Unknown,
            WorkflowFailureAction::FreshCandidateTrigger,
        )
    } else if diagnostic.conclusion.eq_ignore_ascii_case("cancelled") {
        reasons.push("current-generation run was cancelled".to_owned());
        (
            WorkflowFailureClass::Cancelled,
            WorkflowFailureAction::FreshCandidateTrigger,
        )
    } else if retryable_infrastructure(&diagnostic) {
        reasons.push("structured job conclusion indicates retryable infrastructure".to_owned());
        (
            WorkflowFailureClass::RetryableInfrastructure,
            WorkflowFailureAction::RerunFailedJobs,
        )
    } else if diagnostic.failed_jobs.is_empty() {
        reasons.push("provider exposed no bounded failed job/step evidence".to_owned());
        (
            WorkflowFailureClass::Unknown,
            WorkflowFailureAction::WaitOrInspect,
        )
    } else {
        reasons.push("current-generation jobs contain source or test failures".to_owned());
        (
            WorkflowFailureClass::SourceOrTestFailure,
            WorkflowFailureAction::RepairSource,
        )
    };
    ClassifiedWorkflowRunFailure {
        diagnostic,
        generation,
        classification,
        action,
        reasons,
    }
}

fn apply_selected_lineage_receipt(
    diagnostic: &WorkflowRunFailureDiagnostic,
    generation: &mut WorkflowRunGeneration,
    reasons: &mut Vec<String>,
) -> bool {
    let Some(receipt) = diagnostic
        .failed_jobs
        .iter()
        .find_map(|job| job.selected_lineage.as_ref())
    else {
        return false;
    };
    let stale_base = receipt.parents.first() != Some(&diagnostic.expected_base_oid)
        || receipt.expected_base != diagnostic.expected_base_oid;
    let stale_head = receipt.parents.get(1) != Some(&diagnostic.expected_head_oid)
        || receipt.expected_head != diagnostic.expected_head_oid;
    if stale_base || stale_head {
        *generation = match (stale_head, stale_base) {
            (true, true) => WorkflowRunGeneration::StaleHeadAndBase,
            (true, false) => WorkflowRunGeneration::StaleHead,
            (false, true) => WorkflowRunGeneration::StaleBase,
            (false, false) => WorkflowRunGeneration::Current,
        };
    }
    reasons.push(format!(
        "bounded lineage receipt selected ref {} commit {} with parents [{}]",
        receipt.selected_ref,
        receipt.selected_commit,
        receipt
            .parents
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    ));
    true
}

fn workflow_run_generation(
    diagnostic: &WorkflowRunFailureDiagnostic,
) -> (WorkflowRunGeneration, Vec<String>) {
    let Some(association) = diagnostic
        .pull_requests
        .iter()
        .find(|association| association.pr == diagnostic.expected_pr)
    else {
        return (
            WorkflowRunGeneration::MissingAssociation,
            vec!["run has no immutable association for the expected PR".to_owned()],
        );
    };
    let stale_head = association
        .head_oid
        .as_ref()
        .is_some_and(|oid| *oid != diagnostic.expected_head_oid);
    let stale_base = association
        .base_oid
        .as_ref()
        .is_some_and(|oid| *oid != diagnostic.expected_base_oid);
    let generation = match (stale_head, stale_base) {
        (true, true) => WorkflowRunGeneration::StaleHeadAndBase,
        (true, false) => WorkflowRunGeneration::StaleHead,
        (false, true) => WorkflowRunGeneration::StaleBase,
        (false, false) => WorkflowRunGeneration::Current,
    };
    let mut reasons = Vec::new();
    if stale_head {
        reasons.push(format!(
            "run PR head {} does not match current head {}",
            association
                .head_oid
                .as_ref()
                .map_or("missing", |oid| oid.0.as_str()),
            diagnostic.expected_head_oid
        ));
    }
    if stale_base {
        reasons.push(format!(
            "run PR base {} does not match current base {}",
            association
                .base_oid
                .as_ref()
                .map_or("missing", |oid| oid.0.as_str()),
            diagnostic.expected_base_oid
        ));
    }
    (generation, reasons)
}

fn retryable_infrastructure(diagnostic: &WorkflowRunFailureDiagnostic) -> bool {
    const INFRA_CONCLUSIONS: [&str; 4] =
        ["timed_out", "startup_failure", "stale", "action_required"];
    let conclusion_is_infra = |value: &str| {
        INFRA_CONCLUSIONS
            .iter()
            .any(|candidate| value.eq_ignore_ascii_case(candidate))
    };
    conclusion_is_infra(&diagnostic.conclusion)
        || diagnostic
            .failed_jobs
            .iter()
            .any(|job| conclusion_is_infra(&job.conclusion))
}

fn classify_checks(checks: &[CheckSnapshot], forced: bool) -> CiDisposition {
    let fully_successful = !checks.is_empty()
        && checks.iter().all(|check| {
            matches!(
                check.state,
                CheckState::Success | CheckState::Neutral | CheckState::Skipped
            )
        });
    if forced && !fully_successful {
        return CiDisposition::Forced;
    }

    let pending = checks.is_empty()
        || checks.iter().any(|check| {
            matches!(
                check.state,
                CheckState::Expected | CheckState::Queued | CheckState::InProgress
            )
        });
    let failed = checks.iter().any(|check| {
        matches!(
            check.state,
            CheckState::Failure
                | CheckState::Cancelled
                | CheckState::TimedOut
                | CheckState::ActionRequired
                | CheckState::Unknown
        )
    });
    if pending {
        CiDisposition::Waiting
    } else if failed {
        CiDisposition::Failed
    } else {
        CiDisposition::Passing
    }
}

fn check_is_failure(check: &CheckSnapshot) -> bool {
    matches!(
        check.state,
        CheckState::Failure
            | CheckState::Cancelled
            | CheckState::TimedOut
            | CheckState::ActionRequired
            | CheckState::Unknown
    )
}

fn workflow_run_id(url: &str) -> Option<u64> {
    let suffix = url.split_once("/actions/runs/")?.1;
    suffix.split('/').next()?.parse().ok()
}

fn select_rerunnable_run_ids(checks: &[CheckSnapshot], runs: &[WorkflowRunSnapshot]) -> Vec<u64> {
    let failed_run_ids = runs
        .iter()
        .map(|run| run.database_id)
        .collect::<BTreeSet<_>>();
    checks
        .iter()
        .filter(|check| check_is_failure(check))
        .filter_map(|check| check.details_url.as_deref().and_then(workflow_run_id))
        .filter(|run_id| failed_run_ids.contains(run_id))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn event_timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
        .to_string()
}

fn select_caravans(status: &StatusOutput, all: bool) -> Result<Vec<Caravan>, AppError> {
    let mut caravans = if all {
        status.analysis.fleet.caravans.clone()
    } else {
        let current = status.current_pr.ok_or_else(|| {
            if let Some(predecessor) = read::historical_predecessor(status) {
                AppError::structured(
                    ErrorCategory::TargetNotFound,
                    "historical_successor_not_found",
                    format!(
                        "merged Caravan PR #{predecessor} has no unique active rolling successor"
                    ),
                    Some(json!({
                        "historical_predecessor": predecessor,
                        "current_branch": status.current_branch,
                        "fail_closed": true,
                    })),
                )
            } else {
                AppError::validation(
                    "current_pr_not_found",
                    "the current branch has no unique open PR; use `cara sync --all`",
                )
            }
        })?;
        vec![
            status
                .analysis
                .fleet
                .containing(current)
                .cloned()
                .ok_or_else(|| {
                    AppError::validation(
                        "current_pr_not_in_caravan",
                        format!("PR #{current} is not an active caravan member"),
                    )
                })?,
        ]
    };
    caravans.sort_by_key(|caravan| caravan.id);
    Ok(caravans)
}

fn preflight_repository(
    provider: &impl SyncProvider,
    status: &StatusOutput,
    progress: &SyncProgress,
) -> Result<(), AppError> {
    let allows_auto_merge = provider
        .repository_allows_auto_merge(&status.repository)
        .map_err(|error| mutation_error(&error, progress, None))?;
    if !allows_auto_merge {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "auto_merge_not_enabled",
            "repository settings must allow squash auto-merge before synchronization",
            Some(json!({
                "repository": status.repository,
                "resumable": true,
                "next": "enable repository auto-merge and squash merge, then rerun `cara sync`",
            })),
        ));
    }
    let protected = provider
        .branch_is_protected(&status.repository, &status.default_branch)
        .map_err(|error| mutation_error(&error, progress, None))?;
    if !protected {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "default_branch_not_protected",
            "the default branch must have a protection requirement before synchronization",
            Some(json!({
                "repository": status.repository,
                "default_branch": status.default_branch,
                "resumable": true,
                "next": "configure a required status check or review, then rerun `cara sync`",
            })),
        ));
    }
    Ok(())
}

fn validate_graph(
    status: &StatusOutput,
    selected: &[Caravan],
    progress: &SyncProgress,
) -> Result<(), AppError> {
    for problem in &status.analysis.fleet.problems {
        let correctable_auto_merge = problem.kind == GraphProblemKind::AutoMergeInvariant
            && problem.prs.iter().all(|number| {
                selected
                    .iter()
                    .any(|caravan| caravan.members.contains(number))
            });
        let correctable_advancement = problem.kind == GraphProblemKind::DanglingBase
            && recoverable_dangling_problem(status, selected, problem);
        if correctable_auto_merge || correctable_advancement {
            continue;
        }
        return Err(decision_error(
            &decision_for_problem(problem, status, progress),
            progress,
        ));
    }
    Ok(())
}

fn recoverable_dangling_problem(
    status: &StatusOutput,
    selected: &[Caravan],
    problem: &GraphProblem,
) -> bool {
    let [child, predecessor] = problem.prs.as_slice() else {
        return false;
    };
    if !selected
        .iter()
        .any(|caravan| caravan.head() == Some(*child))
    {
        return false;
    }
    let (Some(child), Some(predecessor)) = (
        status.analysis.pull_requests.get(child),
        status.analysis.pull_requests.get(predecessor),
    ) else {
        return false;
    };
    let matching_predecessors = status
        .analysis
        .pull_requests
        .values()
        .filter(|candidate| {
            candidate.state == PullRequestState::Merged
                && candidate.has_label("caravan")
                && candidate.head.name == child.base.name
        })
        .count();
    predecessor.state == PullRequestState::Merged
        && predecessor.has_label("caravan")
        && child.base.name == predecessor.head.name
        && matching_predecessors == 1
}

fn merged_predecessor<'a>(
    status: &'a StatusOutput,
    caravan: &Caravan,
) -> Option<&'a PullRequestSnapshot> {
    let head = status
        .analysis
        .pull_requests
        .get(&caravan.head().expect("caravan head"))?;
    if head.base.name == status.default_branch {
        return None;
    }
    let mut matches = status.analysis.pull_requests.values().filter(|candidate| {
        candidate.state == PullRequestState::Merged
            && candidate.has_label("caravan")
            && candidate.head.name == head.base.name
    });
    let predecessor = matches.next()?;
    matches.next().is_none().then_some(predecessor)
}

fn decision_for_problem(
    problem: &GraphProblem,
    status: &StatusOutput,
    progress: &SyncProgress,
) -> DecisionPoint {
    let kind = match problem.kind {
        GraphProblemKind::Incompatible if problem.prs.len() == 1 => DecisionKind::HeadConflict,
        GraphProblemKind::Incompatible if is_adjacent_pair(status, &problem.prs) => {
            DecisionKind::LinkConflict
        }
        GraphProblemKind::Incompatible => DecisionKind::CrossCaravanConflict,
        _ => DecisionKind::InvalidGraph,
    };
    let caravan_id = problem.prs.iter().find_map(|number| {
        status
            .analysis
            .fleet
            .containing(*number)
            .map(|caravan| caravan.id)
    });
    let mut evidence = BTreeMap::new();
    evidence.insert("problem".to_owned(), json!(problem));
    evidence.insert(
        "default_branch".to_owned(),
        json!(status.analysis.fleet.default_branch),
    );
    evidence.insert("fleet".to_owned(), json!(status.analysis.fleet));
    evidence.insert(
        "pull_requests".to_owned(),
        json!(
            problem
                .prs
                .iter()
                .filter_map(|number| status.analysis.pull_requests.get(number))
                .collect::<Vec<_>>()
        ),
    );
    evidence.insert(
        "compatibility".to_owned(),
        json!(
            status
                .analysis
                .compatibility
                .iter()
                .filter(|report| report.outcome != CompatibilityOutcome::Clean)
                .collect::<Vec<_>>()
        ),
    );
    evidence.insert("events".to_owned(), json!(progress.events));
    evidence.insert("rebase_on_join".to_owned(), json!(status.rebase_on_join));
    let message = if matches!(
        kind,
        DecisionKind::HeadConflict | DecisionKind::LinkConflict
    ) && !status.rebase_on_join.enabled
    {
        format!(
            "{}; rebase_on_join=disabled, so Cara will not rewrite the affected branch",
            problem.message
        )
    } else {
        problem.message.clone()
    };
    DecisionPoint {
        kind,
        operation_id: progress.operation_id.clone(),
        repository: status.repository.clone(),
        caravan_id,
        affected_prs: problem.prs.clone(),
        message,
        evidence,
        completed_steps: progress.steps.clone(),
        resumable: true,
        suggested_actions: suggested_actions(kind, problem, &status.rebase_on_join),
    }
}

fn is_adjacent_pair(status: &StatusOutput, prs: &[PrNumber]) -> bool {
    let [first, second] = prs else {
        return false;
    };
    status.analysis.fleet.caravans.iter().any(|caravan| {
        caravan
            .members
            .windows(2)
            .any(|pair| pair == [*first, *second] || pair == [*second, *first])
    })
}

fn suggested_actions(
    kind: DecisionKind,
    problem: &GraphProblem,
    rebase: &crate::read::RebaseOnJoinStatus,
) -> Vec<String> {
    match kind {
        DecisionKind::HeadConflict | DecisionKind::LinkConflict if !rebase.enabled => vec![
            rebase.required_action.clone().unwrap_or_else(|| {
                "set `rebase_on_join: true` in .caravan/config.yaml".to_owned()
            }),
            "after committing the config change, run `cara check` and then rerun `cara sync --all`"
                .to_owned(),
            problem.prs.last().map_or_else(
                || "inspect `cara status --json` before reshaping".to_owned(),
                |number| {
                    format!("if rewriting is not acceptable, run `cara evict --pr {number} --reason <text>` or split the chain")
                },
            ),
        ],
        DecisionKind::HeadConflict | DecisionKind::LinkConflict => vec![
            "inspect the exact rebase preflight conflict and repair the affected PR before rerunning `cara sync --all`"
                .to_owned(),
            problem.prs.last().map_or_else(
                || "inspect `cara status --json` before reshaping".to_owned(),
                |number| {
                    format!("run `cara evict --pr {number} --reason <text>` or split the chain")
                },
            ),
        ],
        DecisionKind::CrossCaravanConflict => vec![
            "repair one affected caravan head or tail and rerun `cara sync --all`".to_owned(),
            "reshape one caravan with `cara split` or `cara evict`".to_owned(),
        ],
        _ => vec![
            "inspect `cara status --json` and repair the reported graph facts".to_owned(),
            "rerun the same `cara sync` command after the graph is valid".to_owned(),
        ],
    }
}

fn decision_error(decision: &DecisionPoint, progress: &SyncProgress) -> AppError {
    let code = match decision.kind {
        DecisionKind::HeadConflict => "head_conflict",
        DecisionKind::LinkConflict => "link_conflict",
        DecisionKind::CrossCaravanConflict => "cross_caravan_conflict",
        DecisionKind::CiFailure => "ci_failure",
        DecisionKind::InvalidGraph => "invalid_graph",
        DecisionKind::StalePrecondition => "stale_precondition",
        DecisionKind::UnsafeCheckout => "unsafe_checkout",
        DecisionKind::HookFailure => "hook_failure",
        DecisionKind::ForceMergeDenied => "force_merge_denied",
    };
    AppError::structured(
        ErrorCategory::Validation,
        code,
        decision.message.clone(),
        Some(json!({
            "decision": decision,
            "provider_receipts": progress.provider_receipts,
        })),
    )
}

#[derive(Debug)]
struct SyncProgress {
    operation_id: OperationId,
    repository: RepositoryId,
    steps: Vec<MutationStep>,
    provider_receipts: Vec<GitHubMutationReceipt>,
    rebase_plans: Vec<crate::physical_rebase::RebasePlan>,
    rebase_receipts: Vec<crate::physical_rebase::RebaseReceipt>,
    synchronized_caravans: Vec<PrNumber>,
    paused_caravans: Vec<crate::pause::PauseStatus>,
    head_advancements: Vec<HeadAdvancement>,
    ci: Vec<CiObservation>,
    events: Vec<CaravanEvent>,
    current: BTreeMap<PrNumber, PullRequestSnapshot>,
    merge_candidates: BTreeMap<PrNumber, MergeCandidateIdentity>,
    mutation_limit: u32,
}

impl SyncProgress {
    fn new(
        status: &StatusOutput,
        synchronized_caravans: Vec<PrNumber>,
        mutation_limit: u32,
    ) -> Self {
        Self {
            operation_id: OperationId::new(),
            repository: status.repository.clone(),
            steps: Vec::new(),
            provider_receipts: Vec::new(),
            rebase_plans: Vec::new(),
            rebase_receipts: Vec::new(),
            synchronized_caravans,
            paused_caravans: Vec::new(),
            head_advancements: Vec::new(),
            ci: Vec::new(),
            events: Vec::new(),
            current: status.analysis.pull_requests.clone(),
            merge_candidates: status
                .merge_candidates
                .iter()
                .cloned()
                .map(|candidate| (candidate.pr, candidate))
                .collect(),
            mutation_limit,
        }
    }

    fn operation_receipt(&self) -> OperationReceipt {
        OperationReceipt {
            operation_id: self.operation_id.clone(),
            operation: "sync".to_owned(),
            changed: self
                .steps
                .iter()
                .any(|step| step.state == MutationStepState::Completed),
            completed_steps: self.steps.clone(),
        }
    }

    fn precondition(&self, number: PrNumber) -> PullRequestPrecondition {
        PullRequestPrecondition::from(
            self.current
                .get(&number)
                .expect("sync member has current PR facts"),
        )
    }

    fn ensure_mutation_capacity(&self, reserve: u32) -> Result<(), AppError> {
        let used = completed_mutation_count(self);
        if used.saturating_add(reserve) <= self.mutation_limit {
            return Ok(());
        }
        Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "sync_mutation_budget_exhausted",
            format!(
                "the sync mutation budget is exhausted ({used}/{} used; {reserve} required)",
                self.mutation_limit
            ),
            Some(json!({
                "used": used,
                "limit": self.mutation_limit,
                "required": reserve,
                "operation_receipt": self.operation_receipt(),
                "provider_receipts": self.provider_receipts,
                "rebase_receipts": self.rebase_receipts,
                "resumable": true,
                "next": "rerun the same bounded sync tick to continue from fresh provider state",
            })),
        ))
    }

    fn record(&mut self, receipt: GitHubMutationReceipt, summary: &str) {
        let number = receipt.after.number;
        self.current.insert(number, receipt.after.clone());
        self.steps.push(MutationStep {
            kind: receipt.kind,
            state: MutationStepState::Completed,
            pr: Some(number),
            summary: summary.to_owned(),
        });
        self.provider_receipts.push(receipt);
    }

    fn already(&mut self, kind: MutationKind, number: PrNumber, summary: &str) {
        self.steps.push(MutationStep {
            kind,
            state: MutationStepState::AlreadySatisfied,
            pr: Some(number),
            summary: summary.to_owned(),
        });
    }

    fn event(
        &self,
        kind: EventKind,
        caravan_id: Option<PrNumber>,
        prs: Vec<PrNumber>,
        reason: Option<String>,
        metadata: BTreeMap<String, Value>,
    ) -> CaravanEvent {
        CaravanEvent {
            version: 1,
            event_id: EventId::new(),
            operation_id: self.operation_id.clone(),
            kind,
            repository: self.repository.clone(),
            caravan_id,
            prs,
            fleet: None,
            reason,
            metadata,
            timestamp: event_timestamp(),
        }
    }

    fn record_head_advancement(
        &mut self,
        predecessor: PrNumber,
        new_head: PrNumber,
        status: &StatusOutput,
    ) {
        self.head_advancements.push(HeadAdvancement {
            merged_predecessor: predecessor,
            new_head,
            previous_caravan_id: predecessor,
            new_caravan_id: new_head,
        });
        self.events.push(self.event(
            EventKind::HeadAdvanced,
            Some(new_head),
            vec![predecessor, new_head],
            Some("merged predecessor advanced the rolling caravan head".to_owned()),
            BTreeMap::from([
                (
                    "default_branch".to_owned(),
                    json!(status.analysis.fleet.default_branch),
                ),
                (
                    "merged_predecessor".to_owned(),
                    json!(self.current.get(&predecessor)),
                ),
                ("new_head".to_owned(), json!(self.current.get(&new_head))),
            ]),
        ));
    }

    fn observe_ci(
        &self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<CiObservation, AppError> {
        let current = self.current.get(&number).expect("sync member");
        let disposition = classify_checks(&current.checks, current.has_label("caravan-force"));
        let mut failed_runs =
            if matches!(disposition, CiDisposition::Failed | CiDisposition::Forced) {
                provider
                    .failed_runs_for_pull_request(repository, &self.precondition(number))
                    .map_err(|error| mutation_error(&error, self, Some(number)))?
            } else {
                Vec::new()
            };
        failed_runs.sort_by_key(|run| run.database_id);
        failed_runs.dedup_by_key(|run| run.database_id);
        let selected_run_ids = select_rerunnable_run_ids(&current.checks, &failed_runs);
        let failure_diagnostics = if selected_run_ids.is_empty() {
            Vec::new()
        } else {
            provider
                .failed_run_diagnostics(repository, &self.precondition(number), &selected_run_ids)
                .map_err(|error| mutation_error(&error, self, Some(number)))?
                .runs
                .into_iter()
                .map(|diagnostic| {
                    classify_workflow_failure(diagnostic, self.merge_candidates.get(&number))
                })
                .collect::<Vec<_>>()
        };
        let mut rerunnable_run_ids = failure_diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.action == WorkflowFailureAction::RerunFailedJobs)
            .map(|diagnostic| diagnostic.diagnostic.run_id)
            .collect::<Vec<_>>();
        rerunnable_run_ids.sort_unstable();
        rerunnable_run_ids.dedup();
        Ok(CiObservation {
            pr: number,
            disposition,
            checks: current.checks.clone(),
            failed_runs,
            failure_diagnostics,
            rerunnable_run_ids,
        })
    }

    fn rerun_exact_failed_runs(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        number: PrNumber,
        run_ids: &[u64],
    ) -> Result<(), AppError> {
        for run_id in run_ids {
            self.ensure_mutation_capacity(1)?;
            let receipt = provider
                .rerun_failed_run(repository, &self.precondition(number), *run_id)
                .map_err(|error| mutation_error(&error, self, Some(number)))?;
            self.record(
                receipt,
                &format!("reran failed jobs for exact workflow run {run_id}"),
            );
        }
        Ok(())
    }

    fn ensure_control_label_comment(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        number: PrNumber,
        audit: &ControlLabelAudit,
    ) -> Result<(), AppError> {
        self.ensure_mutation_capacity(1)?;
        let receipt = provider
            .ensure_control_label_comment(repository, &self.precondition(number), audit)
            .map_err(|error| comment_mutation_error(&error, self, number))?;
        let already = receipt
            .provider_output
            .as_deref()
            .is_some_and(|output| output.starts_with("existing GitHub comment"));
        if already {
            self.already(
                MutationKind::Comment,
                number,
                "control-label audit comment already present",
            );
            self.current.insert(number, receipt.after);
        } else {
            self.record(receipt, "posted durable force-label audit comment");
        }
        Ok(())
    }

    fn ensure_base(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        number: PrNumber,
        base: &str,
    ) -> Result<(), AppError> {
        if self.current.get(&number).expect("sync member").base.name == base {
            self.already(
                MutationKind::SetBase,
                number,
                "head already targets the default branch",
            );
            return Ok(());
        }
        self.ensure_mutation_capacity(1)?;
        let receipt = provider
            .set_base(repository, &self.precondition(number), base)
            .map_err(|error| mutation_error(&error, self, Some(number)))?;
        self.record(
            receipt,
            "advanced merged predecessor's child to the default branch",
        );
        Ok(())
    }

    fn ensure_auto_merge_disabled(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<(), AppError> {
        if !self
            .current
            .get(&number)
            .expect("sync member")
            .auto_merge
            .enabled
        {
            self.already(
                MutationKind::DisableAutoMerge,
                number,
                "non-head auto-merge already disabled",
            );
            return Ok(());
        }
        self.ensure_mutation_capacity(1)?;
        let receipt = provider
            .disable_auto_merge(repository, &self.precondition(number))
            .map_err(|error| mutation_error(&error, self, Some(number)))?;
        self.record(receipt, "disabled auto-merge on non-head PR");
        Ok(())
    }

    fn ensure_squash_auto_merge(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<(), AppError> {
        let auto_merge = &self.current.get(&number).expect("sync member").auto_merge;
        if auto_merge.enabled && auto_merge.merge_method == Some(MergeMethod::Squash) {
            self.already(
                MutationKind::EnableAutoMerge,
                number,
                "head squash auto-merge already enabled",
            );
            return Ok(());
        }
        if auto_merge.enabled {
            self.ensure_mutation_capacity(1)?;
            let receipt = provider
                .disable_auto_merge(repository, &self.precondition(number))
                .map_err(|error| mutation_error(&error, self, Some(number)))?;
            self.record(receipt, "disabled non-squash auto-merge on head");
        }
        self.ensure_mutation_capacity(1)?;
        let receipt = provider
            .enable_squash_auto_merge(repository, &self.precondition(number))
            .map_err(|error| mutation_error(&error, self, Some(number)))?;
        self.record(receipt, "enabled squash auto-merge on head PR");
        Ok(())
    }
}

fn comment_mutation_error(
    error: &MutationError,
    progress: &SyncProgress,
    affected_pr: PrNumber,
) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "github_comment_failed",
        format!("force label was accepted but its durable GitHub comment failed: {error}"),
        Some(json!({
            "stage": "control_label_comment",
            "affected_pr": affected_pr,
            "operation_receipt": progress.operation_receipt(),
            "provider_receipts": progress.provider_receipts,
            "events": progress.events,
            "resumable": true,
            "dedupe": "deterministic GitHub-visible caravan-control-label-audit marker",
            "next": "rediscover and rerun `cara sync`",
        })),
    )
}

fn mutation_error(
    error: &MutationError,
    progress: &SyncProgress,
    affected_pr: Option<PrNumber>,
) -> AppError {
    if let MutationError::Provider(DiscoveryError::Runner(CommandRunError::Timeout {
        command,
        timeout_ms,
        stdout,
        stderr,
        ..
    })) = error
    {
        return AppError::structured(
            ErrorCategory::Timeout,
            "github_mutation_timeout",
            error.to_string(),
            Some(json!({
                "stage": "github_mutation",
                "command": command.display(),
                "timeout_ms": timeout_ms,
                "stdout": stdout,
                "stderr": stderr,
                "operation_receipt": progress.operation_receipt(),
                "provider_receipts": progress.provider_receipts,
                "events": progress.events,
                "affected_pr": affected_pr,
                "resumable": true,
                "next": "rediscover and rerun the same `cara sync` command",
            })),
        );
    }
    if let MutationError::StalePrecondition {
        expected,
        actual,
        changed_fields,
    } = error
    {
        let mut evidence = BTreeMap::<String, Value>::new();
        evidence.insert("expected".to_owned(), json!(expected));
        evidence.insert("actual".to_owned(), json!(actual));
        evidence.insert("changed_fields".to_owned(), json!(changed_fields));
        evidence.insert("events".to_owned(), json!(progress.events));
        let decision = DecisionPoint {
            kind: DecisionKind::StalePrecondition,
            operation_id: progress.operation_id.clone(),
            repository: progress.repository.clone(),
            caravan_id: progress.synchronized_caravans.first().copied(),
            affected_prs: affected_pr.into_iter().collect(),
            message: error.to_string(),
            evidence,
            completed_steps: progress.steps.clone(),
            resumable: true,
            suggested_actions: vec![
                "rediscover GitHub state and rerun the same `cara sync` command".to_owned(),
            ],
        };
        return decision_error(&decision, progress);
    }
    if let MutationError::BranchHeadMismatch {
        branch,
        expected,
        actual,
    } = error
    {
        let decision = DecisionPoint {
            kind: DecisionKind::StalePrecondition,
            operation_id: progress.operation_id.clone(),
            repository: progress.repository.clone(),
            caravan_id: progress.synchronized_caravans.first().copied(),
            affected_prs: affected_pr.into_iter().collect(),
            message: error.to_string(),
            evidence: BTreeMap::from([
                ("branch".to_owned(), json!(branch)),
                ("expected".to_owned(), json!(expected)),
                ("actual".to_owned(), json!(actual)),
                ("events".to_owned(), json!(progress.events)),
            ]),
            completed_steps: progress.steps.clone(),
            resumable: true,
            suggested_actions: vec![
                "rediscover compatibility against the new default branch and rerun `cara sync`"
                    .to_owned(),
            ],
        };
        return decision_error(&decision, progress);
    }
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "github_mutation_failed",
        error.to_string(),
        Some(json!({
            "error": format!("{error:?}"),
            "operation_receipt": progress.operation_receipt(),
            "provider_receipts": progress.provider_receipts,
            "events": progress.events,
            "affected_pr": affected_pr,
            "resumable": true,
            "next": "rediscover and rerun the same `cara sync` command",
        })),
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet, VecDeque};

    use super::*;
    use crate::graph;
    use crate::model::{
        AutoMergeState, BranchSnapshot, CheckSnapshot, CommitOid, CompatibilityReport,
        RepositorySnapshot,
    };

    struct FakeProvider {
        allows_auto_merge: bool,
        branch_protected: bool,
        pulls: RefCell<BTreeMap<PrNumber, PullRequestSnapshot>>,
        failures: RefCell<VecDeque<MutationKind>>,
        calls: RefCell<Vec<MutationKind>>,
        failed_runs: RefCell<BTreeMap<PrNumber, Vec<WorkflowRunSnapshot>>>,
        diagnostic_heads: RefCell<BTreeMap<PrNumber, CommitOid>>,
        diagnostic_job_conclusions: RefCell<BTreeMap<PrNumber, String>>,
        diagnostic_lineage: RefCell<BTreeMap<PrNumber, crate::ci::SelectedRefLineageReceipt>>,
        admin_permission: bool,
        branch_head: RefCell<crate::model::CommitOid>,
        audits: RefCell<Vec<ControlLabelAudit>>,
        comments: RefCell<BTreeMap<PrNumber, Vec<String>>>,
    }

    impl FakeProvider {
        fn with_pull_requests(pulls: Vec<PullRequestSnapshot>) -> Self {
            Self {
                allows_auto_merge: true,
                branch_protected: true,
                pulls: RefCell::new(
                    pulls
                        .into_iter()
                        .map(|pull_request| (pull_request.number, pull_request))
                        .collect(),
                ),
                failures: RefCell::new(VecDeque::new()),
                calls: RefCell::new(Vec::new()),
                failed_runs: RefCell::new(BTreeMap::new()),
                diagnostic_heads: RefCell::new(BTreeMap::new()),
                diagnostic_job_conclusions: RefCell::new(BTreeMap::new()),
                diagnostic_lineage: RefCell::new(BTreeMap::new()),
                admin_permission: true,
                branch_head: RefCell::new(branch("main").oid),
                audits: RefCell::new(Vec::new()),
                comments: RefCell::new(BTreeMap::new()),
            }
        }

        fn fail_once(&self, kind: MutationKind) {
            self.failures.borrow_mut().push_back(kind);
        }

        fn mutate(
            &self,
            expected: &PullRequestPrecondition,
            kind: MutationKind,
            change: impl FnOnce(&mut PullRequestSnapshot),
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.calls.borrow_mut().push(kind);
            if self.failures.borrow().front() == Some(&kind) {
                self.failures.borrow_mut().pop_front();
                return Err(MutationError::Provider(
                    crate::github::DiscoveryError::CommandFailed {
                        command: crate::command::CommandSpec::new("fake"),
                        code: Some(1),
                        stderr: "injected failure".to_owned(),
                    },
                ));
            }
            let before = self
                .pulls
                .borrow()
                .get(&expected.number)
                .cloned()
                .expect("fake PR");
            let actual = PullRequestPrecondition::from(&before);
            if &actual != expected {
                return Err(MutationError::StalePrecondition {
                    expected: Box::new(expected.clone()),
                    actual: Box::new(actual),
                    changed_fields: vec!["fake_race".to_owned()],
                });
            }
            let mut after = before.clone();
            change(&mut after);
            self.pulls.borrow_mut().insert(after.number, after.clone());
            Ok(GitHubMutationReceipt {
                kind,
                before: Some(before),
                after,
                provider_output: None,
            })
        }
    }

    impl SyncProvider for FakeProvider {
        fn verify_pull_request(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
        ) -> Result<PullRequestSnapshot, MutationError> {
            let actual = self
                .pulls
                .borrow()
                .get(&expected.number)
                .cloned()
                .expect("fake PR");
            let actual_precondition = PullRequestPrecondition::from(&actual);
            if actual_precondition != *expected {
                return Err(MutationError::StalePrecondition {
                    expected: Box::new(expected.clone()),
                    actual: Box::new(actual_precondition),
                    changed_fields: vec!["fake_race".to_owned()],
                });
            }
            Ok(actual)
        }

        fn refetch_pull_request(
            &self,
            _repository: &RepositoryId,
            number: PrNumber,
        ) -> Result<PullRequestSnapshot, MutationError> {
            Ok(self.pulls.borrow()[&number].clone())
        }

        fn verify_branch_head(
            &self,
            _repository: &RepositoryId,
            branch: &str,
            expected: &crate::model::CommitOid,
        ) -> Result<(), MutationError> {
            let actual = self.branch_head.borrow().clone();
            if &actual != expected {
                return Err(MutationError::BranchHeadMismatch {
                    branch: branch.to_owned(),
                    expected: expected.clone(),
                    actual,
                });
            }
            Ok(())
        }

        fn branch_is_protected(
            &self,
            _repository: &RepositoryId,
            _branch: &str,
        ) -> Result<bool, MutationError> {
            Ok(self.branch_protected)
        }

        fn repository_allows_auto_merge(
            &self,
            _repository: &RepositoryId,
        ) -> Result<bool, MutationError> {
            Ok(self.allows_auto_merge)
        }

        fn set_base(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            base: &str,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::SetBase, |pull_request| {
                pull_request.base = branch(base);
            })
        }

        fn enable_squash_auto_merge(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::EnableAutoMerge, |pull_request| {
                pull_request.auto_merge = AutoMergeState::squash();
            })
        }

        fn disable_auto_merge(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::DisableAutoMerge, |pull_request| {
                pull_request.auto_merge = AutoMergeState::disabled();
            })
        }

        fn add_label(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            label: &str,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::AddLabel, |pull_request| {
                pull_request.labels.insert(label.to_owned());
            })
        }

        fn remove_label(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            label: &str,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::RemoveLabel, |pull_request| {
                pull_request.labels.remove(label);
            })
        }

        fn pull_request_comment_bodies(
            &self,
            _repository: &RepositoryId,
            number: PrNumber,
        ) -> Result<Vec<String>, MutationError> {
            Ok(self
                .comments
                .borrow()
                .get(&number)
                .cloned()
                .unwrap_or_default())
        }

        fn ensure_marked_comment(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            marker: &str,
            body: &str,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            let already = self
                .comments
                .borrow()
                .get(&expected.number)
                .is_some_and(|comments| comments.iter().any(|item| item.contains(marker)));
            if !already {
                self.comments
                    .borrow_mut()
                    .entry(expected.number)
                    .or_default()
                    .push(body.to_owned());
            }
            let mut receipt = self.mutate(expected, MutationKind::Comment, |_| {})?;
            if already {
                receipt.provider_output = Some(format!("existing GitHub comment {marker}"));
            }
            Ok(receipt)
        }

        fn failed_runs_for_pull_request(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
        ) -> Result<Vec<WorkflowRunSnapshot>, MutationError> {
            let current = self
                .pulls
                .borrow()
                .get(&expected.number)
                .cloned()
                .expect("fake PR");
            let actual = PullRequestPrecondition::from(&current);
            if &actual != expected {
                return Err(MutationError::StalePrecondition {
                    expected: Box::new(expected.clone()),
                    actual: Box::new(actual),
                    changed_fields: vec!["fake_race".to_owned()],
                });
            }
            Ok(self
                .failed_runs
                .borrow()
                .get(&expected.number)
                .cloned()
                .unwrap_or_default())
        }

        fn failed_run_diagnostics(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            run_ids: &[u64],
        ) -> Result<WorkflowFailureDiagnostics, MutationError> {
            let failed_runs = self
                .failed_runs
                .borrow()
                .get(&expected.number)
                .cloned()
                .unwrap_or_default();
            let runs = run_ids
                .iter()
                .filter_map(|run_id| {
                    failed_runs
                        .iter()
                        .find(|run| run.database_id == *run_id)
                        .map(|run| crate::ci::WorkflowRunFailureDiagnostic {
                            run_id: *run_id,
                            attempt: 1,
                            workflow_id: *run_id,
                            check_suite_id: *run_id,
                            workflow_name: run.workflow_name.clone(),
                            event: "pull_request".to_owned(),
                            status: run.status.clone(),
                            conclusion: run.conclusion.clone(),
                            head_branch: "feature".to_owned(),
                            head_sha: CommitOid(run.head_sha.clone()),
                            expected_pr: expected.number,
                            expected_head_oid: expected.head_oid.clone(),
                            expected_base_oid: expected.base_oid.clone(),
                            pull_requests: vec![crate::ci::WorkflowRunPullRequestAssociation {
                                pr: expected.number,
                                head_oid: Some(
                                    self.diagnostic_heads
                                        .borrow()
                                        .get(&expected.number)
                                        .cloned()
                                        .unwrap_or_else(|| expected.head_oid.clone()),
                                ),
                                base_oid: Some(expected.base_oid.clone()),
                            }],
                            failed_jobs: vec![crate::ci::WorkflowJobFailureDiagnostic {
                                job_id: *run_id,
                                name: "test infrastructure".to_owned(),
                                status: "completed".to_owned(),
                                conclusion: self
                                    .diagnostic_job_conclusions
                                    .borrow()
                                    .get(&expected.number)
                                    .cloned()
                                    .unwrap_or_else(|| "timed_out".to_owned()),
                                url: run.url.clone(),
                                runner_name: None,
                                runner_labels: Vec::new(),
                                failed_steps: Vec::new(),
                                steps_truncated: false,
                                selected_lineage: self
                                    .diagnostic_lineage
                                    .borrow()
                                    .get(&expected.number)
                                    .cloned(),
                                lineage_evidence_status: if self
                                    .diagnostic_lineage
                                    .borrow()
                                    .contains_key(&expected.number)
                                {
                                    crate::ci::LineageEvidenceStatus::Parsed
                                } else {
                                    crate::ci::LineageEvidenceStatus::NotRequested
                                },
                            }],
                            jobs_total: 1,
                            jobs_truncated: false,
                        })
                })
                .collect();
            Ok(WorkflowFailureDiagnostics {
                requested_run_ids: run_ids.to_vec(),
                runs,
                runs_truncated: false,
            })
        }

        fn rerun_failed_run(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            run_id: u64,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            let run = self
                .failed_runs
                .borrow()
                .get(&expected.number)
                .and_then(|runs| runs.iter().find(|run| run.database_id == run_id))
                .cloned()
                .expect("exact failed run");
            if !run.pull_requests.contains(&expected.number) {
                return Err(MutationError::RunPullRequestMismatch {
                    run_id,
                    expected_pr: expected.number,
                    actual_prs: run.pull_requests,
                });
            }
            if run.head_sha != expected.head_oid.0 {
                return Err(MutationError::RunHeadMismatch {
                    run_id,
                    expected_head: expected.head_oid.0.clone(),
                    actual_head: run.head_sha,
                });
            }
            self.mutate(expected, MutationKind::RerunChecks, |pull_request| {
                for check in &mut pull_request.checks {
                    if check.details_url.as_deref().and_then(workflow_run_id) == Some(run_id) {
                        check.state = CheckState::Queued;
                        check.provider_state = Some("QUEUED".to_owned());
                    }
                }
            })
        }

        fn viewer_permission(&self, _repository: &RepositoryId) -> Result<String, MutationError> {
            Ok(if self.admin_permission {
                "ADMIN"
            } else {
                "WRITE"
            }
            .to_owned())
        }

        fn ensure_control_label_comment(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            audit: &ControlLabelAudit,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.audits.borrow_mut().push(audit.clone());
            self.mutate(expected, MutationKind::Comment, |_| {})
        }

        fn admin_squash_merge(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            if !self.admin_permission {
                return Err(MutationError::PermissionDenied {
                    required: "ADMIN".to_owned(),
                    actual: "WRITE".to_owned(),
                });
            }
            self.mutate(expected, MutationKind::SquashMerge, |pull_request| {
                pull_request.state = PullRequestState::Merged;
                pull_request.merged_at = Some("now".to_owned());
                pull_request.auto_merge = AutoMergeState::disabled();
            })
        }
    }

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

    fn pull_request(
        number: u64,
        head: &str,
        base: &str,
        state: PullRequestState,
        auto_merge: AutoMergeState,
    ) -> PullRequestSnapshot {
        PullRequestSnapshot {
            number: PrNumber(number),
            title: format!("PR {number}"),
            url: format!("https://example.invalid/{number}"),
            state,
            draft: false,
            head: branch(head),
            base: branch(base),
            cross_repository: false,
            labels: BTreeSet::from(["caravan".to_owned()]),
            auto_merge,
            checks: Vec::<CheckSnapshot>::new(),
            created_at: Some(format!("2026-01-01T00:00:{number:02}Z")),
            merged_at: (state == PullRequestState::Merged).then(|| "now".to_owned()),
            updated_at: None,
        }
    }

    fn check(name: &str, state: CheckState, run_id: Option<u64>) -> CheckSnapshot {
        CheckSnapshot {
            name: name.to_owned(),
            state,
            provider_state: Some(format!("{state:?}").to_uppercase()),
            details_url: run_id.map(|id| {
                format!("https://github.com/harryaskham/caravan/actions/runs/{id}/job/1")
            }),
        }
    }

    fn selected_lineage_receipt(
        pull_request: &PullRequestSnapshot,
    ) -> crate::ci::SelectedRefLineageReceipt {
        crate::ci::SelectedRefLineageReceipt {
            event: "pull_request".to_owned(),
            head_ref: pull_request.head.name.clone(),
            selected_ref: "refs/pull/1/merge".to_owned(),
            selected_commit: CommitOid("selected-merge".to_owned()),
            actual_head: CommitOid("selected-merge".to_owned()),
            expected_head: pull_request.head.oid.clone(),
            expected_base: pull_request.base.oid.clone(),
            parents: vec![
                pull_request.base.oid.clone(),
                CommitOid("prior-head".to_owned()),
            ],
        }
    }

    fn failed_run(id: u64, head: &PullRequestSnapshot) -> WorkflowRunSnapshot {
        WorkflowRunSnapshot {
            database_id: id,
            pull_requests: vec![head.number],
            head_sha: head.head.oid.0.clone(),
            status: "completed".to_owned(),
            conclusion: "failure".to_owned(),
            event: "pull_request".to_owned(),
            name: "CI".to_owned(),
            workflow_name: "CI".to_owned(),
            url: format!("https://github.com/harryaskham/caravan/actions/runs/{id}"),
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn clean(
        candidate: &BranchSnapshot,
        target: &BranchSnapshot,
    ) -> Result<CompatibilityReport, AppError> {
        Ok(CompatibilityReport {
            candidate: candidate.clone(),
            target: target.clone(),
            outcome: CompatibilityOutcome::Clean,
            conflicting_paths: Vec::new(),
            diagnostic: None,
        })
    }

    fn status(
        pulls: Vec<PullRequestSnapshot>,
        current: Option<PrNumber>,
        checker: &impl graph::CompatibilityChecker,
    ) -> StatusOutput {
        let snapshot = RepositorySnapshot {
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            repository: repository(),
            default_branch: branch("main"),
            current_branch: current.map(|number| format!("pr-{number}")),
            current_pr: current,
            pull_requests: pulls,
            observed_at: None,
        };
        let analysis = graph::analyze(&snapshot, checker).expect("analysis");
        StatusOutput {
            provider_api: crate::model::GitHubApiTelemetry::default(),
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            timing: None,
            repository: repository(),
            rebase_on_join: crate::read::RebaseOnJoinStatus::default(),
            auto_admission: crate::read::AutoAdmissionStatus::default(),
            default_branch: "main".to_owned(),
            current_branch: snapshot.current_branch,
            current_pr: snapshot.current_pr,
            healthy: analysis.healthy(),
            initialization: crate::initialization::InitializationStatus::default(),
            admission: read::resolve_admission(
                &analysis,
                &crate::config::CaravanConfig::default().agent_priority_labels,
            ),
            analysis,
            pauses: Vec::new(),
        }
    }

    fn healthy_chain() -> Vec<PullRequestSnapshot> {
        vec![
            pull_request(
                1,
                "one",
                "main",
                PullRequestState::Open,
                AutoMergeState::squash(),
            ),
            pull_request(
                2,
                "two",
                "one",
                PullRequestState::Open,
                AutoMergeState::disabled(),
            ),
            pull_request(
                3,
                "three",
                "two",
                PullRequestState::Open,
                AutoMergeState::disabled(),
            ),
        ]
    }

    #[test]
    fn no_write_caravan_plan_records_actions_without_provider_mutation() {
        let pulls = healthy_chain();
        let status = status(pulls.clone(), Some(PrNumber(1)), &clean);
        let provider = FakeProvider::with_pull_requests(pulls);
        let caravan = status.analysis.fleet.caravans[0].clone();
        let mut progress = SyncProgress::new(&status, vec![caravan.id], 20);
        let mut actions = Vec::new();
        let mut decisions = Vec::new();
        let mut events = Vec::new();

        plan_caravan_convergence(
            &status,
            &provider,
            &caravan,
            &SyncInput {
                all: true,
                rerun_failed: false,
            },
            false,
            false,
            &mut progress,
            &mut actions,
            &mut decisions,
            &mut events,
        )
        .expect("planning reads but never mutates");

        assert!(provider.calls.borrow().is_empty());
        assert!(decisions.is_empty());
        assert_eq!(progress.ci.len(), 3);
        assert!(actions.iter().any(|action| action.kind == "set_base"));
        assert!(actions.iter().any(|action| action.kind == "observe_ci"));
        assert!(actions.iter().any(|action| {
            action.kind == "enable_squash_auto_merge"
                && action.state == SyncPlanActionState::AlreadySatisfied
        }));
        assert!(actions.iter().all(|action| {
            action.state != SyncPlanActionState::WouldMutate
                && action.state != SyncPlanActionState::WouldStop
        }));
    }

    #[test]
    fn no_write_auto_admission_plans_only_first_exact_candidate() {
        let mut candidate = pull_request(
            9,
            "candidate",
            "main",
            PullRequestState::Open,
            AutoMergeState::disabled(),
        );
        candidate.labels.clear();
        let status = status(vec![candidate.clone()], Some(candidate.number), &clean);
        let mut context = AppContext::default();
        context.config.rebase_on_join = true;
        context.config.sync.actions.join_unlabelled_prs = true;
        let mut actions = Vec::new();
        let mut events = Vec::new();

        let plan = plan_auto_admission_with_checker(
            &context,
            &status,
            &SyncInput {
                all: true,
                rerun_failed: false,
            },
            false,
            &mut actions,
            &mut events,
            &clean,
        )
        .expect("canonical candidate is planned without mutation");

        assert_eq!(plan.candidate_pr, Some(candidate.number));
        assert_eq!(plan.target_tail, None);
        assert_eq!(plan.continuation, "replan_after_first_admission");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, "auto_admission_new");
        assert_eq!(actions[0].state, SyncPlanActionState::WouldMutate);
        assert_eq!(events, vec![EventKind::CaravanCreated]);
    }

    #[test]
    fn no_write_auto_admission_never_leapfrogs_rejected_canonical_candidate() {
        let mut candidate = pull_request(
            9,
            "candidate",
            "main",
            PullRequestState::Open,
            AutoMergeState::disabled(),
        );
        candidate.labels.clear();
        candidate
            .labels
            .insert("caravan-priority:unknown".to_owned());
        let status = status(vec![candidate.clone()], Some(candidate.number), &clean);
        let mut context = AppContext::default();
        context.config.rebase_on_join = true;
        context.config.sync.actions.join_unlabelled_prs = true;
        let mut actions = Vec::new();
        let mut events = Vec::new();
        let plan = plan_auto_admission_with_checker(
            &context,
            &status,
            &SyncInput {
                all: true,
                rerun_failed: false,
            },
            false,
            &mut actions,
            &mut events,
            &clean,
        )
        .expect("rejection is a no-write plan result");
        assert_eq!(plan.candidate_pr, Some(candidate.number));
        assert_eq!(plan.continuation, "rejected_canonical_candidate");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, "reject_canonical_candidate");
        assert_eq!(actions[0].state, SyncPlanActionState::WouldStop);
        assert!(events.is_empty());
    }

    #[test]
    fn plan_hash_binds_exact_actions_not_telemetry() {
        let status = status(healthy_chain(), Some(PrNumber(1)), &clean);
        let base = SyncPlanOutput {
            schema_version: 1,
            mutated: false,
            provider_writes: 0,
            local_ephemeral_preflight: false,
            repository: status.repository.clone(),
            default_branch: status.analysis.fleet.default_branch.clone(),
            all: true,
            plan_hash: String::new(),
            selected_caravans: vec![PrNumber(1)],
            physical_rebase_plans: Vec::new(),
            ci: Vec::new(),
            actions: vec![SyncPlanAction {
                order: 1,
                phase: SyncPlanPhase::ProviderConvergence,
                state: SyncPlanActionState::AlreadySatisfied,
                kind: "set_base".to_owned(),
                pr: Some(PrNumber(1)),
                caravan_id: Some(PrNumber(1)),
                expected: None,
                target: Some(json!({"branch": "main"})),
                reason: "already exact".to_owned(),
            }],
            auto_admission: SyncAutoAdmissionPlan {
                enabled: false,
                heuristic_version: AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
                continuation: "disabled".to_owned(),
                candidate_pr: None,
                target_tail: None,
                tested_tails: Vec::new(),
                compatibility_reasons: Vec::new(),
            },
            decisions: Vec::new(),
            would_emit_events: Vec::new(),
            github_requests_used: 1,
            status,
        };
        let first = base.clone().finalize_hash();
        let mut telemetry_changed = base.clone();
        telemetry_changed.github_requests_used = 99;
        telemetry_changed.status.provider_api.calls = 99;
        let second = telemetry_changed.finalize_hash();
        assert_eq!(first.plan_hash, second.plan_hash);

        let mut changed = base;
        changed.actions[0].reason = "different exact action".to_owned();
        assert_ne!(first.plan_hash, changed.finalize_hash().plan_hash);
    }

    #[test]
    fn greedy_planner_forms_empty_fleet_then_uses_first_compatible_tail() {
        let mut candidate = pull_request(
            9,
            "candidate",
            "main",
            PullRequestState::Open,
            AutoMergeState::disabled(),
        );
        candidate.labels.clear();
        let empty = status(vec![candidate.clone()], Some(candidate.number), &clean);
        let evaluation =
            evaluate_auto_candidate(&empty, &candidate, &clean).expect("empty fleet preflight");
        assert_eq!(evaluation.target, AutoCandidateTarget::New);
        assert!(evaluation.tested_tails.is_empty());
        assert!(evaluation.reasons.is_empty());

        let first = pull_request(
            1,
            "first",
            "main",
            PullRequestState::Open,
            AutoMergeState::squash(),
        );
        let second = pull_request(
            2,
            "second",
            "main",
            PullRequestState::Open,
            AutoMergeState::squash(),
        );
        let fleet = status(
            vec![first, second, candidate.clone()],
            Some(candidate.number),
            &clean,
        );
        let checker = |candidate_branch: &BranchSnapshot,
                       target: &BranchSnapshot|
         -> Result<CompatibilityReport, AppError> {
            Ok(CompatibilityReport {
                candidate: candidate_branch.clone(),
                target: target.clone(),
                outcome: if target.name == "first" {
                    CompatibilityOutcome::Conflict
                } else {
                    CompatibilityOutcome::Clean
                },
                conflicting_paths: if target.name == "first" {
                    vec!["src/lib.rs".to_owned()]
                } else {
                    Vec::new()
                },
                diagnostic: None,
            })
        };
        let evaluation =
            evaluate_auto_candidate(&fleet, &candidate, &checker).expect("tail preflight");
        assert_eq!(evaluation.target, AutoCandidateTarget::Join(PrNumber(2)));
        assert_eq!(
            evaluation
                .tested_tails
                .iter()
                .map(|tail| tail.tail_pr)
                .collect::<Vec<_>>(),
            [PrNumber(1), PrNumber(2)]
        );
        assert!(
            evaluation
                .reasons
                .iter()
                .any(|reason| reason.contains("tail #1"))
        );
    }

    #[test]
    fn skip_receipt_round_trips_and_invalidates_on_generation_change() {
        let active = pull_request(
            1,
            "head",
            "main",
            PullRequestState::Open,
            AutoMergeState::squash(),
        );
        let mut candidate = pull_request(
            9,
            "candidate",
            "main",
            PullRequestState::Open,
            AutoMergeState::disabled(),
        );
        candidate.labels.clear();
        candidate
            .labels
            .insert(AUTO_ADMISSION_SKIP_LABEL.to_owned());
        let status = status(
            vec![active, candidate.clone()],
            Some(candidate.number),
            &clean,
        );
        let context = AppContext::default();
        let receipt = AutoJoinSkipReceipt {
            schema_version: 1,
            repository: status.repository.clone(),
            candidate_pr: candidate.number,
            candidate_head: candidate.head.clone(),
            candidate_base: candidate.base.clone(),
            default_branch: status.analysis.fleet.default_branch.clone(),
            tested_tails: current_tail_generations(&status),
            config_fingerprint: auto_admission_config_fingerprint(&context),
            heuristic_version: AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
            compatibility_reasons: vec!["tail #1: conflict".to_owned()],
            actor: "cara sync automatic admission".to_owned(),
            observed_unix_secs: 1,
            evidence_hash: String::new(),
        }
        .finalize_hash();

        let parsed = AutoJoinSkipReceipt::from_comment(&receipt.comment_body())
            .expect("receipt marker decodes");
        assert_eq!(parsed, receipt);
        assert!(skip_receipt_matches(&context, &status, &receipt));

        let mut moved = status.clone();
        moved
            .analysis
            .pull_requests
            .get_mut(&candidate.number)
            .unwrap()
            .head
            .oid = CommitOid("moved".repeat(8));
        assert!(!skip_receipt_matches(&context, &moved, &receipt));

        let mut tail_moved = status.clone();
        tail_moved
            .analysis
            .pull_requests
            .get_mut(&PrNumber(1))
            .unwrap()
            .head
            .oid = CommitOid("tailmoved".repeat(5));
        assert!(!skip_receipt_matches(&context, &tail_moved, &receipt));

        let mut config_changed = context.clone();
        config_changed.config.sync.max_candidates_per_tick += 1;
        assert!(!skip_receipt_matches(&config_changed, &status, &receipt));
    }

    #[test]
    fn persist_skip_is_idempotent_and_manual_membership_can_consume_the_label() {
        let mut candidate = pull_request(
            9,
            "candidate",
            "main",
            PullRequestState::Open,
            AutoMergeState::disabled(),
        );
        candidate.labels.clear();
        let status = status(vec![candidate.clone()], Some(candidate.number), &clean);
        let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
        let mut progress = SyncProgress::new(&status, Vec::new(), u32::MAX);
        let receipt = AutoJoinSkipReceipt {
            schema_version: 1,
            repository: status.repository.clone(),
            candidate_pr: candidate.number,
            candidate_head: candidate.head.clone(),
            candidate_base: candidate.base.clone(),
            default_branch: status.analysis.fleet.default_branch.clone(),
            tested_tails: Vec::new(),
            config_fingerprint: auto_admission_config_fingerprint(&AppContext::default()),
            heuristic_version: AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
            compatibility_reasons: vec!["default conflict".to_owned()],
            actor: "cara sync automatic admission".to_owned(),
            observed_unix_secs: 1,
            evidence_hash: String::new(),
        }
        .finalize_hash();

        persist_auto_skip(&provider, &mut progress, &repository(), &receipt).unwrap();
        persist_auto_skip(&provider, &mut progress, &repository(), &receipt).unwrap();

        assert!(provider.pulls.borrow()[&candidate.number].has_label(AUTO_ADMISSION_SKIP_LABEL));
        assert_eq!(provider.comments.borrow()[&candidate.number].len(), 1);
        assert_eq!(
            provider
                .calls
                .borrow()
                .iter()
                .filter(|kind| **kind == MutationKind::AddLabel)
                .count(),
            1
        );
    }

    #[test]
    fn oversized_skip_receipt_fails_before_label_mutation() {
        let mut candidate = pull_request(
            9,
            "candidate",
            "main",
            PullRequestState::Open,
            AutoMergeState::disabled(),
        );
        candidate.labels.clear();
        let status = status(vec![candidate.clone()], Some(candidate.number), &clean);
        let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
        let mut progress = SyncProgress::new(&status, Vec::new(), u32::MAX);
        let receipt = AutoJoinSkipReceipt {
            schema_version: 1,
            repository: status.repository.clone(),
            candidate_pr: candidate.number,
            candidate_head: candidate.head.clone(),
            candidate_base: candidate.base.clone(),
            default_branch: status.analysis.fleet.default_branch.clone(),
            tested_tails: Vec::new(),
            config_fingerprint: auto_admission_config_fingerprint(&AppContext::default()),
            heuristic_version: AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
            compatibility_reasons: vec!["x".repeat(MAX_AUTO_ADMISSION_COMMENT_BYTES)],
            actor: "cara sync automatic admission".to_owned(),
            observed_unix_secs: 1,
            evidence_hash: String::new(),
        }
        .finalize_hash();

        let error = persist_auto_skip(&provider, &mut progress, &repository(), &receipt)
            .expect_err("oversized authority must not be truncated");

        assert_eq!(error.code(), "auto_admission_skip_receipt_too_large");
        assert!(provider.calls.borrow().is_empty());
        assert!(!provider.pulls.borrow()[&candidate.number].has_label(AUTO_ADMISSION_SKIP_LABEL));
    }

    #[test]
    fn pending_ci_reports_waiting_without_speculative_mutation() {
        let mut pulls = healthy_chain();
        pulls[0].checks = vec![check("build-test", CheckState::Queued, Some(7))];
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let progress = execute(&status, &provider, false, false, false).expect("pending waits");

        assert_eq!(progress.ci[0].disposition, CiDisposition::Waiting);
        assert!(!progress.operation_receipt().changed);
        assert!(progress.events.is_empty());
        assert!(provider.calls.borrow().is_empty());
        let scheduler =
            successful_scheduler_status(&status, &progress.ci, &progress.paused_caravans, true);
        assert_eq!(scheduler.disposition, SchedulerDisposition::WaitingCi);
        assert_eq!(scheduler.wake_class, SchedulerWakeClass::None);
        assert_eq!(
            scheduler.waiting_prs,
            [PrNumber(1), PrNumber(2), PrNumber(3)]
        );
        assert_eq!(scheduler.caravans[0].root, PrNumber(1));
        assert_eq!(scheduler.caravans[0].tail, PrNumber(3));
        assert_eq!(
            scheduler.caravans[0].members[0].ci,
            Some(CiDisposition::Waiting)
        );
        let encoded = serde_json::to_value(&scheduler).expect("scheduler status JSON");
        assert_eq!(encoded["schema_version"], 1);
        assert_eq!(encoded["disposition"], "waiting_ci");
        assert_eq!(encoded["wake_class"], "none");
        assert_eq!(encoded["default_branch"]["name"], "main");
        assert_eq!(encoded["caravans"][0]["root"], 1);
        assert_eq!(encoded["caravans"][0]["tail"], 3);
        assert_eq!(encoded["caravans"][0]["members"][0]["pr"], 1);
    }

    #[test]
    fn unforced_failure_returns_exact_ci_decision_and_canonical_event() {
        let mut pulls = healthy_chain();
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(10))];
        let matching = failed_run(10, &pulls[0]);
        let spurious = failed_run(11, &pulls[0]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![spurious, matching]);
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let error =
            execute(&status, &provider, false, false, false).expect_err("failed CI decides");

        assert_eq!(mcp_cli::StructuredError::code(&error), "ci_failure");
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["decision"]["evidence"]["ci"]["pr"], 1);
        assert_eq!(
            details["decision"]["evidence"]["ci"]["rerunnable_run_ids"],
            json!([10])
        );
        assert_eq!(
            details["decision"]["evidence"]["event"]["kind"],
            "ci_failed"
        );
        assert_eq!(
            details["decision"]["evidence"]["event"]["operation_id"],
            details["decision"]["operation_id"]
        );
        assert!(provider.calls.borrow().is_empty());
        let scheduler = scheduler_failure_status(&error);
        assert_eq!(
            scheduler.disposition,
            SchedulerDisposition::ExternalDecision
        );
        assert_eq!(scheduler.wake_class, SchedulerWakeClass::ExternalDecision);
        assert!(!scheduler.retryable);
    }

    #[test]
    fn stale_provider_precondition_retries_without_waking_a_repair_actor() {
        let pulls = healthy_chain();
        let status = status(pulls.clone(), Some(PrNumber(1)), &clean);
        let progress = SyncProgress::new(&status, vec![PrNumber(1)], u32::MAX);
        let expected = PullRequestPrecondition::from(&pulls[0]);
        let mut actual_pull = pulls[0].clone();
        actual_pull.head.oid = CommitOid("moved-head".to_owned());
        let actual = PullRequestPrecondition::from(&actual_pull);
        let error = mutation_error(
            &MutationError::StalePrecondition {
                expected: Box::new(expected),
                actual: Box::new(actual),
                changed_fields: vec!["head_oid".to_owned()],
            },
            &progress,
            Some(PrNumber(1)),
        );

        let scheduler = scheduler_failure_status(&error);
        assert_eq!(scheduler.disposition, SchedulerDisposition::RetryTick);
        assert_eq!(scheduler.wake_class, SchedulerWakeClass::RetryTick);
        assert!(scheduler.retryable);
        let attached = attach_scheduler_failure(&error, &scheduler);
        let details = attached.details().expect("scheduler details");
        assert_eq!(details["scheduler_status"]["disposition"], "retry_tick");
        assert_eq!(details["scheduler_status"]["wake_class"], "retry_tick");
    }

    #[test]
    fn provider_generation_invariant_emits_external_decision_wake_event() {
        let error = AppError::structured(
            ErrorCategory::Validation,
            "rebase_midpoint_head_stale",
            "provider exposed a different rewritten head",
            Some(json!({
                "repository": repository(),
                "rebase_plans": [],
                "rebase_receipts": [],
            })),
        );
        let scheduler = scheduler_failure_status(&error);
        assert_eq!(scheduler.wake_class, SchedulerWakeClass::ExternalDecision);
        let attached = attach_scheduler_failure(&error, &scheduler);
        let event = sync_failed_event(&attached).expect("external decision event");
        assert_eq!(event.kind, EventKind::SyncFailed);
        assert_eq!(event.metadata["error_code"], "rebase_midpoint_head_stale");
        assert_eq!(
            event.metadata["scheduler_status"]["wake_class"],
            "external_decision"
        );
    }

    #[test]
    fn nonlinear_range_is_a_stable_external_decision_with_exact_context() {
        let raw = AppError::structured(
            ErrorCategory::Validation,
            "rebase_nonlinear_range",
            "candidate-only history contains merge commits",
            Some(json!({
                "pr": PrNumber(2),
                "merge_oids": ["merge-a", "merge-b"],
                "completed_steps": [],
                "provider_receipts": [],
                "rebase_plans": [],
                "rebase_receipts": [],
            })),
        );
        let physical = attach_physical_rebuild(
            raw,
            &PhysicalRebuildOutcome {
                repository: Some(repository()),
                caravan_id: Some(PrNumber(1)),
                affected_prs: vec![PrNumber(2)],
                ..PhysicalRebuildOutcome::default()
            },
        );
        let scheduler = scheduler_failure_status(&physical);
        assert_eq!(
            scheduler.disposition,
            SchedulerDisposition::ExternalDecision
        );
        assert_eq!(scheduler.wake_class, SchedulerWakeClass::ExternalDecision);
        assert!(!scheduler.retryable);

        let attached = attach_scheduler_failure(&physical, &scheduler);
        let first_fingerprint = attached.details().unwrap()["decision_fingerprint"]
            .as_str()
            .unwrap()
            .to_owned();
        let repeated = attach_scheduler_failure(&physical, &scheduler);
        assert_eq!(
            repeated.details().unwrap()["decision_fingerprint"],
            first_fingerprint
        );
        let event = sync_failed_event(&attached).expect("external decision event");
        assert_eq!(event.caravan_id, Some(PrNumber(1)));
        assert_eq!(event.prs, vec![PrNumber(2)]);
        assert_eq!(event.metadata["decision_fingerprint"], first_fingerprint);
        let details = attached.details().unwrap();
        assert_eq!(details["retryable"], false);
        assert!(
            details["next"]
                .as_str()
                .unwrap()
                .contains("cannot succeed by retry")
        );
        assert_eq!(details["completed_steps"], json!([]));
        assert_eq!(details["provider_receipts"], json!([]));
    }

    #[test]
    fn unsupported_exact_range_shapes_are_never_retry_ticks() {
        for code in [
            "rebase_nonlinear_range",
            "rebase_range_ambiguous",
            "rebase_empty_patch_range",
            "rebase_target_history_changed",
            "rebase_repository_not_owned",
            "rebase_historical_target_mismatch",
            "rebase_unsupported_octopus",
            "rebase_topology_limit",
            "rebase_external_merge_parents",
            "rebase_cousin_history",
            "rebase_merge_tree_conflict",
            "rebase_merge_replay_conflict",
            "rebase_merge_tree_mismatch",
            "rebase_topology_changed",
        ] {
            let error = AppError::structured(
                ErrorCategory::Validation,
                code,
                "exact range decision",
                Some(json!({"repository": repository(), "pr": PrNumber(7)})),
            );
            let scheduler = scheduler_failure_status(&error);
            assert_eq!(
                scheduler.wake_class,
                SchedulerWakeClass::ExternalDecision,
                "{code}"
            );
            assert!(!scheduler.retryable, "{code}");
        }
    }

    #[test]
    fn stale_run_generation_requires_fresh_trigger_and_is_never_rerunnable() {
        let mut pulls = healthy_chain();
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(10))];
        let matching = failed_run(10, &pulls[0]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![matching]);
        provider
            .diagnostic_lineage
            .borrow_mut()
            .insert(PrNumber(1), selected_lineage_receipt(&pulls[0]));
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let error = execute(&status, &provider, false, true, false)
            .expect_err("stale generation cannot be rerun");

        assert!(provider.calls.borrow().is_empty());
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        let ci = &details["decision"]["evidence"]["ci"];
        assert_eq!(ci["rerunnable_run_ids"], json!([]));
        assert_eq!(ci["failure_diagnostics"][0]["generation"], "stale_head");
        assert_eq!(
            ci["failure_diagnostics"][0]["diagnostic"]["failed_jobs"][0]["selected_lineage"]["selected_commit"],
            "selected-merge"
        );
        assert_eq!(
            ci["failure_diagnostics"][0]["action"],
            "fresh_candidate_trigger"
        );
        assert!(
            details["decision"]["suggested_actions"][0]
                .as_str()
                .expect("action")
                .contains("fresh exact-candidate")
        );
    }

    #[test]
    fn current_generation_source_failure_recommends_repair_not_rerun() {
        let mut pulls = healthy_chain();
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(10))];
        let matching = failed_run(10, &pulls[0]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![matching]);
        provider
            .diagnostic_job_conclusions
            .borrow_mut()
            .insert(PrNumber(1), "failure".to_owned());
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let error = execute(&status, &provider, false, true, false)
            .expect_err("source failure requires repair");

        assert!(provider.calls.borrow().is_empty());
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        let ci = &details["decision"]["evidence"]["ci"];
        assert_eq!(ci["rerunnable_run_ids"], json!([]));
        assert_eq!(
            ci["failure_diagnostics"][0]["classification"],
            "source_or_test_failure"
        );
        assert_eq!(ci["failure_diagnostics"][0]["action"], "repair_source");
    }

    #[test]
    fn unknown_provider_state_is_a_non_rerunnable_ci_decision() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].checks = vec![CheckSnapshot {
            name: "future-ci".to_owned(),
            state: CheckState::Unknown,
            provider_state: Some("FUTURE_PROVIDER_STATE".to_owned()),
            details_url: None,
        }];
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let error = execute(&status, &provider, false, true, false)
            .expect_err("unknown CI cannot be guessed or rerun");

        assert_eq!(mcp_cli::StructuredError::code(&error), "ci_failure");
        assert!(provider.calls.borrow().is_empty());
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(
            details["decision"]["evidence"]["ci"]["checks"][0]["provider_state"],
            "FUTURE_PROVIDER_STATE"
        );
        assert_eq!(
            details["decision"]["evidence"]["ci"]["rerunnable_run_ids"],
            json!([])
        );
    }

    #[test]
    fn rerun_failed_selects_only_exact_current_run_then_stops() {
        let mut pulls = healthy_chain();
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(10))];
        let matching = failed_run(10, &pulls[0]);
        let spurious = failed_run(11, &pulls[0]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![spurious, matching]);
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let error = execute(&status, &provider, false, true, false)
            .expect_err("rerun still returns unresolved decision");

        assert_eq!(*provider.calls.borrow(), vec![MutationKind::RerunChecks]);
        assert_eq!(
            provider.pulls.borrow()[&PrNumber(1)].checks[0].state,
            CheckState::Queued
        );
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert!(
            details["decision"]["completed_steps"]
                .as_array()
                .expect("steps")
                .iter()
                .any(|step| step["summary"] == "reran failed jobs for exact workflow run 10")
        );
    }

    #[test]
    fn forced_downstream_failure_remains_in_chain_without_blocking() {
        let mut pulls = healthy_chain();
        pulls[0].checks = vec![check("build-test", CheckState::Success, Some(1))];
        pulls[1].labels.insert("caravan-force".to_owned());
        pulls[1].checks = vec![check("build-test", CheckState::Failure, Some(20))];
        let run = failed_run(20, &pulls[1]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(2), vec![run]);
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let progress =
            execute(&status, &provider, false, false, true).expect("force bypasses downstream");

        assert_eq!(progress.ci[1].disposition, CiDisposition::Forced);
        assert_eq!(progress.current[&PrNumber(2)].state, PullRequestState::Open);
        assert!(progress.events.is_empty());
        assert!(provider.calls.borrow().is_empty());
    }

    #[test]
    fn force_merge_requires_config_before_provider_attempt() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(30))];
        let run = failed_run(30, &pulls[0]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![run]);
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let error = execute(&status, &provider, false, false, false)
            .expect_err("config denies force merge");

        assert_eq!(mcp_cli::StructuredError::code(&error), "force_merge_denied");
        assert!(provider.calls.borrow().is_empty());
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["decision"]["evidence"]["events"], json!([]));
    }

    #[test]
    fn stale_forced_head_stops_before_admin_attempt() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(32))];
        let run = failed_run(32, &pulls[0]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![run]);
        let status = status(pulls, Some(PrNumber(1)), &clean);
        provider
            .pulls
            .borrow_mut()
            .get_mut(&PrNumber(1))
            .expect("head")
            .labels
            .insert("external-change".to_owned());

        let error =
            execute(&status, &provider, false, false, true).expect_err("stale head fails closed");

        assert_eq!(mcp_cli::StructuredError::code(&error), "stale_precondition");
        assert!(provider.calls.borrow().is_empty());
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["decision"]["evidence"]["events"], json!([]));
    }

    #[test]
    fn moved_default_branch_invalidates_force_compatibility_proof() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(33))];
        let run = failed_run(33, &pulls[0]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![run]);
        let status = status(pulls, Some(PrNumber(1)), &clean);
        *provider.branch_head.borrow_mut() = branch("moved-main").oid;

        let error = execute(&status, &provider, false, false, true)
            .expect_err("moved default invalidates proof");

        assert_eq!(mcp_cli::StructuredError::code(&error), "stale_precondition");
        assert!(provider.calls.borrow().is_empty());
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["decision"]["evidence"]["branch"], "main");
        assert_eq!(details["decision"]["evidence"]["events"], json!([]));
    }

    #[test]
    fn force_comment_failure_is_structured_and_prevents_admin_merge() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(34))];
        let run = failed_run(34, &pulls[0]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![run]);
        provider
            .failures
            .borrow_mut()
            .push_back(MutationKind::Comment);
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let error = execute(&status, &provider, false, false, true)
            .expect_err("comment is part of force receipt");

        assert_eq!(error.code(), "github_comment_failed");
        let details = error.details().expect("details");
        assert_eq!(details["stage"], "control_label_comment");
        assert_eq!(details["resumable"], true);
        assert_eq!(details["events"][0]["kind"], "force_merge_attempted");
        let extracted = crate::hooks::events_from_error(&error);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].kind, EventKind::ForceMergeAttempted);
        assert_eq!(
            json!(extracted[0].event_id),
            details["events"][0]["event_id"]
        );
        assert_eq!(*provider.calls.borrow(), vec![MutationKind::Comment]);
    }

    #[test]
    fn force_merge_permission_denial_preserves_attempt_event() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(31))];
        let run = failed_run(31, &pulls[0]);
        let mut provider = FakeProvider::with_pull_requests(pulls.clone());
        provider.admin_permission = false;
        provider
            .failed_runs
            .borrow_mut()
            .insert(PrNumber(1), vec![run]);
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let error = execute(&status, &provider, false, false, true)
            .expect_err("permission denies force merge");

        assert_eq!(mcp_cli::StructuredError::code(&error), "force_merge_denied");
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(
            details["decision"]["evidence"]["events"][0]["kind"],
            "force_merge_attempted"
        );
        assert!(provider.calls.borrow().is_empty());
    }

    #[test]
    fn physical_rewrite_invalidates_force_intent_bound_to_old_head_generation() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);
        let old_head = status.analysis.pull_requests[&PrNumber(1)].head.clone();
        let plan = crate::physical_rebase::RebasePlan {
            pr: PrNumber(1),
            branch: old_head.name.clone(),
            old_head_oid: old_head.oid.clone(),
            old_base_oid: status.analysis.fleet.default_branch.oid.clone(),
            range_source: crate::physical_rebase::PlannedRangeBase::RemoteBranch {
                branch: status.analysis.fleet.default_branch.clone(),
            },
            new_base: crate::physical_rebase::PlannedBase::Remote(
                status.analysis.fleet.default_branch.clone(),
            ),
            new_head_oid: CommitOid("rewritten0000000000000000000000000000000".to_owned()),
            new_tree_oid: CommitOid("tree000000000000000000000000000000000000".to_owned()),
            commit_count: 1,
            merge_topology: None,
            ci_trigger_workflows: vec!["CI".to_owned()],
            lease: format!("refs/heads/{}:{}", old_head.name, old_head.oid),
            already_satisfied: false,
        };
        let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], u32::MAX);

        invalidate_rewritten_force_intents(&status, &provider, &[plan], &mut progress)
            .expect("old-generation force intent is invalidated before rewrite");

        assert!(!progress.current[&PrNumber(1)].has_label("caravan-force"));
        assert_eq!(
            *provider.calls.borrow(),
            vec![MutationKind::RemoveLabel, MutationKind::Comment]
        );
        assert_eq!(progress.provider_receipts.len(), 2);
        let audits = provider.audits.borrow();
        assert_eq!(audits[0].operation, "force_invalidate_rewrite");
        assert!(audits[0].reason.contains(&old_head.oid.0));
        assert!(audits[0].reason.contains("rewritten"));
    }

    #[test]
    fn already_satisfied_generation_preserves_explicit_force_intent() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);
        let head = status.analysis.pull_requests[&PrNumber(1)].head.clone();
        let plan = crate::physical_rebase::RebasePlan {
            pr: PrNumber(1),
            branch: head.name.clone(),
            old_head_oid: head.oid.clone(),
            old_base_oid: status.analysis.fleet.default_branch.oid.clone(),
            range_source: crate::physical_rebase::PlannedRangeBase::RemoteBranch {
                branch: status.analysis.fleet.default_branch.clone(),
            },
            new_base: crate::physical_rebase::PlannedBase::Remote(
                status.analysis.fleet.default_branch.clone(),
            ),
            new_head_oid: head.oid.clone(),
            new_tree_oid: CommitOid("tree000000000000000000000000000000000000".to_owned()),
            commit_count: 1,
            merge_topology: None,
            ci_trigger_workflows: vec!["CI".to_owned()],
            lease: format!("refs/heads/{}:{}", head.name, head.oid),
            already_satisfied: true,
        };
        let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], u32::MAX);

        invalidate_rewritten_force_intents(&status, &provider, &[plan], &mut progress)
            .expect("unchanged generation retains explicit force intent");

        assert!(progress.current[&PrNumber(1)].has_label("caravan-force"));
        assert!(provider.calls.borrow().is_empty());
        assert!(provider.audits.borrow().is_empty());
    }

    #[test]
    fn fresh_force_reapplication_on_rewritten_generation_can_enter_force_path() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let initial_status = status(pulls, Some(PrNumber(1)), &clean);
        let head = initial_status.analysis.pull_requests[&PrNumber(1)]
            .head
            .clone();
        let rewritten_oid = CommitOid("rewritten0000000000000000000000000000000".to_owned());
        let plan = crate::physical_rebase::RebasePlan {
            pr: PrNumber(1),
            branch: head.name.clone(),
            old_head_oid: head.oid.clone(),
            old_base_oid: initial_status.analysis.fleet.default_branch.oid.clone(),
            range_source: crate::physical_rebase::PlannedRangeBase::RemoteBranch {
                branch: initial_status.analysis.fleet.default_branch.clone(),
            },
            new_base: crate::physical_rebase::PlannedBase::Remote(
                initial_status.analysis.fleet.default_branch.clone(),
            ),
            new_head_oid: rewritten_oid.clone(),
            new_tree_oid: CommitOid("tree000000000000000000000000000000000000".to_owned()),
            commit_count: 1,
            merge_topology: None,
            ci_trigger_workflows: vec!["CI".to_owned()],
            lease: format!("refs/heads/{}:{}", head.name, head.oid),
            already_satisfied: false,
        };
        let mut progress = SyncProgress::new(&initial_status, vec![PrNumber(1)], u32::MAX);
        invalidate_rewritten_force_intents(&initial_status, &provider, &[plan], &mut progress)
            .expect("old-generation force is consumed");

        let rewritten = {
            let mut provider_pulls = provider.pulls.borrow_mut();
            let rewritten = provider_pulls.get_mut(&PrNumber(1)).expect("head");
            rewritten.head.oid = rewritten_oid;
            rewritten.checks.clear();
            rewritten.labels.insert("caravan-force".to_owned());
            rewritten.clone()
        };
        let rewritten_status = status(vec![rewritten], Some(PrNumber(1)), &clean);

        let progress = execute(&rewritten_status, &provider, false, false, true)
            .expect("fresh force label on exact rewritten generation is accepted");

        assert_eq!(progress.ci[0].disposition, CiDisposition::Forced);
        assert_eq!(
            progress.current[&PrNumber(1)].state,
            PullRequestState::Merged
        );
        assert_eq!(
            *provider.calls.borrow(),
            vec![
                MutationKind::RemoveLabel,
                MutationKind::Comment,
                MutationKind::Comment,
                MutationKind::SquashMerge,
            ]
        );
    }

    #[test]
    fn forced_head_bypasses_queued_expected_in_progress_and_empty_checks() {
        for checks in [
            vec![check("build-test", CheckState::Queued, Some(40))],
            vec![check("build-test", CheckState::Expected, None)],
            vec![check("build-test", CheckState::InProgress, Some(41))],
            vec![],
        ] {
            let mut pulls = healthy_chain();
            pulls.truncate(1);
            pulls[0].labels.insert("caravan-force".to_owned());
            pulls[0].checks = checks;
            let provider = FakeProvider::with_pull_requests(pulls.clone());
            let status = status(pulls, Some(PrNumber(1)), &clean);

            let progress = execute(&status, &provider, false, false, true)
                .expect("explicit force bypasses every non-successful CI state");

            assert_eq!(progress.ci[0].disposition, CiDisposition::Forced);
            assert_eq!(
                *provider.calls.borrow(),
                vec![MutationKind::Comment, MutationKind::SquashMerge]
            );
            assert_eq!(
                progress.current[&PrNumber(1)].state,
                PullRequestState::Merged
            );
        }
    }

    #[test]
    fn forced_head_bypasses_mixed_pending_and_failed_checks_with_accurate_audit() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![
            check("build-test", CheckState::Failure, Some(42)),
            check("security", CheckState::InProgress, Some(43)),
        ];
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let progress = execute(&status, &provider, false, false, true)
            .expect("explicit force bypasses mixed pending and failed checks");

        assert_eq!(progress.ci[0].disposition, CiDisposition::Forced);
        let audits = provider.audits.borrow();
        assert_eq!(audits.len(), 1);
        assert!(audits[0].reason.contains("observed checks"));
        assert!(audits[0].reason.contains("INPROGRESS"));
        assert!(audits[0].reason.contains("FAILURE"));
        assert!(!audits[0].reason.contains("failed checks:"));
    }

    #[test]
    fn passing_checks_with_stale_force_label_use_normal_auto_merge() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![check("build-test", CheckState::Success, Some(44))];
        pulls[0].auto_merge = AutoMergeState::disabled();
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let progress = execute(&status, &provider, false, false, true)
            .expect("successful CI does not invoke exceptional force");

        assert_eq!(progress.ci[0].disposition, CiDisposition::Passing);
        assert_eq!(
            *provider.calls.borrow(),
            vec![MutationKind::EnableAutoMerge]
        );
        assert!(progress.events.is_empty());
        assert!(provider.audits.borrow().is_empty());
    }

    #[test]
    fn successful_force_merge_is_one_shot_and_advances_child() {
        let mut pulls = healthy_chain();
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(50))];
        pulls[1].labels.insert("caravan-force".to_owned());
        pulls[1].checks = vec![check("build-test", CheckState::Failure, Some(51))];
        pulls[2].checks = vec![check("build-test", CheckState::Success, Some(52))];
        let head_run = failed_run(50, &pulls[0]);
        let child_run = failed_run(51, &pulls[1]);
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider.failed_runs.borrow_mut().extend([
            (PrNumber(1), vec![head_run]),
            (PrNumber(2), vec![child_run]),
        ]);
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let progress =
            execute(&status, &provider, false, false, true).expect("force merge succeeds");

        assert_eq!(
            progress.current[&PrNumber(1)].state,
            PullRequestState::Merged
        );
        assert_eq!(progress.current[&PrNumber(2)].state, PullRequestState::Open);
        assert_eq!(progress.current[&PrNumber(2)].base.name, "main");
        assert_eq!(
            progress.current[&PrNumber(2)].auto_merge,
            AutoMergeState::squash()
        );
        assert_eq!(
            *provider.calls.borrow(),
            vec![
                MutationKind::Comment,
                MutationKind::SquashMerge,
                MutationKind::SetBase,
                MutationKind::EnableAutoMerge,
            ]
        );
        assert_eq!(progress.head_advancements[0].new_head, PrNumber(2));
        assert_eq!(
            progress
                .events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![
                EventKind::ForceMergeAttempted,
                EventKind::ForceMergeCompleted,
                EventKind::HeadAdvanced,
            ]
        );
        assert_eq!(
            progress.events[0].operation_id,
            progress.events[1].operation_id
        );
    }

    #[test]
    fn dead_owner_recovery_is_preserved_on_later_sync_error() {
        let recovery = OperationLockRecovery {
            path: ".git/caravan/operation.lock".to_owned(),
            removed_owner: crate::operation_lock::OperationLockOwner {
                version: 1,
                pid: 99,
                operation: "sync_decision_checkout".to_owned(),
                created_unix_secs: 1,
                token: "exact-token".to_owned(),
                checkpoint: Some(crate::operation_lock::OperationLockCheckpoint {
                    phase: "decision_checkout_in_flight".to_owned(),
                    updated_unix_ms: 2,
                    evidence: json!({ "pr": 2008 }),
                    provider_state_indeterminate: false,
                }),
            },
            age_secs: 3,
            owner_alive: false,
            token_verified: true,
        };
        let error = AppError::validation("repository_not_initialized", "repair init");

        let error = attach_lock_recovery(error, Some(&recovery));

        let details = error.details().unwrap();
        assert_eq!(
            details["lock_recovery"]["removed_owner"]["token"],
            "exact-token"
        );
        assert_eq!(
            details["lock_recovery"]["removed_owner"]["checkpoint"]["phase"],
            "decision_checkout_in_flight"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn sync_lock_checkpoint_stays_bounded_for_large_fleet_receipts() {
        let pulls = healthy_chain();
        let status = status(pulls.clone(), Some(PrNumber(1)), &clean);
        let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], u32::MAX);
        let template = pulls[0].clone();
        for index in 0..1_000_u64 {
            let pr = PrNumber(index + 1);
            progress.steps.push(MutationStep {
                kind: MutationKind::SetBase,
                state: MutationStepState::Completed,
                pr: Some(pr),
                summary: "large historical step evidence ".repeat(32),
            });
            progress
                .rebase_plans
                .push(crate::physical_rebase::RebasePlan {
                    pr,
                    branch: format!("feature-{index}"),
                    old_head_oid: CommitOid(format!("old-{index}")),
                    old_base_oid: CommitOid(format!("base-{index}")),
                    range_source: crate::physical_rebase::PlannedRangeBase::RemoteBranch {
                        branch: branch(&format!("base-{index}")),
                    },
                    new_base: crate::physical_rebase::PlannedBase::Remote(branch(&format!(
                        "target-{index}"
                    ))),
                    new_head_oid: CommitOid(format!("new-{index}")),
                    new_tree_oid: CommitOid(format!("tree-{index}")),
                    commit_count: 1,
                    merge_topology: None,
                    ci_trigger_workflows: (0..32)
                        .map(|workflow| format!(".github/workflows/{workflow}.yml"))
                        .collect(),
                    lease: format!("--force-with-lease=refs/heads/feature-{index}:old-{index}"),
                    already_satisfied: false,
                });
            progress
                .rebase_receipts
                .push(crate::physical_rebase::RebaseReceipt {
                    pr,
                    branch: format!("feature-{index}"),
                    old_head_oid: CommitOid(format!("old-{index}")),
                    new_head_oid: CommitOid(format!("new-{index}")),
                    old_base_oid: CommitOid(format!("base-{index}")),
                    new_base_branch: format!("target-{index}"),
                    new_base_oid: CommitOid(format!("target-oid-{index}")),
                    new_tree_oid: CommitOid(format!("tree-{index}")),
                    commit_count: 1,
                    merge_topology: None,
                    ci_trigger_workflows: Vec::new(),
                    lease: format!("--force-with-lease=refs/heads/feature-{index}:old-{index}"),
                    already_satisfied: false,
                });
            let mut after = template.clone();
            after.number = pr;
            after.labels = (0..64).map(|label| format!("label-{label}")).collect();
            after.checks = (0..64)
                .map(|check| CheckSnapshot {
                    name: format!("check-{check}"),
                    state: CheckState::Success,
                    provider_state: Some("SUCCESS".to_owned()),
                    details_url: None,
                })
                .collect();
            progress.provider_receipts.push(GitHubMutationReceipt {
                kind: MutationKind::SetBase,
                before: Some(after.clone()),
                after,
                provider_output: Some("large provider output".repeat(64)),
            });
            progress.events.push(progress.event(
                EventKind::HeadAdvanced,
                Some(pr),
                vec![pr; 64],
                Some("large event reason".repeat(64)),
                BTreeMap::new(),
            ));
        }

        let evidence = sync_checkpoint_evidence(&progress);
        let encoded = serde_json::to_vec(&evidence).unwrap();

        assert!(encoded.len() < 12 * 1024, "{} bytes", encoded.len());
        assert_eq!(evidence["schema_version"], 2);
        for key in [
            "affected_prs",
            "steps",
            "rebase_plans",
            "rebase_receipts",
            "provider_receipts",
            "events",
        ] {
            assert_eq!(evidence[key]["count"], 1_000, "{key}");
            assert_eq!(evidence[key]["sample"].as_array().unwrap().len(), 4);
            assert_eq!(evidence[key]["truncated"], 996);
            assert!(
                evidence[key]["hash"]
                    .as_str()
                    .unwrap()
                    .starts_with("fnv1a64:")
            );
        }
        assert_eq!(evidence["rebase_plans"]["sample"][0]["pr"], 1);
        assert_eq!(evidence["rebase_plans"]["sample"][3]["pr"], 1_000);
    }

    #[test]
    fn whole_sync_budget_uses_the_explicit_validated_wall_clock_bound() {
        let mut context = AppContext::default();
        assert_eq!(sync_operation_budget(&context), Duration::from_secs(120));
        context.config.sync.max_duration_secs = 10;
        assert_eq!(sync_operation_budget(&context), Duration::from_secs(10));
        context.config.sync.max_duration_secs = 3_600;
        assert_eq!(sync_operation_budget(&context), Duration::from_secs(3_600));
    }

    #[test]
    fn decision_checkout_targets_the_repair_pr_only_when_unambiguous() {
        let decision = |kind, affected_prs| DecisionPoint {
            kind,
            operation_id: OperationId::new(),
            repository: repository(),
            caravan_id: Some(PrNumber(1)),
            affected_prs,
            message: "repair".to_owned(),
            evidence: BTreeMap::new(),
            completed_steps: Vec::new(),
            resumable: true,
            suggested_actions: Vec::new(),
        };

        assert_eq!(
            decision_checkout_target(&decision(DecisionKind::HeadConflict, vec![PrNumber(1)])),
            Some(PrNumber(1))
        );
        assert_eq!(
            decision_checkout_target(&decision(
                DecisionKind::LinkConflict,
                vec![PrNumber(1), PrNumber(2)]
            )),
            Some(PrNumber(2))
        );
        assert_eq!(
            decision_checkout_target(&decision(
                DecisionKind::CrossCaravanConflict,
                vec![PrNumber(1), PrNumber(4)]
            )),
            None
        );
    }

    #[test]
    fn unsafe_decision_checkout_preserves_the_original_decision_error() {
        let temp = tempfile::tempdir().unwrap();
        let context = AppContext {
            repository_path: temp.path().to_path_buf(),
            config_path: temp.path().join("config.yaml"),
            config_existed: false,
            config: crate::config::CaravanConfig::default(),
        };
        let pull_request = pull_request(
            1,
            "one",
            "main",
            PullRequestState::Open,
            AutoMergeState::squash(),
        );
        let decision = DecisionPoint {
            kind: DecisionKind::CiFailure,
            operation_id: OperationId::new(),
            repository: repository(),
            caravan_id: Some(PrNumber(1)),
            affected_prs: vec![PrNumber(1)],
            message: "repair".to_owned(),
            evidence: BTreeMap::from([("pull_request".to_owned(), json!(pull_request))]),
            completed_steps: Vec::new(),
            resumable: true,
            suggested_actions: Vec::new(),
        };
        let error = AppError::structured(
            ErrorCategory::Validation,
            "ci_failure",
            "repair",
            Some(json!({ "decision": decision })),
        );

        let error = checkout_for_decision(&context, error, Instant::now() + Duration::from_secs(1));

        assert_eq!(error.code(), "ci_failure");
        let details = error.details().unwrap();
        assert_eq!(details["checkout"]["state"], "skipped");
        assert_eq!(details["checkout"]["pr"], 1);
        assert_eq!(
            details["checkout"]["error"]["code"],
            "git_repository_not_found"
        );
    }

    #[test]
    fn repeated_healthy_sync_is_a_noop_with_explicit_steps() {
        let pulls = healthy_chain();
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(2)), &clean);

        let progress = execute(&status, &provider, false, false, false).expect("sync converges");

        assert!(!progress.operation_receipt().changed);
        assert!(provider.calls.borrow().is_empty());
        assert_eq!(progress.synchronized_caravans, vec![PrNumber(1)]);
        assert_eq!(progress.steps.len(), 4);
    }

    #[test]
    fn merged_head_advances_child_and_rolls_caravan_id() {
        let pulls = vec![
            pull_request(
                1,
                "one",
                "main",
                PullRequestState::Merged,
                AutoMergeState::disabled(),
            ),
            pull_request(
                2,
                "two",
                "one",
                PullRequestState::Open,
                AutoMergeState::disabled(),
            ),
            pull_request(
                3,
                "three",
                "two",
                PullRequestState::Open,
                AutoMergeState::disabled(),
            ),
        ];
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(2)), &clean);

        let progress =
            execute(&status, &provider, false, false, false).expect("advancement converges");

        assert!(progress.operation_receipt().changed);
        assert_eq!(
            progress.head_advancements,
            vec![HeadAdvancement {
                merged_predecessor: PrNumber(1),
                new_head: PrNumber(2),
                previous_caravan_id: PrNumber(1),
                new_caravan_id: PrNumber(2),
            }]
        );
        let pulls = provider.pulls.borrow();
        assert_eq!(pulls[&PrNumber(2)].base.name, "main");
        assert_eq!(pulls[&PrNumber(2)].auto_merge, AutoMergeState::squash());
        assert!(!pulls[&PrNumber(3)].auto_merge.enabled);
    }

    #[test]
    fn interrupted_advancement_reports_receipt_and_rerun_resumes() {
        let pulls = vec![
            pull_request(
                1,
                "one",
                "main",
                PullRequestState::Merged,
                AutoMergeState::disabled(),
            ),
            pull_request(
                2,
                "two",
                "one",
                PullRequestState::Open,
                AutoMergeState::disabled(),
            ),
        ];
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        provider.fail_once(MutationKind::EnableAutoMerge);
        let initial = status(pulls, Some(PrNumber(2)), &clean);

        let error = execute(&initial, &provider, false, false, false).expect_err("enable fails");
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["operation_receipt"]["changed"], true);
        assert_eq!(provider.pulls.borrow()[&PrNumber(2)].base.name, "main");

        let resumed_pulls: Vec<_> = provider.pulls.borrow().values().cloned().collect();
        let resumed = status(resumed_pulls, Some(PrNumber(2)), &clean);
        let progress = execute(&resumed, &provider, false, false, false).expect("rerun resumes");
        assert!(progress.operation_receipt().changed);
        assert_eq!(
            provider.pulls.borrow()[&PrNumber(2)].auto_merge,
            AutoMergeState::squash()
        );
    }

    #[test]
    fn head_conflict_stops_before_mutation_with_exact_evidence() {
        let conflict = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: CompatibilityOutcome::Conflict,
                conflicting_paths: vec!["src/lib.rs".to_owned()],
                diagnostic: Some("merge-tree conflict".to_owned()),
            })
        };
        let pulls = healthy_chain();
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &conflict);

        let error = execute(&status, &provider, false, false, false).expect_err("conflict decides");

        assert_eq!(mcp_cli::StructuredError::code(&error), "head_conflict");
        assert!(provider.calls.borrow().is_empty());
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["decision"]["affected_prs"], json!([1]));
        assert_eq!(
            details["decision"]["evidence"]["compatibility"][0]["conflicting_paths"],
            json!(["src/lib.rs"])
        );
    }

    #[test]
    fn caravan_force_never_bypasses_textual_conflict() {
        let conflict = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: CompatibilityOutcome::Conflict,
                conflicting_paths: vec!["src/conflict.rs".to_owned()],
                diagnostic: None,
            })
        };
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = vec![check("build-test", CheckState::Expected, None)];
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &conflict);

        let error = execute(&status, &provider, false, false, true)
            .expect_err("force cannot bypass conflict");

        assert_eq!(mcp_cli::StructuredError::code(&error), "head_conflict");
        assert!(provider.calls.borrow().is_empty());
    }

    #[test]
    fn mutation_budget_stops_before_the_next_provider_write() {
        let pulls = healthy_chain();
        let status = status(pulls.clone(), Some(PrNumber(1)), &clean);
        let provider = FakeProvider::with_pull_requests(pulls);
        let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], 1);
        progress.steps.push(MutationStep {
            kind: MutationKind::Comment,
            state: MutationStepState::Completed,
            pr: Some(PrNumber(1)),
            summary: "prior mutation".to_owned(),
        });

        let error = progress
            .ensure_auto_merge_disabled(&provider, &repository(), PrNumber(1))
            .expect_err("budget exhaustion must precede a provider write");

        assert_eq!(error.code(), "sync_mutation_budget_exhausted");
        assert!(provider.calls.borrow().is_empty());
        assert_eq!(error.details().unwrap()["used"], 1);
    }

    #[test]
    fn mutation_timeout_preserves_category_and_completed_steps() {
        let pulls = healthy_chain();
        let status = status(pulls, Some(PrNumber(1)), &clean);
        let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], u32::MAX);
        progress.steps.push(MutationStep {
            kind: MutationKind::SetBase,
            state: MutationStepState::Completed,
            pr: Some(PrNumber(1)),
            summary: "base advanced".to_owned(),
        });
        let error = mutation_error(
            &MutationError::Provider(DiscoveryError::Runner(CommandRunError::Timeout {
                command: crate::command::CommandSpec::new("gh").args(["pr", "merge"]),
                process_group_id: None,
                timeout_ms: 1_200,
                stdout: "partial".to_owned(),
                stderr: "stalled".to_owned(),
            })),
            &progress,
            Some(PrNumber(1)),
        );

        assert_eq!(
            mcp_cli::StructuredError::category(&error),
            ErrorCategory::Timeout
        );
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "github_mutation_timeout"
        );
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["timeout_ms"], 1_200);
        assert_eq!(
            details["operation_receipt"]["completed_steps"][0]["summary"],
            "base advanced"
        );
    }

    #[test]
    fn stale_provider_facts_stop_with_a_resumable_decision() {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].auto_merge = AutoMergeState::disabled();
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);
        provider
            .pulls
            .borrow_mut()
            .get_mut(&PrNumber(1))
            .unwrap()
            .labels
            .insert("external-change".to_owned());

        let error = execute(&status, &provider, false, false, false).expect_err("race stops");

        assert_eq!(mcp_cli::StructuredError::code(&error), "stale_precondition");
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert_eq!(details["decision"]["kind"], "stale_precondition");
        assert_eq!(details["decision"]["resumable"], true);
        assert_eq!(
            details["decision"]["evidence"]["changed_fields"],
            json!(["fake_race"])
        );
    }

    #[test]
    fn sync_all_processes_caravans_in_head_number_order() {
        let pulls = vec![
            pull_request(
                10,
                "ten",
                "main",
                PullRequestState::Open,
                AutoMergeState::disabled(),
            ),
            pull_request(
                2,
                "two",
                "main",
                PullRequestState::Open,
                AutoMergeState::disabled(),
            ),
        ];
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, None, &clean);

        let progress = execute(&status, &provider, true, false, false).expect("all converges");

        assert_eq!(
            progress.synchronized_caravans,
            vec![PrNumber(2), PrNumber(10)]
        );
        assert_eq!(
            progress
                .provider_receipts
                .iter()
                .map(|receipt| receipt.after.number)
                .collect::<Vec<_>>(),
            vec![PrNumber(2), PrNumber(10)]
        );
    }

    #[test]
    fn adjacent_conflict_is_a_link_decision() {
        let checker = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: if target.name == "main" {
                    CompatibilityOutcome::Clean
                } else {
                    CompatibilityOutcome::Conflict
                },
                conflicting_paths: vec!["src/link.rs".to_owned()],
                diagnostic: None,
            })
        };
        let pulls = healthy_chain();
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(2)), &checker);

        let error = execute(&status, &provider, false, false, false).expect_err("link decides");

        assert_eq!(mcp_cli::StructuredError::code(&error), "link_conflict");
        let details = error.details().expect("decision details");
        assert_eq!(
            details["decision"]["evidence"]["rebase_on_join"]["state"],
            "disabled"
        );
        assert!(
            details["decision"]["message"]
                .as_str()
                .expect("message")
                .contains("rebase_on_join=disabled")
        );
        assert!(
            details["decision"]["suggested_actions"][0]
                .as_str()
                .expect("action")
                .contains("rebase_on_join: true")
        );
        assert!(provider.calls.borrow().is_empty());
    }

    #[test]
    fn sync_all_skips_paused_caravan_and_progresses_independent_caravan() {
        let mut pulls = healthy_chain();
        pulls[0].auto_merge = AutoMergeState::disabled();
        pulls[2].base = branch("main");
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let mut status = status(pulls, Some(PrNumber(1)), &clean);
        let head = status.analysis.pull_requests[&PrNumber(1)].clone();
        let record = crate::pause::PauseRecord {
            version: 1,
            caravan_head: PrNumber(1),
            members: vec![PrNumber(1), PrNumber(2)],
            expected_head: {
                let mut expected = PullRequestPrecondition::from(&head);
                expected.auto_merge = AutoMergeState::squash();
                expected
            },
            expected_checks: head.checks.clone(),
            actor: "oncall".to_owned(),
            reason: "incident".to_owned(),
            paused_unix_secs: 1,
            expires_unix_secs: None,
            external_reference: Some("INC-1".to_owned()),
            resume_authorized_by: None,
        };
        status.pauses.push(crate::pause::PauseStatus {
            record,
            state: crate::pause::PauseState::Active,
            auto_merge_suspended: true,
            safe_next_action: "explicit resume".to_owned(),
        });
        status.analysis.fleet.problems.retain(|problem| {
            !(problem.kind == GraphProblemKind::AutoMergeInvariant
                && problem.prs == vec![PrNumber(1)])
        });

        let progress = execute(&status, &provider, true, false, false)
            .expect("independent caravan progresses");

        assert_eq!(progress.synchronized_caravans, vec![PrNumber(3)]);
        assert_eq!(progress.paused_caravans.len(), 1);
        assert_eq!(
            *provider.calls.borrow(),
            vec![MutationKind::EnableAutoMerge]
        );
        assert!(
            progress
                .steps
                .iter()
                .any(|step| step.summary.contains("intentionally paused"))
        );
    }

    #[test]
    fn externally_enabled_non_head_is_disabled_before_head_repair() {
        let mut pulls = healthy_chain();
        pulls[0].auto_merge = AutoMergeState::disabled();
        pulls[1].auto_merge = AutoMergeState::squash();
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);

        execute(&status, &provider, false, false, false).expect("sync repairs shape");

        assert_eq!(
            *provider.calls.borrow(),
            vec![
                MutationKind::DisableAutoMerge,
                MutationKind::EnableAutoMerge
            ]
        );
    }
}
