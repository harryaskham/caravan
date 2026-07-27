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

fn force_rewrite_plan(candidate: &PullRequestSnapshot) -> crate::physical_rebase::RebasePlan {
    crate::physical_rebase::RebasePlan {
        pr: candidate.number,
        branch: candidate.head.name.clone(),
        old_head_oid: candidate.head.oid.clone(),
        old_base_oid: candidate.base.oid.clone(),
        range_source: crate::physical_rebase::PlannedRangeBase::RemoteBranch {
            branch: candidate.base.clone(),
        },
        new_base: crate::physical_rebase::PlannedBase::Remote(candidate.base.clone()),
        new_head_oid: CommitOid("rewritten0000000000000000000000000000000".to_owned()),
        new_tree_oid: CommitOid("tree000000000000000000000000000000000000".to_owned()),
        commit_count: 1,
        merge_topology: None,
        ci_trigger_workflows: vec![".github/workflows/ci.yml".to_owned()],
        lease: format!(
            "--force-with-lease=refs/heads/{}:{}",
            candidate.head.name, candidate.head.oid
        ),
        already_satisfied: false,
    }
}

#[test]
fn membership_rewrite_failure_restores_force_after_exact_nonpublication() {
    let mut candidate = pull_request(2, "two", "main", &[FORCE_LABEL]);
    candidate.labels.insert(ACTIVE_LABEL.to_owned());
    let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    let plan = force_rewrite_plan(&candidate);
    let mut invalidation = ExecutionState::new(MembershipOperation::New);
    invalidation.current = Some(candidate.clone());
    invalidation
        .ensure_label_absent(&provider, &repository(), FORCE_LABEL)
        .unwrap();
    let error = restore_membership_force_after_nonpublication(
        &provider,
        &repository(),
        &candidate,
        &plan,
        Some(&mut invalidation),
        AppError::validation("rebase_stale_lease", "push refused"),
    );
    assert_eq!(error.code(), "rebase_stale_lease");
    assert!(provider.pull_requests.borrow()[&candidate.number].has_label(FORCE_LABEL));
    let details = error.details().unwrap();
    assert_eq!(details["force_intent_restoration"]["state"], "restored");
    assert_eq!(details["force_intent_restoration"]["restored"], true);
}

#[test]
fn membership_rewrite_published_or_indeterminate_never_restores_force() {
    for observed in [
        CommitOid("rewritten0000000000000000000000000000000".to_owned()),
        CommitOid("thirdparty000000000000000000000000000000".to_owned()),
    ] {
        let mut candidate = pull_request(2, "two", "main", &[FORCE_LABEL]);
        candidate.labels.insert(ACTIVE_LABEL.to_owned());
        let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
        let plan = force_rewrite_plan(&candidate);
        let mut invalidation = ExecutionState::new(MembershipOperation::New);
        invalidation.current = Some(candidate.clone());
        invalidation
            .ensure_label_absent(&provider, &repository(), FORCE_LABEL)
            .unwrap();
        provider
            .pull_requests
            .borrow_mut()
            .get_mut(&candidate.number)
            .unwrap()
            .head
            .oid = observed.clone();
        let error = restore_membership_force_after_nonpublication(
            &provider,
            &repository(),
            &candidate,
            &plan,
            Some(&mut invalidation),
            AppError::validation("rebase_stale_lease", "push outcome"),
        );
        assert_eq!(error.code(), "rebase_stale_lease");
        assert!(!provider.pull_requests.borrow()[&candidate.number].has_label(FORCE_LABEL));
        let details = error.details().unwrap();
        assert_eq!(
            details["force_intent_restoration"]["state"],
            if observed == plan.new_head_oid {
                "published"
            } else {
                "indeterminate"
            }
        );
    }
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
        crate::admission::AdmissionOrderOutcome::JoinAheadOfUnjoined
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
        crate::admission::AdmissionOrderOutcome::JoinAheadOfUnjoined
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
