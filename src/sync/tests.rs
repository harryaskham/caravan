//! Hermetic sync policy, decision, force, CI, and receipt fixtures.
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::*;
use crate::graph;
use crate::model::{
    AutoMergeState, BranchSnapshot, CheckSnapshot, CommitOid, CompatibilityReport,
    RepositorySnapshot,
};

struct FakeProvider {
    allows_auto_merge: bool,
    branch_protected: bool,
    pulls: RefCell<BTreeMap<PrNumber, PullRequestSnapshot>>,
    failures: RefCell<VecDeque<MutationKind>>,
    calls: RefCell<Vec<MutationKind>>,
    failed_runs: RefCell<BTreeMap<PrNumber, Vec<WorkflowRunSnapshot>>>,
    diagnostic_heads: RefCell<BTreeMap<PrNumber, CommitOid>>,
    diagnostic_job_conclusions: RefCell<BTreeMap<PrNumber, String>>,
    diagnostic_lineage: RefCell<BTreeMap<PrNumber, crate::ci::SelectedRefLineageReceipt>>,
    admin_permission: bool,
    branch_head: RefCell<crate::model::CommitOid>,
    audits: RefCell<Vec<ControlLabelAudit>>,
    comments: RefCell<BTreeMap<PrNumber, Vec<String>>>,
}

impl FakeProvider {
    fn with_pull_requests(pulls: Vec<PullRequestSnapshot>) -> Self {
        Self {
            allows_auto_merge: true,
            branch_protected: true,
            pulls: RefCell::new(
                pulls
                    .into_iter()
                    .map(|pull_request| (pull_request.number, pull_request))
                    .collect(),
            ),
            failures: RefCell::new(VecDeque::new()),
            calls: RefCell::new(Vec::new()),
            failed_runs: RefCell::new(BTreeMap::new()),
            diagnostic_heads: RefCell::new(BTreeMap::new()),
            diagnostic_job_conclusions: RefCell::new(BTreeMap::new()),
            diagnostic_lineage: RefCell::new(BTreeMap::new()),
            admin_permission: true,
            branch_head: RefCell::new(branch("main").oid),
            audits: RefCell::new(Vec::new()),
            comments: RefCell::new(BTreeMap::new()),
        }
    }

    fn fail_once(&self, kind: MutationKind) {
        self.failures.borrow_mut().push_back(kind);
    }

    fn mutate(
        &self,
        expected: &PullRequestPrecondition,
        kind: MutationKind,
        change: impl FnOnce(&mut PullRequestSnapshot),
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.calls.borrow_mut().push(kind);
        if self.failures.borrow().front() == Some(&kind) {
            self.failures.borrow_mut().pop_front();
            return Err(MutationError::Provider(
                crate::github::DiscoveryError::CommandFailed {
                    command: crate::command::CommandSpec::new("fake"),
                    code: Some(1),
                    stderr: "injected failure".to_owned(),
                },
            ));
        }
        let before = self
            .pulls
            .borrow()
            .get(&expected.number)
            .cloned()
            .expect("fake PR");
        let actual = PullRequestPrecondition::from(&before);
        if &actual != expected {
            return Err(MutationError::StalePrecondition {
                expected: Box::new(expected.clone()),
                actual: Box::new(actual),
                changed_fields: vec!["fake_race".to_owned()],
            });
        }
        let mut after = before.clone();
        change(&mut after);
        self.pulls.borrow_mut().insert(after.number, after.clone());
        Ok(GitHubMutationReceipt {
            kind,
            before: Some(before),
            after,
            provider_output: None,
        })
    }
}

impl SyncProvider for FakeProvider {
    fn verify_pull_request(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<PullRequestSnapshot, MutationError> {
        let actual = self
            .pulls
            .borrow()
            .get(&expected.number)
            .cloned()
            .expect("fake PR");
        let actual_precondition = PullRequestPrecondition::from(&actual);
        if actual_precondition != *expected {
            return Err(MutationError::StalePrecondition {
                expected: Box::new(expected.clone()),
                actual: Box::new(actual_precondition),
                changed_fields: vec!["fake_race".to_owned()],
            });
        }
        Ok(actual)
    }

    fn refetch_pull_request(
        &self,
        _repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<PullRequestSnapshot, MutationError> {
        Ok(self.pulls.borrow()[&number].clone())
    }

    fn verify_branch_head(
        &self,
        _repository: &RepositoryId,
        branch: &str,
        expected: &crate::model::CommitOid,
    ) -> Result<(), MutationError> {
        let actual = self.branch_head.borrow().clone();
        if &actual != expected {
            return Err(MutationError::BranchHeadMismatch {
                branch: branch.to_owned(),
                expected: expected.clone(),
                actual,
            });
        }
        Ok(())
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

    fn set_base(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        base: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.mutate(expected, MutationKind::SetBase, |pull_request| {
            pull_request.base = branch(base);
        })
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

    fn pull_request_comment_bodies(
        &self,
        _repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<Vec<String>, MutationError> {
        Ok(self
            .comments
            .borrow()
            .get(&number)
            .cloned()
            .unwrap_or_default())
    }

    fn ensure_marked_comment(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        marker: &str,
        body: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        let already = self
            .comments
            .borrow()
            .get(&expected.number)
            .is_some_and(|comments| comments.iter().any(|item| item.contains(marker)));
        if !already {
            self.comments
                .borrow_mut()
                .entry(expected.number)
                .or_default()
                .push(body.to_owned());
        }
        let mut receipt = self.mutate(expected, MutationKind::Comment, |_| {})?;
        if already {
            receipt.provider_output = Some(format!("existing GitHub comment {marker}"));
        }
        Ok(receipt)
    }

    fn failed_runs_for_pull_request(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<Vec<WorkflowRunSnapshot>, MutationError> {
        let current = self
            .pulls
            .borrow()
            .get(&expected.number)
            .cloned()
            .expect("fake PR");
        let actual = PullRequestPrecondition::from(&current);
        if &actual != expected {
            return Err(MutationError::StalePrecondition {
                expected: Box::new(expected.clone()),
                actual: Box::new(actual),
                changed_fields: vec!["fake_race".to_owned()],
            });
        }
        Ok(self
            .failed_runs
            .borrow()
            .get(&expected.number)
            .cloned()
            .unwrap_or_default())
    }

    fn failed_run_diagnostics(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        run_ids: &[u64],
    ) -> Result<WorkflowFailureDiagnostics, MutationError> {
        let failed_runs = self
            .failed_runs
            .borrow()
            .get(&expected.number)
            .cloned()
            .unwrap_or_default();
        let runs = run_ids
            .iter()
            .filter_map(|run_id| {
                failed_runs
                    .iter()
                    .find(|run| run.database_id == *run_id)
                    .map(|run| crate::ci::WorkflowRunFailureDiagnostic {
                        run_id: *run_id,
                        attempt: 1,
                        workflow_id: *run_id,
                        check_suite_id: *run_id,
                        workflow_name: run.workflow_name.clone(),
                        event: "pull_request".to_owned(),
                        status: run.status.clone(),
                        conclusion: run.conclusion.clone(),
                        head_branch: "feature".to_owned(),
                        head_sha: CommitOid(run.head_sha.clone()),
                        expected_pr: expected.number,
                        expected_head_oid: expected.head_oid.clone(),
                        expected_base_oid: expected.base_oid.clone(),
                        pull_requests: vec![crate::ci::WorkflowRunPullRequestAssociation {
                            pr: expected.number,
                            head_oid: Some(
                                self.diagnostic_heads
                                    .borrow()
                                    .get(&expected.number)
                                    .cloned()
                                    .unwrap_or_else(|| expected.head_oid.clone()),
                            ),
                            base_oid: Some(expected.base_oid.clone()),
                        }],
                        failed_jobs: vec![crate::ci::WorkflowJobFailureDiagnostic {
                            job_id: *run_id,
                            name: "test infrastructure".to_owned(),
                            status: "completed".to_owned(),
                            conclusion: self
                                .diagnostic_job_conclusions
                                .borrow()
                                .get(&expected.number)
                                .cloned()
                                .unwrap_or_else(|| "timed_out".to_owned()),
                            url: run.url.clone(),
                            runner_name: None,
                            runner_labels: Vec::new(),
                            failed_steps: Vec::new(),
                            steps_truncated: false,
                            selected_lineage: self
                                .diagnostic_lineage
                                .borrow()
                                .get(&expected.number)
                                .cloned(),
                            lineage_evidence_status: if self
                                .diagnostic_lineage
                                .borrow()
                                .contains_key(&expected.number)
                            {
                                crate::ci::LineageEvidenceStatus::Parsed
                            } else {
                                crate::ci::LineageEvidenceStatus::NotRequested
                            },
                        }],
                        jobs_total: 1,
                        jobs_truncated: false,
                    })
            })
            .collect();
        Ok(WorkflowFailureDiagnostics {
            requested_run_ids: run_ids.to_vec(),
            runs,
            runs_truncated: false,
        })
    }

    fn rerun_failed_run(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        run_id: u64,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        let run = self
            .failed_runs
            .borrow()
            .get(&expected.number)
            .and_then(|runs| runs.iter().find(|run| run.database_id == run_id))
            .cloned()
            .expect("exact failed run");
        if !run.pull_requests.contains(&expected.number) {
            return Err(MutationError::RunPullRequestMismatch {
                run_id,
                expected_pr: expected.number,
                actual_prs: run.pull_requests,
            });
        }
        if run.head_sha != expected.head_oid.0 {
            return Err(MutationError::RunHeadMismatch {
                run_id,
                expected_head: expected.head_oid.0.clone(),
                actual_head: run.head_sha,
            });
        }
        self.mutate(expected, MutationKind::RerunChecks, |pull_request| {
            for check in &mut pull_request.checks {
                if check.details_url.as_deref().and_then(workflow_run_id) == Some(run_id) {
                    check.state = CheckState::Queued;
                    check.provider_state = Some("QUEUED".to_owned());
                }
            }
        })
    }

    fn viewer_permission(&self, _repository: &RepositoryId) -> Result<String, MutationError> {
        Ok(if self.admin_permission {
            "ADMIN"
        } else {
            "WRITE"
        }
        .to_owned())
    }

    fn ensure_control_label_comment(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        audit: &ControlLabelAudit,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.audits.borrow_mut().push(audit.clone());
        self.mutate(expected, MutationKind::Comment, |_| {})
    }

    fn admin_squash_merge(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        if !self.admin_permission {
            return Err(MutationError::PermissionDenied {
                required: "ADMIN".to_owned(),
                actual: "WRITE".to_owned(),
            });
        }
        self.mutate(expected, MutationKind::SquashMerge, |pull_request| {
            pull_request.state = PullRequestState::Merged;
            pull_request.merged_at = Some("now".to_owned());
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
        oid: CommitOid(format!("{name:0<40}")),
    }
}

fn pull_request(
    number: u64,
    head: &str,
    base: &str,
    state: PullRequestState,
    auto_merge: AutoMergeState,
) -> PullRequestSnapshot {
    PullRequestSnapshot {
        number: PrNumber(number),
        title: format!("PR {number}"),
        url: format!("https://example.invalid/{number}"),
        state,
        draft: false,
        head: branch(head),
        base: branch(base),
        cross_repository: false,
        labels: BTreeSet::from(["caravan".to_owned()]),
        auto_merge,
        checks: Vec::<CheckSnapshot>::new(),
        created_at: Some(format!("2026-01-01T00:00:{number:02}Z")),
        merged_at: (state == PullRequestState::Merged).then(|| "now".to_owned()),
        updated_at: None,
    }
}

fn check(name: &str, state: CheckState, run_id: Option<u64>) -> CheckSnapshot {
    CheckSnapshot {
        name: name.to_owned(),
        state,
        provider_state: Some(format!("{state:?}").to_uppercase()),
        details_url: run_id
            .map(|id| format!("https://github.com/harryaskham/caravan/actions/runs/{id}/job/1")),
    }
}

fn selected_lineage_receipt(
    pull_request: &PullRequestSnapshot,
) -> crate::ci::SelectedRefLineageReceipt {
    crate::ci::SelectedRefLineageReceipt {
        event: "pull_request".to_owned(),
        head_ref: pull_request.head.name.clone(),
        selected_ref: "refs/pull/1/merge".to_owned(),
        selected_commit: CommitOid("selected-merge".to_owned()),
        actual_head: CommitOid("selected-merge".to_owned()),
        expected_head: pull_request.head.oid.clone(),
        expected_base: pull_request.base.oid.clone(),
        parents: vec![
            pull_request.base.oid.clone(),
            CommitOid("prior-head".to_owned()),
        ],
    }
}

fn failed_run(id: u64, head: &PullRequestSnapshot) -> WorkflowRunSnapshot {
    WorkflowRunSnapshot {
        database_id: id,
        pull_requests: vec![head.number],
        head_sha: head.head.oid.0.clone(),
        status: "completed".to_owned(),
        conclusion: "failure".to_owned(),
        event: "pull_request".to_owned(),
        name: "CI".to_owned(),
        workflow_name: "CI".to_owned(),
        url: format!("https://github.com/harryaskham/caravan/actions/runs/{id}"),
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

fn status(
    pulls: Vec<PullRequestSnapshot>,
    current: Option<PrNumber>,
    checker: &impl graph::CompatibilityChecker,
) -> StatusOutput {
    let snapshot = RepositorySnapshot {
        merge_candidates: Vec::new(),
        merge_candidates_truncated: 0,
        previous_default_oid: None,
        default_branch_movements: Vec::new(),
        repository: repository(),
        default_branch: branch("main"),
        current_branch: current.map(|number| format!("pr-{number}")),
        current_pr: current,
        pull_requests: pulls,
        observed_at: None,
    };
    let analysis = graph::analyze(&snapshot, checker).expect("analysis");
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
        admission: read::resolve_admission(
            &analysis,
            &crate::config::CaravanConfig::default().agent_priority_labels,
        ),
        analysis,
        pauses: Vec::new(),
    }
}

fn healthy_chain() -> Vec<PullRequestSnapshot> {
    vec![
        pull_request(
            1,
            "one",
            "main",
            PullRequestState::Open,
            AutoMergeState::squash(),
        ),
        pull_request(
            2,
            "two",
            "one",
            PullRequestState::Open,
            AutoMergeState::disabled(),
        ),
        pull_request(
            3,
            "three",
            "two",
            PullRequestState::Open,
            AutoMergeState::disabled(),
        ),
    ]
}

#[test]
fn no_write_caravan_plan_records_actions_without_provider_mutation() {
    let pulls = healthy_chain();
    let status = status(pulls.clone(), Some(PrNumber(1)), &clean);
    let provider = FakeProvider::with_pull_requests(pulls);
    let caravan = status.analysis.fleet.caravans[0].clone();
    let mut progress = SyncProgress::new(&status, vec![caravan.id], 20);
    let mut actions = Vec::new();
    let mut decisions = Vec::new();
    let mut events = Vec::new();

    plan_caravan_convergence(
        &status,
        &provider,
        &caravan,
        &SyncInput {
            all: true,
            rerun_failed: false,
        },
        false,
        false,
        &mut progress,
        &mut actions,
        &mut decisions,
        &mut events,
    )
    .expect("planning reads but never mutates");

    assert!(provider.calls.borrow().is_empty());
    assert!(decisions.is_empty());
    assert_eq!(progress.ci.len(), 3);
    assert!(actions.iter().any(|action| action.kind == "set_base"));
    assert!(actions.iter().any(|action| action.kind == "observe_ci"));
    assert!(actions.iter().any(|action| {
        action.kind == "enable_squash_auto_merge"
            && action.state == SyncPlanActionState::AlreadySatisfied
    }));
    assert!(actions.iter().all(|action| {
        action.state != SyncPlanActionState::WouldMutate
            && action.state != SyncPlanActionState::WouldStop
    }));
}

#[test]
fn no_write_auto_admission_plans_only_first_exact_candidate() {
    let mut candidate = pull_request(
        9,
        "candidate",
        "main",
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    candidate.labels.clear();
    let status = status(vec![candidate.clone()], Some(candidate.number), &clean);
    let mut context = AppContext::default();
    context.config.rebase_on_join = true;
    context.config.sync.actions.join_unlabelled_prs = true;
    let mut actions = Vec::new();
    let mut events = Vec::new();

    let plan = plan_auto_admission_with_checker(
        &context,
        &status,
        &SyncInput {
            all: true,
            rerun_failed: false,
        },
        false,
        &mut actions,
        &mut events,
        &clean,
    )
    .expect("canonical candidate is planned without mutation");

    assert_eq!(plan.candidate_pr, Some(candidate.number));
    assert_eq!(plan.target_tail, None);
    assert_eq!(plan.continuation, "replan_after_first_admission");
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].kind, "auto_admission_new");
    assert_eq!(actions[0].state, SyncPlanActionState::WouldMutate);
    assert_eq!(events, vec![EventKind::CaravanCreated]);
}

#[test]
fn no_write_auto_admission_never_leapfrogs_rejected_canonical_candidate() {
    let mut candidate = pull_request(
        9,
        "candidate",
        "main",
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    candidate.labels.clear();
    candidate
        .labels
        .insert("caravan-priority:unknown".to_owned());
    let status = status(vec![candidate.clone()], Some(candidate.number), &clean);
    let mut context = AppContext::default();
    context.config.rebase_on_join = true;
    context.config.sync.actions.join_unlabelled_prs = true;
    let mut actions = Vec::new();
    let mut events = Vec::new();
    let plan = plan_auto_admission_with_checker(
        &context,
        &status,
        &SyncInput {
            all: true,
            rerun_failed: false,
        },
        false,
        &mut actions,
        &mut events,
        &clean,
    )
    .expect("rejection is a no-write plan result");
    assert_eq!(plan.candidate_pr, Some(candidate.number));
    assert_eq!(plan.continuation, "rejected_canonical_candidate");
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].kind, "reject_canonical_candidate");
    assert_eq!(actions[0].state, SyncPlanActionState::WouldStop);
    assert!(events.is_empty());
}

#[test]
fn plan_hash_binds_exact_actions_not_telemetry() {
    let status = status(healthy_chain(), Some(PrNumber(1)), &clean);
    let base = SyncPlanOutput {
        schema_version: 1,
        mutated: false,
        provider_writes: 0,
        local_ephemeral_preflight: false,
        repository: status.repository.clone(),
        default_branch: status.analysis.fleet.default_branch.clone(),
        all: true,
        plan_hash: String::new(),
        selected_caravans: vec![PrNumber(1)],
        physical_rebase_plans: Vec::new(),
        ci: Vec::new(),
        actions: vec![SyncPlanAction {
            order: 1,
            phase: SyncPlanPhase::ProviderConvergence,
            state: SyncPlanActionState::AlreadySatisfied,
            kind: "set_base".to_owned(),
            pr: Some(PrNumber(1)),
            caravan_id: Some(PrNumber(1)),
            expected: None,
            target: Some(json!({"branch": "main"})),
            reason: "already exact".to_owned(),
        }],
        auto_admission: SyncAutoAdmissionPlan {
            enabled: false,
            heuristic_version: AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
            continuation: "disabled".to_owned(),
            candidate_pr: None,
            target_tail: None,
            tested_tails: Vec::new(),
            compatibility_reasons: Vec::new(),
        },
        decisions: Vec::new(),
        would_emit_events: Vec::new(),
        github_requests_used: 1,
        status,
    };
    let first = base.clone().finalize_hash();
    let mut telemetry_changed = base.clone();
    telemetry_changed.github_requests_used = 99;
    telemetry_changed.status.provider_api.calls = 99;
    let second = telemetry_changed.finalize_hash();
    assert_eq!(first.plan_hash, second.plan_hash);

    let mut changed = base;
    changed.actions[0].reason = "different exact action".to_owned();
    assert_ne!(first.plan_hash, changed.finalize_hash().plan_hash);
}

#[test]
fn greedy_planner_forms_empty_fleet_then_uses_first_compatible_tail() {
    let mut candidate = pull_request(
        9,
        "candidate",
        "main",
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    candidate.labels.clear();
    let empty = status(vec![candidate.clone()], Some(candidate.number), &clean);
    let evaluation =
        evaluate_auto_candidate(&empty, &candidate, &clean).expect("empty fleet preflight");
    assert_eq!(evaluation.target, AutoCandidateTarget::New);
    assert!(evaluation.tested_tails.is_empty());
    assert!(evaluation.reasons.is_empty());

    let first = pull_request(
        1,
        "first",
        "main",
        PullRequestState::Open,
        AutoMergeState::squash(),
    );
    let second = pull_request(
        2,
        "second",
        "main",
        PullRequestState::Open,
        AutoMergeState::squash(),
    );
    let fleet = status(
        vec![first, second, candidate.clone()],
        Some(candidate.number),
        &clean,
    );
    let checker = |candidate_branch: &BranchSnapshot,
                   target: &BranchSnapshot|
     -> Result<CompatibilityReport, AppError> {
        Ok(CompatibilityReport {
            candidate: candidate_branch.clone(),
            target: target.clone(),
            outcome: if target.name == "first" {
                CompatibilityOutcome::Conflict
            } else {
                CompatibilityOutcome::Clean
            },
            conflicting_paths: if target.name == "first" {
                vec!["src/lib.rs".to_owned()]
            } else {
                Vec::new()
            },
            diagnostic: None,
        })
    };
    let evaluation = evaluate_auto_candidate(&fleet, &candidate, &checker).expect("tail preflight");
    assert_eq!(evaluation.target, AutoCandidateTarget::Join(PrNumber(2)));
    assert_eq!(
        evaluation
            .tested_tails
            .iter()
            .map(|tail| tail.tail_pr)
            .collect::<Vec<_>>(),
        [PrNumber(1), PrNumber(2)]
    );
    assert!(
        evaluation
            .reasons
            .iter()
            .any(|reason| reason.contains("tail #1"))
    );
}

#[test]
fn skip_receipt_round_trips_and_invalidates_on_generation_change() {
    let active = pull_request(
        1,
        "head",
        "main",
        PullRequestState::Open,
        AutoMergeState::squash(),
    );
    let mut candidate = pull_request(
        9,
        "candidate",
        "main",
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    candidate.labels.clear();
    candidate
        .labels
        .insert(AUTO_ADMISSION_SKIP_LABEL.to_owned());
    let status = status(
        vec![active, candidate.clone()],
        Some(candidate.number),
        &clean,
    );
    let context = AppContext::default();
    let receipt = AutoJoinSkipReceipt {
        schema_version: 1,
        repository: status.repository.clone(),
        candidate_pr: candidate.number,
        candidate_head: candidate.head.clone(),
        candidate_base: candidate.base.clone(),
        default_branch: status.analysis.fleet.default_branch.clone(),
        tested_tails: current_tail_generations(&status),
        config_fingerprint: auto_admission_config_fingerprint(&context),
        heuristic_version: AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
        compatibility_reasons: vec!["tail #1: conflict".to_owned()],
        actor: "cara sync automatic admission".to_owned(),
        observed_unix_secs: 1,
        evidence_hash: String::new(),
    }
    .finalize_hash();

    let parsed =
        AutoJoinSkipReceipt::from_comment(&receipt.comment_body()).expect("receipt marker decodes");
    assert_eq!(parsed, receipt);
    assert!(skip_receipt_matches(&context, &status, &receipt));

    let mut moved = status.clone();
    moved
        .analysis
        .pull_requests
        .get_mut(&candidate.number)
        .unwrap()
        .head
        .oid = CommitOid("moved".repeat(8));
    assert!(!skip_receipt_matches(&context, &moved, &receipt));

    let mut tail_moved = status.clone();
    tail_moved
        .analysis
        .pull_requests
        .get_mut(&PrNumber(1))
        .unwrap()
        .head
        .oid = CommitOid("tailmoved".repeat(5));
    assert!(!skip_receipt_matches(&context, &tail_moved, &receipt));

    let mut config_changed = context.clone();
    config_changed.config.sync.max_candidates_per_tick += 1;
    assert!(!skip_receipt_matches(&config_changed, &status, &receipt));
}

#[test]
fn forty_candidate_auto_admission_preserves_nonzero_exact_git_budget() {
    let mut candidates = (1..=40)
        .map(|number| {
            let mut candidate = pull_request(
                number,
                &format!("candidate-{number}"),
                "main",
                PullRequestState::Open,
                AutoMergeState::disabled(),
            );
            candidate.labels.clear();
            candidate
        })
        .collect::<Vec<_>>();
    let provider = FakeProvider::with_pull_requests(candidates.clone());
    let status = status(std::mem::take(&mut candidates), None, &clean);
    let mut context = AppContext::default();
    context.config.sync.actions.join_unlabelled_prs = true;
    context.config.command_timeout_secs = 30;
    context.config.sync.max_candidates_per_tick = 40;
    let mut progress = SyncProgress::new(&status, Vec::new(), u32::MAX);
    let github_budget = crate::command::GithubRequestBudget::new(100);

    let (_status, output) = run_auto_admission(
        &context,
        status,
        &provider,
        &mut progress,
        Instant::now() + Duration::from_secs(5),
        &github_budget,
    )
    .unwrap();

    assert_eq!(
        output.continuation,
        AutoAdmissionContinuation::DeadlineExhausted
    );
    assert_eq!(output.candidates_considered, 0);
    assert_eq!(output.candidate_budget_reserved_ms, 30_000);
    assert!(output.candidate_budget_remaining_ms <= 5_000);
    assert_eq!(output.remaining_candidates.len(), 40);
    assert!(provider.calls.borrow().is_empty());
}

#[test]
fn persist_skip_is_idempotent_and_manual_membership_can_consume_the_label() {
    let mut candidate = pull_request(
        9,
        "candidate",
        "main",
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    candidate.labels.clear();
    let status = status(vec![candidate.clone()], Some(candidate.number), &clean);
    let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    let mut progress = SyncProgress::new(&status, Vec::new(), u32::MAX);
    let receipt = AutoJoinSkipReceipt {
        schema_version: 1,
        repository: status.repository.clone(),
        candidate_pr: candidate.number,
        candidate_head: candidate.head.clone(),
        candidate_base: candidate.base.clone(),
        default_branch: status.analysis.fleet.default_branch.clone(),
        tested_tails: Vec::new(),
        config_fingerprint: auto_admission_config_fingerprint(&AppContext::default()),
        heuristic_version: AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
        compatibility_reasons: vec!["default conflict".to_owned()],
        actor: "cara sync automatic admission".to_owned(),
        observed_unix_secs: 1,
        evidence_hash: String::new(),
    }
    .finalize_hash();

    persist_auto_skip(&provider, &mut progress, &repository(), &receipt).unwrap();
    persist_auto_skip(&provider, &mut progress, &repository(), &receipt).unwrap();

    assert!(provider.pulls.borrow()[&candidate.number].has_label(AUTO_ADMISSION_SKIP_LABEL));
    assert_eq!(provider.comments.borrow()[&candidate.number].len(), 1);
    assert_eq!(
        provider
            .calls
            .borrow()
            .iter()
            .filter(|kind| **kind == MutationKind::AddLabel)
            .count(),
        1
    );
}

#[test]
fn oversized_skip_receipt_fails_before_label_mutation() {
    let mut candidate = pull_request(
        9,
        "candidate",
        "main",
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    candidate.labels.clear();
    let status = status(vec![candidate.clone()], Some(candidate.number), &clean);
    let provider = FakeProvider::with_pull_requests(vec![candidate.clone()]);
    let mut progress = SyncProgress::new(&status, Vec::new(), u32::MAX);
    let receipt = AutoJoinSkipReceipt {
        schema_version: 1,
        repository: status.repository.clone(),
        candidate_pr: candidate.number,
        candidate_head: candidate.head.clone(),
        candidate_base: candidate.base.clone(),
        default_branch: status.analysis.fleet.default_branch.clone(),
        tested_tails: Vec::new(),
        config_fingerprint: auto_admission_config_fingerprint(&AppContext::default()),
        heuristic_version: AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
        compatibility_reasons: vec!["x".repeat(MAX_AUTO_ADMISSION_COMMENT_BYTES)],
        actor: "cara sync automatic admission".to_owned(),
        observed_unix_secs: 1,
        evidence_hash: String::new(),
    }
    .finalize_hash();

    let error = persist_auto_skip(&provider, &mut progress, &repository(), &receipt)
        .expect_err("oversized authority must not be truncated");

    assert_eq!(error.code(), "auto_admission_skip_receipt_too_large");
    assert!(provider.calls.borrow().is_empty());
    assert!(!provider.pulls.borrow()[&candidate.number].has_label(AUTO_ADMISSION_SKIP_LABEL));
}

#[test]
fn pending_ci_reports_waiting_without_speculative_mutation() {
    let mut pulls = healthy_chain();
    pulls[0].checks = vec![check("build-test", CheckState::Queued, Some(7))];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress = execute(&status, &provider, false, false, false).expect("pending waits");

    assert_eq!(progress.ci[0].disposition, CiDisposition::Waiting);
    assert!(!progress.operation_receipt().changed);
    assert!(progress.events.is_empty());
    assert!(provider.calls.borrow().is_empty());
    let scheduler =
        successful_scheduler_status(&status, &progress.ci, &progress.paused_caravans, true);
    assert_eq!(scheduler.disposition, SchedulerDisposition::WaitingCi);
    assert_eq!(scheduler.wake_class, SchedulerWakeClass::None);
    assert_eq!(
        scheduler.waiting_prs,
        [PrNumber(1), PrNumber(2), PrNumber(3)]
    );
    assert_eq!(scheduler.caravans[0].root, PrNumber(1));
    assert_eq!(scheduler.caravans[0].tail, PrNumber(3));
    assert_eq!(
        scheduler.caravans[0].members[0].ci,
        Some(CiDisposition::Waiting)
    );
    let encoded = serde_json::to_value(&scheduler).expect("scheduler status JSON");
    assert_eq!(encoded["schema_version"], 1);
    assert_eq!(encoded["disposition"], "waiting_ci");
    assert_eq!(encoded["wake_class"], "none");
    assert_eq!(encoded["default_branch"]["name"], "main");
    assert_eq!(encoded["caravans"][0]["root"], 1);
    assert_eq!(encoded["caravans"][0]["tail"], 3);
    assert_eq!(encoded["caravans"][0]["members"][0]["pr"], 1);
}

#[test]
fn unforced_failure_returns_exact_ci_decision_and_canonical_event() {
    let mut pulls = healthy_chain();
    pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(10))];
    let matching = failed_run(10, &pulls[0]);
    let spurious = failed_run(11, &pulls[0]);
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider
        .failed_runs
        .borrow_mut()
        .insert(PrNumber(1), vec![spurious, matching]);
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let error = execute(&status, &provider, false, false, false).expect_err("failed CI decides");

    assert_eq!(mcp_cli::StructuredError::code(&error), "ci_failure");
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(details["decision"]["evidence"]["ci"]["pr"], 1);
    assert_eq!(
        details["decision"]["evidence"]["ci"]["rerunnable_run_ids"],
        json!([10])
    );
    assert_eq!(
        details["decision"]["evidence"]["event"]["kind"],
        "ci_failed"
    );
    assert_eq!(
        details["decision"]["evidence"]["event"]["operation_id"],
        details["decision"]["operation_id"]
    );
    assert!(provider.calls.borrow().is_empty());
    let scheduler = scheduler_failure_status(&error);
    assert_eq!(
        scheduler.disposition,
        SchedulerDisposition::ExternalDecision
    );
    assert_eq!(scheduler.wake_class, SchedulerWakeClass::ExternalDecision);
    assert!(!scheduler.retryable);
}

#[test]
fn stale_provider_precondition_retries_without_waking_a_repair_actor() {
    let pulls = healthy_chain();
    let status = status(pulls.clone(), Some(PrNumber(1)), &clean);
    let progress = SyncProgress::new(&status, vec![PrNumber(1)], u32::MAX);
    let expected = PullRequestPrecondition::from(&pulls[0]);
    let mut actual_pull = pulls[0].clone();
    actual_pull.head.oid = CommitOid("moved-head".to_owned());
    let actual = PullRequestPrecondition::from(&actual_pull);
    let error = mutation_error(
        &MutationError::StalePrecondition {
            expected: Box::new(expected),
            actual: Box::new(actual),
            changed_fields: vec!["head_oid".to_owned()],
        },
        &progress,
        Some(PrNumber(1)),
    );

    let scheduler = scheduler_failure_status(&error);
    assert_eq!(scheduler.disposition, SchedulerDisposition::RetryTick);
    assert_eq!(scheduler.wake_class, SchedulerWakeClass::RetryTick);
    assert!(scheduler.retryable);
    let attached = attach_scheduler_failure(&error, &scheduler);
    let details = attached.details().expect("scheduler details");
    assert_eq!(details["scheduler_status"]["disposition"], "retry_tick");
    assert_eq!(details["scheduler_status"]["wake_class"], "retry_tick");
}

#[test]
fn provider_generation_invariant_emits_external_decision_wake_event() {
    let error = AppError::structured(
        ErrorCategory::Validation,
        "rebase_midpoint_head_stale",
        "provider exposed a different rewritten head",
        Some(json!({
            "repository": repository(),
            "rebase_plans": [],
            "rebase_receipts": [],
        })),
    );
    let scheduler = scheduler_failure_status(&error);
    assert_eq!(scheduler.wake_class, SchedulerWakeClass::ExternalDecision);
    let attached = attach_scheduler_failure(&error, &scheduler);
    let event = sync_failed_event(&attached).expect("external decision event");
    assert_eq!(event.kind, EventKind::SyncFailed);
    assert_eq!(event.metadata["error_code"], "rebase_midpoint_head_stale");
    assert_eq!(
        event.metadata["scheduler_status"]["wake_class"],
        "external_decision"
    );
}

#[test]
fn nonlinear_range_is_a_stable_external_decision_with_exact_context() {
    let raw = AppError::structured(
        ErrorCategory::Validation,
        "rebase_nonlinear_range",
        "candidate-only history contains merge commits",
        Some(json!({
            "pr": PrNumber(2),
            "merge_oids": ["merge-a", "merge-b"],
            "completed_steps": [],
            "provider_receipts": [],
            "rebase_plans": [],
            "rebase_receipts": [],
        })),
    );
    let physical = attach_physical_rebuild(
        raw,
        &PhysicalRebuildOutcome {
            repository: Some(repository()),
            caravan_id: Some(PrNumber(1)),
            affected_prs: vec![PrNumber(2)],
            ..PhysicalRebuildOutcome::default()
        },
    );
    let scheduler = scheduler_failure_status(&physical);
    assert_eq!(
        scheduler.disposition,
        SchedulerDisposition::ExternalDecision
    );
    assert_eq!(scheduler.wake_class, SchedulerWakeClass::ExternalDecision);
    assert!(!scheduler.retryable);

    let attached = attach_scheduler_failure(&physical, &scheduler);
    let first_fingerprint = attached.details().unwrap()["decision_fingerprint"]
        .as_str()
        .unwrap()
        .to_owned();
    let repeated = attach_scheduler_failure(&physical, &scheduler);
    assert_eq!(
        repeated.details().unwrap()["decision_fingerprint"],
        first_fingerprint
    );
    let event = sync_failed_event(&attached).expect("external decision event");
    assert_eq!(event.caravan_id, Some(PrNumber(1)));
    assert_eq!(event.prs, vec![PrNumber(2)]);
    assert_eq!(event.metadata["decision_fingerprint"], first_fingerprint);
    let details = attached.details().unwrap();
    assert_eq!(details["retryable"], false);
    assert!(
        details["next"]
            .as_str()
            .unwrap()
            .contains("cannot succeed by retry")
    );
    assert_eq!(details["completed_steps"], json!([]));
    assert_eq!(details["provider_receipts"], json!([]));
}

#[test]
fn unsupported_exact_range_shapes_are_never_retry_ticks() {
    for code in [
        "rebase_nonlinear_range",
        "rebase_range_ambiguous",
        "rebase_empty_patch_range",
        "rebase_target_history_changed",
        "rebase_repository_not_owned",
        "rebase_historical_target_mismatch",
        "rebase_historical_parent_mismatch",
        "rebase_historical_source_mismatch",
        "rebase_unsupported_octopus",
        "rebase_topology_limit",
        "rebase_external_merge_parents",
        "rebase_cousin_history",
        "rebase_merge_tree_conflict",
        "rebase_merge_replay_conflict",
        "rebase_merge_tree_mismatch",
        "rebase_topology_changed",
    ] {
        let error = AppError::structured(
            ErrorCategory::Validation,
            code,
            "exact range decision",
            Some(json!({"repository": repository(), "pr": PrNumber(7)})),
        );
        let scheduler = scheduler_failure_status(&error);
        assert_eq!(
            scheduler.wake_class,
            SchedulerWakeClass::ExternalDecision,
            "{code}"
        );
        assert!(!scheduler.retryable, "{code}");
    }
}

#[test]
fn stale_run_generation_requires_fresh_trigger_and_is_never_rerunnable() {
    let mut pulls = healthy_chain();
    pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(10))];
    let matching = failed_run(10, &pulls[0]);
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider
        .failed_runs
        .borrow_mut()
        .insert(PrNumber(1), vec![matching]);
    provider
        .diagnostic_lineage
        .borrow_mut()
        .insert(PrNumber(1), selected_lineage_receipt(&pulls[0]));
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let error = execute(&status, &provider, false, true, false)
        .expect_err("stale generation cannot be rerun");

    assert!(provider.calls.borrow().is_empty());
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    let ci = &details["decision"]["evidence"]["ci"];
    assert_eq!(ci["rerunnable_run_ids"], json!([]));
    assert_eq!(ci["failure_diagnostics"][0]["generation"], "stale_head");
    assert_eq!(
        ci["failure_diagnostics"][0]["diagnostic"]["failed_jobs"][0]["selected_lineage"]["selected_commit"],
        "selected-merge"
    );
    assert_eq!(
        ci["failure_diagnostics"][0]["action"],
        "fresh_candidate_trigger"
    );
    assert!(
        details["decision"]["suggested_actions"][0]
            .as_str()
            .expect("action")
            .contains("fresh exact-candidate")
    );
}

#[test]
fn current_generation_source_failure_recommends_repair_not_rerun() {
    let mut pulls = healthy_chain();
    pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(10))];
    let matching = failed_run(10, &pulls[0]);
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider
        .failed_runs
        .borrow_mut()
        .insert(PrNumber(1), vec![matching]);
    provider
        .diagnostic_job_conclusions
        .borrow_mut()
        .insert(PrNumber(1), "failure".to_owned());
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let error = execute(&status, &provider, false, true, false)
        .expect_err("source failure requires repair");

    assert!(provider.calls.borrow().is_empty());
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    let ci = &details["decision"]["evidence"]["ci"];
    assert_eq!(ci["rerunnable_run_ids"], json!([]));
    assert_eq!(
        ci["failure_diagnostics"][0]["classification"],
        "source_or_test_failure"
    );
    assert_eq!(ci["failure_diagnostics"][0]["action"], "repair_source");
}

#[test]
fn unknown_provider_state_is_a_non_rerunnable_ci_decision() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].checks = vec![CheckSnapshot {
        name: "future-ci".to_owned(),
        state: CheckState::Unknown,
        provider_state: Some("FUTURE_PROVIDER_STATE".to_owned()),
        details_url: None,
    }];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let error = execute(&status, &provider, false, true, false)
        .expect_err("unknown CI cannot be guessed or rerun");

    assert_eq!(mcp_cli::StructuredError::code(&error), "ci_failure");
    assert!(provider.calls.borrow().is_empty());
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(
        details["decision"]["evidence"]["ci"]["checks"][0]["provider_state"],
        "FUTURE_PROVIDER_STATE"
    );
    assert_eq!(
        details["decision"]["evidence"]["ci"]["rerunnable_run_ids"],
        json!([])
    );
}

#[test]
fn rerun_failed_selects_only_exact_current_run_then_stops() {
    let mut pulls = healthy_chain();
    pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(10))];
    let matching = failed_run(10, &pulls[0]);
    let spurious = failed_run(11, &pulls[0]);
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider
        .failed_runs
        .borrow_mut()
        .insert(PrNumber(1), vec![spurious, matching]);
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let error = execute(&status, &provider, false, true, false)
        .expect_err("rerun still returns unresolved decision");

    assert_eq!(*provider.calls.borrow(), vec![MutationKind::RerunChecks]);
    assert_eq!(
        provider.pulls.borrow()[&PrNumber(1)].checks[0].state,
        CheckState::Queued
    );
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert!(
        details["decision"]["completed_steps"]
            .as_array()
            .expect("steps")
            .iter()
            .any(|step| step["summary"] == "reran failed jobs for exact workflow run 10")
    );
}

#[test]
fn forced_downstream_failure_remains_in_chain_without_blocking() {
    let mut pulls = healthy_chain();
    pulls[0].checks = vec![check("build-test", CheckState::Success, Some(1))];
    pulls[1].labels.insert("caravan-force".to_owned());
    pulls[1].checks = vec![check("build-test", CheckState::Failure, Some(20))];
    let run = failed_run(20, &pulls[1]);
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider
        .failed_runs
        .borrow_mut()
        .insert(PrNumber(2), vec![run]);
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress =
        execute(&status, &provider, false, false, true).expect("force bypasses downstream");

    assert_eq!(progress.ci[1].disposition, CiDisposition::Forced);
    assert_eq!(progress.current[&PrNumber(2)].state, PullRequestState::Open);
    assert!(progress.events.is_empty());
    assert!(provider.calls.borrow().is_empty());
}

#[test]
fn force_merge_requires_config_before_provider_attempt() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].labels.insert("caravan-force".to_owned());
    pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(30))];
    let run = failed_run(30, &pulls[0]);
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider
        .failed_runs
        .borrow_mut()
        .insert(PrNumber(1), vec![run]);
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let error =
        execute(&status, &provider, false, false, false).expect_err("config denies force merge");

    assert_eq!(mcp_cli::StructuredError::code(&error), "force_merge_denied");
    assert!(provider.calls.borrow().is_empty());
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(details["decision"]["evidence"]["events"], json!([]));
}

#[test]
fn force_head_with_auto_merge_off_uses_direct_admin_squash_without_auto_arm() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].labels.insert("caravan-force".to_owned());
    pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(31))];
    pulls[0].auto_merge = AutoMergeState::disabled();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    // If force handling ever tries to arm native auto-merge, model an immediate
    // provider-side merge/race as a hard failure. The audited admin squash must
    // be the only merge primitive reached.
    provider.fail_once(MutationKind::EnableAutoMerge);
    let initial_status = status(pulls, Some(PrNumber(1)), &clean);
    assert!(
        initial_status
            .analysis
            .fleet
            .problems
            .iter()
            .any(|problem| {
                problem.kind == GraphProblemKind::AutoMergeInvariant
                    && problem.prs == vec![PrNumber(1)]
            })
    );

    let progress = execute(&initial_status, &provider, false, false, true)
        .expect("disabled force head reaches direct admin squash");

    assert_eq!(
        *provider.calls.borrow(),
        vec![MutationKind::Comment, MutationKind::SquashMerge]
    );
    assert_eq!(
        progress.current[&PrNumber(1)].state,
        PullRequestState::Merged
    );
    assert_eq!(progress.events[0].kind, EventKind::ForceMergeAttempted);

    let final_pulls = provider.pulls.borrow().values().cloned().collect();
    let final_status = status(final_pulls, None, &clean);
    assert!(first_blocking_completion_problem(&final_status, &progress, true).is_none());
    let calls_before_replay = provider.calls.borrow().len();
    let replay = execute(&final_status, &provider, true, false, true)
        .expect("post-merge all-sync replay is a no-op");
    assert!(replay.provider_receipts.is_empty());
    assert_eq!(provider.calls.borrow().len(), calls_before_replay);
}

#[test]
fn unrelated_disabled_force_head_does_not_block_targeted_force_sync() {
    let mut unrelated = pull_request(
        1,
        "one",
        "main",
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    unrelated.labels.insert("caravan-force".to_owned());
    unrelated.checks = vec![check("build-test", CheckState::Failure, Some(41))];
    let mut selected = pull_request(
        2,
        "two",
        "main",
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    selected.labels.insert("caravan-force".to_owned());
    selected.checks = vec![check("build-test", CheckState::Failure, Some(42))];
    let pulls = vec![unrelated, selected];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let initial_status = status(pulls, Some(PrNumber(2)), &clean);
    let selected_caravan = initial_status
        .analysis
        .fleet
        .containing(PrNumber(2))
        .expect("selected caravan")
        .clone();
    let preflight_progress = SyncProgress::new(&initial_status, vec![PrNumber(2)], u32::MAX);
    validate_rebase_preflight_graph(
        &initial_status,
        std::slice::from_ref(&selected_caravan),
        &preflight_progress,
        true,
    )
    .expect("physical preflight scopes the unrelated force gap");

    let progress = execute(&initial_status, &provider, false, false, true)
        .expect("unrelated force gap does not block selected force head");

    assert_eq!(progress.ci.len(), 1);
    assert_eq!(progress.ci[0].pr, PrNumber(2));
    assert_eq!(
        provider.pulls.borrow()[&PrNumber(1)].state,
        PullRequestState::Open
    );
    assert_eq!(
        provider.pulls.borrow()[&PrNumber(2)].state,
        PullRequestState::Merged
    );
    assert_eq!(
        *provider.calls.borrow(),
        vec![MutationKind::Comment, MutationKind::SquashMerge]
    );

    let final_pulls = provider.pulls.borrow().values().cloned().collect();
    let final_status = status(final_pulls, Some(PrNumber(2)), &clean);
    assert!(final_status.analysis.fleet.problems.iter().any(|problem| {
        problem.kind == GraphProblemKind::AutoMergeInvariant && problem.prs == vec![PrNumber(1)]
    }));
    assert!(first_blocking_completion_problem(&final_status, &progress, true).is_none());
}

#[test]
fn unrelated_auto_merge_gap_without_force_policy_still_fails_closed() {
    let unrelated = pull_request(
        1,
        "one",
        "main",
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    let mut selected = pull_request(
        2,
        "two",
        "main",
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    selected.labels.insert("caravan-force".to_owned());
    selected.checks = vec![check("build-test", CheckState::Failure, Some(43))];
    let pulls = vec![unrelated, selected];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let initial_status = status(pulls, Some(PrNumber(2)), &clean);

    let error = execute(&initial_status, &provider, false, false, true)
        .expect_err("an unrelated ordinary head invariant stays strict");

    assert_eq!(error.code(), "invalid_graph");
    assert!(provider.calls.borrow().is_empty());
}

#[test]
fn stale_forced_head_stops_before_admin_attempt() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].labels.insert("caravan-force".to_owned());
    pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(32))];
    let run = failed_run(32, &pulls[0]);
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider
        .failed_runs
        .borrow_mut()
        .insert(PrNumber(1), vec![run]);
    let status = status(pulls, Some(PrNumber(1)), &clean);
    provider
        .pulls
        .borrow_mut()
        .get_mut(&PrNumber(1))
        .expect("head")
        .labels
        .insert("external-change".to_owned());

    let error =
        execute(&status, &provider, false, false, true).expect_err("stale head fails closed");

    assert_eq!(mcp_cli::StructuredError::code(&error), "stale_precondition");
    assert!(provider.calls.borrow().is_empty());
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(details["decision"]["evidence"]["events"], json!([]));
}

#[test]
fn moved_default_branch_invalidates_force_compatibility_proof() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].labels.insert("caravan-force".to_owned());
    pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(33))];
    let run = failed_run(33, &pulls[0]);
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider
        .failed_runs
        .borrow_mut()
        .insert(PrNumber(1), vec![run]);
    let status = status(pulls, Some(PrNumber(1)), &clean);
    *provider.branch_head.borrow_mut() = branch("moved-main").oid;

    let error = execute(&status, &provider, false, false, true)
        .expect_err("moved default invalidates proof");

    assert_eq!(mcp_cli::StructuredError::code(&error), "stale_precondition");
    assert!(provider.calls.borrow().is_empty());
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(details["decision"]["evidence"]["branch"], "main");
    assert_eq!(details["decision"]["evidence"]["events"], json!([]));
}

#[test]
fn force_comment_failure_is_structured_and_prevents_admin_merge() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].labels.insert("caravan-force".to_owned());
    pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(34))];
    let run = failed_run(34, &pulls[0]);
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider
        .failed_runs
        .borrow_mut()
        .insert(PrNumber(1), vec![run]);
    provider
        .failures
        .borrow_mut()
        .push_back(MutationKind::Comment);
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let error = execute(&status, &provider, false, false, true)
        .expect_err("comment is part of force receipt");

    assert_eq!(error.code(), "github_comment_failed");
    let details = error.details().expect("details");
    assert_eq!(details["stage"], "control_label_comment");
    assert_eq!(details["resumable"], true);
    assert_eq!(details["events"][0]["kind"], "force_merge_attempted");
    let extracted = crate::hooks::events_from_error(&error);
    assert_eq!(extracted.len(), 1);
    assert_eq!(extracted[0].kind, EventKind::ForceMergeAttempted);
    assert_eq!(
        json!(extracted[0].event_id),
        details["events"][0]["event_id"]
    );
    assert_eq!(*provider.calls.borrow(), vec![MutationKind::Comment]);
}

#[test]
fn force_merge_permission_denial_preserves_attempt_event() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].labels.insert("caravan-force".to_owned());
    pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(31))];
    let run = failed_run(31, &pulls[0]);
    let mut provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.admin_permission = false;
    provider
        .failed_runs
        .borrow_mut()
        .insert(PrNumber(1), vec![run]);
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let error =
        execute(&status, &provider, false, false, true).expect_err("permission denies force merge");

    assert_eq!(mcp_cli::StructuredError::code(&error), "force_merge_denied");
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(
        details["decision"]["evidence"]["events"][0]["kind"],
        "force_merge_attempted"
    );
    assert!(provider.calls.borrow().is_empty());
}

#[test]
fn physical_rewrite_invalidates_force_intent_bound_to_old_head_generation() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].labels.insert("caravan-force".to_owned());
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);
    let old_head = status.analysis.pull_requests[&PrNumber(1)].head.clone();
    let plan = crate::physical_rebase::RebasePlan {
        pr: PrNumber(1),
        branch: old_head.name.clone(),
        old_head_oid: old_head.oid.clone(),
        old_base_oid: status.analysis.fleet.default_branch.oid.clone(),
        range_source: crate::physical_rebase::PlannedRangeBase::RemoteBranch {
            branch: status.analysis.fleet.default_branch.clone(),
        },
        new_base: crate::physical_rebase::PlannedBase::Remote(
            status.analysis.fleet.default_branch.clone(),
        ),
        new_head_oid: CommitOid("rewritten0000000000000000000000000000000".to_owned()),
        new_tree_oid: CommitOid("tree000000000000000000000000000000000000".to_owned()),
        commit_count: 1,
        merge_topology: None,
        ci_trigger_workflows: vec!["CI".to_owned()],
        lease: format!("refs/heads/{}:{}", old_head.name, old_head.oid),
        already_satisfied: false,
    };
    let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], u32::MAX);

    invalidate_rewritten_force_intents(&status, &provider, &[plan], &mut progress)
        .expect("old-generation force intent is invalidated before rewrite");

    assert!(!progress.current[&PrNumber(1)].has_label("caravan-force"));
    assert_eq!(
        *provider.calls.borrow(),
        vec![MutationKind::RemoveLabel, MutationKind::Comment]
    );
    assert_eq!(progress.provider_receipts.len(), 2);
    let audits = provider.audits.borrow();
    assert_eq!(audits[0].operation, "force_invalidate_rewrite");
    assert!(audits[0].reason.contains(&old_head.oid.0));
    assert!(audits[0].reason.contains("rewritten"));
}

#[test]
fn already_satisfied_generation_preserves_explicit_force_intent() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].labels.insert("caravan-force".to_owned());
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);
    let head = status.analysis.pull_requests[&PrNumber(1)].head.clone();
    let plan = crate::physical_rebase::RebasePlan {
        pr: PrNumber(1),
        branch: head.name.clone(),
        old_head_oid: head.oid.clone(),
        old_base_oid: status.analysis.fleet.default_branch.oid.clone(),
        range_source: crate::physical_rebase::PlannedRangeBase::RemoteBranch {
            branch: status.analysis.fleet.default_branch.clone(),
        },
        new_base: crate::physical_rebase::PlannedBase::Remote(
            status.analysis.fleet.default_branch.clone(),
        ),
        new_head_oid: head.oid.clone(),
        new_tree_oid: CommitOid("tree000000000000000000000000000000000000".to_owned()),
        commit_count: 1,
        merge_topology: None,
        ci_trigger_workflows: vec!["CI".to_owned()],
        lease: format!("refs/heads/{}:{}", head.name, head.oid),
        already_satisfied: true,
    };
    let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], u32::MAX);

    invalidate_rewritten_force_intents(&status, &provider, &[plan], &mut progress)
        .expect("unchanged generation retains explicit force intent");

    assert!(progress.current[&PrNumber(1)].has_label("caravan-force"));
    assert!(provider.calls.borrow().is_empty());
    assert!(provider.audits.borrow().is_empty());
}

#[test]
fn fresh_force_reapplication_on_rewritten_generation_can_enter_force_path() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].labels.insert("caravan-force".to_owned());
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let initial_status = status(pulls, Some(PrNumber(1)), &clean);
    let head = initial_status.analysis.pull_requests[&PrNumber(1)]
        .head
        .clone();
    let rewritten_oid = CommitOid("rewritten0000000000000000000000000000000".to_owned());
    let plan = crate::physical_rebase::RebasePlan {
        pr: PrNumber(1),
        branch: head.name.clone(),
        old_head_oid: head.oid.clone(),
        old_base_oid: initial_status.analysis.fleet.default_branch.oid.clone(),
        range_source: crate::physical_rebase::PlannedRangeBase::RemoteBranch {
            branch: initial_status.analysis.fleet.default_branch.clone(),
        },
        new_base: crate::physical_rebase::PlannedBase::Remote(
            initial_status.analysis.fleet.default_branch.clone(),
        ),
        new_head_oid: rewritten_oid.clone(),
        new_tree_oid: CommitOid("tree000000000000000000000000000000000000".to_owned()),
        commit_count: 1,
        merge_topology: None,
        ci_trigger_workflows: vec!["CI".to_owned()],
        lease: format!("refs/heads/{}:{}", head.name, head.oid),
        already_satisfied: false,
    };
    let mut progress = SyncProgress::new(&initial_status, vec![PrNumber(1)], u32::MAX);
    invalidate_rewritten_force_intents(&initial_status, &provider, &[plan], &mut progress)
        .expect("old-generation force is consumed");

    let rewritten = {
        let mut provider_pulls = provider.pulls.borrow_mut();
        let rewritten = provider_pulls.get_mut(&PrNumber(1)).expect("head");
        rewritten.head.oid = rewritten_oid;
        rewritten.checks.clear();
        rewritten.labels.insert("caravan-force".to_owned());
        rewritten.clone()
    };
    let rewritten_status = status(vec![rewritten], Some(PrNumber(1)), &clean);

    let progress = execute(&rewritten_status, &provider, false, false, true)
        .expect("fresh force label on exact rewritten generation is accepted");

    assert_eq!(progress.ci[0].disposition, CiDisposition::Forced);
    assert_eq!(
        progress.current[&PrNumber(1)].state,
        PullRequestState::Merged
    );
    assert_eq!(
        *provider.calls.borrow(),
        vec![
            MutationKind::RemoveLabel,
            MutationKind::Comment,
            MutationKind::Comment,
            MutationKind::SquashMerge,
        ]
    );
}

#[test]
fn forced_head_bypasses_queued_expected_in_progress_and_empty_checks() {
    for checks in [
        vec![check("build-test", CheckState::Queued, Some(40))],
        vec![check("build-test", CheckState::Expected, None)],
        vec![check("build-test", CheckState::InProgress, Some(41))],
        vec![],
    ] {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        pulls[0].checks = checks;
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);

        let progress = execute(&status, &provider, false, false, true)
            .expect("explicit force bypasses every non-successful CI state");

        assert_eq!(progress.ci[0].disposition, CiDisposition::Forced);
        assert_eq!(
            *provider.calls.borrow(),
            vec![MutationKind::Comment, MutationKind::SquashMerge]
        );
        assert_eq!(
            progress.current[&PrNumber(1)].state,
            PullRequestState::Merged
        );
    }
}

#[test]
fn forced_head_bypasses_mixed_pending_and_failed_checks_with_accurate_audit() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].labels.insert("caravan-force".to_owned());
    pulls[0].checks = vec![
        check("build-test", CheckState::Failure, Some(42)),
        check("security", CheckState::InProgress, Some(43)),
    ];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress = execute(&status, &provider, false, false, true)
        .expect("explicit force bypasses mixed pending and failed checks");

    assert_eq!(progress.ci[0].disposition, CiDisposition::Forced);
    let audits = provider.audits.borrow();
    assert_eq!(audits.len(), 1);
    assert!(audits[0].reason.contains("observed checks"));
    assert!(audits[0].reason.contains("INPROGRESS"));
    assert!(audits[0].reason.contains("FAILURE"));
    assert!(!audits[0].reason.contains("failed checks:"));
}

#[test]
fn passing_checks_with_stale_force_label_use_normal_auto_merge() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].labels.insert("caravan-force".to_owned());
    pulls[0].checks = vec![check("build-test", CheckState::Success, Some(44))];
    pulls[0].auto_merge = AutoMergeState::disabled();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress = execute(&status, &provider, false, false, true)
        .expect("successful CI does not invoke exceptional force");

    assert_eq!(progress.ci[0].disposition, CiDisposition::Passing);
    assert_eq!(
        *provider.calls.borrow(),
        vec![MutationKind::EnableAutoMerge]
    );
    assert!(progress.events.is_empty());
    assert!(provider.audits.borrow().is_empty());
}

#[test]
fn successful_force_merge_is_one_shot_and_advances_child() {
    let mut pulls = healthy_chain();
    pulls[0].auto_merge = AutoMergeState::disabled();
    pulls[0].labels.insert("caravan-force".to_owned());
    pulls[0].checks = vec![check("build-test", CheckState::Failure, Some(50))];
    pulls[1].labels.insert("caravan-force".to_owned());
    pulls[1].checks = vec![check("build-test", CheckState::Failure, Some(51))];
    pulls[2].checks = vec![check("build-test", CheckState::Success, Some(52))];
    let head_run = failed_run(50, &pulls[0]);
    let child_run = failed_run(51, &pulls[1]);
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.failed_runs.borrow_mut().extend([
        (PrNumber(1), vec![head_run]),
        (PrNumber(2), vec![child_run]),
    ]);
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress = execute(&status, &provider, false, false, true).expect("force merge succeeds");

    assert_eq!(
        progress.current[&PrNumber(1)].state,
        PullRequestState::Merged
    );
    assert_eq!(progress.current[&PrNumber(2)].state, PullRequestState::Open);
    assert_eq!(progress.current[&PrNumber(2)].base.name, "main");
    assert_eq!(
        progress.current[&PrNumber(2)].auto_merge,
        AutoMergeState::squash()
    );
    assert_eq!(
        *provider.calls.borrow(),
        vec![
            MutationKind::Comment,
            MutationKind::SquashMerge,
            MutationKind::SetBase,
            MutationKind::EnableAutoMerge,
        ]
    );
    assert_eq!(progress.head_advancements[0].new_head, PrNumber(2));
    assert_eq!(
        progress
            .events
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        vec![
            EventKind::ForceMergeAttempted,
            EventKind::ForceMergeCompleted,
            EventKind::HeadAdvanced,
        ]
    );
    assert_eq!(
        progress.events[0].operation_id,
        progress.events[1].operation_id
    );
}

#[test]
fn dead_owner_recovery_is_preserved_on_later_sync_error() {
    let recovery = OperationLockRecovery {
        path: ".git/caravan/operation.lock".to_owned(),
        removed_owner: crate::operation_lock::OperationLockOwner {
            version: 1,
            pid: 99,
            operation: "sync_decision_checkout".to_owned(),
            created_unix_secs: 1,
            token: "exact-token".to_owned(),
            checkpoint: Some(crate::operation_lock::OperationLockCheckpoint {
                phase: "decision_checkout_in_flight".to_owned(),
                updated_unix_ms: 2,
                evidence: json!({ "pr": 2008 }),
                provider_state_indeterminate: false,
            }),
        },
        age_secs: 3,
        owner_alive: false,
        token_verified: true,
    };
    let error = AppError::validation("repository_not_initialized", "repair init");

    let error = attach_lock_recovery(error, Some(&recovery));

    let details = error.details().unwrap();
    assert_eq!(
        details["lock_recovery"]["removed_owner"]["token"],
        "exact-token"
    );
    assert_eq!(
        details["lock_recovery"]["removed_owner"]["checkpoint"]["phase"],
        "decision_checkout_in_flight"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn sync_lock_checkpoint_stays_bounded_for_large_fleet_receipts() {
    let pulls = healthy_chain();
    let status = status(pulls.clone(), Some(PrNumber(1)), &clean);
    let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], u32::MAX);
    let template = pulls[0].clone();
    for index in 0..1_000_u64 {
        let pr = PrNumber(index + 1);
        progress.steps.push(MutationStep {
            kind: MutationKind::SetBase,
            state: MutationStepState::Completed,
            pr: Some(pr),
            summary: "large historical step evidence ".repeat(32),
        });
        progress
            .rebase_plans
            .push(crate::physical_rebase::RebasePlan {
                pr,
                branch: format!("feature-{index}"),
                old_head_oid: CommitOid(format!("old-{index}")),
                old_base_oid: CommitOid(format!("base-{index}")),
                range_source: crate::physical_rebase::PlannedRangeBase::RemoteBranch {
                    branch: branch(&format!("base-{index}")),
                },
                new_base: crate::physical_rebase::PlannedBase::Remote(branch(&format!(
                    "target-{index}"
                ))),
                new_head_oid: CommitOid(format!("new-{index}")),
                new_tree_oid: CommitOid(format!("tree-{index}")),
                commit_count: 1,
                merge_topology: None,
                ci_trigger_workflows: (0..32)
                    .map(|workflow| format!(".github/workflows/{workflow}.yml"))
                    .collect(),
                lease: format!("--force-with-lease=refs/heads/feature-{index}:old-{index}"),
                already_satisfied: false,
            });
        progress
            .rebase_receipts
            .push(crate::physical_rebase::RebaseReceipt {
                pr,
                branch: format!("feature-{index}"),
                old_head_oid: CommitOid(format!("old-{index}")),
                new_head_oid: CommitOid(format!("new-{index}")),
                old_base_oid: CommitOid(format!("base-{index}")),
                new_base_branch: format!("target-{index}"),
                new_base_oid: CommitOid(format!("target-oid-{index}")),
                new_tree_oid: CommitOid(format!("tree-{index}")),
                commit_count: 1,
                merge_topology: None,
                ci_trigger_workflows: Vec::new(),
                lease: format!("--force-with-lease=refs/heads/feature-{index}:old-{index}"),
                already_satisfied: false,
            });
        let mut after = template.clone();
        after.number = pr;
        after.labels = (0..64).map(|label| format!("label-{label}")).collect();
        after.checks = (0..64)
            .map(|check| CheckSnapshot {
                name: format!("check-{check}"),
                state: CheckState::Success,
                provider_state: Some("SUCCESS".to_owned()),
                details_url: None,
            })
            .collect();
        progress.provider_receipts.push(GitHubMutationReceipt {
            kind: MutationKind::SetBase,
            before: Some(after.clone()),
            after,
            provider_output: Some("large provider output".repeat(64)),
        });
        progress.events.push(progress.event(
            EventKind::HeadAdvanced,
            Some(pr),
            vec![pr; 64],
            Some("large event reason".repeat(64)),
            BTreeMap::new(),
        ));
    }

    let evidence = sync_checkpoint_evidence(&progress);
    let encoded = serde_json::to_vec(&evidence).unwrap();

    assert!(encoded.len() < 12 * 1024, "{} bytes", encoded.len());
    assert_eq!(evidence["schema_version"], 2);
    for key in [
        "affected_prs",
        "steps",
        "rebase_plans",
        "rebase_receipts",
        "provider_receipts",
        "events",
    ] {
        assert_eq!(evidence[key]["count"], 1_000, "{key}");
        assert_eq!(evidence[key]["sample"].as_array().unwrap().len(), 4);
        assert_eq!(evidence[key]["truncated"], 996);
        assert!(
            evidence[key]["hash"]
                .as_str()
                .unwrap()
                .starts_with("fnv1a64:")
        );
    }
    assert_eq!(evidence["rebase_plans"]["sample"][0]["pr"], 1);
    assert_eq!(evidence["rebase_plans"]["sample"][3]["pr"], 1_000);
}

#[test]
fn whole_sync_budget_uses_the_explicit_validated_wall_clock_bound() {
    let mut context = AppContext::default();
    assert_eq!(sync_operation_budget(&context), Duration::from_secs(120));
    context.config.sync.max_duration_secs = 10;
    assert_eq!(sync_operation_budget(&context), Duration::from_secs(10));
    context.config.sync.max_duration_secs = 3_600;
    assert_eq!(sync_operation_budget(&context), Duration::from_secs(3_600));
}

#[test]
fn physical_commit_budget_scales_with_chain_and_fails_before_any_write() {
    let mut pulls = healthy_chain();
    pulls.truncate(2);
    let status = status(pulls.clone(), Some(PrNumber(1)), &clean);
    let provider = FakeProvider::with_pull_requests(pulls);
    let selected = status.analysis.fleet.caravans.clone();
    let mut context = AppContext::default();
    context.config.command_timeout_secs = 10;
    let budget = physical_commit_budget(&context, &status, &selected);
    assert_eq!(budget.command_slots, 13);
    assert_eq!(budget.required, Duration::from_secs(130));
    assert_eq!(budget.mutation_reserve, 7);
    let plan = crate::physical_rebase::RebasePlan {
        pr: PrNumber(1),
        branch: "one".to_owned(),
        old_head_oid: branch("one").oid,
        old_base_oid: branch("main").oid.clone(),
        range_source: crate::physical_rebase::PlannedRangeBase::RemoteBranch {
            branch: branch("main"),
        },
        new_base: crate::physical_rebase::PlannedBase::Remote(branch("main")),
        new_head_oid: CommitOid("rewritten0000000000000000000000000000000".to_owned()),
        new_tree_oid: CommitOid("tree000000000000000000000000000000000000".to_owned()),
        commit_count: 1,
        merge_topology: None,
        ci_trigger_workflows: vec!["CI".to_owned()],
        lease: "--force-with-lease=refs/heads/one:one".to_owned(),
        already_satisfied: false,
    };

    let plans = vec![plan];
    let operation_deadline = Instant::now() + Duration::from_secs(129);
    physical_precommit_deadline(
        &context,
        operation_deadline,
        budget,
        &plans,
        "physical_rebase_commit_admission",
    )
    .expect_err("the complete apply reserve must remain before commitment");
    let error = physical_budget_failure(
        &context,
        &status,
        operation_deadline,
        budget,
        plans,
        "physical_rebase_commit_admission",
    );

    assert_eq!(error.code(), "physical_sync_budget_insufficient");
    let details = error.details().expect("budget evidence");
    assert_eq!(details["required_ms"], 130_000);
    assert_eq!(details["prepared_plan_count"], 1);
    assert!(
        details["prepared_plan_hash"]
            .as_str()
            .unwrap()
            .starts_with("fnv1a64:")
    );
    assert_eq!(details["provider_mutations"], 0);
    assert_eq!(details["branch_mutations"], 0);
    assert_eq!(details["rebase_plans"].as_array().unwrap().len(), 1);
    assert!(
        details["next"]
            .as_str()
            .unwrap()
            .contains("increase sync.max_duration_secs")
    );
    assert_eq!(details["retryable"], false);
    assert!(provider.calls.borrow().is_empty());
    let scheduler = scheduler_failure_status(&error);
    assert_eq!(scheduler.disposition, SchedulerDisposition::OperatorAction);
    assert_eq!(scheduler.wake_class, SchedulerWakeClass::OperatorAction);
    assert!(!scheduler.retryable);

    physical_precommit_deadline(
        &context,
        Instant::now() + Duration::from_secs(131),
        budget,
        &[],
        "physical_rebase_planning",
    )
    .expect("sufficient whole-tick budget retains a planning phase");
}

#[test]
fn command_timeout_longer_than_tick_cannot_enter_physical_mutation() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    let status = status(pulls, Some(PrNumber(1)), &clean);
    let mut context = AppContext::default();
    context.config.command_timeout_secs = 300;
    context.config.sync.max_duration_secs = 120;
    let budget = physical_commit_budget(&context, &status, &status.analysis.fleet.caravans);

    let error = physical_precommit_deadline(
        &context,
        Instant::now() + sync_operation_budget(&context),
        budget,
        &[],
        "physical_rebase_planning",
    )
    .expect_err("a child timeout cannot exceed the mutation-phase reserve");

    assert_eq!(error.code(), "physical_sync_budget_insufficient");
    assert!(error.details().unwrap()["required_ms"].as_u64().unwrap() > 120_000);
}

#[test]
fn decision_checkout_targets_the_repair_pr_only_when_unambiguous() {
    let decision = |kind, affected_prs| DecisionPoint {
        kind,
        operation_id: OperationId::new(),
        repository: repository(),
        caravan_id: Some(PrNumber(1)),
        affected_prs,
        message: "repair".to_owned(),
        evidence: BTreeMap::new(),
        completed_steps: Vec::new(),
        resumable: true,
        suggested_actions: Vec::new(),
    };

    assert_eq!(
        decision_checkout_target(&decision(DecisionKind::HeadConflict, vec![PrNumber(1)])),
        Some(PrNumber(1))
    );
    assert_eq!(
        decision_checkout_target(&decision(
            DecisionKind::LinkConflict,
            vec![PrNumber(1), PrNumber(2)]
        )),
        Some(PrNumber(2))
    );
    assert_eq!(
        decision_checkout_target(&decision(
            DecisionKind::CrossCaravanConflict,
            vec![PrNumber(1), PrNumber(4)]
        )),
        None
    );
}

#[test]
fn unsafe_decision_checkout_preserves_the_original_decision_error() {
    let temp = tempfile::tempdir().unwrap();
    let context = AppContext {
        repository_path: temp.path().to_path_buf(),
        config_path: temp.path().join("config.yaml"),
        config_existed: false,
        config: crate::config::CaravanConfig::default(),
    };
    let pull_request = pull_request(
        1,
        "one",
        "main",
        PullRequestState::Open,
        AutoMergeState::squash(),
    );
    let decision = DecisionPoint {
        kind: DecisionKind::CiFailure,
        operation_id: OperationId::new(),
        repository: repository(),
        caravan_id: Some(PrNumber(1)),
        affected_prs: vec![PrNumber(1)],
        message: "repair".to_owned(),
        evidence: BTreeMap::from([("pull_request".to_owned(), json!(pull_request))]),
        completed_steps: Vec::new(),
        resumable: true,
        suggested_actions: Vec::new(),
    };
    let error = AppError::structured(
        ErrorCategory::Validation,
        "ci_failure",
        "repair",
        Some(json!({ "decision": decision })),
    );

    let error = checkout_for_decision(&context, error, Instant::now() + Duration::from_secs(1));

    assert_eq!(error.code(), "ci_failure");
    let details = error.details().unwrap();
    assert_eq!(details["checkout"]["state"], "skipped");
    assert_eq!(details["checkout"]["pr"], 1);
    assert_eq!(
        details["checkout"]["error"]["code"],
        "git_repository_not_found"
    );
}

#[test]
fn repeated_healthy_sync_is_a_noop_with_explicit_steps() {
    let pulls = healthy_chain();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(2)), &clean);

    let progress = execute(&status, &provider, false, false, false).expect("sync converges");

    assert!(!progress.operation_receipt().changed);
    assert!(provider.calls.borrow().is_empty());
    assert_eq!(progress.synchronized_caravans, vec![PrNumber(1)]);
    assert_eq!(progress.steps.len(), 4);
}

#[test]
fn merged_head_advances_child_and_rolls_caravan_id() {
    let pulls = vec![
        pull_request(
            1,
            "one",
            "main",
            PullRequestState::Merged,
            AutoMergeState::disabled(),
        ),
        pull_request(
            2,
            "two",
            "one",
            PullRequestState::Open,
            AutoMergeState::disabled(),
        ),
        pull_request(
            3,
            "three",
            "two",
            PullRequestState::Open,
            AutoMergeState::disabled(),
        ),
    ];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(2)), &clean);

    let progress = execute(&status, &provider, false, false, false).expect("advancement converges");

    assert!(progress.operation_receipt().changed);
    assert_eq!(
        progress.head_advancements,
        vec![HeadAdvancement {
            merged_predecessor: PrNumber(1),
            new_head: PrNumber(2),
            previous_caravan_id: PrNumber(1),
            new_caravan_id: PrNumber(2),
        }]
    );
    let pulls = provider.pulls.borrow();
    assert_eq!(pulls[&PrNumber(2)].base.name, "main");
    assert_eq!(pulls[&PrNumber(2)].auto_merge, AutoMergeState::squash());
    assert!(!pulls[&PrNumber(3)].auto_merge.enabled);
}

#[test]
fn interrupted_advancement_reports_receipt_and_rerun_resumes() {
    let pulls = vec![
        pull_request(
            1,
            "one",
            "main",
            PullRequestState::Merged,
            AutoMergeState::disabled(),
        ),
        pull_request(
            2,
            "two",
            "one",
            PullRequestState::Open,
            AutoMergeState::disabled(),
        ),
    ];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.fail_once(MutationKind::EnableAutoMerge);
    let initial = status(pulls, Some(PrNumber(2)), &clean);

    let error = execute(&initial, &provider, false, false, false).expect_err("enable fails");
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(details["operation_receipt"]["changed"], true);
    assert_eq!(provider.pulls.borrow()[&PrNumber(2)].base.name, "main");

    let resumed_pulls: Vec<_> = provider.pulls.borrow().values().cloned().collect();
    let resumed = status(resumed_pulls, Some(PrNumber(2)), &clean);
    let progress = execute(&resumed, &provider, false, false, false).expect("rerun resumes");
    assert!(progress.operation_receipt().changed);
    assert_eq!(
        provider.pulls.borrow()[&PrNumber(2)].auto_merge,
        AutoMergeState::squash()
    );
}

#[test]
fn head_conflict_stops_before_mutation_with_exact_evidence() {
    let conflict = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
        Ok(CompatibilityReport {
            candidate: candidate.clone(),
            target: target.clone(),
            outcome: CompatibilityOutcome::Conflict,
            conflicting_paths: vec!["src/lib.rs".to_owned()],
            diagnostic: Some("merge-tree conflict".to_owned()),
        })
    };
    let pulls = healthy_chain();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &conflict);

    let error = execute(&status, &provider, false, false, false).expect_err("conflict decides");

    assert_eq!(mcp_cli::StructuredError::code(&error), "head_conflict");
    assert!(provider.calls.borrow().is_empty());
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(details["decision"]["affected_prs"], json!([1]));
    assert_eq!(
        details["decision"]["evidence"]["compatibility"][0]["conflicting_paths"],
        json!(["src/lib.rs"])
    );
}

#[test]
fn caravan_force_never_bypasses_textual_conflict() {
    let conflict = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
        Ok(CompatibilityReport {
            candidate: candidate.clone(),
            target: target.clone(),
            outcome: CompatibilityOutcome::Conflict,
            conflicting_paths: vec!["src/conflict.rs".to_owned()],
            diagnostic: None,
        })
    };
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].labels.insert("caravan-force".to_owned());
    pulls[0].checks = vec![check("build-test", CheckState::Expected, None)];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &conflict);

    let error =
        execute(&status, &provider, false, false, true).expect_err("force cannot bypass conflict");

    assert_eq!(mcp_cli::StructuredError::code(&error), "head_conflict");
    assert!(provider.calls.borrow().is_empty());
}

#[test]
fn mutation_budget_stops_before_the_next_provider_write() {
    let pulls = healthy_chain();
    let status = status(pulls.clone(), Some(PrNumber(1)), &clean);
    let provider = FakeProvider::with_pull_requests(pulls);
    let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], 1);
    progress.steps.push(MutationStep {
        kind: MutationKind::Comment,
        state: MutationStepState::Completed,
        pr: Some(PrNumber(1)),
        summary: "prior mutation".to_owned(),
    });

    let error = progress
        .ensure_auto_merge_disabled(&provider, &repository(), PrNumber(1))
        .expect_err("budget exhaustion must precede a provider write");

    assert_eq!(error.code(), "sync_mutation_budget_exhausted");
    assert!(provider.calls.borrow().is_empty());
    assert_eq!(error.details().unwrap()["used"], 1);
}

#[test]
fn mutation_timeout_preserves_category_and_completed_steps() {
    let pulls = healthy_chain();
    let status = status(pulls, Some(PrNumber(1)), &clean);
    let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], u32::MAX);
    progress.steps.push(MutationStep {
        kind: MutationKind::SetBase,
        state: MutationStepState::Completed,
        pr: Some(PrNumber(1)),
        summary: "base advanced".to_owned(),
    });
    let error = mutation_error(
        &MutationError::Provider(DiscoveryError::Runner(CommandRunError::Timeout {
            command: crate::command::CommandSpec::new("gh").args(["pr", "merge"]),
            process_group_id: None,
            timeout_ms: 1_200,
            stdout: "partial".to_owned(),
            stderr: "stalled".to_owned(),
        })),
        &progress,
        Some(PrNumber(1)),
    );

    assert_eq!(
        mcp_cli::StructuredError::category(&error),
        ErrorCategory::Timeout
    );
    assert_eq!(
        mcp_cli::StructuredError::code(&error),
        "github_mutation_timeout"
    );
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(details["timeout_ms"], 1_200);
    assert_eq!(
        details["operation_receipt"]["completed_steps"][0]["summary"],
        "base advanced"
    );
}

#[test]
fn stale_provider_facts_stop_with_a_resumable_decision() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].auto_merge = AutoMergeState::disabled();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);
    provider
        .pulls
        .borrow_mut()
        .get_mut(&PrNumber(1))
        .unwrap()
        .labels
        .insert("external-change".to_owned());

    let error = execute(&status, &provider, false, false, false).expect_err("race stops");

    assert_eq!(mcp_cli::StructuredError::code(&error), "stale_precondition");
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(details["decision"]["kind"], "stale_precondition");
    assert_eq!(details["decision"]["resumable"], true);
    assert_eq!(
        details["decision"]["evidence"]["changed_fields"],
        json!(["fake_race"])
    );
}

#[test]
fn sync_all_processes_caravans_in_head_number_order() {
    let pulls = vec![
        pull_request(
            10,
            "ten",
            "main",
            PullRequestState::Open,
            AutoMergeState::disabled(),
        ),
        pull_request(
            2,
            "two",
            "main",
            PullRequestState::Open,
            AutoMergeState::disabled(),
        ),
    ];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, None, &clean);

    let progress = execute(&status, &provider, true, false, false).expect("all converges");

    assert_eq!(
        progress.synchronized_caravans,
        vec![PrNumber(2), PrNumber(10)]
    );
    assert_eq!(
        progress
            .provider_receipts
            .iter()
            .map(|receipt| receipt.after.number)
            .collect::<Vec<_>>(),
        vec![PrNumber(2), PrNumber(10)]
    );
}

#[test]
fn adjacent_conflict_is_a_link_decision() {
    let checker = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
        Ok(CompatibilityReport {
            candidate: candidate.clone(),
            target: target.clone(),
            outcome: if target.name == "main" {
                CompatibilityOutcome::Clean
            } else {
                CompatibilityOutcome::Conflict
            },
            conflicting_paths: vec!["src/link.rs".to_owned()],
            diagnostic: None,
        })
    };
    let pulls = healthy_chain();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(2)), &checker);

    let error = execute(&status, &provider, false, false, false).expect_err("link decides");

    assert_eq!(mcp_cli::StructuredError::code(&error), "link_conflict");
    let details = error.details().expect("decision details");
    assert_eq!(
        details["decision"]["evidence"]["rebase_on_join"]["state"],
        "disabled"
    );
    assert!(
        details["decision"]["message"]
            .as_str()
            .expect("message")
            .contains("rebase_on_join=disabled")
    );
    assert!(
        details["decision"]["suggested_actions"][0]
            .as_str()
            .expect("action")
            .contains("rebase_on_join: true")
    );
    assert!(provider.calls.borrow().is_empty());
}

#[test]
fn sync_all_skips_paused_caravan_and_progresses_independent_caravan() {
    let mut pulls = healthy_chain();
    pulls[0].auto_merge = AutoMergeState::disabled();
    pulls[2].base = branch("main");
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let mut status = status(pulls, Some(PrNumber(1)), &clean);
    let head = status.analysis.pull_requests[&PrNumber(1)].clone();
    let record = crate::pause::PauseRecord {
        version: 1,
        caravan_head: PrNumber(1),
        members: vec![PrNumber(1), PrNumber(2)],
        expected_head: {
            let mut expected = PullRequestPrecondition::from(&head);
            expected.auto_merge = AutoMergeState::squash();
            expected
        },
        expected_checks: head.checks.clone(),
        actor: "oncall".to_owned(),
        reason: "incident".to_owned(),
        paused_unix_secs: 1,
        expires_unix_secs: None,
        external_reference: Some("INC-1".to_owned()),
        resume_authorized_by: None,
    };
    status.pauses.push(crate::pause::PauseStatus {
        record,
        state: crate::pause::PauseState::Active,
        auto_merge_suspended: true,
        safe_next_action: "explicit resume".to_owned(),
    });
    status.analysis.fleet.problems.retain(|problem| {
        !(problem.kind == GraphProblemKind::AutoMergeInvariant && problem.prs == vec![PrNumber(1)])
    });

    let progress =
        execute(&status, &provider, true, false, false).expect("independent caravan progresses");

    assert_eq!(progress.synchronized_caravans, vec![PrNumber(3)]);
    assert_eq!(progress.paused_caravans.len(), 1);
    assert_eq!(
        *provider.calls.borrow(),
        vec![MutationKind::EnableAutoMerge]
    );
    assert!(
        progress
            .steps
            .iter()
            .any(|step| step.summary.contains("intentionally paused"))
    );
}

#[test]
fn externally_enabled_non_head_is_disabled_before_head_repair() {
    let mut pulls = healthy_chain();
    pulls[0].auto_merge = AutoMergeState::disabled();
    pulls[1].auto_merge = AutoMergeState::squash();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);

    execute(&status, &provider, false, false, false).expect("sync repairs shape");

    assert_eq!(
        *provider.calls.borrow(),
        vec![
            MutationKind::DisableAutoMerge,
            MutationKind::EnableAutoMerge
        ]
    );
}
