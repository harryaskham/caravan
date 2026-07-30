//! Hermetic sync policy, decision, force, CI, and receipt fixtures.
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::process::Command;

use super::*;
use crate::graph;
use crate::model::{
    AutoMergeState, BranchSnapshot, CheckSnapshot, CommitOid, CompatibilityReport,
    RepositorySnapshot,
};
use crate::required_runs::{
    CheckSuiteLineage, HeadRunLineage, MissingRequiredRunsKind, RequiredContextsRead,
    RequiredRunsStatus, WorkflowRunLineage,
};

/// Provider timestamp used by every hermetic head in this fixture set. It is
/// far enough in the past that the default grace period has always elapsed.
const PUBLISHED_AT: &str = "2020-01-01T00:00:00Z";

#[allow(clippy::struct_excessive_bools)]
struct FakeProvider {
    allows_auto_merge: bool,
    allows_squash_merge: bool,
    branch_protected: bool,
    /// Squash merges the provider accepts but never exposes as merged.
    unpersisted_merges: RefCell<BTreeMap<PrNumber, u32>>,
    /// Merge commit reported for a merged PR, keyed by PR number.
    merge_commits: RefCell<BTreeMap<PrNumber, CommitOid>>,
    /// Merge commits the fetched default branch does *not* contain.
    unreachable_merges: RefCell<BTreeSet<CommitOid>>,
    /// Default-branch head observed after each caravan-owned merge.
    default_branch_head_after_merge: RefCell<Option<CommitOid>>,
    pulls: RefCell<BTreeMap<PrNumber, PullRequestSnapshot>>,
    /// Scripted stale provider list/read responses served before live facts.
    refetch_overrides: RefCell<BTreeMap<PrNumber, VecDeque<PullRequestSnapshot>>>,
    /// Arming requests the provider accepts but silently never persists.
    unpersisted_armings: RefCell<BTreeMap<PrNumber, u32>>,
    refetches: RefCell<Vec<PrNumber>>,
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
    /// Protection-declared required contexts, keyed by branch name.
    required_contexts: RefCell<BTreeMap<String, RequiredContextsRead>>,
    /// Head lineage served for a PR, keyed by PR number.
    head_lineage: RefCell<BTreeMap<PrNumber, VecDeque<HeadRunLineage>>>,
    /// Every PR whose head lineage was actually read, in order.
    lineage_reads: RefCell<Vec<PrNumber>>,
    /// Check suites the provider will accept a rerequest for.
    rerequestable_suites: RefCell<BTreeMap<u64, String>>,
    /// Check-suite rerequests observed, in order.
    rerequests: RefCell<Vec<(PrNumber, u64)>>,
}

impl FakeProvider {
    fn with_pull_requests(pulls: Vec<PullRequestSnapshot>) -> Self {
        Self {
            allows_auto_merge: true,
            allows_squash_merge: true,
            branch_protected: true,
            unpersisted_merges: RefCell::new(BTreeMap::new()),
            merge_commits: RefCell::new(BTreeMap::new()),
            unreachable_merges: RefCell::new(BTreeSet::new()),
            default_branch_head_after_merge: RefCell::new(None),
            pulls: RefCell::new(
                pulls
                    .into_iter()
                    .map(|pull_request| (pull_request.number, pull_request))
                    .collect(),
            ),
            failures: RefCell::new(VecDeque::new()),
            refetch_overrides: RefCell::new(BTreeMap::new()),
            unpersisted_armings: RefCell::new(BTreeMap::new()),
            refetches: RefCell::new(Vec::new()),
            calls: RefCell::new(Vec::new()),
            failed_runs: RefCell::new(BTreeMap::new()),
            diagnostic_heads: RefCell::new(BTreeMap::new()),
            diagnostic_job_conclusions: RefCell::new(BTreeMap::new()),
            diagnostic_lineage: RefCell::new(BTreeMap::new()),
            admin_permission: true,
            branch_head: RefCell::new(branch("main").oid),
            audits: RefCell::new(Vec::new()),
            comments: RefCell::new(BTreeMap::new()),
            required_contexts: RefCell::new(BTreeMap::new()),
            head_lineage: RefCell::new(BTreeMap::new()),
            lineage_reads: RefCell::new(Vec::new()),
            rerequestable_suites: RefCell::new(BTreeMap::new()),
            rerequests: RefCell::new(Vec::new()),
        }
    }

    /// Declare protection-required contexts for one base branch.
    fn require_contexts(&self, branch: &str, contexts: &[&str]) {
        self.required_contexts.borrow_mut().insert(
            branch.to_owned(),
            RequiredContextsRead {
                branch: branch.to_owned(),
                protected: true,
                contexts: contexts.iter().map(|value| (*value).to_owned()).collect(),
                complete: true,
            }
            .normalized(),
        );
    }

    /// Declare an unreadable protection endpoint for one base branch.
    fn partial_contexts(&self, branch: &str) {
        self.required_contexts
            .borrow_mut()
            .insert(branch.to_owned(), RequiredContextsRead::partial(branch));
    }

    /// Queue one lineage response; the last queued response repeats.
    fn serve_lineage(&self, number: PrNumber, lineage: HeadRunLineage) {
        self.head_lineage
            .borrow_mut()
            .entry(number)
            .or_default()
            .push_back(lineage);
    }

    /// Allow one check-suite rerequest against the given head.
    fn allow_rerequest(&self, check_suite_id: u64, head_sha: &str) {
        self.rerequestable_suites
            .borrow_mut()
            .insert(check_suite_id, head_sha.to_owned());
    }

    fn fail_once(&self, kind: MutationKind) {
        self.failures.borrow_mut().push_back(kind);
    }

    /// Serve one stale provider generation before live facts are exposed.
    fn serve_stale_read(&self, number: PrNumber, stale: PullRequestSnapshot) {
        self.refetch_overrides
            .borrow_mut()
            .entry(number)
            .or_default()
            .push_back(stale);
    }

    /// Accept `count` arming requests without ever persisting auto-merge.
    fn never_persist_arming(&self, number: PrNumber, count: u32) {
        self.unpersisted_armings.borrow_mut().insert(number, count);
    }

    /// Accept `count` squash merges without ever exposing a merged PR.
    fn never_persist_merge(&self, number: PrNumber, count: u32) {
        self.unpersisted_merges.borrow_mut().insert(number, count);
    }

    /// Report a merge commit the fetched default branch does not contain.
    fn serve_unreachable_merge(&self, number: PrNumber, merge_commit: &str) {
        let oid = CommitOid(merge_commit.to_owned());
        self.merge_commits.borrow_mut().insert(number, oid.clone());
        self.unreachable_merges.borrow_mut().insert(oid);
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
        if !actual.mutation_identity_eq(expected) {
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
        if !actual_precondition.mutation_identity_eq(expected) {
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
        self.refetches.borrow_mut().push(number);
        if let Some(stale) = self
            .refetch_overrides
            .borrow_mut()
            .get_mut(&number)
            .and_then(VecDeque::pop_front)
        {
            return Ok(stale);
        }
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

    fn repository_allows_squash_merge(
        &self,
        _repository: &RepositoryId,
    ) -> Result<bool, MutationError> {
        Ok(self.allows_squash_merge)
    }

    fn branch_head_oid(
        &self,
        _repository: &RepositoryId,
        _branch: &str,
    ) -> Result<CommitOid, MutationError> {
        Ok(self
            .default_branch_head_after_merge
            .borrow()
            .clone()
            .unwrap_or_else(|| self.branch_head.borrow().clone()))
    }

    fn compare_commits(
        &self,
        _repository: &RepositoryId,
        base: &CommitOid,
        _head: &CommitOid,
    ) -> Result<crate::generation::CommitRelation, MutationError> {
        if self.unreachable_merges.borrow().contains(base) {
            return Ok(crate::generation::CommitRelation::Diverged);
        }
        Ok(crate::generation::CommitRelation::Ahead)
    }

    fn merge_commit_oid(
        &self,
        _repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<Option<CommitOid>, MutationError> {
        Ok(self.merge_commits.borrow().get(&number).cloned())
    }

    fn squash_merge(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        let unpersisted = self
            .unpersisted_merges
            .borrow()
            .get(&expected.number)
            .copied()
            .unwrap_or(0);
        if unpersisted > 0 {
            self.unpersisted_merges
                .borrow_mut()
                .insert(expected.number, unpersisted - 1);
            // The provider accepts the merge and still exposes an open PR.
            return self.mutate(expected, MutationKind::SquashMerge, |_| {});
        }
        self.merge_commits
            .borrow_mut()
            .entry(expected.number)
            .or_insert_with(|| CommitOid(format!("merge-{}", expected.number.0)));
        self.mutate(expected, MutationKind::SquashMerge, |pull_request| {
            pull_request.state = PullRequestState::Merged;
            pull_request.merged_at = Some("now".to_owned());
            pull_request.auto_merge = AutoMergeState::disabled();
        })
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
        let unpersisted = self
            .unpersisted_armings
            .borrow()
            .get(&expected.number)
            .copied()
            .unwrap_or(0);
        if unpersisted > 0 {
            self.unpersisted_armings
                .borrow_mut()
                .insert(expected.number, unpersisted - 1);
            // The provider accepts the request and then exposes no
            // `autoMergeRequest`, exactly as observed on live caravan roots.
            return self.mutate(expected, MutationKind::EnableAutoMerge, |_| {});
        }
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

    fn branch_required_contexts(
        &self,
        _repository: &RepositoryId,
        branch: &str,
    ) -> Result<RequiredContextsRead, MutationError> {
        Ok(self
            .required_contexts
            .borrow()
            .get(branch)
            .cloned()
            .unwrap_or_else(|| RequiredContextsRead::unprotected(branch)))
    }

    fn head_run_lineage(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<HeadRunLineage, MutationError> {
        let current = self
            .pulls
            .borrow()
            .get(&expected.number)
            .cloned()
            .expect("fake PR");
        let actual = PullRequestPrecondition::from(&current);
        if !actual.mutation_identity_eq(expected) {
            return Err(MutationError::StalePrecondition {
                expected: Box::new(expected.clone()),
                actual: Box::new(actual),
                changed_fields: vec!["fake_race".to_owned()],
            });
        }
        let mut queued = self.head_lineage.borrow_mut();
        self.lineage_reads.borrow_mut().push(expected.number);
        let Some(responses) = queued.get_mut(&expected.number) else {
            return Ok(HeadRunLineage {
                head_sha: current.head.oid.0.clone(),
                check_suites: Vec::new(),
                workflow_runs: Vec::new(),
                head_committed_at: Some(PUBLISHED_AT.to_owned()),
                complete: true,
            });
        };
        if responses.len() > 1 {
            Ok(responses.pop_front().expect("queued lineage"))
        } else {
            Ok(responses.front().cloned().expect("queued lineage"))
        }
    }

    fn rerequest_check_suite(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        check_suite_id: u64,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        let current = self
            .pulls
            .borrow()
            .get(&expected.number)
            .cloned()
            .expect("fake PR");
        let Some(head_sha) = self
            .rerequestable_suites
            .borrow()
            .get(&check_suite_id)
            .cloned()
        else {
            return Err(MutationError::MissingProviderResource {
                resource: format!("check-suite/{check_suite_id}"),
            });
        };
        if head_sha != current.head.oid.0 {
            return Err(MutationError::CheckSuiteHeadMismatch {
                check_suite_id,
                expected_head: current.head.oid.0.clone(),
                actual_head: head_sha,
            });
        }
        self.rerequests
            .borrow_mut()
            .push((expected.number, check_suite_id));
        self.mutate(expected, MutationKind::RequestCheckSuite, |_| {})
    }

    fn ensure_control_label_comment(
        &self,
        _repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        audit: &ControlLabelAudit,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.audits.borrow_mut().push(audit.clone());
        let already = self
            .comments
            .borrow()
            .get(&expected.number)
            .is_some_and(|comments| comments.iter().any(|body| body == &audit.marker));
        if !already {
            self.comments
                .borrow_mut()
                .entry(expected.number)
                .or_default()
                .push(audit.marker.clone());
        }
        let mut receipt = self.mutate(expected, MutationKind::Comment, |_| {})?;
        if already {
            receipt.provider_output = Some(format!("existing GitHub comment {}", audit.marker));
        }
        Ok(receipt)
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
        merge_state_status: None,
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
        generation_facts: Vec::new(),
        observed_at: None,
    };
    // The historical fixture set exercises provider-native delegation. The
    // caravan-owned architecture has its own fixtures below.
    let analysis =
        graph::analyze_for_actor(&snapshot, checker, crate::model::HeadMergeActor::Github)
            .expect("analysis");
    StatusOutput {
        config_provenance: None,
        head_merge: crate::read::HeadMergeStatus {
            actor: crate::model::HeadMergeActor::Github,
            ..crate::read::HeadMergeStatus::default()
        },
        runtime: crate::read::RuntimeProvenance::default(),
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
        sync_budget: crate::sync::SyncBudgetStatus::default(),
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
        physical_apply_admission: crate::sync::SyncApplyAdmissionPlan::default(),
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
fn empty_physical_apply_is_valid_before_root_auto_admission() {
    let status = status(Vec::new(), None, &clean);
    let provider = FakeProvider::with_pull_requests(Vec::new());
    let progress = SyncProgress::new(&status, Vec::new(), 64);
    let temporary = tempfile::tempdir().unwrap();
    let initialized = Command::new("git")
        .current_dir(temporary.path())
        .args(["init", "--quiet"])
        .status()
        .unwrap();
    assert!(initialized.success());
    let mut lock = OperationLock::acquire(temporary.path(), "empty_physical_apply").unwrap();

    let outcome = apply_physical_chains(&status, &provider, &[], progress, &mut lock)
        .expect("empty selected caravan set must not panic or mutate");

    assert_eq!(outcome.caravan_id, None);
    assert!(outcome.plans.is_empty());
    assert!(outcome.receipts.is_empty());
    assert!(outcome.provider_receipts.is_empty());
    assert!(provider.calls.borrow().is_empty());
    lock.release().unwrap();
}

/// The exact cacophony shape, which never once worked in production: an empty
/// fleet, several unqueued PRs, and the FIFO-leading one mechanically unable to
/// merge into the default branch.
///
/// Two separate defects made this unreachable, and each hid the other. The
/// conflicting candidate HELD the admission front instead of being skipped, and
/// its conflict was classified as a fleet-blocking `head_conflict`, which
/// aborted the whole tick before anything could be joined. So a single bad PR
/// meant no caravan could ever form, no matter how many clean candidates
/// queued behind it.
///
/// The existing empty-fleet test passes a single clean candidate, so it proved
/// bootstrap worked in a shape the operator never actually had.
#[test]
fn a_conflicting_leading_candidate_still_lets_a_clean_one_form_the_first_caravan() {
    let mut blocked = pull_request(
        10,
        "blocked",
        "main",
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    blocked.labels.clear();
    let mut clean_follower = pull_request(
        20,
        "clean-follower",
        "main",
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    clean_follower.labels.clear();

    // Only the leading candidate conflicts with the default branch.
    let selective = |candidate: &BranchSnapshot,
                     target: &BranchSnapshot|
     -> Result<CompatibilityReport, AppError> {
        let conflicts = candidate.name == "blocked";
        Ok(CompatibilityReport {
            candidate: candidate.clone(),
            target: target.clone(),
            outcome: if conflicts {
                CompatibilityOutcome::Conflict
            } else {
                CompatibilityOutcome::Clean
            },
            conflicting_paths: if conflicts {
                vec!["src/lib.rs".to_owned()]
            } else {
                Vec::new()
            },
            diagnostic: None,
        })
    };

    let fleet = status(
        vec![blocked.clone(), clean_follower.clone()],
        Some(clean_follower.number),
        &selective,
    );

    // The conflict is advisory evidence, never a fleet-blocking problem.
    assert!(
        !fleet
            .analysis
            .fleet
            .problems
            .iter()
            .any(|problem| problem.kind == crate::model::GraphProblemKind::Incompatible),
        "a conflicting UNADMITTED candidate must not abort the tick: {:?}",
        fleet.analysis.fleet.problems
    );

    // The queue advances past it rather than starving every clean candidate.
    assert_eq!(
        fleet.admission.next_candidate,
        Some(clean_follower.number),
        "the clean follower must be elected, not the conflicting leader"
    );

    // And the empty fleet can then actually bootstrap its first caravan.
    let evaluation = evaluate_auto_candidate(&fleet, &clean_follower, &selective)
        .expect("empty-fleet preflight is evidence, not an execution failure");
    assert_eq!(
        evaluation.target,
        AutoCandidateTarget::New,
        "reasons: {:?}",
        evaluation.reasons
    );
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
    let scheduler = successful_scheduler_status(
        &status,
        &progress.ci,
        &progress.paused_caravans,
        true,
        &progress.required_runs,
        &progress.missing_required_runs,
    );
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
    let force_fingerprint = details["decision"]["evidence"]["force_intent"]["failure_fingerprint"]
        .as_str()
        .expect("CI decision exposes reviewed force fingerprint");
    assert!(force_fingerprint.starts_with("fnv1a64:"));
    assert_eq!(
        details["decision"]["evidence"]["force_intent"]["required_checks"]["failure_fingerprint"],
        force_fingerprint
    );
    assert_eq!(
        details["decision"]["evidence"]["force_intent"]["current_decision"]["failure_fingerprint"],
        force_fingerprint
    );
    let attached = attach_scheduler_failure(
        &error,
        &SyncFailureSchedulerStatus {
            schema_version: 1,
            disposition: SchedulerDisposition::ExternalDecision,
            wake_class: SchedulerWakeClass::ExternalDecision,
            retryable: false,
            error_code: "ci_failure".to_owned(),
        },
    );
    assert_eq!(
        mcp_cli::StructuredError::details(&attached).unwrap()["decision_fingerprint"],
        force_fingerprint
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

fn force_rewrite_plan(status: &StatusOutput) -> crate::physical_rebase::RebasePlan {
    let old_head = status.analysis.pull_requests[&PrNumber(1)].head.clone();
    crate::physical_rebase::RebasePlan {
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
        squash_reconciliation: None,
        ci_trigger_workflows: vec!["CI".to_owned()],
        lease: format!("refs/heads/{}:{}", old_head.name, old_head.oid),
        already_satisfied: false,
    }
}

#[test]
fn failed_rewrite_restores_force_only_after_proven_nonpublication() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].labels.insert("caravan-force".to_owned());
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);
    let plan = force_rewrite_plan(&status);
    let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], u32::MAX);
    invalidate_rewritten_force_intents(
        &status,
        &provider,
        std::slice::from_ref(&plan),
        &mut progress,
    )
    .unwrap();
    assert!(!provider.pulls.borrow()[&PrNumber(1)].has_label("caravan-force"));
    let mut outcome = PhysicalRebuildOutcome::default();
    let error = restore_force_intent_after_nonpublication(
        &status,
        &provider,
        &plan,
        &mut progress,
        &mut outcome,
        AppError::validation("rebase_stale_lease", "push refused"),
    );

    assert_eq!(error.code(), "rebase_stale_lease");
    assert!(provider.pulls.borrow()[&PrNumber(1)].has_label("caravan-force"));
    assert_eq!(outcome.force_intent_restorations[0]["state"], "restored");
    assert_eq!(outcome.force_intent_restorations[0]["restored"], true);
    assert_eq!(
        provider.calls.borrow().as_slice(),
        [
            MutationKind::RemoveLabel,
            MutationKind::Comment,
            MutationKind::AddLabel,
            MutationKind::Comment,
        ]
    );
    assert_eq!(
        provider.audits.borrow()[1].operation,
        "force_restore_nonpublication"
    );

    let mut repeated_outcome = PhysicalRebuildOutcome::default();
    let repeated = restore_force_intent_after_nonpublication(
        &status,
        &provider,
        &plan,
        &mut progress,
        &mut repeated_outcome,
        AppError::validation("rebase_stale_lease", "push refused"),
    );
    assert_eq!(repeated.code(), "rebase_stale_lease");
    assert_eq!(
        repeated_outcome.force_intent_restorations[0]["state"],
        "restored"
    );
    assert_eq!(provider.comments.borrow()[&PrNumber(1)].len(), 2);
    assert_eq!(
        provider.comments.borrow()[&PrNumber(1)]
            .iter()
            .filter(|body| body.contains("force_restore_nonpublication"))
            .count(),
        1
    );
}

#[test]
fn published_or_indeterminate_rewrite_never_restores_old_force_intent() {
    for observed_oid in [
        CommitOid("rewritten0000000000000000000000000000000".to_owned()),
        CommitOid("thirdparty000000000000000000000000000000".to_owned()),
    ] {
        let mut pulls = healthy_chain();
        pulls.truncate(1);
        pulls[0].labels.insert("caravan-force".to_owned());
        let provider = FakeProvider::with_pull_requests(pulls.clone());
        let status = status(pulls, Some(PrNumber(1)), &clean);
        let plan = force_rewrite_plan(&status);
        let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], u32::MAX);
        invalidate_rewritten_force_intents(
            &status,
            &provider,
            std::slice::from_ref(&plan),
            &mut progress,
        )
        .unwrap();
        provider
            .pulls
            .borrow_mut()
            .get_mut(&PrNumber(1))
            .unwrap()
            .head
            .oid = observed_oid.clone();
        let mut outcome = PhysicalRebuildOutcome::default();
        let error = restore_force_intent_after_nonpublication(
            &status,
            &provider,
            &plan,
            &mut progress,
            &mut outcome,
            AppError::validation("rebase_stale_lease", "push outcome"),
        );
        assert_eq!(error.code(), "rebase_stale_lease");
        assert!(!provider.pulls.borrow()[&PrNumber(1)].has_label("caravan-force"));
        assert_eq!(
            outcome.force_intent_restorations[0]["state"],
            if observed_oid == plan.new_head_oid {
                "published"
            } else {
                "indeterminate"
            }
        );
        assert_eq!(provider.calls.borrow().len(), 2);
    }
}

#[test]
fn force_restore_comment_failure_retains_partial_label_receipt() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].labels.insert("caravan-force".to_owned());
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);
    let plan = force_rewrite_plan(&status);
    let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], u32::MAX);
    invalidate_rewritten_force_intents(
        &status,
        &provider,
        std::slice::from_ref(&plan),
        &mut progress,
    )
    .unwrap();
    provider.fail_once(MutationKind::Comment);
    let mut outcome = PhysicalRebuildOutcome::default();
    let error = restore_force_intent_after_nonpublication(
        &status,
        &provider,
        &plan,
        &mut progress,
        &mut outcome,
        AppError::validation("rebase_stale_lease", "push refused"),
    );
    assert_eq!(error.code(), "force_intent_restore_failed");
    assert!(provider.pulls.borrow()[&PrNumber(1)].has_label("caravan-force"));
    let details = error.details().unwrap();
    assert_eq!(details["original_error"]["code"], "rebase_stale_lease");
    assert!(details["provider_receipts"].as_array().unwrap().len() >= 3);
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
        squash_reconciliation: None,
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
        squash_reconciliation: None,
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
        squash_reconciliation: None,
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
    assert_eq!(
        progress
            .events
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        vec![EventKind::RootAutoMergeArmed]
    );
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
            EventKind::RootPromoted,
            EventKind::HeadAdvanced,
            EventKind::RootAutoMergeArmed,
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
                evidence_compaction: None,
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
                squash_reconciliation: None,
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
                squash_reconciliation: None,
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
        squash_reconciliation: None,
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

    assert!(
        !progress.operation_receipt().changed,
        "{:#?}",
        progress.steps
    );
    assert!(
        provider.calls.borrow().is_empty(),
        "{:?}",
        provider.calls.borrow()
    );
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
fn check_progress_does_not_stale_sync_auto_merge_mutation() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].auto_merge = AutoMergeState::disabled();
    pulls[0].checks = vec![check(
        "Changed surface admission",
        CheckState::Queued,
        Some(99),
    )];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);
    let current = provider.pulls.borrow()[&PrNumber(1)].clone();
    provider
        .pulls
        .borrow_mut()
        .get_mut(&PrNumber(1))
        .unwrap()
        .checks[0]
        .state = CheckState::InProgress;
    provider
        .pulls
        .borrow_mut()
        .get_mut(&PrNumber(1))
        .unwrap()
        .checks[0]
        .provider_state = Some("IN_PROGRESS".to_owned());

    let progress = execute(&status, &provider, false, false, false)
        .expect("check-only churn must not stale auto-merge repair");

    assert!(progress.operation_receipt().changed);
    assert!(
        provider
            .calls
            .borrow()
            .contains(&MutationKind::EnableAutoMerge)
    );
    let after = provider.pulls.borrow()[&PrNumber(1)].clone();
    assert_eq!(after.head, current.head);
    assert_eq!(after.base, current.base);
    assert_eq!(after.labels, current.labels);
    assert_eq!(after.auto_merge, AutoMergeState::squash());
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
        .insert("caravan-evicted".to_owned());

    let error = execute(&status, &provider, false, false, false).expect_err("race stops");

    assert_eq!(mcp_cli::StructuredError::code(&error), "stale_precondition");
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(details["decision"]["kind"], "stale_precondition");
    assert_eq!(details["decision"]["resumable"], true);
    // Root convergence reads the exact provider generation itself, so the raced
    // control-label transition is named exactly rather than reported as an
    // opaque provider-side precondition rejection.
    assert_eq!(
        details["decision"]["evidence"]["changed_fields"],
        json!(["labels.caravan-evicted"])
    );
    assert!(
        !provider
            .calls
            .borrow()
            .contains(&MutationKind::EnableAutoMerge),
        "a raced membership transition must not be converged blind"
    );
}

#[test]
fn unrelated_label_churn_never_blocks_required_root_convergence() {
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
        .insert("caravan-priority:high".to_owned());

    let progress =
        execute(&status, &provider, false, false, false).expect("routine label churn converges");

    assert!(root_receipt(&progress, 1).provenance.engine_armed);
    assert_eq!(
        provider.pulls.borrow()[&PrNumber(1)].auto_merge,
        AutoMergeState::squash()
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
        retired_state: None,
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

// ---------------------------------------------------------------------------
// Scheduler-owned root squash auto-merge durability (bd-2015d2).
//
// Live caravan2208 repeatedly lost required root arming across scheduler
// rebase and head transitions and stayed degraded until somebody re-armed it by
// hand. These fixtures pin arming as convergent scheduler-owned state proven on
// the exact resulting head with auditable engine provenance.
// ---------------------------------------------------------------------------

fn root_receipt(progress: &SyncProgress, pr: u64) -> &crate::root_auto_merge::RootAutoMergeReceipt {
    progress
        .root_auto_merge
        .iter()
        .find(|receipt| receipt.pr == PrNumber(pr))
        .expect("converged root carries a durable auto-merge receipt")
}

#[test]
fn created_caravan_root_is_armed_with_sealed_engine_provenance() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].auto_merge = AutoMergeState::disabled();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress = execute(&status, &provider, false, false, false).expect("root converges");

    let receipt = root_receipt(&progress, 1);
    assert!(receipt.hash_is_valid());
    assert_eq!(receipt.merge_method, crate::model::MergeMethod::Squash);
    assert_eq!(receipt.head, provider.pulls.borrow()[&PrNumber(1)].head);
    assert!(receipt.observed_after.enabled);
    assert!(receipt.provenance.engine_armed);
    assert_eq!(
        receipt.provenance.owner,
        crate::root_auto_merge::ROOT_AUTO_MERGE_OWNER
    );
    assert_eq!(
        receipt.provenance.trigger,
        crate::root_auto_merge::RootAutoMergeTrigger::RootAdmitted
    );
    assert_eq!(receipt.provenance.operation_id, progress.operation_id);
    assert!(
        progress
            .events
            .iter()
            .any(|event| event.kind == EventKind::RootAutoMergeArmed)
    );
}

#[test]
fn rewritten_root_head_is_rearmed_on_the_resulting_generation() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    // Post-rebase discovery: the provider dropped auto-merge when the scheduler
    // rewrote the root branch.
    pulls[0].auto_merge = AutoMergeState::disabled();
    pulls[0].head.oid = CommitOid("79abc31d4efc07a579145cf904c83c1420f8b4ac".to_owned());
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);
    let rewritten = BTreeMap::from([(
        PrNumber(1),
        CommitOid("79abc31d4efc07a579145cf904c83c1420f8b4ac".to_owned()),
    )]);

    let progress = execute_bounded(
        &status,
        &provider,
        false,
        false,
        false,
        u32::MAX,
        &rewritten,
        RequiredRunsPolicy::default(),
    )
    .expect("rewritten root converges");

    let receipt = root_receipt(&progress, 1);
    assert_eq!(
        receipt.provenance.trigger,
        crate::root_auto_merge::RootAutoMergeTrigger::RootHeadRewritten
    );
    // The proof belongs to the resulting head, never the pre-rebase generation.
    assert_eq!(
        receipt.head.oid,
        CommitOid("79abc31d4efc07a579145cf904c83c1420f8b4ac".to_owned())
    );
    assert_eq!(
        provider.pulls.borrow()[&PrNumber(1)].auto_merge,
        AutoMergeState::squash()
    );
}

#[test]
fn root_arming_converges_before_a_failing_ci_generation_stops_the_tick() {
    let mut pulls = healthy_chain();
    pulls[0].auto_merge = AutoMergeState::disabled();
    pulls[0].checks = vec![check("build-test", CheckState::Success, Some(70))];
    pulls[1].checks = vec![check("build-test", CheckState::Failure, Some(71))];
    let failing = failed_run(71, &pulls[1]);
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider
        .failed_runs
        .borrow_mut()
        .insert(PrNumber(2), vec![failing]);
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let error =
        execute(&status, &provider, false, false, false).expect_err("failing CI stops the tick");

    assert_eq!(mcp_cli::StructuredError::code(&error), "ci_failure");
    // The required root invariant is convergent state, so a CI stop must never
    // leave the admitted root disarmed for an operator to repair.
    assert_eq!(
        provider.pulls.borrow()[&PrNumber(1)].auto_merge,
        AutoMergeState::squash()
    );
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(details["root_auto_merge"][0]["pr"], 1);
    assert_eq!(
        details["root_auto_merge"][0]["provenance"]["owner"],
        crate::root_auto_merge::ROOT_AUTO_MERGE_OWNER
    );
}

#[test]
fn externally_disarmed_root_is_rearmed_with_external_provenance() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    // Discovery saw the armed generation; the provider now exposes it disarmed
    // on the exact same head and base.
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);
    provider
        .pulls
        .borrow_mut()
        .get_mut(&PrNumber(1))
        .expect("fake root")
        .auto_merge = AutoMergeState::disabled();

    let progress = execute(&status, &provider, false, false, false).expect("root reconverges");

    let receipt = root_receipt(&progress, 1);
    assert_eq!(
        receipt.provenance.trigger,
        crate::root_auto_merge::RootAutoMergeTrigger::ExternallyDisarmed
    );
    assert!(receipt.provenance.engine_armed);
    assert_eq!(
        provider.pulls.borrow()[&PrNumber(1)].auto_merge,
        AutoMergeState::squash()
    );
}

#[test]
fn idempotent_replay_proves_arming_without_a_provider_write() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let first = execute(&status, &provider, false, false, false).expect("already armed");
    let second = execute(&status, &provider, false, false, false).expect("replay is a no-op");

    assert!(provider.calls.borrow().is_empty());
    for progress in [&first, &second] {
        let receipt = root_receipt(progress, 1);
        assert!(receipt.hash_is_valid());
        assert!(!receipt.provenance.engine_armed);
        assert_eq!(receipt.arming_attempts, 0);
        assert_eq!(
            receipt.provenance.trigger,
            crate::root_auto_merge::RootAutoMergeTrigger::IdempotentReplay
        );
        assert!(
            progress
                .events
                .iter()
                .all(|event| event.kind != EventKind::RootAutoMergeArmed),
            "an unchanged root emits no arming event"
        );
    }
}

#[test]
fn a_stale_armed_list_view_never_satisfies_the_root_invariant() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    // Discovery still projects the pre-rewrite `autoMergeRequest`.
    let status = status(pulls.clone(), Some(PrNumber(1)), &clean);
    pulls[0].auto_merge = AutoMergeState::disabled();
    let provider = FakeProvider::with_pull_requests(pulls);

    let progress = execute(&status, &provider, false, false, false).expect("fresh read converges");

    assert!(
        provider
            .calls
            .borrow()
            .contains(&MutationKind::EnableAutoMerge),
        "a stale armed projection must not short-circuit required arming"
    );
    assert!(root_receipt(&progress, 1).provenance.engine_armed);
    assert_eq!(
        provider.pulls.borrow()[&PrNumber(1)].auto_merge,
        AutoMergeState::squash()
    );
}

#[test]
fn a_stale_head_read_converges_within_bounded_rereads() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].auto_merge = AutoMergeState::disabled();
    let mut superseded = pulls[0].clone();
    superseded.head.oid = CommitOid("pre-rebase".to_owned());
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.serve_stale_read(PrNumber(1), superseded);
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress =
        execute(&status, &provider, false, false, false).expect("bounded re-reads converge");

    let receipt = root_receipt(&progress, 1);
    assert!(receipt.confirmation_reads >= 2);
    assert_eq!(
        receipt.head.oid,
        CommitOid("one0000000000000000000000000000000000000".to_owned())
    );
}

#[test]
fn a_persistently_stale_head_read_stops_with_a_typed_bounded_cause() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].auto_merge = AutoMergeState::disabled();
    let mut superseded = pulls[0].clone();
    superseded.head.oid = CommitOid("pre-rebase".to_owned());
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    for _ in 0..crate::root_auto_merge::ROOT_AUTO_MERGE_CONFIRMATION_READS {
        provider.serve_stale_read(PrNumber(1), superseded.clone());
    }
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let error = execute(&status, &provider, false, false, false)
        .expect_err("a superseded generation is never promoted or armed");

    // Promotion is the first fenced step, so a persistently stale provider view
    // stops there: nothing downstream can arm or merge an unproven generation.
    assert_eq!(
        mcp_cli::StructuredError::code(&error),
        "root_promotion_incomplete"
    );
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(details["cause"], "stale_provider_view");
    assert_eq!(details["resumable"], true);
    assert_eq!(details["operator_action_required"], false);
    assert_eq!(details["merged"], false);
    assert!(
        !provider
            .calls
            .borrow()
            .contains(&MutationKind::EnableAutoMerge)
    );
    assert!(
        scheduler_failure_status(&error).retryable,
        "bounded sync policy owns the retry, never an operator"
    );
}

#[test]
fn an_unpersisted_arming_stops_with_a_typed_bounded_cause() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].auto_merge = AutoMergeState::disabled();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.never_persist_arming(
        PrNumber(1),
        crate::root_auto_merge::ROOT_AUTO_MERGE_ARMING_ATTEMPTS,
    );
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let error = execute(&status, &provider, false, false, false)
        .expect_err("unproven arming is never reported as converged");

    assert_eq!(
        mcp_cli::StructuredError::code(&error),
        "root_auto_merge_not_durable"
    );
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(details["cause"], "provider_did_not_persist_arming");
    assert_eq!(
        details["arming_attempts"],
        crate::root_auto_merge::ROOT_AUTO_MERGE_ARMING_ATTEMPTS
    );
    assert_eq!(details["operator_action_required"], false);
    assert!(
        details["next"]
            .as_str()
            .expect("next")
            .starts_with("rerun the same idempotent bounded sync tick")
    );
    assert!(scheduler_failure_status(&error).retryable);
}

#[test]
fn root_convergence_never_mutates_compliant_child_members() {
    let mut pulls = healthy_chain();
    pulls[0].auto_merge = AutoMergeState::disabled();
    let before = pulls.clone();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress = execute(&status, &provider, false, false, false).expect("root converges");

    assert_eq!(
        *provider.calls.borrow(),
        vec![MutationKind::EnableAutoMerge],
        "only the admitted root is mutated"
    );
    for child in before.iter().skip(1) {
        assert_eq!(&provider.pulls.borrow()[&child.number], child);
    }
    assert_eq!(progress.root_auto_merge.len(), 1);
    assert_eq!(progress.root_auto_merge[0].pr, PrNumber(1));
}

#[test]
fn required_root_arming_and_candidate_ineligibility_stay_distinct() {
    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].auto_merge = AutoMergeState::disabled();
    // An unadmitted candidate with native auto-merge is structurally
    // ineligible; the admitted root instead *requires* squash auto-merge.
    let mut candidate = pull_request(
        7,
        "seven",
        "main",
        PullRequestState::Open,
        AutoMergeState::squash(),
    );
    candidate.labels.remove("caravan");
    pulls.push(candidate);
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);

    assert!(
        status
            .admission
            .rejected
            .iter()
            .any(|rejected| rejected.pr == PrNumber(7)
                && rejected.reason.contains("externally enabled auto-merge")),
        "native auto-merge keeps an unadmitted candidate structurally ineligible"
    );

    let progress = execute(&status, &provider, false, false, false).expect("root converges");

    assert!(root_receipt(&progress, 1).observed_after.enabled);
    assert_eq!(
        provider.pulls.borrow()[&PrNumber(7)].auto_merge,
        AutoMergeState::squash(),
        "an unadmitted candidate is never mutated by root convergence"
    );
}

/// Exact live caravan2208 generation observed on 2026-07-26 while this defect
/// was reproduced read-only: members `[2208, 2210, 2213, 2215]`, root head
/// `79abc31d…` on `main@b464e1ae…`, `autoMergeRequest` null, caravan label and
/// membership intact. The root had already been re-armed by hand more than once
/// and needed yet another external re-arm at 23:06:11Z, which is precisely the
/// operator-babysitting loop required root convergence must remove.
fn caravan2208_generation() -> Vec<PullRequestSnapshot> {
    let mut root = pull_request(
        2208,
        "root",
        "main",
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    root.head.oid = CommitOid("79abc31d4efc07a579145cf904c83c1420f8b4ac".to_owned());
    root.base.oid = CommitOid("b464e1ae5cb8033a0789997652d97d6b3efd5c7e".to_owned());
    let mut members = vec![root];
    for (number, head, base, head_oid) in [
        (
            2210,
            "member-2210",
            "root",
            "2e02a53116fc3a4afc14542104b026b1bbf750fe",
        ),
        (
            2213,
            "member-2213",
            "member-2210",
            "c9fe0b2baf3a4fcbf779cf5ac20efb1851ed6416",
        ),
        (
            2215,
            "member-2215",
            "member-2213",
            "90ce3e98bef2df4c93dd3be0966f159867bc62ad",
        ),
    ] {
        let mut member = pull_request(
            number,
            head,
            base,
            PullRequestState::Open,
            AutoMergeState::disabled(),
        );
        member.head.oid = CommitOid(head_oid.to_owned());
        members.push(member);
    }
    members
}

#[test]
fn live_caravan2208_root_converges_without_touching_its_children() {
    let pulls = caravan2208_generation();
    let children = pulls[1..].to_vec();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(2208)), &clean);
    let rewritten = BTreeMap::from([(
        PrNumber(2208),
        CommitOid("79abc31d4efc07a579145cf904c83c1420f8b4ac".to_owned()),
    )]);

    assert_eq!(
        status.analysis.fleet.caravans[0].members,
        vec![
            PrNumber(2208),
            PrNumber(2210),
            PrNumber(2213),
            PrNumber(2215)
        ]
    );
    assert!(
        status.analysis.fleet.problems.iter().any(|problem| {
            problem.kind == GraphProblemKind::AutoMergeInvariant
                && problem.message.contains("caravan head #2208")
        }),
        "the live generation reproduces the reported auto_merge_invariant"
    );

    let progress = execute_bounded(
        &status,
        &provider,
        true,
        false,
        true,
        u32::MAX,
        &rewritten,
        RequiredRunsPolicy::default(),
    )
    .expect("scheduler converges required root arming");

    let receipt = root_receipt(&progress, 2208);
    assert!(receipt.hash_is_valid());
    assert_eq!(
        receipt.head.oid,
        CommitOid("79abc31d4efc07a579145cf904c83c1420f8b4ac".to_owned())
    );
    assert_eq!(
        receipt.base.oid,
        CommitOid("b464e1ae5cb8033a0789997652d97d6b3efd5c7e".to_owned())
    );
    assert_eq!(
        receipt.provenance.trigger,
        crate::root_auto_merge::RootAutoMergeTrigger::RootHeadRewritten
    );
    assert!(receipt.provenance.engine_armed);
    assert_eq!(
        provider.pulls.borrow()[&PrNumber(2208)].auto_merge,
        AutoMergeState::squash()
    );
    assert_eq!(
        *provider.calls.borrow(),
        vec![MutationKind::EnableAutoMerge],
        "convergence mutates only the admitted root"
    );
    for child in &children {
        assert_eq!(&provider.pulls.borrow()[&child.number], child);
    }
}

#[test]
fn live_caravan2208_replay_is_a_proven_no_op() {
    let pulls = caravan2208_generation();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(2208)), &clean);
    let rewritten = BTreeMap::from([(
        PrNumber(2208),
        CommitOid("79abc31d4efc07a579145cf904c83c1420f8b4ac".to_owned()),
    )]);

    execute_bounded(
        &status,
        &provider,
        true,
        false,
        true,
        u32::MAX,
        &rewritten,
        RequiredRunsPolicy::default(),
    )
    .expect("first tick arms the root");
    let armed_pulls = provider
        .pulls
        .borrow()
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let armed = self::status(armed_pulls, Some(PrNumber(2208)), &clean);
    provider.calls.borrow_mut().clear();

    let progress = execute_bounded(
        &armed,
        &provider,
        true,
        false,
        true,
        u32::MAX,
        &rewritten,
        RequiredRunsPolicy::default(),
    )
    .expect("replay converges without another write");

    assert!(provider.calls.borrow().is_empty());
    assert!(armed.analysis.fleet.problems.is_empty());
    let receipt = root_receipt(&progress, 2208);
    assert!(!receipt.provenance.engine_armed);
    assert_eq!(
        receipt.provenance.trigger,
        crate::root_auto_merge::RootAutoMergeTrigger::IdempotentReplay
    );
}

// ---------------------------------------------------------------------------
// bd-e99fe6: bounded forward progress when the apply reserve exceeds one tick.
// ---------------------------------------------------------------------------

/// Exact live Cacophony configuration that produced the monotonic deadlock:
/// `sync.max_duration_secs: 3600`, `command_timeout_secs: 120`.
fn cacophony_context() -> AppContext {
    let mut context = AppContext::default();
    context.config.rebase_on_join = true;
    context.config.command_timeout_secs = 120;
    context.config.sync.max_duration_secs = 3_600;
    // These fixtures deliberately assert the strict worst-case cliff model, so
    // they pin the reserve to the full command timeout. Production defaults to
    // a proportional reserve (bd-5528e6).
    context.config.sync.reserve_secs_per_command = 120;
    context
}

/// The sound admission bound for a fixture context, which must exist.
fn capacity_of(context: &AppContext, deadline: Duration) -> u64 {
    admission_capacity(context, deadline).expect("a sound admission bound")
}

/// A linear caravan of `members` open PRs with only the root armed.
fn linear_chain(members: u64) -> Vec<PullRequestSnapshot> {
    (1..=members)
        .map(|number| {
            let base = if number == 1 {
                "main".to_owned()
            } else {
                format!("branch-{}", number - 1)
            };
            let head = format!("branch-{number}");
            pull_request(
                number,
                &head,
                &base,
                PullRequestState::Open,
                if number == 1 {
                    AutoMergeState::squash()
                } else {
                    AutoMergeState::disabled()
                },
            )
        })
        .collect()
}

fn chain_costs(status: &StatusOutput) -> Vec<ChainCost> {
    chain_costs_from_status(status, &status.analysis.fleet.caravans)
}

/// Mark a leading prefix as already applied by a previous tick.
fn with_retained_prefix(mut chains: Vec<ChainCost>, retained: usize) -> Vec<ChainCost> {
    for chain in &mut chains {
        for member in chain.members.iter_mut().take(retained) {
            member.pending = false;
        }
    }
    chains
}

#[test]
fn apply_reserve_models_actual_operations_instead_of_a_whole_chain_worst_case() {
    let mut context = AppContext::default();
    context.config.command_timeout_secs = 10;
    let pulls = linear_chain(7);
    let status = status(pulls, Some(PrNumber(1)), &clean);
    let chains = chain_costs(&status);

    // One armed root, seven pending rewrites, seven CI observations, one root
    // base retarget plus arming, and the two mandatory rediscoveries.
    let pending = complete_budget(&context, &chains);
    assert_eq!(pending.command_slots, 1 + 7 * 3 + 7 + 2 + 2);

    // The identical graph after a completed prefix is strictly cheaper: no
    // push, no auto-merge drop, no force invalidation for retained members.
    let retained = complete_budget(&context, &with_retained_prefix(chains.clone(), 7));
    assert_eq!(retained.command_slots, 7 * 2 + 7 + 2 + 2);
    assert!(retained.required < pending.required);
    assert!(retained.mutation_reserve < pending.mutation_reserve);

    // The bounded-prefix scope drops only the deferrable reconciliation.
    let prefix = budget_for(
        &context,
        &chains,
        &[chains[0].members.len()],
        ReserveScope::BoundedPrefix,
    );
    assert_eq!(prefix.command_slots, 1 + 7 * 3 + 2);
}

#[test]
fn every_chain_size_around_the_threshold_still_drains_at_least_one_member() {
    let context = cacophony_context();
    let deadline = sync_operation_budget(&context);
    let capacity = capacity_of(&context, deadline);
    assert!(capacity >= 7, "live caravan2208 size must stay admissible");

    for members in 1..=capacity {
        let pulls = linear_chain(members);
        let status = status(pulls, Some(PrNumber(1)), &clean);
        let chains = chain_costs(&status);
        let admission = admit_physical_prefix(&context, &chains, deadline);
        assert!(
            admission.makes_progress(),
            "a {members}-member chain must apply at least one pending member per tick"
        );
        assert!(admission.pending_admitted >= 1);
        assert!(admission.budget.required < deadline);

        // Whatever is admitted is an exact root-to-descendant prefix.
        let expected = (1..=u64::try_from(admission.admitted_prs.len()).unwrap())
            .map(PrNumber)
            .collect::<Vec<_>>();
        assert_eq!(admission.admitted_prs, expected);

        // The hardest resumable shape: only the last member still pending.
        let resumed = with_retained_prefix(chains, usize::try_from(members - 1).unwrap());
        let resumed_admission = admit_physical_prefix(&context, &resumed, deadline);
        assert!(
            resumed_admission.makes_progress(),
            "a resumed {members}-member chain must still drain its trailing member"
        );
        assert_eq!(resumed_admission.pending_admitted, 1);
    }
}

#[test]
fn seven_member_caravan2208_drains_in_bounded_ticks_without_reordering() {
    let context = cacophony_context();
    let deadline = sync_operation_budget(&context);
    let pulls = linear_chain(7);
    let status = status(pulls, Some(PrNumber(1)), &clean);
    let members = status.analysis.fleet.caravans[0].members.clone();
    let chains = chain_costs(&status);

    // The whole-graph reserve still does not fit: that refusal was correct.
    let complete = complete_budget(&context, &chains);
    assert!(complete.required > deadline);

    // The first tick applies an exact verified prefix and defers convergence.
    let first = admit_physical_prefix(&context, &chains, deadline);
    assert!(first.makes_progress());
    assert!(first.deferred_convergence);
    assert!(first.pending_admitted >= 1);

    // Ticks are replayed until nothing pending remains. Every tick keeps the
    // caravan's exact member order; nothing is reordered, evicted, or split.
    let mut costs = chains;
    let mut ticks = 0;
    while costs
        .iter()
        .any(|chain| chain.members.iter().any(|member| member.pending))
    {
        ticks += 1;
        assert!(ticks <= 7, "bounded prefixes must converge, not livelock");
        let admission = admit_physical_prefix(&context, &costs, deadline);
        assert!(admission.makes_progress());
        for (chain, admitted) in costs.iter_mut().zip(admission.admitted.iter().copied()) {
            for member in chain.members.iter_mut().take(admitted) {
                member.pending = false;
            }
        }
        assert_eq!(
            costs[0]
                .members
                .iter()
                .map(|member| member.pr)
                .collect::<Vec<_>>(),
            members,
        );
    }

    // The converged graph now fits one complete tick, so ordinary convergence
    // runs instead of deferring forever.
    let final_admission = admit_physical_prefix(&context, &costs, deadline);
    assert!(final_admission.deferred.is_empty());
    assert!(!final_admission.deferred_convergence);
    assert_eq!(final_admission.pending_admitted, 0);
}

#[test]
fn an_interrupted_prefix_resumes_without_replaying_completed_mutations() {
    let context = cacophony_context();
    let deadline = sync_operation_budget(&context);
    let status = status(linear_chain(7), Some(PrNumber(1)), &clean);
    let resumed = with_retained_prefix(chain_costs(&status), 4);

    let admission = admit_physical_prefix(&context, &resumed, deadline);

    // The completed prefix is admitted again (its exact ancestry is revalidated)
    // but contributes no pending write and no control mutation, so no provider
    // mutation is ever replayed.
    assert!(admission.admitted_prs.starts_with(&[
        PrNumber(1),
        PrNumber(2),
        PrNumber(3),
        PrNumber(4)
    ]));
    assert_eq!(admission.pending_admitted, 3);
    // Resuming after a completed prefix is strictly cheaper, so the graph that
    // could not fit one tick now finishes inside one.
    let pending_prefix = budget_for(
        &context,
        &chain_costs(&status),
        &[7],
        ReserveScope::BoundedPrefix,
    );
    let resumed_prefix = budget_for(&context, &resumed, &[7], ReserveScope::BoundedPrefix);
    assert!(resumed_prefix.required < pending_prefix.required);
    assert!(resumed_prefix.mutation_reserve < pending_prefix.mutation_reserve);
    assert!(admission.deferred.is_empty());
    assert!(!admission.deferred_convergence);
    assert!(complete_budget(&context, &chain_costs(&status)).required > deadline);
    assert!(complete_budget(&context, &resumed).required < deadline);
}

#[test]
fn independent_caravans_grow_round_robin_under_bounded_parallelism() {
    let mut context = cacophony_context();
    // A deadline that cannot hold both complete chains forces a bounded prefix.
    context.config.sync.max_duration_secs = 1_200;
    let deadline = sync_operation_budget(&context);
    let mut pulls = linear_chain(4);
    pulls.extend([
        pull_request(
            11,
            "other-1",
            "main",
            PullRequestState::Open,
            AutoMergeState::squash(),
        ),
        pull_request(
            12,
            "other-2",
            "other-1",
            PullRequestState::Open,
            AutoMergeState::disabled(),
        ),
    ]);
    let status = status(pulls, None, &clean);
    let chains = chain_costs(&status);
    assert_eq!(chains.len(), 2);

    let admission = admit_physical_prefix(&context, &chains, deadline);

    assert!(admission.makes_progress());
    // Both independent chains advance in the same bounded parallel round.
    assert!(admission.admitted[0] >= 1);
    assert!(admission.admitted[1] >= 1);
    assert!(admission.admitted[0].abs_diff(admission.admitted[1]) <= 1);
    // Bounded parallelism means two independent chains cost one shared round,
    // not the sum of both chains.
    let serial = u64::try_from(admission.admitted[0] + admission.admitted[1]).unwrap() * 3;
    assert!(admission.budget.command_slots < serial + 2);
}

/// bd-5528e6 live case: caravan 2210 had six members under `max_duration_secs`
/// 3600 and `command_timeout_secs` 120. The old worst-case reserve wanted 32 slots x 120s
/// = 3,840,000ms against 3,600,000ms available, so sync refused before any
/// mutation and even a cheap base-retarget plus arm could never run.
#[test]
fn a_six_member_caravan_makes_progress_under_the_proportional_reserve() {
    let mut context = cacophony_context();
    // Production default rather than the worst-case pin used by these fixtures.
    context.config.sync.reserve_secs_per_command =
        crate::config::SyncConfig::default().reserve_secs_per_command;
    let deadline = sync_operation_budget(&context);
    let status = status(linear_chain(6), Some(PrNumber(1)), &clean);
    let chains = chain_costs(&status);

    let admission = admit_physical_prefix(&context, &chains, deadline);

    assert!(
        admission.makes_progress(),
        "a six-member caravan must be able to converge within an hour"
    );
    // The whole graph fits, so nothing is deferred purely by the reserve model.
    assert!(admission.deferred.is_empty());
    assert!(capacity_of(&context, deadline) >= 6);
}

#[test]
fn a_chain_larger_than_any_deadline_fails_closed_with_capacity_evidence() {
    let mut context = cacophony_context();
    // One command slot for the whole tick cannot cover any apply round.
    context.config.sync.max_duration_secs = 120;
    let deadline = sync_operation_budget(&context);
    let status = status(linear_chain(3), Some(PrNumber(1)), &clean);
    let chains = chain_costs(&status);

    let admission = admit_physical_prefix(&context, &chains, deadline);
    assert!(!admission.makes_progress());
    assert_eq!(admission.pending_admitted, 0);
    // bd-b1c7b7: a bound this configuration cannot make sound is a defect, not
    // a zero-member capacity that ordinary gating would pretend to enforce.
    let defect = admission_capacity(&context, deadline).expect_err("an unsound bound is a defect");
    assert_eq!(defect.code, "sync_budget_capacity_unsound");
    assert_eq!(defect.computed_bound, 0);

    let error = physical_capacity_failure(
        &context,
        &status,
        Instant::now() + deadline,
        &admission,
        Vec::new(),
        "physical_rebase_commit_admission",
    );
    assert_eq!(error.code(), "physical_sync_budget_insufficient");
    let details = error.details().expect("capacity evidence");
    assert!(
        details["max_admissible_members"].is_null(),
        "a zero bound is never emitted"
    );
    assert_eq!(details["capacity_defect"]["computed_bound"], 0);
    assert_eq!(details["capacity_defect"]["minimum_sound_bound"], 2);
    assert_eq!(details["configured_deadline_ms"], 120_000);
    assert_eq!(details["provider_mutations"], 0);
    assert_eq!(details["branch_mutations"], 0);
    assert_eq!(details["processable_prefix"].as_array().unwrap().len(), 0);
    assert_eq!(details["deferred_members"].as_array().unwrap().len(), 3);
}

#[test]
fn admission_at_capacity_is_refused_with_typed_capacity_evidence() {
    let context = cacophony_context();
    let deadline = sync_operation_budget(&context);
    let capacity = capacity_of(&context, deadline);
    let below = status(linear_chain(capacity - 1), None, &clean);
    let at = status(linear_chain(capacity), None, &clean);
    let tail = PrNumber(capacity);

    assert!(
        caravan_capacity_refusal(
            &context,
            &below,
            PrNumber(999),
            Some(PrNumber(capacity - 1))
        )
        .is_none(),
        "a chain below capacity keeps accepting members"
    );
    // A brand-new independent caravan is never refused by chain capacity.
    assert!(caravan_capacity_refusal(&context, &at, PrNumber(999), None).is_none());

    let refusal = caravan_capacity_refusal(&context, &at, PrNumber(999), Some(tail))
        .expect("a chain at capacity must refuse further joins");
    assert_eq!(refusal.code, "caravan_budget_capacity_exhausted");
    assert_eq!(refusal.caravan_members, capacity);
    assert_eq!(refusal.max_admissible_members, Some(capacity));
    assert!(refusal.capacity_defect.is_none());
    assert_eq!(refusal.configured_deadline_ms, 3_600_000);
    assert!(refusal.safe_next_action.contains("drain"));

    let error = caravan_capacity_error(&refusal);
    assert_eq!(error.code(), "caravan_budget_capacity_exhausted");
    let details = error.details().expect("typed refusal evidence");
    assert_eq!(details["mutated"], false);
    assert_eq!(details["retryable"], false);
    assert_eq!(details["max_admissible_members"], capacity);

    // The already-admitted chain still drains while admission stays closed.
    let admission = admit_physical_prefix(&context, &chain_costs(&at), deadline);
    assert!(admission.makes_progress());
}

#[test]
fn capacity_is_disabled_without_physical_chain_rebuilding() {
    let mut context = cacophony_context();
    context.config.rebase_on_join = false;
    let at = status(linear_chain(64), None, &clean);
    assert!(caravan_capacity_refusal(&context, &at, PrNumber(999), Some(PrNumber(64))).is_none());
}

#[test]
fn capacity_scales_with_the_configured_deadline_and_child_timeout() {
    let mut context = cacophony_context();
    let small = capacity_of(&context, sync_operation_budget(&context));
    context.config.command_timeout_secs = 60;
    let cheaper_children = capacity_of(&context, sync_operation_budget(&context));
    assert!(cheaper_children > small);

    context.config.command_timeout_secs = 120;
    context.config.sync.max_duration_secs = 1_800;
    let shorter_deadline = capacity_of(&context, sync_operation_budget(&context));
    assert!(shorter_deadline < small);
}

#[test]
fn status_exposes_the_reserve_prefix_and_next_action_before_the_cliff() {
    let context = cacophony_context();
    let status = status(linear_chain(7), Some(PrNumber(1)), &clean);

    let projection = crate::sync::project_status(&context, &status);

    assert_eq!(projection.schema_version, 2);
    assert!(projection.rebase_on_join);
    assert_eq!(projection.deadline_ms, 3_600_000);
    assert_eq!(projection.command_timeout_ms, 120_000);
    assert_eq!(projection.reserve_ms_per_command, 120_000);
    assert_eq!(projection.deadline_command_slots, 30);
    assert!(projection.capacity_defect.is_none());
    assert!(projection.max_admissible_members.unwrap() >= 7);
    let caravan = &projection.caravans[0];
    assert_eq!(caravan.caravan_id, PrNumber(1));
    assert_eq!(caravan.members.len(), 7);
    assert!(caravan.required_ms > 0);
    assert!(caravan.retained_ms < caravan.required_ms);
    assert!(!caravan.processable_prefix.is_empty());
    assert_eq!(caravan.processable_prefix[0], PrNumber(1));
    assert!(!caravan.at_capacity);
    assert!(caravan.deferred_convergence);
    assert!(caravan.safe_next_action.contains("cara sync --all"));
    assert!(
        projection
            .safe_next_action
            .contains("resume on the next tick")
    );
    assert!(projection.blocked_candidate.is_none());
}

#[test]
fn status_names_the_blocked_candidate_once_every_caravan_is_at_capacity() {
    let context = cacophony_context();
    let capacity = capacity_of(&context, sync_operation_budget(&context));
    let mut pulls = linear_chain(capacity);
    let mut candidate = pull_request(
        900,
        "candidate",
        "main",
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    candidate.labels.clear();
    pulls.push(candidate);
    let status = status(pulls, None, &clean);

    let projection = crate::sync::project_status(&context, &status);

    assert!(
        projection
            .caravans
            .iter()
            .all(|caravan| caravan.at_capacity)
    );
    assert_eq!(projection.blocked_candidate, Some(PrNumber(900)));
    assert!(
        projection
            .safe_next_action
            .contains("caravan_budget_capacity_exhausted")
    );
    assert!(projection.caravans[0].safe_next_action.contains("drain"));
}

// ---------------------------------------------------------------------------
// bd-b1c7b7: admission prices slots with the actual-work reserve, and an
// unsound bound is a defect instead of an impossible zero-member capacity.
// ---------------------------------------------------------------------------

/// Exact live configuration that produced the impossible bound: the reserve
/// already priced work proportionally (`sync.reserve_secs_per_command: 15`)
/// while admission still priced every slot at `command_timeout_secs: 600`.
fn live_admission_context() -> AppContext {
    let mut context = AppContext::default();
    context.config.rebase_on_join = true;
    context.config.command_timeout_secs = 600;
    context.config.sync.max_duration_secs = 3_600;
    assert_eq!(context.config.sync.reserve_secs_per_command, 15);
    context
}

/// A candidate PR outside every caravan, so status names a blocked candidate.
fn unlabelled_candidate() -> PullRequestSnapshot {
    let mut candidate = pull_request(
        900,
        "candidate",
        "main",
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    candidate.labels.clear();
    candidate
}

/// Live caravan 2215: four members, 21 command slots, 315000ms required. The
/// reserve called that under nine percent of an hour while admission, pricing
/// the identical slots at the full command timeout, called it over capacity.
#[test]
fn admission_capacity_uses_the_same_actual_work_reserve_as_the_apply_budget() {
    let context = live_admission_context();
    let status = status(linear_chain(4), Some(PrNumber(1)), &clean);

    let projection = crate::sync::project_status(&context, &status);

    assert_eq!(projection.reserve_ms_per_command, 15_000);
    assert_eq!(projection.command_timeout_ms, 600_000);
    // Deadline slots are priced by the reserve model, not by the worst case:
    // 3600s / 15s = 240, never 3600s / 600s = 6.
    assert_eq!(projection.deadline_command_slots, 240);
    let caravan = &projection.caravans[0];
    assert_eq!(caravan.required_command_slots, 21);
    assert_eq!(caravan.required_ms, 315_000);
    assert_eq!(
        caravan.required_ms,
        caravan.required_command_slots * projection.reserve_ms_per_command,
        "one model prices both the reserve and the admission bound"
    );
    let bound = projection
        .max_admissible_members
        .expect("a sound admission bound");
    assert!(
        bound > 4,
        "a chain costing under nine percent of the deadline is not at capacity"
    );
    assert!(!caravan.at_capacity);
    assert!(projection.capacity_defect.is_none());
    assert!(projection.blocked_candidate.is_none());
    assert!(!caravan.safe_next_action.contains("drain"));
}

/// Live caravan 2233: one member, nine command slots, 135000ms required, and
/// reported at capacity against a zero-member bound. A caravan holding a
/// single member can never be at capacity under a sound bound.
#[test]
fn a_single_member_caravan_is_never_reported_at_capacity() {
    for command_timeout_secs in [30, 120, 600] {
        for max_duration_secs in [120, 600, 3_600] {
            let mut context = live_admission_context();
            context.config.command_timeout_secs = command_timeout_secs;
            context.config.sync.max_duration_secs = max_duration_secs;
            let status = status(linear_chain(1), None, &clean);

            let projection = crate::sync::project_status(&context, &status);
            let caravan = &projection.caravans[0];

            assert!(
                !caravan.at_capacity,
                "a one-member caravan is at capacity only under a self-contradictory bound ({command_timeout_secs}s/{max_duration_secs}s)"
            );
            assert!(
                projection.max_admissible_members != Some(0),
                "a zero-member bound is never emitted"
            );
            assert_eq!(
                projection.max_admissible_members.is_none(),
                projection.capacity_defect.is_some(),
                "an absent bound is always explained by a typed defect"
            );
        }
    }

    let context = live_admission_context();
    let projection = crate::sync::project_status(&context, &status(linear_chain(1), None, &clean));
    let caravan = &projection.caravans[0];
    assert_eq!(caravan.required_command_slots, 9);
    assert_eq!(caravan.required_ms, 135_000);
}

/// The stale-checkout control arm: `command_timeout_secs` 120 and 600 must
/// imply the same admission bound, because a proven-safe per-command timeout
/// is a ceiling on one command, not the price of every planned slot.
#[test]
fn raising_a_proven_safe_command_timeout_no_longer_closes_admission() {
    let mut context = live_admission_context();
    context.config.command_timeout_secs = 120;
    let stale_checkout = capacity_of(&context, sync_operation_budget(&context));

    context.config.command_timeout_secs = 600;
    let current_checkout = capacity_of(&context, sync_operation_budget(&context));

    assert_eq!(
        stale_checkout, current_checkout,
        "raising a proven-safe command timeout must not silently close admission"
    );
    assert!(current_checkout > 4);
}

/// The bound and the reserve must never disagree about the same chain: the
/// hardest ordinary shape at the bound still fits the configured deadline.
#[test]
fn the_admission_bound_and_the_apply_reserve_agree_on_the_same_chain() {
    let context = live_admission_context();
    let deadline = sync_operation_budget(&context);
    let bound = capacity_of(&context, deadline);
    let retained = usize::try_from(bound - 1).expect("bounded fixture");
    let status = status(linear_chain(bound), Some(PrNumber(1)), &clean);
    let chains = with_retained_prefix(chain_costs(&status), retained);

    let budget = budget_for(
        &context,
        &chains,
        &[retained + 1],
        ReserveScope::BoundedPrefix,
    );

    assert!(
        budget.required < deadline,
        "the bound must never admit a chain the reserve would refuse"
    );
    assert!(admit_physical_prefix(&context, &chains, deadline).makes_progress());
}

/// A non-positive bound is a configuration defect: it is reported as one, no
/// caravan is gated by it, and the guidance names the configuration change
/// that repairs it instead of a drain that provably cannot.
#[test]
fn an_unsound_bound_is_reported_as_a_defect_instead_of_zero_capacity() {
    let mut context = live_admission_context();
    // Four reserve slots for the whole tick cannot cover a two-member chain.
    context.config.sync.max_duration_secs = 60;
    let mut pulls = linear_chain(1);
    pulls.push(unlabelled_candidate());
    let status = status(pulls, None, &clean);

    let projection = crate::sync::project_status(&context, &status);

    assert_eq!(projection.max_admissible_members, None);
    let defect = projection.capacity_defect.expect("a typed capacity defect");
    assert_eq!(defect.code, "sync_budget_capacity_unsound");
    assert_eq!(defect.computed_bound, 0);
    assert_eq!(defect.minimum_sound_bound, 2);
    assert_eq!(defect.reserve_ms_per_command, 15_000);
    assert!(
        defect.minimum_deadline_ms > defect.deadline_ms,
        "the defect names the deadline that repairs it"
    );
    assert!(
        defect
            .safe_next_action
            .contains("raise sync.max_duration_secs")
    );

    // Gating never fires under a defect, so no caravan is quietly closed and
    // no candidate is reported as blocked behind an impossible bound.
    assert!(
        projection
            .caravans
            .iter()
            .all(|caravan| !caravan.at_capacity)
    );
    assert_eq!(projection.blocked_candidate, None);
    assert!(
        !projection
            .safe_next_action
            .contains("until a caravan drains"),
        "guidance must not recommend an action that cannot resolve the condition"
    );
    assert!(
        projection
            .safe_next_action
            .contains("draining cannot repair")
    );
    assert!(
        projection.caravans[0]
            .safe_next_action
            .contains("raise sync.max_duration_secs")
    );
}

/// The complete filed fleet: a four-member caravan, a one-member caravan, and
/// a canonical candidate that the impossible bound refused. Admission must be
/// open for all of them under one shared, actual-work budget model.
#[test]
fn the_filed_fleet_reopens_admission_under_one_shared_budget_model() {
    let context = live_admission_context();
    let mut pulls = linear_chain(4);
    pulls.push(pull_request(
        11,
        "other-1",
        "main",
        PullRequestState::Open,
        AutoMergeState::squash(),
    ));
    pulls.push(unlabelled_candidate());
    let status = status(pulls, None, &clean);

    let projection = crate::sync::project_status(&context, &status);

    assert_eq!(projection.caravans.len(), 2);
    let four = &projection.caravans[0];
    let one = &projection.caravans[1];
    assert_eq!(four.members.len(), 4);
    assert_eq!(four.required_command_slots, 21);
    assert_eq!(four.required_ms, 315_000);
    assert_eq!(one.members.len(), 1);
    assert_eq!(one.required_command_slots, 9);
    assert_eq!(one.required_ms, 135_000);
    assert!(!four.at_capacity);
    assert!(!one.at_capacity);
    assert_eq!(projection.blocked_candidate, None);
    assert!(projection.capacity_defect.is_none());
    assert!(
        !projection
            .safe_next_action
            .contains("caravan_budget_capacity_exhausted")
    );
    assert!(
        caravan_capacity_refusal(&context, &status, PrNumber(900), Some(PrNumber(4))).is_none(),
        "the candidate that was blocked by the impossible bound can join again"
    );
}

#[test]
fn gating_a_caravan_below_the_sound_floor_is_reported_as_a_contradiction() {
    let context = live_admission_context();
    let deadline = sync_operation_budget(&context);

    // The exact filed shape: a one-member caravan measured against a bound it
    // could never satisfy. Ordinary gating must never claim this.
    let gate = gate_for_bound(&context, deadline, 1, 0);

    let CapacityGate::Defect(defect) = gate else {
        panic!("a one-member caravan at capacity is a contradiction, not gating");
    };
    assert_eq!(defect.code, "sync_budget_capacity_contradiction");
    assert!(defect.safe_next_action.contains("no drain can ever clear"));
    assert!(
        defect
            .safe_next_action
            .contains("raise sync.max_duration_secs")
    );

    // A sound bound still gates a chain that genuinely reached it.
    assert!(matches!(
        gate_for_bound(&context, deadline, 4, 4),
        CapacityGate::AtCapacity { bound: 4 }
    ));
    assert!(matches!(
        gate_for_bound(&context, deadline, 3, 4),
        CapacityGate::Open { bound: 4 }
    ));
}

/// Under a defect, a join fails loudly with distinct typed evidence instead of
/// being quietly refused as ordinary at-capacity gating.
#[test]
fn an_unsound_bound_refuses_joins_loudly_instead_of_gating_them() {
    let mut context = live_admission_context();
    context.config.sync.max_duration_secs = 60;
    let at = status(linear_chain(1), None, &clean);

    let refusal = caravan_capacity_refusal(&context, &at, PrNumber(999), Some(PrNumber(1)))
        .expect("an unsound bound must refuse the join as a defect");

    assert_eq!(refusal.code, "caravan_budget_capacity_defect");
    assert_eq!(refusal.caravan_members, 1);
    assert!(
        refusal.max_admissible_members.is_none(),
        "a zero bound is never emitted"
    );
    let defect = refusal
        .capacity_defect
        .as_ref()
        .expect("typed defect evidence");
    assert_eq!(defect.minimum_sound_bound, 2);
    assert!(defect.minimum_deadline_ms > refusal.configured_deadline_ms);
    assert!(
        !refusal.safe_next_action.contains("drain below"),
        "draining cannot clear a bound derived from configuration alone"
    );
    assert!(
        refusal
            .safe_next_action
            .contains("raise sync.max_duration_secs")
    );

    let error = caravan_capacity_error(&refusal);
    assert_eq!(error.code(), "caravan_budget_capacity_defect");
    let details = error.details().expect("typed refusal evidence");
    assert_eq!(details["mutated"], false);
    assert_eq!(details["retryable"], false);
    assert!(details["max_admissible_members"].is_null());
    assert_eq!(details["capacity_defect"]["computed_bound"], 0);
    let suggested = details["suggested_actions"].as_array().expect("actions");
    assert!(
        suggested.iter().all(|action| !action
            .as_str()
            .unwrap_or_default()
            .contains("until the existing bounded prefix drains members out of the caravan")),
        "no suggested action may be one that cannot resolve the defect"
    );
}

#[test]
fn deferred_members_keep_their_exact_generation_force_intent() {
    // Force invalidation is a control mutation bound to a rewrite. A member
    // deferred by the bounded prefix is not rewritten, so its exact-generation
    // intent must not be invalidated and must not be charged to the reserve.
    let mut context = cacophony_context();
    context.config.command_timeout_secs = 10;
    let mut pulls = linear_chain(3);
    pulls[2].labels.insert("caravan-force".to_owned());
    let status = status(pulls, Some(PrNumber(1)), &clean);
    let chains = chain_costs(&status);

    let prefix_without_forced_member =
        budget_for(&context, &chains, &[2], ReserveScope::BoundedPrefix);
    let prefix_with_forced_member =
        budget_for(&context, &chains, &[3], ReserveScope::BoundedPrefix);

    assert_eq!(
        prefix_with_forced_member.command_slots - prefix_without_forced_member.command_slots,
        3 + 6,
        "only an admitted forced member charges its invalidation and compensation"
    );
}

#[test]
fn provider_drift_in_control_state_changes_the_modelled_reserve() {
    let mut context = cacophony_context();
    context.config.command_timeout_secs = 10;
    let quiet = status(linear_chain(3), Some(PrNumber(1)), &clean);
    let mut drifted_pulls = linear_chain(3);
    // An external actor armed a non-root member between ticks.
    drifted_pulls[1].auto_merge = AutoMergeState::squash();
    let drifted = status(drifted_pulls, Some(PrNumber(1)), &clean);

    let quiet_budget = complete_budget(&context, &chain_costs(&quiet));
    let drifted_budget = complete_budget(&context, &chain_costs(&drifted));

    // One extra pre-rewrite auto-merge drop plus one reconciliation repair.
    assert_eq!(
        drifted_budget.command_slots - quiet_budget.command_slots,
        2,
        "exact observed provider state, not a fixed worst case, drives the reserve"
    );
    assert!(drifted_budget.mutation_reserve > quiet_budget.mutation_reserve);
}

#[test]
fn a_deferred_prefix_tick_succeeds_as_a_retryable_bounded_progress_receipt() {
    let mut context = cacophony_context();
    context.config.sync.actions.join_unlabelled_prs = true;
    let pulls = linear_chain(7);
    let status = status(pulls, Some(PrNumber(1)), &clean);
    let chains = chain_costs(&status);
    let deadline = sync_operation_budget(&context);
    let admission = admit_physical_prefix(&context, &chains, deadline);
    assert!(admission.deferred_convergence);

    let receipt = crate::physical_rebase::RebaseReceipt {
        pr: PrNumber(1),
        branch: "branch-1".to_owned(),
        old_head_oid: branch("branch-1").oid,
        new_head_oid: CommitOid("rewritten0000000000000000000000000000000".to_owned()),
        old_base_oid: branch("main").oid.clone(),
        new_base_branch: "main".to_owned(),
        new_base_oid: branch("main").oid,
        new_tree_oid: CommitOid("tree000000000000000000000000000000000000".to_owned()),
        commit_count: 1,
        merge_topology: None,
        squash_reconciliation: None,
        ci_trigger_workflows: vec!["CI".to_owned()],
        lease: "--force-with-lease=refs/heads/branch-1:branch-1".to_owned(),
        already_satisfied: false,
    };
    let physical_rebuild = PhysicalRebuildOutcome {
        repository: Some(repository()),
        caravan_id: Some(PrNumber(1)),
        affected_prs: vec![PrNumber(1)],
        deferred: admission.deferred.clone(),
        receipts: vec![receipt.clone()],
        steps: vec![MutationStep {
            kind: MutationKind::RebaseBranch,
            state: MutationStepState::Completed,
            pr: Some(PrNumber(1)),
            summary: "rebased under exact lease".to_owned(),
        }],
        ..PhysicalRebuildOutcome::default()
    };

    let temporary = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .current_dir(temporary.path())
            .args(["init", "--quiet"])
            .status()
            .unwrap()
            .success()
    );
    let mut lock = OperationLock::acquire(temporary.path(), "bounded_prefix").unwrap();
    let started = Instant::now();

    let output = bounded_prefix_output(
        &context,
        &SyncInput {
            all: true,
            rerun_failed: false,
        },
        started,
        started + deadline,
        Duration::from_millis(5),
        Duration::from_millis(7),
        &admission,
        physical_rebuild,
        status,
        None,
        &mut lock,
    )
    .expect("a bounded prefix tick is forward progress, not a failure");

    // Bounded progress is a success with an explicit retry classification.
    assert_eq!(
        output.scheduler_status.disposition,
        SchedulerDisposition::RetryTick
    );
    assert_eq!(
        output.scheduler_status.wake_class,
        SchedulerWakeClass::RetryTick
    );
    assert!(output.scheduler_status.reason.contains("never replayed"));
    // Completed branch receipts are durable and reported exactly once.
    assert_eq!(output.rebase_receipts, vec![receipt]);
    assert!(output.receipt.changed);
    assert_eq!(output.receipt.completed_steps.len(), 1);
    // Ordinary convergence is intentionally skipped, so nothing is armed or
    // observed against a chain that is still mid-rebuild.
    assert!(output.ci.is_empty());
    assert!(output.root_auto_merge.is_empty());
    assert!(output.head_advancements.is_empty());
    assert!(output.events.is_empty());
    // No admission runs while a caravan is draining, and the receipt says why.
    assert!(output.auto_admission.joins.is_empty());
    assert_eq!(
        output.auto_admission.continuation,
        AutoAdmissionContinuation::RequiresConvergedFleet,
    );
    lock.release().unwrap();
}

#[test]
fn a_fully_retained_chain_still_converges_when_reconciliation_alone_overruns() {
    // The livelock the bounded prefix must not create: every member already
    // rebased, but the deferrable reconciliation reserve no longer fits. The
    // tick has nothing irreversible left to protect, so convergence must run
    // instead of deferring forever behind a reserve it can never satisfy.
    let mut context = cacophony_context();
    context.config.sync.max_duration_secs = 2_400;
    let deadline = sync_operation_budget(&context);
    let status = status(linear_chain(7), Some(PrNumber(1)), &clean);
    let retained = with_retained_prefix(chain_costs(&status), 7);

    assert!(
        complete_budget(&context, &retained).required > deadline,
        "this fixture must exercise the reconciliation overrun"
    );

    let admission = admit_physical_prefix(&context, &retained, deadline);

    assert!(admission.makes_progress());
    assert!(admission.deferred.is_empty());
    assert_eq!(admission.pending_admitted, 0);
    assert!(!admission.deferred_convergence);
    // Only the hard reserve is held, so the precommit barrier admits the tick.
    assert!(admission.budget.required < deadline);
    assert!(admission.budget.required < admission.complete_budget.required);
}

// --- Missing required-run detection ------------------------------------------
//
// Live caravan2208 evidence: a rebase-on-join published root head
// `79abc31d…` for which GitHub never started a workflow run. Required contexts
// `Check & Lint` and `Fast Tests (unit)` had zero reporting runs, so the PR sat
// MERGEABLE/BLOCKED with nothing pending and nothing failed while Cara reported
// healthy. These fixtures pin the exact classification and recovery instead.

const CHECK_LINT: &str = "Check & Lint";
const FAST_TESTS: &str = "Fast Tests (unit)";

fn required_context_names() -> [&'static str; 2] {
    [CHECK_LINT, FAST_TESTS]
}

fn head_of(pulls: &[PullRequestSnapshot], number: u64) -> String {
    pulls
        .iter()
        .find(|pull_request| pull_request.number == PrNumber(number))
        .expect("fixture PR")
        .head
        .oid
        .0
        .clone()
}

fn lineage(
    head_sha: &str,
    suites: Vec<CheckSuiteLineage>,
    runs: Vec<WorkflowRunLineage>,
) -> HeadRunLineage {
    HeadRunLineage {
        head_sha: head_sha.to_owned(),
        check_suites: suites,
        workflow_runs: runs,
        head_committed_at: Some(PUBLISHED_AT.to_owned()),
        complete: true,
    }
    .bounded()
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

fn head_run(run_id: u64, head_sha: &str, status: &str, conclusion: &str) -> WorkflowRunLineage {
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

fn required_runs_receipt(
    progress: &SyncProgress,
    number: u64,
) -> &crate::required_runs::RequiredRunsReceipt {
    progress
        .required_runs
        .iter()
        .find(|receipt| receipt.pr == PrNumber(number))
        .expect("every verified member carries a receipt")
}

#[test]
fn a_head_with_zero_required_runs_is_reported_instead_of_waiting_forever() {
    let pulls = healthy_chain();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.require_contexts("main", &required_context_names());
    let status = status(pulls.clone(), Some(PrNumber(1)), &clean);

    let progress =
        execute(&status, &provider, false, false, false).expect("a stall is not an error");

    let receipt = required_runs_receipt(&progress, 1);
    assert_eq!(
        receipt.assessment.status,
        RequiredRunsStatus::MissingRequiredRuns
    );
    assert!(receipt.hash_is_valid());
    assert_eq!(
        receipt.assessment.missing_contexts,
        vec![CHECK_LINT.to_owned(), FAST_TESTS.to_owned()]
    );
    assert_eq!(receipt.assessment.observed_check_suites, 0);
    assert_eq!(receipt.assessment.observed_runs, 0);

    let problem = progress
        .missing_required_runs
        .iter()
        .find(|problem| problem.pr == PrNumber(1))
        .expect("the stall must be visible");
    assert_eq!(problem.kind, MissingRequiredRunsKind::MissingRequiredRuns);
    assert!(problem.operator_action_required);
    assert_eq!(problem.head.oid.0, head_of(&pulls, 1));
    assert!(problem.next.contains("close and immediately reopen"));

    // Head, base, branch, and membership stay exactly as they were: no empty
    // commit, no close/reopen loop, no force, no broad rerun.
    let after = provider.pulls.borrow()[&PrNumber(1)].clone();
    assert_eq!(after.head, pulls[0].head);
    assert_eq!(after.base, pulls[0].base);
    assert_eq!(after.labels, pulls[0].labels);
    assert_eq!(after.state, pulls[0].state);
    assert!(
        !provider
            .calls
            .borrow()
            .iter()
            .any(|kind| *kind == MutationKind::RequestCheckSuite
                || *kind == MutationKind::RerunChecks),
        "no safe rerequestable suite exists, so nothing may be triggered"
    );

    let scheduler = successful_scheduler_status(
        &status,
        &progress.ci,
        &progress.paused_caravans,
        false,
        &progress.required_runs,
        &progress.missing_required_runs,
    );
    assert_eq!(scheduler.disposition, SchedulerDisposition::OperatorAction);
    assert_eq!(scheduler.wake_class, SchedulerWakeClass::OperatorAction);
    assert_eq!(scheduler.missing_required_runs.len(), 1);
}

#[test]
fn one_missing_required_context_still_blocks_while_the_other_reports() {
    let mut pulls = healthy_chain();
    pulls[0].checks = vec![check(CHECK_LINT, CheckState::Success, Some(7))];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.require_contexts("main", &required_context_names());
    let head = head_of(&pulls, 1);
    provider.serve_lineage(
        PrNumber(1),
        lineage(
            &head,
            Vec::new(),
            vec![head_run(7, &head, "completed", "success")],
        ),
    );
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress =
        execute(&status, &provider, false, false, false).expect("a stall is not an error");

    let receipt = required_runs_receipt(&progress, 1);
    assert_eq!(
        receipt.assessment.status,
        RequiredRunsStatus::MissingRequiredRuns
    );
    assert_eq!(
        receipt.assessment.missing_contexts,
        vec![FAST_TESTS.to_owned()],
        "only the uncovered context may be named"
    );
    assert_eq!(receipt.assessment.observed_runs, 1);
    let problem = &progress.missing_required_runs[0];
    assert_eq!(problem.contexts, vec![FAST_TESTS.to_owned()]);
}

#[test]
fn a_delayed_run_inside_the_grace_period_is_an_ordinary_ci_wait() {
    let pulls = healthy_chain();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.require_contexts("main", &required_context_names());
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress = execute_with_required_runs(
        &status,
        &provider,
        RequiredRunsPolicy {
            // The fixture head was published in 2020, so only an absurd grace
            // keeps the tick inside the window.
            grace_secs: u64::MAX,
            retrigger: true,
        },
    )
    .expect("waiting is not an error");

    let receipt = required_runs_receipt(&progress, 1);
    assert_eq!(receipt.assessment.status, RequiredRunsStatus::AwaitingGrace);
    assert!(!receipt.assessment.grace_elapsed);
    assert!(
        progress.missing_required_runs.is_empty(),
        "nothing may be declared missing inside the bounded grace period"
    );
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.kind != EventKind::RequiredRunsMissing)
    );

    let scheduler = successful_scheduler_status(
        &status,
        &progress.ci,
        &progress.paused_caravans,
        false,
        &progress.required_runs,
        &progress.missing_required_runs,
    );
    assert_eq!(scheduler.disposition, SchedulerDisposition::WaitingCi);
    assert!(scheduler.waiting_prs.contains(&PrNumber(1)));
}

#[test]
fn a_delayed_run_that_finally_arrives_is_pending_not_missing() {
    let pulls = healthy_chain();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.require_contexts("main", &required_context_names());
    let head = head_of(&pulls, 1);
    provider.serve_lineage(
        PrNumber(1),
        lineage(
            &head,
            vec![suite(4242, &head, "queued", "")],
            vec![head_run(30_222_268_397, &head, "queued", "")],
        ),
    );
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress = execute(&status, &provider, false, false, false).expect("pending waits");

    assert_eq!(
        required_runs_receipt(&progress, 1).assessment.status,
        RequiredRunsStatus::Pending
    );
    assert!(progress.missing_required_runs.is_empty());
    assert!(
        provider.rerequests.borrow().is_empty(),
        "a live suite is never retriggered"
    );
}

#[test]
fn a_run_on_a_superseded_head_never_satisfies_the_current_head() {
    let pulls = healthy_chain();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.require_contexts("main", &required_context_names());
    let head = head_of(&pulls, 1);
    provider.serve_lineage(
        PrNumber(1),
        lineage(
            &head,
            vec![suite(11, "pre-rebase-generation", "completed", "success")],
            vec![head_run(
                30_222_100_000,
                "pre-rebase-generation",
                "completed",
                "success",
            )],
        ),
    );
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress =
        execute(&status, &provider, false, false, false).expect("a stall is not an error");

    let receipt = required_runs_receipt(&progress, 1);
    assert_eq!(
        receipt.assessment.status,
        RequiredRunsStatus::MissingRequiredRuns
    );
    assert_eq!(receipt.assessment.observed_runs, 0);
    assert_eq!(receipt.assessment.stale_head_runs, 2);
    assert!(
        provider.rerequests.borrow().is_empty(),
        "a superseded generation must never be retriggered"
    );
}

#[test]
fn a_cancelled_superseded_suite_is_retriggered_exactly_once_and_recovers() {
    let pulls = healthy_chain();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.require_contexts("main", &required_context_names());
    let head = head_of(&pulls, 1);
    provider.allow_rerequest(4242, &head);
    provider.serve_lineage(
        PrNumber(1),
        lineage(
            &head,
            vec![suite(4242, &head, "completed", "cancelled")],
            vec![head_run(30_222_268_000, &head, "completed", "cancelled")],
        ),
    );
    provider.serve_lineage(
        PrNumber(1),
        lineage(
            &head,
            vec![suite(4242, &head, "queued", "")],
            vec![head_run(30_222_268_397, &head, "queued", "")],
        ),
    );
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress = execute(&status, &provider, false, false, false).expect("recovery converges");

    assert_eq!(
        *provider.rerequests.borrow(),
        vec![(PrNumber(1), 4242)],
        "exactly one auditable request against the unchanged head"
    );
    let receipt = required_runs_receipt(&progress, 1);
    assert_eq!(receipt.assessment.status, RequiredRunsStatus::Pending);
    let retrigger = receipt.retrigger.as_ref().expect("retrigger receipt");
    assert!(retrigger.requested);
    assert!(retrigger.rediscovered);
    assert_eq!(retrigger.attempts, 1);
    assert_eq!(retrigger.head_oid.0, head);
    assert!(retrigger.failure.is_none());
    assert!(
        progress.missing_required_runs.is_empty(),
        "a recovered head is not an operator problem"
    );
    assert!(
        progress
            .events
            .iter()
            .any(|event| event.kind == EventKind::RequiredRunsRetriggered)
    );
}

#[test]
fn a_refused_retrigger_becomes_a_typed_operator_problem() {
    let pulls = healthy_chain();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.require_contexts("main", &required_context_names());
    let head = head_of(&pulls, 1);
    // The suite is advertised as rerequestable but the provider refuses it.
    provider.serve_lineage(
        PrNumber(1),
        lineage(
            &head,
            vec![suite(4242, &head, "completed", "cancelled")],
            Vec::new(),
        ),
    );
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress =
        execute(&status, &provider, false, false, false).expect("a refusal is not a crash");

    let receipt = required_runs_receipt(&progress, 1);
    let retrigger = receipt.retrigger.as_ref().expect("retrigger receipt");
    assert!(!retrigger.requested);
    assert!(!retrigger.rediscovered);
    assert!(retrigger.failure.is_some());
    assert_eq!(
        retrigger.status_after,
        RequiredRunsStatus::CancelledSuperseded,
        "a refused request changes nothing, so the pre-recovery verdict stands"
    );
    let problem = progress
        .missing_required_runs
        .iter()
        .find(|problem| problem.pr == PrNumber(1))
        .expect("a refused recovery stays visible");
    assert_eq!(
        problem.kind,
        MissingRequiredRunsKind::CancelledSupersededRequiredRuns
    );
    assert!(problem.operator_action_required);
    assert!(problem.retrigger.is_some());
}

#[test]
fn disabled_retrigger_still_detects_and_reports_the_stall() {
    let pulls = healthy_chain();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.require_contexts("main", &required_context_names());
    let head = head_of(&pulls, 1);
    provider.allow_rerequest(4242, &head);
    provider.serve_lineage(
        PrNumber(1),
        lineage(
            &head,
            vec![suite(4242, &head, "completed", "cancelled")],
            Vec::new(),
        ),
    );
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress = execute_with_required_runs(
        &status,
        &provider,
        RequiredRunsPolicy {
            grace_secs: 300,
            retrigger: false,
        },
    )
    .expect("detection never depends on recovery");

    assert!(provider.rerequests.borrow().is_empty());
    assert_eq!(progress.missing_required_runs.len(), 1);
    assert!(required_runs_receipt(&progress, 1).retrigger.is_none());
}

#[test]
fn a_partial_provider_read_never_claims_a_missing_required_run() {
    let pulls = healthy_chain();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.partial_contexts("main");
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress =
        execute(&status, &provider, false, false, false).expect("unknown is not an error");

    let receipt = required_runs_receipt(&progress, 1);
    assert_eq!(
        receipt.assessment.status,
        RequiredRunsStatus::UnknownProviderState
    );
    assert!(!receipt.assessment.provider_reads_complete);
    assert!(
        provider.lineage_reads.borrow().is_empty(),
        "an unreadable protection endpoint must not trigger lineage reads"
    );
    let problem = &progress.missing_required_runs[0];
    assert_eq!(problem.kind, MissingRequiredRunsKind::UnknownProviderState);
    assert!(!problem.operator_action_required);

    let scheduler = successful_scheduler_status(
        &status,
        &progress.ci,
        &progress.paused_caravans,
        false,
        &progress.required_runs,
        &progress.missing_required_runs,
    );
    assert_eq!(scheduler.disposition, SchedulerDisposition::RetryTick);
    assert_eq!(scheduler.wake_class, SchedulerWakeClass::RetryTick);
}

#[test]
fn a_partial_lineage_read_never_claims_a_missing_required_run() {
    let pulls = healthy_chain();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.require_contexts("main", &required_context_names());
    let head = head_of(&pulls, 1);
    provider.serve_lineage(
        PrNumber(1),
        HeadRunLineage {
            head_sha: head,
            check_suites: Vec::new(),
            workflow_runs: Vec::new(),
            head_committed_at: Some(PUBLISHED_AT.to_owned()),
            complete: false,
        },
    );
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress =
        execute(&status, &provider, false, false, false).expect("unknown is not an error");

    assert_eq!(
        required_runs_receipt(&progress, 1).assessment.status,
        RequiredRunsStatus::UnknownProviderState
    );
    assert!(provider.rerequests.borrow().is_empty());
}

#[test]
fn a_satisfied_head_reads_no_lineage_and_reports_no_problem() {
    let mut pulls = healthy_chain();
    pulls[0].checks = vec![
        check(CHECK_LINT, CheckState::Success, Some(7)),
        check(FAST_TESTS, CheckState::Success, Some(8)),
    ];
    pulls[1].checks = vec![check("build-test", CheckState::Success, Some(9))];
    pulls[2].checks = vec![check("build-test", CheckState::Success, Some(10))];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.require_contexts("main", &required_context_names());
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress = execute(&status, &provider, false, false, false).expect("healthy converges");

    assert_eq!(
        required_runs_receipt(&progress, 1).assessment.status,
        RequiredRunsStatus::Satisfied
    );
    assert!(
        provider.lineage_reads.borrow().is_empty(),
        "the expensive lineage read belongs to the pathological path only"
    );
    assert!(progress.missing_required_runs.is_empty());

    let scheduler = successful_scheduler_status(
        &status,
        &progress.ci,
        &progress.paused_caravans,
        false,
        &progress.required_runs,
        &progress.missing_required_runs,
    );
    assert_eq!(scheduler.disposition, SchedulerDisposition::Healthy);
}

#[test]
fn an_unprotected_base_requires_nothing_from_a_member() {
    let pulls = healthy_chain();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.require_contexts("main", &required_context_names());
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress =
        execute(&status, &provider, false, false, false).expect("a stall is not an error");

    for number in [2_u64, 3] {
        assert_eq!(
            required_runs_receipt(&progress, number).assessment.status,
            RequiredRunsStatus::NotRequired,
            "member #{number} stacks on an unprotected branch"
        );
    }
    assert!(!provider.lineage_reads.borrow().contains(&PrNumber(2)));
}

#[test]
fn one_stalled_member_never_hides_or_contaminates_another() {
    let mut pulls = healthy_chain();
    pulls[1].checks = vec![
        check(CHECK_LINT, CheckState::Success, Some(9)),
        check(FAST_TESTS, CheckState::InProgress, Some(10)),
    ];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    // Both the root and its first child stack on protected branches.
    provider.require_contexts("main", &required_context_names());
    provider.require_contexts("one", &required_context_names());
    let status = status(pulls.clone(), Some(PrNumber(1)), &clean);

    let progress =
        execute(&status, &provider, false, false, false).expect("a stall is not an error");

    assert_eq!(
        required_runs_receipt(&progress, 1).assessment.status,
        RequiredRunsStatus::MissingRequiredRuns
    );
    assert_eq!(
        required_runs_receipt(&progress, 2).assessment.status,
        RequiredRunsStatus::Pending,
        "a healthy member must be unaffected by its stalled predecessor"
    );
    assert_eq!(progress.missing_required_runs.len(), 1);
    assert_eq!(progress.missing_required_runs[0].pr, PrNumber(1));
    assert!(
        !provider.lineage_reads.borrow().contains(&PrNumber(2)),
        "a fully reported member never pays for its predecessor's stall"
    );
}

#[test]
fn independently_stalled_members_are_reported_separately() {
    let pulls = healthy_chain();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.require_contexts("main", &required_context_names());
    provider.require_contexts("one", &required_context_names());
    let status = status(pulls, Some(PrNumber(1)), &clean);

    let progress =
        execute(&status, &provider, false, false, false).expect("a stall is not an error");

    let stalled = progress
        .missing_required_runs
        .iter()
        .map(|problem| problem.pr)
        .collect::<Vec<_>>();
    assert_eq!(stalled, vec![PrNumber(1), PrNumber(2)]);
    let fingerprints = progress
        .missing_required_runs
        .iter()
        .map(|problem| problem.fingerprint.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        fingerprints.len(),
        2,
        "each stalled member owns a distinct identity"
    );
}

#[test]
fn missing_required_run_hook_evidence_is_deduplicated_and_bounded() {
    let pulls = healthy_chain();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.require_contexts("main", &required_context_names());
    let status = status(pulls, Some(PrNumber(1)), &clean);

    // Two caravans over the same member cannot double-report the same stall.
    let mut progress = execute(&status, &provider, false, false, false).expect("first pass");
    let replay = execute(&status, &provider, false, false, false).expect("second pass");
    merge_sync_progress(&mut progress, replay);

    assert_eq!(progress.missing_required_runs.len(), 1);
    assert_eq!(
        progress
            .required_runs
            .iter()
            .filter(|receipt| receipt.pr == PrNumber(1))
            .count(),
        1,
        "receipts are keyed by member, never appended per pass"
    );
    let emitted = progress
        .events
        .iter()
        .filter(|event| event.kind == EventKind::RequiredRunsMissing)
        .count();
    assert_eq!(
        emitted, 1,
        "two convergence passes over one member must not notify hooks twice"
    );
    let problem = &progress.missing_required_runs[0];
    assert!(problem.contexts.len() <= crate::required_runs::MAX_REPORTED_CONTEXTS);
}

#[test]
fn grace_starts_at_publication_not_at_a_preserved_commit_date() {
    // Live Cacophony PR2208 evidence: the rebased head
    // `79abc31d4efc07a579145cf904c83c1420f8b4ac` kept committer date
    // 2026-07-26T15:27:43Z while the branch was actually published at
    // 2026-07-26T22:01:09Z (`updatedAt`). Starting the grace countdown at the
    // preserved commit date would accuse every rebased head of missing its runs
    // roughly six hours before GitHub could possibly have started one.
    const PRESERVED_COMMIT_DATE: &str = "2026-07-26T15:27:43Z";
    const PUBLISHED: &str = "2026-07-26T22:01:09Z";

    let mut pull_request = pull_request(
        2208,
        "one",
        "main",
        PullRequestState::Open,
        AutoMergeState::squash(),
    );
    pull_request.updated_at = Some(PUBLISHED.to_owned());
    let lineage = HeadRunLineage {
        head_sha: pull_request.head.oid.0.clone(),
        check_suites: Vec::new(),
        workflow_runs: Vec::new(),
        head_committed_at: Some(PRESERVED_COMMIT_DATE.to_owned()),
        complete: true,
    };

    assert_eq!(
        head_published_at(&pull_request, Some(&lineage)).as_deref(),
        Some(PUBLISHED),
        "the later provider timestamp owns the countdown start"
    );

    // A tick two minutes after publication is still inside the default grace.
    let published_unix = crate::required_runs::rfc3339_to_unix_secs(PUBLISHED).expect("timestamp");
    let assessment = crate::required_runs::assess(&crate::required_runs::RequiredRunsInput {
        pr: pull_request.number,
        head: &pull_request.head,
        base: &pull_request.base,
        contexts: &RequiredContextsRead {
            branch: "main".to_owned(),
            protected: true,
            contexts: vec![CHECK_LINT.to_owned(), FAST_TESTS.to_owned()],
            complete: true,
        },
        lineage: Some(&lineage),
        checks: &pull_request.checks,
        head_published_at: head_published_at(&pull_request, Some(&lineage)).as_deref(),
        clock: crate::required_runs::RequiredRunsClock {
            now_unix: published_unix + 120,
            grace_secs: DEFAULT_MISSING_REQUIRED_RUNS_GRACE_SECS,
        },
    });
    assert_eq!(assessment.status, RequiredRunsStatus::AwaitingGrace);
    assert_eq!(assessment.head_age_secs, Some(120));

    // Well past the grace, the same head is the stalling class.
    let stalled = crate::required_runs::assess(&crate::required_runs::RequiredRunsInput {
        pr: pull_request.number,
        head: &pull_request.head,
        base: &pull_request.base,
        contexts: &RequiredContextsRead {
            branch: "main".to_owned(),
            protected: true,
            contexts: vec![CHECK_LINT.to_owned(), FAST_TESTS.to_owned()],
            complete: true,
        },
        lineage: Some(&lineage),
        checks: &pull_request.checks,
        head_published_at: head_published_at(&pull_request, Some(&lineage)).as_deref(),
        clock: crate::required_runs::RequiredRunsClock {
            now_unix: published_unix + DEFAULT_MISSING_REQUIRED_RUNS_GRACE_SECS + 1,
            grace_secs: DEFAULT_MISSING_REQUIRED_RUNS_GRACE_SECS,
        },
    });
    assert_eq!(stalled.status, RequiredRunsStatus::MissingRequiredRuns);
}

// ---------------------------------------------------------------------------
// Caravan-owned root promotion and direct squash merge (bd-f8cf99).
//
// Live incident: PR2210 squash-landed on `main`; PR2213 stayed based on
// PR2210's generation branch, was armed with provider-native auto-merge while
// still `CLEAN`, and merged *instantly into that already-merged predecessor*
// instead of `main`. Its content never reached `main` and PR2215 inherited both
// the cumulative content and a dangling base.
//
// These fixtures pin the replacement contract: one merge actor, one ordered
// fenced transaction — promote to the exact default branch, re-verify, prove the
// already-validated tree is what lands, then squash exactly once and prove it
// reached the default branch before advancing the root.
// ---------------------------------------------------------------------------

/// Fleet fixture under the default caravan-owned merge actor.
fn caravan_status(
    pulls: Vec<PullRequestSnapshot>,
    current: Option<PrNumber>,
    identical_trees: bool,
) -> StatusOutput {
    caravan_status_with_containment(pulls, current, identical_trees, true)
}

/// As [`caravan_status`], but able to model a default branch the caravan head
/// does not contain — for example after an operator reverted or discarded an
/// already-landed ancestor.
fn caravan_status_with_containment(
    pulls: Vec<PullRequestSnapshot>,
    current: Option<PrNumber>,
    identical_trees: bool,
    target_reachable_from_candidate: bool,
) -> StatusOutput {
    let mut status = status(pulls, current, &clean);
    // Caravan-owned merging is opt-in: these fixtures set it explicitly, just
    // as a repository does once every consumer of its config understands the
    // key.
    status.head_merge = crate::read::HeadMergeStatus {
        actor: crate::model::HeadMergeActor::Caravan,
        ..crate::read::HeadMergeStatus::default()
    };
    let snapshot = RepositorySnapshot {
        merge_candidates: Vec::new(),
        merge_candidates_truncated: 0,
        previous_default_oid: None,
        default_branch_movements: Vec::new(),
        repository: repository(),
        default_branch: branch("main"),
        current_branch: status.current_branch.clone(),
        current_pr: status.current_pr,
        pull_requests: status.analysis.pull_requests.values().cloned().collect(),
        generation_facts: Vec::new(),
        observed_at: None,
    };
    let mut analysis =
        graph::analyze_for_actor(&snapshot, &clean, crate::model::HeadMergeActor::Caravan)
            .expect("analysis");
    // Members are physically rebased before CI runs, so the exact head SHA
    // already carries the cumulative reviewed tree. `identical_trees=false`
    // models a default branch that gained foreign content since then.
    analysis.cumulative_trees = analysis
        .fleet
        .caravans
        .iter()
        .flat_map(|caravan| caravan.members.clone())
        .filter_map(|member| analysis.pull_requests.get(&member).cloned())
        .map(|head| crate::model::CumulativeTreeProof {
            candidate: head.head.clone(),
            target: branch("main"),
            candidate_tree: CommitOid("tree-validated".to_owned()),
            merge_result_tree: CommitOid(if identical_trees {
                "tree-validated".to_owned()
            } else {
                "tree-foreign".to_owned()
            }),
            identical: identical_trees,
            target_reachable_from_candidate,
        })
        .collect();
    status.healthy = analysis.healthy();
    status.analysis = analysis;
    status
}

/// One green, disarmed caravan member.
fn caravan_member(number: u64, head: &str, base: &str) -> PullRequestSnapshot {
    let mut pull_request = pull_request(
        number,
        head,
        base,
        PullRequestState::Open,
        AutoMergeState::disabled(),
    );
    pull_request.checks = vec![check("build-test", CheckState::Success, None)];
    pull_request
}

#[test]
fn a_promoted_green_root_is_squash_merged_by_cara_with_sealed_landing_proof() {
    let pulls = vec![caravan_member(1, "one", "main")];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = caravan_status(pulls, Some(PrNumber(1)), true);

    let progress = execute(&status, &provider, false, false, false).expect("cara merges the root");

    // Exactly one non-admin squash, and never the administrator bypass.
    assert_eq!(*provider.calls.borrow(), vec![MutationKind::SquashMerge]);
    assert_eq!(
        provider.pulls.borrow()[&PrNumber(1)].state,
        PullRequestState::Merged
    );
    let receipt = progress
        .root_merge
        .iter()
        .find(|receipt| receipt.pr == PrNumber(1))
        .expect("a landed root carries a durable merge receipt");
    assert!(receipt.hash_is_valid());
    assert!(receipt.proves_default_branch_landing());
    assert_eq!(receipt.merge_method, crate::model::MergeMethod::Squash);
    assert_eq!(receipt.base.name, "main");
    assert_eq!(
        receipt.provenance.owner,
        crate::root_merge::ROOT_MERGE_OWNER
    );
    assert!(receipt.provenance.engine_mutated);
    assert!(
        receipt
            .ancestry
            .cumulative_tree
            .as_ref()
            .is_some_and(|proof| proof.identical),
        "landing is authorized by the already-validated tree"
    );
    assert!(receipt.ancestry.merge_commit.is_some());
    assert!(
        progress
            .events
            .iter()
            .any(|event| event.kind == EventKind::RootMerged)
    );
    assert!(
        progress.root_auto_merge.is_empty(),
        "cara never arms provider auto-merge"
    );
}

#[test]
fn a_root_still_based_on_a_merged_predecessor_is_retargeted_before_any_merge() {
    // Exactly the PR2210 -> PR2213 -> PR2215 topology: #2210 already merged,
    // #2213 still pointing at its generation branch, #2215 stacked behind.
    let merged_predecessor = pull_request(
        2210,
        "pr2210",
        "main",
        PullRequestState::Merged,
        AutoMergeState::disabled(),
    );
    let mut root = caravan_member(2213, "pr2213", "pr2210");
    root.labels.insert("caravan".to_owned());
    let child = caravan_member(2215, "pr2215", "pr2213");
    let pulls = vec![merged_predecessor, root, child];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let mut status = caravan_status(pulls, Some(PrNumber(2213)), true);
    // One landing per tick keeps this fixture focused on the exact topology
    // failure: the root must be retargeted before anything merges it.
    status.head_merge.max_root_merges_per_tick = 1;

    let progress = execute(&status, &provider, false, false, false).expect("promotion converges");

    // The retarget happens first and is proven, and the child is untouched.
    let promotion = progress
        .root_promotion
        .iter()
        .find(|receipt| receipt.pr == PrNumber(2213))
        .expect("promoted root carries a durable promotion receipt");
    assert!(promotion.hash_is_valid());
    assert!(promotion.proves_default_base());
    assert_eq!(promotion.base_before.name, "pr2210");
    assert_eq!(promotion.base_after.name, "main");
    assert_eq!(promotion.predecessor, Some(PrNumber(2210)));
    assert!(promotion.predecessor_merged);
    assert_eq!(
        promotion.trigger,
        crate::root_merge::RootPromotionTrigger::MergedPredecessorRetarget
    );
    assert_eq!(
        provider.calls.borrow().first(),
        Some(&MutationKind::SetBase),
        "the base is authoritative before any merge mechanism"
    );
    assert_eq!(
        provider.pulls.borrow()[&PrNumber(2213)].base.name,
        "main",
        "the root never merges into an already-merged predecessor"
    );
    let child_after = provider.pulls.borrow()[&PrNumber(2215)].clone();
    assert_eq!(
        child_after.state,
        PullRequestState::Open,
        "child content is preserved, never rewritten or dropped"
    );
    assert_eq!(
        child_after.head.oid,
        status.analysis.pull_requests[&PrNumber(2215)].head.oid,
        "the child generation is retargeted, never rewritten"
    );
    assert_eq!(
        child_after.base.name, "main",
        "once its predecessor lands, the child is promoted rather than left dangling"
    );
    assert_eq!(
        child_after.auto_merge,
        AutoMergeState::disabled(),
        "the promoted child is never armed with a second merge actor"
    );
    assert!(
        progress
            .events
            .iter()
            .any(|event| event.kind == EventKind::RootPromoted)
    );
}

#[test]
fn a_root_whose_base_is_not_the_default_branch_is_refused_not_merged() {
    // The provider view drifts back to the predecessor base after promotion:
    // the exact live hazard. Cara refuses rather than landing on that branch.
    let pulls = vec![caravan_member(1, "one", "main")];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let mut drifted = provider.pulls.borrow()[&PrNumber(1)].clone();
    drifted.base = branch("predecessor");
    provider.serve_stale_read(PrNumber(1), drifted.clone());
    provider.serve_stale_read(PrNumber(1), drifted);
    let status = caravan_status(pulls, Some(PrNumber(1)), true);

    let error = execute(&status, &provider, false, false, false)
        .expect_err("a non-default base is never a merge target");

    // Whatever the exact fail-closed class, the invariant is absolute: nothing
    // merges while the observed base is not the exact default branch.
    assert!(!provider.calls.borrow().contains(&MutationKind::SquashMerge));
    let code = mcp_cli::StructuredError::code(&error);
    assert!(
        matches!(
            code.as_str(),
            "root_merge_refused" | "root_promotion_incomplete" | "stale_precondition"
        ),
        "{code}"
    );
}

#[test]
fn pending_and_unsatisfied_required_checks_wait_without_merging() {
    let mut pending = caravan_member(1, "one", "main");
    pending.checks = vec![check("build-test", CheckState::InProgress, None)];
    let pulls = vec![pending];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = caravan_status(pulls, Some(PrNumber(1)), true);

    let progress =
        execute(&status, &provider, false, false, false).expect("waiting is not failing");

    assert!(provider.calls.borrow().is_empty());
    assert!(progress.root_merge.is_empty());
    assert!(progress.steps.iter().any(|step| {
        step.kind == MutationKind::SquashMerge
            && step.state == MutationStepState::AlreadySatisfied
            && step
                .summary
                .contains(crate::root_merge::RootMergeBlock::ChecksNotPassing.reason())
    }));
}

#[test]
fn a_changed_cumulative_tree_revalidates_instead_of_landing_unvalidated_content() {
    // Retarget-only promotion is sound exactly while the squash lands the tree
    // CI already validated. A default branch that gained foreign content makes
    // the merge result differ, so the caravan revalidates instead of merging.
    let pulls = vec![caravan_member(1, "one", "main")];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = caravan_status(pulls, Some(PrNumber(1)), false);

    let progress =
        execute(&status, &provider, false, false, false).expect("a changed tree is a bounded wait");

    assert!(provider.calls.borrow().is_empty());
    assert!(progress.root_merge.is_empty());
    assert!(progress.steps.iter().any(|step| {
        step.kind == MutationKind::SquashMerge
            && step
                .summary
                .contains(crate::root_merge::RootMergeBlock::CumulativeTreeChanged.reason())
    }));
}

#[test]
fn an_unproven_cumulative_tree_never_authorizes_a_landing() {
    let pulls = vec![caravan_member(1, "one", "main")];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let mut status = caravan_status(pulls, Some(PrNumber(1)), true);
    status.analysis.cumulative_trees.clear();

    let progress = execute(&status, &provider, false, false, false).expect("unproven is a wait");

    assert!(provider.calls.borrow().is_empty());
    assert!(progress.root_merge.is_empty());
    assert!(progress.steps.iter().any(|step| {
        step.summary
            .contains(crate::root_merge::RootMergeBlock::CumulativeTreeUnproven.reason())
    }));
}

#[test]
fn a_merge_the_default_branch_does_not_contain_never_counts_as_delivered() {
    let pulls = vec![
        caravan_member(1, "one", "main"),
        caravan_member(2, "two", "one"),
    ];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.serve_unreachable_merge(PrNumber(1), "3da6addbf0b1334888658413a74ac91f18afb9ea");
    let status = caravan_status(pulls, Some(PrNumber(1)), true);

    let error = execute(&status, &provider, false, false, false)
        .expect_err("an unreachable merge commit is not a landing");

    assert_eq!(mcp_cli::StructuredError::code(&error), "root_merge_refused");
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(details["cause"], "merge_not_reachable_from_default");
    assert_eq!(
        details["evidence"]["claimed_merge_commit"],
        "3da6addbf0b1334888658413a74ac91f18afb9ea"
    );
    assert!(
        !provider.calls.borrow().contains(&MutationKind::SetBase),
        "no successor is promoted without landing proof"
    );
}

#[test]
fn a_provider_that_never_exposes_the_merge_stops_with_a_typed_resumable_cause() {
    let pulls = vec![caravan_member(1, "one", "main")];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.never_persist_merge(
        PrNumber(1),
        crate::root_merge::ROOT_MERGE_CONFIRMATION_READS,
    );
    let status = caravan_status(pulls, Some(PrNumber(1)), true);

    let error = execute(&status, &provider, false, false, false)
        .expect_err("an unproven merge is never reported as landed");

    assert_eq!(mcp_cli::StructuredError::code(&error), "root_merge_refused");
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(details["cause"], "provider_did_not_persist_merge");
    assert_eq!(details["resumable"], true);
    assert_eq!(details["operator_action_required"], false);
}

#[test]
fn an_already_merged_root_replays_idempotently_and_advances_the_next_root() {
    let mut merged = caravan_member(1, "one", "main");
    merged.state = PullRequestState::Merged;
    merged.merged_at = Some("now".to_owned());
    let pulls = vec![merged, caravan_member(2, "two", "one")];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = caravan_status(pulls, Some(PrNumber(2)), true);

    let progress = execute(&status, &provider, false, false, false).expect("replay converges");

    assert_eq!(
        provider.pulls.borrow()[&PrNumber(2)].base.name,
        "main",
        "the successor is promoted to the default branch"
    );
    assert!(
        progress
            .root_promotion
            .iter()
            .any(|receipt| receipt.pr == PrNumber(2) && receipt.proves_default_base())
    );
    assert!(
        !provider
            .calls
            .borrow()
            .contains(&MutationKind::EnableAutoMerge)
    );
}

#[test]
fn a_whole_green_caravan_drains_in_one_bounded_tick_with_per_root_revalidation() {
    // Every iteration re-reads exact provider facts, re-promotes, re-observes
    // CI, and re-proves the cumulative tree before the next landing.
    let pulls = vec![
        caravan_member(1, "one", "main"),
        caravan_member(2, "two", "one"),
    ];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = caravan_status(pulls, Some(PrNumber(1)), true);

    let progress = execute(&status, &provider, false, false, false).expect("the caravan drains");

    assert_eq!(
        provider.pulls.borrow()[&PrNumber(1)].state,
        PullRequestState::Merged
    );
    // The successor is promoted, re-validated, and lands in the same tick
    // because its own cumulative tree is proven against the exact default
    // branch. Without that proof it would wait instead.
    assert_eq!(provider.pulls.borrow()[&PrNumber(2)].base.name, "main");
    assert_eq!(
        provider.pulls.borrow()[&PrNumber(2)].state,
        PullRequestState::Merged
    );
    assert_eq!(
        progress
            .root_merge
            .iter()
            .filter(|receipt| receipt.merged)
            .count(),
        2
    );
    assert!(
        progress
            .root_promotion
            .iter()
            .any(|receipt| receipt.pr == PrNumber(2))
    );
    assert_eq!(
        progress.head_advancements.first().map(|item| item.new_head),
        Some(PrNumber(2))
    );
    let landed = progress
        .root_merge
        .iter()
        .find(|receipt| receipt.pr == PrNumber(1))
        .expect("first root landed");
    assert_eq!(landed.ancestry.next_root, Some(PrNumber(2)));
    assert_eq!(landed.ancestry.remaining_members, vec![PrNumber(2)]);
}

#[test]
fn a_bounded_merge_allowance_defers_the_rest_of_the_chain_to_the_next_tick() {
    let pulls = vec![
        caravan_member(1, "one", "main"),
        caravan_member(2, "two", "one"),
    ];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let mut status = caravan_status(pulls, Some(PrNumber(1)), true);
    status.head_merge.max_root_merges_per_tick = 1;

    let progress = execute(&status, &provider, false, false, false).expect("bounded drain");

    assert_eq!(
        progress
            .root_merge
            .iter()
            .filter(|receipt| receipt.merged)
            .count(),
        1
    );
    assert!(progress.steps.iter().any(|step| {
        step.summary
            .contains(crate::root_merge::RootMergeBlock::MergeBudgetReached.reason())
    }));
}

#[test]
fn a_foreign_auto_merge_request_is_converged_away_or_refused_but_never_raced() {
    let mut armed = caravan_member(1, "one", "main");
    armed.auto_merge = AutoMergeState::squash();
    let pulls = vec![armed];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = caravan_status(pulls.clone(), Some(PrNumber(1)), true);

    let progress =
        execute(&status, &provider, false, false, false).expect("foreign request cleared");

    assert_eq!(
        *provider.calls.borrow(),
        vec![MutationKind::DisableAutoMerge, MutationKind::SquashMerge],
        "the foreign merge actor is removed before cara merges"
    );
    assert!(progress.steps.iter().any(|step| {
        step.kind == MutationKind::DisableAutoMerge && step.summary.contains("single merge actor")
    }));

    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let mut refusing = caravan_status(pulls, Some(PrNumber(1)), true);
    refusing.head_merge.external_auto_merge_policy =
        crate::root_merge::ExternalAutoMergePolicy::Refuse;

    let error = execute(&refusing, &provider, false, false, false)
        .expect_err("reviewed policy refuses to race a second merge actor");

    assert_eq!(mcp_cli::StructuredError::code(&error), "root_merge_refused");
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(details["cause"], "foreign_auto_merge_actor");
    assert!(!provider.calls.borrow().contains(&MutationKind::SquashMerge));
}

#[test]
fn disabled_repository_auto_merge_no_longer_refuses_a_caravan_owned_tick() {
    let pulls = vec![caravan_member(1, "one", "main")];
    let mut provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.allows_auto_merge = false;
    let status = caravan_status(pulls.clone(), Some(PrNumber(1)), true);

    execute(&status, &provider, false, false, false)
        .expect("a repository that disabled native auto-merge still synchronizes");

    // Squash merging itself remains mandatory: cara performs the squash.
    let mut provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.allows_auto_merge = false;
    provider.allows_squash_merge = false;
    let error = execute(&status, &provider, false, false, false)
        .expect_err("cara cannot squash without squash merging");
    assert_eq!(
        mcp_cli::StructuredError::code(&error),
        "squash_merge_not_enabled"
    );
}

#[test]
fn a_caravan_owned_tick_never_mutates_compliant_child_members() {
    let pulls = vec![
        caravan_member(1, "one", "main"),
        caravan_member(2, "two", "one"),
        caravan_member(3, "three", "two"),
    ];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let mut status = caravan_status(pulls, Some(PrNumber(1)), true);
    status.head_merge.max_root_merges_per_tick = 1;

    execute(&status, &provider, false, false, false).expect("bounded drain");

    let state = provider.pulls.borrow();
    assert_eq!(state[&PrNumber(3)].base.name, "two");
    assert_eq!(state[&PrNumber(3)].auto_merge, AutoMergeState::disabled());
    assert_eq!(state[&PrNumber(3)].state, PullRequestState::Open);
}

#[test]
fn a_default_branch_that_moved_since_discovery_defers_the_landing_to_a_fresh_proof() {
    // Tree equality is what authorizes a landing, and a proof constructed
    // against a superseded default-branch generation proves nothing about the
    // one this merge would land on. Disjoint movement is not refused: the next
    // tick re-proves against the new generation and still lands when the
    // cumulative tree is identical.
    let pulls = vec![caravan_member(1, "one", "main")];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    *provider.default_branch_head_after_merge.borrow_mut() =
        Some(CommitOid("main-moved-by-someone-else".to_owned()));
    let status = caravan_status(pulls, Some(PrNumber(1)), true);

    let progress =
        execute(&status, &provider, false, false, false).expect("a moved default branch is a wait");

    assert!(!provider.calls.borrow().contains(&MutationKind::SquashMerge));
    assert!(progress.root_merge.is_empty());
    assert!(progress.steps.iter().any(|step| {
        step.summary
            .contains(crate::root_merge::RootMergeBlock::CumulativeTreeUnproven.reason())
    }));
}

#[test]
fn an_existing_config_on_a_new_runtime_keeps_the_native_merge_actor() {
    // bd-f8cf99 / backcompat-default-github. A runtime upgrade alone must never
    // change who merges a repository's pull requests. A fleet whose config
    // predates `head_merge_actor` therefore keeps arming the root exactly as
    // before, and cara performs no squash merge of its own.
    let status = crate::read::HeadMergeStatus::from_config(&crate::config::SyncConfig::default());
    assert_eq!(status.actor, crate::model::HeadMergeActor::Github);

    let mut pulls = healthy_chain();
    pulls.truncate(1);
    pulls[0].auto_merge = AutoMergeState::disabled();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    // `status()` is the historical fixture: exactly what an existing repository
    // looks like when the new binary is deployed with no config change.
    let fleet = self::status(pulls, Some(PrNumber(1)), &clean);
    assert_eq!(fleet.head_merge.actor, crate::model::HeadMergeActor::Github);

    let progress = execute(&fleet, &provider, false, false, false).expect("native actor converges");

    assert!(
        provider
            .calls
            .borrow()
            .contains(&MutationKind::EnableAutoMerge),
        "the historical actor still arms the root"
    );
    assert!(
        !provider.calls.borrow().contains(&MutationKind::SquashMerge),
        "cara does not take over merging without an explicit opt-in"
    );
    assert!(!progress.root_auto_merge.is_empty());
    assert!(progress.root_merge.is_empty());
}

#[test]
fn head_of_line_stall_names_the_exact_blocking_member_and_its_remedies() {
    // bd-3f99dc: a conflicted front member re-confirms the same refusal on
    // every tick while mergeable work waits behind it. Frequency cannot fix
    // that; naming the exact position, class, and remedy can.
    let mut pulls = healthy_chain();
    pulls[1].checks = vec![check("build-test", CheckState::Failure, Some(11))];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = status(pulls, Some(PrNumber(1)), &clean);
    let ci = vec![
        CiObservation {
            pr: PrNumber(1),
            disposition: CiDisposition::Passing,
            checks: Vec::new(),
            failed_runs: Vec::new(),
            failure_diagnostics: Vec::new(),
            rerunnable_run_ids: Vec::new(),
        },
        CiObservation {
            pr: PrNumber(2),
            disposition: CiDisposition::Failed,
            checks: Vec::new(),
            failed_runs: Vec::new(),
            failure_diagnostics: Vec::new(),
            rerunnable_run_ids: Vec::new(),
        },
    ];

    let scheduler = successful_scheduler_status(&status, &ci, &[], true, &[], &[]);

    assert_eq!(scheduler.head_of_line.len(), 1);
    let stall = &scheduler.head_of_line[0];
    assert_eq!(stall.blocking_pr, PrNumber(2));
    assert_eq!(
        stall.position, 2,
        "position, not attractiveness, selects work"
    );
    assert_eq!(stall.blocked_prs, vec![PrNumber(3)]);
    assert_eq!(stall.kind, crate::sync::HeadOfLineBlockKind::CiFailure);
    assert!(stall.remedies.iter().any(|remedy| remedy.contains("evict")));
    assert!(stall.fingerprint.starts_with("fnv1a64:"));
    assert_eq!(
        scheduler.disposition,
        SchedulerDisposition::ExternalDecision,
        "a stalled front is never reported as healthy or idle"
    );
    assert_eq!(scheduler.wake_class, SchedulerWakeClass::ExternalDecision);
    assert!(provider.calls.borrow().is_empty());
}

#[test]
fn converged_fleet_reports_no_head_of_line_stall() {
    let pulls = healthy_chain();
    let status = status(pulls, Some(PrNumber(1)), &clean);
    let ci = (1..=3)
        .map(|pr| CiObservation {
            pr: PrNumber(pr),
            disposition: CiDisposition::Passing,
            checks: Vec::new(),
            failed_runs: Vec::new(),
            failure_diagnostics: Vec::new(),
            rerunnable_run_ids: Vec::new(),
        })
        .collect::<Vec<_>>();

    let scheduler = successful_scheduler_status(&status, &ci, &[], true, &[], &[]);

    assert!(
        scheduler.head_of_line.is_empty(),
        "idle and stuck must stay distinguishable"
    );
    assert_eq!(scheduler.disposition, SchedulerDisposition::Healthy);
}

#[test]
fn an_operator_reverted_ancestor_is_refused_never_silently_reintroduced() {
    // Live migration fixture: cumulative root #2215 was landed and then
    // discarded/reverted by the operator on the default branch, while
    // successors #2223/#2225/#2227 still carry its diff because they were
    // physically rebased on top of it before CI.
    //
    // This is the case cumulative tree identity alone cannot catch. The
    // three-way merge of the successor with the reverted default branch
    // reapplies #2215's diff and still yields exactly the successor's own tree,
    // so `identical` stays true. What changed is containment: the default
    // branch is no longer an ancestor of the successor. Landing here would
    // silently reintroduce content the operator deliberately removed, so the
    // tick refuses and leaves the decision to whoever can rescope the content.
    let pulls = vec![
        caravan_member(2223, "pr2223", "main"),
        caravan_member(2225, "pr2225", "pr2223"),
    ];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = caravan_status_with_containment(pulls, Some(PrNumber(2223)), true, false);

    let error = execute(&status, &provider, false, false, false)
        .expect_err("a reverted ancestor is never silently reintroduced");

    assert_eq!(mcp_cli::StructuredError::code(&error), "root_merge_refused");
    let details = mcp_cli::StructuredError::details(&error).expect("details");
    assert_eq!(
        details["cause"],
        "default_branch_diverged_from_retained_patch_set"
    );
    assert_eq!(
        details["resumable"], false,
        "rerunning cannot decide whether reverted content should return"
    );
    assert_eq!(details["operator_action_required"], true);
    // The tree proof still reports identical: that is exactly why containment
    // has to be a separate, load-bearing fact.
    assert_eq!(details["evidence"]["cumulative_tree"]["identical"], true);
    assert_eq!(
        details["evidence"]["cumulative_tree"]["target_reachable_from_candidate"],
        false
    );
    assert!(
        !provider.calls.borrow().contains(&MutationKind::SquashMerge),
        "no landing is attempted"
    );
    assert!(
        provider.pulls.borrow()[&PrNumber(2225)].state == PullRequestState::Open,
        "successors are left untouched for rescoping"
    );
}

#[test]
fn a_conflicting_caravan_wakes_a_repair_actor_but_a_race_only_reruns_the_tick() {
    // Scheduler posture: no Actions runtime, a Caco-managed cron tick, and
    // hooks dispatching repair agents. The cron cannot read prose, so the
    // caravan-owned merge actor must classify its own refusals: one error code
    // covers both bounded races and states no rerun can resolve.
    let pulls = vec![
        caravan_member(2223, "pr2223", "main"),
        caravan_member(2225, "pr2225", "pr2223"),
    ];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let status = caravan_status_with_containment(pulls, Some(PrNumber(2223)), true, false);

    let error = execute(&status, &provider, false, false, false).expect_err("refused");
    let scheduler = super::decision::scheduler_failure_status(&error);
    assert_eq!(scheduler.wake_class, SchedulerWakeClass::ExternalDecision);
    assert!(!scheduler.retryable);
    assert_eq!(scheduler.error_code, "root_merge_refused");

    // The repair-wake event carries the exact caravan and PRs, so a dispatched
    // agent starts from provider facts rather than from log scraping.
    let error = super::decision::attach_scheduler_failure(&error, &scheduler);
    let event = super::decision::sync_failed_event(&error).expect("a conflicting caravan wakes");
    assert_eq!(event.kind, EventKind::SyncFailed);
    assert_eq!(event.caravan_id, Some(PrNumber(2223)));
    assert_eq!(event.prs, vec![PrNumber(2223)]);
    assert_eq!(event.metadata["error_code"], "root_merge_refused");
    assert_eq!(
        event.metadata["scheduler_status"]["wake_class"],
        "external_decision"
    );
    assert!(
        event.metadata["decision_fingerprint"]
            .as_str()
            .is_some_and(|fingerprint| fingerprint.starts_with("fnv1a64:")),
        "external deduplication needs a stable fingerprint"
    );

    // A bounded provider race under the same error code is the opposite: rerun
    // the same idempotent tick and never wake a repair agent.
    let pulls = vec![caravan_member(1, "one", "main")];
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.never_persist_merge(
        PrNumber(1),
        crate::root_merge::ROOT_MERGE_CONFIRMATION_READS,
    );
    let status = caravan_status(pulls, Some(PrNumber(1)), true);

    let error = execute(&status, &provider, false, false, false).expect_err("unproven merge");
    let scheduler = super::decision::scheduler_failure_status(&error);
    assert_eq!(scheduler.error_code, "root_merge_refused");
    assert_eq!(scheduler.wake_class, SchedulerWakeClass::RetryTick);
    assert!(scheduler.retryable);
    let error = super::decision::attach_scheduler_failure(&error, &scheduler);
    assert!(
        super::decision::sync_failed_event(&error).is_none(),
        "a bounded race never dispatches a repair agent"
    );
}

#[test]
fn a_repository_without_squash_merging_is_operator_action_not_a_retry() {
    let pulls = vec![caravan_member(1, "one", "main")];
    let mut provider = FakeProvider::with_pull_requests(pulls.clone());
    provider.allows_squash_merge = false;
    let status = caravan_status(pulls, Some(PrNumber(1)), true);

    let error = execute(&status, &provider, false, false, false).expect_err("cannot squash");
    let scheduler = super::decision::scheduler_failure_status(&error);
    assert_eq!(scheduler.error_code, "squash_merge_not_enabled");
    assert_eq!(scheduler.wake_class, SchedulerWakeClass::OperatorAction);
    assert!(
        !scheduler.retryable,
        "repository settings are not fixed by rerunning the tick"
    );
}

/// Operator report, cacophony PR 2276: the forge said `mergeStateStatus=BLOCKED`
/// with `Rust Check & Lint: FAILURE` and `Rust Fast Tests` still pending, yet
/// Cara did not treat the PR as failed.
///
/// `classify_checks` evaluated `pending` BEFORE `failed`, so any concurrently
/// running check masked a hard failure. A failing required check does not become
/// successful by waiting, so failure must be decisive: admitting it would stack
/// every following PR on top of known-red work, which is the exact re-stitching
/// cost the queue exists to prevent.
#[test]
fn a_hard_failure_is_not_masked_by_a_still_running_check() {
    let failed_then_pending = vec![
        CheckSnapshot {
            name: "Rust Check & Lint work".to_owned(),
            state: crate::model::CheckState::Failure,
            provider_state: Some("FAILURE".to_owned()),
            details_url: None,
        },
        CheckSnapshot {
            name: "Rust Fast Tests work".to_owned(),
            state: crate::model::CheckState::InProgress,
            provider_state: None,
            details_url: None,
        },
    ];

    assert_eq!(
        classify_checks(&failed_then_pending, false),
        CiDisposition::Failed,
        "a FAILURE must not be reported as merely waiting because a sibling check is still running"
    );
}

/// An absent conclusion is not a success. GitHub reports a check with no
/// conclusion while it is still running, and coercing that to passing would
/// admit work whose result nobody has seen.
#[test]
fn an_absent_conclusion_is_never_treated_as_passing() {
    let unknown = vec![CheckSnapshot {
        name: "Rust Fast Tests work".to_owned(),
        state: crate::model::CheckState::Unknown,
        provider_state: None,
        details_url: None,
    }];

    assert_ne!(
        classify_checks(&unknown, false),
        CiDisposition::Passing,
        "an unknown conclusion must never classify as passing"
    );
}

/// bd-550b0e: live on Cacophony (2026-07-30) a fleet with zero caravans, eight
/// unqueued PRs, and two `candidate_incompatible` problems aborted every tick
/// with `invalid_graph`, so automatic admission was never reached and the first
/// caravan could never form. `blocks_fleet` classified the kind correctly, but
/// the two sync-tick guards never consulted it.
#[test]
fn a_conflicting_unqueued_candidate_never_aborts_the_tick() {
    let pulls = healthy_chain();
    let provider = FakeProvider::with_pull_requests(pulls.clone());
    let mut status = status(pulls, Some(PrNumber(1)), &clean);
    status
        .analysis
        .fleet
        .problems
        .push(crate::model::GraphProblem {
            kind: GraphProblemKind::CandidateIncompatible,
            prs: vec![PrNumber(2245)],
            message:
                "leading admission candidate does not merge cleanly into the current default branch"
                    .to_owned(),
        });
    status.healthy = status.analysis.healthy();
    assert!(
        status.healthy,
        "an unadmitted candidate is not fleet ill-health"
    );

    let progress = execute(&status, &provider, true, false, false)
        .expect("a non-member candidate conflict must never abort the tick");

    assert!(!progress.synchronized_caravans.is_empty());
    assert!(
        status
            .analysis
            .fleet
            .problems
            .iter()
            .any(|problem| problem.kind == GraphProblemKind::CandidateIncompatible),
        "the conflict stays reported for the candidate's owner"
    );
}

/// bd-c04d9b live shape (cacophony PR2208, run 30222268397): every producer job
/// was CANCELLED under runner-capacity pressure, the aggregate required checks
/// went FAILURE in six seconds without a single test or lint step failing, and a
/// five-member caravan could not advance.
///
/// A cancellation is a capacity or supersession event, not a verdict on the
/// code. Excluding it from the infrastructure set meant an operator freeing a
/// busy runner turned a caravan red and needed a human to re-trigger.
#[test]
fn a_cancelled_producer_is_retryable_infrastructure_not_a_code_failure() {
    let diagnostic = |conclusion: &str| crate::ci::WorkflowRunFailureDiagnostic {
        run_id: 30_222_268_397,
        attempt: 1,
        workflow_id: 1,
        check_suite_id: 1,
        workflow_name: "Check & Lint".to_owned(),
        event: "pull_request".to_owned(),
        status: "completed".to_owned(),
        conclusion: conclusion.to_owned(),
        head_branch: "feature".to_owned(),
        head_sha: CommitOid("79abc31d4efc07a579145cf904c83c1420f8b4ac".to_owned()),
        expected_pr: PrNumber(2208),
        expected_head_oid: CommitOid("79abc31d4efc07a579145cf904c83c1420f8b4ac".to_owned()),
        expected_base_oid: CommitOid("base".to_owned()),
        pull_requests: Vec::new(),
        failed_jobs: Vec::new(),
        jobs_total: 0,
        jobs_truncated: false,
    };

    assert!(
        retryable_infrastructure(&diagnostic("cancelled")),
        "a cancelled producer must be recoverable by the bounded rerun path"
    );
    assert!(
        retryable_infrastructure(&diagnostic("canceled")),
        "the American spelling is the same event"
    );
    // A real code failure must NOT be swept into the retryable set: rerunning it
    // would loop forever on work that will never go green by itself.
    assert!(
        !retryable_infrastructure(&diagnostic("failure")),
        "a genuine failure is not infrastructure"
    );
}
