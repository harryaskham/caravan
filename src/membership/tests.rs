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
    generation_facts: RefCell<Vec<crate::model::PullRequestGenerationFact>>,
    generation_relations:
        RefCell<BTreeMap<(CommitOid, CommitOid), crate::generation::CommitRelation>>,
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
            generation_facts: RefCell::new(Vec::new()),
            generation_relations: RefCell::new(BTreeMap::new()),
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
        if !actual.mutation_identity_eq(expected) {
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
    fn open_generation_facts(
        &self,
        _repository: &RepositoryId,
    ) -> Result<Vec<crate::model::PullRequestGenerationFact>, MutationError> {
        Ok(self.generation_facts.borrow().clone())
    }

    fn compare_generation_commits(
        &self,
        _repository: &RepositoryId,
        base: &CommitOid,
        head: &CommitOid,
    ) -> Result<crate::generation::CommitRelation, MutationError> {
        Ok(self
            .generation_relations
            .borrow()
            .get(&(base.clone(), head.clone()))
            .cloned()
            .unwrap_or_else(|| crate::generation::CommitRelation::Unknown {
                reason: "unconfigured fixture relation".to_owned(),
            }))
    }

    fn verify_branch_head(
        &self,
        _repository: &RepositoryId,
        _branch: &str,
        _expected: &CommitOid,
    ) -> Result<(), MutationError> {
        Ok(())
    }

    fn refetch_pull_request(
        &self,
        _repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<PullRequestSnapshot, MutationError> {
        Ok(self.pull_requests.borrow()[&number].clone())
    }

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

fn git(directory: &std::path::Path, arguments: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(directory)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
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
        merge_state_status: None,
        number: PrNumber(number),
        title: format!("PR {number}"),
        url: format!("https://example.invalid/{number}"),
        state: PullRequestState::Open,
        draft: false,
        head: branch(head),
        base: branch(base),
        cross_repository: false,
        labels: labels.iter().map(|label| (*label).to_owned()).collect(),
        // Absent configuration keeps the historical provider-native actor, so
        // a root in this fixture is armed exactly as an existing fleet is.
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

fn generation_fact(
    pr: u64,
    agent: &str,
    bead: &str,
    source: char,
    created_at: &str,
) -> crate::model::PullRequestGenerationFact {
    let source_head = CommitOid(source.to_string().repeat(40));
    crate::model::PullRequestGenerationFact {
        pr: PrNumber(pr),
        provider_head: CommitOid(format!("provider-{pr}")),
        created_at: Some(created_at.to_owned()),
        provenance: Some(crate::model::CacophonyGenerationProvenance {
            generation: format!("agent/{agent}-pr-g{}", source_head.0),
            agent: agent.to_owned(),
            source_head,
            bead_ids: BTreeSet::from([bead.to_owned()]),
            stack_base: "main".to_owned(),
            stack_state: "root".to_owned(),
        }),
        metadata_error: None,
        supersedes: BTreeSet::new(),
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
        generation_facts: Vec::new(),
        observed_at: None,
    };
    let analysis = analyze(&snapshot, &clean).unwrap();
    StatusOutput {
        config_provenance: None,
        head_merge: crate::read::HeadMergeStatus::default(),
        runtime: crate::read::RuntimeProvenance::default(),
        provider_api: crate::model::GitHubApiTelemetry::default(),
        merge_candidates: Vec::new(),
        merge_candidates_truncated: 0,
        previous_default_oid: None,
        default_branch_movements: Vec::new(),
        timing: None,
        repository: repository(),
        rebase_on_join: crate::read::RebaseOnJoinStatus::default(),
        stack_backend: crate::read::StackBackendStatus::default(),
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
        sync_budget: crate::sync::SyncBudgetStatus::default(),
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
        squash_reconciliation: None,
        ci_trigger_workflows: vec![".github/workflows/ci.yml".to_owned()],
        lease: format!(
            "--force-with-lease=refs/heads/{}:{}",
            candidate.head.name, candidate.head.oid
        ),
        already_satisfied: true,
        rewrite_reason: crate::physical_rebase::BranchRewriteReason::Unspecified,
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

/// bd-84f82d: the post-rewrite reserve must be priced against a deadline that
/// can actually hold it.
///
/// `require_post_rewrite_budget` requires `POST_REWRITE_COMMAND_RESERVE * timeout`
/// (four commands). The exact-candidate binding returned its OWN one-command
/// window and that was assigned back over the operation deadline, so the guard
/// measured four against one and could never pass. No configuration value fixes
/// that: raising `command_timeout_secs` scales both sides and the ratio stays 4:1.
///
/// The candidate share must therefore be at least the reserve, and still bounded
/// so one candidate cannot consume the tick.
#[test]
fn a_candidate_share_can_hold_the_reserve_it_will_be_asked_for() {
    let mut context = AppContext::default();
    context.config.command_timeout_secs = 60;
    let timeout = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let reserve = timeout * super::POST_REWRITE_COMMAND_RESERVE;

    // A generous tick: the candidate share must leave room for the reserve.
    let tick = std::time::Instant::now() + std::time::Duration::from_secs(3600);
    let binding_read = std::time::Instant::now() + timeout;
    let share = super::candidate_operation_deadline(&context, tick, binding_read);
    assert!(
        share.saturating_duration_since(std::time::Instant::now()) >= reserve,
        "the share must hold the reserve the guard will demand, or the rewrite path is unreachable"
    );
    assert!(
        super::require_post_rewrite_budget(&context, Some(share), PrNumber(2317)).is_ok(),
        "the guard must pass on a healthy tick"
    );

    // The single-command window that used to be assigned here cannot hold it.
    let one_command = std::time::Instant::now() + timeout;
    assert!(
        super::require_post_rewrite_budget(&context, Some(one_command), PrNumber(2317)).is_err(),
        "four commands against one is the defect: this must still refuse"
    );

    // And a candidate must not swallow the whole tick.
    assert!(
        share < tick,
        "the share must remain bounded below the tick deadline"
    );
}

/// bd-10303b: exercise the complete post-rewrite half of a fresh join under
/// the same candidate-scoped operation deadline. The arithmetic-only guard test
/// above would stay green if rediscovery accidentally collapsed the deadline
/// back to one command or if membership writes no longer consumed the freshly
/// rediscovered rewritten head.
#[test]
fn rewrite_required_join_rediscovery_and_membership_share_one_operation_budget() {
    let mut context = AppContext::default();
    context.config.command_timeout_secs = 1;
    context.config.rebase_on_join = true;
    context.config.sync.actions.join_unlabelled_prs = true;

    let tail = pull_request(1, "tail", "main", &[ACTIVE_LABEL]);
    let fresh = pull_request(2, "candidate", "main", &[]);
    let initial_status = status(fresh.clone(), vec![tail.clone()]);
    assert_eq!(fresh.base.name, "main");
    assert_ne!(
        fresh.base.name, tail.head.name,
        "the join requires a rewrite"
    );

    // Candidate binding receives a bounded share of the sync tick. The
    // post-push rediscovery must retain that exact operation deadline rather
    // than replacing it with its own one-command read deadline.
    let tick_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let binding_read_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let operation_deadline =
        super::candidate_operation_deadline(&context, tick_deadline, binding_read_deadline);
    assert!(
        operation_deadline > binding_read_deadline,
        "the one-command binding deadline must not replace the operation budget"
    );
    super::require_post_rewrite_budget(&context, Some(operation_deadline), fresh.number)
        .expect("fresh rewrite has enough budget before the irreversible push");

    // Model the authoritative provider rediscovery after the force-with-lease
    // push: the head changed, while membership base/label writes have not run.
    let mut rediscovered = fresh.clone();
    rediscovered.head.oid = CommitOid("rewritten-candidate-head".to_owned());
    let post_rewrite_status = status(rediscovered.clone(), vec![tail.clone()]);
    let rediscovery_read_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let post_rediscovery_deadline = super::candidate_operation_deadline(
        &context,
        operation_deadline,
        rediscovery_read_deadline,
    );
    assert_eq!(
        post_rediscovery_deadline, operation_deadline,
        "rediscovery must retain the candidate operation budget"
    );
    super::require_post_rewrite_budget(
        &context,
        Some(post_rediscovery_deadline),
        rediscovered.number,
    )
    .expect("mandatory membership writes still fit after rediscovery");

    let rebase = crate::physical_rebase::RebaseReceipt {
        pr: fresh.number,
        branch: fresh.head.name.clone(),
        old_head_oid: fresh.head.oid.clone(),
        new_head_oid: rediscovered.head.oid.clone(),
        old_base_oid: fresh.base.oid.clone(),
        new_base_branch: tail.head.name.clone(),
        new_base_oid: tail.head.oid.clone(),
        new_tree_oid: CommitOid("rewritten-result-tree".to_owned()),
        commit_count: 1,
        merge_topology: None,
        squash_reconciliation: None,
        ci_trigger_workflows: vec![".github/workflows/ci.yml".to_owned()],
        lease: format!(
            "--force-with-lease=refs/heads/{}:{}",
            fresh.head.name, fresh.head.oid
        ),
        already_satisfied: false,
        rewrite_reason: crate::physical_rebase::BranchRewriteReason::Unspecified,
    };
    let mut provider = FakeProvider::with_pull_requests(vec![tail.clone(), rediscovered]);
    provider.labels.insert(SKIPPED_LABEL.to_owned());
    let output = execute_with_rebase_guard(
        post_rewrite_status,
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::Join,
            create_pr: false,
            tail_pr: Some(tail.number.0),
            head_pr: None,
            reason: Some("automatic tail re-formation regression".to_owned()),
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
        Some(&rebase),
        true,
    )
    .expect("rewritten candidate completes membership in the same operation");

    assert!(std::time::Instant::now() < post_rediscovery_deadline);
    assert_eq!(output.pull_request.base.name, tail.head.name);
    assert_eq!(
        output.pull_request.base.oid,
        CommitOid(format!("{}-oid", tail.head.name))
    );
    assert!(output.pull_request.has_label(ACTIVE_LABEL));
    assert_eq!(output.pull_request.auto_merge, AutoMergeState::disabled());
    assert!(output.provider_receipts.iter().any(|receipt| {
        receipt.kind == MutationKind::SetBase && receipt.after.base.name == tail.head.name
    }));
    assert!(output.provider_receipts.iter().any(|receipt| {
        receipt.kind == MutationKind::AddLabel && receipt.after.has_label(ACTIVE_LABEL)
    }));
    assert_eq!(output.pull_request.head.oid, rebase.new_head_oid);
    assert_eq!(initial_status.current_pr, Some(fresh.number));
}

/// bd-4e4615: an irreversible branch rewrite must not start without budget for
/// the mandatory post-rewrite rediscovery and membership writes.
#[test]
fn post_rewrite_budget_is_reserved_before_any_branch_rewrite() {
    let mut context = AppContext::default();
    context.config.command_timeout_secs = 30;

    // Almost no budget left: refuse before touching the remote.
    let exhausted = std::time::Instant::now() + std::time::Duration::from_millis(212);
    let error = super::require_post_rewrite_budget(&context, Some(exhausted), PrNumber(2079))
        .expect_err("an exhausted budget must refuse before mutation");
    assert_eq!(
        mcp_cli::StructuredError::code(&error),
        "membership_post_rewrite_budget_insufficient"
    );
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(details["mutated"], false);
    assert_eq!(details["pr"], 2079);

    // The reserve is a MULTIPLE of `command_timeout_secs`, so the remedy must not
    // tell the reader to raise it. An operator read the old advice and proposed
    // 60 -> 300, which takes the requirement from 240s to 1200s: advice that
    // steers deeper into the fault it is describing (bd-cef42d).
    let remedy = details["safe_next_action"]
        .as_str()
        .expect("safe_next_action is a string");
    assert!(
        remedy.contains("LOWER command_timeout_secs"),
        "the remedy must name the direction that reduces the reserve: {remedy}"
    );
    assert!(
        !remedy.contains("raise command_timeout_secs"),
        "raising it increases this very requirement: {remedy}"
    );
    assert!(
        remedy.contains("sync.max_duration_secs"),
        "the other real lever is the operation deadline: {remedy}"
    );
    assert_eq!(
        details["required_ms"],
        u64::from(super::POST_REWRITE_COMMAND_RESERVE) * 30_000,
        "required is exactly the reserve times the per-command timeout"
    );

    // Ample budget proceeds, and an unbounded operation is unaffected.
    let ample = std::time::Instant::now() + std::time::Duration::from_secs(600);
    assert!(super::require_post_rewrite_budget(&context, Some(ample), PrNumber(2079)).is_ok());
    assert!(super::require_post_rewrite_budget(&context, None, PrNumber(2079)).is_ok());
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
fn root_admission_never_treats_default_branch_as_empty_source() {
    let candidate = pull_request(2, "two", "main", &[]);
    let mut discovered = status(candidate, Vec::new());
    discovered.current_pr = None;
    discovered.current_branch = Some("main".to_owned());
    let mut request = MembershipRequest {
        operation: MembershipOperation::New,
        create_pr: false,
        tail_pr: None,
        head_pr: None,
        reason: Some("Saloon admission".to_owned()),
        priority_label: None,
        agent_priority_labels: Vec::new(),
    };

    let missing = validate_membership_source_request(&discovered, &request).unwrap_err();
    assert_eq!(missing.code(), "current_pr_not_found");
    assert_eq!(missing.details().unwrap()["mutated"], false);

    request.create_pr = true;
    let default = validate_membership_source_request(&discovered, &request).unwrap_err();
    assert_eq!(default.code(), "create_pr_on_default_branch");
    assert_eq!(default.details().unwrap()["mutated"], false);
}

#[test]
fn join_refuses_stale_root_before_any_provider_mutation() {
    let mut root = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
    root.base.oid = CommitOid("stale-main".to_owned());
    let candidate = pull_request(2, "two", "main", &[]);
    let status = status(candidate.clone(), vec![root]);
    let request = MembershipRequest {
        operation: MembershipOperation::Join,
        create_pr: false,
        tail_pr: Some(1),
        head_pr: None,
        reason: Some("exact stale-root fixture".to_owned()),
        priority_label: None,
        agent_priority_labels: Vec::new(),
    };
    let target = resolve_join_target(&status, &request).unwrap();

    let error = require_current_join_root(&status, &target).unwrap_err();

    assert_eq!(error.code(), "join_root_stale_default");
    let details = error.details().unwrap();
    assert_eq!(details["mutated"], false);
    assert_eq!(details["root_pr"], 1);
    let event = join_failed_event(&status, &request, &error);
    assert_eq!(event.metadata["error_code"], "join_root_stale_default");
    assert_eq!(event.metadata["error_details"]["mutated"], false);
}

#[test]
fn join_root_check_progress_does_not_stale_mutation_identity() {
    let mut root = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
    root.checks = vec![crate::model::CheckSnapshot {
        name: "Changed surface admission".to_owned(),
        state: crate::model::CheckState::Queued,
        provider_state: Some("QUEUED".to_owned()),
        details_url: None,
        ..crate::model::CheckSnapshot::default()
    }];
    let candidate = pull_request(2, "two", "main", &[]);
    let status = status(candidate.clone(), vec![root.clone()]);
    let request = MembershipRequest {
        operation: MembershipOperation::Join,
        create_pr: false,
        tail_pr: Some(1),
        head_pr: None,
        reason: Some("check progress fixture".to_owned()),
        priority_label: None,
        agent_priority_labels: Vec::new(),
    };
    let target = resolve_join_target(&status, &request).unwrap();
    root.checks[0].state = crate::model::CheckState::InProgress;
    root.checks[0].provider_state = Some("IN_PROGRESS".to_owned());
    let provider = FakeProvider::with_pull_requests(vec![root, candidate]);

    revalidate_join_root(&status, &target, &provider)
        .expect("check-only churn is not mutation-authority drift");
}

#[test]
fn join_root_drift_after_preview_fails_before_provider_mutation() {
    let root = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
    let candidate = pull_request(2, "two", "main", &[]);
    let status = status(candidate.clone(), vec![root.clone()]);
    let request = MembershipRequest {
        operation: MembershipOperation::Join,
        create_pr: false,
        tail_pr: Some(1),
        head_pr: None,
        reason: Some("root race fixture".to_owned()),
        priority_label: None,
        agent_priority_labels: Vec::new(),
    };
    let target = resolve_join_target(&status, &request).unwrap();
    let mut moved_root = root;
    moved_root.head.oid = CommitOid("moved-root".to_owned());
    let provider = FakeProvider::with_pull_requests(vec![moved_root, candidate]);
    let provider_before = provider.pull_requests.borrow().clone();

    let error = revalidate_join_root(&status, &target, &provider).unwrap_err();

    assert_eq!(error.code(), "join_root_moved_before_apply");
    let details = error.details().unwrap();
    assert_eq!(details["mutated"], false);
    assert_eq!(details["retryable"], true);
    assert_eq!(details["changed_fields"], json!(["head"]));
    assert!(details["expected"].get("checks").is_none());
    assert!(details["actual"].get("checks").is_none());
    assert_eq!(*provider.pull_requests.borrow(), provider_before);
}

#[test]
fn empty_source_join_is_zero_mutation_with_exact_receipt() {
    let temporary = tempfile::tempdir().unwrap();
    let remote = temporary.path().join("remote.git");
    let work = temporary.path().join("work");
    git(
        temporary.path(),
        &["init", "--bare", remote.to_str().unwrap()],
    );
    git(
        temporary.path(),
        &["clone", remote.to_str().unwrap(), work.to_str().unwrap()],
    );
    git(&work, &["config", "user.name", "Caravan Test"]);
    git(&work, &["config", "user.email", "caravan@example.invalid"]);
    std::fs::write(work.join("base.txt"), "base\n").unwrap();
    git(&work, &["add", "base.txt"]);
    git(&work, &["commit", "-m", "base"]);
    git(&work, &["branch", "-M", "main"]);
    git(&work, &["push", "-u", "origin", "main"]);
    let main_oid = CommitOid(git(&work, &["rev-parse", "HEAD"]));

    git(&work, &["checkout", "-b", "tail"]);
    std::fs::write(work.join("tail.txt"), "tail\n").unwrap();
    git(&work, &["add", "tail.txt"]);
    git(&work, &["commit", "-m", "tail"]);
    git(&work, &["push", "-u", "origin", "tail"]);
    let tail_oid = CommitOid(git(&work, &["rev-parse", "HEAD"]));

    git(&work, &["checkout", "main"]);
    git(&work, &["checkout", "-b", "empty-source"]);
    git(&work, &["commit", "--allow-empty", "-m", "empty source"]);
    git(&work, &["push", "-u", "origin", "empty-source"]);
    let source_oid_value = CommitOid(git(&work, &["rev-parse", "HEAD"]));

    let mut root = pull_request(1, "tail", "main", &[ACTIVE_LABEL]);
    root.head.oid.clone_from(&tail_oid);
    root.base.oid.clone_from(&main_oid);
    let mut candidate = pull_request(2, "empty-source", "main", &[]);
    candidate.head.oid.clone_from(&source_oid_value);
    candidate.base.oid.clone_from(&main_oid);
    let mut discovered = status(candidate.clone(), vec![root]);
    discovered
        .analysis
        .fleet
        .default_branch
        .oid
        .clone_from(&main_oid);
    let provider = FakeProvider::with_pull_requests(vec![candidate]);
    let provider_before = provider.pull_requests.borrow().clone();
    let config = crate::config::CaravanConfig {
        command_timeout_secs: 30,
        ..crate::config::CaravanConfig::default()
    };
    let context = AppContext {
        repository_path: work,
        config,
        ..AppContext::default()
    };
    let predecessor = JoinPredecessorReceipt {
        pr: PrNumber(1),
        branch: "tail".to_owned(),
        head_oid: tail_oid,
    };
    let source = BranchSnapshot {
        repository: repository(),
        name: "empty-source".to_owned(),
        oid: source_oid_value,
    };

    let error = preflight_join_source(
        &context,
        &provider,
        &discovered,
        &source,
        &predecessor,
        std::time::Instant::now() + std::time::Duration::from_secs(30),
    )
    .unwrap_err();

    assert_eq!(error.code(), "join_empty_source_noop");
    let details = error.details().unwrap();
    assert_eq!(details["mutated"], false);
    assert_eq!(details["noop"], true);
    assert_eq!(details["source"]["branch"], "empty-source");
    assert_eq!(details["source"]["selected_tail"]["branch"], "tail");
    let event = join_failed_event(
        &discovered,
        &MembershipRequest {
            operation: MembershipOperation::Join,
            create_pr: false,
            tail_pr: Some(1),
            head_pr: None,
            reason: Some("empty source fixture".to_owned()),
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
        &error,
    );
    assert_eq!(
        event.metadata["error_details"]["source"]["patch_fingerprint"],
        details["source"]["patch_fingerprint"]
    );
    assert_eq!(*provider.pull_requests.borrow(), provider_before);
}

#[test]
#[allow(clippy::too_many_lines)]
fn patch_already_landed_under_distinct_oid_is_zero_mutation_noop() {
    let temporary = tempfile::tempdir().unwrap();
    let remote = temporary.path().join("remote.git");
    let work = temporary.path().join("work");
    git(
        temporary.path(),
        &["init", "--bare", remote.to_str().unwrap()],
    );
    git(
        temporary.path(),
        &["clone", remote.to_str().unwrap(), work.to_str().unwrap()],
    );
    git(&work, &["config", "user.name", "Caravan Test"]);
    git(&work, &["config", "user.email", "caravan@example.invalid"]);
    std::fs::write(work.join("base.txt"), "base\n").unwrap();
    git(&work, &["add", "base.txt"]);
    git(&work, &["commit", "-m", "base"]);
    git(&work, &["branch", "-M", "main"]);
    git(&work, &["push", "-u", "origin", "main"]);

    git(&work, &["checkout", "-b", "source"]);
    std::fs::write(work.join("release.txt"), "same release patch\n").unwrap();
    git(&work, &["add", "release.txt"]);
    git(&work, &["commit", "-m", "source release"]);
    git(&work, &["push", "-u", "origin", "source"]);
    let source_oid_value = CommitOid(git(&work, &["rev-parse", "HEAD"]));

    git(&work, &["checkout", "main"]);
    std::fs::write(work.join("release.txt"), "same release patch\n").unwrap();
    git(&work, &["add", "release.txt"]);
    git(&work, &["commit", "-m", "independent main release"]);
    git(&work, &["push", "origin", "main"]);
    let main_oid = CommitOid(git(&work, &["rev-parse", "HEAD"]));
    git(&work, &["checkout", "-b", "tail"]);
    std::fs::write(work.join("tail.txt"), "tail\n").unwrap();
    git(&work, &["add", "tail.txt"]);
    git(&work, &["commit", "-m", "tail"]);
    git(&work, &["push", "-u", "origin", "tail"]);
    let tail_oid = CommitOid(git(&work, &["rev-parse", "HEAD"]));

    let mut root = pull_request(1, "tail", "main", &[ACTIVE_LABEL]);
    root.head.oid.clone_from(&tail_oid);
    root.base.oid.clone_from(&main_oid);
    let mut candidate = pull_request(2, "source", "main", &[]);
    candidate.head.oid.clone_from(&source_oid_value);
    candidate.base.oid.clone_from(&main_oid);
    let mut discovered = status(candidate.clone(), vec![root]);
    discovered
        .analysis
        .fleet
        .default_branch
        .oid
        .clone_from(&main_oid);
    let provider = FakeProvider::with_pull_requests(vec![candidate]);
    let provider_before = provider.pull_requests.borrow().clone();
    let context = AppContext {
        repository_path: work,
        config: crate::config::CaravanConfig {
            command_timeout_secs: 30,
            ..crate::config::CaravanConfig::default()
        },
        ..AppContext::default()
    };
    let predecessor = JoinPredecessorReceipt {
        pr: PrNumber(1),
        branch: "tail".to_owned(),
        head_oid: tail_oid,
    };
    let source = BranchSnapshot {
        repository: repository(),
        name: "source".to_owned(),
        oid: source_oid_value,
    };

    let error = preflight_join_source(
        &context,
        &provider,
        &discovered,
        &source,
        &predecessor,
        std::time::Instant::now() + std::time::Duration::from_secs(30),
    )
    .unwrap_err();

    assert_eq!(error.code(), "join_empty_source_noop");
    let details = error.details().unwrap();
    assert_eq!(details["mutated"], false);
    assert_eq!(
        details["source"]["source_commits"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        details["source"]["already_landed_commits"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(*provider.pull_requests.borrow(), provider_before);
}

#[test]
#[allow(clippy::too_many_lines)]
fn source_only_plan_excludes_release_already_on_current_main() {
    let temporary = tempfile::tempdir().unwrap();
    let remote = temporary.path().join("remote.git");
    let work = temporary.path().join("work");
    git(
        temporary.path(),
        &["init", "--bare", remote.to_str().unwrap()],
    );
    git(
        temporary.path(),
        &["clone", remote.to_str().unwrap(), work.to_str().unwrap()],
    );
    git(&work, &["config", "user.name", "Caravan Test"]);
    git(&work, &["config", "user.email", "caravan@example.invalid"]);
    std::fs::create_dir_all(work.join(".github/workflows")).unwrap();
    std::fs::write(work.join("base.txt"), "base\n").unwrap();
    std::fs::write(
        work.join(".github/workflows/ci.yml"),
        "name: CI\non:\n  pull_request:\n    types: [opened, synchronize, reopened, edited, labeled, unlabeled]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n",
    )
    .unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-m", "base"]);
    git(&work, &["branch", "-M", "main"]);
    git(&work, &["push", "-u", "origin", "main"]);
    let base_oid = CommitOid(git(&work, &["rev-parse", "HEAD"]));

    git(&work, &["checkout", "-b", "source"]);
    std::fs::write(work.join("release.txt"), "already on main\n").unwrap();
    git(&work, &["add", "release.txt"]);
    git(&work, &["commit", "-m", "source copy of release"]);
    std::fs::write(work.join("source.txt"), "source-only\n").unwrap();
    git(&work, &["add", "source.txt"]);
    git(&work, &["commit", "-m", "source-only change"]);
    git(&work, &["push", "-u", "origin", "source"]);
    let source_oid_value = CommitOid(git(&work, &["rev-parse", "HEAD"]));

    git(&work, &["checkout", "main"]);
    std::fs::write(work.join("release.txt"), "already on main\n").unwrap();
    git(&work, &["add", "release.txt"]);
    git(&work, &["commit", "-m", "release already landed"]);
    git(&work, &["push", "origin", "main"]);
    let current_main_oid = CommitOid(git(&work, &["rev-parse", "HEAD"]));

    git(&work, &["checkout", "-b", "tail"]);
    std::fs::write(work.join("tail.txt"), "tail\n").unwrap();
    git(&work, &["add", "tail.txt"]);
    git(&work, &["commit", "-m", "tail"]);
    git(&work, &["push", "-u", "origin", "tail"]);
    let tail_oid = CommitOid(git(&work, &["rev-parse", "HEAD"]));

    let mut root = pull_request(1, "tail", "main", &[ACTIVE_LABEL]);
    root.head.oid.clone_from(&tail_oid);
    root.base.oid.clone_from(&current_main_oid);
    let mut candidate = pull_request(2, "source", "main", &[]);
    candidate.head.oid.clone_from(&source_oid_value);
    candidate.base.oid.clone_from(&current_main_oid);
    let mut discovered = status(candidate.clone(), vec![root]);
    discovered
        .analysis
        .fleet
        .default_branch
        .oid
        .clone_from(&current_main_oid);
    let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    let provider_before = provider.pull_requests.borrow().clone();
    let config = crate::config::CaravanConfig {
        command_timeout_secs: 30,
        ..crate::config::CaravanConfig::default()
    };
    let context = AppContext {
        repository_path: work.clone(),
        config,
        ..AppContext::default()
    };
    let predecessor = JoinPredecessorReceipt {
        pr: PrNumber(1),
        branch: "tail".to_owned(),
        head_oid: tail_oid.clone(),
    };
    let source = BranchSnapshot {
        repository: repository(),
        name: "source".to_owned(),
        oid: source_oid_value,
    };
    let receipt = preflight_join_source(
        &context,
        &provider,
        &discovered,
        &source,
        &predecessor,
        std::time::Instant::now() + std::time::Duration::from_secs(30),
    )
    .unwrap();

    assert_eq!(receipt.parent.oid, base_oid);
    assert_eq!(receipt.source_title, "source-only change");
    assert_eq!(receipt.source_commits.len(), 2);
    assert_eq!(receipt.already_landed_commits.len(), 1);
    assert_ne!(
        receipt.patch_fingerprint,
        receipt.effective_patch_fingerprint
    );
    let mut planning_candidate = candidate;
    planning_candidate.base.clone_from(&receipt.parent);
    let prepared = crate::physical_rebase::prepare_candidate(
        &work,
        &repository(),
        &planning_candidate,
        crate::physical_rebase::PlannedRangeBase::HistoricalSourceBranch {
            branch: receipt.parent.clone(),
            current: discovered.analysis.fleet.default_branch.clone(),
        },
        crate::physical_rebase::PlannedBase::Remote(BranchSnapshot {
            repository: repository(),
            name: "tail".to_owned(),
            oid: tail_oid,
        }),
        &discovered.analysis.fleet.default_branch,
        crate::physical_rebase::RebaseExecutionBudget::new(std::time::Duration::from_secs(30)),
    )
    .unwrap();

    assert_eq!(prepared.plan.new_tree_oid, receipt.expected_result_tree_oid);
    assert_eq!(prepared.plan.old_base_oid, receipt.parent.oid);
    let files = git(
        &work,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            prepared.plan.new_tree_oid.0.as_str(),
        ],
    );
    for expected in ["base.txt", "release.txt", "source.txt", "tail.txt"] {
        assert!(
            files.lines().any(|path| path == expected),
            "missing {expected}"
        );
    }
    assert_eq!(*provider.pull_requests.borrow(), provider_before);
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
        squash_reconciliation: None,
        ci_trigger_workflows: vec![".github/workflows/ci.yml".to_owned()],
        lease: format!(
            "--force-with-lease=refs/heads/{}:{}",
            candidate.head.name, candidate.head.oid
        ),
        already_satisfied: false,
        rewrite_reason: crate::physical_rebase::BranchRewriteReason::Unspecified,
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
fn join_receipt_proves_exact_tail_ancestry_and_durable_force_preservation() {
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
        squash_reconciliation: None,
        ci_trigger_workflows: vec![".github/workflows/ci.yml".to_owned()],
        lease: format!(
            "--force-with-lease=refs/heads/{}:{}",
            candidate.head.name, candidate.head.oid
        ),
        already_satisfied: true,
        rewrite_reason: crate::physical_rebase::BranchRewriteReason::Unspecified,
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
            candidate_source_head_oid: Some(candidate.head.oid.clone()),
            source: Some(JoinSourceReceipt {
                branch: candidate.head.name.clone(),
                head_oid: candidate.head.oid.clone(),
                parent: before.analysis.fleet.default_branch.clone(),
                tree_oid: CommitOid("source-tree".to_owned()),
                patch_fingerprint: "fnv1a64:source".to_owned(),
                effective_patch_fingerprint: "fnv1a64:effective".to_owned(),
                source_commits: vec![candidate.head.oid.clone()],
                already_landed_commits: Vec::new(),
                source_title: "source title".to_owned(),
                selected_tail: JoinPredecessorReceipt {
                    pr: head.number,
                    branch: head.head.name.clone(),
                    head_oid: head.head.oid.clone(),
                },
                expected_result_tree_oid: rebase.new_tree_oid.clone(),
            }),
            default_branch_oid: before.analysis.fleet.default_branch.oid.clone(),
            rebase_receipt: Some(&rebase),
        },
        &output,
    )
    .unwrap();

    assert_eq!(receipt.force_intent, JoinForceIntent::Preserved);
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
    assert!(output.pull_request.has_label(FORCE_LABEL));
    assert!(!receipt.provider_receipts.iter().any(|provider| {
        provider.kind == MutationKind::RemoveLabel
            && provider
                .before
                .as_ref()
                .is_some_and(|before| before.has_label(FORCE_LABEL))
    }));
}

#[test]
fn root_new_receipt_uses_default_branch_predecessor_bd_d15ba3() {
    let candidate = pull_request(1, "one", "main", &[]);
    let before = status(candidate.clone(), Vec::new());
    let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    let output = execute(
        before.clone(),
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
    .expect("root new succeeds");
    let default = before.analysis.fleet.default_branch.clone();
    let rebase = crate::physical_rebase::RebaseReceipt {
        pr: candidate.number,
        branch: candidate.head.name.clone(),
        old_head_oid: candidate.head.oid.clone(),
        new_head_oid: output.pull_request.head.oid.clone(),
        old_base_oid: candidate.base.oid.clone(),
        new_base_branch: default.name.clone(),
        new_base_oid: default.oid.clone(),
        new_tree_oid: crate::model::CommitOid("e".repeat(40)),
        commit_count: 1,
        merge_topology: None,
        squash_reconciliation: None,
        ci_trigger_workflows: vec![".github/workflows/ci.yml".to_owned()],
        lease: format!(
            "--force-with-lease=refs/heads/{}:{}",
            candidate.head.name, candidate.head.oid
        ),
        already_satisfied: true,
        rewrite_reason: crate::physical_rebase::BranchRewriteReason::Unspecified,
    };
    let mut context = AppContext::default();
    context.config.rebase_on_join = true;
    let receipt = build_join_receipt(
        &context,
        &repository(),
        &before,
        JoinReceiptEvidence {
            predecessor: Some(JoinPredecessorReceipt {
                pr: PrNumber(0),
                branch: default.name.clone(),
                head_oid: default.oid.clone(),
            }),
            candidate_source_head_oid: Some(candidate.head.oid.clone()),
            source: Some(JoinSourceReceipt {
                branch: candidate.head.name.clone(),
                head_oid: candidate.head.oid.clone(),
                parent: default.clone(),
                tree_oid: CommitOid("source-tree".to_owned()),
                patch_fingerprint: "fnv1a64:source".to_owned(),
                effective_patch_fingerprint: "fnv1a64:effective".to_owned(),
                source_commits: vec![candidate.head.oid.clone()],
                already_landed_commits: Vec::new(),
                source_title: "source title".to_owned(),
                selected_tail: JoinPredecessorReceipt {
                    pr: PrNumber(0),
                    branch: default.name.clone(),
                    head_oid: default.oid.clone(),
                },
                expected_result_tree_oid: rebase.new_tree_oid.clone(),
            }),
            default_branch_oid: default.oid.clone(),
            rebase_receipt: Some(&rebase),
        },
        &output,
    )
    .unwrap();

    assert_eq!(receipt.predecessor.pr, PrNumber(0));
    assert_eq!(receipt.predecessor.branch, "main");
    assert_eq!(receipt.predecessor.head_oid, default.oid);
    assert_eq!(receipt.result.base_ref, "main");
    assert!(receipt.ancestry_verified);
    assert!(receipt.membership_durable);
    assert!(receipt.receipt_hash.starts_with("fnv1a64:"));
}

#[test]
fn a_root_admission_reports_the_caravans_it_declined_to_join() {
    // An existing single-member caravan, and a candidate that creates its own.
    let existing = pull_request(50, "existing", "main", &[ACTIVE_LABEL]);
    let candidate = pull_request(1, "one", "main", &[]);
    let provider = FakeProvider::with_pull_requests(vec![existing.clone(), candidate.clone()]);

    let output = execute(
        status(candidate.clone(), vec![existing]),
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

    assert_eq!(
        output.coexisting_caravans,
        vec![PrNumber(50)],
        "a root admission must name the caravan it created a separate one alongside: every caravan on the live fleet was made with `new` while candidates queued behind one nobody joined"
    );
    assert_eq!(
        output.caravan_id,
        PrNumber(1),
        "advisory only: the operation itself is unchanged"
    );
}

/// The advisory must not fire when there is nothing to advise, or the note
/// becomes noise that readers learn to skip.
#[test]
fn a_root_admission_on_an_empty_fleet_reports_no_alternative() {
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

    assert!(
        output.coexisting_caravans.is_empty(),
        "the first caravan on a fleet declined nothing"
    );
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
    // Absent configuration keeps the historical provider-native actor.
    assert_eq!(output.pull_request.auto_merge, AutoMergeState::squash());
    assert_eq!(output.caravan_id, PrNumber(1));
    assert!(output.receipt.changed);
}

/// bd-c462db: check may recommend the one visible caravan, but an explicit
/// `cara new` request still owns its declared mutation and preflight.
#[test]
fn new_with_one_visible_caravan_does_not_inherit_check_recommendation() {
    let head = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
    let candidate = pull_request(2, "two", "main", &[]);
    let provider = FakeProvider::with_pull_requests(vec![head.clone(), candidate.clone()]);

    let output = execute(
        status(candidate, vec![head]),
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
    .expect("explicit new remains a new caravan");

    assert_eq!(output.caravan_id, PrNumber(2));
    assert_eq!(output.pull_request.base.name, "main");
    assert!(output.pull_request.auto_merge.enabled);
    assert_eq!(
        output
            .admission_intent
            .expect("typed membership intent")
            .intent,
        crate::admission::AdmissionIntent::New
    );
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

/// Explicit join intent attaches ahead of an older unjoined FIFO row and its
/// receipt carries exact intent, target, bypass, mutation, and idempotency.
#[test]
fn explicit_join_admits_ahead_of_older_unjoined_row_with_bound_provenance() {
    let head = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
    let older = pull_request(2, "two", "main", &[]);
    let candidate = pull_request(3, "three", "main", &[]);
    let provider =
        FakeProvider::with_pull_requests(vec![head.clone(), older.clone(), candidate.clone()]);
    let discovered = status(candidate, vec![head, older]);
    assert_eq!(discovered.admission.next_candidate, Some(PrNumber(2)));

    let output = execute(
        discovered,
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
    .expect("explicit join intent is not blocked by an unrelated unjoined row");

    assert_eq!(output.pull_request.base.name, "one");
    assert_eq!(output.caravan_id, PrNumber(1));
    let intent = output
        .admission_intent
        .expect("membership binds the typed admission decision");
    assert_eq!(intent.intent, crate::admission::AdmissionIntent::Join);
    assert_eq!(
        intent.outcome,
        crate::admission::AdmissionOrderOutcome::ExplicitAheadOfUnjoined
    );
    assert_eq!(intent.candidate_pr, PrNumber(3));
    assert_eq!(intent.target_caravan, Some(PrNumber(1)));
    assert_eq!(intent.target_tail, Some(PrNumber(1)));
    assert_eq!(intent.bypassed_unjoined_prs, vec![PrNumber(2)]);
    assert!(intent.blocking_prs.is_empty());
    assert!(intent.compatibility_clean && intent.preflight_clean);
    assert!(intent.provider_mutated);
    assert!(!intent.idempotent);
}

/// An exact duplicate retry resumes the same attach without a second durable
/// membership and keeps identical typed intent provenance.
#[test]
fn duplicate_explicit_join_retry_resumes_the_same_attach() {
    let head = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
    let older = pull_request(2, "two", "main", &[]);
    let candidate = pull_request(3, "three", "main", &[]);
    let provider =
        FakeProvider::with_pull_requests(vec![head.clone(), older.clone(), candidate.clone()]);
    *provider.fail_kind.borrow_mut() = Some(MutationKind::AddLabel);
    let request = MembershipRequest {
        operation: MembershipOperation::Join,
        create_pr: false,
        tail_pr: Some(1),
        head_pr: None,
        reason: None,
        priority_label: None,
        agent_priority_labels: Vec::new(),
    };
    let error = execute(
        status(candidate, vec![head.clone(), older.clone()]),
        &clean,
        &provider,
        request.clone(),
    )
    .expect_err("injected provider failure stops the first attempt");
    assert_eq!(error.code(), "github_mutation_failed");

    *provider.fail_kind.borrow_mut() = None;
    let partial = provider.pull_requests.borrow()[&PrNumber(3)].clone();
    assert_eq!(partial.base.name, "one");
    assert!(!partial.has_label(ACTIVE_LABEL));

    let output = execute(
        status(partial, vec![head, older]),
        &clean,
        &provider,
        request,
    )
    .expect("the exact retry resumes rather than restarting");

    assert_eq!(output.pull_request.base.name, "one");
    assert!(output.pull_request.has_label(ACTIVE_LABEL));
    assert!(output.receipt.completed_steps.iter().any(|step| {
        step.kind == MutationKind::SetBase && step.state == MutationStepState::AlreadySatisfied
    }));
    let intent = output
        .admission_intent
        .expect("retry still emits typed provenance");
    assert_eq!(
        intent.outcome,
        crate::admission::AdmissionOrderOutcome::ExplicitAheadOfUnjoined
    );
    assert_eq!(intent.target_caravan, Some(PrNumber(1)));
    assert_eq!(intent.bypassed_unjoined_prs, vec![PrNumber(2)]);
    assert_eq!(
        intent.dependency_prs,
        vec![PrNumber(1)],
        "the resumed candidate now depends on its joined target root"
    );
    assert!(intent.provider_mutated);
}

/// bd-7099e8: explicit owner `new` membership is the same deliberate intent as
/// explicit `join`. The typed decision and the durable audit must both name the
/// bypassed unrelated unjoined row, exactly as `cara check` reported, and the
/// mutation must actually happen.
#[test]
fn explicit_new_membership_admits_ahead_of_an_older_unjoined_row() {
    let older = pull_request(2, "two", "main", &[]);
    let candidate = pull_request(3, "three", "main", &[]);
    let provider = FakeProvider::with_pull_requests(vec![older.clone(), candidate.clone()]);
    let discovered = status(candidate, vec![older]);
    assert_eq!(discovered.admission.next_candidate, Some(PrNumber(2)));

    let output = execute(
        discovered,
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
    .expect("explicit new intent is not blocked by an unrelated unjoined row");

    assert_eq!(output.pull_request.base.name, "main");
    assert!(output.pull_request.has_label(ACTIVE_LABEL));
    assert_eq!(output.caravan_id, PrNumber(3));
    let intent = output
        .admission_intent
        .expect("membership binds the typed admission decision");
    assert_eq!(intent.intent, crate::admission::AdmissionIntent::New);
    assert_eq!(
        intent.outcome,
        crate::admission::AdmissionOrderOutcome::ExplicitAheadOfUnjoined
    );
    assert!(intent.target_caravan.is_none());
    assert_eq!(intent.bypassed_unjoined_prs, vec![PrNumber(2)]);
    assert!(intent.blocking_prs.is_empty());
    assert!(intent.provider_mutated);
    assert!(!intent.idempotent);
}

/// An ambiguous join target fails closed and never reaches ordering.
#[test]
fn ambiguous_join_target_fails_closed_before_any_bypass() {
    let first_root = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
    let second_root = pull_request(2, "two", "main", &[ACTIVE_LABEL]);
    let candidate = pull_request(3, "three", "main", &[]);
    let provider = FakeProvider::with_pull_requests(vec![
        first_root.clone(),
        second_root.clone(),
        candidate.clone(),
    ]);

    let error = execute(
        status(candidate, vec![first_root, second_root]),
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
    .expect_err("an ambiguous target is never guessed");

    assert_eq!(error.code(), "ambiguous_caravan_tail");
}

/// A provider failure during an intent-permitted join is never a silent bypass.
#[test]
fn provider_failure_during_permitted_join_reports_partial_evidence() {
    let head = pull_request(1, "one", "main", &[ACTIVE_LABEL]);
    let older = pull_request(2, "two", "main", &[]);
    let candidate = pull_request(3, "three", "main", &[]);
    let provider =
        FakeProvider::with_pull_requests(vec![head.clone(), older.clone(), candidate.clone()]);
    *provider.fail_kind.borrow_mut() = Some(MutationKind::AddLabel);

    let error = execute(
        status(candidate, vec![head, older]),
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
    .expect_err("provider failure fails the operation");

    assert_eq!(error.code(), "github_mutation_failed");
}

#[test]
fn routine_join_preserves_durable_force_intent() {
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
    .expect("routine join carries durable force intent with the PR");

    assert!(output.pull_request.has_label(FORCE_LABEL));
    assert!(!output.provider_receipts.iter().any(|receipt| {
        receipt.kind == MutationKind::RemoveLabel
            && receipt
                .before
                .as_ref()
                .is_some_and(|before| before.has_label(FORCE_LABEL))
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

/// Live operator report: a repository running `sync.head_merge_actor: caravan`
/// was refused head creation with `auto_merge_not_enabled`. Native auto-merge is
/// only a precondition when the provider performs the merge; when cara merges
/// the green head itself, requiring a repository setting it will never use
/// blocks the caravan for no reason.
#[test]
fn a_caravan_merge_actor_does_not_require_native_auto_merge() {
    let candidate = pull_request(1, "one", "main", &[]);
    let mut provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    provider.allows_auto_merge = false;
    let mut status = status(candidate, Vec::new());
    status.head_merge.actor = crate::model::HeadMergeActor::Caravan;

    let result = execute(
        status,
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
    );

    let refused_for_auto_merge = result
        .as_ref()
        .err()
        .is_some_and(|error| mcp_cli::StructuredError::code(error) == "auto_merge_not_enabled");
    assert!(
        !refused_for_auto_merge,
        "cara-owned merges must not require native auto-merge: {result:?}"
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
fn rejoin_removes_evicted_but_preserves_durable_force_after_full_preflight() {
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
    assert!(output.pull_request.has_label(FORCE_LABEL));
    assert_eq!(output.pull_request.base.name, "one");
}

#[test]
fn newer_same_stream_generation_appearing_after_discovery_stops_before_membership_write() {
    let candidate = pull_request(2107, "old", "main", &[]);
    let old_fact = generation_fact(
        2107,
        "android-agent",
        "bd-c7440c",
        'a',
        "2026-07-23T01:50:32Z",
    );
    let newer_fact = generation_fact(
        2123,
        "android-agent",
        "bd-c7440c",
        'b',
        "2026-07-23T18:34:41Z",
    );
    let initial_integrity = crate::generation::analyze(
        std::slice::from_ref(&old_fact),
        |_base, _head| unreachable!(),
    );
    let mut initial_status = status(candidate.clone(), Vec::new());
    initial_status.admission = crate::read::resolve_admission_with_generation(
        &initial_status.analysis,
        &[],
        initial_integrity,
    );
    let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    *provider.generation_facts.borrow_mut() = vec![old_fact.clone(), newer_fact.clone()];
    provider.generation_relations.borrow_mut().insert(
        (
            old_fact.provenance.as_ref().unwrap().source_head.clone(),
            newer_fact.provenance.as_ref().unwrap().source_head.clone(),
        ),
        crate::generation::CommitRelation::Ahead,
    );

    let error = execute(
        initial_status,
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
            reason: Some("automatic admission".to_owned()),
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
    )
    .expect_err("newer same-stream generation must stop the stale candidate");

    assert_eq!(
        mcp_cli::StructuredError::code(&error),
        "superseded_generation"
    );
    let unchanged = &provider.pull_requests.borrow()[&PrNumber(2107)];
    assert!(!unchanged.has_label(ACTIVE_LABEL));
    assert!(!unchanged.auto_merge.enabled);
    assert_eq!(unchanged.base.name, "main");
}

#[test]
fn generation_candidate_close_race_fails_before_any_membership_mutation() {
    let candidate = pull_request(2107, "old", "main", &[]);
    let candidate_fact = generation_fact(2107, "agent-a", "bd-c7440c", 'a', "2026-07-23T01:50:32Z");
    let initial_integrity = crate::generation::analyze(
        std::slice::from_ref(&candidate_fact),
        |_base, _head| unreachable!(),
    );
    let mut initial_status = status(candidate.clone(), Vec::new());
    initial_status.admission = crate::read::resolve_admission_with_generation(
        &initial_status.analysis,
        &[],
        initial_integrity,
    );
    let provider = FakeProvider::with_pull_requests(vec![candidate]);
    // The fresh open-generation read observes the candidate closed/absent.
    provider.generation_facts.borrow_mut().clear();

    let error = execute(
        initial_status,
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
            reason: Some("manual admission".to_owned()),
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
    )
    .expect_err("closed generation must never be admitted from stale discovery");

    assert_eq!(
        mcp_cli::StructuredError::code(&error),
        "generation_candidate_missing"
    );
    let unchanged = &provider.pull_requests.borrow()[&PrNumber(2107)];
    assert!(!unchanged.has_label(ACTIVE_LABEL));
    assert!(!unchanged.auto_merge.enabled);
}

#[test]
fn same_bead_generation_from_unrelated_agent_does_not_block_membership() {
    let candidate = pull_request(2107, "old", "main", &[]);
    let candidate_fact = generation_fact(2107, "agent-a", "bd-c7440c", 'a', "2026-07-23T01:50:32Z");
    let unrelated = generation_fact(2123, "agent-b", "bd-c7440c", 'b', "2026-07-23T18:34:41Z");
    let initial_integrity = crate::generation::analyze(
        std::slice::from_ref(&candidate_fact),
        |_base, _head| unreachable!(),
    );
    let mut initial_status = status(candidate.clone(), Vec::new());
    initial_status.admission = crate::read::resolve_admission_with_generation(
        &initial_status.analysis,
        &[],
        initial_integrity,
    );
    let provider = FakeProvider::with_pull_requests(vec![candidate]);
    *provider.generation_facts.borrow_mut() = vec![candidate_fact, unrelated];

    let output = execute(
        initial_status,
        &clean,
        &provider,
        MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: false,
            tail_pr: None,
            head_pr: None,
            reason: Some("manual admission".to_owned()),
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
    )
    .expect("unrelated owner stream remains independently admissible");

    assert!(output.pull_request.has_label(ACTIVE_LABEL));
    assert!(output.pull_request.auto_merge.enabled);
}
