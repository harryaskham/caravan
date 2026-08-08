//! Detect caravan heads whose required contexts have no reporting run at all.
//!
//! A rebase-on-join can publish a new caravan head for which GitHub never
//! starts a workflow run. The pull request then sits `MERGEABLE`/`BLOCKED` with
//! *zero* reporting required contexts: nothing is pending, nothing failed, and
//! a scheduler that only reads `statusCheckRollup` concludes "waiting for CI"
//! forever while the whole caravan is dead.
//!
//! This module owns the exact facts needed to tell that state apart from honest
//! CI latency, and it deliberately consumes provider evidence only:
//!
//! - the required contexts declared by protection on the *exact base branch*;
//! - the check-suite and workflow-run lineage observed on the *exact head OID*,
//!   so a run belonging to a superseded generation never counts as coverage;
//! - a bounded grace period measured from the latest provider timestamp that
//!   could have triggered CI for that head, so a freshly published head is
//!   never prematurely accused;
//! - a typed [`RequiredRunsStatus`] separating `missing_required_runs` from
//!   pending, failing, cancelled/superseded and unknown provider state.
//!
//! Recovery is deliberately minimal. The only mutation this policy will ever
//! request is exactly one auditable check-suite rerequest against the
//! *unchanged* head. Empty commits, close/reopen loops, force pushes and broad
//! reruns are never workarounds: when no safe provider primitive exists, a
//! typed operator-action problem carries the exact PR, head, and contexts plus
//! the reviewed manual recovery, and the scheduler degrades visibly instead of
//! waiting silently forever.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::{
    BranchSnapshot, CheckSnapshot, CheckState, OperationId, PrNumber, RepositoryId,
};

/// Stable schema for durable required-run receipts.
pub const REQUIRED_RUNS_RECEIPT_SCHEMA_VERSION: u32 = 1;

/// Exactly one auditable retrigger request per head generation per tick.
pub const REQUIRED_RUNS_RETRIGGER_ATTEMPTS: u32 = 1;

/// Bounded contexts retained on any single receipt or problem.
pub const MAX_REPORTED_CONTEXTS: usize = 32;

/// Bounded lineage rows retained on any single receipt.
pub const MAX_REPORTED_LINEAGE: usize = 32;

/// Bounded distinct problems retained for hooks and scheduler status.
pub const MAX_MISSING_REQUIRED_RUNS_PROBLEMS: usize = 32;

/// Stable owner recorded on every required-run assessment.
pub const REQUIRED_RUNS_OWNER: &str = "caravan-scheduler";

/// Stable component recorded on every required-run assessment.
pub const REQUIRED_RUNS_COMPONENT: &str = "cara sync";

/// Reviewed manual recovery. Cara never performs it: a close/reopen loop is an
/// implicit workaround that mutates PR state, and this policy preserves
/// head/base/branch/membership exactly.
pub const REVIEWED_MANUAL_RECOVERY: &str = "reviewed manual recovery: close and immediately reopen the exact PR to make GitHub queue a `pull_request` run on the unchanged head; never push an empty commit, force-push, retarget, or broadly rerun another generation";

/// Exact protection-declared required contexts for one base branch.
///
/// `complete` is false when the provider refused or truncated the protection
/// read. A partial read can never prove a context is missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequiredContextsRead {
    pub branch: String,
    pub protected: bool,
    #[serde(default)]
    pub contexts: Vec<String>,
    pub complete: bool,
}

impl RequiredContextsRead {
    /// An unprotected or requirement-free branch requires nothing.
    #[must_use]
    pub fn unprotected(branch: &str) -> Self {
        Self {
            branch: branch.to_owned(),
            protected: false,
            contexts: Vec::new(),
            complete: true,
        }
    }

    /// A protection read the provider would not serve completely.
    #[must_use]
    pub fn partial(branch: &str) -> Self {
        Self {
            branch: branch.to_owned(),
            protected: true,
            contexts: Vec::new(),
            complete: false,
        }
    }

    /// Deterministically ordered, deduplicated, bounded contexts.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.contexts.sort();
        self.contexts.dedup();
        self.contexts.truncate(MAX_REPORTED_CONTEXTS);
        self
    }
}

/// One check suite observed for a commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckSuiteLineage {
    pub id: u64,
    pub head_sha: String,
    pub status: String,
    pub conclusion: String,
    #[serde(default)]
    pub app_slug: String,
    /// Whether the provider exposes a safe rerequest primitive for this suite.
    pub rerequestable: bool,
}

/// One workflow run observed for a commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowRunLineage {
    pub run_id: u64,
    pub check_suite_id: u64,
    pub workflow_name: String,
    pub head_sha: String,
    pub status: String,
    pub conclusion: String,
    pub event: String,
}

/// Provider run/check-suite lineage read for one exact head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct HeadRunLineage {
    pub head_sha: String,
    #[serde(default)]
    pub check_suites: Vec<CheckSuiteLineage>,
    #[serde(default)]
    pub workflow_runs: Vec<WorkflowRunLineage>,
    /// Immutable commit timestamp of the exact head, when the provider served it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_committed_at: Option<String>,
    /// False when any lineage read was refused, truncated, or unparsable.
    pub complete: bool,
}

impl HeadRunLineage {
    /// Bound retained lineage without changing any classification input.
    #[must_use]
    pub fn bounded(mut self) -> Self {
        self.check_suites.sort_by_key(|suite| suite.id);
        self.check_suites.dedup_by_key(|suite| suite.id);
        self.check_suites.truncate(MAX_REPORTED_LINEAGE);
        self.workflow_runs.sort_by_key(|run| run.run_id);
        self.workflow_runs.dedup_by_key(|run| run.run_id);
        self.workflow_runs.truncate(MAX_REPORTED_LINEAGE);
        self
    }
}

/// Whether a provider status/conclusion pair is still expected to report.
#[must_use]
fn lineage_is_live(status: &str, conclusion: &str) -> bool {
    const LIVE: [&str; 6] = [
        "queued",
        "in_progress",
        "requested",
        "waiting",
        "pending",
        "action_required",
    ];
    if LIVE
        .iter()
        .any(|candidate| status.eq_ignore_ascii_case(candidate))
    {
        return true;
    }
    status.is_empty() && conclusion.is_empty()
}

/// Whether a provider conclusion means the run will never report a verdict.
#[must_use]
fn lineage_is_abandoned(conclusion: &str) -> bool {
    const ABANDONED: [&str; 3] = ["cancelled", "stale", "skipped"];
    ABANDONED
        .iter()
        .any(|candidate| conclusion.eq_ignore_ascii_case(candidate))
}

/// Coverage class of one required context on one exact head.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RequiredContextState {
    /// A reporting run concluded successfully (or neutral/skipped).
    Passing,
    /// A reporting run exists and is still expected to conclude.
    Pending,
    /// A reporting run concluded with an honest failure.
    Failing,
    /// The only lineage on the exact head was cancelled/superseded and will
    /// never report a verdict.
    CancelledSuperseded,
    /// Zero run/check-suite lineage reports this context on the exact head.
    Missing,
    /// The provider exposed a state this policy refuses to interpret.
    Unknown,
}

/// One required context and the exact evidence that classified it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequiredContextCoverage {
    pub context: String,
    pub state: RequiredContextState,
    /// Reporting check names observed on the exact head, if any.
    #[serde(default)]
    pub reporting_checks: Vec<String>,
    /// Exact provider state retained when normalization yields `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<String>,
}

/// Whole-PR required-run verdict for one exact head.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RequiredRunsStatus {
    /// The exact base branch declares no required context.
    NotRequired,
    /// Every required context reported a passing verdict.
    Satisfied,
    /// Every required context reports and at least one is still running.
    Pending,
    /// At least one required context reported an honest failure.
    Failing,
    /// At least one required context's only lineage was cancelled/superseded.
    CancelledSuperseded,
    /// At least one required context has zero lineage on the exact head, but
    /// the bounded grace period has not elapsed yet.
    AwaitingGrace,
    /// At least one required context has zero lineage on the exact head after
    /// the bounded grace period. This is the caravan-stalling class.
    MissingRequiredRuns,
    /// A provider read was partial, so nothing may be claimed missing.
    UnknownProviderState,
}

impl RequiredRunsStatus {
    /// Stable code embedded in receipts, problems, and events.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotRequired => "not_required",
            Self::Satisfied => "satisfied",
            Self::Pending => "pending",
            Self::Failing => "failing",
            Self::CancelledSuperseded => "cancelled_superseded",
            Self::AwaitingGrace => "awaiting_grace",
            Self::MissingRequiredRuns => "missing_required_runs",
            Self::UnknownProviderState => "unknown_provider_state",
        }
    }

    /// Whether this status stalls the caravan with no reporting lineage.
    #[must_use]
    pub const fn stalls_forever(self) -> bool {
        matches!(self, Self::MissingRequiredRuns | Self::CancelledSuperseded)
    }

    /// Whether this status is an ordinary bounded CI wait.
    #[must_use]
    pub const fn is_waiting(self) -> bool {
        matches!(self, Self::Pending | Self::AwaitingGrace)
    }
}

/// The single safe action this policy is willing to take or ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RequiredRunsRecovery {
    /// Nothing to do; required coverage is satisfied, pending, or failing.
    None,
    /// Coverage is absent but the bounded grace period has not elapsed.
    AwaitGrace,
    /// Exactly one auditable rerequest of an existing suite on the unchanged head.
    RerequestCheckSuite { check_suite_id: u64 },
    /// No safe provider primitive exists; a typed operator problem is emitted.
    OperatorAction,
}

impl RequiredRunsRecovery {
    /// Stable code embedded in receipts and problems.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::AwaitGrace => "await_grace",
            Self::RerequestCheckSuite { .. } => "rerequest_check_suite",
            Self::OperatorAction => "operator_action",
        }
    }
}

/// Bounded provider timestamps that bound the grace period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequiredRunsClock {
    pub now_unix: u64,
    pub grace_secs: u64,
}

/// Everything one pure assessment consumes. Nothing else may influence it.
#[derive(Debug, Clone, Copy)]
pub struct RequiredRunsInput<'a> {
    pub pr: PrNumber,
    pub head: &'a BranchSnapshot,
    pub base: &'a BranchSnapshot,
    pub contexts: &'a RequiredContextsRead,
    /// `None` when no required context was absent from the rollup, so the
    /// expensive lineage read was deliberately skipped.
    pub lineage: Option<&'a HeadRunLineage>,
    pub checks: &'a [CheckSnapshot],
    /// Latest provider timestamp that could have triggered CI for this head.
    pub head_published_at: Option<&'a str>,
    pub clock: RequiredRunsClock,
}

/// Complete assessment of one PR's required-run coverage on its exact head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequiredRunsAssessment {
    pub pr: PrNumber,
    pub head: BranchSnapshot,
    pub base: BranchSnapshot,
    pub status: RequiredRunsStatus,
    #[serde(default)]
    pub required_contexts: Vec<String>,
    #[serde(default)]
    pub coverage: Vec<RequiredContextCoverage>,
    /// Exact contexts with zero reporting lineage on the exact head.
    #[serde(default)]
    pub missing_contexts: Vec<String>,
    /// Check suites observed on the exact head.
    pub observed_check_suites: usize,
    /// Workflow runs observed on the exact head.
    pub observed_runs: usize,
    /// Runs the provider associated with a *different* commit. These never
    /// count as coverage; they are retained as superseded-generation evidence.
    pub stale_head_runs: usize,
    /// Whether every consulted provider read was complete.
    pub provider_reads_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_age_secs: Option<u64>,
    pub grace_secs: u64,
    pub grace_elapsed: bool,
    pub recovery: RequiredRunsRecovery,
    pub reason: String,
}

impl RequiredRunsAssessment {
    /// Whether the scheduler must expose a visible problem for this assessment.
    #[must_use]
    pub const fn requires_problem(&self) -> bool {
        self.status.stalls_forever()
            || matches!(self.status, RequiredRunsStatus::UnknownProviderState)
    }
}

/// Classify the reporting rollup entries for one required context.
///
/// `EXPECTED` is deliberately *not* reporting evidence: GitHub exposes a
/// required context as expected precisely when nothing has reported it, which is
/// the exact state that used to look like an ordinary pending check forever.
fn state_from_checks(
    matching: &[&CheckSnapshot],
) -> Option<(RequiredContextState, Option<String>)> {
    let reporting = matching
        .iter()
        .filter(|check| check.state != CheckState::Expected)
        .collect::<Vec<_>>();
    if reporting.is_empty() {
        return None;
    }
    if let Some(unknown) = reporting
        .iter()
        .find(|check| check.state == CheckState::Unknown)
    {
        return Some((
            RequiredContextState::Unknown,
            unknown.provider_state.clone(),
        ));
    }
    let provider_state = reporting
        .first()
        .and_then(|check| check.provider_state.clone());
    let state = if reporting.iter().any(|check| {
        matches!(
            check.state,
            CheckState::Failure | CheckState::TimedOut | CheckState::ActionRequired
        )
    }) {
        RequiredContextState::Failing
    } else if reporting
        .iter()
        .any(|check| check.state == CheckState::Cancelled)
    {
        RequiredContextState::CancelledSuperseded
    } else if reporting
        .iter()
        .any(|check| matches!(check.state, CheckState::Queued | CheckState::InProgress))
    {
        RequiredContextState::Pending
    } else {
        RequiredContextState::Passing
    };
    Some((state, provider_state))
}

/// Classify a context with no reporting rollup entry from head lineage alone.
fn state_from_lineage(lineage: Option<&HeadRunLineage>, head_sha: &str) -> RequiredContextState {
    let Some(lineage) = lineage else {
        // The lineage read was skipped, so absence cannot be proven here.
        return RequiredContextState::Unknown;
    };
    let live = lineage
        .check_suites
        .iter()
        .filter(|suite| suite.head_sha == head_sha)
        .any(|suite| lineage_is_live(&suite.status, &suite.conclusion))
        || lineage
            .workflow_runs
            .iter()
            .filter(|run| run.head_sha == head_sha)
            .any(|run| lineage_is_live(&run.status, &run.conclusion));
    if live {
        return RequiredContextState::Pending;
    }
    let abandoned = lineage
        .check_suites
        .iter()
        .filter(|suite| suite.head_sha == head_sha)
        .any(|suite| lineage_is_abandoned(&suite.conclusion))
        || lineage
            .workflow_runs
            .iter()
            .filter(|run| run.head_sha == head_sha)
            .any(|run| lineage_is_abandoned(&run.conclusion));
    if abandoned {
        return RequiredContextState::CancelledSuperseded;
    }
    RequiredContextState::Missing
}

/// Exact head-scoped lineage counts, keeping superseded generations separate.
fn lineage_counts(lineage: Option<&HeadRunLineage>, head_sha: &str) -> (usize, usize, usize) {
    let Some(lineage) = lineage else {
        return (0, 0, 0);
    };
    let suites = lineage
        .check_suites
        .iter()
        .filter(|suite| suite.head_sha == head_sha)
        .count();
    let runs = lineage
        .workflow_runs
        .iter()
        .filter(|run| run.head_sha == head_sha)
        .count();
    let stale = lineage
        .check_suites
        .iter()
        .filter(|suite| suite.head_sha != head_sha)
        .count()
        + lineage
            .workflow_runs
            .iter()
            .filter(|run| run.head_sha != head_sha)
            .count();
    (suites, runs, stale)
}

/// The single suite this policy is willing to rerequest on the unchanged head.
///
/// Deterministic (lowest ID) so a retry addresses the same suite, and strictly
/// scoped to the exact head so a superseded generation is never touched.
#[must_use]
pub fn rerequestable_suite(lineage: Option<&HeadRunLineage>, head_sha: &str) -> Option<u64> {
    lineage?
        .check_suites
        .iter()
        .filter(|suite| suite.head_sha == head_sha && suite.rerequestable)
        .map(|suite| suite.id)
        .min()
}

/// Assess required-run coverage for one PR against one exact head.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn assess(input: &RequiredRunsInput<'_>) -> RequiredRunsAssessment {
    let head_sha = input.head.oid.0.clone();
    let (observed_check_suites, observed_runs, stale_head_runs) =
        lineage_counts(input.lineage, &head_sha);
    let head_age_secs = input
        .head_published_at
        .or(input
            .lineage
            .and_then(|lineage| lineage.head_committed_at.as_deref()))
        .and_then(rfc3339_to_unix_secs)
        .map(|published| input.clock.now_unix.saturating_sub(published));
    let grace_elapsed = head_age_secs.is_some_and(|age| age >= input.clock.grace_secs);

    if input.contexts.contexts.is_empty() && input.contexts.complete {
        return RequiredRunsAssessment {
            pr: input.pr,
            head: input.head.clone(),
            base: input.base.clone(),
            status: RequiredRunsStatus::NotRequired,
            required_contexts: Vec::new(),
            coverage: Vec::new(),
            missing_contexts: Vec::new(),
            observed_check_suites,
            observed_runs,
            stale_head_runs,
            provider_reads_complete: true,
            head_age_secs,
            grace_secs: input.clock.grace_secs,
            grace_elapsed,
            recovery: RequiredRunsRecovery::None,
            reason: format!(
                "base branch `{}` declares no required status check; required-run coverage cannot stall this head",
                input.base.name
            ),
        };
    }

    // Required-run coverage must consume the same canonical current-check
    // projection as CI classification. Keep the full matching lineage below
    // for diagnostics, but only rows not positively proven superseded may vote.
    // Reducing the complete rollup (rather than each context independently) is
    // important: a newer workflow generation can retire an older job before
    // that job has materialized in the replacement run.
    let (current_checks, _superseded_checks) =
        crate::model::latest_checks_per_identity(input.checks);
    let mut coverage = Vec::new();
    for context in &input.contexts.contexts {
        let matching = input
            .checks
            .iter()
            .filter(|check| check.name == *context)
            .collect::<Vec<_>>();
        let current_matching = current_checks
            .iter()
            .copied()
            .filter(|check| check.name == *context)
            .collect::<Vec<_>>();
        let reporting_checks = matching
            .iter()
            .filter(|check| check.state != CheckState::Expected)
            .map(|check| check.name.clone())
            .collect::<Vec<_>>();
        let (state, provider_state) = match state_from_checks(&current_matching) {
            Some(reported) => reported,
            None => (state_from_lineage(input.lineage, &head_sha), None),
        };
        coverage.push(RequiredContextCoverage {
            context: context.clone(),
            state,
            reporting_checks,
            provider_state,
        });
    }
    coverage.truncate(MAX_REPORTED_CONTEXTS);

    let missing_contexts = coverage
        .iter()
        .filter(|item| item.state == RequiredContextState::Missing)
        .map(|item| item.context.clone())
        .collect::<Vec<_>>();
    let lineage_complete = input.lineage.is_none_or(|lineage| lineage.complete);
    let provider_reads_complete = input.contexts.complete && lineage_complete;
    let has_unknown = coverage
        .iter()
        .any(|item| item.state == RequiredContextState::Unknown);

    let (status, reason) = if has_unknown || !provider_reads_complete {
        (
            RequiredRunsStatus::UnknownProviderState,
            format!(
                "provider evidence for PR #{} head {head_sha} is incomplete (protection_read_complete={}, lineage_read_complete={lineage_complete}); nothing may be declared missing from a partial read",
                input.pr.0, input.contexts.complete
            ),
        )
    } else if !missing_contexts.is_empty() {
        if head_age_secs.is_none() {
            (
                RequiredRunsStatus::UnknownProviderState,
                format!(
                    "PR #{} head {head_sha} has no reporting run for {} but the provider served no head timestamp, so the bounded grace period cannot be evaluated",
                    input.pr.0,
                    missing_contexts.join(", ")
                ),
            )
        } else if grace_elapsed {
            (
                RequiredRunsStatus::MissingRequiredRuns,
                format!(
                    "PR #{} head {head_sha} has zero reporting run or check-suite lineage for required {} after {}s (grace {}s); {observed_check_suites} suites and {observed_runs} runs exist on this exact head",
                    input.pr.0,
                    missing_contexts.join(", "),
                    head_age_secs.unwrap_or_default(),
                    input.clock.grace_secs
                ),
            )
        } else {
            (
                RequiredRunsStatus::AwaitingGrace,
                format!(
                    "PR #{} head {head_sha} has no reporting run for {} yet, still inside the bounded {}s grace period",
                    input.pr.0,
                    missing_contexts.join(", "),
                    input.clock.grace_secs
                ),
            )
        }
    } else if coverage
        .iter()
        .any(|item| item.state == RequiredContextState::CancelledSuperseded)
    {
        (
            RequiredRunsStatus::CancelledSuperseded,
            format!(
                "PR #{} head {head_sha} has only cancelled or superseded lineage for a required context, which never reports a verdict",
                input.pr.0
            ),
        )
    } else if coverage
        .iter()
        .any(|item| item.state == RequiredContextState::Failing)
    {
        (
            RequiredRunsStatus::Failing,
            format!(
                "PR #{} head {head_sha} has an honest required-context failure owned by CI decision policy",
                input.pr.0
            ),
        )
    } else if coverage
        .iter()
        .any(|item| item.state == RequiredContextState::Pending)
    {
        (
            RequiredRunsStatus::Pending,
            format!(
                "PR #{} head {head_sha} has a reporting run for every required context and at least one is still running",
                input.pr.0
            ),
        )
    } else {
        (
            RequiredRunsStatus::Satisfied,
            format!(
                "PR #{} head {head_sha} has a passing reporting run for every required context",
                input.pr.0
            ),
        )
    };

    let recovery = match status {
        RequiredRunsStatus::MissingRequiredRuns | RequiredRunsStatus::CancelledSuperseded => {
            rerequestable_suite(input.lineage, &head_sha)
                .map_or(RequiredRunsRecovery::OperatorAction, |check_suite_id| {
                    RequiredRunsRecovery::RerequestCheckSuite { check_suite_id }
                })
        }
        RequiredRunsStatus::AwaitingGrace => RequiredRunsRecovery::AwaitGrace,
        _ => RequiredRunsRecovery::None,
    };

    RequiredRunsAssessment {
        pr: input.pr,
        head: input.head.clone(),
        base: input.base.clone(),
        status,
        required_contexts: input.contexts.contexts.clone(),
        coverage,
        missing_contexts,
        observed_check_suites,
        observed_runs,
        stale_head_runs,
        provider_reads_complete,
        head_age_secs,
        grace_secs: input.clock.grace_secs,
        grace_elapsed,
        recovery,
        reason,
    }
}

/// Outcome of the single auditable retrigger this policy may request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequiredRunsRetrigger {
    pub check_suite_id: u64,
    /// Exact unchanged head the request was scoped to.
    pub head_oid: crate::model::CommitOid,
    pub attempts: u32,
    /// Whether the provider accepted the rerequest.
    pub requested: bool,
    /// Whether exactly one rediscovery followed the request.
    pub rediscovered: bool,
    /// Status observed by that single rediscovery.
    pub status_after: RequiredRunsStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// Auditable provenance for one required-run assessment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequiredRunsProvenance {
    pub owner: String,
    pub component: String,
    pub operation_id: OperationId,
    pub reason: String,
}

/// Build provenance for one required-run decision.
#[must_use]
pub fn provenance(operation_id: &OperationId, reason: &str) -> RequiredRunsProvenance {
    RequiredRunsProvenance {
        owner: REQUIRED_RUNS_OWNER.to_owned(),
        component: REQUIRED_RUNS_COMPONENT.to_owned(),
        operation_id: operation_id.clone(),
        reason: reason.to_owned(),
    }
}

/// Durable proof of what one tick observed about required-run coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequiredRunsReceipt {
    pub schema_version: u32,
    pub repository: RepositoryId,
    pub caravan_id: PrNumber,
    pub pr: PrNumber,
    pub assessment: RequiredRunsAssessment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrigger: Option<RequiredRunsRetrigger>,
    pub provenance: RequiredRunsProvenance,
    /// Deterministic hash with this field omitted.
    pub evidence_hash: String,
}

impl RequiredRunsReceipt {
    /// Seal the receipt with its deterministic evidence hash.
    #[must_use]
    pub fn finalize_hash(mut self) -> Self {
        self.evidence_hash.clear();
        let material = serde_json::to_vec(&self).expect("required-run receipt serializes");
        self.evidence_hash = crate::membership::fnv1a64(&material);
        self
    }

    /// Whether the sealed hash still matches the receipt body.
    #[must_use]
    pub fn hash_is_valid(&self) -> bool {
        let mut material = self.clone();
        let expected = material.evidence_hash.clone();
        material.evidence_hash.clear();
        serde_json::to_vec(&material)
            .ok()
            .is_some_and(|bytes| crate::membership::fnv1a64(&bytes) == expected)
    }
}

/// Build a sealed receipt for one assessed PR.
#[must_use]
pub fn receipt(
    repository: &RepositoryId,
    caravan_id: PrNumber,
    assessment: RequiredRunsAssessment,
    retrigger: Option<RequiredRunsRetrigger>,
    provenance: RequiredRunsProvenance,
) -> RequiredRunsReceipt {
    RequiredRunsReceipt {
        schema_version: REQUIRED_RUNS_RECEIPT_SCHEMA_VERSION,
        repository: repository.clone(),
        caravan_id,
        pr: assessment.pr,
        assessment,
        retrigger,
        provenance,
        evidence_hash: String::new(),
    }
    .finalize_hash()
}

/// Typed problem class exposed to schedulers and hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissingRequiredRunsKind {
    /// Required contexts have zero lineage on the exact head after grace.
    MissingRequiredRuns,
    /// The only lineage on the exact head was cancelled/superseded.
    CancelledSupersededRequiredRuns,
    /// A provider read was partial, so coverage could not be proven either way.
    UnknownProviderState,
}

impl MissingRequiredRunsKind {
    /// Stable code embedded in scheduler status and hook events.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::MissingRequiredRuns => "missing_required_runs",
            Self::CancelledSupersededRequiredRuns => "cancelled_superseded_required_runs",
            Self::UnknownProviderState => "unknown_required_runs_provider_state",
        }
    }

    /// Whether an operator, rather than another bounded tick, must act.
    #[must_use]
    pub const fn operator_action_required(self) -> bool {
        !matches!(self, Self::UnknownProviderState)
    }
}

/// A visible, deduplicated, bounded scheduler problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MissingRequiredRunsProblem {
    pub kind: MissingRequiredRunsKind,
    pub caravan_id: PrNumber,
    pub pr: PrNumber,
    pub head: BranchSnapshot,
    pub base: BranchSnapshot,
    /// Exact contexts with no reporting lineage on the exact head.
    #[serde(default)]
    pub contexts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrigger: Option<RequiredRunsRetrigger>,
    pub operator_action_required: bool,
    pub message: String,
    pub next: String,
    /// Stable identity used to deduplicate receipts, events, and hook payloads.
    pub fingerprint: String,
}

/// Derive the visible problem for one assessment, if any.
#[must_use]
pub fn problem(
    caravan_id: PrNumber,
    assessment: &RequiredRunsAssessment,
    retrigger: Option<&RequiredRunsRetrigger>,
) -> Option<MissingRequiredRunsProblem> {
    let kind = match assessment.status {
        RequiredRunsStatus::MissingRequiredRuns => MissingRequiredRunsKind::MissingRequiredRuns,
        RequiredRunsStatus::CancelledSuperseded => {
            MissingRequiredRunsKind::CancelledSupersededRequiredRuns
        }
        RequiredRunsStatus::UnknownProviderState => MissingRequiredRunsKind::UnknownProviderState,
        _ => return None,
    };
    let contexts = if assessment.missing_contexts.is_empty() {
        assessment.required_contexts.clone()
    } else {
        assessment.missing_contexts.clone()
    };
    let next = match kind {
        MissingRequiredRunsKind::UnknownProviderState => {
            "rerun the same idempotent bounded sync tick once provider reads are complete; nothing is claimed missing from a partial read".to_owned()
        }
        _ => match assessment.recovery {
            RequiredRunsRecovery::RerequestCheckSuite { check_suite_id } => format!(
                "the single auditable rerequest of check suite {check_suite_id} on unchanged head {} did not produce reporting lineage; {REVIEWED_MANUAL_RECOVERY}",
                assessment.head.oid
            ),
            _ => format!(
                "no safe provider retrigger exists for head {} (zero rerequestable check suites); {REVIEWED_MANUAL_RECOVERY}",
                assessment.head.oid
            ),
        },
    };
    let fingerprint = fingerprint(kind, assessment.pr, &assessment.head.oid.0, &contexts);
    Some(MissingRequiredRunsProblem {
        kind,
        caravan_id,
        pr: assessment.pr,
        head: assessment.head.clone(),
        base: assessment.base.clone(),
        contexts,
        retrigger: retrigger.cloned(),
        operator_action_required: kind.operator_action_required(),
        message: assessment.reason.clone(),
        next,
        fingerprint,
    })
}

/// Stable identity of one problem: kind, PR, exact head, and exact contexts.
#[must_use]
pub fn fingerprint(
    kind: MissingRequiredRunsKind,
    pr: PrNumber,
    head_oid: &str,
    contexts: &[String],
) -> String {
    let unique = contexts.iter().cloned().collect::<BTreeSet<_>>();
    let material = serde_json::to_vec(&serde_json::json!({
        "kind": kind.code(),
        "pr": pr,
        "head_oid": head_oid,
        "contexts": unique,
    }))
    .expect("required-run fingerprint material serializes");
    crate::membership::fnv1a64(&material)
}

/// Merge a problem into a deduplicated, bounded problem list.
///
/// Returns whether the problem was retained, so a caller can keep hook evidence
/// exactly as deduplicated and bounded as the problem list itself.
pub fn push_problem(
    problems: &mut Vec<MissingRequiredRunsProblem>,
    problem: MissingRequiredRunsProblem,
) -> bool {
    if problems
        .iter()
        .any(|existing| existing.fingerprint == problem.fingerprint)
    {
        return false;
    }
    if problems.len() >= MAX_MISSING_REQUIRED_RUNS_PROBLEMS {
        return false;
    }
    problems.push(problem);
    true
}

/// Convert a strict RFC 3339 provider timestamp to Unix seconds.
///
/// GitHub serves both `Z` and numeric-offset commit timestamps, so both are
/// accepted. Anything else stays `None` rather than being guessed: an unparsed
/// timestamp makes the grace period unknown, which is reported as unknown
/// provider state instead of silently starting a countdown from zero.
#[must_use]
pub fn rfc3339_to_unix_secs(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let number = |start: usize, end: usize| -> Option<i64> {
        let slice = value.get(start..end)?;
        if !slice.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        slice.parse::<i64>().ok()
    };
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    if !matches!(bytes[10], b'T' | b't') {
        return None;
    }
    let year = number(0, 4)?;
    let month = number(5, 7)?;
    let day = number(8, 10)?;
    let hour = number(11, 13)?;
    let minute = number(14, 16)?;
    let second = number(17, 19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut index = 19;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
    }
    let offset_secs = match bytes.get(index) {
        Some(b'Z' | b'z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            if bytes.get(index + 3) != Some(&b':') {
                return None;
            }
            let hours = number(index + 1, index + 3)?;
            let minutes = number(index + 4, index + 6)?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let magnitude = hours * 3_600 + minutes * 60;
            if *sign == b'+' { magnitude } else { -magnitude }
        }
        _ => return None,
    };
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
        - offset_secs;
    u64::try_from(seconds).ok()
}

/// Days since the Unix epoch for a proleptic Gregorian civil date.
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests;
