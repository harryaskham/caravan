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

mod decision;
mod plan;
#[cfg(test)]
use decision::decision_checkout_target;
use decision::{
    attach_scheduler_failure, checkout_for_decision, scheduler_failure_status,
    successful_scheduler_status, sync_failed_event,
};
pub use plan::plan_sync;
#[cfg(test)]
use plan::{plan_auto_admission_with_checker, plan_caravan_convergence};

const MAX_SYNC_OPERATION_SECS: u64 = 3_600;
const MAX_PARALLEL_REBASE_CHAINS: usize = 2;
const AUTO_ADMISSION_SKIP_LABEL: &str = "caravan-join-skipped";
const AUTO_ADMISSION_SKIP_PREFIX: &str = "<!-- caravan-auto-join-skip-receipt:";
const MAX_AUTO_ADMISSION_COMMENT_BYTES: usize = 60 * 1024;
const MAX_RESERVED_CANDIDATE_BUDGET_SECS: u64 = 30;
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
    /// Minimum wall-clock budget reserved before starting exact candidate Git work.
    pub candidate_budget_reserved_ms: u64,
    /// Wall-clock budget still available at the end of this admission phase.
    pub candidate_budget_remaining_ms: u64,
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
            candidate_budget_reserved_ms: 0,
            candidate_budget_remaining_ms: 0,
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
            candidate_budget_reserved_ms: duration_millis(reserved_candidate_budget(context)),
            candidate_budget_remaining_ms: 0,
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
    validate_rebase_preflight_graph(status, &selected, &progress, context.config.force_merge)?;
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
    force_merge: bool,
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
        let deferred_force_head = force_head_auto_merge_gap(status, problem, force_merge)
            .is_some_and(|number| {
                !selected
                    .iter()
                    .any(|caravan| caravan.members.contains(&number))
            });
        if auto_merge || advancement || rebase || deferred_force_head {
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
    if let Some(problem) =
        first_blocking_completion_problem(&final_status, &progress, context.config.force_merge)
    {
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
            if let Some(problem) = first_blocking_completion_problem(
                &final_status,
                &progress,
                context.config.force_merge,
            ) {
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

fn reserved_candidate_budget(context: &AppContext) -> Duration {
    Duration::from_secs(
        context
            .config
            .command_timeout_secs
            .clamp(1, MAX_RESERVED_CANDIDATE_BUDGET_SECS),
    )
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
        candidate_budget_reserved_ms: duration_millis(reserved_candidate_budget(context)),
        candidate_budget_remaining_ms: duration_millis(
            operation_deadline.saturating_duration_since(Instant::now()),
        ),
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
        let remaining = operation_deadline.saturating_duration_since(Instant::now());
        output.candidate_budget_remaining_ms = duration_millis(remaining);
        if remaining < reserved_candidate_budget(context) {
            output.continuation = AutoAdmissionContinuation::DeadlineExhausted;
            break;
        }
        output.candidates_considered += 1;
        let evaluation = evaluate_auto_candidate(&status, &candidate, &checker)?;

        let mut admitted_this_iteration = false;
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
                status.clone(),
                candidate.number,
                target_tail,
                candidate_order.priority_label,
                operation_deadline,
                github_budget,
            )?;
            append_membership_progress(progress, &membership);
            admitted_this_iteration = true;
            output.joins.push(AutoAdmissionJoinReceipt {
                candidate_pr: candidate.number,
                target_tail,
                membership,
            });
        }

        let refresh_deadline = if admitted_this_iteration {
            std::cmp::max(
                operation_deadline,
                Instant::now() + Duration::from_secs(context.config.command_timeout_secs),
            )
        } else {
            operation_deadline
        };
        status =
            read::status_with_deadline_and_budget(context, refresh_deadline, Some(github_budget))?;
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
    output.candidate_budget_remaining_ms =
        duration_millis(operation_deadline.saturating_duration_since(Instant::now()));
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
    validate_graph(status, &caravans, &progress, force_merge)?;

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
        "candidate_budget_reserved_ms": output.candidate_budget_reserved_ms,
        "candidate_budget_remaining_ms": output.candidate_budget_remaining_ms,
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
    force_merge: bool,
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
        let deferred_force_head = force_head_auto_merge_gap(status, problem, force_merge)
            .is_some_and(|number| {
                !selected
                    .iter()
                    .any(|caravan| caravan.members.contains(&number))
            });
        if correctable_auto_merge || correctable_advancement || deferred_force_head {
            continue;
        }
        return Err(decision_error(
            &decision_for_problem(problem, status, progress),
            progress,
        ));
    }
    Ok(())
}

/// A force-labelled head with native auto-merge disabled is an explicit,
/// repairable force-policy state, but only while the configured force path is
/// available. Non-squash auto-merge and every structural problem remain fatal.
fn force_head_auto_merge_gap(
    status: &StatusOutput,
    problem: &GraphProblem,
    force_merge: bool,
) -> Option<PrNumber> {
    if !force_merge || problem.kind != GraphProblemKind::AutoMergeInvariant {
        return None;
    }
    let [number] = problem.prs.as_slice() else {
        return None;
    };
    let pull_request = status.analysis.pull_requests.get(number)?;
    let caravan = status.analysis.fleet.containing(*number)?;
    (caravan.head() == Some(*number)
        && pull_request.state == PullRequestState::Open
        && !pull_request.draft
        && pull_request.has_label("caravan-force")
        && !pull_request.auto_merge.enabled)
        .then_some(*number)
}

/// Final rediscovery may retain an unrelated force head which this targeted
/// tick deliberately did not observe or mutate. A selected head (or any head
/// whose non-force CI path was observed) must still converge exactly.
fn first_blocking_completion_problem<'a>(
    status: &'a StatusOutput,
    progress: &SyncProgress,
    force_merge: bool,
) -> Option<&'a GraphProblem> {
    status.analysis.fleet.problems.iter().find(|problem| {
        let Some(number) = force_head_auto_merge_gap(status, problem, force_merge) else {
            return true;
        };
        let observation = progress.ci.iter().find(|item| item.pr == number);
        let attempted = progress.events.iter().any(|event| {
            event.kind == EventKind::ForceMergeAttempted && event.prs.contains(&number)
        });
        let safely_deferred = match observation {
            None => true,
            Some(item) if item.disposition == CiDisposition::Forced => !attempted,
            Some(_) => false,
        };
        !safely_deferred
    })
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

#[allow(clippy::too_many_lines)]
fn mutation_error(
    error: &MutationError,
    progress: &SyncProgress,
    affected_pr: Option<PrNumber>,
) -> AppError {
    if let MutationError::Provider(DiscoveryError::Runner(CommandRunError::OutputLimit {
        command,
        code,
        stdout,
        stderr,
    })) = error
    {
        return AppError::structured(
            ErrorCategory::ExecutionFailure,
            "command_output_limit",
            error.to_string(),
            Some(json!({
                "stage": "github_mutation_output",
                "command": command.display(),
                "exit_code": code,
                "stdout": stdout,
                "stderr": stderr,
                "streams_combined": false,
                "operation_receipt": progress.operation_receipt(),
                "provider_receipts": progress.provider_receipts,
                "events": progress.events,
                "affected_pr": affected_pr,
                "resumable": true,
                "next": "reduce provider output, rediscover, and rerun the same `cara sync` command",
            })),
        );
    }
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
mod tests;
