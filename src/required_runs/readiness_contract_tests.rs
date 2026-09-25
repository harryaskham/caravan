//! bd-5d78d1: duplicate context/App/head does not grant cross-workflow replacement.
//! The producer pair is synthetic; tests prove consumer semantics, not live admission.
use super::*;
use crate::model::{CommitOid, latest_checks_per_identity};

struct Fixture {
    pr: PrNumber,
    head: BranchSnapshot,
    base: BranchSnapshot,
    checks: Vec<CheckSnapshot>,
    policy: RequiredContextsRead,
}

fn fixture() -> Fixture {
    let data: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/readiness-required-selection.json"
    ))
    .unwrap();
    assert_eq!(
        data["provenance"]["kind"],
        "synthetic_source_traced_fixture_not_provider_receipt"
    );
    assert_eq!(data["provenance"]["provider_supersession_proven"], false);
    let repository = RepositoryId {
        owner: "fixture".to_owned(),
        name: "repo".to_owned(),
    };
    Fixture {
        pr: PrNumber(data["pr"].as_u64().unwrap()),
        head: BranchSnapshot {
            repository: repository.clone(),
            name: "source".to_owned(),
            oid: CommitOid(data["head"].as_str().unwrap().to_owned()),
        },
        base: BranchSnapshot {
            repository,
            name: "main".to_owned(),
            oid: CommitOid(data["base"].as_str().unwrap().to_owned()),
        },
        checks: serde_json::from_value(data["checks"].clone()).unwrap(),
        policy: RequiredContextsRead {
            branch: "main".to_owned(),
            protected: true,
            complete: true,
            contexts: Vec::new(),
            checks: vec![serde_json::from_value(data["required"].clone()).unwrap()],
        }
        .normalized(),
    }
}

fn evaluate(f: &Fixture, lineage: Option<&HeadRunLineage>) -> RequiredRunsAssessment {
    assess(&RequiredRunsInput {
        pr: f.pr,
        head: &f.head,
        base: &f.base,
        contexts: &f.policy,
        lineage,
        checks: &f.checks,
        head_published_at: Some("2026-09-24T09:00:00Z"),
        clock: RequiredRunsClock {
            now_unix: 1_800_000_000,
            grace_secs: 1,
        },
    })
}

#[test]
fn readiness_cross_workflow_success_does_not_suppress_failed_self_gate() {
    let f = fixture();
    let before = f.checks.clone();
    let (current, superseded) = latest_checks_per_identity(&f.checks);
    assert_eq!(current.len(), 2);
    assert!(superseded.is_empty());
    let result = evaluate(&f, None);
    assert_eq!(result.status, RequiredRunsStatus::Failing);
    assert_eq!(result.coverage[0].current_reporting_checks.len(), 2);
    assert_eq!(result.recovery, RequiredRunsRecovery::None);
    assert_eq!(
        f.checks, before,
        "historical red and source identity remain intact"
    );
}

#[test]
fn readiness_report_name_description_and_order_cannot_authorize_replacement() {
    for name in [
        "Caravan readiness membership refresh",
        "run_member",
        "trusted readiness",
    ] {
        let mut f = fixture();
        f.checks[1].workflow_name = Some(name.to_owned());
        f.checks[1].provider_state =
            Some("successful run_member replaces prior run_unproven".to_owned());
        for _ in 0..2 {
            assert_eq!(evaluate(&f, None).status, RequiredRunsStatus::Failing);
            f.checks.reverse();
        }
    }
}

#[test]
fn readiness_missing_or_wrong_app_head_and_required_policy_never_prove_green() {
    for case in 0..6 {
        let mut f = fixture();
        match case {
            0 => f.checks[1].app_id = Some(999),
            1 => f.checks[1].head_oid = Some(CommitOid("c".repeat(40))),
            2 => f.checks[1].app_id = None,
            3 => f.checks[1].head_oid = None,
            4 => f.checks[1].provider_kind = Some("WorkflowRunLineage".to_owned()),
            _ => f.policy.complete = false,
        }
        assert_ne!(
            evaluate(&f, None).status,
            RequiredRunsStatus::Satisfied,
            "case {case}"
        );
    }
    let mut f = fixture();
    f.checks[0].state = CheckState::Unknown;
    assert_eq!(
        evaluate(&f, None).status,
        RequiredRunsStatus::UnknownProviderState
    );
}

#[test]
fn readiness_single_workflow_reuses_only_actual_current_check_generation() {
    let mut f = fixture();
    let workflow = f.checks[1].workflow_name.clone();
    f.checks[0].workflow_name = workflow;
    let original = f.checks[0].clone();
    let (current, superseded) = latest_checks_per_identity(&f.checks);
    assert_eq!(current.len(), 1);
    assert_eq!(superseded, [&f.checks[0]]);
    assert_eq!(evaluate(&f, None).status, RequiredRunsStatus::Satisfied);
    assert_eq!(
        f.checks[0], original,
        "supersession retains immutable history"
    );
    // A newer observation is not automatically passing or source acceptance.
    f.checks[1].state = CheckState::InProgress;
    assert_eq!(evaluate(&f, None).status, RequiredRunsStatus::Pending);
    f.checks[1].state = CheckState::Failure;
    assert_eq!(evaluate(&f, None).status, RequiredRunsStatus::Failing);
}

#[test]
fn readiness_same_workflow_without_ordering_keeps_both_rows_current() {
    let mut f = fixture();
    let name = f.checks[0].workflow_name.clone();
    for check in &mut f.checks {
        check.workflow_name.clone_from(&name);
        check.details_url = None;
        check.started_at = None;
        check.completed_at = None;
    }
    assert_eq!(latest_checks_per_identity(&f.checks).0.len(), 2);
    assert_eq!(evaluate(&f, None).status, RequiredRunsStatus::Failing);
}

#[test]
fn readiness_pass_cannot_waive_required_source_or_third_party_verdicts() {
    let mut f = fixture();
    f.checks.remove(0);
    f.policy.checks.push(RequiredCheck {
        context: "source tests".to_owned(),
        app_id: Some(15368),
    });
    f.policy.checks.push(RequiredCheck {
        context: "security".to_owned(),
        app_id: Some(44),
    });
    let source = CheckSnapshot {
        name: "source tests".to_owned(),
        state: CheckState::Failure,
        workflow_name: Some("CI".to_owned()),
        ..f.checks[0].clone()
    };
    let security = CheckSnapshot {
        name: "security".to_owned(),
        app_id: Some(44),
        state: CheckState::Success,
        workflow_name: Some("independent security".to_owned()),
        ..source.clone()
    };
    f.checks.extend([source, security]);
    assert_eq!(evaluate(&f, None).status, RequiredRunsStatus::Failing);
    f.checks[1].state = CheckState::Success;
    f.checks[2].state = CheckState::Failure;
    assert_eq!(evaluate(&f, None).status, RequiredRunsStatus::Failing);
    f.checks[2].state = CheckState::Success;
    assert_eq!(evaluate(&f, None).status, RequiredRunsStatus::Satisfied);
    f.checks[2].state = CheckState::Unknown;
    assert_eq!(
        evaluate(&f, None).status,
        RequiredRunsStatus::UnknownProviderState
    );
}

#[test]
fn readiness_success_cannot_certify_changed_head_or_partial_lineage() {
    let mut f = fixture();
    f.checks.remove(0);
    f.head.oid = CommitOid("c".repeat(40));
    assert_ne!(evaluate(&f, None).status, RequiredRunsStatus::Satisfied);
    f.head.oid = f.checks[0].head_oid.clone().unwrap();
    let lineage = HeadRunLineage {
        head_sha: f.head.oid.0.clone(),
        complete: false,
        ..HeadRunLineage::default()
    };
    assert_eq!(
        evaluate(&f, Some(&lineage)).status,
        RequiredRunsStatus::UnknownProviderState
    );
}
