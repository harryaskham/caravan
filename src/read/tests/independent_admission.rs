//! bd-55886b: review the scope without enabling an independent-new bypass.
use super::*;

fn conflicting_fleet() -> StatusOutput {
    let candidate = pr(9, "nine", "main", false);
    let mut observed = status(
        candidate.clone(),
        vec![
            pr(1, "one", "main", true),
            pr(2, "two", "one", true),
            candidate,
        ],
    );
    observed.analysis.fleet.problems.push(GraphProblem {
        kind: GraphProblemKind::Incompatible,
        prs: vec![PrNumber(1), PrNumber(2)],
        message: "existing child conflicts with its parent".to_owned(),
    });
    observed.healthy = false;
    observed
}

#[test]
fn independent_admission_clean_new_still_blocks_but_names_existing_owner_scope() {
    let observed = conflicting_fleet();
    let input = CheckInput {
        pr: Some(9),
        ..CheckInput::default()
    };
    let requested = check_requested_action_analysis(&observed, &input, &clean_checker).unwrap();
    let recommendation = check_analysis(&observed, &input, &clean_checker).unwrap();
    for receipt in [&requested, &recommendation] {
        assert!(!receipt.eligible);
        assert_eq!(receipt.mode, CheckMode::NewCaravan);
        assert!(
            receipt
                .compatibility
                .iter()
                .all(|report| report.outcome == CompatibilityOutcome::Clean)
        );
        assert_eq!(receipt.problems[0].prs, [PrNumber(1), PrNumber(2)]);
        let note = receipt.admission_note.as_ref().unwrap();
        assert!(note.contains("blocked_by_existing_fleet"));
        assert!(note.contains("does not identify a source defect in the candidate"));
        assert!(note.contains("no independent-new bypass"));
    }
    let local = check_requested_action_analysis(&observed, &CheckInput::default(), &clean_checker)
        .unwrap_err();
    assert_eq!(local.code(), "check_failed");
    assert_eq!(local.details().unwrap()["eligible"], false);
}

#[test]
fn independent_admission_explicit_join_keeps_selected_tail_and_refusal() {
    let observed = conflicting_fleet();
    let result = check_requested_action_analysis(
        &observed,
        &CheckInput {
            expected_admission: None,
            pr: Some(9),
            tail_pr: Some(2),
            head_pr: None,
        },
        &clean_checker,
    )
    .unwrap();
    assert_eq!(result.mode, CheckMode::JoinTail);
    assert_eq!(result.target_pr, Some(PrNumber(2)));
    assert!(!result.eligible);
    assert!(
        result
            .problems
            .iter()
            .any(|problem| problem.prs == [PrNumber(1), PrNumber(2)])
    );
}

#[test]
fn independent_admission_candidate_and_cross_caravan_conflicts_stay_distinct() {
    for (candidate_name, target_name) in [("nine", "main"), ("nine", "two"), ("one", "nine")] {
        let observed = conflicting_fleet();
        let checker = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
            let mut report = clean_checker(candidate, target)?;
            if candidate.name == candidate_name && target.name == target_name {
                report.outcome = CompatibilityOutcome::Conflict;
                report.conflicting_paths.push("shared.rs".to_owned());
            }
            Ok(report)
        };
        let result = check_requested_action_analysis(
            &observed,
            &CheckInput {
                pr: Some(9),
                ..CheckInput::default()
            },
            &checker,
        )
        .unwrap();
        assert!(!result.eligible);
        assert!(
            result
                .problems
                .iter()
                .any(|problem| problem.prs.contains(&PrNumber(9)))
        );
        assert!(
            result
                .admission_note
                .unwrap()
                .contains("candidate or global problems also remain")
        );
    }
}

#[test]
fn independent_admission_malformed_unknown_and_dependency_evidence_never_bypassed() {
    for kind in [
        GraphProblemKind::Cycle,
        GraphProblemKind::DanglingBase,
        GraphProblemKind::DuplicateMember,
        GraphProblemKind::InvalidGenerationMetadata,
        GraphProblemKind::Unknown,
    ] {
        let mut observed = conflicting_fleet();
        observed.analysis.fleet.problems.push(GraphProblem {
            kind,
            prs: Vec::new(),
            message: "global malformed or unknown generation".to_owned(),
        });
        let result = check_requested_action_analysis(
            &observed,
            &CheckInput {
                pr: Some(9),
                ..CheckInput::default()
            },
            &clean_checker,
        )
        .unwrap();
        assert!(!result.eligible);
        assert!(result.problems.iter().any(|problem| problem.kind == kind));
        assert!(
            result
                .admission_note
                .unwrap()
                .contains("candidate or global problems also remain")
        );
    }
    let candidate = pr(9, "nine", "two", false);
    let observed = status(
        candidate.clone(),
        vec![
            pr(1, "one", "main", false),
            pr(2, "two", "main", false),
            candidate,
        ],
    );
    let result = check_requested_action_analysis(
        &observed,
        &CheckInput {
            pr: Some(9),
            ..CheckInput::default()
        },
        &clean_checker,
    )
    .unwrap();
    assert!(
        !result.eligible,
        "an unresolved earlier unjoined base dependency still blocks ordering"
    );
    assert_eq!(result.admission_intent.unwrap().blocking_prs, [PrNumber(2)]);
}

#[test]
fn independent_admission_clean_compatibility_is_not_dependency_independence() {
    let candidate = pr(9, "nine", "two", false);
    let observed = status(
        candidate.clone(),
        vec![
            pr(1, "one", "main", true),
            pr(2, "two", "one", true),
            candidate,
        ],
    );
    let result = check_requested_action_analysis(
        &observed,
        &CheckInput {
            pr: Some(9),
            ..CheckInput::default()
        },
        &clean_checker,
    )
    .unwrap();
    // Existing ordinary explicit-new can be compatible. A future isolation
    // policy MUST check dependencies, not reinterpret that as independence.
    assert!(result.eligible);
    assert!(!result.admission_intent.unwrap().dependency_prs.is_empty());
}

#[test]
fn independent_admission_capacity_refusal_preserves_existing_fleet() {
    let observed = conflicting_fleet();
    let mut context = AppContext::default();
    context.config.sync.max_caravans = 1;
    let refusal =
        crate::sync::caravan_fleet_capacity_refusal(&context, &observed, PrNumber(9)).unwrap();
    assert_eq!(refusal.active_caravan_ids, [PrNumber(1)]);
    assert_eq!(refusal.code, "max_caravans_reached");
    context.config.sync.max_caravans = 2;
    assert!(
        crate::sync::caravan_fleet_capacity_refusal(&context, &observed, PrNumber(9)).is_none()
    );
    assert!(
        !check_requested_action_analysis(
            &observed,
            &CheckInput {
                pr: Some(9),
                ..CheckInput::default()
            },
            &clean_checker
        )
        .unwrap()
        .eligible,
        "spare capacity is not an incompatibility waiver"
    );
}

#[test]
fn independent_admission_scope_note_does_not_misclassify_unknown_or_candidate_rows() {
    let observed = conflicting_fleet();
    for prs in [vec![], vec![PrNumber(9)], vec![PrNumber(999)]] {
        let problem = GraphProblem {
            kind: GraphProblemKind::Incompatible,
            prs,
            message: "unproved scope".to_owned(),
        };
        assert_eq!(
            admission_blocker_note(
                &observed,
                PrNumber(9),
                &[problem],
                Some("ordering evidence")
            ),
            Some("ordering evidence".to_owned())
        );
    }
}
