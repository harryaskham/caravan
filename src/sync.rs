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
    RequiredRunsStatus,
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
use crate::writer_guard::WriterOperationGuard;
use crate::{AppContext, AppError, CheckInput, SyncInput};

mod budget;
mod decision;
mod plan;
pub mod progress;
pub use budget::{CapacityDefect, CaravanBudgetProjection, SyncBudgetStatus, project_status};
use budget::{
    CapacityGate, ChainCost, MemberCost, PhysicalApplyAdmission, PhysicalCommitBudget,
    admit_physical_prefix, capacity_evidence, capacity_gate, configured_batch_bound,
    externally_armed_non_roots,
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
const PARKED_LABEL: &str = "caravan-parked";
const CLOSED_LABEL: &str = "caravan-closed";
/// Exact remote range/target verification plus one force-with-lease push.
const PHYSICAL_APPLY_COMMAND_SLOTS_PER_PENDING_MEMBER: u64 = 3;
/// A member whose exact cumulative ancestry already holds still revalidates
/// its range and target generations, but never pushes.
const PHYSICAL_APPLY_COMMAND_SLOTS_PER_RETAINED_MEMBER: u64 = 2;
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
    /// Whether the red evidence is cancellation rather than a verdict.
    #[serde(default)]
    pub cancellation: CiCancellationSummary,
}

/// Exact classification separating "did not finish" from "was judged and failed".
///
/// Cancellation is not a verdict: it usually means contended runner capacity, a
/// superseded push, or a killed upstream producer, so the change was never
/// evaluated. Aggregate required checks that convert a cancelled prerequisite
/// into a terminal failure make the two indistinguishable in the forge summary,
/// and two of three cancellations measured on a live repository were spurious.
/// Any actor deciding on "red" therefore needs this distinction inline, without
/// another provider round-trip (bd-1ac172).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CiCancellationSummary {
    /// Required checks the provider reported as cancelled.
    #[serde(default)]
    pub cancelled_checks: Vec<String>,
    /// Terminal failures whose bounded diagnostics name no failing step, which
    /// is the shape an aggregate check produces from a cancelled prerequisite.
    #[serde(default)]
    pub failures_without_failing_step: Vec<String>,
    /// True when every piece of terminal red evidence is cancellation or a
    /// stepless aggregate conversion, and nothing was actually judged to fail.
    pub cancellation_only: bool,
}

impl CiCancellationSummary {
    /// True when any cancellation evidence exists at all.
    #[must_use]
    pub fn observed(&self) -> bool {
        !self.cancelled_checks.is_empty() || !self.failures_without_failing_step.is_empty()
    }
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

/// Exact terminal lifecycle outcome for a PR carrying `caravan-closed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClosedLifecycleDisposition {
    /// Provider reports CLOSED with no merge timestamp.
    ClosedUnmerged,
    /// A formerly terminal generation is open again.
    Reopened,
    /// Provider merge evidence wins over any stale terminal label.
    Merged,
}

/// Trusted-sync receipt for one closed lifecycle label transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClosedLifecycleTransitionReceipt {
    pub pr: PrNumber,
    pub disposition: ClosedLifecycleDisposition,
    pub before: PullRequestSnapshot,
    pub after: PullRequestSnapshot,
    #[serde(default)]
    pub removed_active_labels: Vec<String>,
    pub terminal_label_added: bool,
    /// The one exact-precondition complete-label replacement, with provider
    /// before/after facts. A transition never exposes a multi-write partial state.
    #[serde(default)]
    pub provider_receipts: Vec<GitHubMutationReceipt>,
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

/// Why the exact front of a queue cannot proceed.
///
/// Frequency never fixes head-of-line blocking: a conflicted or check-blocked
/// front member re-confirms the same refusal on every tick while mergeable work
/// waits behind it. Naming the exact blocking position, its class, and its
/// remedies is what lets a hook repair or evict *that* member instead of
/// greedily fixing whichever later member looks cheapest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HeadOfLineBlockKind {
    /// Textual incompatibility with the exact target; syncing cannot resolve it.
    Conflict,
    /// Terminal or unknown CI on the exact current generation.
    CiFailure,
    /// Required contexts have no reporting run on the exact head.
    MissingRequiredRuns,
    /// Structural graph problem naming this member.
    InvalidGraph,
    /// Provider or policy state refuses the canonical admission attempt.
    AdmissionRejected,
}

/// Bounded receipt naming the exact member that blocks queue progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HeadOfLineStall {
    pub kind: HeadOfLineBlockKind,
    /// Caravan whose front is blocked; absent for an admission-order stall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caravan_id: Option<PrNumber>,
    pub blocking_pr: PrNumber,
    /// One-based position in the blocked ordering.
    pub position: usize,
    /// Members or candidates that cannot proceed until this one does.
    #[serde(default)]
    pub blocked_prs: Vec<PrNumber>,
    pub evidence: String,
    /// Exact operator/hook continuations, ordered most direct first.
    #[serde(default)]
    pub remedies: Vec<String>,
    /// Stable identity for deduplicating receipts and counting no-progress passes.
    pub fingerprint: String,
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
    /// Exact members or candidates that block all progress behind them. Empty
    /// means the tick is genuinely idle rather than silently stuck.
    #[serde(default)]
    pub head_of_line: Vec<HeadOfLineStall>,
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
    /// Forming another caravan would exceed `sync.max_caravans`. Existing
    /// excess remains untouched and parked caravans do not consume capacity.
    MaxCaravansReached,
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
    /// Caravan already holds the configured `max_caravan_length` batch bound,
    /// so deterministic admission must use another caravan instead of growing
    /// this one. Always false without a configured bound.
    #[serde(default)]
    pub batch_full: bool,
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
    /// Exact repository-wide refusal when forming another caravan would exceed
    /// `sync.max_caravans`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet_capacity_refusal: Option<CaravanFleetCapacityRefusal>,
}

/// Typed, zero-write repository-wide capacity evidence for a new caravan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CaravanFleetCapacityRefusal {
    pub code: String,
    pub candidate_pr: PrNumber,
    pub max_caravans: u32,
    pub active_caravans: usize,
    #[serde(default)]
    pub active_caravan_ids: Vec<PrNumber>,
    pub parked_caravans: usize,
    #[serde(default)]
    pub parked_caravan_ids: Vec<PrNumber>,
    /// Existing active caravans above the configured fence. They remain valid
    /// and continue converging; capacity is never destructive repair authority.
    pub excess_active_caravans: usize,
    pub safe_next_action: String,
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
            fleet_capacity_refusal: None,
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
            fleet_capacity_refusal: None,
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
    pub fleet_capacity_refusal: Option<CaravanFleetCapacityRefusal>,
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
    /// Why a real tick would refuse to start, when the plan itself still renders.
    ///
    /// Tick bounds are enforced in `sync_with_lock` rather than at config load,
    /// so a bad budget cannot silence read-only surfaces. Without this the plan
    /// described operations a tick would never reach, which converts "I checked"
    /// into false confidence in the one surface consulted precisely by people who
    /// do not yet trust the tick (bd-765c65).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick_refusal: Option<String>,
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

/// Compact evidence that one sync pass actually ran, and what it saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncTickReceipt {
    pub schema_version: u32,
    /// Verb this pass performed, so a receipt is never ambiguous about which
    /// command produced it.
    pub verb: String,
    /// Caravans observed after the pass.
    pub caravans: usize,
    /// Unqueued pull requests observed after the pass. A non-zero count with
    /// zero joins is the exact shape of "running but not joining".
    pub unqueued: usize,
    /// Caravans this pass synchronized.
    pub synchronized: usize,
    /// Automatic joins this pass performed.
    pub joins: usize,
    /// Whether this pass mutated anything at all.
    pub changed: bool,
}

/// Stable result of one converged synchronization tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncOutput {
    pub receipt: OperationReceipt,
    /// One compact per-pass receipt (bd-180cd3).
    ///
    /// A fleet once spent hours unable to tell "the loop is running and
    /// declining to join" from "the loop is not running at all", testing
    /// hypotheses against a process that did not exist. One line per tick
    /// naming the verb and the counts it saw makes those two states
    /// distinguishable at a glance, in a log or over a shoulder.
    pub tick: SyncTickReceipt,
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
    /// Closed/unmerged, reopened, and merged label convergence performed before
    /// any active repair, rebase, capacity, or auto-merge logic.
    #[serde(default)]
    pub closed_lifecycle_transitions: Vec<ClosedLifecycleTransitionReceipt>,
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
    /// Durable native Stack landing transactions completed or still pending in
    /// this tick. Absent on the stable Caravan backend.
    #[serde(default)]
    pub native_stack_land: Vec<crate::github::GitHubStackLandCheckpoint>,
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
    /// Every complete, fresh provider Stack generation intersecting the
    /// candidate caravan. Default refuses so a provider double cannot turn an
    /// unimplemented inventory read into false absence and ordinary PR merge.
    fn native_stack_intersections_for_sync(
        &self,
        _repository: &RepositoryId,
        _members: &[PrNumber],
    ) -> Result<Vec<crate::github::GitHubStackGeneration>, AppError> {
        Err(AppError::structured(
            ErrorCategory::Validation,
            "github_stack_routing_read_refused",
            "this sync provider does not implement exact native Stack intersection reads",
            Some(json!({
                "mutated": false,
                "retryable": false,
                "safe_next_action": "use a provider with complete native Stack inventory support or set stack_type: caravan through review",
            })),
        ))
    }

    /// Exact native Stack generation. Default refuses so test/provider doubles
    /// and the stable Caravan path gain no accidental native authority.
    fn native_stack_generation_for_sync(
        &self,
        _repository: &RepositoryId,
        _stack_number: u64,
    ) -> Result<Option<crate::github::GitHubStackGeneration>, AppError> {
        Err(AppError::validation(
            "github_stack_sync_provider_unavailable",
            "this sync provider does not implement native Stack reads",
        ))
    }

    fn native_stack_land_lock_for_sync(
        &self,
        _repository: &RepositoryId,
        _checkpoint: &crate::github::GitHubStackLandCheckpoint,
    ) -> Result<crate::github::GitHubStackLandCheckpoint, AppError> {
        Err(AppError::validation(
            "github_stack_sync_provider_unavailable",
            "this sync provider does not implement native Stack lock acquisition",
        ))
    }

    fn native_stack_land_submit_for_sync(
        &self,
        _repository: &RepositoryId,
        _checkpoint: &crate::github::GitHubStackLandCheckpoint,
    ) -> Result<crate::github::GitHubStackLandCheckpoint, AppError> {
        Err(AppError::validation(
            "github_stack_sync_provider_unavailable",
            "this sync provider does not implement native Stack submission",
        ))
    }

    fn native_stack_land_poll_for_sync(
        &self,
        _repository: &RepositoryId,
        _checkpoint: &crate::github::GitHubStackLandCheckpoint,
    ) -> Result<crate::github::GitHubStackLandCheckpoint, AppError> {
        Err(AppError::validation(
            "github_stack_sync_provider_unavailable",
            "this sync provider does not implement native Stack polling",
        ))
    }

    fn native_stack_land_release_for_sync(
        &self,
        _repository: &RepositoryId,
        _checkpoint: &crate::github::GitHubStackLandCheckpoint,
    ) -> Result<crate::github::GitHubStackLandCheckpoint, AppError> {
        Err(AppError::validation(
            "github_stack_sync_provider_unavailable",
            "this sync provider does not implement native Stack lock release",
        ))
    }

    fn verify_pull_request(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<PullRequestSnapshot, MutationError>;

    /// Refetch mutation identity plus the exact check observation that decided
    /// a CI-sensitive transition.
    fn verify_pull_request_with_checks(
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

    /// Replace all labels in one provider mutation after an exact-state read.
    fn replace_labels(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        labels: &BTreeSet<String>,
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

fn native_sync_error(error: impl std::fmt::Display) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "github_stack_sync_incomplete",
        error.to_string(),
        Some(json!({
            "resumable": true,
            "safe_next_action": "rerun the same sync; the persisted native Stack checkpoint resumes without repeating a proven lock, submission, merge, or release",
        })),
    )
}

fn native_routing_error(error: &crate::github::GitHubStackMutationError) -> AppError {
    AppError::structured(
        ErrorCategory::Validation,
        "github_stack_routing_read_refused",
        format!("exact native Stack routing preflight failed: {error}"),
        Some(json!({
            "source": format!("{error:?}"),
            "mutated": false,
            "retryable": false,
            "safe_next_action": "inspect provider Stack capability and the complete intersecting inventory; rerun only after the provider evidence or repository policy changes",
        })),
    )
}

impl<R: crate::command::CommandRunner> SyncProvider for GitHubMutationAdapter<R> {
    fn native_stack_intersections_for_sync(
        &self,
        repository: &RepositoryId,
        members: &[PrNumber],
    ) -> Result<Vec<crate::github::GitHubStackGeneration>, AppError> {
        self.native_stack_intersections(repository, members)
            .map_err(|error| native_routing_error(&error))
    }

    fn native_stack_generation_for_sync(
        &self,
        repository: &RepositoryId,
        stack_number: u64,
    ) -> Result<Option<crate::github::GitHubStackGeneration>, AppError> {
        self.native_stack_generation(repository, stack_number)
            .map_err(native_sync_error)
    }

    fn native_stack_land_lock_for_sync(
        &self,
        repository: &RepositoryId,
        checkpoint: &crate::github::GitHubStackLandCheckpoint,
    ) -> Result<crate::github::GitHubStackLandCheckpoint, AppError> {
        self.native_stack_land_lock(repository, checkpoint)
            .map_err(native_sync_error)
    }

    fn native_stack_land_submit_for_sync(
        &self,
        repository: &RepositoryId,
        checkpoint: &crate::github::GitHubStackLandCheckpoint,
    ) -> Result<crate::github::GitHubStackLandCheckpoint, AppError> {
        self.native_stack_land_submit(repository, checkpoint)
            .map_err(native_sync_error)
    }

    fn native_stack_land_poll_for_sync(
        &self,
        repository: &RepositoryId,
        checkpoint: &crate::github::GitHubStackLandCheckpoint,
    ) -> Result<crate::github::GitHubStackLandCheckpoint, AppError> {
        self.native_stack_land_poll(repository, checkpoint)
            .map_err(native_sync_error)
    }

    fn native_stack_land_release_for_sync(
        &self,
        repository: &RepositoryId,
        checkpoint: &crate::github::GitHubStackLandCheckpoint,
    ) -> Result<crate::github::GitHubStackLandCheckpoint, AppError> {
        self.native_stack_land_release(repository, checkpoint)
            .map_err(native_sync_error)
    }

    fn verify_pull_request(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<PullRequestSnapshot, MutationError> {
        self.verify_precondition(repository, expected)
    }

    fn verify_pull_request_with_checks(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<PullRequestSnapshot, MutationError> {
        self.verify_precondition_with_checks(repository, expected)
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

    fn replace_labels(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        labels: &BTreeSet<String>,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.replace_labels(repository, expected, labels)
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
    let prepared = crate::sync_authority::prepare(context)?;
    sync_with_optional_writer_guard(prepared.context(), input, None, prepared.authority())
}

pub(crate) fn sync_with_writer_guard(
    context: &AppContext,
    input: &SyncInput,
    writer_guard: WriterOperationGuard,
) -> Result<SyncOutput, AppError> {
    let prepared = crate::sync_authority::prepare(context)?;
    sync_with_optional_writer_guard(
        prepared.context(),
        input,
        Some(writer_guard),
        prepared.authority(),
    )
}

pub(crate) fn sync_prepared(
    context: &AppContext,
    input: &SyncInput,
    authority: Option<&crate::sync_authority::DefaultBranchAuthority>,
) -> Result<SyncOutput, AppError> {
    sync_with_optional_writer_guard(context, input, None, authority)
}

fn sync_with_optional_writer_guard(
    context: &AppContext,
    input: &SyncInput,
    writer_guard: Option<WriterOperationGuard>,
    authority: Option<&crate::sync_authority::DefaultBranchAuthority>,
) -> Result<SyncOutput, AppError> {
    let started = Instant::now();
    let budget = sync_operation_budget(context);
    let operation_deadline = started + budget;
    match sync_without_hooks(
        context,
        input,
        started,
        operation_deadline,
        writer_guard,
        authority,
    ) {
        Ok(mut output) => {
            output.hook_deliveries =
                hooks::dispatch_events_before(context, &output.events, operation_deadline)?;
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
            let deliveries = hooks::dispatch_events_before(context, &events, operation_deadline)?;
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

#[derive(Debug, Default)]
struct ClosedLifecycleReconciliation {
    changed: bool,
    steps: Vec<MutationStep>,
    provider_receipts: Vec<GitHubMutationReceipt>,
    transitions: Vec<ClosedLifecycleTransitionReceipt>,
}

fn closed_lifecycle_failure_classification(error: &MutationError) -> &'static str {
    match error {
        MutationError::StalePrecondition { actual, .. } => match actual.state {
            PullRequestState::Open => "provider_race_reopened",
            PullRequestState::Merged => "provider_race_merged",
            PullRequestState::Closed if actual.merged_at.is_some() => "provider_race_merged",
            PullRequestState::Closed => "provider_race_closed_generation_changed",
        },
        MutationError::Provider(_) => "provider_mutation_failed",
        _ => "provider_mutation_refused",
    }
}

fn closed_lifecycle_provider_read_error(
    error: &MutationError,
    planned: &PullRequestSnapshot,
) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "closed_pr_terminalization_provider_read_failed",
        format!(
            "could not freshly read provider state for PR #{} immediately before label cleanup: {error}",
            planned.number
        ),
        Some(json!({
            "classification": "provider_state_unknown",
            "operation": "fresh_provider_read",
            "pr": planned.number,
            "planned": planned,
            "source": error.to_string(),
            "mutated": false,
            "resumable": true,
            "branch_action": "preserved",
            "safe_next_action": "rerun the same trusted sync; cleanup requires a fresh authoritative CLOSED-without-merge read",
        })),
    )
}

fn closed_lifecycle_plan_drift_error(
    planned: &PullRequestSnapshot,
    fresh: &PullRequestSnapshot,
) -> AppError {
    let classification = if fresh.is_merged() {
        "provider_race_merged"
    } else if fresh.state == PullRequestState::Open {
        "provider_race_reopened"
    } else {
        "provider_race_closed_generation_changed"
    };
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "closed_pr_terminalization_plan_drift",
        format!(
            "PR #{} changed after cleanup planning; no labels were written",
            planned.number
        ),
        Some(json!({
            "classification": classification,
            "operation": "fresh_provider_read",
            "pr": planned.number,
            "planned": planned,
            "fresh": fresh,
            "mutated": false,
            "resumable": true,
            "branch_action": "preserved",
            "safe_next_action": "rerun the same trusted sync from fresh provider discovery",
        })),
    )
}

fn closed_lifecycle_mutation_error(
    error: &MutationError,
    operation: &str,
    before: &PullRequestSnapshot,
    completed: &[GitHubMutationReceipt],
) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "closed_pr_terminalization_failed",
        format!("closed PR lifecycle transition failed during {operation}: {error}"),
        Some(json!({
            "classification": closed_lifecycle_failure_classification(error),
            "operation": operation,
            "pr": before.number,
            "before": before,
            "completed_provider_receipts": completed,
            "source": error.to_string(),
            "resumable": true,
            "branch_action": "preserved",
            "safe_next_action": "rerun the same trusted sync; fresh provider state is the only cursor",
        })),
    )
}

fn closed_lifecycle_postcondition_error(
    operation: &str,
    before: &PullRequestSnapshot,
    after: &PullRequestSnapshot,
    completed: &[GitHubMutationReceipt],
) -> AppError {
    let classification = if after.is_merged() {
        "provider_race_merged"
    } else if after.state == PullRequestState::Open {
        "provider_race_reopened"
    } else {
        "provider_race_closed_generation_changed"
    };
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "closed_pr_terminalization_provider_race",
        format!(
            "PR #{} changed provider lifecycle state during {operation}",
            before.number
        ),
        Some(json!({
            "classification": classification,
            "operation": operation,
            "pr": before.number,
            "before": before,
            "after": after,
            "completed_provider_receipts": completed,
            "resumable": true,
            "branch_action": "preserved",
            "safe_next_action": "rerun the same trusted sync; reopened and merged rows are reconciled without active-label repair",
        })),
    )
}

fn record_closed_lifecycle_mutation(
    output: &mut ClosedLifecycleReconciliation,
    receipt: GitHubMutationReceipt,
    summary: &str,
) {
    output.changed = true;
    output.steps.push(MutationStep {
        kind: receipt.kind,
        state: MutationStepState::Completed,
        pr: Some(receipt.after.number),
        summary: summary.to_owned(),
    });
    output.provider_receipts.push(receipt);
}

/// Converge terminal closed records before any active queue logic runs.
///
/// Closed-unmerged rows gain `caravan-closed` while active labels are removed
/// in one complete-label provider mutation, so a race cannot expose partial
/// lifecycle labels. Open/merged rows lose only a stale terminal label. Every
/// candidate is freshly refetched and exact-precondition fenced immediately
/// before its single write; a changed pass returns before repair/rebase/auto-
/// merge so terminal records never trigger active work.
// Keep the fresh-read lease, one-write label transaction, and lifecycle-specific
// postconditions visible in one linear safety boundary.
#[allow(clippy::too_many_lines)]
fn reconcile_closed_lifecycle(
    status: &StatusOutput,
    provider: &impl SyncProvider,
) -> Result<ClosedLifecycleReconciliation, AppError> {
    let mut output = ClosedLifecycleReconciliation::default();
    let candidates = status
        .analysis
        .pull_requests
        .values()
        .filter(|pull_request| {
            pull_request.is_closed_unmerged() || pull_request.has_label(CLOSED_LABEL)
        })
        .cloned()
        .collect::<Vec<_>>();

    for planned in candidates {
        let receipt_start = output.provider_receipts.len();
        // Discovery may be historical last-good evidence. Eligibility is
        // therefore established again from a fresh authoritative provider read
        // before constructing any write. Open, merged, reopened, or otherwise
        // drifted facts refuse the stale plan with zero label mutations.
        let fresh = provider
            .refetch_pull_request(&status.repository, planned.number)
            .map_err(|error| closed_lifecycle_provider_read_error(&error, &planned))?;
        if !PullRequestPrecondition::from(&planned)
            .mutation_identity_eq(&PullRequestPrecondition::from(&fresh))
        {
            return Err(closed_lifecycle_plan_drift_error(&planned, &fresh));
        }

        if fresh.is_closed_unmerged() {
            let mut desired_labels = fresh.labels.clone();
            let terminal_label_added = desired_labels.insert(CLOSED_LABEL.to_owned());
            let removed_active_labels = [PARKED_LABEL, "caravan"]
                .into_iter()
                .filter(|label| desired_labels.remove(*label))
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if !terminal_label_added && removed_active_labels.is_empty() {
                continue;
            }

            // One complete-label replacement avoids the observable partial
            // states produced by add-then-remove sequencing. The adapter repeats
            // the exact provider precondition immediately before this one write.
            let receipt = provider
                .replace_labels(
                    &status.repository,
                    &PullRequestPrecondition::from(&fresh),
                    &desired_labels,
                )
                .map_err(|error| {
                    closed_lifecycle_mutation_error(
                        &error,
                        "replace_terminal_labels",
                        &planned,
                        &output.provider_receipts[receipt_start..],
                    )
                })?;
            let current = receipt.after.clone();
            record_closed_lifecycle_mutation(
                &mut output,
                receipt,
                "atomically replaced closed-unmerged PR lifecycle labels",
            );
            if !current.is_closed_unmerged() || current.labels != desired_labels {
                return Err(closed_lifecycle_postcondition_error(
                    "replace_terminal_labels",
                    &planned,
                    &current,
                    &output.provider_receipts[receipt_start..],
                ));
            }
            output.transitions.push(ClosedLifecycleTransitionReceipt {
                pr: planned.number,
                disposition: ClosedLifecycleDisposition::ClosedUnmerged,
                before: planned,
                after: current,
                removed_active_labels,
                terminal_label_added,
                provider_receipts: output.provider_receipts[receipt_start..].to_vec(),
            });
            continue;
        }

        // `caravan-closed` never survives an independently discovered reopen or
        // merge. Remove only that marker in one complete-label write; active and
        // parked membership labels are preserved byte-for-byte.
        if fresh.has_label(CLOSED_LABEL) {
            let disposition = if fresh.is_merged() {
                ClosedLifecycleDisposition::Merged
            } else {
                ClosedLifecycleDisposition::Reopened
            };
            let mut desired_labels = fresh.labels.clone();
            desired_labels.remove(CLOSED_LABEL);
            let receipt = provider
                .replace_labels(
                    &status.repository,
                    &PullRequestPrecondition::from(&fresh),
                    &desired_labels,
                )
                .map_err(|error| {
                    closed_lifecycle_mutation_error(
                        &error,
                        "remove_stale_terminal_label",
                        &planned,
                        &output.provider_receipts[receipt_start..],
                    )
                })?;
            let current = receipt.after.clone();
            record_closed_lifecycle_mutation(
                &mut output,
                receipt,
                "removed caravan-closed while preserving open or merged labels",
            );
            let lifecycle_matches = match disposition {
                ClosedLifecycleDisposition::Merged => current.is_merged(),
                ClosedLifecycleDisposition::Reopened => {
                    current.state == PullRequestState::Open && current.merged_at.is_none()
                }
                ClosedLifecycleDisposition::ClosedUnmerged => false,
            };
            if !lifecycle_matches || current.labels != desired_labels {
                return Err(closed_lifecycle_postcondition_error(
                    "remove_stale_terminal_label",
                    &planned,
                    &current,
                    &output.provider_receipts[receipt_start..],
                ));
            }
            output.transitions.push(ClosedLifecycleTransitionReceipt {
                pr: planned.number,
                disposition,
                before: planned,
                after: current,
                removed_active_labels: Vec::new(),
                terminal_label_added: false,
                provider_receipts: output.provider_receipts[receipt_start..].to_vec(),
            });
        }
    }
    Ok(output)
}

#[derive(Default)]
struct ParkingReconciliation {
    changed: bool,
    steps: Vec<MutationStep>,
    provider_receipts: Vec<GitHubMutationReceipt>,
    events: Vec<CaravanEvent>,
}

/// Reconcile explicit terminal-red quarantine before active capacity and
/// convergence are selected. Default `block` performs no work and preserves
/// historical behavior exactly.
// Keep transition ordering (evidence -> label -> auto-merge -> event) visible at
// one transactional boundary; partially initialized parking helpers are harder
// to audit than this linear policy.
#[allow(clippy::too_many_lines)]
fn reconcile_terminal_red_parking(
    context: &AppContext,
    status: &StatusOutput,
    provider: &impl SyncProvider,
) -> Result<ParkingReconciliation, AppError> {
    if context.config.sync.terminal_red.action != crate::config::TerminalRedAction::Park {
        return Ok(ParkingReconciliation::default());
    }
    let mut output = ParkingReconciliation::default();
    for caravan in &status.analysis.fleet.caravans {
        let receipt_start = output.provider_receipts.len();
        let mut failures = Vec::new();
        let mut verdicts = Vec::new();
        let mut all_members_green = true;
        for number in &caravan.members {
            let pull_request = status
                .analysis
                .pull_requests
                .get(number)
                .expect("caravan member has provider facts");
            let (current, superseded) =
                crate::model::latest_checks_per_identity(&pull_request.checks);
            let terminal = current
                .iter()
                .filter(|check| {
                    matches!(
                        check.state,
                        CheckState::Failure
                            | CheckState::Cancelled
                            | CheckState::TimedOut
                            | CheckState::ActionRequired
                    )
                })
                .copied()
                .collect::<Vec<_>>();
            let member_green = !current.is_empty()
                && current.iter().all(|check| {
                    matches!(
                        check.state,
                        CheckState::Success | CheckState::Neutral | CheckState::Skipped
                    )
                });
            all_members_green &= member_green;
            let classification = if !terminal.is_empty() {
                parking_failure_classification(&current)
            } else if member_green {
                "green"
            } else if current.iter().any(|check| {
                matches!(
                    check.state,
                    CheckState::Expected | CheckState::Queued | CheckState::InProgress
                )
            }) {
                "pending"
            } else {
                "unknown"
            };
            verdicts.push(json!({
                "pr": number,
                "head": pull_request.head.oid,
                "checks": &current,
                "superseded_checks": &superseded,
                "classification": classification,
            }));
            if !terminal.is_empty() {
                failures.push(json!({
                    "pr": number,
                    "head": pull_request.head.oid,
                    "checks": &terminal,
                    "superseded_checks": &superseded,
                    "classification": classification,
                }));
            }
        }
        let head = status
            .analysis
            .pull_requests
            .get(&caravan.id)
            .expect("caravan head has provider facts");
        let should_park = !failures.is_empty();
        let is_parked = head.has_label(PARKED_LABEL);
        let green_required_runs = if is_parked && !should_park && all_members_green {
            parking_required_runs_green(status, caravan, provider)
        } else {
            None
        };
        // Parking is a three-state transition. Terminal red parks, complete
        // protection-declared green evidence can unpark, and
        // pending/unknown/incomplete evidence preserves the existing label.
        // Treating every non-red observation as recovery made label-triggered
        // checks flap the same immutable head.
        let should_unpark = green_required_runs.is_some();
        if is_parked && !should_unpark {
            // Parked heads must never remain armed even after a partial earlier
            // transition. This is idempotent and exact-precondition fenced.
            if head.auto_merge.enabled {
                let receipt = provider
                    .disable_auto_merge(&status.repository, &PullRequestPrecondition::from(head))
                    .map_err(|error| parking_mutation_error(&error, caravan, &failures))?;
                output.changed = true;
                output.steps.push(MutationStep {
                    kind: MutationKind::DisableAutoMerge,
                    state: MutationStepState::Completed,
                    pr: Some(caravan.id),
                    summary: "disabled auto-merge on parked caravan head".to_owned(),
                });
                output.provider_receipts.push(receipt);
            }
            continue;
        }
        if !is_parked && !should_park {
            continue;
        }

        // Unparking reactivates an already-enrolled caravan; it never forms an
        // additional one. `sync.max_caravans` is an admission fence only, so an
        // already-excess fleet must keep converging instead of being frozen at
        // the moment a parked generation becomes green.
        let expected = PullRequestPrecondition::from(head);
        provider
            .verify_pull_request_with_checks(&status.repository, &expected)
            .map_err(|error| parking_mutation_error(&error, caravan, &failures))?;
        let receipt = if should_park {
            provider.add_label(&status.repository, &expected, PARKED_LABEL)
        } else {
            provider.remove_label(&status.repository, &expected, PARKED_LABEL)
        }
        .map_err(|error| parking_mutation_error(&error, caravan, &failures))?;
        output.changed = true;
        output.steps.push(MutationStep {
            kind: if should_park {
                MutationKind::AddLabel
            } else {
                MutationKind::RemoveLabel
            },
            state: MutationStepState::Completed,
            pr: Some(caravan.id),
            summary: if should_park {
                "parked exact terminal-red caravan outside active capacity".to_owned()
            } else {
                "reactivated parked caravan after the complete current verdict turned green"
                    .to_owned()
            },
        });
        let after = receipt.after.clone();
        output.provider_receipts.push(receipt);
        if should_park && after.auto_merge.enabled {
            let disable = provider
                .disable_auto_merge(&status.repository, &PullRequestPrecondition::from(&after))
                .map_err(|error| parking_mutation_error(&error, caravan, &failures))?;
            output.steps.push(MutationStep {
                kind: MutationKind::DisableAutoMerge,
                state: MutationStepState::Completed,
                pr: Some(caravan.id),
                summary: "disabled auto-merge on newly parked caravan head".to_owned(),
            });
            output.provider_receipts.push(disable);
        }

        let fingerprint = crate::membership::fnv1a64(
            &serde_json::to_vec(&json!({
                "caravan": caravan,
                "failures": failures,
                "verdicts": verdicts,
                "required_runs": &green_required_runs,
                "head": head.head,
                "policy": "park",
            }))
            .expect("parking evidence serializes"),
        );
        output.events.push(hooks::event(
            if should_park {
                EventKind::CaravanParked
            } else {
                EventKind::CaravanUnparked
            },
            OperationId::new(),
            status.repository.clone(),
            Some(caravan.id),
            caravan.members.clone(),
            Some(status.analysis.fleet.clone()),
            Some(if should_park {
                "exact current terminal-red verdict parked the caravan".to_owned()
            } else {
                "complete current green verdict reactivated the caravan".to_owned()
            }),
            BTreeMap::from([
                ("policy".to_owned(), json!("park")),
                ("fingerprint".to_owned(), json!(fingerprint)),
                ("failures".to_owned(), json!(failures)),
                ("verdicts".to_owned(), json!(verdicts)),
                ("required_runs".to_owned(), json!(green_required_runs)),
                ("ordering".to_owned(), json!(caravan.members)),
                (
                    "provider_receipts".to_owned(),
                    json!(&output.provider_receipts[receipt_start..]),
                ),
            ]),
        ));
    }
    Ok(output)
}

/// Require the same protection-declared complete-green evidence as explicit
/// `unpark` before a normal sync can remove parking. A partial/unreadable
/// protection response or a required context that has not materialized is a
/// provider-write-free hold, never implicit recovery authority.
fn parking_required_runs_green(
    status: &StatusOutput,
    caravan: &Caravan,
    provider: &impl SyncProvider,
) -> Option<Vec<crate::required_runs::RequiredRunsAssessment>> {
    let mut contexts_by_branch: BTreeMap<String, RequiredContextsRead> = BTreeMap::new();
    let mut assessments = Vec::new();
    for number in &caravan.members {
        let pull = status.analysis.pull_requests.get(number)?;
        let contexts = if let Some(contexts) = contexts_by_branch.get(&pull.base.name) {
            contexts.clone()
        } else {
            let contexts = provider
                .branch_required_contexts(&status.repository, &pull.base.name)
                .ok()?
                .normalized();
            contexts_by_branch.insert(pull.base.name.clone(), contexts.clone());
            contexts
        };
        let assessment = required_runs::assess(&RequiredRunsInput {
            pr: pull.number,
            head: &pull.head,
            base: &pull.base,
            contexts: &contexts,
            lineage: None,
            checks: &pull.checks,
            head_published_at: pull.updated_at.as_deref(),
            clock: RequiredRunsClock {
                now_unix: now_unix(),
                grace_secs: 0,
            },
        });
        if !matches!(
            assessment.status,
            RequiredRunsStatus::Satisfied | RequiredRunsStatus::NotRequired
        ) {
            return None;
        }
        assessments.push(assessment);
    }
    Some(assessments)
}

fn parking_failure_classification(checks: &[&CheckSnapshot]) -> &'static str {
    if checks
        .iter()
        .any(|check| check.state == CheckState::Failure)
    {
        "source_or_infrastructure_failure"
    } else if checks
        .iter()
        .any(|check| check.state == CheckState::TimedOut)
    {
        "transport_or_timeout"
    } else if checks
        .iter()
        .any(|check| check.state == CheckState::Cancelled)
    {
        "cancellation"
    } else if checks
        .iter()
        .any(|check| check.state == CheckState::ActionRequired)
    {
        "action_required"
    } else {
        "unknown"
    }
}

fn parking_mutation_error(
    error: &MutationError,
    caravan: &Caravan,
    failures: &[Value],
) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "terminal_red_parking_failed",
        format!("terminal-red parking transition failed: {error}"),
        Some(
            json!({"caravan": caravan, "failures": failures, "mutated": false, "resumable": true}),
        ),
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
    writer_guard: &WriterOperationGuard,
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
                    .with_writer_fence(writer_guard.remote_fence())
                    .because(if index == 0 {
                        crate::physical_rebase::BranchRewriteReason::CurrentDefaultAdvanced
                    } else {
                        crate::physical_rebase::BranchRewriteReason::ParentAdvanced {
                            parent_pr: caravan.members[index - 1],
                        }
                    })
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
        // An unadmitted candidate is in no caravan, so it can never block a
        // member rewrite. Without this the whole tick aborts as `invalid_graph`
        // and automatic admission is never reached, which is why a fleet with
        // zero caravans and one conflicting candidate could never form its
        // first caravan (bd-550b0e).
        //
        // Gated on `blocks_fleet()` rather than `is_candidate_scoped()`: a tick
        // must stop for exactly the problems that gate the fleet, and no others.
        // Naming one non-blocking CATEGORY meant a second category, historical
        // dissolutions, fell through into a blocking decision, so `status`
        // reported healthy while every tick aborted with `invalid_graph`
        // (bd-226a07).
        if !problem.kind.blocks_fleet() || !problem_affects_selected_caravans(problem, selected) {
            continue;
        }
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
    // Every actual branch write is followed by one idempotent marked comment.
    // Reserve both before crossing the write barrier so attribution cannot be
    // silently starved by later provider work (bd-8e97bf).
    progress
        .ensure_mutation_capacity(planned_branch_writes.saturating_mul(2))
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
        for (_plan, error) in failed_plans {
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
        progress.current.insert(receipt.pr, observed.clone());
        progress.steps.push(MutationStep {
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
        if let Some(comment) =
            ensure_branch_rewrite_comment(provider, &status.repository, &observed, receipt)
                .map_err(|error| {
                    attach_physical_rebuild(
                        mutation_error(&error, &progress, Some(receipt.pr)),
                        &outcome,
                    )
                })?
        {
            record_marked_comment(
                &mut progress,
                comment,
                receipt.pr,
                "posted one-line branch rewrite reason",
            );
        }
    }
    outcome
        .provider_receipts
        .clone_from(&progress.provider_receipts);
    outcome.steps.clone_from(&progress.steps);
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
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
        tick: SyncTickReceipt {
            schema_version: 1,
            verb: "sync".to_owned(),
            caravans: status.analysis.fleet.caravans.len(),
            unqueued: status.analysis.fleet.unqueued.len(),
            synchronized: progress.synchronized_caravans.len(),
            joins: 0,
            changed: progress.operation_receipt().changed,
        },
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
        closed_lifecycle_transitions: Vec::new(),
        root_auto_merge: Vec::new(),
        root_promotion: progress.root_promotion,
        root_merge: progress.root_merge,
        native_stack_land: progress.native_stack_land,
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

#[allow(clippy::too_many_arguments)]
fn root_first_output(
    context: &AppContext,
    input: &SyncInput,
    started: Instant,
    operation_deadline: Instant,
    initial_status_elapsed: Duration,
    root_elapsed: Duration,
    progress: SyncProgress,
    status: StatusOutput,
    lock_recovery: Option<OperationLockRecovery>,
    lock: &mut WriterOperationGuard,
) -> Result<SyncOutput, AppError> {
    lock.checkpoint(
        "root_first_merge_complete",
        json!({
            "root_merge": &progress.root_merge,
            "provider_state": sync_checkpoint_evidence(&progress),
            "continuation": "ordinary fleet analysis deferred to the next bounded tick",
        }),
        false,
    )?;
    lock.checkpoint("completed", sync_checkpoint_evidence(&progress), false)?;
    let mut scheduler_status = successful_scheduler_status(
        &status,
        &progress.ci,
        &progress.paused_caravans,
        context.config.rebase_on_join,
        &progress.required_runs,
        &progress.missing_required_runs,
    );
    scheduler_status.disposition = SchedulerDisposition::RetryTick;
    scheduler_status.wake_class = SchedulerWakeClass::RetryTick;
    scheduler_status.reason = format!(
        "landed {} exact green Cara-owned root(s) before whole-fleet analysis; rerun the next bounded tick from the durable provider cursor",
        progress.root_merge.len()
    );
    let receipt = progress.operation_receipt();
    Ok(SyncOutput {
        tick: SyncTickReceipt {
            schema_version: 1,
            verb: "sync".to_owned(),
            caravans: status.analysis.fleet.caravans.len(),
            unqueued: status.analysis.fleet.unqueued.len(),
            synchronized: progress.synchronized_caravans.len(),
            joins: 0,
            changed: receipt.changed,
        },
        receipt,
        auto_admission: AutoAdmissionOutput::disabled(context, input.all),
        scheduler_status,
        timing: Some(SyncTiming {
            deadline_ms: duration_millis(operation_deadline.saturating_duration_since(started)),
            total_ms: duration_millis(started.elapsed()),
            initial_status_ms: duration_millis(initial_status_elapsed),
            provider_convergence_ms: duration_millis(root_elapsed),
            final_status_ms: 0,
        }),
        lock_recovery,
        provider_receipts: progress.provider_receipts,
        closed_lifecycle_transitions: Vec::new(),
        root_auto_merge: progress.root_auto_merge,
        root_promotion: progress.root_promotion,
        root_merge: progress.root_merge,
        native_stack_land: progress.native_stack_land,
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
        // This is intentionally the bounded pre-merge snapshot. Root receipts
        // are authoritative; whole-fleet rediscovery belongs to the next tick.
        status,
    })
}

#[allow(clippy::too_many_arguments)]
fn closed_lifecycle_output(
    context: &AppContext,
    input: &SyncInput,
    started: Instant,
    operation_deadline: Instant,
    initial_status_elapsed: Duration,
    convergence_elapsed: Duration,
    final_status_elapsed: Duration,
    reconciliation: ClosedLifecycleReconciliation,
    status: StatusOutput,
    lock_recovery: Option<OperationLockRecovery>,
    lock: &mut WriterOperationGuard,
) -> Result<SyncOutput, AppError> {
    let operation_id = OperationId::new();
    let receipt = OperationReceipt {
        operation_id,
        operation: "sync".to_owned(),
        changed: reconciliation.changed,
        completed_steps: reconciliation.steps,
    };
    lock.checkpoint(
        "closed_lifecycle_converged",
        json!({
            "operation_receipt": &receipt,
            "closed_lifecycle_transitions": &reconciliation.transitions,
            "provider_receipts": checkpoint_provider_receipts(&reconciliation.provider_receipts),
            "continuation": "ordinary active-fleet work is deferred to the next trusted sync tick",
        }),
        false,
    )?;
    lock.checkpoint("completed", json!({"operation_receipt": &receipt}), false)?;

    let mut scheduler_status =
        successful_scheduler_status(&status, &[], &[], context.config.rebase_on_join, &[], &[]);
    if scheduler_status.disposition == SchedulerDisposition::Healthy {
        scheduler_status.disposition = SchedulerDisposition::RetryTick;
        scheduler_status.wake_class = SchedulerWakeClass::RetryTick;
        scheduler_status.reason = format!(
            "converged {} closed lifecycle row(s); active repair, rebase, admission, and merge work is deferred to the next tick",
            reconciliation.transitions.len()
        );
    }
    Ok(SyncOutput {
        tick: SyncTickReceipt {
            schema_version: 1,
            verb: "sync".to_owned(),
            caravans: status.analysis.fleet.caravans.len(),
            unqueued: status.analysis.fleet.unqueued.len(),
            synchronized: 0,
            joins: 0,
            changed: receipt.changed,
        },
        receipt,
        auto_admission: AutoAdmissionOutput::disabled(context, input.all),
        scheduler_status,
        timing: Some(SyncTiming {
            deadline_ms: duration_millis(operation_deadline.saturating_duration_since(started)),
            total_ms: duration_millis(started.elapsed()),
            initial_status_ms: duration_millis(initial_status_elapsed),
            provider_convergence_ms: duration_millis(convergence_elapsed),
            final_status_ms: duration_millis(final_status_elapsed),
        }),
        lock_recovery,
        provider_receipts: reconciliation.provider_receipts,
        closed_lifecycle_transitions: reconciliation.transitions,
        root_auto_merge: Vec::new(),
        root_promotion: Vec::new(),
        root_merge: Vec::new(),
        native_stack_land: Vec::new(),
        required_runs: Vec::new(),
        rebase_plans: Vec::new(),
        rebase_receipts: Vec::new(),
        historical_predecessor: read::historical_predecessor(&status),
        synchronized_caravans: Vec::new(),
        paused_caravans: Vec::new(),
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
    writer_guard: Option<WriterOperationGuard>,
    authority: Option<&crate::sync_authority::DefaultBranchAuthority>,
) -> Result<SyncOutput, AppError> {
    let lock = match writer_guard {
        Some(lock) => lock,
        None => context.acquire_writer_operation("sync")?,
    };
    let lock_recovery = lock.recovered_dead_owner().cloned();
    sync_with_lock(
        context,
        input,
        started,
        operation_deadline,
        lock,
        lock_recovery.clone(),
        authority,
    )
    .map_err(|error| attach_lock_recovery(error, lock_recovery.as_ref()))
}

#[allow(clippy::too_many_lines)]
fn sync_with_lock(
    context: &AppContext,
    input: &SyncInput,
    started: Instant,
    operation_deadline: Instant,
    mut lock: WriterOperationGuard,
    lock_recovery: Option<OperationLockRecovery>,
    authority: Option<&crate::sync_authority::DefaultBranchAuthority>,
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
        // A sync tick operates on the whole fleet and addresses every PR
        // explicitly, so provider discovery does not depend on the temporary
        // authoritative worktree's detached HEAD. The invocation identity is
        // rebound below for targeted (non-`--all`) selection only.
        read::fleet_status(context, operation_deadline, Some(&github_budget))?;
    if let Some(authority) = authority {
        authority.bind_invocation(&mut status)?;
    }
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
    require_current_policy(context, &status)?;
    if let Some(authority) = authority {
        // Provider discovery above is read-only. Fence the first possible
        // provider mutation on the exact default-policy generation fetched for
        // this tick; a concurrent main movement gets a fresh tick, never mixed
        // policy and writes.
        authority.revalidate()?;
    }
    // Tick budgets are enforced here rather than at config load, so a bad budget
    // can never silence the read-only surfaces needed to diagnose it.
    context.config.validate_tick_bounds().map_err(|error| {
        AppError::structured(
            ErrorCategory::Validation,
            "invalid_tick_bounds",
            error.to_string(),
            Some(json!({
                "resumable": true,
                "operator_action_required": true,
                "next": "correct the named sync/loop bound in .caravan/config.yaml, then rerun the same command",
                "note": "`cara status`, `cara check`, and `cara log` stay available while this bound is invalid",
            })),
        )
    })?;
    let runner = crate::command::ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(timeout)
        .with_operation_deadline(operation_deadline)
        .with_github_request_budget(github_budget.clone());
    let runner = lock.runner(runner);
    // A decision can require an exact branch checkout. Prove checkout safety
    // before the first provider mutation so a dirty worktree can never turn a
    // partially-mutated sync into an unrepairable decision receipt.
    crate::navigation::ensure_safe_worktree(
        &context.repository_path,
        &context.config_path,
        &runner,
    )?;
    let provider = GitHubMutationAdapter::new(runner);

    // Terminal provider state is reconciled before root landing, repair,
    // physical rebase, capacity, or admission. A changed pass returns after an
    // authoritative rediscovery, so a closed generation can never trigger
    // active queue work in the same tick.
    let closed_lifecycle_started = Instant::now();
    progress::emit(
        "closed_lifecycle",
        "converging closed-unmerged, reopened, and merged lifecycle labels",
    );
    let closed_lifecycle = reconcile_closed_lifecycle(&status, &provider)?;
    if closed_lifecycle.changed {
        let final_status_started = Instant::now();
        status = read::status_with_deadline_and_budget(
            context,
            operation_deadline,
            Some(&github_budget),
        )
        .map_err(|error| {
            AppError::structured(
                error.category(),
                "closed_pr_terminalization_rediscovery_failed",
                "closed PR labels changed but the authoritative postcondition could not be rediscovered",
                Some(json!({
                    "source": error.details(),
                    "closed_lifecycle_transitions": &closed_lifecycle.transitions,
                    "provider_receipts": &closed_lifecycle.provider_receipts,
                    "resumable": true,
                    "branch_action": "preserved",
                    "safe_next_action": "rerun the same trusted sync to rediscover and converge exact provider state",
                })),
            )
        })?;
        progress::emit(
            "closed_lifecycle",
            format!(
                "converged {} closed lifecycle row(s); active work deferred",
                closed_lifecycle.transitions.len()
            ),
        );
        return closed_lifecycle_output(
            context,
            input,
            started,
            operation_deadline,
            initial_status_elapsed,
            closed_lifecycle_started.elapsed(),
            final_status_started.elapsed(),
            closed_lifecycle,
            status,
            lock_recovery,
            &mut lock,
        );
    }

    // Root landing is the first active-fleet provider convergence action for
    // the Caravan-owned backend. Native mode must first perform the complete
    // fresh Stack-intersection read in `reconcile_caravan`; otherwise this fast
    // path could bypass routing and issue synchronous `gh pr merge`.
    if context.config.stack_type != crate::config::StackType::Github {
        let root_first_started = Instant::now();
        progress::emit(
            "root_first",
            "evaluating one exact green Cara-owned root before fleet planning",
        );
        if let Some(progress) = execute_root_first(
            &status,
            &provider,
            input.all,
            context.config.sync.max_mutations_per_tick,
            RequiredRunsPolicy::from_config(&context.config.sync),
        )? {
            progress::emit(
                "root_first",
                format!(
                    "landed {} root(s); deferring unrelated analysis to the next tick",
                    progress.root_merge.len()
                ),
            );
            return root_first_output(
                context,
                input,
                started,
                operation_deadline,
                initial_status_elapsed,
                root_first_started.elapsed(),
                progress,
                status,
                lock_recovery,
                &mut lock,
            );
        }
    }

    let parking = reconcile_terminal_red_parking(context, &status, &provider)?;
    let parking_mutations = u32::try_from(
        parking
            .steps
            .iter()
            .filter(|step| step.state == MutationStepState::Completed)
            .count(),
    )
    .unwrap_or(u32::MAX);
    if parking.changed {
        status = read::status_with_deadline_and_budget(
            context,
            operation_deadline,
            Some(&github_budget),
        )?;
    }
    let convergence_started = Instant::now();
    let mut physical_rebuild = PhysicalRebuildOutcome::default();
    if context.config.physical_branch_rewrites_enabled() {
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
        let (prepared, progress_state, admission) = prepare_physical_chains(
            context,
            &status,
            input.all,
            &provider,
            operation_deadline,
            &lock,
        )?;
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
        // `caravan-force` is durable PR-scoped intent. Branch pushes retain PR
        // labels, so physical apply carries it through without provider control
        // mutations or compensation choreography (bd-91e96a).
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
    let mut progress = execute_bounded_with_native(
        &status,
        &provider,
        input.all,
        input.rerun_failed,
        context.config.force_merge,
        context
            .config
            .sync
            .max_mutations_per_tick
            .saturating_sub(physical_mutations)
            .saturating_sub(parking_mutations),
        &rewritten_heads,
        RequiredRunsPolicy::from_config(&context.config.sync),
        NativeSyncContext::from_context(context),
    )?;
    if !parking.steps.is_empty() {
        let mut steps = parking.steps;
        steps.append(&mut progress.steps);
        progress.steps = steps;
        let mut receipts = parking.provider_receipts;
        receipts.append(&mut progress.provider_receipts);
        progress.provider_receipts = receipts;
        let mut events = parking.events;
        events.append(&mut progress.events);
        progress.events = events;
    }
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
    if context.config.physical_branch_rewrites_enabled() {
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
                "max_caravans": context.config.sync.max_caravans,
                "active_caravans": final_status.auto_admission.active_caravans,
                "parked_caravans": final_status.auto_admission.parked_caravans,
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
            &lock,
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
            let post_admission = execute_bounded_with_native(
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
                NativeSyncContext::from_context(context),
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
        tick: SyncTickReceipt {
            schema_version: 1,
            verb: "sync".to_owned(),
            caravans: final_status.analysis.fleet.caravans.len(),
            unqueued: final_status.analysis.fleet.unqueued.len(),
            synchronized: progress.synchronized_caravans.len(),
            joins: auto_admission.joins.len(),
            changed: progress.operation_receipt().changed,
        },
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
        closed_lifecycle_transitions: Vec::new(),
        root_auto_merge: progress.root_auto_merge,
        root_promotion: progress.root_promotion,
        root_merge: progress.root_merge,
        native_stack_land: progress.native_stack_land,
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

/// Keep one hook notification per distinct exact-generation problem.
fn dedupe_hook_events(events: &mut Vec<CaravanEvent>) {
    let mut required_runs = BTreeSet::new();
    let mut conflicts = BTreeSet::new();
    events.retain(|event| match event.kind {
        EventKind::RequiredRunsMissing => event
            .metadata
            .get("fingerprint")
            .and_then(Value::as_str)
            .is_none_or(|fingerprint| required_runs.insert(fingerprint.to_owned())),
        EventKind::ConflictDetected => event
            .metadata
            .get("dedupe_key")
            .and_then(Value::as_str)
            .is_none_or(|fingerprint| conflicts.insert(fingerprint.to_owned())),
        _ => true,
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
    target
        .native_stack_land
        .append(&mut source.native_stack_land);
    target.rebase_plans.append(&mut source.rebase_plans);
    target.rebase_receipts.append(&mut source.rebase_receipts);
    target.paused_caravans.append(&mut source.paused_caravans);
    target
        .head_advancements
        .append(&mut source.head_advancements);
    target.events.append(&mut source.events);
    // Two convergence passes over the same member inside one tick must not
    // notify hooks twice about the same exact stall.
    dedupe_hook_events(&mut target.events);
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
    writer_guard: &WriterOperationGuard,
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
        fleet_capacity_refusal: None,
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
    // GitHub can lag regeneration of refs/pull/<n>/merge after default moves.
    // Give each immutable native candidate exactly one uncached provider
    // rediscovery before considering the exact-Git stale-base fallback.
    let mut refreshed_stale_native_candidates = BTreeSet::new();

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

        // A stale receipt authorises removal; an ABSENT receipt does not.
        //
        // `retained` is None when nobody posted a receipt, which is exactly the
        // state of a label applied BY A HUMAN to hold a pull request. Removing it
        // strips a marker Cara never wrote, and the hold evaporates on the next
        // tick rather than on any generation change — so an operator who believes
        // the PR is protected finds it admitted. That is worse than no hold at
        // all (bd-239640).
        if retained.is_none() {
            validated_skips.insert(skipped.pr);
            progress.already(
                MutationKind::RemoveLabel,
                skipped.pr,
                "left a foreign admission skip alone: no Cara receipt, so the label is not ours to remove",
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
        let needs_native_candidate_refresh = context.config.stack_type
            == crate::config::StackType::Github
            && !context.config.physical_branch_rewrites_enabled()
            && status.merge_candidates.iter().any(|identity| {
                identity.pr == next_pr
                    && identity.freshness == crate::model::MergeCandidateFreshness::StaleBase
                    && identity.stale_base
                    && !identity.stale_head
            });
        if needs_native_candidate_refresh && refreshed_stale_native_candidates.insert(next_pr) {
            read::invalidate_status_cache(context);
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
            continue;
        }
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
        let evaluation = evaluate_auto_candidate_bounded(
            &status,
            &candidate,
            &checker,
            configured_batch_bound(context),
        )?;
        if matches!(evaluation.target, AutoCandidateTarget::New)
            && let Some(refusal) =
                caravan_fleet_capacity_refusal(context, &status, candidate.number)
        {
            output.continuation = AutoAdmissionContinuation::MaxCaravansReached;
            output.fleet_capacity_refusal = Some(refusal);
            break;
        }

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
                writer_guard,
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

/// Repository-wide admission fence for forming one new caravan.
///
/// This counts only active, non-parked caravans. It deliberately preserves an
/// already-excess fleet: lowering the bound never authorizes deletion, merging,
/// eviction, or reshaping, and joins to an existing caravan do not increase the
/// count.
pub(crate) fn caravan_fleet_capacity_refusal(
    context: &AppContext,
    status: &StatusOutput,
    candidate_pr: PrNumber,
) -> Option<CaravanFleetCapacityRefusal> {
    let active_caravan_ids = status
        .analysis
        .fleet
        .caravans
        .iter()
        .filter(|caravan| !caravan.parked)
        .map(|caravan| caravan.id)
        .collect::<Vec<_>>();
    let parked_caravan_ids = status
        .analysis
        .fleet
        .caravans
        .iter()
        .filter(|caravan| caravan.parked)
        .map(|caravan| caravan.id)
        .collect::<Vec<_>>();
    let active_caravans = active_caravan_ids.len();
    let parked_caravans = parked_caravan_ids.len();
    let max = usize::try_from(context.config.sync.max_caravans).unwrap_or(usize::MAX);
    (active_caravans >= max).then(|| CaravanFleetCapacityRefusal {
        code: "max_caravans_reached".to_owned(),
        candidate_pr,
        max_caravans: context.config.sync.max_caravans,
        active_caravans,
        active_caravan_ids,
        parked_caravans,
        parked_caravan_ids,
        excess_active_caravans: active_caravans.saturating_sub(max),
        safe_next_action: format!(
            "join candidate #{candidate_pr} to an existing compatible caravan, or let an active caravan land before forming another; {parked_caravans} parked caravan(s) do not consume the configured capacity and existing excess caravans remain untouched",
        ),
    })
}

/// Typed zero-write error for an explicit request to form a new caravan at
/// repository capacity.
pub(crate) fn caravan_fleet_capacity_error(refusal: &CaravanFleetCapacityRefusal) -> AppError {
    AppError::structured(
        ErrorCategory::Validation,
        refusal.code.clone(),
        "forming another caravan would exceed sync.max_caravans",
        Some(json!({
            "mutated": false,
            "candidate_pr": refusal.candidate_pr,
            "max_caravans": refusal.max_caravans,
            "active_caravans": refusal.active_caravans,
            "active_caravan_ids": refusal.active_caravan_ids,
            "parked_caravans": refusal.parked_caravans,
            "parked_caravan_ids": refusal.parked_caravan_ids,
            "excess_active_caravans": refusal.excess_active_caravans,
            "preserves_existing_caravans": true,
            "retryable": false,
            "safe_next_action": refusal.safe_next_action,
            "suggested_actions": [
                "join an existing compatible caravan instead of creating another",
                "let an active caravan land, then retry new",
                "raise sync.max_caravans through reviewed repository policy when parallel caravans are intentional"
            ],
        })),
    )
}

/// Deterministic pre-admission capacity gate for one candidate join.
///
/// Returns typed refusal evidence when accepting the candidate would push the
/// exact target chain past the largest size the configured deadline can still
/// guarantee to drain, or when the configured arithmetic yields no bound
/// admission could honestly enforce. This chain-local gate does not evaluate a
/// new caravan; [`caravan_fleet_capacity_refusal`] owns that repository-wide
/// decision.
pub(crate) fn caravan_capacity_refusal(
    context: &AppContext,
    status: &StatusOutput,
    candidate_pr: PrNumber,
    target_tail: Option<PrNumber>,
) -> Option<CaravanCapacityRefusal> {
    let deadline = sync_operation_budget(context);
    // A configured batch bound applies even without physical chain rebuilding:
    // it bounds one atomic native merge batch, not an apply reserve.
    if let Some(batch) = configured_batch_bound(context) {
        let caravan = status.analysis.fleet.containing(target_tail?)?;
        let members = u64::try_from(caravan.members.len()).unwrap_or(u64::MAX);
        if members >= batch {
            return Some(CaravanCapacityRefusal {
                code: "caravan_batch_capacity_exhausted".to_owned(),
                candidate_pr,
                caravan_id: caravan.id,
                caravan_members: members,
                max_admissible_members: Some(batch),
                capacity_defect: None,
                configured_deadline_ms: duration_millis(deadline),
                command_timeout_ms: context.config.command_timeout_secs.saturating_mul(1_000),
                safe_next_action: format!(
                    "caravan #{} already holds the configured {batch}-member batch bound (max_caravan_length); admit #{candidate_pr} into another compatible caravan or start a new one instead of extending a full batch",
                    caravan.id,
                ),
            });
        }
    }
    if !context.config.physical_branch_rewrites_enabled() {
        return None;
    }
    let caravan = status.analysis.fleet.containing(target_tail?)?;
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
    let batch = refusal.code == "caravan_batch_capacity_exhausted";
    let message = if defect {
        "the configured sync deadline yields no admissible chain size, so admission cannot be gated honestly and the join fails as a defect"
    } else if batch {
        "the target caravan already holds the configured max_caravan_length batch bound"
    } else {
        "the target caravan already holds every member the configured sync deadline can guarantee to drain"
    };
    let suggested_actions = if defect {
        json!([
            "raise sync.max_duration_secs until the reported minimum_deadline_ms fits, so a chain of at least two members is admissible again",
            "lower sync.reserve_secs_per_command to a proven-safe per-command reserve",
            "start an independent caravan; waiting for an existing caravan to drain cannot repair an unsound bound"
        ])
    } else if batch {
        json!([
            "admit this candidate into another compatible caravan below the configured batch bound",
            "start an independent caravan; a full batch is never extended",
            "run `cara sync --all` and let the full batch land before reusing it"
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

#[cfg(test)]
fn evaluate_auto_candidate(
    status: &StatusOutput,
    candidate: &PullRequestSnapshot,
    checker: &impl crate::graph::CompatibilityChecker,
) -> Result<AutoCandidateEvaluation, AppError> {
    evaluate_auto_candidate_bounded(status, candidate, checker, None)
}

/// Deterministic target selection under an optional configured batch bound.
///
/// A full batch is never extended. When every visible tail is full, admission
/// opens another caravan rather than waiting for one to drain, which is what
/// keeps a bounded native Stack from becoming an unbounded queue.
fn evaluate_auto_candidate_bounded(
    status: &StatusOutput,
    candidate: &PullRequestSnapshot,
    checker: &impl crate::graph::CompatibilityChecker,
    batch_bound: Option<u64>,
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
    let tails = current_tail_generations_bounded(status, batch_bound);
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
    let mut open_tails = 0_usize;
    for tail in &tails {
        if tail.batch_full {
            reasons.push(format!(
                "tail #{}: caravan #{} already holds the configured max_caravan_length batch bound",
                tail.tail_pr, tail.caravan_id
            ));
            continue;
        }
        open_tails += 1;
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
    // Every visible caravan is a full batch: start another one instead of
    // deferring the candidate behind a batch that must land as a unit.
    if open_tails == 0 {
        let output = check_auto_target(&virtual_status, &CheckInput::default(), checker)?;
        if output.eligible {
            return Ok(AutoCandidateEvaluation {
                target: AutoCandidateTarget::New,
                tested_tails: tails,
                reasons,
            });
        }
        reasons.extend(check_reasons(&output));
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
    match crate::read::check_requested_action_analysis(status, input, checker) {
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
    current_tail_generations_bounded(status, None)
}

fn current_tail_generations_bounded(
    status: &StatusOutput,
    batch_bound: Option<u64>,
) -> Vec<AutoAdmissionTailGeneration> {
    status
        .analysis
        .fleet
        .caravans
        .iter()
        .filter(|caravan| !caravan.parked)
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
                batch_full: batch_bound.is_some_and(|bound| {
                    u64::try_from(caravan.members.len()).unwrap_or(u64::MAX) >= bound
                }),
            })
        })
        .collect()
}

fn auto_admission_config_fingerprint(context: &AppContext) -> String {
    let material = serde_json::to_vec(&json!({
        "version": context.config.version,
        "rebase_on_join": context.config.rebase_on_join,
        "max_caravan_length": context.config.effective_max_caravan_length(),
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
    // Prove the candidate is still tracked before writing anything.
    if !progress.current.contains_key(&receipt.candidate_pr) {
        return Err(AppError::validation(
            "auto_admission_candidate_missing",
            format!(
                "candidate #{} disappeared before skip",
                receipt.candidate_pr
            ),
        ));
    }
    // Receipt BEFORE label. Both writes can fail independently, so the order
    // decides which partial state is survivable.
    //
    // Label-first leaves a skip label with no receipt, and a later tick cannot
    // distinguish that from a deliberate operator hold, so bd-239640's
    // fail-closed rule correctly refuses to remove it and the candidate is
    // excluded permanently — observed on #2245, #2259 and #2314. Receipt-first
    // leaves at most a receipt with no label: the candidate stays admissible,
    // the next tick rewrites the same marked comment idempotently, and nothing
    // needs operator recovery (bd-8b1160).
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
    // Re-read the candidate: the receipt write advanced the tracked generation.
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
    Ok(())
}

/// Post one exact-generation rewrite reason, or prove no comment is required.
///
/// Shared by sync, atomic membership, and reshape so every physical write uses
/// the same one-line/idempotency contract (bd-8e97bf).
pub(crate) fn ensure_branch_rewrite_comment(
    provider: &impl SyncProvider,
    repository: &RepositoryId,
    observed: &PullRequestSnapshot,
    receipt: &crate::physical_rebase::RebaseReceipt,
) -> Result<Option<GitHubMutationReceipt>, MutationError> {
    let Some((marker, body)) = receipt.rewrite_reason.comment(receipt) else {
        return Ok(None);
    };
    let expected = PullRequestPrecondition::from(observed);
    let mut last_provider_error = None;
    for attempt in 0..3 {
        match provider.ensure_marked_comment(repository, &expected, &marker, &body) {
            Ok(receipt) => return Ok(Some(receipt)),
            Err(error @ MutationError::Provider(_)) if attempt < 2 => {
                // The comment write may have succeeded before a read/refetch
                // failed. The marker makes the retry safe and turns that case
                // into AlreadySatisfied instead of a duplicate line.
                last_provider_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_provider_error.expect("a bounded provider retry retains its error"))
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
    execute_bounded_with_native(
        status,
        provider,
        all,
        rerun_failed,
        force_merge,
        u32::MAX,
        &BTreeMap::new(),
        RequiredRunsPolicy::default(),
        None,
    )
}

#[cfg(test)]
fn execute_with_required_runs(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    required_runs: RequiredRunsPolicy,
) -> Result<SyncProgress, AppError> {
    execute_bounded_with_native(
        status,
        provider,
        false,
        false,
        false,
        u32::MAX,
        &BTreeMap::new(),
        required_runs,
        None,
    )
}

#[cfg(test)]
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
    execute_bounded_with_native(
        status,
        provider,
        all,
        rerun_failed,
        force_merge,
        mutation_limit,
        rewritten_heads,
        required_runs,
        None,
    )
}

fn root_first_eligible(status: &StatusOutput, caravan: &Caravan) -> bool {
    let Some(root) = caravan.head() else {
        return false;
    };
    let Some(observed) = status.analysis.pull_requests.get(&root) else {
        return false;
    };
    observed.state == PullRequestState::Open
        && !observed.draft
        && observed.base.name == status.default_branch
        && !observed.auto_merge.enabled
        && classify_checks(&observed.checks, observed.has_label("caravan-force"))
            == CiDisposition::Passing
        && status.analysis.cumulative_trees.iter().any(|proof| {
            proof.candidate == observed.head
                && proof.target == status.analysis.fleet.default_branch
                && proof.identical
        })
}

/// Land at most one already-admitted exact-green Cara-owned root before any
/// physical planning or whole-member CI analysis. A successful landing returns
/// immediately; the next tick resumes from GitHub's durable cursor.
fn execute_root_first(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    all: bool,
    mutation_limit: u32,
    required_runs: RequiredRunsPolicy,
) -> Result<Option<SyncProgress>, AppError> {
    if !status.head_merge.actor.caravan() {
        return Ok(None);
    }
    let mut caravans = select_caravans(status, all)?;
    caravans.retain(|caravan| {
        !caravan.parked
            && !status
                .pauses
                .iter()
                .any(|pause| pause.state.is_effective() && pause.record.caravan_head == caravan.id)
            && root_first_eligible(status, caravan)
    });
    if caravans.is_empty() {
        return Ok(None);
    }

    let mut progress = SyncProgress::new(
        status,
        caravans.iter().map(|caravan| caravan.id).collect(),
        mutation_limit,
    );
    progress.required_runs_grace_secs = required_runs.grace_secs;
    // Root-first is a merge-priority pass, never a recovery/mutation pass for
    // absent CI. Missing lineage remains visible on the ordinary continuation.
    progress.required_runs_retrigger_enabled = false;
    preflight_repository(provider, status, &progress)?;

    for caravan in &caravans {
        let root = caravan.head().expect("eligible caravan has a root");
        progress.promote_root(provider, status, caravan.id, root, None, false)?;
        progress.ensure_no_foreign_auto_merge(provider, &status.repository, caravan.id, root)?;
        let observation = progress.observe_ci(provider, &status.repository, root)?;
        progress.ci.push(observation);
        let assessment = progress.observe_required_runs(provider, &status.repository, root)?;
        progress.push_required_runs(caravan.id, assessment, None);
        if progress.merge_root(provider, status, caravan.id, root, &caravan.members, None)? {
            return Ok(Some(progress));
        }
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn execute_bounded_with_native(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    all: bool,
    rerun_failed: bool,
    force_merge: bool,
    mutation_limit: u32,
    rewritten_heads: &BTreeMap<PrNumber, crate::model::CommitOid>,
    required_runs: RequiredRunsPolicy,
    native_stack: Option<NativeSyncContext>,
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
    progress.native_stack = native_stack;
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

fn native_route_context(error: &AppError, status: &StatusOutput, caravan: &Caravan) -> AppError {
    let mut details = error.details().unwrap_or_else(|| json!({}));
    if let Some(object) = details.as_object_mut() {
        object.insert("repository".to_owned(), json!(status.repository));
        object.insert("caravan_id".to_owned(), json!(caravan.id));
        object.insert("affected_prs".to_owned(), json!(caravan.members));
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

#[allow(clippy::too_many_arguments)]
fn native_stack_number_for_caravan(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    caravan: &Caravan,
    native: Option<&NativeSyncContext>,
) -> Result<Option<u64>, AppError> {
    let Some(native) = native else {
        return Ok(None);
    };
    let intersections = provider
        .native_stack_intersections_for_sync(&status.repository, &caravan.members)
        .map_err(|error| native_route_context(&error, status, caravan))?;
    let route = crate::stack_policy::route_landing(
        &native.config,
        &status.stack_backend,
        caravan,
        &intersections,
        &status.analysis.pull_requests,
    )
    .map_err(|error| native_route_context(&error, status, caravan))?;
    Ok(match route {
        crate::stack_policy::StackLandingRoute::NativeStack { stack_number, .. } => {
            Some(stack_number)
        }
        crate::stack_policy::StackLandingRoute::CaravanOwned
        | crate::stack_policy::StackLandingRoute::SingletonCaravanOwned => None,
    })
}

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

    // Native routing is a fresh provider-intersection precondition, not a
    // discovery-time projection. It runs before promotion, auto-merge disarm,
    // force handling, or any landing write, so an intersecting provider Stack
    // can never fall through to synchronous `gh pr merge`.
    let native_stack_number =
        native_stack_number_for_caravan(status, provider, caravan, progress.native_stack.as_ref())?;

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

    // Durable force intent is a PR-level instruction, not a classification of
    // one check generation. Once that PR is the root, skip every CI and
    // required-run read for the caravan and attempt the fresh mechanical/admin
    // force transaction immediately (bd-91e96a).
    if native_stack_number.is_none()
        && progress
            .current
            .get(&head)
            .is_some_and(|current| current.has_label("caravan-force"))
    {
        return force_merge_head(
            status,
            provider,
            caravan,
            force_merge,
            rewritten_heads,
            progress,
        );
    }

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
    }

    if let Some(stack_number) = native_stack_number {
        let native = progress
            .native_stack
            .clone()
            .expect("native route requires native sync context");
        return progress.drain_native_stack(provider, status, caravan, stack_number, &native);
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
    let mut current = progress
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
        .verify_pull_request(&status.repository, &PullRequestPrecondition::from(&current))
        .map_err(|error| mutation_error(&error, progress, Some(head)))?;
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
        Some("durable caravan-force intent bypassed CI and authorized immediate mechanical/admin merge".to_owned()),
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

    // Durable force has one merge actor: Cara's administrator squash. Remove a
    // historical native auto-merge request only after all zero-write policy and
    // permission checks pass, then rebind the final write to that exact row.
    progress.ensure_auto_merge_disabled(provider, &status.repository, head)?;
    current = progress
        .current
        .get(&head)
        .expect("forced head remains current after auto-merge disarm")
        .clone();
    provider
        .verify_pull_request(&status.repository, &PullRequestPrecondition::from(&current))
        .map_err(|error| mutation_error(&error, progress, Some(head)))?;

    let mut before_labels = current.labels.clone();
    before_labels.remove("caravan-force");
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
            "observed durable `caravan-force`; force_merge=true; ADMIN permission confirmed; CI bypassed without waiting; current visible checks: {}",
            serde_json::to_string(&current.checks).expect("checks serialize")
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
    // Reserve both irreversible merge and one-shot intent consumption before
    // entering the merge. The label stays present until the merge succeeds, so
    // a failed merge retains durable operator intent (bd-694436).
    progress.ensure_mutation_capacity(2)?;
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
    let consumed = provider
        .remove_label(
            &status.repository,
            &progress.precondition(head),
            "caravan-force",
        )
        .map_err(|error| {
            let source = mutation_error(&error, progress, Some(head));
            AppError::structured(
                source.category(),
                "force_intent_consume_failed",
                "forced root merged, but its one-shot caravan-force label could not be consumed",
                Some(json!({
                    "pr": head,
                    "merged": true,
                    "source": source.details(),
                    "operation_receipt": progress.operation_receipt(),
                    "provider_receipts": progress.provider_receipts,
                    "mutated": true,
                    "resumable": true,
                    "safe_next_action": "remove only the residual caravan-force label from the already merged PR; do not retry or duplicate the merge",
                })),
            )
        })?;
    progress.record(
        consumed,
        "consumed one-shot force intent after successful merge",
    );
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
        let successor_forced = progress
            .current
            .get(&new_head)
            .is_some_and(|pull| pull.has_label("caravan-force"));
        if progress.head_merge_actor.github() && !successor_forced {
            progress.ensure_root_squash_auto_merge(
                provider,
                &status.repository,
                new_head,
                new_head,
                status.analysis.pull_requests.get(&new_head),
                rewritten_heads.get(&new_head),
            )?;
        } else {
            // A durable forced successor must never be handed to native
            // auto-merge. The next fresh tick re-proves it against the advanced
            // default and performs the immediate administrator squash.
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
        "merged_at": precondition.merged_at,
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
        "capacity_refusal": output.capacity_refusal,
        "fleet_capacity_refusal": output.fleet_capacity_refusal,
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
        "native_stack_land": bounded_checkpoint_sequence(
            progress.native_stack_land.iter().map(|checkpoint| json!({
                "repository": checkpoint.repository,
                "stack_number": checkpoint.plan.before.number,
                "phase": checkpoint.phase,
                "terminal_status": checkpoint.terminal_status,
                "evidence_hash": checkpoint.evidence_hash,
            })).collect()
        ),
        "events": checkpoint_events(&progress.events),
        "recovery": "rediscover provider state and replay the same idempotent sync; hashes bind complete omitted evidence",
    })
}

fn force_event_metadata(progress: &SyncProgress, head: PrNumber) -> BTreeMap<String, Value> {
    let mut metadata = BTreeMap::new();
    metadata.insert("head".to_owned(), json!(progress.current.get(&head)));
    metadata.insert("ci_bypassed".to_owned(), json!(true));
    metadata.insert(
        "visible_checks".to_owned(),
        json!(progress.current.get(&head).map(|pull| &pull.checks)),
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
            ("cancellation".to_owned(), json!(observation.cancellation)),
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
    // Cancellation is not a verdict, so eviction must never be the leading
    // advice for it: two of three live cancellations were spurious, and one was
    // the only member of the first caravan the repository ever formed
    // (bd-1ac172). Cara never evicts automatically; this keeps the advice it
    // hands an agent honest too.
    if observation.cancellation.cancellation_only {
        suggested_actions.insert(
            0,
            "re-run the cancelled checks: nothing was judged to fail, so this is not evidence to evict on"
                .to_owned(),
        );
        suggested_actions.push(format!(
            "cancellation evidence: cancelled={:?}, stepless aggregate failures={:?}",
            observation.cancellation.cancelled_checks,
            observation.cancellation.failures_without_failing_step,
        ));
    }
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
    const INFRA_CONCLUSIONS: [&str; 6] = [
        "timed_out",
        "startup_failure",
        "stale",
        "action_required",
        "cancelled",
        "canceled",
    ];
    // An aggregate that concluded `failure` while NO job failed did not run the
    // work it reports on. That is the shape of a gate short-circuiting because a
    // prerequisite never produced a result: `Check & Lint` going red in seconds
    // with zero failing steps, downstream of a cancelled preparation job.
    //
    // Guarded on `jobs_total > 0` and `!jobs_truncated` so "no failing job" means
    // exactly that, and never "we did not look" or "the list was cut short".
    let failed_without_a_failing_job = !diagnostic.failed_jobs.is_empty()
        || diagnostic.jobs_total == 0
        || diagnostic.jobs_truncated;
    if !failed_without_a_failing_job {
        return true;
    }
    // A cancellation is a capacity or supersession event, not a verdict on the
    // code: no test or lint step failed, the producer simply never produced a
    // result. Excluding it from this set meant an operator freeing a busy runner
    // turned a whole caravan red and required a human to re-trigger, because
    // recovery only reruns what it classifies as infrastructure (bd-c04d9b).
    //
    // This does NOT make a cancelled run count as success anywhere. Absent
    // validation still fails closed for admission and for merge; it only becomes
    // eligible for the bounded rerun path, which is already opt-in via
    // `--rerun-failed` and already bound to the exact current head, so a stale
    // superseded generation is never resurrected.
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
    if forced {
        return CiDisposition::Forced;
    }

    // bd-eff1dc: a rollup is a lineage, not a set of simultaneous verdicts.
    // Retried and cancelled runs of the same required check stay attached to the
    // same head forever, so only the latest observation per identity may drive
    // disposition. Superseded rows remain available for diagnostics.
    let (current, _superseded) = crate::model::latest_checks_per_identity(checks);

    let failed = current.iter().any(|check| {
        matches!(
            check.state,
            CheckState::Failure
                | CheckState::Cancelled
                | CheckState::TimedOut
                | CheckState::ActionRequired
                | CheckState::Unknown
        )
    });
    // Failure is decisive, and is deliberately evaluated BEFORE pending. A
    // failing required check does not become successful by waiting, so treating
    // the row as merely waiting because a sibling job is still running hides a
    // hard failure for as long as anything else is in flight — long enough to
    // admit known-red work and stack every following PR on top of it
    // (operator report, cacophony PR 2276). This still holds under bd-eff1dc:
    // the precedence is unchanged, it simply applies to CURRENT rows, so a
    // current failure in one context still beats a current pending sibling.
    if failed {
        return CiDisposition::Failed;
    }
    let pending = current.is_empty()
        || current.iter().any(|check| {
            matches!(
                check.state,
                CheckState::Expected | CheckState::Queued | CheckState::InProgress
            )
        });
    if pending {
        CiDisposition::Waiting
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
        status
            .analysis
            .fleet
            .caravans
            .iter()
            .filter(|caravan| !caravan.parked)
            .cloned()
            .collect()
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
        let caravan = status
            .analysis
            .fleet
            .containing(current)
            .cloned()
            .ok_or_else(|| {
                AppError::validation(
                    "current_pr_not_in_caravan",
                    format!("PR #{current} is not an active caravan member"),
                )
            })?;
        if caravan.parked {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "caravan_parked_red",
                "the selected caravan is parked outside active convergence until current CI recovers",
                Some(
                    json!({"caravan": caravan, "resumable": true, "safe_next_action": "rerun after a new head or latest CI verdict becomes nonterminal/green"}),
                ),
            ));
        }
        vec![caravan]
    };
    // Reactivation keeps the root's immutable FIFO age; it never jumps ahead
    // because it was unparked recently, and newer green work cannot starve it.
    caravans.sort_by_key(|caravan| {
        let created_at = status
            .analysis
            .pull_requests
            .get(&caravan.id)
            .and_then(|head| head.created_at.clone());
        (
            created_at.is_none(),
            created_at.unwrap_or_default(),
            caravan.id,
        )
    });
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

fn problem_affects_selected_caravans(problem: &GraphProblem, selected: &[Caravan]) -> bool {
    problem.prs.is_empty()
        || problem.prs.iter().any(|number| {
            selected
                .iter()
                .any(|caravan| caravan.members.contains(number))
        })
}

fn validate_graph(
    status: &StatusOutput,
    selected: &[Caravan],
    progress: &SyncProgress,
    force_merge: bool,
) -> Result<(), AppError> {
    for problem in &status.analysis.fleet.problems {
        // A targeted tick owns only the selected caravans. Problems scoped to
        // unrelated admission rows or caravans cannot block this transaction;
        // empty-PR problems remain repository-global and fail closed.
        if !problem.kind.blocks_fleet() || !problem_affects_selected_caravans(problem, selected) {
            continue;
        }
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
        // Completion is judged against problems that GATE THE FLEET. A
        // candidate-scoped conflict belongs to a pull request in no caravan, so
        // it cannot describe a failure to converge and must not abort a tick
        // that has already rewritten branches. The two admission gates learned
        // this in bd-226a07; this site and its sibling below did not, so a tick
        // rebased two generations, reached final rediscovery, found a candidate
        // that does not merge cleanly into main, and aborted with invalid_graph
        // (bd-c39de4).
        if !problem.kind.blocks_fleet() {
            return false;
        }
        if !problem.prs.is_empty()
            && !problem.prs.iter().any(|number| {
                status
                    .analysis
                    .fleet
                    .containing(*number)
                    .is_some_and(|caravan| progress.synchronized_caravans.contains(&caravan.id))
            })
        {
            return false;
        }
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

// `match_same_arms` is allowed deliberately: the candidate-scoped arm shares a
// body with the fleet-fatal ones, but collapsing it would hide the one
// classification that has already caused an outage by defaulting silently. The
// arm exists to be visible, not to compute a different value.
#[allow(clippy::match_same_arms)]
fn decision_for_problem(
    problem: &GraphProblem,
    status: &StatusOutput,
    progress: &SyncProgress,
) -> DecisionPoint {
    // Deliberately exhaustive, with no `_` arm. A wildcard here is what let
    // `CandidateIncompatible` inherit a fleet-fatal `InvalidGraph` decision by
    // default when it was introduced: the compiler had been told not to help,
    // so a single unqueued conflicting candidate aborted every tick and no test
    // failed (bd-550b0e). `blocks_fleet` being exhaustive only protects call
    // sites that ask the classifier, and this one never did. Adding a variant
    // must now fail to compile until somebody chooses its decision.
    let kind = match problem.kind {
        GraphProblemKind::Incompatible if problem.prs.len() == 1 => DecisionKind::HeadConflict,
        GraphProblemKind::Incompatible if is_adjacent_pair(status, &problem.prs) => {
            DecisionKind::LinkConflict
        }
        GraphProblemKind::Incompatible => DecisionKind::CrossCaravanConflict,
        // A candidate-scoped problem never reaches a decision on the healthy
        // path: every fleet gate skips it before deciding. It is mapped
        // explicitly rather than by wildcard so that the classification stays
        // visible if a caller ever does reach here.
        GraphProblemKind::CandidateIncompatible => DecisionKind::InvalidGraph,
        GraphProblemKind::MissingHead
        | GraphProblemKind::MultipleHeads
        | GraphProblemKind::Branching
        | GraphProblemKind::Cycle
        | GraphProblemKind::DanglingBase
        | GraphProblemKind::ActiveAndEvicted
        | GraphProblemKind::DuplicateMember
        | GraphProblemKind::ForkOnlyPredecessor
        | GraphProblemKind::AutoMergeInvariant
        | GraphProblemKind::ReusedBranchProvenance
        | GraphProblemKind::SupersededGeneration
        // A dissolved member needs a human: the caravan is gone, and whether its
        // work should be requeued depends on whether a successor exists and is
        // chain-compatible, which only its owner knows.
        | GraphProblemKind::DissolvedMember
        | GraphProblemKind::AmbiguousGeneration
        | GraphProblemKind::InvalidGenerationMetadata
        | GraphProblemKind::Unknown => DecisionKind::InvalidGraph,
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

#[derive(Debug, Clone)]
struct NativeSyncContext {
    config: crate::config::CaravanConfig,
    repository_path: std::path::PathBuf,
}

impl NativeSyncContext {
    fn from_context(context: &AppContext) -> Option<Self> {
        (context.config.stack_type == crate::config::StackType::Github).then(|| Self {
            config: context.config.clone(),
            repository_path: context.repository_path.clone(),
        })
    }
}

const CONFLICT_EVENT_MAX_PATHS: usize = 64;

fn conflict_class(status: &StatusOutput, problem: &GraphProblem) -> &'static str {
    match problem.kind {
        GraphProblemKind::CandidateIncompatible => "candidate",
        GraphProblemKind::Incompatible if problem.prs.len() == 1 => "head",
        GraphProblemKind::Incompatible if is_adjacent_pair(status, &problem.prs) => "link",
        GraphProblemKind::Incompatible => "cross_caravan",
        _ => "unknown",
    }
}

/// Project current exact graph evidence into repair notifications. This performs
/// no provider access and grants no mutation authority: hooks receive only the
/// facts already proven by discovery and mechanical compatibility analysis.
// Keep the complete exact-generation filter, classification, bounded evidence,
// and no-authority payload construction together: splitting these gates across
// helpers makes it easier for one event path to omit a safety condition.
#[allow(clippy::too_many_lines)]
fn conflict_detected_events(
    status: &StatusOutput,
    operation_id: &OperationId,
) -> Vec<CaravanEvent> {
    let mut events = Vec::new();
    let mut seen = BTreeSet::new();
    for problem in &status.analysis.fleet.problems {
        if !matches!(
            problem.kind,
            GraphProblemKind::Incompatible | GraphProblemKind::CandidateIncompatible
        ) {
            continue;
        }
        let class = conflict_class(status, problem);
        for report in status
            .analysis
            .compatibility
            .iter()
            .filter(|report| report.outcome == CompatibilityOutcome::Conflict)
        {
            let Some((number, pull_request)) = problem.prs.iter().find_map(|number| {
                status
                    .analysis
                    .pull_requests
                    .get(number)
                    .filter(|pull_request| pull_request.head == report.candidate)
                    .map(|pull_request| (*number, pull_request))
            }) else {
                continue;
            };
            if pull_request.state != PullRequestState::Open {
                continue;
            }
            let target_matches = if problem.prs.len() == 1 {
                report.target == status.analysis.fleet.default_branch
            } else {
                problem
                    .prs
                    .iter()
                    .filter(|target| **target != number)
                    .any(|target| {
                        status
                            .analysis
                            .pull_requests
                            .get(target)
                            .is_some_and(|target| target.head == report.target)
                    })
            };
            if !target_matches {
                continue;
            }
            let dedupe_key = format!(
                "{}/{}#{}@{}",
                status.repository.owner, status.repository.name, number, pull_request.head.oid
            );
            let identity = (dedupe_key.clone(), class, report.target.oid.clone());
            if !seen.insert(identity) {
                continue;
            }
            let paths = report
                .conflicting_paths
                .iter()
                .take(CONFLICT_EVENT_MAX_PATHS)
                .cloned()
                .collect::<Vec<_>>();
            let prs = std::iter::once(number)
                .chain(problem.prs.iter().copied().filter(|pr| *pr != number))
                .collect();
            events.push(hooks::event(
                EventKind::ConflictDetected,
                operation_id.clone(),
                status.repository.clone(),
                status
                    .analysis
                    .fleet
                    .containing(number)
                    .map(|caravan| caravan.id),
                prs,
                None,
                Some(problem.message.clone()),
                BTreeMap::from([
                    ("dedupe_key".to_owned(), json!(dedupe_key)),
                    ("pr".to_owned(), json!(number)),
                    ("head".to_owned(), json!(pull_request.head)),
                    ("observed_base".to_owned(), json!(pull_request.base)),
                    (
                        "default_branch".to_owned(),
                        json!(status.analysis.fleet.default_branch),
                    ),
                    ("target".to_owned(), json!(report.target)),
                    ("conflict_class".to_owned(), json!(class)),
                    ("conflicting_paths".to_owned(), json!(paths)),
                    (
                        "conflicting_paths_truncated".to_owned(),
                        json!(
                            report
                                .conflicting_paths
                                .len()
                                .saturating_sub(CONFLICT_EVENT_MAX_PATHS)
                        ),
                    ),
                    ("provider_state".to_owned(), json!("exact_current_open")),
                    ("mutation_authority".to_owned(), json!("none")),
                ]),
            ));
        }
    }
    events
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
    native_stack_land: Vec<crate::github::GitHubStackLandCheckpoint>,
    /// Configured merge actor for this tick.
    head_merge_actor: HeadMergeActor,
    /// Reviewed policy for a foreign provider auto-merge request.
    external_auto_merge_policy: ExternalAutoMergePolicy,
    /// Bounded caravan-owned merges allowed in this tick.
    max_root_merges: u32,
    /// Present only for explicit native Stack mode. The stable path never
    /// evaluates native routing or checkpoint storage.
    native_stack: Option<NativeSyncContext>,
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
        let operation_id = OperationId::new();
        let events = conflict_detected_events(status, &operation_id);
        Self {
            operation_id,
            repository: status.repository.clone(),
            default_branch: status.default_branch.clone(),
            steps: Vec::new(),
            provider_receipts: Vec::new(),
            root_auto_merge: Vec::new(),
            root_promotion: Vec::new(),
            root_merge: Vec::new(),
            native_stack_land: Vec::new(),
            // Exactly one fact decides who merges: the configured policy
            // projected onto status. Every surface reads the same value.
            head_merge_actor: status.head_merge.actor,
            external_auto_merge_policy: status.head_merge.external_auto_merge_policy,
            max_root_merges: status.head_merge.max_root_merges_per_tick.max(1),
            native_stack: None,
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
            events,
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
        let cancellation = classify_cancellation(&current.checks, &failure_diagnostics);
        Ok(CiObservation {
            pr: number,
            disposition,
            checks: current.checks.clone(),
            failed_runs,
            failure_diagnostics,
            rerunnable_run_ids,
            cancellation,
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
        // Decide whether lineage is needed from the same canonical current
        // projection used by CI and required-run assessment. Historical rows
        // must not make a context appear covered after a newer workflow
        // generation supersedes them.
        let (current_checks, _superseded_checks) =
            crate::model::latest_checks_per_identity(&current.checks);
        let reporting = current_checks
            .into_iter()
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

    /// Land one fully ready native Stack under a complete-generation source-ref
    /// lock. The readiness planner still identifies a maximal prefix, but a
    /// blocked suffix requires typed reshape before submission because GitHub
    /// otherwise rewrites that suffix's source branch. Every phase is persisted
    /// before the next provider write, so another tick resumes rather than
    /// resubmits.
    #[allow(clippy::too_many_lines)]
    fn drain_native_stack(
        &mut self,
        provider: &impl SyncProvider,
        status: &StatusOutput,
        caravan: &Caravan,
        stack_number: u64,
        native: &NativeSyncContext,
    ) -> Result<(), AppError> {
        let repository = &status.repository;
        let key = format!("land-{stack_number}");
        let mut checkpoint = if let Some(checkpoint) = crate::stack_checkpoint::load::<
            crate::github::GitHubStackLandCheckpoint,
        >(&native.repository_path, &key)?
        {
            if !checkpoint.verify()
                || checkpoint.repository != *repository
                || checkpoint.plan.before.number != stack_number
            {
                return Err(AppError::validation(
                    "github_stack_land_checkpoint_stale",
                    "persisted Stack landing checkpoint does not match this repository/Stack",
                ));
            }
            checkpoint
        } else {
            let stack = provider
                .native_stack_generation_for_sync(repository, stack_number)?
                .ok_or_else(|| {
                    AppError::validation(
                        "github_stack_sync_stack_missing",
                        "mapped native Stack disappeared before landing",
                    )
                })?;
            let held = crate::stack_policy::held_caravan_members(status);
            let facts = crate::stack_policy::StackPolicyFacts {
                pull_requests: &self.current,
                merge_candidates: &self.merge_candidates,
                compatibility: &status.analysis.compatibility,
                held_members: &held,
            };
            let evidence = crate::stack_policy::stack_merge_evidence(facts, &stack, &|pr| {
                let checks = self
                    .ci
                    .iter()
                    .rev()
                    .find(|observation| observation.pr == pr)
                    .is_some_and(|observation| observation.disposition == CiDisposition::Passing);
                let required = self
                    .required_runs
                    .iter()
                    .rev()
                    .find(|receipt| receipt.pr == pr)
                    .is_none_or(|receipt| {
                        matches!(
                            receipt.assessment.status,
                            crate::required_runs::RequiredRunsStatus::Satisfied
                                | crate::required_runs::RequiredRunsStatus::NotRequired
                        )
                    });
                if checks && required {
                    crate::stack_policy::StackEntryCi::Ready
                } else {
                    crate::stack_policy::StackEntryCi::NotReady
                }
            });
            let prefix = crate::github::plan_github_stack_ready_prefix(&stack, &evidence)
                .map_err(native_sync_error)?;
            if prefix.selected.is_empty() {
                let detail = prefix.first_blocked.map(|blocked| {
                    format!(
                        "native Stack prefix waits at PR #{}: {:?}",
                        blocked.pr, blocked.blockers
                    )
                });
                self.record_merge_wait(
                    caravan.head().expect("caravan has a head"),
                    RootMergeBlock::ChecksNotPassing,
                    detail,
                );
                return Ok(());
            }
            let current_open = stack
                .topology
                .entries
                .iter()
                .filter(|entry| entry.pull_request_state == PullRequestState::Open)
                .collect::<Vec<_>>();
            if prefix.selected.len() != current_open.len() {
                let selected = prefix
                    .selected
                    .iter()
                    .map(|entry| entry.pr)
                    .collect::<Vec<_>>();
                let blocked_suffix = current_open[prefix.selected.len()..]
                    .iter()
                    .map(|entry| entry.pr)
                    .collect::<Vec<_>>();
                let immutable_heads = stack
                    .topology
                    .entries
                    .iter()
                    .map(|entry| (entry.pr, entry.head.clone()))
                    .collect::<BTreeMap<_, _>>();
                return Err(AppError::structured(
                    ErrorCategory::Validation,
                    "github_stack_partial_prefix_requires_tail_eviction",
                    "native Stack partial-prefix merge would let GitHub rewrite an unselected tail source branch",
                    Some(json!({
                        "repository": repository,
                        "caravan_id": caravan.id,
                        "stack_number": stack_number,
                        "selected_ready_prefix": selected,
                        "blocked_suffix": blocked_suffix,
                        "first_blocked": prefix.first_blocked,
                        "immutable_source_heads": immutable_heads,
                        "mutated": false,
                        "resumable": false,
                        "safe_next_action": "use the typed native Stack reshape path to evict/split the blocked final suffix, then rerun sync against the remaining exact Stack; never update or force-push a source branch to refresh CI",
                    })),
                ));
            }
            let plan = prefix
                .direct_squash_plan(
                    format!("{}:stack:{stack_number}", self.operation_id),
                    native.config.stack_rollout.reviewed_by.clone(),
                )
                .map_err(native_sync_error)?;
            let checkpoint =
                GitHubMutationAdapter::<crate::command::ProcessRunner>::native_stack_land_begin(
                    repository, &plan,
                );
            // Durable before lock acquisition, the first provider write.
            crate::stack_checkpoint::write(&native.repository_path, &key, &checkpoint)?;
            checkpoint
        };

        self.ensure_mutation_capacity(3)?;
        let mut submission_marked_now = false;
        // At most one poll that leaves the phase unchanged per tick. Lock,
        // submission, terminal proof, and release may all progress immediately;
        // a genuinely pending UUID keeps its lock and resumes next tick.
        for _ in 0..5 {
            let before_phase = checkpoint.phase;
            checkpoint = match checkpoint.phase {
                crate::github::GitHubStackLandPhase::Planned => {
                    provider.native_stack_land_lock_for_sync(repository, &checkpoint)?
                }
                crate::github::GitHubStackLandPhase::Locked => {
                    submission_marked_now = true;
                    GitHubMutationAdapter::<crate::command::ProcessRunner>::native_stack_land_mark_submitting(
                        repository,
                        &checkpoint,
                    )
                    .map_err(native_sync_error)?
                }
                crate::github::GitHubStackLandPhase::Submitting if submission_marked_now => {
                    provider.native_stack_land_submit_for_sync(repository, &checkpoint)?
                }
                crate::github::GitHubStackLandPhase::Submitting => {
                    // A previous process may have submitted after persisting the
                    // marker but before persisting its UUID. Never blind-retry.
                    GitHubMutationAdapter::<crate::command::ProcessRunner>::native_stack_land_abandon_uncertain_submission(
                        repository,
                        &checkpoint,
                    )
                    .map_err(native_sync_error)?
                }
                crate::github::GitHubStackLandPhase::Submitted => {
                    provider.native_stack_land_poll_for_sync(repository, &checkpoint)?
                }
                crate::github::GitHubStackLandPhase::Terminal => {
                    provider.native_stack_land_release_for_sync(repository, &checkpoint)?
                }
                crate::github::GitHubStackLandPhase::Released => break,
            };
            crate::stack_checkpoint::write(&native.repository_path, &key, &checkpoint)?;
            if checkpoint.phase == before_phase {
                break;
            }
            if matches!(
                checkpoint.phase,
                crate::github::GitHubStackLandPhase::Locked
                    | crate::github::GitHubStackLandPhase::Submitted
                    | crate::github::GitHubStackLandPhase::Released
            ) {
                self.steps.push(MutationStep {
                    kind: MutationKind::NativeStackLand,
                    state: MutationStepState::Completed,
                    pr: caravan.head(),
                    summary: format!(
                        "native Stack #{stack_number} landing advanced from {before_phase:?} to {:?}",
                        checkpoint.phase
                    ),
                });
            }
        }

        if checkpoint.phase != crate::github::GitHubStackLandPhase::Released {
            self.native_stack_land.push(checkpoint);
            return Ok(());
        }
        crate::stack_checkpoint::remove(&native.repository_path, &key)?;
        let status = checkpoint.terminal_status;
        self.native_stack_land.push(checkpoint.clone());
        match status {
            Some(crate::github::GitHubStackMergeStatus::Merged) => {
                for entry in &checkpoint.plan.selected {
                    self.steps.push(MutationStep {
                        kind: MutationKind::SquashMerge,
                        state: MutationStepState::Completed,
                        pr: Some(entry.pr),
                        summary: format!(
                            "native Stack #{stack_number} atomically merged the selected ready prefix under exact-ref lock"
                        ),
                    });
                }
                Ok(())
            }
            Some(crate::github::GitHubStackMergeStatus::Failed) => Err(AppError::structured(
                ErrorCategory::ExecutionFailure,
                "github_stack_merge_failed",
                "GitHub reported terminal native Stack merge failure",
                Some(json!({"checkpoint": checkpoint, "resumable": false})),
            )),
            Some(crate::github::GitHubStackMergeStatus::Indeterminate) => {
                Err(AppError::structured(
                    ErrorCategory::ExecutionFailure,
                    "github_stack_merge_indeterminate",
                    "native Stack merge ended indeterminate; exact receipts require operator inspection",
                    Some(json!({"checkpoint": checkpoint, "resumable": false})),
                ))
            }
            other => Err(AppError::structured(
                ErrorCategory::ExecutionFailure,
                "github_stack_terminal_status_invalid",
                "released native Stack transaction lacks terminal provider proof",
                Some(json!({"status": other, "checkpoint": checkpoint})),
            )),
        }
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
        // The exact default-branch generation this tick's own landing produced.
        // It is the only movement of the default branch a successor is allowed
        // to land against without containing it, because it is the only
        // movement this caravan can prove it caused.
        let mut landed_default: Option<crate::model::CommitOid> = None;
        while let Some(&root) = remaining.first() {
            if merged >= self.max_root_merges {
                self.record_merge_wait(root, RootMergeBlock::MergeBudgetReached, None);
                return Ok(());
            }
            let landed = self.merge_root(
                provider,
                status,
                caravan.id,
                root,
                &remaining,
                landed_default.as_ref(),
            )?;
            if !landed {
                return Ok(());
            }
            landed_default = self
                .root_merge
                .iter()
                .rev()
                .find(|receipt| receipt.pr == root)
                .map(|receipt| receipt.ancestry.default_after.oid.clone());
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
        landed_default: Option<&crate::model::CommitOid>,
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
        // Tree identity proves *what* would land, not that the default branch
        // still wants it. An operator who reverts or discards an already-landed
        // ancestor leaves the successor carrying that ancestor's diff; the
        // three-way merge silently reapplies it and still yields exactly the
        // successor's own tree. Containment is what separates that from the
        // ordinary case, and it is refused rather than waited on because only
        // proving or rescoping the caravan's content can resolve it.
        if !root_merge::retained_patch_set_holds(
            tree_proof.target_reachable_from_candidate,
            landed_default.is_some_and(|oid| oid == &observed_default),
        ) {
            return Err(self.root_merge_failure(
                caravan_id,
                number,
                RootMergeFailureCause::DefaultBranchDivergedFromRetainedPatchSet,
                &observed,
                Some(&expected_head),
                &json!({
                    "default_branch": default_branch,
                    "observed_default_oid": observed_default,
                    "landed_by_this_tick": landed_default,
                    "cumulative_tree": tree_proof,
                    "explanation": "the exact default branch neither contains this head nor is the generation this tick's own landing produced; landing would reintroduce content the default branch no longer carries",
                }),
            ));
        }

        // Independent forge cross-check, deliberately placed HERE and not at
        // admission. Measured on live cacophony, `BLOCKED` does not distinguish
        // red from clean: a PR whose required checks are merely still running
        // reports it exactly like one whose checks failed, so refusing admission
        // on it would stall every candidate. By this point the required checks
        // are already proven green, so a forge that still declines knows
        // something we do not, and attempting the merge only produces a
        // confusing provider error.
        if let Some(status) = observed
            .merge_state_status
            .as_deref()
            .filter(|status| matches!(*status, "BLOCKED" | "DIRTY" | "DRAFT"))
        {
            return Err(self.root_merge_failure(
                caravan_id,
                number,
                RootMergeFailureCause::ForgeRefusesMerge,
                &observed,
                Some(&expected_head),
                &json!({
                    "merge_state_status": status,
                    "explanation": "required checks are green but the forge still refuses this exact head; an unsatisfied protection rule or required review is the usual cause",
                }),
            ));
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
            .map_err(|error| {
                if provider_requires_native_stack_merge(&error) {
                    stack_membership_detected_during_owned_merge(&error, self, caravan_id, number)
                } else {
                    mutation_error(&error, self, Some(number))
                }
            })?;
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
                "repository": self.repository,
                "caravan_id": caravan_id,
                "affected_pr": number,
                "affected_prs": [number],
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
                // Harvested by the sync_failed repair-wake path so a
                // cron-dispatched agent receives the exact caravan and PRs
                // without parsing prose.
                "repository": self.repository,
                "caravan_id": caravan_id,
                "affected_pr": number,
                "affected_prs": [number],
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

fn provider_requires_native_stack_merge(error: &MutationError) -> bool {
    let diagnostic = error.to_string().to_ascii_lowercase();
    diagnostic.contains("part of a stack")
        || diagnostic.contains("use the stack api")
        || diagnostic.contains("use stack api")
}

fn stack_membership_detected_during_owned_merge(
    error: &MutationError,
    progress: &SyncProgress,
    caravan_id: PrNumber,
    affected_pr: PrNumber,
) -> AppError {
    AppError::structured(
        ErrorCategory::Validation,
        "github_stack_membership_detected_during_owned_merge",
        "GitHub refused ordinary PR merge because the exact PR became a native Stack member after routing preflight",
        Some(json!({
            "repository": progress.repository,
            "caravan_id": caravan_id,
            "affected_prs": [affected_pr],
            "provider_error": format!("{error:?}"),
            "operation_receipt": progress.operation_receipt(),
            "provider_receipts": progress.provider_receipts,
            "events": progress.events,
            "merge_mutated": false,
            "retryable": false,
            "resumable": false,
            "safe_next_action": "rediscover the complete provider Stack inventory and inspect the exact intersecting generation; do not retry unchanged synchronous PR merge",
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

/// Separate "did not finish" from "was judged and failed".
///
/// A cancelled required check is not a verdict, and an aggregate check that
/// converts a cancelled prerequisite into a terminal failure looks identical to
/// a real failure in the forge summary. The distinguishing fact is bounded and
/// already fetched: a genuine failure names at least one failing step, while an
/// aggregate conversion names none (bd-1ac172).
fn classify_cancellation(
    checks: &[CheckSnapshot],
    diagnostics: &[ClassifiedWorkflowRunFailure],
) -> CiCancellationSummary {
    // bd-eff1dc: explain the CURRENT refusal. A superseded cancellation is
    // lineage, and naming it here would tell the reader to rerun a run that has
    // already been rerun.
    let (current, _superseded) = crate::model::latest_checks_per_identity(checks);
    let cancelled_checks = current
        .iter()
        .filter(|check| check.state == CheckState::Cancelled)
        .map(|check| check.name.clone())
        .collect::<Vec<_>>();
    let stepless = |diagnostic: &ClassifiedWorkflowRunFailure| {
        diagnostic
            .diagnostic
            .failed_jobs
            .iter()
            .all(|job| job.failed_steps.is_empty())
    };
    let failures_without_failing_step = diagnostics
        .iter()
        .filter(|diagnostic| stepless(diagnostic))
        .map(|diagnostic| diagnostic.diagnostic.workflow_name.clone())
        .collect::<Vec<_>>();
    let judged_failure_exists = current
        .iter()
        .any(|check| check.state == CheckState::Failure)
        && diagnostics.iter().any(|diagnostic| !stepless(diagnostic));
    let cancellation_only = !judged_failure_exists
        && (!cancelled_checks.is_empty() || !failures_without_failing_step.is_empty());
    CiCancellationSummary {
        cancelled_checks,
        failures_without_failing_step,
        cancellation_only,
    }
}

/// Refuse a mutating tick whose policy provably came from an older generation.
///
/// A sync worktree was found parked on a dead agent's branch 95 commits behind
/// main. Every value the queue read — including its own duration budget — came
/// from a three-day-old commit, the operator's current policy was never read,
/// and nothing noticed for days. The distance was locally available the whole
/// time; only nothing consulted it. A deliberate branch proposal is still
/// allowed: refusal requires differing policy *and* a checkout that is behind
/// (bd-6f234e).
fn require_current_policy(context: &AppContext, status: &StatusOutput) -> Result<(), AppError> {
    let Some(provenance) = status
        .config_provenance
        .as_ref()
        .filter(|provenance| provenance.is_stale_policy())
    else {
        return Ok(());
    };
    Err(AppError::structured(
        ErrorCategory::Validation,
        "stale_repository_policy",
        "the effective configuration came from a checkout behind the default branch",
        Some(json!({
            "provenance": provenance,
            "resumable": true,
            "operator_action_required": true,
            "next": if context.config.sync.allow_fetch {
                "rerun after resolving the reported authoritative-default materialization failure"
            } else {
                "enable sync.allow_fetch (or CARA_ALLOW_FETCH=true) and rerun the same idempotent sync"
            },
            "safe_next_action": if context.config.sync.allow_fetch {
                "inspect the preceding sync_default_branch_* error; never reset the invoking branch just to run sync"
            } else {
                "enable the default-on bounded fetch, or pass an explicit reviewed --config; Cara will not reset or check out the invoking branch"
            },
            "why": "every admission, budget, and hook decision must come from one exact authoritative policy generation; an older branch-local generation is never used for provider writes",
        })),
    ))
}
