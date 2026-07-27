//! Explicit owner intent versus automatic first-admission FIFO order.
//!
//! Priority-then-FIFO order is the contract for *automatic* selection: which
//! unjoined PR sync picks next as the new caravan root or the next
//! automatically grown member. It was never a claim that an operator or agent
//! naming one exact PR must first wait for every unrelated unjoined PR ahead of
//! it.
//!
//! Two axes are therefore modelled separately, because conflating them is
//! exactly how this behaviour regressed once already:
//!
//! * [`AdmissionSelection`] — *who chose this candidate*: automatic priority/
//!   FIFO order, an explicit owner request naming a remote PR, or the owner's
//!   own checked-out PR.
//! * [`AdmissionIntent`] — *what the candidate asked for*: form a new caravan
//!   (`new`) or attach to a resolved live target (`join`).
//!
//! Automatic selection is bound by FIFO for both intents, without exception.
//! Explicit owner selection is resolved *before* FIFO canonical-candidate
//! rejection for both intents, and may pass earlier rows *only* when every
//! bypassed row is an unrelated, unjoined first-admission attempt. It never
//! passes a joined row, a base-chain dependency, or a row whose canonical rank
//! cannot even be computed, and it never substitutes for compatibility,
//! freshness, generation, policy, or provider preflight.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::graph::GraphAnalysis;
use crate::model::{Caravan, PrNumber, PullRequestSnapshot, PullRequestState};
use crate::read::AdmissionStatus;

/// Deterministic contract text bound into every emitted decision.
pub const ADMISSION_INTENT_POLICY: &str = "Priority/FIFO order binds automatic admission selection for both `new` and `join` intent, without exception. Explicit owner intent — an operator or agent naming one exact remote candidate with `cara check --pr N`, optionally `--tail-pr`/`--head-pr` — is resolved before FIFO canonical-candidate rejection for both `new` and `join`, and may attach ahead of earlier rows only while every bypassed row is an unrelated unjoined first-admission attempt; a joined row, a base-chain dependency of the candidate, a rank-indeterminate row, a candidate that is not itself a current ordered admission attempt, or an unresolved/ambiguous join target fails closed on canonical order. An owner operating on their own checked-out PR (local check, membership, renew/rejoin) reports canonical position as evidence only, exactly as before. Ordering never substitutes for compatibility, dependency, policy, freshness, generation integrity, or provider preflight.";

/// Maximum base-chain hops walked while deriving candidate dependencies.
const MAX_DEPENDENCY_DEPTH: usize = 64;

/// Which admission intent the candidate declared before FIFO evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionIntent {
    /// First admission: form a new caravan from an unjoined PR.
    New,
    /// Attach to a named, already resolved live caravan target.
    Join,
}

impl AdmissionIntent {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Join => "join",
        }
    }
}

/// Who selected this candidate. Orthogonal to [`AdmissionIntent`]: `new` is not
/// a synonym for automatic selection, and `join` is not a synonym for explicit
/// selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionSelection {
    /// Sync/`next-candidate` automatic order. FIFO binds it without exception.
    Automatic,
    /// An owner named this exact remote candidate (`cara check --pr N`, with or
    /// without `--tail-pr`/`--head-pr`). Deliberate admission intent.
    Explicit,
    /// The owner is operating on their own checked-out PR (local `check`, and
    /// every local membership operation including renew/rejoin). Canonical
    /// position is reported as evidence and never gates the receipt.
    CheckedOut,
}

impl AdmissionSelection {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Explicit => "explicit",
            Self::CheckedOut => "checked_out",
        }
    }

    /// Whether canonical order may be relaxed for an unrelated unjoined row.
    /// Automatic selection never relaxes it; both owner-driven selections do.
    #[must_use]
    pub const fn relaxes_order(self) -> bool {
        matches!(self, Self::Explicit | Self::CheckedOut)
    }

    /// Whether the owner is operating on their own checked-out PR, where
    /// canonical position has always been evidence rather than a gate.
    #[must_use]
    pub const fn is_checked_out(self) -> bool {
        matches!(self, Self::CheckedOut)
    }
}

/// Why one earlier-ordered admission row was or was not bypassable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrderedRowDisposition {
    /// Unrelated unjoined first-admission attempt; explicit join may pass it.
    BypassedUnjoined,
    /// Row is a live caravan member; passing it would reorder a real fleet.
    BlockedJoined,
    /// The candidate's exact base chain depends on this row.
    BlockedDependency,
    /// Row's canonical rank cannot be computed, so nothing may pass it.
    BlockedRankIndeterminate,
    /// Automatic selection never passes an earlier ordered row, for either
    /// intent.
    BlockedAutomaticOrder,
}

impl OrderedRowDisposition {
    #[must_use]
    pub const fn bypassed(self) -> bool {
        matches!(self, Self::BypassedUnjoined)
    }
}

/// One canonical admission row ordered strictly ahead of the candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OrderedRow {
    pub pr: PrNumber,
    pub disposition: OrderedRowDisposition,
    /// Whether the row is an active member of a live caravan.
    pub joined: bool,
    /// Exact ordering/eligibility evidence copied from the admission list.
    pub reason: String,
}

/// Typed outcome of intent-aware admission ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionOrderOutcome {
    /// Candidate is the canonical first ordered admission attempt.
    Canonical,
    /// Explicit owner intent attaches ahead of unrelated unjoined FIFO rows.
    ExplicitAheadOfUnjoined,
    /// Fail closed behind the canonical row.
    BlockedByOrder,
    /// Ordering permitted the attach; exact preflight rejected it anyway.
    BlockedByPreflight,
    /// Candidate is already an active caravan member; ordering does not apply.
    AlreadyEnrolled,
    /// The owner is operating on their own checked-out PR; canonical position
    /// is evidence only and never gates this receipt.
    OwnerSelected,
}

impl AdmissionOrderOutcome {
    #[must_use]
    pub const fn admits_ahead_of_fifo(self) -> bool {
        matches!(self, Self::ExplicitAheadOfUnjoined)
    }
}

/// Typed decision and provenance for one intent-aware admission evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[allow(clippy::struct_excessive_bools)]
pub struct AdmissionIntentDecision {
    pub schema_version: u32,
    pub intent: AdmissionIntent,
    /// Who chose this candidate. Automatic selection is FIFO-bound for both
    /// intents; explicit owner selection is not.
    pub selection: AdmissionSelection,
    pub outcome: AdmissionOrderOutcome,
    pub candidate_pr: PrNumber,
    /// Canonical first ordered attempt at decision time, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_candidate_pr: Option<PrNumber>,
    /// Resolved join target caravan; absent for new-caravan intent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_caravan: Option<PrNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_tail: Option<PrNumber>,
    /// Every canonical row ordered strictly ahead of the candidate.
    #[serde(default)]
    pub ordered_rows_ahead: Vec<OrderedRow>,
    /// Rows passed only because they are unrelated unjoined attempts.
    #[serde(default)]
    pub bypassed_unjoined_prs: Vec<PrNumber>,
    /// Rows which fail the attach closed.
    #[serde(default)]
    pub blocking_prs: Vec<PrNumber>,
    /// Candidate base-chain dependencies observed in this snapshot.
    #[serde(default)]
    pub dependency_prs: Vec<PrNumber>,
    /// Whether exact compatibility preflight was clean for this attach.
    #[serde(default)]
    pub compatibility_clean: bool,
    /// Whether every non-ordering preflight (policy, freshness, graph) passed.
    #[serde(default)]
    pub preflight_clean: bool,
    /// Whether this decision was followed by a real provider mutation.
    #[serde(default)]
    pub provider_mutated: bool,
    /// Whether the bound operation was an exact no-op replay.
    #[serde(default)]
    pub idempotent: bool,
    pub reason: String,
    pub policy: String,
}

impl AdmissionIntentDecision {
    /// Whether ordering alone permits admitting this candidate now.
    #[must_use]
    pub const fn order_permits_admission(&self) -> bool {
        matches!(
            self.outcome,
            AdmissionOrderOutcome::Canonical
                | AdmissionOrderOutcome::ExplicitAheadOfUnjoined
                | AdmissionOrderOutcome::AlreadyEnrolled
                | AdmissionOrderOutcome::OwnerSelected
        )
    }

    /// Whether this decision passed earlier FIFO rows on explicit join intent.
    #[must_use]
    pub const fn bypasses_fifo(&self) -> bool {
        self.outcome.admits_ahead_of_fifo()
    }

    /// Bind exact preflight evidence once compatibility and policy are known.
    pub fn record_preflight(&mut self, compatibility_clean: bool, preflight_clean: bool) {
        self.compatibility_clean = compatibility_clean;
        self.preflight_clean = preflight_clean;
        if !preflight_clean && self.outcome.admits_ahead_of_fifo() {
            self.outcome = AdmissionOrderOutcome::BlockedByPreflight;
            self.reason = format!(
                "explicit {} intent cleared canonical order, but exact preflight rejected the attach; {}",
                self.intent.name(),
                self.reason
            );
        }
    }

    /// Bind exact provider-mutation and idempotency evidence after execution.
    pub fn record_execution(&mut self, provider_mutated: bool) {
        self.provider_mutated = provider_mutated;
        self.idempotent = !provider_mutated;
    }
}

/// Deterministic canonical sort key shared with `resolve_admission`.
fn key(
    priority_rank: Option<usize>,
    created_at: Option<&String>,
    pr: PrNumber,
    unranked: usize,
) -> (usize, bool, String, PrNumber) {
    (
        priority_rank.unwrap_or(unranked),
        created_at.is_none(),
        created_at.cloned().unwrap_or_default(),
        pr,
    )
}

/// Walk the candidate's exact base chain and collect every open PR it depends on.
#[must_use]
pub fn dependency_prs(analysis: &GraphAnalysis, candidate: &PullRequestSnapshot) -> Vec<PrNumber> {
    let mut dependencies = BTreeSet::new();
    let mut branch = candidate.base.name.clone();
    let repository = candidate.base.repository.clone();
    for _ in 0..MAX_DEPENDENCY_DEPTH {
        let Some(parent) = analysis.pull_requests.values().find(|pull_request| {
            pull_request.state == PullRequestState::Open
                && !pull_request.cross_repository
                && pull_request.head.repository == repository
                && pull_request.head.name == branch
                && pull_request.number != candidate.number
        }) else {
            break;
        };
        if !dependencies.insert(parent.number) {
            break;
        }
        branch = parent.base.name.clone();
    }
    dependencies.into_iter().collect()
}

/// Evaluate intent-aware admission ordering for one candidate.
///
/// `target` is the already resolved join target: an unresolved or ambiguous
/// target never reaches this function, so ordering can never be relaxed by a
/// guess about which caravan an operator meant. `selection` records who chose
/// the candidate: [`AdmissionSelection::Automatic`] is FIFO-bound for either
/// intent, [`AdmissionSelection::Explicit`] may pass earlier unrelated unjoined
/// rows for either intent, and [`AdmissionSelection::CheckedOut`] reports
/// canonical position as evidence without ever gating on it.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn evaluate(
    admission: &AdmissionStatus,
    analysis: &GraphAnalysis,
    candidate: &PullRequestSnapshot,
    target: Option<&Caravan>,
    selection: AdmissionSelection,
) -> AdmissionIntentDecision {
    let intent = target.map_or(AdmissionIntent::New, |_| AdmissionIntent::Join);
    let unranked = admission.priority_labels.len() + 1;
    let candidate_row = admission
        .candidates
        .iter()
        .find(|row| row.pr == candidate.number);
    let dependencies = dependency_prs(analysis, candidate);
    let enrolled = analysis.fleet.containing(candidate.number).is_some();

    let mut decision = AdmissionIntentDecision {
        schema_version: 1,
        intent,
        selection,
        outcome: AdmissionOrderOutcome::BlockedByOrder,
        candidate_pr: candidate.number,
        canonical_candidate_pr: admission.next_candidate,
        target_caravan: target.map(|caravan| caravan.id),
        target_tail: target.and_then(Caravan::tail),
        ordered_rows_ahead: Vec::new(),
        bypassed_unjoined_prs: Vec::new(),
        blocking_prs: Vec::new(),
        dependency_prs: dependencies.clone(),
        compatibility_clean: false,
        preflight_clean: false,
        provider_mutated: false,
        idempotent: false,
        reason: String::new(),
        policy: ADMISSION_INTENT_POLICY.to_owned(),
    };

    if enrolled {
        decision.outcome = AdmissionOrderOutcome::AlreadyEnrolled;
        "candidate is already an active caravan member; admission ordering does not apply"
            .clone_into(&mut decision.reason);
        return decision;
    }

    if admission.next_candidate == Some(candidate.number) {
        decision.outcome = AdmissionOrderOutcome::Canonical;
        decision.reason = format!(
            "candidate is the canonical first ordered {} admission attempt",
            intent.name()
        );
        return decision;
    }

    let Some(candidate_row) = candidate_row else {
        if selection == AdmissionSelection::CheckedOut {
            decision.outcome = AdmissionOrderOutcome::OwnerSelected;
            "owner is operating on this exact checked-out PR; canonical order is evidence only"
                .clone_into(&mut decision.reason);
            return decision;
        }
        decision.reason = admission
            .rejected
            .iter()
            .find(|row| row.pr == candidate.number)
            .map_or_else(
                || {
                    "candidate is not a current ordered admission attempt; explicit intent cannot relax canonical order".to_owned()
                },
                |row| {
                    format!(
                        "candidate is not a current ordered admission attempt: {}",
                        row.reason
                    )
                },
            );
        return decision;
    };

    let candidate_key = key(
        candidate_row.priority_rank,
        candidate_row.created_at.as_ref(),
        candidate_row.pr,
        unranked,
    );
    let dependency_set: BTreeSet<PrNumber> = dependencies.into_iter().collect();
    let mut rows: Vec<OrderedRow> = Vec::new();
    for row in &admission.candidates {
        if row.pr == candidate.number {
            continue;
        }
        if key(row.priority_rank, row.created_at.as_ref(), row.pr, unranked) >= candidate_key {
            continue;
        }
        let joined = analysis.fleet.containing(row.pr).is_some();
        let disposition = if joined {
            OrderedRowDisposition::BlockedJoined
        } else if dependency_set.contains(&row.pr) {
            OrderedRowDisposition::BlockedDependency
        } else if selection.relaxes_order() {
            OrderedRowDisposition::BypassedUnjoined
        } else {
            OrderedRowDisposition::BlockedAutomaticOrder
        };
        rows.push(OrderedRow {
            pr: row.pr,
            disposition,
            joined,
            reason: row.reason.clone(),
        });
    }
    for row in &admission.rejected {
        if !row.blocks_order || row.pr == candidate.number {
            continue;
        }
        if key(row.priority_rank, row.created_at.as_ref(), row.pr, unranked) >= candidate_key {
            continue;
        }
        let joined = analysis.fleet.containing(row.pr).is_some();
        rows.push(OrderedRow {
            pr: row.pr,
            disposition: if joined {
                OrderedRowDisposition::BlockedJoined
            } else if dependency_set.contains(&row.pr) {
                OrderedRowDisposition::BlockedDependency
            } else {
                OrderedRowDisposition::BlockedRankIndeterminate
            },
            joined,
            reason: row.reason.clone(),
        });
    }
    rows.sort_by_key(|row| row.pr);

    decision.bypassed_unjoined_prs = rows
        .iter()
        .filter(|row| row.disposition.bypassed())
        .map(|row| row.pr)
        .collect();
    decision.blocking_prs = rows
        .iter()
        .filter(|row| !row.disposition.bypassed())
        .map(|row| row.pr)
        .collect();
    decision.ordered_rows_ahead = rows;

    if !selection.relaxes_order() {
        decision.reason = admission.next_candidate.map_or_else(
            || {
                "automatic admission is not selectable because no canonical attempt exists"
                    .to_owned()
            },
            |canonical| {
                format!(
                    "priority/FIFO binds automatic selection for {} intent; fail closed on canonical PR #{canonical}",
                    intent.name()
                )
            },
        );
        return decision;
    }

    let target_text = decision.target_caravan.map_or_else(
        || "forming a new caravan".to_owned(),
        |id| format!("to caravan #{id}"),
    );
    if decision.blocking_prs.is_empty() {
        decision.outcome = AdmissionOrderOutcome::ExplicitAheadOfUnjoined;
        decision.reason = if decision.bypassed_unjoined_prs.is_empty() {
            format!(
                "explicit {} intent {target_text}; no earlier ordered row exists",
                intent.name()
            )
        } else {
            format!(
                "explicit {} intent {target_text}; earlier row(s) {} bypassed only because they are unrelated unjoined first-admission attempts and keep their canonical order",
                intent.name(),
                decision
                    .bypassed_unjoined_prs
                    .iter()
                    .map(|pr| format!("#{pr}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        return decision;
    }

    let blocking = decision
        .ordered_rows_ahead
        .iter()
        .filter(|row| !row.disposition.bypassed())
        .map(|row| format!("#{} ({:?})", row.pr, row.disposition))
        .collect::<Vec<_>>()
        .join(", ");
    if selection.is_checked_out() {
        // The owner is already on this exact PR; local `check` and every local
        // membership operation have never been gated by canonical order. The
        // rows are still reported so the receipt states exactly what a remote
        // explicit request would have failed closed on.
        decision.outcome = AdmissionOrderOutcome::OwnerSelected;
        decision.reason = format!(
            "owner is operating on this exact checked-out PR; canonical order is evidence only; a remote explicit {} request would fail closed on earlier row(s) {blocking}",
            intent.name()
        );
        return decision;
    }

    decision.reason = format!(
        "explicit {} intent {target_text} fails closed: earlier row(s) {blocking} are not unrelated unjoined attempts",
        intent.name()
    );
    decision
}

#[cfg(test)]
mod tests;
