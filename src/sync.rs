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
    DecisionPoint, EventId, EventKind, GraphProblem, GraphProblemKind, HeadMergeActor,
    MergeCandidateIdentity, MutationKind, MutationStep, MutationStepState, OperationId,
    OperationReceipt, PrNumber, PullRequestPrecondition, PullRequestSnapshot, PullRequestState,
    RepositoryId,
};
use crate::operation_lock::{OperationLock, OperationLockRecovery};
use crate::read::{self, StatusOutput};
use crate::required_runs::{
    self, HeadRunLineage, MissingRequiredRunsProblem, RequiredContextsRead, RequiredRunsClock,
    RequiredRunsInput, RequiredRunsReceipt, RequiredRunsRecovery, RequiredRunsRetrigger,
};
use crate::root_auto_merge::{
    self, ROOT_AUTO_MERGE_ARMING_ATTEMPTS, ROOT_AUTO_MERGE_CONFIRMATION_DELAY,
    ROOT_AUTO_MERGE_CONFIRMATION_READS, RootAutoMergeFailureCause, RootAutoMergeReceipt,
    RootAutoMergeTrigger,
};
use crate::root_merge::{
    self, ExternalAutoMergePolicy, ROOT_MERGE_CONFIRMATION_DELAY, ROOT_MERGE_CONFIRMATION_READS,
    RootMergeAncestry, RootMergeBlock, RootMergeFacts, RootMergeFailureCause, RootMergeGate,
    RootMergeReceipt, RootPromotionFailureCause, RootPromotionReceipt, RootPromotionTrigger,
};
use crate::{AppContext, AppError, CheckInput, SyncInput};

mod budget;
mod decision;
mod plan;
pub mod progress;
pub use budget::{CapacityDefect, CaravanBudgetProjection, SyncBudgetStatus, project_status};
use budget::{
    CapacityGate, ChainCost, MemberCost, PhysicalApplyAdmission, PhysicalCommitBudget,
    admit_physical_prefix, capacity_evidence, capacity_gate, externally_armed_non_roots,
};
#[cfg(test)]
use budget::{
    ReserveScope, admission_capacity, budget_for, chain_costs_from_status, complete_budget,
    gate_for_bound,
};
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
/// Exact remote range/target verification plus one force-with-lease push.
const PHYSICAL_APPLY_COMMAND_SLOTS_PER_PENDING_MEMBER: u64 = 3;
/// A member whose exact cumulative ancestry already holds still revalidates
/// its range and target generations, but never pushes.
const PHYSICAL_APPLY_COMMAND_SLOTS_PER_RETAINED_MEMBER: u64 = 2;
/// Invalidation reserves remove+audit and the complete compensating add+audit
/// path when branch non-publication is later proven.
const PHYSICAL_FORCE_INVALIDATION_COMMAND_SLOTS: u64 = 6;
const PHYSICAL_FORCE_INVALIDATION_MUTATIONS: u64 = 4;
/// One bounded CI observation per member during ordinary reconciliation.
const PHYSICAL_RECONCILIATION_COMMAND_SLOTS_PER_MEMBER: u64 = 1;
/// Root base retarget plus convergent root auto-merge arming per caravan.
const PHYSICAL_RECONCILIATION_COMMAND_SLOTS_PER_CARAVAN: u64 = 2;
/// Mandatory midpoint and final rediscovery after the write barrier.
const PHYSICAL_FIXED_POST_WRITE_COMMAND_SLOTS: u64 = 2;
const AUTO_ADMISSION_SKIP_LABEL: &str = "caravan-join-skipped";
/// Labels whose transition changes whether a PR may be an admitted caravan root
/// at all. Only these gate convergent root arming; other label churn does not.
const CARAVAN_CONTROL_LABELS: [&str; 2] = ["caravan", "caravan-evicted"];
const AUTO_ADMISSION_SKIP_PREFIX: &str = "<!-- caravan-auto-join-skip-receipt:";
const MAX_AUTO_ADMISSION_COMMENT_BYTES: usize = 60 * 1024;
const MAX_RESERVED_CANDIDATE_BUDGET_SECS: u64 = 30;
/// Default bounded wait before an unreported required context is a stall.
const DEFAULT_MISSING_REQUIRED_RUNS_GRACE_SECS: u64 = 300;
/// Stable provenance reason retained on every required-run receipt.
const REQUIRED_RUNS_CONVERGENCE_REASON: &str = "bounded scheduler verification that every required context has reporting run lineage on the exact current head";
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
    /// Members whose required contexts have no reporting run lineage on their
    /// exact current head. A non-empty list always degrades the disposition;
    /// the scheduler is never healthy while a caravan cannot start CI at all.
    #[serde(default)]
    pub missing_required_runs: Vec<MissingRequiredRunsProblem>,
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
    /// The exact target chain already holds every member the configured
    /// deadline can guarantee to drain; the existing prefix keeps draining.
    CaravanBudgetCapacityExhausted,
    /// The configured admission arithmetic yields no enforceable bound, so
    /// joins fail loudly as a defect instead of being quietly gated by a bound
    /// that no drain could ever clear.
    CaravanBudgetCapacityDefect,
    /// The existing fleet is mid-rebuild after a bounded prefix apply, so no
    /// candidate is admitted until a tick converges it.
    RequiresConvergedFleet,
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
    /// Exact capacity refusal evidence when the configured deadline can no
    /// longer guarantee that a larger chain drains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_refusal: Option<CaravanCapacityRefusal>,
}

/// Typed capacity evidence for one refused join.
///
/// The `code` distinguishes ordinary gating (`caravan_budget_capacity_exhausted`,
/// clearable by draining) from a configuration defect
/// (`caravan_budget_capacity_defect`, which no drain can clear).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CaravanCapacityRefusal {
    pub code: String,
    pub candidate_pr: PrNumber,
    pub caravan_id: PrNumber,
    pub caravan_members: u64,
    /// Sound admission bound. Absent exactly when `capacity_defect` explains
    /// why no bound could be enforced; a zero bound is never emitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_admissible_members: Option<u64>,
    /// Typed defect when the configured arithmetic yields no enforceable bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_defect: Option<CapacityDefect>,
    pub configured_deadline_ms: u64,
    pub command_timeout_ms: u64,
    pub safe_next_action: String,
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
            capacity_refusal: None,
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
            capacity_refusal: None,
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

/// Exact bounded-prefix apply admission modelled by one planned tick.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncApplyAdmissionPlan {
    /// Members one tick would apply now, in exact root-to-descendant order.
    #[serde(default)]
    pub admitted_prefix: Vec<PrNumber>,
    /// Verified members the same tick would resume on a later tick.
    #[serde(default)]
    pub deferred_members: Vec<PrNumber>,
    pub required_ms: u64,
    pub complete_graph_required_ms: u64,
    pub configured_deadline_ms: u64,
    /// Sound admission bound; absent exactly when `capacity_defect` is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_admissible_members: Option<u64>,
    /// Typed defect when the configured arithmetic yields no sound bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_defect: Option<CapacityDefect>,
    /// True when ordinary convergence is intentionally left to the next tick.
    pub deferred_convergence: bool,
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
    /// Exact prefix/deferral the same tick would admit against its deadline.
    #[serde(default)]
    pub physical_apply_admission: SyncApplyAdmissionPlan,
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
            "physical_apply_admission": &self.physical_apply_admission,
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
    /// Durable proof that every converged caravan root carries required native
    /// SQUASH auto-merge on its exact current head, with engine provenance.
    /// Only populated under the historical `head_merge_actor="github"` policy.
    #[serde(default)]
    pub root_auto_merge: Vec<RootAutoMergeReceipt>,
    /// Durable proof that every caravan root targets the exact default branch
    /// before any merge is attempted.
    #[serde(default)]
    pub root_promotion: Vec<RootPromotionReceipt>,
    /// Durable proof of every caravan-owned squash merge, including where the
    /// landed content actually reached the default branch.
    #[serde(default)]
    pub root_merge: Vec<RootMergeReceipt>,
    /// Durable per-member proof that every required context has reporting run
    /// lineage on the exact current head, or the typed reason it does not.
    #[serde(default)]
    pub required_runs: Vec<RequiredRunsReceipt>,
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

    /// Whether repository settings permit squash merging at all. A
    /// caravan-owned tick needs exactly this and never native auto-merge.
    fn repository_allows_squash_merge(
        &self,
        repository: &RepositoryId,
    ) -> Result<bool, MutationError>;

    /// Exact current head revision of one repository branch.
    fn branch_head_oid(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<crate::model::CommitOid, MutationError>;

    /// Exact provider comparison used to prove a merge actually landed.
    fn compare_commits(
        &self,
        repository: &RepositoryId,
        base: &crate::model::CommitOid,
        head: &crate::model::CommitOid,
    ) -> Result<crate::generation::CommitRelation, MutationError>;

    /// Exact provider merge commit for one merged pull request, when exposed.
    fn merge_commit_oid(
        &self,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<Option<crate::model::CommitOid>, MutationError>;

    /// Ordinary non-admin squash merge fenced on the exact head. Branch
    /// protection still applies: this is never an administrator bypass.
    fn squash_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError>;

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

    /// Exact protection-declared required contexts for one base branch.
    fn branch_required_contexts(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<RequiredContextsRead, MutationError>;

    /// Check-suite and workflow-run lineage on the exact verified head.
    fn head_run_lineage(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<HeadRunLineage, MutationError>;

    /// Request exactly one existing check suite again on the unchanged head.
    fn rerequest_check_suite(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        check_suite_id: u64,
    ) -> Result<GitHubMutationReceipt, MutationError>;

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

    fn repository_allows_squash_merge(
        &self,
        repository: &RepositoryId,
    ) -> Result<bool, MutationError> {
        self.repository_allows_squash_merge(repository)
    }

    fn branch_head_oid(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<crate::model::CommitOid, MutationError> {
        self.branch_head_oid(repository, branch)
    }

    fn compare_commits(
        &self,
        repository: &RepositoryId,
        base: &crate::model::CommitOid,
        head: &crate::model::CommitOid,
    ) -> Result<crate::generation::CommitRelation, MutationError> {
        self.compare_commits(repository, base, head)
    }

    fn merge_commit_oid(
        &self,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<Option<crate::model::CommitOid>, MutationError> {
        self.merge_commit_oid(repository, number)
    }

    fn squash_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.squash_merge(repository, expected)
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

    fn branch_required_contexts(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<RequiredContextsRead, MutationError> {
        self.branch_required_contexts(repository, branch)
    }

    fn head_run_lineage(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<HeadRunLineage, MutationError> {
        self.head_run_lineage(repository, expected)
    }

    fn rerequest_check_suite(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        check_suite_id: u64,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.rerequest_check_suite(repository, expected, check_suite_id)
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
    /// Leading members admitted for apply this tick, root-to-descendant.
    admitted: usize,
}

impl PreparedChain {
    fn admitted_members(&self) -> impl Iterator<Item = &crate::physical_rebase::PreparedRebase> {
        self.members.iter().take(self.admitted)
    }

    fn admitted_plans(&self) -> impl Iterator<Item = crate::physical_rebase::RebasePlan> + '_ {
        self.admitted_members()
            .map(|prepared| prepared.plan.clone())
    }
}

/// Exact per-member cost inputs taken from approved plans rather than from a
/// whole-chain worst case, so a completed prefix makes the next tick cheaper.
fn chain_costs_from_plans(status: &StatusOutput, chains: &[PreparedChain]) -> Vec<ChainCost> {
    chains
        .iter()
        .map(|chain| ChainCost {
            caravan_id: chain.caravan.id,
            externally_armed_non_roots: externally_armed_non_roots(status, &chain.caravan),
            members: chain
                .members
                .iter()
                .map(|prepared| {
                    let current = status.analysis.pull_requests.get(&prepared.plan.pr);
                    MemberCost {
                        pr: prepared.plan.pr,
                        pending: !prepared.plan.already_satisfied,
                        auto_merge_enabled: current
                            .is_some_and(|pull_request| pull_request.auto_merge.enabled),
                        force_labelled: current
                            .is_some_and(|pull_request| pull_request.has_label("caravan-force")),
                    }
                })
                .collect(),
        })
        .collect()
}

/// Worst-case whole-graph reserve used before any Git range is planned.
#[cfg(test)]
fn physical_commit_budget(
    context: &AppContext,
    status: &StatusOutput,
    selected: &[Caravan],
) -> PhysicalCommitBudget {
    budget::complete_budget(context, &budget::chain_costs_from_status(status, selected))
}

fn physical_sync_budget_error(
    context: &AppContext,
    operation_deadline: Instant,
    budget: PhysicalCommitBudget,
    plans: &[crate::physical_rebase::RebasePlan],
    phase: &'static str,
) -> AppError {
    physical_sync_budget_error_with_admission(
        context,
        operation_deadline,
        budget,
        plans,
        phase,
        None,
    )
}

/// Typed zero-write refusal carrying the exact reserve model, the configured
/// deadline, the prefix that could still have drained, and the capacity bound
/// implied by the configuration.
fn physical_sync_budget_error_with_admission(
    context: &AppContext,
    operation_deadline: Instant,
    budget: PhysicalCommitBudget,
    plans: &[crate::physical_rebase::RebasePlan],
    phase: &'static str,
    admission: Option<&PhysicalApplyAdmission>,
) -> AppError {
    let remaining = operation_deadline.saturating_duration_since(Instant::now());
    let plan_material = serde_json::to_vec(plans).expect("physical plans serialize");
    let deadline = sync_operation_budget(context);
    let (capacity, capacity_defect) = capacity_evidence(context, deadline);
    let mut details = json!({
        "phase": phase,
        "required_ms": duration_millis(budget.required),
        "remaining_ms": duration_millis(remaining),
        "minimum_additional_ms": duration_millis(budget.required.saturating_sub(remaining)),
        "command_timeout_ms": context.config.command_timeout_secs.saturating_mul(1_000),
        "configured_deadline_ms": duration_millis(deadline),
        "required_command_slots": budget.command_slots,
        "worst_case_required_ms": duration_millis(
            crate::sync::budget::slots_to_worst_case_duration(context, budget.command_slots),
        ),
        "reserve_model": "proportional per-command reserve; each command remains bounded by command_timeout_secs and the tick by the operation deadline",
        "required_mutation_capacity": budget.mutation_reserve,
        "max_admissible_members": capacity,
        "capacity_defect": capacity_defect,
        "prepared_plan_count": plans.len(),
        "prepared_plan_hash": crate::membership::fnv1a64(&plan_material),
        "provider_mutations": 0,
        "branch_mutations": 0,
        "retryable": false,
        "config_guidance": "increase sync.max_duration_secs until physical planning completes with required_ms still remaining, or lower sync.reserve_secs_per_command (the per-command price shared by this reserve and the admission bound) only when the provider and Git latency bound supports it; the operation deadline is never extended",
    });
    if let (Some(admission), Some(object)) = (admission, details.as_object_mut()) {
        object.insert(
            "processable_prefix".to_owned(),
            json!(admission.admitted_prs),
        );
        object.insert("deferred_members".to_owned(), json!(admission.deferred));
        object.insert(
            "complete_graph_required_ms".to_owned(),
            json!(duration_millis(admission.complete_budget.required)),
        );
        object.insert(
            "complete_graph_command_slots".to_owned(),
            json!(admission.complete_budget.command_slots),
        );
    }
    AppError::structured(
        ErrorCategory::Validation,
        "physical_sync_budget_insufficient",
        "physical sync cannot enter its mutation phase without the reserved apply budget",
        Some(details),
    )
}

fn physical_precommit_deadline(
    context: &AppContext,
    operation_deadline: Instant,
    budget: PhysicalCommitBudget,
    plans: &[crate::physical_rebase::RebasePlan],
    phase: &'static str,
) -> Result<Instant, AppError> {
    let now = Instant::now();
    let remaining = operation_deadline.saturating_duration_since(now);
    let Some(precommit_deadline) = operation_deadline.checked_sub(budget.required) else {
        return Err(physical_sync_budget_error(
            context,
            operation_deadline,
            budget,
            plans,
            phase,
        ));
    };
    if remaining <= budget.required || now >= precommit_deadline {
        return Err(physical_sync_budget_error(
            context,
            operation_deadline,
            budget,
            plans,
            phase,
        ));
    }
    Ok(precommit_deadline)
}

fn physical_budget_failure(
    context: &AppContext,
    status: &StatusOutput,
    operation_deadline: Instant,
    budget: PhysicalCommitBudget,
    plans: Vec<crate::physical_rebase::RebasePlan>,
    phase: &'static str,
) -> AppError {
    let affected_prs = plans.iter().map(|plan| plan.pr).collect();
    attach_physical_rebuild(
        physical_sync_budget_error(context, operation_deadline, budget, &plans, phase),
        &PhysicalRebuildOutcome {
            repository: Some(status.repository.clone()),
            affected_prs,
            plans,
            ..PhysicalRebuildOutcome::default()
        },
    )
}

/// Refusal for the one case bounded prefixes cannot rescue: not even a single
/// pending member fits the configured deadline.
fn physical_capacity_failure(
    context: &AppContext,
    status: &StatusOutput,
    operation_deadline: Instant,
    admission: &PhysicalApplyAdmission,
    plans: Vec<crate::physical_rebase::RebasePlan>,
    phase: &'static str,
) -> AppError {
    let affected_prs = plans.iter().map(|plan| plan.pr).collect();
    attach_physical_rebuild(
        physical_sync_budget_error_with_admission(
            context,
            operation_deadline,
            admission.budget,
            &plans,
            phase,
            Some(admission),
        ),
        &PhysicalRebuildOutcome {
            repository: Some(status.repository.clone()),
            affected_prs,
            plans,
            ..PhysicalRebuildOutcome::default()
        },
    )
}

#[derive(Default)]
struct PhysicalRebuildOutcome {
    repository: Option<RepositoryId>,
    caravan_id: Option<PrNumber>,
    affected_prs: Vec<PrNumber>,
    plans: Vec<crate::physical_rebase::RebasePlan>,
    /// Approved members intentionally left for a later tick by the bounded
    /// prefix admission. Never a failure: their exact plans stay verified.
    deferred: Vec<PrNumber>,
    receipts: Vec<crate::physical_rebase::RebaseReceipt>,
    provider_receipts: Vec<GitHubMutationReceipt>,
    steps: Vec<MutationStep>,
    force_intent_restorations: Vec<Value>,
}

fn selected_unpaused_caravans(status: &StatusOutput, all: bool) -> Result<Vec<Caravan>, AppError> {
    let mut selected = select_caravans(status, all)?;
    selected.retain(|caravan| {
        !status
            .pauses
            .iter()
            .any(|pause| pause.state.is_effective() && pause.record.caravan_head == caravan.id)
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
) -> Result<(Vec<PreparedChain>, SyncProgress, PhysicalApplyAdmission), AppError> {
    let selected = selected_unpaused_caravans(status, all)?;
    let progress = SyncProgress::new(
        status,
        selected.iter().map(|caravan| caravan.id).collect(),
        context.config.sync.max_mutations_per_tick,
    );
    // Pre-plan sizing is deliberately worst case (every member still pending)
    // and deliberately non-fatal: only real plans know which members already
    // hold their exact cumulative ancestry and therefore cost nothing.
    let projected = admit_physical_prefix(
        context,
        &budget::chain_costs_from_status(status, &selected),
        operation_deadline.saturating_duration_since(Instant::now()),
    );
    let planning_budget = PhysicalCommitBudget {
        command_slots: 1,
        required: Duration::from_secs(context.config.command_timeout_secs),
        mutation_reserve: projected.budget.mutation_reserve,
    };
    let planning_deadline = physical_precommit_deadline(
        context,
        operation_deadline,
        planning_budget,
        &[],
        "physical_rebase_planning",
    )
    .map_err(|error| {
        attach_physical_rebuild(
            error,
            &PhysicalRebuildOutcome {
                repository: Some(status.repository.clone()),
                ..PhysicalRebuildOutcome::default()
            },
        )
    })?;
    progress.ensure_mutation_capacity(projected.budget.mutation_reserve)?;
    if let Err(error) = preflight_repository(provider, status, &progress) {
        if Instant::now() >= planning_deadline {
            return Err(physical_budget_failure(
                context,
                status,
                operation_deadline,
                planning_budget,
                Vec::new(),
                "physical_rebase_repository_preflight",
            ));
        }
        return Err(error);
    }
    let mut precommit_deadline = physical_precommit_deadline(
        context,
        operation_deadline,
        planning_budget,
        &[],
        "physical_rebase_repository_preflight",
    )
    .map_err(|error| {
        attach_physical_rebuild(
            error,
            &PhysicalRebuildOutcome {
                repository: Some(status.repository.clone()),
                ..PhysicalRebuildOutcome::default()
            },
        )
    })?;
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
                let parent = status
                    .analysis
                    .pull_requests
                    .get(&caravan.members[index - 1])
                    .expect("selected parent has provider facts");
                crate::physical_rebase::range_base_for_rewritten_parent(candidate, &parent.head)
            };
            let prepared = match crate::physical_rebase::prepare_candidate(
                &context.repository_path,
                &status.repository,
                candidate,
                range_source,
                target,
                &status.analysis.fleet.default_branch,
                crate::physical_rebase::RebaseExecutionBudget::new(timeout)
                    .with_deadline(precommit_deadline)
                    // bd-85b71d: only the root is squash-merged by Cara, and
                    // only then is its history discarded at landing. A child's
                    // ancestry must still physically follow the chain, so it is
                    // never flattened.
                    .flattening_squashed_root(
                        index == 0
                            && context.config.sync.resolved_head_merge_actor()
                                == crate::model::HeadMergeActor::Caravan,
                    ),
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
                            .collect::<Vec<_>>();
                    if Instant::now() >= precommit_deadline {
                        return Err(physical_budget_failure(
                            context,
                            status,
                            operation_deadline,
                            planning_budget,
                            plans,
                            "physical_rebase_planning",
                        ));
                    }
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
        chains.push(PreparedChain {
            admitted: members.len(),
            caravan,
            members,
        });
    }
    let plans = chains
        .iter()
        .flat_map(|chain| chain.members.iter().map(|item| item.plan.clone()))
        .collect::<Vec<_>>();
    // Every reserve below is derived from the approved plans, so an already
    // applied prefix is free and each resumed tick is strictly cheaper.
    let costs = chain_costs_from_plans(status, &chains);
    let mut admission = admit_physical_prefix(
        context,
        &costs,
        operation_deadline.saturating_duration_since(Instant::now()),
    );
    if !admission.makes_progress() {
        return Err(physical_capacity_failure(
            context,
            status,
            operation_deadline,
            &admission,
            plans,
            "physical_rebase_global_write_barrier",
        ));
    }
    precommit_deadline = physical_precommit_deadline(
        context,
        operation_deadline,
        admission.budget,
        &plans,
        "physical_rebase_global_write_barrier",
    )
    .map_err(|_| {
        physical_budget_failure(
            context,
            status,
            operation_deadline,
            admission.budget,
            plans.clone(),
            "physical_rebase_global_write_barrier",
        )
    })?;
    // The barrier always verifies the complete graph, never only the prefix:
    // a deferred descendant must still be provably appliable before any
    // ancestor of it is rewritten.
    if let Err(error) =
        verify_physical_write_barrier(context, status, provider, &chains, Some(precommit_deadline))
    {
        if Instant::now() >= precommit_deadline {
            return Err(physical_budget_failure(
                context,
                status,
                operation_deadline,
                admission.budget,
                plans,
                "physical_rebase_global_write_barrier",
            ));
        }
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
    // Re-admit against the wall clock the complete-graph barrier actually
    // consumed, so commitment reserves the exact prefix it is about to write.
    admission = admit_physical_prefix(
        context,
        &costs,
        operation_deadline.saturating_duration_since(Instant::now()),
    );
    if !admission.makes_progress() {
        return Err(physical_capacity_failure(
            context,
            status,
            operation_deadline,
            &admission,
            plans,
            "physical_rebase_commit_admission",
        ));
    }
    physical_precommit_deadline(
        context,
        operation_deadline,
        admission.budget,
        &plans,
        "physical_rebase_commit_admission",
    )
    .map_err(|_| {
        physical_budget_failure(
            context,
            status,
            operation_deadline,
            admission.budget,
            plans,
            "physical_rebase_commit_admission",
        )
    })?;
    for (chain, admitted) in chains.iter_mut().zip(admission.admitted.iter().copied()) {
        chain.admitted = admitted;
    }
    Ok((chains, progress, admission))
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
    phase_deadline: Option<Instant>,
) -> Result<(), AppError> {
    let timeout = Duration::from_secs(context.config.command_timeout_secs);
    crate::physical_rebase::verify_branch_snapshot_before(
        &context.repository_path,
        &status.analysis.fleet.default_branch,
        timeout,
        phase_deadline,
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
            crate::physical_rebase::verify_prepared_before(prepared, phase_deadline)?;
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
fn restore_force_intent_after_nonpublication(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    plan: &crate::physical_rebase::RebasePlan,
    progress: &mut SyncProgress,
    outcome: &mut PhysicalRebuildOutcome,
    original_error: AppError,
) -> AppError {
    let Some(original) = status.analysis.pull_requests.get(&plan.pr) else {
        return original_error;
    };
    if plan.already_satisfied || !original.has_label("caravan-force") {
        return original_error;
    }
    let observed = match provider.refetch_pull_request(&status.repository, plan.pr) {
        Ok(observed) => observed,
        Err(error) => {
            outcome.force_intent_restorations.push(json!({
                "pr": plan.pr,
                "state": "indeterminate",
                "old_head_oid": plan.old_head_oid,
                "planned_head_oid": plan.new_head_oid,
                "provider_error": error.to_string(),
                "restored": false,
            }));
            return original_error;
        }
    };
    if observed.head.oid == plan.new_head_oid {
        outcome.force_intent_restorations.push(json!({
            "pr": plan.pr,
            "state": "published",
            "observed_head_oid": observed.head.oid,
            "restored": false,
            "reason": "planned generation is provider-visible; old-generation intent stays invalidated",
        }));
        return original_error;
    }
    if observed.head.oid != plan.old_head_oid {
        outcome.force_intent_restorations.push(json!({
            "pr": plan.pr,
            "state": "indeterminate",
            "old_head_oid": plan.old_head_oid,
            "planned_head_oid": plan.new_head_oid,
            "observed_head_oid": observed.head.oid,
            "restored": false,
        }));
        return original_error;
    }

    progress.current.insert(plan.pr, observed.clone());
    let mut audit_before_labels = observed.labels.clone();
    audit_before_labels.remove("caravan-force");
    let mut after_labels = audit_before_labels.clone();
    after_labels.insert("caravan-force".to_owned());
    let audit = ControlLabelAudit {
        operation: "force_restore_nonpublication".to_owned(),
        marker: control_label_marker(
            "force_restore_nonpublication",
            plan.pr,
            &plan.old_head_oid,
            &audit_before_labels,
            &after_labels,
        ),
        before_labels: audit_before_labels,
        after_labels,
        actor: "cara physical-rebase recovery policy".to_owned(),
        reason: format!(
            "restored caravan-force intent on unchanged old head {} after planned generation {} was proven not published ({})",
            plan.old_head_oid,
            plan.new_head_oid,
            original_error.code(),
        ),
        reason_source: "exact provider non-publication proof after failed branch apply".to_owned(),
        compatibility_evidence: format!(
            "retained physical plan for PR #{} failed before provider exposed the planned head",
            plan.pr
        ),
        clean_squash_evidence:
            "old-generation intent only; any later successful rewrite invalidates it again"
                .to_owned(),
        admission_priority_basis: "not applicable: restoration does not change caravan order"
            .to_owned(),
    };

    let restore = (|| -> Result<(), AppError> {
        if observed.has_label("caravan-force") {
            progress.already(
                MutationKind::AddLabel,
                plan.pr,
                "old-generation force intent already restored",
            );
        } else {
            progress.ensure_mutation_capacity(1)?;
            let receipt = provider
                .add_label(
                    &status.repository,
                    &progress.precondition(plan.pr),
                    "caravan-force",
                )
                .map_err(|error| mutation_error(&error, progress, Some(plan.pr)))?;
            progress.record(
                receipt,
                "restored caravan-force after proven rewrite non-publication",
            );
        }
        progress.ensure_control_label_comment(provider, &status.repository, plan.pr, &audit)
    })();
    match restore {
        Ok(()) => {
            outcome.force_intent_restorations.push(json!({
                "pr": plan.pr,
                "state": "restored",
                "old_head_oid": plan.old_head_oid,
                "planned_head_oid": plan.new_head_oid,
                "observed_head_oid": progress.current.get(&plan.pr).map(|pr| &pr.head.oid),
                "restored": true,
                "audit_marker": audit.marker,
            }));
            original_error
        }
        Err(restore_error) => AppError::structured(
            ErrorCategory::ExecutionFailure,
            "force_intent_restore_failed",
            "rewrite non-publication was proven but old-generation force intent restoration did not complete",
            Some(json!({
                "pr": plan.pr,
                "old_head_oid": plan.old_head_oid,
                "planned_head_oid": plan.new_head_oid,
                "original_error": {
                    "category": original_error.category(),
                    "code": original_error.code(),
                    "message": original_error.message(),
                    "details": original_error.details(),
                },
                "restore_error": {
                    "category": restore_error.category(),
                    "code": restore_error.code(),
                    "message": restore_error.message(),
                    "details": restore_error.details(),
                },
                "completed_steps": progress.steps,
                "provider_receipts": progress.provider_receipts,
                "resumable": true,
                "next": "rediscover the exact old head and rerun sync; restoration audits deduplicate by exact transition",
            })),
        ),
    }
}

#[allow(clippy::too_many_lines)]
/// Bounded provider-consistency retries after an exact force-with-lease push.
///
/// GitHub occasionally serves a PR head that lags a completed push by seconds.
/// Retrying the exact expected OID is not a policy weakening: the lease already
/// proved the write, and any other observed generation still fails closed.
pub(crate) const PROVIDER_HEAD_CONVERGENCE_ATTEMPTS: u32 = 6;
const PROVIDER_HEAD_CONVERGENCE_DELAY: Duration = Duration::from_millis(1_500);

fn refetch_until_exact_head(
    provider: &impl SyncProvider,
    repository: &RepositoryId,
    pr: PrNumber,
    expected: &crate::model::CommitOid,
) -> Result<PullRequestSnapshot, MutationError> {
    let mut observed = provider.refetch_pull_request(repository, pr)?;
    let mut attempt = 1;
    while &observed.head.oid != expected && attempt < PROVIDER_HEAD_CONVERGENCE_ATTEMPTS {
        std::thread::sleep(PROVIDER_HEAD_CONVERGENCE_DELAY);
        observed = provider.refetch_pull_request(repository, pr)?;
        attempt += 1;
    }
    Ok(observed)
}

#[allow(clippy::too_many_lines)]
fn apply_physical_chains(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    chains: &[PreparedChain],
    mut progress: SyncProgress,
    lock: &mut OperationLock,
) -> Result<PhysicalRebuildOutcome, AppError> {
    let plans = chains
        .iter()
        .flat_map(|chain| chain.members.iter().map(|prepared| prepared.plan.clone()))
        .collect::<Vec<_>>();
    let deferred = chains
        .iter()
        .flat_map(|chain| chain.members.iter().skip(chain.admitted))
        .map(|prepared| prepared.plan.pr)
        .collect::<Vec<_>>();
    let mut outcome = PhysicalRebuildOutcome {
        repository: Some(status.repository.clone()),
        caravan_id: if chains.len() == 1 {
            chains.first().map(|chain| chain.caravan.id)
        } else {
            None
        },
        affected_prs: plans.iter().map(|plan| plan.pr).collect(),
        plans,
        deferred,
        ..PhysicalRebuildOutcome::default()
    };
    for chain in chains {
        for prepared in chain.admitted_members() {
            // Only a member whose exact branch generation is actually rewritten
            // needs its native auto-merge dropped first. Disarming an untouched
            // caravan root every tick creates a durability window in which any
            // later failure (CI stop, budget, provider error) leaves required
            // root arming off until an operator notices.
            if prepared.plan.already_satisfied {
                progress.already(
                    MutationKind::DisableAutoMerge,
                    prepared.plan.pr,
                    "exact cumulative ancestry already satisfied; no branch rewrite, so native auto-merge is retained",
                );
                continue;
            }
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
        chains
            .iter()
            .flat_map(PreparedChain::admitted_members)
            .filter(|prepared| !prepared.plan.already_satisfied)
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
    let control_checkpoint = json!({
        "rebase_plans": checkpoint_rebase_plans(&outcome.plans),
        "provider_receipts": checkpoint_provider_receipts(&outcome.provider_receipts),
        "completed_steps": bounded_checkpoint_sequence(
            outcome
                .steps
                .iter()
                .map(|step| serde_json::to_value(step).expect("mutation step serializes"))
                .collect(),
        ),
        "branch_writes": 0,
        "deferred_members": outcome.deferred,
        "next": "apply retained objects under the exact globally verified leases",
    });
    lock.checkpoint(
        "physical_rebase_control_mutations_complete",
        control_checkpoint.clone(),
        false,
    )
    .map_err(|error| attach_physical_rebuild(error, &outcome))?;
    lock.checkpoint(
        "physical_rebase_branch_apply_in_flight",
        control_checkpoint,
        true,
    )
    .map_err(|error| attach_physical_rebuild(error, &outcome))?;
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
        let mut failed_plans = Vec::new();
        for (chain, result) in batch.iter().zip(results) {
            match result {
                Ok((receipts, error)) => {
                    outcome.receipts.extend(receipts);
                    if let Some(error) = error {
                        failed_plans.push(error);
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
        for (plan, error) in failed_plans {
            let error = restore_force_intent_after_nonpublication(
                status,
                provider,
                &plan,
                &mut progress,
                &mut outcome,
                error,
            );
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
        if let Some(error) = first_error {
            outcome
                .provider_receipts
                .clone_from(&progress.provider_receipts);
            outcome.steps.clone_from(&progress.steps);
            return Err(attach_physical_rebuild(error, &outcome));
        }
    }
    for receipt in &outcome.receipts {
        let observed = refetch_until_exact_head(
            provider,
            &status.repository,
            receipt.pr,
            &receipt.new_head_oid,
        )
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
                    Some(json!({
                        "receipt": receipt,
                        "observed_head": observed.head.oid,
                        "expected_head": receipt.new_head_oid,
                        "attempts": PROVIDER_HEAD_CONVERGENCE_ATTEMPTS,
                        "resumable": true,
                        "auto_merge_state": "head auto-merge stays intentionally disabled until a tick revalidates the rewritten generation's fresh CI",
                        "safe_next_action": "rerun the same idempotent sync; the exact pushed generation converges without another rewrite",
                    })),
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
) -> (
    Vec<crate::physical_rebase::RebaseReceipt>,
    Option<(crate::physical_rebase::RebasePlan, AppError)>,
) {
    let mut receipts = Vec::with_capacity(chain.admitted);
    for prepared in chain.admitted_members() {
        match crate::physical_rebase::apply_prepared_after_write_barrier(prepared) {
            Ok(receipt) => receipts.push(receipt),
            Err(error) => return (receipts, Some((prepared.plan.clone(), error))),
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
        // A bounded-prefix refusal already carries the exact deferred set; a
        // later outcome attachment must never blank that evidence.
        object
            .entry("deferred_members".to_owned())
            .or_insert_with(|| json!(outcome.deferred));
        object.insert("rebase_receipts".to_owned(), json!(outcome.receipts));
        object.insert(
            "provider_receipts".to_owned(),
            json!(outcome.provider_receipts),
        );
        object.insert("completed_steps".to_owned(), json!(outcome.steps));
        object.insert(
            "force_intent_restorations".to_owned(),
            json!(outcome.force_intent_restorations),
        );
        object.insert("resumable".to_owned(), json!(true));
        let deterministic_history_decision = matches!(
            error.code().as_str(),
            "rebase_nonlinear_range"
                | "rebase_range_ambiguous"
                | "rebase_empty_patch_range"
                | "rebase_target_history_changed"
                | "rebase_repository_not_owned"
                | "rebase_historical_target_mismatch"
                | "rebase_historical_parent_mismatch"
                | "rebase_historical_source_mismatch"
                | "rebase_unsupported_octopus"
                | "rebase_topology_limit"
                | "rebase_external_merge_parents"
                | "rebase_cousin_history"
                | "rebase_merge_tree_conflict"
                | "rebase_merge_replay_conflict"
                | "rebase_merge_tree_mismatch"
                | "rebase_topology_changed"
        );
        let configuration_decision = error.code() == "physical_sync_budget_insufficient";
        object.insert(
            "next".to_owned(),
            json!(if deterministic_history_decision {
                "the unchanged exact generation cannot succeed by retry: inspect the reported topology and explicitly repair/reshape/evict, use an audited merge-preserving strategy, or change the candidate head before rerunning"
            } else if configuration_decision {
                "increase sync.max_duration_secs enough to retain required_ms after planning (or lower a proven-safe child timeout), then rerun the unchanged command"
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
        } else if configuration_decision {
            object.insert("retryable".to_owned(), json!(false));
            object.insert(
                "suggested_actions".to_owned(),
                json!([
                    "increase sync.max_duration_secs above observed planning time plus required_ms",
                    "lower command_timeout_secs only with a proven provider and Git latency bound",
                    "rerun plan sync before allowing any mutation"
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

/// Successful bounded-prefix tick: an exact verified prefix of the complete
/// approved graph was applied, its receipts are durable, and the remainder
/// resumes on the next tick without replaying any completed provider mutation.
#[allow(clippy::too_many_arguments)]
fn bounded_prefix_output(
    context: &AppContext,
    input: &SyncInput,
    started: Instant,
    operation_deadline: Instant,
    initial_status_elapsed: Duration,
    convergence_elapsed: Duration,
    admission: &PhysicalApplyAdmission,
    physical_rebuild: PhysicalRebuildOutcome,
    status: StatusOutput,
    lock_recovery: Option<OperationLockRecovery>,
    lock: &mut OperationLock,
) -> Result<SyncOutput, AppError> {
    let selected = selected_unpaused_caravans(&status, input.all)?;
    let mut progress = SyncProgress::new(
        &status,
        selected.iter().map(|caravan| caravan.id).collect(),
        context.config.sync.max_mutations_per_tick,
    );
    progress.steps = physical_rebuild.steps;
    progress.provider_receipts = physical_rebuild.provider_receipts;
    progress.rebase_plans = physical_rebuild.plans;
    progress.rebase_receipts = physical_rebuild.receipts;
    progress.paused_caravans = status
        .pauses
        .iter()
        .filter(|pause| pause.state.is_effective())
        .cloned()
        .collect();
    let (checkpoint_capacity, checkpoint_capacity_defect) =
        capacity_evidence(context, sync_operation_budget(context));
    let evidence = json!({
        "admitted_prefix": admission.admitted_prs,
        "deferred_members": admission.deferred,
        "required_ms": duration_millis(admission.budget.required),
        "required_command_slots": admission.budget.command_slots,
        "complete_graph_required_ms": duration_millis(admission.complete_budget.required),
        "complete_graph_command_slots": admission.complete_budget.command_slots,
        "configured_deadline_ms": duration_millis(sync_operation_budget(context)),
        "max_admissible_members": checkpoint_capacity,
        "capacity_defect": checkpoint_capacity_defect,
        "provider_state": sync_checkpoint_evidence(&progress),
    });
    lock.checkpoint("physical_rebase_bounded_prefix_complete", evidence, false)?;
    lock.checkpoint("completed", sync_checkpoint_evidence(&progress), false)?;

    let reason = if admission.deferred.is_empty() {
        format!(
            "applied the complete approved graph ({} member(s)) and deferred ordinary convergence to the next tick; completed receipts are durable and are never replayed",
            admission.admitted_prs.len(),
        )
    } else {
        format!(
            "applied an exact verified prefix of {} member(s) and deferred {} member(s) plus ordinary convergence to the next tick; completed receipts are durable and are never replayed",
            admission.admitted_prs.len(),
            admission.deferred.len(),
        )
    };
    let scheduler_status = SyncSchedulerStatus {
        wake_class: SchedulerWakeClass::RetryTick,
        disposition: SchedulerDisposition::RetryTick,
        reason,
        ..successful_scheduler_status(
            &status,
            &progress.ci,
            &progress.paused_caravans,
            context.config.rebase_on_join,
            &progress.required_runs,
            &progress.missing_required_runs,
        )
    };
    Ok(SyncOutput {
        receipt: progress.operation_receipt(),
        auto_admission: AutoAdmissionOutput {
            continuation: if context.config.sync.actions.join_unlabelled_prs && input.all {
                AutoAdmissionContinuation::RequiresConvergedFleet
            } else {
                AutoAdmissionOutput::disabled(context, input.all).continuation
            },
            mutation_limit: context.config.sync.max_mutations_per_tick,
            mutations_used: completed_mutation_count(&progress),
            ..AutoAdmissionOutput::disabled(context, input.all)
        },
        scheduler_status,
        timing: Some(SyncTiming {
            deadline_ms: duration_millis(operation_deadline.saturating_duration_since(started)),
            total_ms: duration_millis(started.elapsed()),
            initial_status_ms: duration_millis(initial_status_elapsed),
            provider_convergence_ms: duration_millis(convergence_elapsed),
            final_status_ms: 0,
        }),
        lock_recovery,
        provider_receipts: progress.provider_receipts,
        root_auto_merge: Vec::new(),
        root_promotion: progress.root_promotion,
        root_merge: progress.root_merge,
        required_runs: progress.required_runs,
        rebase_plans: progress.rebase_plans,
        rebase_receipts: progress.rebase_receipts,
        historical_predecessor: read::historical_predecessor(&status),
        synchronized_caravans: progress.synchronized_caravans,
        paused_caravans: progress.paused_caravans,
        head_advancements: Vec::new(),
        ci: Vec::new(),
        events: Vec::new(),
        hook_deliveries: Vec::new(),
        status,
    })
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
    progress::emit(
        "initial_discovery",
        "reading exact provider graph, checks, and admission order",
    );
    let mut status =
        read::status_with_deadline_and_budget(context, operation_deadline, Some(&github_budget))?;
    let initial_status_elapsed = initial_status_started.elapsed();
    progress::emit(
        "initial_discovery",
        format!(
            "discovered {} caravan(s), {} unqueued PR(s) in {}ms",
            status.analysis.fleet.caravans.len(),
            status.analysis.fleet.unqueued.len(),
            duration_millis(initial_status_elapsed),
        ),
    );
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
        let (prepared, progress_state, admission) =
            prepare_physical_chains(context, &status, input.all, &provider, operation_deadline)?;
        progress::emit(
            "physical_rebase_planning",
            format!(
                "planned {} chain(s); admitted prefix {:?}, deferred {}",
                prepared.len(),
                admission.admitted_prs,
                admission.deferred.len(),
            ),
        );
        let progress = progress_state;
        let plans = prepared
            .iter()
            .flat_map(|chain| chain.members.iter().map(|item| item.plan.clone()))
            .collect::<Vec<_>>();
        // Control mutations follow the admitted prefix exactly: a deferred
        // member keeps its exact-generation force intent because nothing has
        // rewritten the generation that intent is bound to.
        let admitted_plans = prepared
            .iter()
            .flat_map(PreparedChain::admitted_plans)
            .collect::<Vec<_>>();
        lock.checkpoint(
            "physical_rebase_global_preflight_complete",
            json!({
                "rebase_plans": checkpoint_rebase_plans(&plans),
                "admitted_prefix": admission.admitted_prs,
                "deferred_members": admission.deferred,
                "required_ms": duration_millis(admission.budget.required),
                "complete_graph_required_ms": duration_millis(admission.complete_budget.required),
                "provider_writes": 0,
                "branch_writes": 0
            }),
            false,
        )?;
        let mut progress = progress;
        invalidate_rewritten_force_intents(&status, &provider, &admitted_plans, &mut progress)?;
        lock.checkpoint(
            "physical_rebase_force_intents_invalidated",
            json!({
                "rebase_plans": checkpoint_rebase_plans(&plans),
                "provider_receipts": checkpoint_provider_receipts(&progress.provider_receipts),
                "completed_steps": bounded_checkpoint_sequence(
                    progress
                        .steps
                        .iter()
                        .map(|step| serde_json::to_value(step).expect("mutation step serializes"))
                        .collect(),
                ),
                "branch_writes": 0,
            }),
            false,
        )
        .map_err(|error| {
            attach_physical_rebuild(
                error,
                &PhysicalRebuildOutcome {
                    repository: Some(status.repository.clone()),
                    affected_prs: plans.iter().map(|plan| plan.pr).collect(),
                    plans: plans.clone(),
                    provider_receipts: progress.provider_receipts.clone(),
                    steps: progress.steps.clone(),
                    ..PhysicalRebuildOutcome::default()
                },
            )
        })?;
        physical_rebuild =
            apply_physical_chains(&status, &provider, &prepared, progress, &mut lock)?;
        progress::emit(
            "physical_rebase_applied",
            format!(
                "rewrote {} branch generation(s) under exact leases",
                physical_rebuild
                    .receipts
                    .iter()
                    .filter(|receipt| !receipt.already_satisfied)
                    .count(),
            ),
        );
        lock.checkpoint(
            "physical_rebase_applied",
            json!({
                "rebase_plans": checkpoint_rebase_plans(&physical_rebuild.plans),
                "rebase_receipts": checkpoint_rebase_receipts(&physical_rebuild.receipts),
                "deferred_members": physical_rebuild.deferred,
                "provider_receipts": checkpoint_provider_receipts(&physical_rebuild.provider_receipts),
            }),
            false,
        )?;
        progress::emit(
            "midpoint_rediscovery",
            "revalidating every pushed generation before provider convergence",
        );
        let mut midpoint = read::status_with_deadline_and_budget(
            context,
            operation_deadline,
            Some(&github_budget),
        )
        .map_err(|error| attach_physical_rebuild(error, &physical_rebuild))?;
        // One bounded re-read absorbs provider list-view lag behind an exact
        // proven push; a persistent mismatch still fails closed below.
        if physical_rebuild.receipts.iter().any(|receipt| {
            midpoint
                .analysis
                .pull_requests
                .get(&receipt.pr)
                .is_none_or(|observed| observed.head.oid != receipt.new_head_oid)
        }) {
            std::thread::sleep(PROVIDER_HEAD_CONVERGENCE_DELAY);
            midpoint = read::status_with_deadline_and_budget(
                context,
                operation_deadline,
                Some(&github_budget),
            )
            .map_err(|error| attach_physical_rebuild(error, &physical_rebuild))?;
        }
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
                        Some(json!({
                            "receipt": receipt,
                            "observed_head": observed.head.oid,
                            "expected_head": receipt.new_head_oid,
                            "resumable": true,
                            "auto_merge_state": "head auto-merge stays intentionally disabled until a tick revalidates the rewritten generation's fresh CI",
                            "safe_next_action": "rerun the same idempotent sync; the exact pushed generation converges without another rewrite",
                        })),
                    ),
                    &physical_rebuild,
                ));
            }
        }
        status = midpoint;
        // A bounded prefix apply intentionally stops before ordinary
        // convergence: the chain is mid-rebuild, so CI observation, auto-merge
        // repair, root arming, and admission all wait for the resumed tick that
        // finishes the graph. Completed receipts are already durable, so the
        // resume never replays a provider mutation.
        if admission.deferred_convergence {
            return bounded_prefix_output(
                context,
                input,
                started,
                operation_deadline,
                initial_status_elapsed,
                convergence_started.elapsed(),
                &admission,
                physical_rebuild,
                status,
                lock_recovery,
                &mut lock,
            );
        }
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
    // Exact generations this tick's own scheduler rebase published. Root arming
    // provenance must attribute a provider-side auto-merge drop to the engine's
    // own rewrite rather than to an external actor.
    let rewritten_heads = physical_rebuild
        .receipts
        .iter()
        .filter(|receipt| !receipt.already_satisfied)
        .map(|receipt| (receipt.pr, receipt.new_head_oid.clone()))
        .collect::<BTreeMap<_, _>>();
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
        &rewritten_heads,
        RequiredRunsPolicy::from_config(&context.config.sync),
    )?;
    progress::emit(
        "provider_convergence",
        format!(
            "converged {} caravan(s) with {} completed provider mutation(s)",
            progress.synchronized_caravans.len(),
            progress
                .steps
                .iter()
                .filter(|step| step.state == MutationStepState::Completed)
                .count(),
        ),
    );
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
    progress::emit(
        "final_rediscovery",
        "reading the authoritative post-mutation graph",
    );
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
        progress::emit(
            "auto_admission",
            format!(
                "considered {} candidate(s): {} join(s), {} skip(s), continuation {:?}",
                admission.candidates_considered,
                admission.joins.len(),
                admission.skips.len(),
                admission.continuation,
            ),
        );
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
                &rewritten_heads,
                RequiredRunsPolicy::from_config(&context.config.sync),
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
        &progress.required_runs,
        &progress.missing_required_runs,
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
        root_auto_merge: progress.root_auto_merge,
        root_promotion: progress.root_promotion,
        root_merge: progress.root_merge,
        required_runs: progress.required_runs,
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

/// Keep one hook notification per distinct required-run problem fingerprint.
fn dedupe_required_runs_events(events: &mut Vec<CaravanEvent>) {
    let mut seen = BTreeSet::new();
    events.retain(|event| {
        if event.kind != EventKind::RequiredRunsMissing {
            return true;
        }
        event
            .metadata
            .get("fingerprint")
            .and_then(Value::as_str)
            .is_none_or(|fingerprint| seen.insert(fingerprint.to_owned()))
    });
}

fn merge_sync_progress(target: &mut SyncProgress, mut source: SyncProgress) {
    target.steps.append(&mut source.steps);
    target
        .provider_receipts
        .append(&mut source.provider_receipts);
    for receipt in std::mem::take(&mut source.root_auto_merge) {
        target.root_auto_merge.retain(|item| item.pr != receipt.pr);
        target.root_auto_merge.push(receipt);
    }
    for receipt in std::mem::take(&mut source.required_runs) {
        target.required_runs.retain(|item| item.pr != receipt.pr);
        target.required_runs.push(receipt);
    }
    for problem in std::mem::take(&mut source.missing_required_runs) {
        required_runs::push_problem(&mut target.missing_required_runs, problem);
    }
    for (branch, read) in std::mem::take(&mut source.required_contexts) {
        target.required_contexts.entry(branch).or_insert(read);
    }
    target.rebase_plans.append(&mut source.rebase_plans);
    target.rebase_receipts.append(&mut source.rebase_receipts);
    target.paused_caravans.append(&mut source.paused_caravans);
    target
        .head_advancements
        .append(&mut source.head_advancements);
    target.events.append(&mut source.events);
    // Two convergence passes over the same member inside one tick must not
    // notify hooks twice about the same exact stall.
    dedupe_required_runs_events(&mut target.events);
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
        capacity_refusal: None,
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
            // A chain that already holds every member the configured deadline
            // can guarantee to drain must stop growing. Refusing admission here
            // is what keeps the existing processable prefix draining instead of
            // raising the reserve again on every pass.
            if let Some(refusal) =
                caravan_capacity_refusal(context, &status, candidate.number, target_tail)
            {
                output.continuation = if refusal.capacity_defect.is_some() {
                    AutoAdmissionContinuation::CaravanBudgetCapacityDefect
                } else {
                    AutoAdmissionContinuation::CaravanBudgetCapacityExhausted
                };
                output.capacity_refusal = Some(refusal);
                break;
            }
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

/// Deterministic pre-admission capacity gate for one candidate join.
///
/// Returns typed refusal evidence when accepting the candidate would push the
/// exact target chain past the largest size the configured deadline can still
/// guarantee to drain, or when the configured arithmetic yields no bound
/// admission could honestly enforce. Forming a brand-new caravan is never
/// refused here: an independent chain has its own bounded prefix.
pub(crate) fn caravan_capacity_refusal(
    context: &AppContext,
    status: &StatusOutput,
    candidate_pr: PrNumber,
    target_tail: Option<PrNumber>,
) -> Option<CaravanCapacityRefusal> {
    if !context.config.rebase_on_join {
        return None;
    }
    let caravan = status.analysis.fleet.containing(target_tail?)?;
    let deadline = sync_operation_budget(context);
    let members = u64::try_from(caravan.members.len()).unwrap_or(u64::MAX);
    let (code, bound, defect, safe_next_action) = match capacity_gate(context, deadline, members) {
        CapacityGate::Open { .. } => return None,
        CapacityGate::AtCapacity { bound } => (
            "caravan_budget_capacity_exhausted",
            Some(bound),
            None,
            format!(
                "let caravan #{} drain below {bound} members, or raise sync.max_duration_secs (currently {}s) before admitting #{candidate_pr}; no member is reordered, evicted, or split to make room",
                caravan.id,
                deadline.as_secs(),
            ),
        ),
        // bd-b1c7b7: an unsound bound is a configuration defect, so the
        // guidance names the configuration change that repairs it instead
        // of a drain that provably cannot.
        CapacityGate::Defect(defect) => {
            let action = format!(
                "do not wait for caravan #{} to drain: draining cannot repair an unsound admission bound. {}",
                caravan.id, defect.safe_next_action,
            );
            ("caravan_budget_capacity_defect", None, Some(defect), action)
        }
    };
    Some(CaravanCapacityRefusal {
        code: code.to_owned(),
        candidate_pr,
        caravan_id: caravan.id,
        caravan_members: members,
        max_admissible_members: bound,
        capacity_defect: defect,
        configured_deadline_ms: duration_millis(deadline),
        command_timeout_ms: context.config.command_timeout_secs.saturating_mul(1_000),
        safe_next_action,
    })
}

/// Typed zero-write refusal for an explicit join at chain capacity, or for an
/// admission bound the configuration cannot make sound.
pub(crate) fn caravan_capacity_error(refusal: &CaravanCapacityRefusal) -> AppError {
    let defect = refusal.capacity_defect.is_some();
    let message = if defect {
        "the configured sync deadline yields no admissible chain size, so admission cannot be gated honestly and the join fails as a defect"
    } else {
        "the target caravan already holds every member the configured sync deadline can guarantee to drain"
    };
    let suggested_actions = if defect {
        json!([
            "raise sync.max_duration_secs until the reported minimum_deadline_ms fits, so a chain of at least two members is admissible again",
            "lower sync.reserve_secs_per_command to a proven-safe per-command reserve",
            "start an independent caravan; waiting for an existing caravan to drain cannot repair an unsound bound"
        ])
    } else {
        json!([
            "run `cara sync --all` until the existing bounded prefix drains members out of the caravan",
            "raise sync.max_duration_secs, or lower a proven-safe sync.reserve_secs_per_command, to raise max_admissible_members",
            "start an independent caravan instead of extending one already at capacity"
        ])
    };
    AppError::structured(
        ErrorCategory::Validation,
        refusal.code.clone(),
        message,
        Some(json!({
            "mutated": false,
            "candidate_pr": refusal.candidate_pr,
            "caravan_id": refusal.caravan_id,
            "caravan_members": refusal.caravan_members,
            "max_admissible_members": refusal.max_admissible_members,
            "capacity_defect": refusal.capacity_defect,
            "configured_deadline_ms": refusal.configured_deadline_ms,
            "command_timeout_ms": refusal.command_timeout_ms,
            "retryable": false,
            "safe_next_action": refusal.safe_next_action,
            "suggested_actions": suggested_actions,
        })),
    )
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
                    pause.state.is_effective() && pause.record.caravan_head == caravan.id
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
    execute_bounded(
        status,
        provider,
        all,
        rerun_failed,
        force_merge,
        u32::MAX,
        &BTreeMap::new(),
        RequiredRunsPolicy::default(),
    )
}

#[cfg(test)]
fn execute_with_required_runs(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    required_runs: RequiredRunsPolicy,
) -> Result<SyncProgress, AppError> {
    execute_bounded(
        status,
        provider,
        false,
        false,
        false,
        u32::MAX,
        &BTreeMap::new(),
        required_runs,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_bounded(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    all: bool,
    rerun_failed: bool,
    force_merge: bool,
    mutation_limit: u32,
    rewritten_heads: &BTreeMap<PrNumber, crate::model::CommitOid>,
    required_runs: RequiredRunsPolicy,
) -> Result<SyncProgress, AppError> {
    let mut caravans = select_caravans(status, all)?;
    let paused_caravans = status
        .pauses
        .iter()
        .filter(|pause| {
            pause.state.is_effective()
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
    progress.required_runs_grace_secs = required_runs.grace_secs;
    progress.required_runs_retrigger_enabled = required_runs.retrigger;
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
            rewritten_heads,
            &mut progress,
        )?;
    }

    Ok(progress)
}

#[allow(clippy::too_many_arguments)]
fn reconcile_caravan(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    caravan: &Caravan,
    rerun_failed: bool,
    force_merge: bool,
    rewritten_heads: &BTreeMap<PrNumber, crate::model::CommitOid>,
    progress: &mut SyncProgress,
) -> Result<(), AppError> {
    let head = caravan.head().expect("caravans are non-empty");
    let predecessor = merged_predecessor(status, caravan).map(|snapshot| snapshot.number);

    // Step 1 of the fenced transaction. Promotion always precedes any merge
    // mechanism: a root whose base is still an already-merged predecessor
    // branch must never become eligible for a merge of any kind.
    progress.promote_root(
        provider,
        status,
        caravan.id,
        head,
        predecessor,
        predecessor.is_some(),
    )?;
    if let Some(predecessor) = predecessor {
        progress.record_head_advancement(predecessor, head, status);
    }

    // Step 2. Exactly one merge actor. Under the caravan-owned policy that
    // includes the root itself: a foreign `autoMergeRequest` is either
    // converged away or refused, never raced.
    let members_to_disarm: Vec<PrNumber> = if progress.head_merge_actor.caravan() {
        caravan.members.clone()
    } else {
        caravan.members.iter().skip(1).copied().collect()
    };
    for number in members_to_disarm {
        progress.ensure_no_foreign_auto_merge(provider, &status.repository, caravan.id, number)?;
    }

    let mut forced_head = false;
    let mut ci_failure = None;
    for number in caravan.members.iter().copied() {
        let observation = progress.observe_ci(provider, &status.repository, number)?;
        let disposition = observation.disposition;
        progress.ci.push(observation.clone());
        // Required-run coverage is verified per member before any CI stop, so a
        // head whose required contexts never started a run is visible even when
        // an earlier member is legitimately failing, and one stalled member
        // never suppresses another member's evidence. Because promotion already
        // retargeted the root, this is evaluated against the *new* merge
        // identity: contexts required by the default branch, not by a
        // predecessor branch.
        progress.verify_required_runs(provider, &status.repository, caravan.id, number)?;
        if disposition == CiDisposition::Failed {
            if rerun_failed {
                progress.rerun_exact_failed_runs(
                    provider,
                    &status.repository,
                    number,
                    &observation.rerunnable_run_ids,
                )?;
            }
            ci_failure = Some(observation);
            break;
        }
        forced_head |= number == head && disposition == CiDisposition::Forced;
    }

    if forced_head {
        return force_merge_head(
            status,
            provider,
            caravan,
            force_merge,
            rewritten_heads,
            progress,
        );
    }

    if progress.head_merge_actor.github() {
        // Historical delegation. Required root arming stays scheduler-owned
        // convergent state and converges before any CI stop, because native
        // auto-merge merges only a passing head.
        progress.ensure_root_squash_auto_merge(
            provider,
            &status.repository,
            caravan.id,
            head,
            status.analysis.pull_requests.get(&head),
            rewritten_heads.get(&head),
        )?;
        if let Some(observation) = ci_failure {
            return Err(ci_decision_error(status, caravan, &observation, progress));
        }
        return Ok(());
    }

    if let Some(observation) = ci_failure {
        return Err(ci_decision_error(status, caravan, &observation, progress));
    }

    // Steps 3-5. Cara is the merge actor: re-read exact facts, prove the
    // already-validated tree is what lands, squash once, prove it reached the
    // default branch, then promote the successor and try again.
    progress.drain_caravan_roots(provider, status, caravan)
}

#[allow(clippy::too_many_lines)]
fn force_merge_head(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    caravan: &Caravan,
    force_merge: bool,
    rewritten_heads: &BTreeMap<PrNumber, crate::model::CommitOid>,
    progress: &mut SyncProgress,
) -> Result<(), AppError> {
    let head = caravan.head().expect("caravan head");
    // An exceptional non-green administrator merge binds the *exact discovery*
    // generation it was authorized against. Promotion deliberately tolerates
    // unrelated churn so routine convergence is not operator babysitting, but
    // force is not routine: any drift since discovery fails closed here, before
    // any provider write.
    if let Some(discovered) = status.analysis.pull_requests.get(&head) {
        provider
            .verify_pull_request(
                &status.repository,
                &PullRequestPrecondition::from(discovered),
            )
            .map_err(|error| mutation_error(&error, progress, Some(head)))?;
    }
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
        // Promotion is the same fenced transaction the ordinary path uses: the
        // successor must target the exact default branch before any merge
        // mechanism, caravan-owned or native, can act on it.
        progress.promote_root(provider, status, new_head, new_head, Some(head), true)?;
        progress.record_head_advancement(head, new_head, status);
        for number in caravan.members.iter().skip(2).copied() {
            progress.ensure_auto_merge_disabled(provider, &status.repository, number)?;
        }
        if progress.head_merge_actor.github() {
            progress.ensure_root_squash_auto_merge(
                provider,
                &status.repository,
                new_head,
                new_head,
                status.analysis.pull_requests.get(&new_head),
                rewritten_heads.get(&new_head),
            )?;
        } else {
            progress.ensure_no_foreign_auto_merge(
                provider,
                &status.repository,
                new_head,
                new_head,
            )?;
        }
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
    if let Some(force_intent) = crate::force_intent::sync_decision_evidence(status, observation.pr)
    {
        evidence.insert("force_intent".to_owned(), force_intent);
    }
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

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// Bounded required-run policy for one tick.
#[derive(Debug, Clone, Copy)]
struct RequiredRunsPolicy {
    grace_secs: u64,
    retrigger: bool,
}

impl Default for RequiredRunsPolicy {
    fn default() -> Self {
        Self {
            grace_secs: DEFAULT_MISSING_REQUIRED_RUNS_GRACE_SECS,
            retrigger: true,
        }
    }
}

impl RequiredRunsPolicy {
    fn from_config(config: &crate::config::SyncConfig) -> Self {
        Self {
            grace_secs: config.missing_required_runs_grace_secs,
            retrigger: config.retrigger_missing_required_runs,
        }
    }
}

/// Result of the single auditable retrigger plus its one rediscovery.
#[derive(Debug)]
struct RequiredRunsRetriggerOutcome {
    receipt: RequiredRunsRetrigger,
    assessment: crate::required_runs::RequiredRunsAssessment,
    rediscovered: bool,
}

/// The latest provider timestamp that could have triggered CI for this head.
///
/// A rebase publishes a commit whose committer date *is* its publication time,
/// but an old commit can also be pushed onto a branch long after it was
/// authored. Taking the later of the commit date and the PR's `updated_at`
/// therefore never starts the grace countdown before the provider could
/// possibly have known about the head, so a freshly published head is never
/// prematurely accused of missing its runs.
fn head_published_at(
    current: &PullRequestSnapshot,
    lineage: Option<&HeadRunLineage>,
) -> Option<String> {
    let committed = lineage.and_then(|lineage| lineage.head_committed_at.clone());
    let updated = current.updated_at.clone();
    match (committed, updated) {
        (Some(committed), Some(updated)) => {
            let committed_secs = required_runs::rfc3339_to_unix_secs(&committed);
            let updated_secs = required_runs::rfc3339_to_unix_secs(&updated);
            match (committed_secs, updated_secs) {
                (Some(left), Some(right)) => Some(if right > left { updated } else { committed }),
                (Some(_), None) => Some(committed),
                (None, Some(_)) => Some(updated),
                (None, None) => None,
            }
        }
        (Some(committed), None) => Some(committed),
        (None, updated) => updated,
    }
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
    // A caravan-owned tick performs the squash itself, so it requires squash
    // merging and nothing else. Requiring provider-native auto-merge here would
    // permanently refuse to synchronize exactly the repositories that disabled
    // it so Cara could own the merge.
    if progress.head_merge_actor.caravan() {
        let allows_squash_merge = provider
            .repository_allows_squash_merge(&status.repository)
            .map_err(|error| mutation_error(&error, progress, None))?;
        if !allows_squash_merge {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "squash_merge_not_enabled",
                "repository settings must allow squash merging before synchronization",
                Some(json!({
                    "repository": status.repository,
                    "head_merge_actor": progress.head_merge_actor,
                    "resumable": true,
                    "next": "enable repository squash merge, then rerun `cara sync`",
                })),
            ));
        }
    } else {
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
                    "head_merge_actor": progress.head_merge_actor,
                    "resumable": true,
                    "next": "enable repository auto-merge and squash merge, or set sync.head_merge_actor=\"caravan\" so cara owns the merge, then rerun `cara sync`",
                })),
            ));
        }
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
            "root_auto_merge": progress.root_auto_merge,
        })),
    )
}

#[derive(Debug)]
struct SyncProgress {
    operation_id: OperationId,
    repository: RepositoryId,
    /// Exact default branch every caravan root must target and land on.
    default_branch: String,
    steps: Vec<MutationStep>,
    provider_receipts: Vec<GitHubMutationReceipt>,
    root_auto_merge: Vec<RootAutoMergeReceipt>,
    /// Durable proof that each caravan root targets the exact default branch.
    root_promotion: Vec<RootPromotionReceipt>,
    /// Durable proof of each caravan-owned squash merge and where it landed.
    root_merge: Vec<RootMergeReceipt>,
    /// Configured merge actor for this tick.
    head_merge_actor: HeadMergeActor,
    /// Reviewed policy for a foreign provider auto-merge request.
    external_auto_merge_policy: ExternalAutoMergePolicy,
    /// Bounded caravan-owned merges allowed in this tick.
    max_root_merges: u32,
    /// Durable per-member required-run coverage proof for the exact head.
    required_runs: Vec<RequiredRunsReceipt>,
    /// Deduplicated, bounded visible problems for stalled required coverage.
    missing_required_runs: Vec<MissingRequiredRunsProblem>,
    /// One protection read per distinct base branch per tick.
    required_contexts: BTreeMap<String, RequiredContextsRead>,
    /// Bounded required-run policy configuration for this tick.
    required_runs_grace_secs: u64,
    required_runs_retrigger_enabled: bool,
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
            default_branch: status.default_branch.clone(),
            steps: Vec::new(),
            provider_receipts: Vec::new(),
            root_auto_merge: Vec::new(),
            root_promotion: Vec::new(),
            root_merge: Vec::new(),
            // Exactly one fact decides who merges: the configured policy
            // projected onto status. Every surface reads the same value.
            head_merge_actor: status.head_merge.actor,
            external_auto_merge_policy: status.head_merge.external_auto_merge_policy,
            max_root_merges: status.head_merge.max_root_merges_per_tick.max(1),
            required_runs: Vec::new(),
            missing_required_runs: Vec::new(),
            required_contexts: BTreeMap::new(),
            required_runs_grace_secs: DEFAULT_MISSING_REQUIRED_RUNS_GRACE_SECS,
            required_runs_retrigger_enabled: true,
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

    /// Prove that every required context has *some* reporting lineage on the
    /// exact current head, instead of trusting an empty rollup forever.
    ///
    /// A rebase-on-join can publish a head GitHub never starts a run for. The
    /// PR then reports no pending and no failed check, so a rollup-only
    /// scheduler waits silently while the whole caravan is dead. This member-
    /// scoped verification:
    ///
    /// 1. discovers required contexts from protection on the exact base branch;
    /// 2. reads run/check-suite lineage for the exact head only when a required
    ///    context is absent from the rollup, keeping the healthy path cheap;
    /// 3. classifies `missing_required_runs` apart from pending, failing,
    ///    cancelled/superseded and unknown provider state;
    /// 4. requests at most one auditable check-suite rerequest on the unchanged
    ///    head and rediscovers exactly once;
    /// 5. records a sealed receipt plus a deduplicated visible problem instead
    ///    of failing the tick, so one stalled member never hides another.
    fn verify_required_runs(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        caravan_id: PrNumber,
        number: PrNumber,
    ) -> Result<(), AppError> {
        let current = self.current.get(&number).expect("sync member").clone();
        let contexts =
            self.discover_required_contexts(provider, repository, &current.base.name, number)?;
        let assessment = self.assess_required_runs(provider, repository, &current, &contexts)?;

        let outcome = match assessment.recovery {
            RequiredRunsRecovery::RerequestCheckSuite { check_suite_id }
                if self.required_runs_retrigger_enabled =>
            {
                Some(self.retrigger_required_runs(
                    provider,
                    repository,
                    number,
                    check_suite_id,
                    &contexts,
                    &assessment,
                )?)
            }
            _ => None,
        };
        // Exactly one rediscovery decides the final receipt, so a successful
        // retrigger is never reported as a stall and a refused one is never
        // retried inside the same tick.
        let (assessment, retrigger) = match outcome {
            Some(outcome) if outcome.rediscovered => (outcome.assessment, Some(outcome.receipt)),
            Some(outcome) => (assessment, Some(outcome.receipt)),
            None => (assessment, None),
        };

        self.push_required_runs(caravan_id, assessment, retrigger);
        Ok(())
    }

    /// Read-only required-run assessment for planning. Never mutates the
    /// provider, so a dry run reveals an upcoming stall without recovering it.
    fn observe_required_runs(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<crate::required_runs::RequiredRunsAssessment, AppError> {
        let current = self.current.get(&number).expect("sync member").clone();
        let contexts =
            self.discover_required_contexts(provider, repository, &current.base.name, number)?;
        self.assess_required_runs(provider, repository, &current, &contexts)
    }

    /// One protection read per distinct base branch per tick.
    fn discover_required_contexts(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        branch: &str,
        number: PrNumber,
    ) -> Result<RequiredContextsRead, AppError> {
        if let Some(cached) = self.required_contexts.get(branch) {
            return Ok(cached.clone());
        }
        let read = provider
            .branch_required_contexts(repository, branch)
            .map_err(|error| mutation_error(&error, self, Some(number)))?
            .normalized();
        self.required_contexts
            .insert(branch.to_owned(), read.clone());
        Ok(read)
    }

    /// Assess one member, reading head lineage only when it can change the answer.
    fn assess_required_runs(
        &self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        current: &PullRequestSnapshot,
        contexts: &RequiredContextsRead,
    ) -> Result<crate::required_runs::RequiredRunsAssessment, AppError> {
        let reporting = current
            .checks
            .iter()
            .filter(|check| check.state != crate::model::CheckState::Expected)
            .map(|check| check.name.as_str())
            .collect::<BTreeSet<_>>();
        let absent = contexts
            .contexts
            .iter()
            .any(|context| !reporting.contains(context.as_str()));
        let lineage = if absent && contexts.complete && !contexts.contexts.is_empty() {
            Some(
                provider
                    .head_run_lineage(repository, &PullRequestPrecondition::from(current))
                    .map_err(|error| mutation_error(&error, self, Some(current.number)))?,
            )
        } else {
            None
        };
        Ok(self.assess_with_lineage(current, contexts, lineage.as_ref()))
    }

    fn assess_with_lineage(
        &self,
        current: &PullRequestSnapshot,
        contexts: &RequiredContextsRead,
        lineage: Option<&HeadRunLineage>,
    ) -> crate::required_runs::RequiredRunsAssessment {
        let published = head_published_at(current, lineage);
        required_runs::assess(&RequiredRunsInput {
            pr: current.number,
            head: &current.head,
            base: &current.base,
            contexts,
            lineage,
            checks: &current.checks,
            head_published_at: published.as_deref(),
            clock: RequiredRunsClock {
                now_unix: now_unix(),
                grace_secs: self.required_runs_grace_secs,
            },
        })
    }

    /// Issue the single auditable rerequest and rediscover exactly once.
    fn retrigger_required_runs(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        number: PrNumber,
        check_suite_id: u64,
        contexts: &RequiredContextsRead,
        before_recovery: &crate::required_runs::RequiredRunsAssessment,
    ) -> Result<RequiredRunsRetriggerOutcome, AppError> {
        let before = self.current.get(&number).expect("sync member").clone();
        self.ensure_mutation_capacity(1)?;
        let requested =
            provider.rerequest_check_suite(repository, &self.precondition(number), check_suite_id);
        let failure = match requested {
            Ok(receipt) => {
                self.record(
                    receipt,
                    &format!(
                        "requested check suite {check_suite_id} again on unchanged head {}",
                        before.head.oid
                    ),
                );
                None
            }
            Err(error) => Some(error.to_string()),
        };
        if let Some(failure) = failure {
            // A refused request changes nothing, so the pre-recovery verdict is
            // still the exact truth about this head.
            return Ok(RequiredRunsRetriggerOutcome {
                receipt: RequiredRunsRetrigger {
                    check_suite_id,
                    head_oid: before.head.oid.clone(),
                    attempts: crate::required_runs::REQUIRED_RUNS_RETRIGGER_ATTEMPTS,
                    requested: false,
                    rediscovered: false,
                    status_after: before_recovery.status,
                    failure: Some(failure),
                },
                assessment: before_recovery.clone(),
                rediscovered: false,
            });
        }

        let refreshed = provider
            .refetch_pull_request(repository, number)
            .map_err(|error| mutation_error(&error, self, Some(number)))?;
        // A head that moved during recovery belongs to a different generation;
        // its coverage is decided by the next bounded tick, never by this one.
        if refreshed.head.oid != before.head.oid {
            return Ok(RequiredRunsRetriggerOutcome {
                receipt: RequiredRunsRetrigger {
                    check_suite_id,
                    head_oid: before.head.oid.clone(),
                    attempts: crate::required_runs::REQUIRED_RUNS_RETRIGGER_ATTEMPTS,
                    requested: true,
                    rediscovered: false,
                    status_after: before_recovery.status,
                    failure: Some(format!(
                        "head moved from {} to {} during recovery",
                        before.head.oid, refreshed.head.oid
                    )),
                },
                assessment: before_recovery.clone(),
                rediscovered: false,
            });
        }
        self.current.insert(number, refreshed.clone());
        let lineage = provider
            .head_run_lineage(repository, &PullRequestPrecondition::from(&refreshed))
            .map_err(|error| mutation_error(&error, self, Some(number)))?;
        let assessment = self.assess_with_lineage(&refreshed, contexts, Some(&lineage));
        Ok(RequiredRunsRetriggerOutcome {
            receipt: RequiredRunsRetrigger {
                check_suite_id,
                head_oid: refreshed.head.oid.clone(),
                attempts: crate::required_runs::REQUIRED_RUNS_RETRIGGER_ATTEMPTS,
                requested: true,
                rediscovered: true,
                status_after: assessment.status,
                failure: None,
            },
            assessment,
            rediscovered: true,
        })
    }

    /// Seal one receipt, emit bounded deduplicated evidence, and stay visible.
    fn push_required_runs(
        &mut self,
        caravan_id: PrNumber,
        assessment: crate::required_runs::RequiredRunsAssessment,
        retrigger: Option<RequiredRunsRetrigger>,
    ) {
        let number = assessment.pr;
        if let Some(problem) = required_runs::problem(caravan_id, &assessment, retrigger.as_ref()) {
            // Hook evidence is exactly as deduplicated and bounded as the
            // problem list: an already-visible stall never re-notifies.
            let event = self.event(
                EventKind::RequiredRunsMissing,
                Some(caravan_id),
                vec![number],
                Some(problem.message.clone()),
                BTreeMap::from([
                    ("problem".to_owned(), json!(problem.clone())),
                    ("status".to_owned(), json!(assessment.status)),
                    ("fingerprint".to_owned(), json!(problem.fingerprint.clone())),
                ]),
            );
            if required_runs::push_problem(&mut self.missing_required_runs, problem) {
                self.events.push(event);
            }
        }
        if let Some(retrigger) = retrigger.as_ref().filter(|item| item.requested) {
            self.events.push(self.event(
                EventKind::RequiredRunsRetriggered,
                Some(caravan_id),
                vec![number],
                Some(format!(
                    "requested check suite {} again on unchanged head {}",
                    retrigger.check_suite_id, retrigger.head_oid
                )),
                BTreeMap::from([
                    ("retrigger".to_owned(), json!(retrigger.clone())),
                    ("status_after".to_owned(), json!(retrigger.status_after)),
                ]),
            ));
        }
        let receipt = required_runs::receipt(
            &self.repository,
            caravan_id,
            assessment,
            retrigger,
            required_runs::provenance(&self.operation_id, REQUIRED_RUNS_CONVERGENCE_REASON),
        );
        self.required_runs.retain(|existing| existing.pr != number);
        self.required_runs.push(receipt);
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

    /// Converge exactly one merge actor for one caravan member.
    ///
    /// Under the caravan-owned policy this also covers the root: a foreign
    /// `autoMergeRequest` on the root is what merged PR2213 into an
    /// already-merged predecessor branch, so it is either converged away or
    /// refused with typed evidence. It is never left armed beside a
    /// caravan-owned merge.
    fn ensure_no_foreign_auto_merge(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        caravan_id: PrNumber,
        number: PrNumber,
    ) -> Result<(), AppError> {
        let observed = self.current.get(&number).expect("sync member").clone();
        if !observed.auto_merge.enabled {
            self.already(
                MutationKind::DisableAutoMerge,
                number,
                if self.head_merge_actor.caravan() {
                    "no provider auto-merge request; cara is the single merge actor"
                } else {
                    "non-head auto-merge already disabled"
                },
            );
            return Ok(());
        }
        if self.head_merge_actor.caravan()
            && self.external_auto_merge_policy == ExternalAutoMergePolicy::Refuse
        {
            return Err(self.root_merge_failure(
                caravan_id,
                number,
                RootMergeFailureCause::ForeignAutoMergeActor,
                &observed,
                None,
                &json!({
                    "observed_auto_merge": observed.auto_merge,
                    "external_auto_merge_policy": self.external_auto_merge_policy,
                }),
            ));
        }
        self.ensure_mutation_capacity(1)?;
        let receipt = provider
            .disable_auto_merge(repository, &self.precondition(number))
            .map_err(|error| mutation_error(&error, self, Some(number)))?;
        self.record(
            receipt,
            if self.head_merge_actor.caravan() {
                "disabled a foreign provider auto-merge request; cara is the single merge actor"
            } else {
                "disabled auto-merge on non-head PR"
            },
        );
        Ok(())
    }

    /// Fenced root promotion: make the provider base authoritative *before* any
    /// merge mechanism can act on the root.
    ///
    /// 1. re-read the exact current root generation;
    /// 2. retarget to the exact default branch when the observed base is
    ///    anything else, in particular an already-merged predecessor branch;
    /// 3. re-read and prove base/ref/head after the retarget;
    /// 4. persist a sealed [`RootPromotionReceipt`].
    ///
    /// Any unproven step fails with typed `root_promotion_incomplete` evidence
    /// *before* arming or merging anything, so a root can never merge into the
    /// wrong target.
    fn promote_root(
        &mut self,
        provider: &impl SyncProvider,
        status: &StatusOutput,
        caravan_id: PrNumber,
        number: PrNumber,
        predecessor: Option<PrNumber>,
        predecessor_merged: bool,
    ) -> Result<(), AppError> {
        let repository = status.repository.clone();
        let default_branch = status.default_branch.clone();
        let previous = self.current.get(&number).cloned();
        let (observed, mut reads) = self.read_exact_root_generation(
            provider,
            &repository,
            caravan_id,
            number,
            previous.as_ref(),
            &default_branch,
        )?;
        let base_before = observed.base.clone();
        let expected_head = observed.head.oid.clone();
        let trigger =
            root_merge::promotion_trigger(&base_before.name, &default_branch, predecessor_merged);

        if !trigger.requires_write() {
            self.already(
                MutationKind::SetBase,
                number,
                "promoted caravan root already targets the exact default branch",
            );
            self.push_root_promotion(
                caravan_id,
                &observed,
                base_before,
                &default_branch,
                predecessor,
                predecessor_merged,
                trigger,
                reads,
                false,
            );
            return Ok(());
        }

        self.ensure_mutation_capacity(1)?;
        let receipt = provider
            .set_base(&repository, &self.precondition(number), &default_branch)
            .map_err(|error| mutation_error(&error, self, Some(number)))?;
        self.record(
            receipt,
            "retargeted the promoted caravan root to the exact default branch",
        );

        // Re-read the provider instead of trusting the mutation response: the
        // failure class this closes is precisely a provider view that still
        // exposes a superseded base or head.
        let mut observed = self
            .current
            .get(&number)
            .cloned()
            .expect("set-base receipt records current facts");
        while reads < ROOT_MERGE_CONFIRMATION_READS
            && (observed.base.name != default_branch || observed.head.oid != expected_head)
        {
            std::thread::sleep(ROOT_MERGE_CONFIRMATION_DELAY);
            observed = provider
                .refetch_pull_request(&repository, number)
                .map_err(|error| mutation_error(&error, self, Some(number)))?;
            self.current.insert(number, observed.clone());
            reads = reads.saturating_add(1);
        }
        if observed.head.oid != expected_head {
            return Err(self.root_promotion_failure(
                caravan_id,
                number,
                RootPromotionFailureCause::RootHeadMovedDuringPromotion,
                trigger,
                &observed,
                Some(&expected_head),
                &default_branch,
                reads,
            ));
        }
        if observed.base.name != default_branch {
            return Err(self.root_promotion_failure(
                caravan_id,
                number,
                RootPromotionFailureCause::BaseRetargetNotObserved,
                trigger,
                &observed,
                Some(&expected_head),
                &default_branch,
                reads,
            ));
        }
        self.push_root_promotion(
            caravan_id,
            &observed,
            base_before,
            &default_branch,
            predecessor,
            predecessor_merged,
            trigger,
            reads,
            true,
        );
        Ok(())
    }

    /// Merge every promoted, proven-green root this tick may land.
    ///
    /// A whole green caravan can drain in one bounded tick, but every iteration
    /// re-reads exact provider facts, re-proves the cumulative tree, and proves
    /// the previous merge actually reached the default branch before the next
    /// root is promoted. No proof, no next iteration.
    fn drain_caravan_roots(
        &mut self,
        provider: &impl SyncProvider,
        status: &StatusOutput,
        caravan: &Caravan,
    ) -> Result<(), AppError> {
        let mut remaining = caravan.members.clone();
        let mut merged = 0_u32;
        while let Some(&root) = remaining.first() {
            if merged >= self.max_root_merges {
                self.record_merge_wait(root, RootMergeBlock::MergeBudgetReached, None);
                return Ok(());
            }
            if !self.merge_root(provider, status, caravan.id, root, &remaining)? {
                return Ok(());
            }
            merged = merged.saturating_add(1);
            remaining.remove(0);
            let Some(&next) = remaining.first() else {
                return Ok(());
            };
            // The successor is promoted immediately: it must never sit pointing
            // at the predecessor generation this tick just merged.
            self.promote_root(provider, status, caravan.id, next, Some(root), true)?;
            self.record_head_advancement(root, next, status);
            // Its own CI is re-observed against the new merge identity before
            // the loop may consider merging it.
            let observation = self.observe_ci(provider, &status.repository, next)?;
            self.ci.retain(|item| item.pr != next);
            self.ci.push(observation);
            self.verify_required_runs(provider, &status.repository, caravan.id, next)?;
        }
        Ok(())
    }

    /// Perform at most one exact caravan-owned squash merge.
    ///
    /// Returns whether the root landed, so the drain loop stops at the first
    /// bounded wait instead of guessing about the rest of the chain.
    #[allow(clippy::too_many_lines)]
    fn merge_root(
        &mut self,
        provider: &impl SyncProvider,
        status: &StatusOutput,
        caravan_id: PrNumber,
        number: PrNumber,
        remaining: &[PrNumber],
    ) -> Result<bool, AppError> {
        let repository = status.repository.clone();
        let default_branch = status.default_branch.clone();
        let previous = self.current.get(&number).cloned();
        let (observed, mut reads) = self.read_exact_root_generation(
            provider,
            &repository,
            caravan_id,
            number,
            previous.as_ref(),
            &default_branch,
        )?;
        let expected_head = observed.head.oid.clone();
        // The proof only authorizes a landing while it was constructed against
        // the *exact* default-branch generation this merge will land on. A
        // default branch that moved since discovery is not refused outright:
        // the next tick re-proves against the new generation and still lands
        // when the cumulative tree is identical.
        let observed_default = provider
            .branch_head_oid(&repository, &default_branch)
            .map_err(|error| mutation_error(&error, self, Some(number)))?;
        let tree_proof = status
            .analysis
            .cumulative_trees
            .iter()
            .find(|proof| {
                proof.candidate == observed.head
                    && proof.target.name == default_branch
                    && proof.target.oid == observed_default
            })
            .cloned();
        let facts = RootMergeFacts {
            default_branch: &default_branch,
            checks_passing: self
                .ci
                .iter()
                .rev()
                .find(|observation| observation.pr == number)
                .is_some_and(|observation| observation.disposition == CiDisposition::Passing),
            required_runs_satisfied: self
                .required_runs
                .iter()
                .rev()
                .find(|receipt| receipt.pr == number)
                .is_none_or(|receipt| {
                    matches!(
                        receipt.assessment.status,
                        crate::required_runs::RequiredRunsStatus::Satisfied
                            | crate::required_runs::RequiredRunsStatus::NotRequired
                    )
                }),
            // An identical cumulative tree *is* mechanical proof of a clean
            // merge: `git merge-tree` cannot construct the candidate's own tree
            // from a conflicting merge. The discovery-time compatibility report
            // remains accepted for roots that have no fresh tree proof yet.
            conflict_free_with_default: tree_proof.as_ref().is_some_and(|proof| proof.identical)
                || head_is_conflict_free_with_default(status, &observed),
            external_auto_merge: self.external_auto_merge_policy,
        };
        match root_merge::merge_gate(&observed, facts) {
            RootMergeGate::Refuse(cause) => {
                return Err(self.root_merge_failure(
                    caravan_id,
                    number,
                    cause,
                    &observed,
                    Some(&expected_head),
                    &json!({
                        "default_branch": default_branch,
                        "observed_base": observed.base,
                        "cumulative_tree": tree_proof,
                    }),
                ));
            }
            RootMergeGate::Wait(block) => {
                self.record_merge_wait(number, block, None);
                return Ok(false);
            }
            RootMergeGate::Eligible => {}
        }

        // The cumulative-tree proof is what makes retarget-only promotion sound.
        // Members are physically rebased before CI runs, so the exact head SHA
        // already carries the reviewed cumulative content and its checks survive
        // a retarget. The squash may only land while its result tree is exactly
        // that validated tree; a changed tree means the default branch gained
        // content this generation never saw and the chain must revalidate.
        let Some(tree_proof) = tree_proof else {
            self.record_merge_wait(number, RootMergeBlock::CumulativeTreeUnproven, None);
            return Ok(false);
        };
        if !tree_proof.identical {
            self.record_merge_wait(
                number,
                RootMergeBlock::CumulativeTreeChanged,
                Some(tree_proof.reason()),
            );
            return Ok(false);
        }

        let default_before = crate::model::BranchSnapshot {
            repository: repository.clone(),
            name: default_branch.clone(),
            oid: observed_default,
        };
        let next_root = remaining.get(1).copied();
        let next_root_base_before = next_root
            .and_then(|next| self.current.get(&next))
            .map(|next| next.base.name.clone());
        self.ensure_mutation_capacity(1)?;
        let receipt = provider
            .squash_merge(&repository, &self.precondition(number))
            .map_err(|error| mutation_error(&error, self, Some(number)))?;
        self.record(
            receipt,
            "squash-merged the promoted caravan root into the exact default branch",
        );

        let mut observed = self
            .current
            .get(&number)
            .cloned()
            .expect("merge receipt records current facts");
        while reads < ROOT_MERGE_CONFIRMATION_READS && observed.state != PullRequestState::Merged {
            std::thread::sleep(ROOT_MERGE_CONFIRMATION_DELAY);
            observed = provider
                .refetch_pull_request(&repository, number)
                .map_err(|error| mutation_error(&error, self, Some(number)))?;
            self.current.insert(number, observed.clone());
            reads = reads.saturating_add(1);
        }
        if observed.state != PullRequestState::Merged {
            return Err(self.root_merge_failure(
                caravan_id,
                number,
                RootMergeFailureCause::ProviderDidNotPersistMerge,
                &observed,
                Some(&expected_head),
                &json!({ "confirmation_reads": reads }),
            ));
        }
        if observed.base.name != default_branch {
            return Err(self.root_merge_failure(
                caravan_id,
                number,
                RootMergeFailureCause::MergedIntoUnexpectedBase,
                &observed,
                Some(&expected_head),
                &json!({
                    "default_branch": default_branch,
                    "merged_base": observed.base,
                }),
            ));
        }

        // Landing postflight. A merge commit the fetched default branch does not
        // contain never reached the default branch, which is exactly how the
        // live incident presented. Without this proof the root stays open and
        // recoverable and no successor is promoted.
        let default_after = provider
            .branch_head_oid(&repository, &default_branch)
            .map_err(|error| mutation_error(&error, self, Some(number)))?;
        let merge_commit = provider
            .merge_commit_oid(&repository, number)
            .map_err(|error| mutation_error(&error, self, Some(number)))?;
        if let Some(merge_commit) = merge_commit.as_ref() {
            let comparison = provider
                .compare_commits(&repository, merge_commit, &default_after)
                .map_err(|error| mutation_error(&error, self, Some(number)))?;
            if !matches!(
                comparison,
                crate::generation::CommitRelation::Ahead
                    | crate::generation::CommitRelation::Identical
            ) {
                return Err(self.root_merge_failure(
                    caravan_id,
                    number,
                    RootMergeFailureCause::MergeNotReachableFromDefault,
                    &observed,
                    Some(&expected_head),
                    &json!({
                        "default_branch": default_branch,
                        "claimed_merge_commit": merge_commit,
                        "observed_default_oid": default_after,
                        "comparison": comparison,
                    }),
                ));
            }
        }

        let ancestry = RootMergeAncestry {
            default_before,
            default_after: crate::model::BranchSnapshot {
                repository: repository.clone(),
                name: default_branch.clone(),
                oid: default_after,
            },
            merge_commit,
            cumulative_tree: Some(tree_proof),
            predecessor: self
                .head_advancements
                .iter()
                .rev()
                .find(|advancement| advancement.new_head == number)
                .map(|advancement| advancement.merged_predecessor),
            remaining_members: remaining.iter().skip(1).copied().collect(),
            next_root,
            next_root_base_before,
            next_root_base_after: next_root.map(|_| default_branch.clone()),
        };
        self.push_root_merge(caravan_id, &observed, &default_branch, ancestry, reads);
        Ok(true)
    }

    /// Record one visible, non-failing reason a promoted root did not land.
    fn record_merge_wait(
        &mut self,
        number: PrNumber,
        block: RootMergeBlock,
        detail: Option<String>,
    ) {
        let summary = detail.map_or_else(
            || block.reason().to_owned(),
            |detail| format!("{}: {detail}", block.reason()),
        );
        self.already(MutationKind::SquashMerge, number, &summary);
    }

    /// Converge scheduler-owned required squash auto-merge on the exact current
    /// caravan root head.
    ///
    /// Native auto-merge is dropped by the provider whenever the root's head or
    /// base generation is rewritten, and the provider's list projection can
    /// still expose the pre-rewrite `autoMergeRequest`. Deciding this required
    /// invariant from discovery facts therefore silently degrades a caravan
    /// until somebody re-arms it by hand. This convergence instead:
    ///
    /// 1. re-reads the exact current root generation from a fresh single-PR
    ///    provider read;
    /// 2. refuses to prove anything against a generation other than the one this
    ///    tick already verified;
    /// 3. arms and then re-reads until squash auto-merge is proven on the
    ///    resulting head, never on the pre-rebase generation;
    /// 4. persists a sealed receipt carrying auditable engine provenance;
    /// 5. reports a typed cause and defers retry to the next bounded tick when
    ///    arming cannot be proven.
    fn ensure_root_squash_auto_merge(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        caravan_id: PrNumber,
        number: PrNumber,
        discovery: Option<&PullRequestSnapshot>,
        rewritten_head: Option<&crate::model::CommitOid>,
    ) -> Result<(), AppError> {
        let previous = self.current.get(&number).cloned();
        let (mut observed, mut reads) = self.read_exact_root_generation(
            provider,
            repository,
            caravan_id,
            number,
            previous.as_ref(),
            &self.default_branch.clone(),
        )?;
        let proven = self
            .root_auto_merge
            .iter()
            .rev()
            .find(|receipt| receipt.pr == number)
            .map(|receipt| receipt.head.oid.clone());
        let observed_before = observed.auto_merge.clone();
        let trigger = root_auto_merge::classify_trigger(
            discovery,
            &observed,
            rewritten_head,
            proven.as_ref(),
        );

        if !trigger.requires_write() {
            self.already(
                MutationKind::EnableAutoMerge,
                number,
                "exact current caravan root head already carries required squash auto-merge",
            );
            self.push_root_auto_merge(
                caravan_id,
                &observed,
                trigger,
                &observed_before,
                false,
                reads,
                0,
            );
            return Ok(());
        }

        let mut attempts = 0_u32;
        while attempts < ROOT_AUTO_MERGE_ARMING_ATTEMPTS {
            attempts += 1;
            let target_head = observed.head.oid.clone();
            let (armed, arming_reads) =
                self.arm_root_once(provider, repository, number, &observed, &target_head)?;
            observed = armed;
            reads = reads.saturating_add(arming_reads);
            if observed.head.oid != target_head {
                return Err(self.root_auto_merge_failure(
                    caravan_id,
                    number,
                    RootAutoMergeFailureCause::RootHeadMovedDuringArming,
                    trigger,
                    &observed,
                    Some(&target_head),
                    attempts,
                    reads,
                ));
            }
            if root_auto_merge::squash_armed(&observed) {
                self.push_root_auto_merge(
                    caravan_id,
                    &observed,
                    trigger,
                    &observed_before,
                    true,
                    reads,
                    attempts,
                );
                return Ok(());
            }
        }
        Err(self.root_auto_merge_failure(
            caravan_id,
            number,
            RootAutoMergeFailureCause::ProviderDidNotPersistArming,
            trigger,
            &observed,
            None,
            attempts,
            reads,
        ))
    }

    /// Perform one bounded arming attempt and re-read the exact resulting head
    /// until squash auto-merge is proven or the bounded reads are exhausted.
    fn arm_root_once(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        number: PrNumber,
        observed: &PullRequestSnapshot,
        target_head: &crate::model::CommitOid,
    ) -> Result<(PullRequestSnapshot, u32), AppError> {
        if observed.auto_merge.enabled {
            self.ensure_mutation_capacity(1)?;
            let receipt = provider
                .disable_auto_merge(repository, &self.precondition(number))
                .map_err(|error| mutation_error(&error, self, Some(number)))?;
            self.record(
                receipt,
                "disabled non-squash auto-merge on the exact caravan root head",
            );
        }
        self.ensure_mutation_capacity(1)?;
        let receipt = provider
            .enable_squash_auto_merge(repository, &self.precondition(number))
            .map_err(|error| mutation_error(&error, self, Some(number)))?;
        let mut observed = receipt.after.clone();
        self.record(
            receipt,
            "armed scheduler-owned squash auto-merge on the exact caravan root head",
        );
        let mut reads = 1_u32;

        // Bounded confirmation re-reads absorb provider read lag behind an
        // accepted mutation. They never accept a different generation.
        while reads < ROOT_AUTO_MERGE_CONFIRMATION_READS
            && !(&observed.head.oid == target_head && root_auto_merge::squash_armed(&observed))
        {
            std::thread::sleep(ROOT_AUTO_MERGE_CONFIRMATION_DELAY);
            observed = provider
                .refetch_pull_request(repository, number)
                .map_err(|error| mutation_error(&error, self, Some(number)))?;
            self.current.insert(number, observed.clone());
            reads = reads.saturating_add(1);
        }
        Ok((observed, reads))
    }

    /// Read the exact current root generation.
    ///
    /// Bounded re-reads absorb provider read lag behind a generation this tick
    /// already verified. Any other structural divergence from the tick's own
    /// facts stays an ordinary resumable stale-precondition decision instead of
    /// being converged blind.
    fn read_exact_root_generation(
        &mut self,
        provider: &impl SyncProvider,
        repository: &RepositoryId,
        caravan_id: PrNumber,
        number: PrNumber,
        previous: Option<&PullRequestSnapshot>,
        default_branch: &str,
    ) -> Result<(PullRequestSnapshot, u32), AppError> {
        let expected_head = previous.map(|snapshot| snapshot.head.oid.clone());
        let mut observed = provider
            .refetch_pull_request(repository, number)
            .map_err(|error| mutation_error(&error, self, Some(number)))?;
        let mut reads = 1_u32;
        while reads < ROOT_AUTO_MERGE_CONFIRMATION_READS
            && expected_head
                .as_ref()
                .is_some_and(|oid| &observed.head.oid != oid)
        {
            std::thread::sleep(ROOT_AUTO_MERGE_CONFIRMATION_DELAY);
            observed = provider
                .refetch_pull_request(repository, number)
                .map_err(|error| mutation_error(&error, self, Some(number)))?;
            reads = reads.saturating_add(1);
        }
        if let Some(expected) = expected_head.as_ref()
            && &observed.head.oid != expected
        {
            return Err(self.root_promotion_failure(
                caravan_id,
                number,
                RootPromotionFailureCause::StaleProviderView,
                RootPromotionTrigger::AlreadyOnDefaultBranch,
                &observed,
                Some(expected),
                default_branch,
                reads,
            ));
        }
        // Auto-merge and head facts are convergent scheduler-owned state, but a
        // raced membership/state/base transition still belongs to the ordinary
        // optimistic contract and must stop with a resumable decision. Unrelated
        // label churn (priority, force, review metadata) is deliberately not a
        // stop: it cannot make required root arming wrong, and treating it as a
        // decision would turn routine fleet activity into operator babysitting.
        if let Some(previous) = previous {
            let expected = PullRequestPrecondition::from(previous);
            let actual = PullRequestPrecondition::from(&observed);
            let mut changed_fields = crate::github::changed_precondition_fields(&expected, &actual)
                .into_iter()
                .filter(|field| field != "auto_merge" && field != "labels")
                // A base that already advanced *to the exact default branch* is
                // convergence toward the promotion this tick is performing, not
                // a race: GitHub itself retargets a child when its merged
                // predecessor's branch is deleted. Any other base transition
                // stays an ordinary resumable decision.
                .filter(|field| {
                    !matches!(field.as_str(), "base_ref" | "base_oid")
                        || observed.base.name != default_branch
                })
                .collect::<Vec<_>>();
            for label in CARAVAN_CONTROL_LABELS {
                if previous.has_label(label) != observed.has_label(label) {
                    changed_fields.push(format!("labels.{label}"));
                }
            }
            if !changed_fields.is_empty() {
                let error = MutationError::StalePrecondition {
                    expected: Box::new(expected),
                    actual: Box::new(actual),
                    changed_fields,
                };
                return Err(mutation_error(&error, self, Some(number)));
            }
        }
        self.current.insert(number, observed.clone());
        Ok((observed, reads))
    }

    #[allow(clippy::too_many_arguments)]
    fn push_root_promotion(
        &mut self,
        caravan_id: PrNumber,
        observed: &PullRequestSnapshot,
        base_before: crate::model::BranchSnapshot,
        default_branch: &str,
        predecessor: Option<PrNumber>,
        predecessor_merged: bool,
        trigger: RootPromotionTrigger,
        confirmation_reads: u32,
        engine_retargeted: bool,
    ) {
        let receipt = root_merge::promotion_receipt(
            &self.repository,
            caravan_id,
            observed,
            base_before,
            default_branch,
            predecessor,
            predecessor_merged,
            trigger,
            confirmation_reads,
            root_merge::provenance(
                &self.operation_id,
                trigger.reason(),
                engine_retargeted,
                &observed.auto_merge,
            ),
        );
        if engine_retargeted {
            self.events.push(self.event(
                EventKind::RootPromoted,
                Some(caravan_id),
                vec![observed.number],
                Some(trigger.reason().to_owned()),
                BTreeMap::from([
                    ("root_promotion_receipt".to_owned(), json!(receipt.clone())),
                    ("trigger".to_owned(), json!(trigger)),
                ]),
            ));
        }
        self.root_promotion
            .retain(|existing| existing.pr != receipt.pr);
        self.root_promotion.push(receipt);
    }

    fn push_root_merge(
        &mut self,
        caravan_id: PrNumber,
        observed: &PullRequestSnapshot,
        default_branch: &str,
        ancestry: RootMergeAncestry,
        confirmation_reads: u32,
    ) {
        let receipt = root_merge::merge_receipt(
            &self.repository,
            caravan_id,
            observed,
            default_branch,
            ancestry,
            confirmation_reads,
            root_merge::provenance(
                &self.operation_id,
                "caravan-owned squash merge of the exact promoted root head",
                true,
                &observed.auto_merge,
            ),
        );
        self.events.push(self.event(
            EventKind::RootMerged,
            Some(caravan_id),
            vec![observed.number],
            Some(format!(
                "squash-merged caravan root #{} into {default_branch}",
                observed.number
            )),
            BTreeMap::from([("root_merge_receipt".to_owned(), json!(receipt.clone()))]),
        ));
        self.root_merge.retain(|existing| existing.pr != receipt.pr);
        self.root_merge.push(receipt);
    }

    /// Typed `root_promotion_incomplete` evidence. Emitted *before* any merge.
    #[allow(clippy::too_many_arguments)]
    fn root_promotion_failure(
        &self,
        caravan_id: PrNumber,
        number: PrNumber,
        cause: RootPromotionFailureCause,
        trigger: RootPromotionTrigger,
        observed: &PullRequestSnapshot,
        expected_head: Option<&crate::model::CommitOid>,
        default_branch: &str,
        confirmation_reads: u32,
    ) -> AppError {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "root_promotion_incomplete",
            format!(
                "caravan root #{number} could not be proven to target the exact default branch before merging ({})",
                cause.code()
            ),
            Some(json!({
                "cause": cause,
                "cause_code": cause.code(),
                "trigger": trigger,
                "trigger_reason": trigger.reason(),
                "caravan_id": caravan_id,
                "affected_pr": number,
                "default_branch": default_branch,
                "observed_head": observed.head.oid,
                "expected_head": expected_head,
                "observed_base": observed.base,
                "confirmation_reads": confirmation_reads,
                "confirmation_read_limit": ROOT_MERGE_CONFIRMATION_READS,
                "head_merge_actor": self.head_merge_actor,
                "operation_receipt": self.operation_receipt(),
                "provider_receipts": self.provider_receipts,
                "root_promotion": self.root_promotion,
                "root_merge": self.root_merge,
                "events": self.events,
                "merged": false,
                "operator_action_required": false,
                "resumable": true,
                "next": cause.next(),
            })),
        )
    }

    /// Typed `root_merge_refused` evidence for the caravan-owned merge actor.
    fn root_merge_failure(
        &self,
        caravan_id: PrNumber,
        number: PrNumber,
        cause: RootMergeFailureCause,
        observed: &PullRequestSnapshot,
        expected_head: Option<&crate::model::CommitOid>,
        extra: &Value,
    ) -> AppError {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "root_merge_refused",
            format!(
                "caravan root #{number} was not merged by cara ({})",
                cause.code()
            ),
            Some(json!({
                "cause": cause,
                "cause_code": cause.code(),
                "caravan_id": caravan_id,
                "affected_pr": number,
                "observed_head": observed.head.oid,
                "expected_head": expected_head,
                "observed_state": observed.state,
                "observed_base": observed.base,
                "evidence": extra,
                "head_merge_actor": self.head_merge_actor,
                "external_auto_merge_policy": self.external_auto_merge_policy,
                "operation_receipt": self.operation_receipt(),
                "provider_receipts": self.provider_receipts,
                "root_promotion": self.root_promotion,
                "root_merge": self.root_merge,
                "events": self.events,
                "operator_action_required": !cause.resumable(),
                "resumable": cause.resumable(),
                "next": cause.next(),
            })),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn push_root_auto_merge(
        &mut self,
        caravan_id: PrNumber,
        observed: &PullRequestSnapshot,
        trigger: RootAutoMergeTrigger,
        observed_before: &crate::model::AutoMergeState,
        engine_armed: bool,
        confirmation_reads: u32,
        arming_attempts: u32,
    ) {
        let receipt = root_auto_merge::receipt(
            &self.repository,
            caravan_id,
            observed,
            root_auto_merge::provenance(&self.operation_id, trigger, observed_before, engine_armed),
            confirmation_reads,
            arming_attempts,
        );
        if engine_armed {
            self.events.push(self.event(
                EventKind::RootAutoMergeArmed,
                Some(caravan_id),
                vec![observed.number],
                Some(trigger.reason().to_owned()),
                BTreeMap::from([
                    ("root_auto_merge_receipt".to_owned(), json!(receipt.clone())),
                    ("trigger".to_owned(), json!(trigger)),
                ]),
            ));
        }
        self.root_auto_merge
            .retain(|existing| existing.pr != receipt.pr);
        self.root_auto_merge.push(receipt);
    }

    #[allow(clippy::too_many_arguments)]
    fn root_auto_merge_failure(
        &self,
        caravan_id: PrNumber,
        number: PrNumber,
        cause: RootAutoMergeFailureCause,
        trigger: RootAutoMergeTrigger,
        observed: &PullRequestSnapshot,
        expected_head: Option<&crate::model::CommitOid>,
        arming_attempts: u32,
        confirmation_reads: u32,
    ) -> AppError {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "root_auto_merge_not_durable",
            format!(
                "caravan root #{number} could not be proven to carry required squash auto-merge on its exact current head ({})",
                cause.code()
            ),
            Some(json!({
                "cause": cause,
                "cause_code": cause.code(),
                "trigger": trigger,
                "trigger_reason": trigger.reason(),
                "provenance": root_auto_merge::provenance(
                    &self.operation_id,
                    trigger,
                    &observed.auto_merge,
                    false,
                ),
                "caravan_id": caravan_id,
                "affected_pr": number,
                "observed_head": observed.head.oid,
                "expected_head": expected_head,
                "observed_auto_merge": observed.auto_merge,
                "arming_attempts": arming_attempts,
                "arming_attempt_limit": ROOT_AUTO_MERGE_ARMING_ATTEMPTS,
                "confirmation_reads": confirmation_reads,
                "confirmation_read_limit": ROOT_AUTO_MERGE_CONFIRMATION_READS,
                "operation_receipt": self.operation_receipt(),
                "provider_receipts": self.provider_receipts,
                "root_auto_merge": self.root_auto_merge,
                "events": self.events,
                "operator_action_required": false,
                "resumable": true,
                "next": cause.next(),
            })),
        )
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
