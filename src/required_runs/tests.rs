//! Hermetic required-run coverage classification fixtures.
use super::*;
use crate::model::CommitOid;

fn repository() -> RepositoryId {
    RepositoryId {
        owner: "harryaskham".to_owned(),
        name: "caravan".to_owned(),
    }
}

fn branch(name: &str, oid: &str) -> BranchSnapshot {
    BranchSnapshot {
        repository: repository(),
        name: name.to_owned(),
        oid: CommitOid(oid.to_owned()),
    }
}

const HEAD: &str = "79abc31d4efc07a579145cf904c83c1420f8b4ac";
const BASE: &str = "b464e1ae5cb8033a0789997652d97d6b3efd5c7e";
const PUBLISHED: &str = "2026-07-26T22:03:00Z";
/// One hour after the head was published.
const NOW: u64 = 1_785_106_980;

fn contexts(values: &[&str]) -> RequiredContextsRead {
    RequiredContextsRead {
        branch: "main".to_owned(),
        protected: true,
        contexts: values.iter().map(|value| (*value).to_owned()).collect(),
        complete: true,
    }
    .normalized()
}

fn required() -> RequiredContextsRead {
    contexts(&["Check & Lint", "Fast Tests (unit)"])
}

fn check(name: &str, state: CheckState) -> CheckSnapshot {
    CheckSnapshot {
        name: name.to_owned(),
        state,
        provider_state: Some(format!("{state:?}").to_uppercase()),
        details_url: None,
    }
}

fn suite(id: u64, head_sha: &str, status: &str, conclusion: &str) -> CheckSuiteLineage {
    CheckSuiteLineage {
        id,
        head_sha: head_sha.to_owned(),
        status: status.to_owned(),
        conclusion: conclusion.to_owned(),
        app_slug: "github-actions".to_owned(),
        rerequestable: true,
    }
}

fn run(run_id: u64, head_sha: &str, status: &str, conclusion: &str) -> WorkflowRunLineage {
    WorkflowRunLineage {
        run_id,
        check_suite_id: run_id,
        workflow_name: "CI".to_owned(),
        head_sha: head_sha.to_owned(),
        status: status.to_owned(),
        conclusion: conclusion.to_owned(),
        event: "pull_request".to_owned(),
    }
}

fn lineage(
    suites: Vec<CheckSuiteLineage>,
    runs: Vec<WorkflowRunLineage>,
    complete: bool,
) -> HeadRunLineage {
    HeadRunLineage {
        head_sha: HEAD.to_owned(),
        check_suites: suites,
        workflow_runs: runs,
        head_committed_at: Some(PUBLISHED.to_owned()),
        complete,
    }
    .bounded()
}

struct Case {
    contexts: RequiredContextsRead,
    lineage: Option<HeadRunLineage>,
    checks: Vec<CheckSnapshot>,
    published: Option<String>,
    grace_secs: u64,
    now_unix: u64,
    pr: u64,
}

impl Case {
    fn new() -> Self {
        Self {
            contexts: required(),
            lineage: Some(lineage(Vec::new(), Vec::new(), true)),
            checks: Vec::new(),
            published: Some(PUBLISHED.to_owned()),
            grace_secs: 300,
            now_unix: NOW,
            pr: 2208,
        }
    }

    fn assess(&self) -> RequiredRunsAssessment {
        let head = branch("caravan/2208", HEAD);
        let base = branch("main", BASE);
        super::assess(&RequiredRunsInput {
            pr: PrNumber(self.pr),
            head: &head,
            base: &base,
            contexts: &self.contexts,
            lineage: self.lineage.as_ref(),
            checks: &self.checks,
            head_published_at: self.published.as_deref(),
            clock: RequiredRunsClock {
                now_unix: self.now_unix,
                grace_secs: self.grace_secs,
            },
        })
    }
}

#[test]
fn timestamps_parse_utc_offsets_and_fractions() {
    let utc = rfc3339_to_unix_secs("2026-07-26T22:03:00Z").expect("Z parses");
    assert_eq!(utc, 1_785_103_380);
    assert_eq!(
        rfc3339_to_unix_secs("2026-07-26T23:03:00+01:00"),
        Some(utc),
        "a numeric offset must normalize to the same instant"
    );
    assert_eq!(rfc3339_to_unix_secs("2026-07-26T21:03:00-01:00"), Some(utc));
    assert_eq!(
        rfc3339_to_unix_secs("2026-07-26T22:03:00.123456Z"),
        Some(utc),
        "fractional seconds must not shift the instant"
    );
    assert_eq!(rfc3339_to_unix_secs("1970-01-01T00:00:00Z"), Some(0));
    for invalid in [
        "",
        "2026-07-26",
        "2026-07-26 22:03:00Z",
        "2026-13-26T22:03:00Z",
        "2026-07-26T22:03:00",
        "not-a-timestamp-at-all",
    ] {
        assert_eq!(
            rfc3339_to_unix_secs(invalid),
            None,
            "`{invalid}` must never be guessed into a countdown start"
        );
    }
}

#[test]
fn every_required_context_missing_is_the_stalling_class() {
    let assessment = Case::new().assess();
    assert_eq!(assessment.status, RequiredRunsStatus::MissingRequiredRuns);
    assert_eq!(
        assessment.missing_contexts,
        vec!["Check & Lint".to_owned(), "Fast Tests (unit)".to_owned()]
    );
    assert_eq!(assessment.observed_check_suites, 0);
    assert_eq!(assessment.observed_runs, 0);
    assert_eq!(assessment.recovery, RequiredRunsRecovery::OperatorAction);
    assert!(assessment.requires_problem());
    let problem = problem(PrNumber(2208), &assessment, None).expect("stall must be visible");
    assert_eq!(problem.kind, MissingRequiredRunsKind::MissingRequiredRuns);
    assert!(problem.operator_action_required);
    assert!(problem.next.contains("close and immediately reopen"));
    assert!(
        problem.message.contains(HEAD),
        "the exact head must be named"
    );
    assert!(problem.contexts.contains(&"Check & Lint".to_owned()));
}

#[test]
fn one_missing_context_still_stalls_while_the_other_passes() {
    let mut case = Case::new();
    case.checks = vec![check("Check & Lint", CheckState::Success)];
    case.lineage = Some(lineage(
        Vec::new(),
        vec![run(30_222_268_397, HEAD, "completed", "success")],
        true,
    ));
    let assessment = case.assess();
    assert_eq!(assessment.status, RequiredRunsStatus::MissingRequiredRuns);
    assert_eq!(
        assessment.missing_contexts,
        vec!["Fast Tests (unit)".to_owned()]
    );
    assert_eq!(assessment.observed_runs, 1);
    let passing = assessment
        .coverage
        .iter()
        .find(|item| item.context == "Check & Lint")
        .expect("reported context");
    assert_eq!(passing.state, RequiredContextState::Passing);
}

#[test]
fn expected_only_rollup_entries_are_not_reporting_evidence() {
    let mut case = Case::new();
    case.checks = vec![
        check("Check & Lint", CheckState::Expected),
        check("Fast Tests (unit)", CheckState::Expected),
    ];
    let assessment = case.assess();
    assert_eq!(
        assessment.status,
        RequiredRunsStatus::MissingRequiredRuns,
        "an EXPECTED placeholder is exactly the forever-pending trap"
    );
    assert!(
        assessment
            .coverage
            .iter()
            .all(|item| item.reporting_checks.is_empty())
    );
}

#[test]
fn delayed_run_arrival_inside_grace_is_an_ordinary_wait() {
    let mut case = Case::new();
    case.now_unix = 1_785_103_400;
    let assessment = case.assess();
    assert_eq!(assessment.status, RequiredRunsStatus::AwaitingGrace);
    assert!(!assessment.grace_elapsed);
    assert_eq!(assessment.recovery, RequiredRunsRecovery::AwaitGrace);
    assert!(assessment.status.is_waiting());
    assert!(problem(PrNumber(2208), &assessment, None).is_none());
}

#[test]
fn delayed_run_arrival_after_grace_is_pending_once_lineage_appears() {
    let mut case = Case::new();
    case.lineage = Some(lineage(
        vec![suite(1, HEAD, "queued", "")],
        vec![run(30_222_268_397, HEAD, "queued", "")],
        true,
    ));
    let assessment = case.assess();
    assert_eq!(
        assessment.status,
        RequiredRunsStatus::Pending,
        "a live suite on the exact head means the context is still coming"
    );
    assert!(assessment.missing_contexts.is_empty());
    assert_eq!(assessment.recovery, RequiredRunsRecovery::None);
}

#[test]
fn a_run_on_a_superseded_head_never_counts_as_coverage() {
    let mut case = Case::new();
    case.lineage = Some(lineage(
        vec![suite(
            1,
            "0000000000000000000000000000000000000000",
            "completed",
            "success",
        )],
        vec![run(
            30_222_268_000,
            "0000000000000000000000000000000000000000",
            "completed",
            "success",
        )],
        true,
    ));
    let assessment = case.assess();
    assert_eq!(assessment.status, RequiredRunsStatus::MissingRequiredRuns);
    assert_eq!(assessment.observed_check_suites, 0);
    assert_eq!(assessment.observed_runs, 0);
    assert_eq!(
        assessment.stale_head_runs, 2,
        "superseded-generation lineage is retained as evidence, never coverage"
    );
    assert_eq!(
        assessment.recovery,
        RequiredRunsRecovery::OperatorAction,
        "a foreign generation is never rerequested"
    );
}

#[test]
fn cancelled_superseded_lineage_is_distinct_from_missing() {
    let mut case = Case::new();
    case.lineage = Some(lineage(
        vec![suite(4242, HEAD, "completed", "cancelled")],
        vec![run(30_222_268_397, HEAD, "completed", "cancelled")],
        true,
    ));
    let assessment = case.assess();
    assert_eq!(assessment.status, RequiredRunsStatus::CancelledSuperseded);
    assert!(assessment.missing_contexts.is_empty());
    assert_eq!(
        assessment.recovery,
        RequiredRunsRecovery::RerequestCheckSuite {
            check_suite_id: 4242
        }
    );
    let problem = problem(PrNumber(2208), &assessment, None).expect("stall must be visible");
    assert_eq!(
        problem.kind,
        MissingRequiredRunsKind::CancelledSupersededRequiredRuns
    );
    assert!(problem.operator_action_required);
}

#[test]
fn a_partial_protection_read_can_never_prove_a_missing_context() {
    let mut case = Case::new();
    case.contexts = RequiredContextsRead::partial("main");
    let assessment = case.assess();
    assert_eq!(assessment.status, RequiredRunsStatus::UnknownProviderState);
    assert!(!assessment.provider_reads_complete);
    let problem = problem(PrNumber(2208), &assessment, None).expect("unknown state stays visible");
    assert_eq!(problem.kind, MissingRequiredRunsKind::UnknownProviderState);
    assert!(
        !problem.operator_action_required,
        "an unreadable provider is a retry, not an operator chore"
    );
    assert!(problem.next.contains("bounded sync tick"));
}

#[test]
fn a_partial_lineage_read_can_never_prove_a_missing_context() {
    let mut case = Case::new();
    case.lineage = Some(lineage(Vec::new(), Vec::new(), false));
    let assessment = case.assess();
    assert_eq!(assessment.status, RequiredRunsStatus::UnknownProviderState);
    assert!(assessment.recovery == RequiredRunsRecovery::None);
}

#[test]
fn a_skipped_lineage_read_with_absent_contexts_is_unknown_not_missing() {
    let mut case = Case::new();
    case.lineage = None;
    let assessment = case.assess();
    assert_eq!(assessment.status, RequiredRunsStatus::UnknownProviderState);
}

#[test]
fn an_unreadable_head_timestamp_never_starts_a_countdown_from_zero() {
    let mut case = Case::new();
    case.published = None;
    case.lineage = Some(HeadRunLineage {
        head_sha: HEAD.to_owned(),
        check_suites: Vec::new(),
        workflow_runs: Vec::new(),
        head_committed_at: None,
        complete: true,
    });
    let assessment = case.assess();
    assert_eq!(assessment.status, RequiredRunsStatus::UnknownProviderState);
    assert_eq!(assessment.head_age_secs, None);
    assert!(assessment.reason.contains("no head timestamp"));
}

#[test]
fn an_unprotected_base_requires_nothing_and_reads_no_lineage() {
    let mut case = Case::new();
    case.contexts = RequiredContextsRead::unprotected("caravan/2208");
    case.lineage = None;
    let assessment = case.assess();
    assert_eq!(assessment.status, RequiredRunsStatus::NotRequired);
    assert!(!assessment.requires_problem());
    assert_eq!(assessment.recovery, RequiredRunsRecovery::None);
}

#[test]
fn fully_reported_contexts_are_satisfied_pending_or_failing() {
    let mut satisfied = Case::new();
    satisfied.checks = vec![
        check("Check & Lint", CheckState::Success),
        check("Fast Tests (unit)", CheckState::Skipped),
    ];
    satisfied.lineage = None;
    assert_eq!(satisfied.assess().status, RequiredRunsStatus::Satisfied);

    let mut pending = Case::new();
    pending.checks = vec![
        check("Check & Lint", CheckState::Success),
        check("Fast Tests (unit)", CheckState::InProgress),
    ];
    pending.lineage = None;
    let pending = pending.assess();
    assert_eq!(pending.status, RequiredRunsStatus::Pending);
    assert!(!pending.requires_problem());

    let mut failing = Case::new();
    failing.checks = vec![
        check("Check & Lint", CheckState::Failure),
        check("Fast Tests (unit)", CheckState::Success),
    ];
    failing.lineage = None;
    let failing = failing.assess();
    assert_eq!(
        failing.status,
        RequiredRunsStatus::Failing,
        "honest failures stay owned by CI decision policy"
    );
    assert!(!failing.requires_problem());
}

#[test]
fn an_uninterpretable_rollup_state_is_unknown_not_missing() {
    let mut case = Case::new();
    case.checks = vec![
        check("Check & Lint", CheckState::Success),
        CheckSnapshot {
            name: "Fast Tests (unit)".to_owned(),
            state: CheckState::Unknown,
            provider_state: Some("SOMETHING_NEW".to_owned()),
            details_url: None,
        },
    ];
    let assessment = case.assess();
    assert_eq!(assessment.status, RequiredRunsStatus::UnknownProviderState);
    let unknown = assessment
        .coverage
        .iter()
        .find(|item| item.context == "Fast Tests (unit)")
        .expect("required context");
    assert_eq!(unknown.state, RequiredContextState::Unknown);
    assert_eq!(unknown.provider_state.as_deref(), Some("SOMETHING_NEW"));
}

#[test]
fn only_rerequestable_suites_on_the_exact_head_are_selectable() {
    let mixed = lineage(
        vec![
            CheckSuiteLineage {
                rerequestable: false,
                ..suite(11, HEAD, "completed", "cancelled")
            },
            suite(99, HEAD, "completed", "cancelled"),
            suite(2, "other", "completed", "cancelled"),
        ],
        Vec::new(),
        true,
    );
    assert_eq!(rerequestable_suite(Some(&mixed), HEAD), Some(99));
    assert_eq!(rerequestable_suite(None, HEAD), None);
}

#[test]
fn problems_deduplicate_by_kind_pr_head_and_contexts() {
    let assessment = Case::new().assess();
    let first = problem(PrNumber(2208), &assessment, None).expect("problem");
    let second = problem(PrNumber(2208), &assessment, None).expect("problem");
    assert_eq!(first.fingerprint, second.fingerprint);
    let mut problems = Vec::new();
    push_problem(&mut problems, first);
    push_problem(&mut problems, second);
    assert_eq!(problems.len(), 1, "hook evidence must be deduplicated");

    let mut other = Case::new();
    other.pr = 2210;
    let other = problem(PrNumber(2208), &other.assess(), None).expect("problem");
    assert_ne!(
        other.fingerprint, problems[0].fingerprint,
        "a different member must be an independently visible problem"
    );
    push_problem(&mut problems, other);
    assert_eq!(problems.len(), 2);

    for index in 0..MAX_MISSING_REQUIRED_RUNS_PROBLEMS * 2 {
        let mut case = Case::new();
        case.pr = 9_000 + index as u64;
        if let Some(item) = problem(PrNumber(2208), &case.assess(), None) {
            push_problem(&mut problems, item);
        }
    }
    assert_eq!(
        problems.len(),
        MAX_MISSING_REQUIRED_RUNS_PROBLEMS,
        "problem evidence must stay bounded"
    );
}

#[test]
fn receipts_seal_and_retain_auditable_provenance() {
    let assessment = Case::new().assess();
    let operation = OperationId("operation-1".to_owned());
    let sealed = receipt(
        &repository(),
        PrNumber(2208),
        assessment.clone(),
        Some(RequiredRunsRetrigger {
            check_suite_id: 4242,
            head_oid: CommitOid(HEAD.to_owned()),
            attempts: REQUIRED_RUNS_RETRIGGER_ATTEMPTS,
            requested: true,
            rediscovered: true,
            status_after: RequiredRunsStatus::MissingRequiredRuns,
            failure: None,
        }),
        provenance(&operation, assessment.reason.as_str()),
    );
    assert!(sealed.hash_is_valid());
    assert_eq!(sealed.pr, PrNumber(2208));
    assert_eq!(sealed.assessment.head.oid, CommitOid(HEAD.to_owned()));
    assert_eq!(sealed.provenance.owner, REQUIRED_RUNS_OWNER);
    assert_eq!(sealed.provenance.component, REQUIRED_RUNS_COMPONENT);
    assert_eq!(sealed.provenance.operation_id, operation);

    let mut tampered = sealed.clone();
    tampered.assessment.status = RequiredRunsStatus::Satisfied;
    assert!(!tampered.hash_is_valid());
}

#[test]
fn statuses_expose_stable_codes_for_every_class() {
    for (status, code) in [
        (RequiredRunsStatus::NotRequired, "not_required"),
        (RequiredRunsStatus::Satisfied, "satisfied"),
        (RequiredRunsStatus::Pending, "pending"),
        (RequiredRunsStatus::Failing, "failing"),
        (
            RequiredRunsStatus::CancelledSuperseded,
            "cancelled_superseded",
        ),
        (RequiredRunsStatus::AwaitingGrace, "awaiting_grace"),
        (
            RequiredRunsStatus::MissingRequiredRuns,
            "missing_required_runs",
        ),
        (
            RequiredRunsStatus::UnknownProviderState,
            "unknown_provider_state",
        ),
    ] {
        assert_eq!(status.code(), code);
    }
    assert!(RequiredRunsStatus::MissingRequiredRuns.stalls_forever());
    assert!(RequiredRunsStatus::CancelledSuperseded.stalls_forever());
    assert!(!RequiredRunsStatus::Failing.stalls_forever());
    assert_eq!(
        RequiredRunsRecovery::RerequestCheckSuite { check_suite_id: 1 }.code(),
        "rerequest_check_suite"
    );
}

#[test]
fn bounded_reads_never_grow_without_limit() {
    let suites = (0..(MAX_REPORTED_LINEAGE as u64 * 3))
        .map(|id| suite(id, HEAD, "completed", "success"))
        .collect::<Vec<_>>();
    let runs = (0..(MAX_REPORTED_LINEAGE as u64 * 3))
        .map(|id| run(id, HEAD, "completed", "success"))
        .collect::<Vec<_>>();
    let bounded = lineage(suites, runs, true);
    assert_eq!(bounded.check_suites.len(), MAX_REPORTED_LINEAGE);
    assert_eq!(bounded.workflow_runs.len(), MAX_REPORTED_LINEAGE);

    let many = (0..(MAX_REPORTED_CONTEXTS * 3))
        .map(|index| format!("context-{index:03}"))
        .collect::<Vec<_>>();
    let read = RequiredContextsRead {
        branch: "main".to_owned(),
        protected: true,
        contexts: many,
        complete: true,
    }
    .normalized();
    assert_eq!(read.contexts.len(), MAX_REPORTED_CONTEXTS);
}

// --- Live Cacophony PR2208 evidence -----------------------------------------
//
// Exact read-only facts observed on `harryaskham/cacophony` PR 2208, head
// `79abc31d4efc07a579145cf904c83c1420f8b4ac`, base `main` at
// `b464e1ae5cb8033a0789997652d97d6b3efd5c7e`. Protection on `main` requires
// `Check & Lint` and `Fast Tests (unit)`. No child PR was mutated to obtain
// them; these fixtures replay the payloads deterministically.

/// The rollup GitHub actually served: five reporting CI jobs, none of which is
/// one of the two required contexts.
fn pr2208_rollup() -> Vec<CheckSnapshot> {
    [
        ("sccache rustc spawn canary", CheckState::Skipped),
        ("Changed surface admission", CheckState::Success),
        ("macOS app (nix build)", CheckState::Skipped),
        ("Public Fast Tests preparation", CheckState::InProgress),
        ("Fast Tests preparation", CheckState::Skipped),
    ]
    .into_iter()
    .map(|(name, state)| check(name, state))
    .collect()
}

#[test]
fn cacophony_pr2208_stalled_head_is_the_missing_required_runs_class() {
    // The observed incident state: the rebased head existed with zero workflow
    // runs and zero check suites, so both required contexts had no reporting
    // lineage while the PR sat MERGEABLE/BLOCKED.
    let mut case = Case::new();
    case.checks = pr2208_rollup();
    case.lineage = Some(lineage(Vec::new(), Vec::new(), true));

    let assessment = case.assess();

    assert_eq!(assessment.status, RequiredRunsStatus::MissingRequiredRuns);
    assert_eq!(
        assessment.missing_contexts,
        vec!["Check & Lint".to_owned(), "Fast Tests (unit)".to_owned()],
        "neither required context is produced by the reporting jobs"
    );
    assert_eq!(
        assessment.recovery,
        RequiredRunsRecovery::OperatorAction,
        "with zero suites there is nothing safe to rerequest"
    );
    let problem = problem(PrNumber(2208), &assessment, None).expect("the stall must be visible");
    assert!(problem.operator_action_required);
    assert_eq!(problem.head.oid.0, HEAD);
    assert_eq!(problem.base.oid.0, BASE);
}

#[test]
fn cacophony_pr2208_recovered_head_is_pending_not_a_stall() {
    // After the close/reopen queued `pull_request` run 30222268397 on the
    // unchanged head, the same commit exposed five check suites (three foreign
    // apps queued, one cancelled Actions suite, one in-progress Actions suite)
    // and two Actions runs. Required contexts still do not report yet, but live
    // lineage exists, so this is an ordinary wait, not a stall.
    let mut case = Case::new();
    case.checks = pr2208_rollup();
    case.lineage = Some(lineage(
        vec![
            CheckSuiteLineage {
                app_slug: "cursor".to_owned(),
                ..suite(81_895_334_808, HEAD, "queued", "")
            },
            CheckSuiteLineage {
                app_slug: "claude".to_owned(),
                ..suite(81_895_334_871, HEAD, "queued", "")
            },
            CheckSuiteLineage {
                app_slug: "aviator-app".to_owned(),
                ..suite(81_895_334_923, HEAD, "queued", "")
            },
            suite(81_895_339_455, HEAD, "completed", "cancelled"),
            suite(81_895_922_485, HEAD, "in_progress", ""),
        ],
        vec![
            run(30_222_268_397, HEAD, "in_progress", ""),
            run(30_222_037_735, HEAD, "completed", "cancelled"),
        ],
        true,
    ));

    let assessment = case.assess();

    assert_eq!(
        assessment.status,
        RequiredRunsStatus::Pending,
        "a live suite on the exact head outranks a sibling cancelled suite"
    );
    assert!(assessment.missing_contexts.is_empty());
    assert_eq!(assessment.observed_check_suites, 5);
    assert_eq!(assessment.observed_runs, 2);
    assert_eq!(assessment.stale_head_runs, 0);
    assert_eq!(assessment.recovery, RequiredRunsRecovery::None);
    assert!(problem(PrNumber(2208), &assessment, None).is_none());
}

#[test]
fn cacophony_pr2208_required_contexts_survive_legacy_and_typed_declarations() {
    // Protection served the same two contexts through both the legacy
    // `contexts` array and the typed `checks` array; the union must not
    // duplicate them into a phantom third requirement.
    let read = RequiredContextsRead {
        branch: "main".to_owned(),
        protected: true,
        contexts: vec![
            "Check & Lint".to_owned(),
            "Fast Tests (unit)".to_owned(),
            "Check & Lint".to_owned(),
            "Fast Tests (unit)".to_owned(),
        ],
        complete: true,
    }
    .normalized();
    assert_eq!(
        read.contexts,
        vec!["Check & Lint".to_owned(), "Fast Tests (unit)".to_owned()]
    );
}
