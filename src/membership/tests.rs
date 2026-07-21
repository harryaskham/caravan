//! Hermetic membership provider, policy, race, and resumability fixtures.
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::graph::analyze;
use crate::model::{
    AutoMergeState, BranchSnapshot, CommitOid, CompatibilityOutcome, CompatibilityReport,
};

#[derive(Default)]
struct FakeProvider {
    labels: BTreeSet<String>,
    allows_auto_merge: bool,
    branch_protected: bool,
    pull_requests: RefCell<BTreeMap<PrNumber, PullRequestSnapshot>>,
    fail_kind: RefCell<Option<MutationKind>>,
}

impl FakeProvider {
    fn with_pull_requests(pull_requests: Vec<PullRequestSnapshot>) -> Self {
        Self {
            labels: REQUIRED_LABELS.into_iter().map(str::to_owned).collect(),
            allows_auto_merge: true,
            branch_protected: true,
            pull_requests: RefCell::new(
                pull_requests
                    .into_iter()
                    .map(|pull_request| (pull_request.number, pull_request))
                    .collect(),
            ),
            fail_kind: RefCell::new(None),
        }
    }

    fn mutate(
        &self,
        expected: &PullRequestPrecondition,
        kind: MutationKind,
        update: impl FnOnce(&mut PullRequestSnapshot),
    ) -> Result<GitHubMutationReceipt, MutationError> {
        let mut pull_requests = self.pull_requests.borrow_mut();
        let current = pull_requests.get_mut(&expected.number).expect("fake PR");
        let actual = PullRequestPrecondition::from(&*current);
        if actual != *expected {
            return Err(MutationError::StalePrecondition {
                expected: Box::new(expected.clone()),
                actual: Box::new(actual),
                changed_fields: vec!["fake_race".to_owned()],
            });
        }
        if self.fail_kind.borrow().as_ref() == Some(&kind) {
            return Err(MutationError::Provider(
                crate::github::DiscoveryError::CommandFailed {
                    command: crate::command::CommandSpec::new("gh"),
                    code: Some(1),
                    stderr: "injected provider failure".to_owned(),
                },
            ));
        }
        let before = current.clone();
        update(current);
        Ok(GitHubMutationReceipt {
            kind,
            before: Some(before),
            after: current.clone(),
            provider_output: None,
        })
    }
}

impl MembershipProvider for FakeProvider {
    fn branch_is_protected(
        &self,
        _repository: &RepositoryId,
        _branch: &str,
    ) -> Result<bool, MutationError> {
        Ok(self.branch_protected)
    }

    fn repository_allows_auto_merge(
        &self,
        _repository: &RepositoryId,
    ) -> Result<bool, MutationError> {
        Ok(self.allows_auto_merge)
    }

    fn repository_labels(
        &self,
        _repository: &RepositoryId,
    ) -> Result<BTreeSet<String>, MutationError> {
        Ok(self.labels.clone())
    }

    fn create_pull_request(
        &self,
        _repository: &RepositoryId,
        _input: &CreatePullRequestInput,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        panic!("fixture tests use an existing PR")
    }

    fn set_base(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        base: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.mutate(expected, MutationKind::SetBase, |pull_request| {
            pull_request.base.name = base.to_owned();
            pull_request.base.oid = CommitOid(format!("{base}-oid"));
        })
    }

    fn add_label(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.mutate(expected, MutationKind::AddLabel, |pull_request| {
            pull_request.labels.insert(label.to_owned());
        })
    }

    fn remove_label(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.mutate(expected, MutationKind::RemoveLabel, |pull_request| {
            pull_request.labels.remove(label);
        })
    }

    fn ensure_control_label_comment(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        _audit: &ControlLabelAudit,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.mutate(expected, MutationKind::Comment, |_| {})
    }

    fn enable_squash_auto_merge(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.mutate(expected, MutationKind::EnableAutoMerge, |pull_request| {
            pull_request.auto_merge = AutoMergeState::squash();
        })
    }

    fn disable_auto_merge(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.mutate(expected, MutationKind::DisableAutoMerge, |pull_request| {
            pull_request.auto_merge = AutoMergeState::disabled();
        })
    }
}

fn repository() -> RepositoryId {
    RepositoryId {
        owner: "harryaskham".to_owned(),
        name: "caravan".to_owned(),
    }
}

fn branch(name: &str) -> BranchSnapshot {
    BranchSnapshot {
        repository: repository(),
        name: name.to_owned(),
        oid: CommitOid(format!("{name}-oid")),
    }
}

fn pull_request(number: u64, head: &str, base: &str, labels: &[&str]) -> PullRequestSnapshot {
    PullRequestSnapshot {
        number: PrNumber(number),
        title: format!("PR {number}"),
        url: format!("https://example.invalid/{number}"),
        state: PullRequestState::Open,
        draft: false,
        head: branch(head),
        base: branch(base),
        cross_repository: false,
        labels: labels.iter().map(|label| (*label).to_owned()).collect(),
        auto_merge: if labels.contains(&ACTIVE_LABEL) && base == "main" {
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

fn status(current: PullRequestSnapshot, others: Vec<PullRequestSnapshot>) -> StatusOutput {
    let current_number = current.number;
    let mut pull_requests = others;
    if !pull_requests
        .iter()
        .any(|item| item.number == current_number)
    {
        pull_requests.push(current);
    }
    let snapshot = crate::model::RepositorySnapshot {
        merge_candidates: Vec::new(),
        merge_candidates_truncated: 0,
        previous_default_oid: None,
        default_branch_movements: Vec::new(),
        repository: repository(),
        default_branch: branch("main"),
        current_branch: Some("current".to_owned()),
        current_pr: Some(current_number),
        pull_requests,
        observed_at: None,
    };
    let analysis = analyze(&snapshot, &clean).unwrap();
    StatusOutput {
        provider_api: crate::model::GitHubApiTelemetry::default(),
        merge_candidates: Vec::new(),
        merge_candidates_truncated: 0,
        previous_default_oid: None,
        default_branch_movements: Vec::new(),
        timing: None,
        repository: repository(),
        rebase_on_join: crate::read::RebaseOnJoinStatus::default(),
        auto_admission: crate::read::AutoAdmissionStatus::default(),
        default_branch: "main".to_owned(),
        current_branch: snapshot.current_branch,
        current_pr: snapshot.current_pr,
        healthy: analysis.healthy(),
        initialization: crate::initialization::InitializationStatus::default(),
        admission: crate::read::resolve_admission(
            &analysis,
            &crate::config::CaravanConfig::default().agent_priority_labels,
        ),
        analysis,
        pauses: Vec::new(),
    }
}

fn rebase_receipt(
    candidate: &PullRequestSnapshot,
    new_base: &BranchSnapshot,
) -> crate::physical_rebase::RebaseReceipt {
    crate::physical_rebase::RebaseReceipt {
        pr: candidate.number,
        branch: candidate.head.name.clone(),
        old_head_oid: candidate.head.oid.clone(),
        new_head_oid: candidate.head.oid.clone(),
        old_base_oid: candidate.base.oid.clone(),
        new_base_branch: new_base.name.clone(),
        new_base_oid: new_base.oid.clone(),
        new_tree_oid: CommitOid("tree-oid".to_owned()),
        commit_count: 1,
        merge_topology: None,
        ci_trigger_workflows: vec![".github/workflows/ci.yml".to_owned()],
        lease: format!(
            "--force-with-lease=refs/heads/{}:{}",
            candidate.head.name, candidate.head.oid
        ),
        already_satisfied: true,
    }
}

#[allow(clippy::unnecessary_wraps)]
fn clean(
    candidate: &BranchSnapshot,
    target: &BranchSnapshot,
) -> Result<CompatibilityReport, AppError> {
    Ok(CompatibilityReport {
        candidate: candidate.clone(),
        target: target.clone(),
        outcome: CompatibilityOutcome::Clean,
        conflicting_paths: Vec::new(),
        diagnostic: None,
    })
}

#[test]
fn join_failure_event_carries_target_fleet_and_error_code() {
    let head = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
    let candidate = pull_request(2, "two", "main", &[]);
    let status = status(candidate, vec![head]);
    let request = MembershipRequest {
        operation: MembershipOperation::Join,
        create_pr: false,
        tail_pr: Some(1),
        head_pr: None,
        reason: None,
        priority_label: None,
        agent_priority_labels: Vec::new(),
    };
    let error = AppError::validation("candidate_rejected", "cannot join");

    let event = join_failed_event(&status, &request, &error);

    assert_eq!(event.kind, EventKind::JoinFailed);
    assert_eq!(event.caravan_id, Some(PrNumber(1)));
    assert_eq!(event.prs, vec![PrNumber(1), PrNumber(2)]);
    assert_eq!(event.fleet, Some(status.analysis.fleet));
    assert_eq!(event.metadata["error_code"], "candidate_rejected");
}

#[test]
fn atomic_new_and_renew_accept_exact_default_without_a_join_tail() {
    for (operation, labels) in [
        (MembershipOperation::New, Vec::<&str>::new()),
        (MembershipOperation::Renew, vec![EVICTED_LABEL]),
    ] {
        let candidate = pull_request(2, "two", "main", &labels);
        let status = status(candidate.clone(), Vec::new());
        let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
        let rebase = rebase_receipt(&candidate, &status.analysis.fleet.default_branch);

        let output = execute_with_rebase_guard(
            status,
            &clean,
            &provider,
            MembershipRequest {
                operation,
                create_pr: false,
                tail_pr: None,
                head_pr: None,
                reason: None,
                priority_label: None,
                agent_priority_labels: Vec::new(),
            },
            Some(&rebase),
            false,
        )
        .expect("new/renew uses the exact default target and no join tail");

        assert!(output.pull_request.has_label(ACTIVE_LABEL));
        assert!(!output.pull_request.has_label(EVICTED_LABEL));
        assert_eq!(output.pull_request.auto_merge, AutoMergeState::squash());
        assert_eq!(output.pull_request.base, status_default(&rebase));
    }
}

#[test]
fn atomic_new_rejects_head_default_and_membership_races_before_mutation() {
    let candidate = pull_request(2, "two", "main", &[]);
    let initial = status(candidate.clone(), Vec::new());
    let rebase = rebase_receipt(&candidate, &initial.analysis.fleet.default_branch);

    let mut moved_head = candidate.clone();
    moved_head.head.oid = CommitOid("moved-head-oid".to_owned());
    let moved_provider = FakeProvider::with_pull_requests(vec![moved_head.clone()]);
    let error = execute_with_rebase_guard(
        status(moved_head.clone(), Vec::new()),
        &clean,
        &moved_provider,
        MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
            reason: None,
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
        Some(&rebase),
        false,
    )
    .expect_err("changed candidate head invalidates physical preflight");
    assert_eq!(error.code(), "new_target_moved_after_rebase");
    assert!(error.details().unwrap()["mutated_membership"] == false);
    assert_eq!(
        moved_provider.pull_requests.borrow()[&PrNumber(2)],
        moved_head
    );

    let mut moved_default = initial.clone();
    moved_default.analysis.fleet.default_branch.oid = CommitOid("new-main-oid".to_owned());
    let default_provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    let error = execute_with_rebase_guard(
        moved_default,
        &clean,
        &default_provider,
        MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
            reason: None,
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
        Some(&rebase),
        false,
    )
    .expect_err("changed default invalidates physical preflight");
    assert_eq!(error.code(), "new_target_moved_after_rebase");
    assert!(default_provider.pull_requests.borrow()[&PrNumber(2)] == candidate);

    let enrolled = pull_request(2, "two", "main", &[ACTIVE_LABEL]);
    let enrolled_provider = FakeProvider::with_pull_requests(vec![enrolled.clone()]);
    let error = execute_with_rebase_guard(
        status(enrolled.clone(), Vec::new()),
        &clean,
        &enrolled_provider,
        MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
            reason: None,
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
        Some(&rebase),
        false,
    )
    .expect_err("unexpected membership invalidates new admission");
    assert_eq!(error.code(), "new_target_moved_after_rebase");
    assert_eq!(
        error.details().unwrap()["candidate_caravan"]["id"],
        PrNumber(2).0
    );
    assert_eq!(
        enrolled_provider.pull_requests.borrow()[&PrNumber(2)],
        enrolled
    );
}

fn status_default(receipt: &crate::physical_rebase::RebaseReceipt) -> BranchSnapshot {
    BranchSnapshot {
        repository: repository(),
        name: receipt.new_base_branch.clone(),
        oid: receipt.new_base_oid.clone(),
    }
}

#[test]
fn atomic_join_rejects_live_tail_drift_after_physical_rebase() {
    let head = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
    let candidate = pull_request(2, "two", "main", &[]);
    let provider = FakeProvider::with_pull_requests(vec![head.clone(), candidate.clone()]);
    let rebase = crate::physical_rebase::RebaseReceipt {
        pr: candidate.number,
        branch: candidate.head.name.clone(),
        old_head_oid: candidate.head.oid.clone(),
        new_head_oid: candidate.head.oid.clone(),
        old_base_oid: candidate.base.oid.clone(),
        new_base_branch: "stale-tail".to_owned(),
        new_base_oid: crate::model::CommitOid("f".repeat(40)),
        new_tree_oid: crate::model::CommitOid("e".repeat(40)),
        commit_count: 1,
        merge_topology: None,
        ci_trigger_workflows: vec![".github/workflows/ci.yml".to_owned()],
        lease: format!(
            "--force-with-lease=refs/heads/{}:{}",
            candidate.head.name, candidate.head.oid
        ),
        already_satisfied: false,
    };

    let error = execute_with_rebase_guard(
        status(candidate.clone(), vec![head]),
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::Join,
            create_pr: false,
            tail_pr: Some(1),
            head_pr: None,
            reason: None,
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
        Some(&rebase),
        false,
    )
    .expect_err("tail drift must stop before admission");

    assert_eq!(error.code(), "join_target_moved_after_rebase");
    assert_eq!(provider.pull_requests.borrow()[&PrNumber(2)], candidate);
    assert_eq!(error.details().unwrap()["mutated_membership"], false);
}

#[test]
fn join_receipt_proves_exact_tail_ancestry_and_stale_force_removal() {
    let head = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
    let candidate = pull_request(2, "two", "main", &[FORCE_LABEL]);
    let before = status(candidate.clone(), vec![head.clone()]);
    let provider = FakeProvider::with_pull_requests(vec![head.clone(), candidate.clone()]);
    let output = execute(
        before.clone(),
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::Join,
            create_pr: false,
            tail_pr: Some(1),
            head_pr: None,
            reason: None,
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
    )
    .expect("join succeeds");
    let rebase = crate::physical_rebase::RebaseReceipt {
        pr: candidate.number,
        branch: candidate.head.name.clone(),
        old_head_oid: candidate.head.oid.clone(),
        new_head_oid: output.pull_request.head.oid.clone(),
        old_base_oid: candidate.base.oid.clone(),
        new_base_branch: head.head.name.clone(),
        new_base_oid: head.head.oid.clone(),
        new_tree_oid: crate::model::CommitOid("e".repeat(40)),
        commit_count: 1,
        merge_topology: None,
        ci_trigger_workflows: vec![".github/workflows/ci.yml".to_owned()],
        lease: format!(
            "--force-with-lease=refs/heads/{}:{}",
            candidate.head.name, candidate.head.oid
        ),
        already_satisfied: true,
    };
    let mut context = AppContext::default();
    context.config.rebase_on_join = true;
    let receipt = build_join_receipt(
        &context,
        &repository(),
        &before,
        JoinReceiptEvidence {
            predecessor: Some(JoinPredecessorReceipt {
                pr: head.number,
                branch: head.head.name.clone(),
                head_oid: head.head.oid.clone(),
            }),
            candidate_source_head_oid: Some(candidate.head.oid),
            default_branch_oid: before.analysis.fleet.default_branch.oid.clone(),
            rebase_receipt: Some(&rebase),
        },
        &output,
    )
    .unwrap();

    assert_eq!(
        receipt.force_intent,
        JoinForceIntent::RemovedStaleGeneration
    );
    assert!(receipt.ancestry_verified);
    assert!(receipt.membership_durable);
    assert_eq!(receipt.predecessor.pr, PrNumber(1));
    assert_eq!(receipt.result.base_oid, head.head.oid);
    assert!(receipt.config_fingerprint.starts_with("fnv1a64:"));
    assert!(receipt.receipt_hash.starts_with("fnv1a64:"));
    let mut unhashed = receipt.clone();
    let expected_hash = unhashed.receipt_hash.clone();
    unhashed.receipt_hash.clear();
    assert_eq!(
        fnv1a64(&serde_json::to_vec(&unhashed).unwrap()),
        expected_hash
    );
    assert!(receipt.provider_receipts.iter().any(|provider| {
        provider.kind == MutationKind::RemoveLabel
            && provider
                .before
                .as_ref()
                .is_some_and(|before| before.has_label(FORCE_LABEL))
    }));
}

#[test]
fn new_applies_active_label_and_squash_auto_merge() {
    let candidate = pull_request(1, "one", "main", &[]);
    let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    let output = execute(
        status(candidate, Vec::new()),
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
            reason: None,
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
    )
    .unwrap();

    assert!(output.pull_request.has_label(ACTIVE_LABEL));
    assert_eq!(output.pull_request.auto_merge, AutoMergeState::squash());
    assert_eq!(output.caravan_id, PrNumber(1));
    assert!(output.receipt.changed);
}

#[test]
fn explicit_membership_consumes_advisory_auto_admission_skip() {
    let candidate = pull_request(1, "one", "main", &[SKIPPED_LABEL]);
    let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    let output = execute(
        status(candidate, Vec::new()),
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
            reason: Some("manual override".to_owned()),
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
    )
    .unwrap();

    assert!(output.pull_request.has_label(ACTIVE_LABEL));
    assert!(!output.pull_request.has_label(SKIPPED_LABEL));
    assert!(output.receipt.completed_steps.iter().any(|step| {
        step.kind == MutationKind::RemoveLabel && step.state == MutationStepState::Completed
    }));
}

#[test]
fn join_infers_unique_tail_and_preserves_non_head_auto_merge_off() {
    let head = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
    let candidate = pull_request(2, "two", "main", &[]);
    let provider = FakeProvider::with_pull_requests(vec![head.clone(), candidate.clone()]);
    let output = execute(
        status(candidate, vec![head]),
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::Join,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
            reason: None,
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
    )
    .unwrap();

    assert_eq!(output.pull_request.base.name, "one");
    assert!(output.pull_request.has_label(ACTIVE_LABEL));
    assert!(!output.pull_request.auto_merge.enabled);
    assert_eq!(output.caravan_id, PrNumber(1));
}

#[test]
fn routine_join_consumes_stale_force_label_instead_of_carrying_bypass_intent() {
    let head = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
    let candidate = pull_request(2, "two", "main", &[FORCE_LABEL]);
    let provider = FakeProvider::with_pull_requests(vec![head.clone(), candidate.clone()]);

    let output = execute(
        status(candidate, vec![head]),
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::Join,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
            reason: None,
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
    )
    .expect("routine join removes unrelated force intent");

    assert!(!output.pull_request.has_label(FORCE_LABEL));
    assert!(output.provider_receipts.iter().any(|receipt| {
        receipt.kind == MutationKind::RemoveLabel
            && receipt
                .before
                .as_ref()
                .is_some_and(|before| before.has_label(FORCE_LABEL))
            && !receipt.after.has_label(FORCE_LABEL)
    }));
}

#[test]
fn unprotected_default_branch_fails_before_head_mutation() {
    let candidate = pull_request(1, "one", "main", &[]);
    let mut provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    provider.branch_protected = false;
    let error = execute(
        status(candidate, Vec::new()),
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
            reason: None,
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
    )
    .unwrap_err();

    assert_eq!(
        mcp_cli::StructuredError::code(&error),
        "default_branch_not_protected"
    );
    assert!(
        !provider
            .pull_requests
            .borrow()
            .get(&PrNumber(1))
            .unwrap()
            .has_label(ACTIVE_LABEL)
    );
}

#[test]
fn disabled_repository_auto_merge_fails_before_head_mutation() {
    let candidate = pull_request(1, "one", "main", &[]);
    let mut provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    provider.allows_auto_merge = false;
    let error = execute(
        status(candidate, Vec::new()),
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
            reason: None,
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
    )
    .unwrap_err();

    assert_eq!(
        mcp_cli::StructuredError::code(&error),
        "auto_merge_not_enabled"
    );
    assert!(
        !provider
            .pull_requests
            .borrow()
            .get(&PrNumber(1))
            .unwrap()
            .has_label(ACTIVE_LABEL)
    );
}

#[test]
fn missing_labels_fail_before_provider_mutation() {
    let candidate = pull_request(1, "one", "main", &[]);
    let mut provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    provider.labels.remove(FORCE_LABEL);
    let error = execute(
        status(candidate, Vec::new()),
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
            reason: None,
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
    )
    .unwrap_err();

    assert_eq!(
        mcp_cli::StructuredError::code(&error),
        "required_labels_missing"
    );
    assert!(
        !provider
            .pull_requests
            .borrow()
            .get(&PrNumber(1))
            .unwrap()
            .has_label(ACTIVE_LABEL)
    );
}

#[test]
fn partial_failure_reports_receipts_and_rerun_resumes() {
    let candidate = pull_request(1, "one", "main", &[]);
    let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    *provider.fail_kind.borrow_mut() = Some(MutationKind::EnableAutoMerge);
    let request = MembershipRequest {
        operation: MembershipOperation::New,
        create_pr: false,
        tail_pr: None,
        head_pr: None,
        reason: None,
        priority_label: None,
        agent_priority_labels: Vec::new(),
    };
    let error = execute(
        status(candidate, Vec::new()),
        &clean,
        &provider,
        request.clone(),
    )
    .unwrap_err();
    let details = mcp_cli::StructuredError::details(&error).unwrap();
    assert!(
        details["provider_receipts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|receipt| {
                receipt["kind"] == serde_json::Value::String("add_label".to_owned())
            })
    );

    *provider.fail_kind.borrow_mut() = None;
    let partial = provider
        .pull_requests
        .borrow()
        .get(&PrNumber(1))
        .unwrap()
        .clone();
    let output = execute(status(partial, Vec::new()), &clean, &provider, request).unwrap();
    assert_eq!(output.pull_request.auto_merge, AutoMergeState::squash());
}

#[test]
fn explicit_membership_reason_must_not_be_whitespace() {
    let candidate = pull_request(1, "one", "main", &[]);
    let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    let error = execute(
        status(candidate, Vec::new()),
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
            reason: Some("  \n".to_owned()),
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
    )
    .unwrap_err();

    assert_eq!(error.code(), "membership_reason_empty");
    assert!(!provider.pull_requests.borrow()[&PrNumber(1)].has_label(ACTIVE_LABEL));
}

#[test]
fn comment_failure_is_a_resumable_partial_label_mutation() {
    let candidate = pull_request(1, "one", "main", &[]);
    let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    *provider.fail_kind.borrow_mut() = Some(MutationKind::Comment);
    let error = execute(
        status(candidate, Vec::new()),
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
            reason: Some("operator admission".to_owned()),
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
    )
    .unwrap_err();

    assert_eq!(error.code(), "github_comment_failed");
    let details = error.details().unwrap();
    assert_eq!(details["stage"], "control_label_comment");
    assert_eq!(details["resumable"], true);
    assert!(
        details["completed_steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|step| step["kind"] == "add_label")
    );
}

#[test]
fn explicit_priority_applies_configured_control_label() {
    let candidate = pull_request(1, "one", "main", &[]);
    let mut provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    provider.labels.insert("caravan-priority:high".to_owned());
    let output = execute(
        status(candidate, Vec::new()),
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
            reason: None,
            priority_label: Some("caravan-priority:high".to_owned()),
            agent_priority_labels: vec!["caravan-priority:high".to_owned()],
        },
    )
    .unwrap();

    assert!(output.pull_request.has_label("caravan-priority:high"));
    assert!(
        output
            .receipt
            .completed_steps
            .iter()
            .any(|step| step.kind == MutationKind::Comment)
    );
}

#[test]
fn mutation_timeout_preserves_timeout_category_and_partial_receipts() {
    let mut state = ExecutionState::new(MembershipOperation::Join);
    state.steps.push(MutationStep {
        kind: MutationKind::AddLabel,
        state: MutationStepState::Completed,
        pr: Some(PrNumber(2)),
        summary: "label added".to_owned(),
    });
    let error = mutation_error(
        &MutationError::Provider(DiscoveryError::Runner(CommandRunError::Timeout {
            command: crate::command::CommandSpec::new("gh").args(["pr", "edit"]),
            process_group_id: None,
            timeout_ms: 900,
            stdout: "partial".to_owned(),
            stderr: "stalled".to_owned(),
        })),
        &state,
    );

    assert_eq!(
        mcp_cli::StructuredError::category(&error),
        ErrorCategory::Timeout
    );
    assert_eq!(
        mcp_cli::StructuredError::code(&error),
        "github_mutation_timeout"
    );
    let details = mcp_cli::StructuredError::details(&error).unwrap();
    assert_eq!(details["stage"], "github_mutation");
    assert_eq!(details["timeout_ms"], 900);
    assert_eq!(details["completed_steps"][0]["summary"], "label added");
}

#[test]
fn rejoin_removes_evicted_and_force_after_full_preflight() {
    let head = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
    let candidate = pull_request(2, "two", "main", &[EVICTED_LABEL, FORCE_LABEL]);
    let provider = FakeProvider::with_pull_requests(vec![head.clone(), candidate.clone()]);
    let output = execute(
        status(candidate, vec![head]),
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::Rejoin,
            create_pr: false,
            tail_pr: Some(1),
            head_pr: None,
            reason: None,
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
    )
    .unwrap();

    assert!(output.pull_request.has_label(ACTIVE_LABEL));
    assert!(!output.pull_request.has_label(EVICTED_LABEL));
    assert!(!output.pull_request.has_label(FORCE_LABEL));
    assert_eq!(output.pull_request.base.name, "one");
}
