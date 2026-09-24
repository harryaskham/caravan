//! bd-e81848: required policy is a context/App/commit contract, not an all-CI gate.
use super::*;
use crate::model::CommitOid;

fn observation(name: &str, app: u64, head: &str, state: CheckState) -> CheckSnapshot {
    CheckSnapshot {
        name: name.to_owned(),
        state,
        provider_kind: Some("CheckRun".to_owned()),
        app_id: Some(app),
        head_oid: Some(CommitOid(head.to_owned())),
        ..CheckSnapshot::default()
    }
}

fn evaluate(policy: &RequiredContextsRead, checks: &[CheckSnapshot]) -> RequiredRunsAssessment {
    let repository = RepositoryId {
        owner: "test".to_owned(),
        name: "repo".to_owned(),
    };
    let head = BranchSnapshot {
        repository: repository.clone(),
        name: "topic".to_owned(),
        oid: CommitOid("head".to_owned()),
    };
    let base = BranchSnapshot {
        repository,
        name: "parent-topic".to_owned(),
        oid: CommitOid("parent".to_owned()),
    };
    let lineage = HeadRunLineage {
        head_sha: "head".to_owned(),
        complete: true,
        ..HeadRunLineage::default()
    };
    assess(&RequiredRunsInput {
        pr: PrNumber(1),
        head: &head,
        base: &base,
        contexts: policy,
        lineage: Some(&lineage),
        checks,
        head_published_at: Some("2026-01-01T00:00:00Z"),
        clock: RequiredRunsClock {
            now_unix: 1_800_000_000,
            grace_secs: 1,
        },
    })
}

fn policy() -> RequiredContextsRead {
    RequiredContextsRead {
        branch: "main".to_owned(),
        protected: true,
        complete: true,
        contexts: vec!["gate".to_owned()],
        checks: vec![RequiredCheck {
            context: "gate".to_owned(),
            app_id: Some(15368),
        }],
    }
    .normalized()
}

#[test]
fn effective_policy_ignores_optional_red_pending_and_same_named_foreign_apps() {
    let checks = vec![
        observation("gate", 15368, "head", CheckState::Success),
        observation("gate", 44, "head", CheckState::Failure),
        observation("optional", 15368, "head", CheckState::Failure),
        observation("optional-pending", 44, "head", CheckState::InProgress),
    ];
    let result = evaluate(&policy(), &checks);
    assert_eq!(result.status, RequiredRunsStatus::Satisfied);
    assert_eq!(result.required_policy.as_ref().unwrap().branch, "main");
    assert_eq!(result.coverage.len(), 1);
    assert_eq!(result.coverage[0].app_id, Some(15368));
    assert_eq!(
        checks[1].state,
        CheckState::Failure,
        "diagnostics remain intact"
    );
}

#[test]
fn effective_policy_requires_exact_app_head_and_real_reporting_check() {
    for check in [
        observation("gate", 44, "head", CheckState::Success),
        observation("gate", 15368, "old-head", CheckState::Success),
        CheckSnapshot {
            app_id: None,
            ..observation("gate", 15368, "head", CheckState::Success)
        },
        CheckSnapshot {
            head_oid: None,
            ..observation("gate", 15368, "head", CheckState::Success)
        },
        CheckSnapshot {
            provider_kind: Some("WorkflowRunLineage".to_owned()),
            ..observation("gate", 15368, "head", CheckState::Success)
        },
    ] {
        assert_ne!(
            evaluate(&policy(), &[check]).status,
            RequiredRunsStatus::Satisfied
        );
    }
}

#[test]
fn effective_policy_preserves_required_third_party_failure_pending_and_unknown() {
    let mut policy = policy();
    policy.checks.push(RequiredCheck {
        context: "security".to_owned(),
        app_id: Some(44),
    });
    let gate = observation("gate", 15368, "head", CheckState::Success);
    for (state, expected) in [
        (CheckState::Failure, RequiredRunsStatus::Failing),
        (CheckState::InProgress, RequiredRunsStatus::Pending),
        (
            CheckState::Unknown,
            RequiredRunsStatus::UnknownProviderState,
        ),
        (
            CheckState::Cancelled,
            RequiredRunsStatus::CancelledSuperseded,
        ),
        (CheckState::Success, RequiredRunsStatus::Satisfied),
    ] {
        assert_eq!(
            evaluate(
                &policy,
                &[gate.clone(), observation("security", 44, "head", state)]
            )
            .status,
            expected
        );
    }
    assert_eq!(
        evaluate(&policy, &[gate]).status,
        RequiredRunsStatus::MissingRequiredRuns
    );
}

#[test]
fn effective_policy_never_proves_a_truncated_or_unavailable_required_set() {
    let mut policy = policy();
    policy.complete = false;
    let green = observation("gate", 15368, "head", CheckState::Success);
    assert_eq!(
        evaluate(&policy, &[green.clone()]).status,
        RequiredRunsStatus::UnknownProviderState
    );
    assert_eq!(
        evaluate(&RequiredContextsRead::partial("main"), &[green]).status,
        RequiredRunsStatus::UnknownProviderState
    );
    policy.complete = true;
    policy
        .checks
        .extend((0..MAX_REPORTED_CONTEXTS).map(|i| RequiredCheck {
            context: format!("extra-{i}"),
            app_id: Some(44),
        }));
    assert!(!policy.normalized().complete);
}

#[test]
fn effective_policy_keeps_legacy_any_app_and_distinct_typed_requirements() {
    let mut policy = policy();
    policy.checks.push(RequiredCheck {
        context: "gate".to_owned(),
        app_id: Some(44),
    });
    policy.contexts.push("legacy".to_owned());
    let result = evaluate(
        &policy,
        &[
            observation("gate", 15368, "head", CheckState::Success),
            observation("gate", 44, "head", CheckState::Success),
            observation("legacy", 99, "head", CheckState::Success),
        ],
    );
    assert_eq!(result.status, RequiredRunsStatus::Satisfied);
    assert_eq!(result.coverage.len(), 3);
}

#[test]
fn effective_policy_foreign_newer_workflow_cannot_supersede_required_failure() {
    let mut failed = observation("gate", 15368, "head", CheckState::Failure);
    failed.workflow_name = Some("CI".to_owned());
    failed.details_url = Some("https://example.test/actions/runs/1/job/1".to_owned());
    let mut foreign = observation("gate", 44, "head", CheckState::Success);
    foreign.workflow_name = failed.workflow_name.clone();
    foreign.details_url = Some("https://example.test/actions/runs/2/job/1".to_owned());
    assert_eq!(
        evaluate(&policy(), &[failed, foreign]).status,
        RequiredRunsStatus::Failing
    );
}
