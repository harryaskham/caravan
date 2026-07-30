//! Deterministic intent-aware admission-order fixtures.
//!
//! Every case is a pure snapshot: no provider, clock, or filesystem input.

use std::collections::BTreeSet;

use super::*;
use crate::model::{
    AutoMergeState, BranchSnapshot, CommitOid, CompatibilityOutcome, CompatibilityReport,
    PullRequestState, RepositoryId,
};
use crate::read::{AdmissionStatus, resolve_admission};

fn repository() -> RepositoryId {
    RepositoryId {
        owner: "harryaskham".to_owned(),
        name: "cacophony".to_owned(),
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

#[allow(clippy::unnecessary_wraps)]
fn clean(
    candidate: &BranchSnapshot,
    target: &BranchSnapshot,
) -> Result<CompatibilityReport, crate::AppError> {
    Ok(CompatibilityReport {
        candidate: candidate.clone(),
        target: target.clone(),
        outcome: CompatibilityOutcome::Clean,
        conflicting_paths: Vec::new(),
        diagnostic: None,
    })
}

fn analysis(pull_requests: Vec<PullRequestSnapshot>) -> GraphAnalysis {
    let snapshot = crate::model::RepositorySnapshot {
        merge_candidates: Vec::new(),
        merge_candidates_truncated: 0,
        previous_default_oid: None,
        default_branch_movements: Vec::new(),
        repository: repository(),
        default_branch: branch("main"),
        current_branch: Some("current".to_owned()),
        current_pr: None,
        pull_requests,
        generation_facts: Vec::new(),
        observed_at: None,
    };
    crate::graph::analyze(&snapshot, &clean).expect("fixture analysis")
}

fn admission(analysis: &GraphAnalysis) -> AdmissionStatus {
    resolve_admission(
        analysis,
        &crate::config::CaravanConfig::default().agent_priority_labels,
    )
}

/// Explicit owner `join` passes an older unrelated unjoined FIFO row while that
/// row keeps its canonical first-admission position.
#[test]
fn explicit_join_passes_only_unrelated_unjoined_fifo_rows() {
    let analysis = analysis(vec![
        pr(1, "root", "main", true),
        pr(2113, "old-unjoined", "main", false),
        pr(2179, "green", "main", false),
    ]);
    let admission = admission(&analysis);
    let candidate = analysis.pull_requests[&PrNumber(2179)].clone();
    let target = analysis.fleet.caravans[0].clone();

    assert_eq!(
        admission.next_candidate,
        Some(PrNumber(2113)),
        "FIFO still names the oldest unjoined row"
    );

    let decision = evaluate(
        &admission,
        &analysis,
        &candidate,
        Some(&target),
        AdmissionSelection::Explicit,
    );

    assert_eq!(decision.intent, AdmissionIntent::Join);
    assert_eq!(decision.selection, AdmissionSelection::Explicit);
    assert_eq!(
        decision.outcome,
        AdmissionOrderOutcome::ExplicitAheadOfUnjoined
    );
    assert!(decision.bypasses_fifo());
    assert_eq!(decision.target_caravan, Some(PrNumber(1)));
    assert_eq!(decision.target_tail, Some(PrNumber(1)));
    assert_eq!(decision.canonical_candidate_pr, Some(PrNumber(2113)));
    assert_eq!(decision.bypassed_unjoined_prs, vec![PrNumber(2113)]);
    assert!(decision.blocking_prs.is_empty());
    assert_eq!(
        decision
            .ordered_rows_ahead
            .iter()
            .map(|row| (row.pr, row.disposition, row.joined))
            .collect::<Vec<_>>(),
        vec![(
            PrNumber(2113),
            OrderedRowDisposition::BypassedUnjoined,
            false
        )],
        "only unjoined rows may be bypassed and each is named"
    );
    assert!(decision.reason.contains("unrelated unjoined"));
    assert_eq!(decision.policy, ADMISSION_INTENT_POLICY);
}

/// Restored reviewed semantics (bd-7099e8): explicit owner `new` intent is the
/// same deliberate admission intent as explicit `join`. Cara 0.0.10 recognized
/// the intent but applied FIFO anyway; the two must agree.
#[test]
fn explicit_new_intent_also_passes_unrelated_unjoined_fifo_rows() {
    let analysis = analysis(vec![
        pr(2113, "old-unjoined", "main", false),
        pr(2213, "generation4", "main", false),
    ]);
    let admission = admission(&analysis);
    let candidate = analysis.pull_requests[&PrNumber(2213)].clone();

    let decision = evaluate(
        &admission,
        &analysis,
        &candidate,
        None,
        AdmissionSelection::Explicit,
    );

    assert_eq!(decision.intent, AdmissionIntent::New);
    assert_eq!(decision.selection, AdmissionSelection::Explicit);
    assert_eq!(
        decision.outcome,
        AdmissionOrderOutcome::ExplicitAheadOfUnjoined
    );
    assert!(decision.bypasses_fifo());
    assert!(decision.target_caravan.is_none());
    assert_eq!(decision.bypassed_unjoined_prs, vec![PrNumber(2113)]);
    assert!(decision.blocking_prs.is_empty());
    assert_eq!(
        decision.ordered_rows_ahead[0].disposition,
        OrderedRowDisposition::BypassedUnjoined
    );
    assert!(decision.reason.contains("forming a new caravan"));
    assert_eq!(
        admission.next_candidate,
        Some(PrNumber(2113)),
        "the bypassed row keeps its canonical first-admission position"
    );
}

/// Automatic priority/FIFO selection is a separate axis and is bound by order
/// for `new` and `join` intent alike.
#[test]
fn automatic_selection_never_bypasses_an_earlier_row_for_either_intent() {
    let analysis = analysis(vec![
        pr(1, "root", "main", true),
        pr(2113, "old-unjoined", "main", false),
        pr(2179, "green", "main", false),
    ]);
    let admission = admission(&analysis);
    let candidate = analysis.pull_requests[&PrNumber(2179)].clone();
    let target = analysis.fleet.caravans[0].clone();

    for intent_target in [None, Some(&target)] {
        let decision = evaluate(
            &admission,
            &analysis,
            &candidate,
            intent_target,
            AdmissionSelection::Automatic,
        );

        assert_eq!(decision.selection, AdmissionSelection::Automatic);
        assert_eq!(decision.outcome, AdmissionOrderOutcome::BlockedByOrder);
        assert!(!decision.bypasses_fifo());
        assert!(!decision.order_permits_admission());
        assert!(decision.bypassed_unjoined_prs.is_empty());
        assert_eq!(decision.blocking_prs, vec![PrNumber(2113)]);
        assert_eq!(
            decision.ordered_rows_ahead[0].disposition,
            OrderedRowDisposition::BlockedAutomaticOrder
        );
        assert!(decision.reason.contains("binds automatic selection"));
    }
}

/// The canonical row itself is canonical for either intent and either
/// selection.
#[test]
fn canonical_candidate_is_reported_without_bypass() {
    let analysis = analysis(vec![
        pr(1, "root", "main", true),
        pr(2113, "old-unjoined", "main", false),
    ]);
    let admission = admission(&analysis);
    let candidate = analysis.pull_requests[&PrNumber(2113)].clone();
    let target = analysis.fleet.caravans[0].clone();

    for selection in [
        AdmissionSelection::Automatic,
        AdmissionSelection::Explicit,
        AdmissionSelection::CheckedOut,
    ] {
        let decision = evaluate(&admission, &analysis, &candidate, Some(&target), selection);

        assert_eq!(decision.outcome, AdmissionOrderOutcome::Canonical);
        assert!(!decision.bypasses_fifo());
        assert!(decision.order_permits_admission());
        assert!(decision.ordered_rows_ahead.is_empty());
    }
}

/// A joined ancestor/dependency is never skipped, for `new` or `join` intent.
#[test]
fn joined_and_dependency_rows_are_never_bypassed() {
    // #2100 is the candidate's exact base-chain parent and still unjoined;
    // #2050 is an unrelated unjoined row that may be bypassed.
    let analysis = analysis(vec![
        pr(1, "root", "main", true),
        pr(2050, "unrelated", "main", false),
        pr(2100, "parent", "main", false),
        pr(2179, "child", "parent", false),
    ]);
    let admission = admission(&analysis);
    let candidate = analysis.pull_requests[&PrNumber(2179)].clone();
    let target = analysis.fleet.caravans[0].clone();

    for intent_target in [None, Some(&target)] {
        let decision = evaluate(
            &admission,
            &analysis,
            &candidate,
            intent_target,
            AdmissionSelection::Explicit,
        );

        assert_eq!(decision.dependency_prs, vec![PrNumber(2100)]);
        assert_eq!(decision.outcome, AdmissionOrderOutcome::BlockedByOrder);
        assert!(!decision.bypasses_fifo());
        assert_eq!(decision.blocking_prs, vec![PrNumber(2100)]);
        assert_eq!(decision.bypassed_unjoined_prs, vec![PrNumber(2050)]);
        assert_eq!(
            decision
                .ordered_rows_ahead
                .iter()
                .find(|row| row.pr == PrNumber(2100))
                .expect("dependency row is reported")
                .disposition,
            OrderedRowDisposition::BlockedDependency
        );
    }
}

/// A rank-indeterminate row blocks every later attempt, for either intent.
#[test]
fn rank_indeterminate_rows_block_explicit_intent() {
    let mut blocked = pr(2113, "old-unjoined", "main", false);
    blocked.labels.insert("caravan-priority:unknown".to_owned());
    let analysis = analysis(vec![
        pr(1, "root", "main", true),
        blocked,
        pr(2179, "green", "main", false),
    ]);
    let admission = admission(&analysis);
    let candidate = analysis.pull_requests[&PrNumber(2179)].clone();
    let target = analysis.fleet.caravans[0].clone();

    for intent_target in [None, Some(&target)] {
        let decision = evaluate(
            &admission,
            &analysis,
            &candidate,
            intent_target,
            AdmissionSelection::Explicit,
        );

        assert_eq!(decision.outcome, AdmissionOrderOutcome::BlockedByOrder);
        assert_eq!(decision.blocking_prs, vec![PrNumber(2113)]);
        assert_eq!(
            decision.ordered_rows_ahead[0].disposition,
            OrderedRowDisposition::BlockedRankIndeterminate
        );
    }
}

/// A candidate that is not an ordered admission attempt gains nothing from
/// declaring intent, for `new` or `join`.
#[test]
fn stale_pinned_or_rejected_candidate_cannot_use_explicit_intent() {
    let mut skipped = pr(2179, "green", "main", false);
    skipped.labels.insert("caravan-join-skipped".to_owned());
    let analysis = analysis(vec![
        pr(1, "root", "main", true),
        pr(2113, "old-unjoined", "main", false),
        skipped,
    ]);
    let admission = admission(&analysis);
    let candidate = analysis.pull_requests[&PrNumber(2179)].clone();
    let target = analysis.fleet.caravans[0].clone();

    for intent_target in [None, Some(&target)] {
        let decision = evaluate(
            &admission,
            &analysis,
            &candidate,
            intent_target,
            AdmissionSelection::Explicit,
        );

        assert_eq!(decision.outcome, AdmissionOrderOutcome::BlockedByOrder);
        assert!(!decision.bypasses_fifo());
        assert!(
            decision
                .reason
                .contains("not a current ordered admission attempt")
        );
    }
}

/// The owner's own checked-out PR reports canonical position as evidence only:
/// local `check`, renew, and rejoin were never gated by admission order.
#[test]
fn checked_out_owner_selection_reports_order_as_evidence_only() {
    let stacked = analysis(vec![
        pr(2050, "unrelated", "main", false),
        pr(2100, "parent", "main", false),
        pr(2179, "child", "parent", false),
    ]);
    let stacked_admission = admission(&stacked);
    let candidate = stacked.pull_requests[&PrNumber(2179)].clone();

    let decision = evaluate(
        &stacked_admission,
        &stacked,
        &candidate,
        None,
        AdmissionSelection::CheckedOut,
    );

    assert_eq!(decision.selection, AdmissionSelection::CheckedOut);
    assert_eq!(decision.outcome, AdmissionOrderOutcome::OwnerSelected);
    assert!(decision.order_permits_admission());
    assert!(!decision.bypasses_fifo());
    assert!(decision.reason.contains("evidence only"));
    assert_eq!(
        decision
            .ordered_rows_ahead
            .iter()
            .map(|row| row.pr)
            .collect::<Vec<_>>(),
        vec![PrNumber(2050), PrNumber(2100)],
        "provenance still names every ordered row ahead"
    );

    // A candidate that is not an ordered attempt at all (evicted, skipped) is
    // still an owner-selected checked-out operation, so renew/rejoin work.
    let mut evicted_pr = pr(2179, "green", "main", false);
    evicted_pr.labels.insert("caravan-evicted".to_owned());
    let evicted = analysis(vec![evicted_pr, pr(2113, "old-unjoined", "main", false)]);
    let evicted_admission = admission(&evicted);
    let evicted_candidate = evicted.pull_requests[&PrNumber(2179)].clone();
    let evicted_decision = evaluate(
        &evicted_admission,
        &evicted,
        &evicted_candidate,
        None,
        AdmissionSelection::CheckedOut,
    );
    assert_eq!(
        evicted_decision.outcome,
        AdmissionOrderOutcome::OwnerSelected
    );
    assert!(evicted_decision.order_permits_admission());
}

/// Failed preflight downgrades an otherwise permitted ordering decision.
#[test]
fn failed_preflight_downgrades_a_permitted_join() {
    let analysis = analysis(vec![
        pr(1, "root", "main", true),
        pr(2113, "old-unjoined", "main", false),
        pr(2179, "green", "main", false),
    ]);
    let admission = admission(&analysis);
    let candidate = analysis.pull_requests[&PrNumber(2179)].clone();
    let target = analysis.fleet.caravans[0].clone();

    let mut decision = evaluate(
        &admission,
        &analysis,
        &candidate,
        Some(&target),
        AdmissionSelection::Explicit,
    );
    assert!(decision.bypasses_fifo());
    decision.record_preflight(false, false);

    assert_eq!(decision.outcome, AdmissionOrderOutcome::BlockedByPreflight);
    assert!(!decision.compatibility_clean);
    assert!(!decision.preflight_clean);
    assert!(!decision.order_permits_admission());
    assert!(decision.reason.contains("exact preflight rejected"));
}

/// The same downgrade applies to a permitted explicit `new`.
#[test]
fn failed_preflight_downgrades_a_permitted_new() {
    let analysis = analysis(vec![
        pr(2113, "old-unjoined", "main", false),
        pr(2179, "green", "main", false),
    ]);
    let admission = admission(&analysis);
    let candidate = analysis.pull_requests[&PrNumber(2179)].clone();

    let mut decision = evaluate(
        &admission,
        &analysis,
        &candidate,
        None,
        AdmissionSelection::Explicit,
    );
    assert!(decision.bypasses_fifo());
    decision.record_preflight(false, false);

    assert_eq!(decision.outcome, AdmissionOrderOutcome::BlockedByPreflight);
    assert!(!decision.order_permits_admission());
    assert!(decision.reason.contains("explicit new intent"));
}

/// Provider mutation and idempotency are exact, not inferred.
#[test]
fn execution_evidence_records_mutation_and_idempotent_replay() {
    let analysis = analysis(vec![
        pr(1, "root", "main", true),
        pr(2179, "green", "main", false),
    ]);
    let admission = admission(&analysis);
    let candidate = analysis.pull_requests[&PrNumber(2179)].clone();
    let target = analysis.fleet.caravans[0].clone();

    let mut mutated = evaluate(
        &admission,
        &analysis,
        &candidate,
        Some(&target),
        AdmissionSelection::Explicit,
    );
    mutated.record_preflight(true, true);
    mutated.record_execution(true);
    let mut replayed = evaluate(
        &admission,
        &analysis,
        &candidate,
        Some(&target),
        AdmissionSelection::Explicit,
    );
    replayed.record_preflight(true, true);
    replayed.record_execution(false);

    assert!(mutated.provider_mutated && !mutated.idempotent);
    assert!(!replayed.provider_mutated && replayed.idempotent);
    assert_eq!(mutated.outcome, replayed.outcome);
}

/// An already enrolled candidate is reported without ordering claims.
#[test]
fn enrolled_candidate_reports_already_enrolled() {
    let analysis = analysis(vec![
        pr(1, "root", "main", true),
        pr(2, "member", "root", true),
    ]);
    let admission = admission(&analysis);
    let candidate = analysis.pull_requests[&PrNumber(2)].clone();

    let decision = evaluate(
        &admission,
        &analysis,
        &candidate,
        None,
        AdmissionSelection::Explicit,
    );

    assert_eq!(decision.outcome, AdmissionOrderOutcome::AlreadyEnrolled);
    assert!(decision.order_permits_admission());
    assert!(!decision.bypasses_fifo());
}

/// Deterministic base-chain dependency derivation terminates on cycles.
#[test]
fn dependency_walk_is_bounded_and_deterministic() {
    let analysis = analysis(vec![
        pr(10, "a", "b", false),
        pr(11, "b", "c", false),
        pr(12, "c", "main", false),
    ]);
    let candidate = analysis.pull_requests[&PrNumber(10)].clone();

    assert_eq!(
        dependency_prs(&analysis, &candidate),
        vec![PrNumber(11), PrNumber(12)]
    );
}

/// The typed decision serializes with both axes so Cacophony can A/B explicit
/// owner intent against automatic FIFO selection from JSON alone.
#[test]
fn decision_json_names_selection_intent_and_dispositions() {
    let analysis = analysis(vec![
        pr(2113, "old-unjoined", "main", false),
        pr(2213, "generation4", "main", false),
    ]);
    let admission = admission(&analysis);
    let candidate = analysis.pull_requests[&PrNumber(2213)].clone();

    let explicit = serde_json::to_value(evaluate(
        &admission,
        &analysis,
        &candidate,
        None,
        AdmissionSelection::Explicit,
    ))
    .expect("decision serializes");
    let automatic = serde_json::to_value(evaluate(
        &admission,
        &analysis,
        &candidate,
        None,
        AdmissionSelection::Automatic,
    ))
    .expect("decision serializes");

    assert_eq!(explicit["selection"], "explicit");
    assert_eq!(explicit["intent"], "new");
    assert_eq!(explicit["outcome"], "explicit_ahead_of_unjoined");
    assert_eq!(explicit["ordered_rows_ahead"][0]["pr"], 2113);
    assert_eq!(
        explicit["ordered_rows_ahead"][0]["disposition"],
        "bypassed_unjoined"
    );

    assert_eq!(automatic["selection"], "automatic");
    assert_eq!(automatic["intent"], "new");
    assert_eq!(automatic["outcome"], "blocked_by_order");
    assert_eq!(
        automatic["ordered_rows_ahead"][0]["disposition"],
        "blocked_automatic_order"
    );
}
