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

/// Older unjoined FIFO row plus a valid newer explicit join.
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

    let decision = evaluate(&admission, &analysis, &candidate, Some(&target));

    assert_eq!(decision.intent, AdmissionIntent::Join);
    assert_eq!(decision.outcome, AdmissionOrderOutcome::JoinAheadOfUnjoined);
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

/// FIFO still governs first admission and new-caravan intent.
#[test]
fn new_intent_never_bypasses_an_older_fifo_row() {
    let analysis = analysis(vec![
        pr(2113, "old-unjoined", "main", false),
        pr(2179, "green", "main", false),
    ]);
    let admission = admission(&analysis);
    let candidate = analysis.pull_requests[&PrNumber(2179)].clone();

    let decision = evaluate(&admission, &analysis, &candidate, None);

    assert_eq!(decision.intent, AdmissionIntent::New);
    assert_eq!(decision.outcome, AdmissionOrderOutcome::BlockedByOrder);
    assert!(!decision.bypasses_fifo());
    assert!(decision.bypassed_unjoined_prs.is_empty());
    assert_eq!(decision.blocking_prs, vec![PrNumber(2113)]);
    assert_eq!(
        decision.ordered_rows_ahead[0].disposition,
        OrderedRowDisposition::BlockedNewIntent
    );
    assert!(decision.reason.contains("FIFO governs first admission"));
}

/// The canonical row itself is canonical for either intent.
#[test]
fn canonical_candidate_is_reported_without_bypass() {
    let analysis = analysis(vec![
        pr(1, "root", "main", true),
        pr(2113, "old-unjoined", "main", false),
    ]);
    let admission = admission(&analysis);
    let candidate = analysis.pull_requests[&PrNumber(2113)].clone();
    let target = analysis.fleet.caravans[0].clone();

    let decision = evaluate(&admission, &analysis, &candidate, Some(&target));

    assert_eq!(decision.outcome, AdmissionOrderOutcome::Canonical);
    assert!(!decision.bypasses_fifo());
    assert!(decision.ordered_rows_ahead.is_empty());
}

/// A joined ancestor/dependency is never skipped.
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

    let decision = evaluate(&admission, &analysis, &candidate, Some(&target));

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

/// A rank-indeterminate row blocks every later attempt, including explicit join.
#[test]
fn rank_indeterminate_rows_block_explicit_join() {
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

    let decision = evaluate(&admission, &analysis, &candidate, Some(&target));

    assert_eq!(decision.outcome, AdmissionOrderOutcome::BlockedByOrder);
    assert_eq!(decision.blocking_prs, vec![PrNumber(2113)]);
    assert_eq!(
        decision.ordered_rows_ahead[0].disposition,
        OrderedRowDisposition::BlockedRankIndeterminate
    );
}

/// A candidate that is not an ordered admission attempt gains nothing from intent.
#[test]
fn stale_pinned_or_rejected_candidate_cannot_use_join_intent() {
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

    let decision = evaluate(&admission, &analysis, &candidate, Some(&target));

    assert_eq!(decision.outcome, AdmissionOrderOutcome::BlockedByOrder);
    assert!(!decision.bypasses_fifo());
    assert!(
        decision
            .reason
            .contains("not a current ordered admission attempt")
    );
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

    let mut decision = evaluate(&admission, &analysis, &candidate, Some(&target));
    assert!(decision.bypasses_fifo());
    decision.record_preflight(false, false);

    assert_eq!(decision.outcome, AdmissionOrderOutcome::BlockedByPreflight);
    assert!(!decision.compatibility_clean);
    assert!(!decision.preflight_clean);
    assert!(!decision.order_permits_admission());
    assert!(decision.reason.contains("exact preflight rejected"));
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

    let mut mutated = evaluate(&admission, &analysis, &candidate, Some(&target));
    mutated.record_preflight(true, true);
    mutated.record_execution(true);
    let mut replayed = evaluate(&admission, &analysis, &candidate, Some(&target));
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

    let decision = evaluate(&admission, &analysis, &candidate, None);

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
