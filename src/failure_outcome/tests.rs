//! Hermetic fixtures for every failure class (bd-7e0aa1).

use super::*;
use crate::model::{
    AutoMergeState, BranchSnapshot, CommitOid, GitHubApiTelemetry, PullRequestSnapshot,
    PullRequestState, RepositoryId,
};

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/failures/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn assert_secret_free(outcome: &FailureOutcome) {
    let encoded = serde_json::to_string(outcome).unwrap();
    for forbidden in [
        "ghs_",
        "token",
        "receipt invariant mismatch",
        "Something went wrong",
        "user ID",
        "expected absent",
    ] {
        assert!(!encoded.contains(forbidden), "{forbidden} in {encoded}");
    }
}

type FixtureCase = (
    &'static str,
    FailureSurface,
    FailureClass,
    Option<&'static str>,
    Option<&'static str>,
    bool,
);

#[test]
#[allow(clippy::too_many_lines)] // One row per fixture keeps the class table auditable.
fn every_fixture_maps_to_one_stable_class_and_refusing_step() {
    let cases: [FixtureCase; 9] = [
        (
            "caco-force-intent-dead-letter-268",
            FailureSurface::Publication,
            FailureClass::ProcessRefusal,
            Some("force_intent"),
            Some("caravan:bd-78d4aa cacophony:bd-5666aa"),
            false,
        ),
        (
            "cara-app-probe-timeout",
            FailureSurface::Join,
            FailureClass::Timeout,
            Some("repository_probe"),
            Some("caravan:bd-096b1e"),
            true,
        ),
        (
            "caco-graphql-rate-limit",
            FailureSurface::Unknown,
            FailureClass::BudgetRefusal,
            None,
            None,
            true,
        ),
        (
            "cara-graphql-internal-error",
            FailureSurface::Sync,
            FailureClass::ProviderError,
            None,
            None,
            true,
        ),
        (
            "readiness-deferred-label-absent-4575",
            FailureSurface::Unknown,
            FailureClass::ProcessRefusal,
            Some("deferred_label_absent"),
            Some("caravan:bd-06e059"),
            false,
        ),
        (
            "cara-stack-recovery-backend-unhealthy",
            FailureSurface::Sync,
            FailureClass::ProcessRefusal,
            None,
            Some("caravan:bd-4d4621"),
            false,
        ),
        (
            "cara-membership-envelope-rolled-back",
            FailureSurface::Join,
            FailureClass::ProcessRefusal,
            None,
            Some("caravan:bd-420191"),
            false,
        ),
        (
            "cara-rebase-conflict",
            FailureSurface::Join,
            FailureClass::ConflictOrTestFailure,
            None,
            None,
            false,
        ),
        (
            "cara-command-timeout",
            FailureSurface::Sync,
            FailureClass::Timeout,
            None,
            None,
            true,
        ),
    ];
    for (name, surface, class, step, bug, retryable) in cases {
        let outcome = classify_envelope(surface, &fixture(name));
        assert_eq!(outcome.class, class, "{name}");
        assert_eq!(outcome.refusing_step.as_deref(), step, "{name}");
        assert_eq!(outcome.bug.as_deref(), bug, "{name}");
        assert_eq!(outcome.retryable, retryable, "{name}");
        assert!(!outcome.needs_bug, "{name}: every known class links a bug");
        assert_eq!(outcome.process_only, None, "{name}: unproven without truth");
        assert_secret_free(&outcome);
    }
    // Envelope-declared surfaces are honoured when the caller names none.
    assert_eq!(
        classify_envelope(
            FailureSurface::Unknown,
            &fixture("readiness-deferred-label-absent-4575")
        )
        .surface,
        FailureSurface::Readiness
    );
    assert_eq!(
        classify_envelope(FailureSurface::Unknown, &fixture("caco-graphql-rate-limit")).surface,
        FailureSurface::Publication
    );
}

#[test]
fn stacked_parent_merged_mid_publication_is_a_new_process_refusal_needing_a_bug() {
    // 2026-10-20: Caravan PR #270 was published on PR #269's branch; #269
    // merged before the join, the PR was retargeted to main and labelled, and
    // the queue merged it. Caco still refused its native Stack handoff.
    let outcome = classify_envelope(
        FailureSurface::Publication,
        &fixture("caco-native-stack-handoff-270"),
    );
    assert_eq!(outcome.class, FailureClass::ProcessRefusal);
    assert_eq!(
        outcome.code,
        "native_github_stack_handoff_validation_failed"
    );
    assert!(outcome.needs_bug);
    assert!(!outcome.retryable);
    assert_secret_free(&outcome);
}

#[test]
fn unknown_process_refusals_ask_for_a_bug() {
    let outcome = classify(
        FailureSurface::Join,
        "join_receipt_predecessor_missing",
        "successful membership operation did not retain its exact predecessor receipt",
        None,
    );
    assert_eq!(outcome.class, FailureClass::ProcessRefusal);
    assert!(outcome.needs_bug);
    assert!(
        !outcome.retryable,
        "process refusals are fixed, not retried"
    );
}

fn proof(succeeded: bool) -> crate::membership_proof::MembershipProofOutput {
    let repository = RepositoryId {
        owner: "harryaskham".to_owned(),
        name: "caravan".to_owned(),
    };
    let branch = |name: &str, oid: &str| BranchSnapshot {
        repository: repository.clone(),
        name: name.to_owned(),
        oid: CommitOid(oid.to_owned()),
    };
    let head = "05e14f58f0103dc2372e9b96172c6b6cc9560488";
    let candidate = PullRequestSnapshot {
        number: PrNumber(268),
        title: "bd-096b1e".to_owned(),
        url: "https://github.com/harryaskham/caravan/pull/268".to_owned(),
        state: PullRequestState::Open,
        draft: false,
        head: branch("agent/x", head),
        base: branch("main", "0dc40787a81320c7ec5b5065d515475646cde92c"),
        cross_repository: false,
        labels: if succeeded {
            std::iter::once("caravan".to_owned()).collect()
        } else {
            std::collections::BTreeSet::new()
        },
        auto_merge: AutoMergeState {
            enabled: false,
            merge_method: None,
            actor: None,
        },
        checks: Vec::new(),
        created_at: None,
        merged_at: None,
        updated_at: None,
        merge_state_status: None,
    };
    crate::membership_proof::derive(
        crate::membership_proof::MembershipProofFacts {
            repository: repository.clone(),
            candidate,
            caravan: succeeded.then_some((PrNumber(268), 1)),
            default_branch: branch("main", "0dc40787a81320c7ec5b5065d515475646cde92c"),
            member_label: "caravan".to_owned(),
            provider_api: GitHubApiTelemetry::default(),
        },
        &crate::membership_proof::MembershipProofInput {
            pr: 268,
            expected_head: Some(head.to_owned()),
            expected_base_ref: Some("main".to_owned()),
        },
    )
}

#[test]
fn force_intent_dead_letter_is_process_only_when_provider_shows_enrollment() {
    // 2026-10-20: Caravan PR #268 was open, labelled and a caravan member at
    // the exact published head, yet Caco dead-lettered it over `none`.
    let outcome = classify_envelope(
        FailureSurface::Publication,
        &fixture("caco-force-intent-dead-letter-268"),
    );
    let enrolled = with_provider_truth(outcome.clone(), &proof(true));
    assert_eq!(enrolled.process_only, Some(true));
    let unjoined = with_provider_truth(outcome, &proof(false));
    assert_eq!(unjoined.process_only, Some(false));
    // Truth never relabels a non-process failure as process-only.
    let timeout = with_provider_truth(
        classify_envelope(FailureSurface::Join, &fixture("cara-app-probe-timeout")),
        &proof(true),
    );
    assert_eq!(timeout.process_only, None);
}

fn failure_event(
    kind: EventKind,
    code: &str,
    reason: &str,
    prs: &[u64],
    timestamp: &str,
) -> crate::journal::JournalRecord {
    let mut event = crate::hooks::event(
        kind,
        crate::model::OperationId("01a159de-0000-7000-8000-000000000000".to_owned()),
        RepositoryId {
            owner: "harryaskham".to_owned(),
            name: "caravan".to_owned(),
        },
        prs.first().map(|pr| PrNumber(*pr)),
        prs.iter().map(|pr| PrNumber(*pr)).collect(),
        None,
        Some(reason.to_owned()),
        std::collections::BTreeMap::from([("error_code".to_owned(), serde_json::json!(code))]),
    );
    event.timestamp = timestamp.to_owned();
    crate::journal::JournalRecord::Event { version: 1, event }
}

#[test]
fn journal_summary_counts_classes_flags_process_only_and_keeps_no_messages() {
    let records = vec![
        failure_event(
            EventKind::SyncFailed,
            "github_stack_recovery_backend_unhealthy",
            "native Stack recovery permits only the expected missing-Stack backend problem",
            &[4444],
            "1792500000000",
        ),
        failure_event(
            EventKind::JoinFailed,
            "membership_envelope_rolled_back",
            "secret-free but long reason text that must not be copied",
            &[4481],
            "1792500001000",
        ),
        failure_event(
            EventKind::SyncFailed,
            "github_discovery_failed",
            "gh: Something went wrong while executing your query",
            &[],
            "1792500002000",
        ),
        crate::journal::JournalRecord::Event {
            version: 1,
            event: crate::hooks::event(
                EventKind::SyncCompleted,
                crate::model::OperationId("ok".to_owned()),
                RepositoryId {
                    owner: "harryaskham".to_owned(),
                    name: "caravan".to_owned(),
                },
                None,
                Vec::new(),
                None,
                None,
                std::collections::BTreeMap::new(),
            ),
        },
    ];
    let truth = |pr: PrNumber| match pr.0 {
        4481 => Some(true),
        4444 => Some(false),
        _ => None,
    };
    let summary = summarize(&records, &truth);
    assert_eq!(summary.scanned_records, 4);
    assert_eq!(summary.failures, 3);
    assert_eq!(summary.by_class["process_refusal"], 2);
    assert_eq!(summary.by_class["provider_error"], 1);
    assert_eq!(summary.process_only, 1);
    assert_eq!(
        summary.by_signature["join/process_refusal/membership_envelope_rolled_back"],
        1
    );
    // Newest first.
    assert_eq!(summary.recent[0].outcome.code, "github_discovery_failed");
    assert_eq!(summary.recent[1].outcome.process_only, Some(true));
    assert_eq!(summary.recent[2].outcome.process_only, Some(false));
    let encoded = serde_json::to_string(&summary).unwrap();
    assert!(!encoded.contains("must not be copied"));
    assert!(!encoded.contains("Something went wrong"));
}

#[test]
fn identifiers_are_bounded_and_allowlisted() {
    let outcome = classify(
        FailureSurface::Readiness,
        "Bad Code; rm -rf /",
        "cara-readiness-refused predicate=deferred_label_absent$(curl evil) exit=1",
        None,
    );
    assert_eq!(outcome.code, "unknown");
    assert_eq!(
        outcome.refusing_step.as_deref(),
        Some("deferred_label_absent")
    );
    let long = "a".repeat(500);
    let outcome = classify(FailureSurface::Sync, &long, "", None);
    assert_eq!(outcome.code.len(), MAX_IDENTIFIER_BYTES);
}
