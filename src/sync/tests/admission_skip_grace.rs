//! Clock-only changes must not turn a settled terminal failure into a new admission.
use super::*;

const PR: PrNumber = PrNumber(9);
const PUBLISHED: &str = "2026-01-01T00:00:00Z";

struct Fixture {
    _directory: tempfile::TempDir,
    context: AppContext,
    provider: FakeProvider,
    default_branch: BranchSnapshot,
    published: u64,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
        let mut candidate = pull_request(
            PR.0,
            "candidate",
            "main",
            PullRequestState::Open,
            AutoMergeState::disabled(),
        );
        candidate.labels.clear();
        candidate.updated_at = Some(PUBLISHED.to_owned());
        candidate.checks = vec![check("required", CheckState::Failure, Some(10))];
        let provider = FakeProvider::with_pull_requests(vec![candidate]);
        provider.require_contexts("main", &["required"]);
        *provider.mutation_updated_at.borrow_mut() = Some("2026-01-01T00:00:20Z".to_owned());
        let mut context = AppContext {
            repository_path: directory.path().to_path_buf(),
            ..AppContext::default()
        };
        context.config.sync.actions.join_unlabelled_prs = true;
        // Enough for a skip or invalidation, never an unrelated membership write.
        context.config.sync.max_mutations_per_tick = 2;
        Self {
            _directory: directory,
            context,
            provider,
            default_branch: branch("main"),
            published: required_runs::rfc3339_to_unix_secs(PUBLISHED).unwrap(),
        }
    }

    fn snapshot(&self) -> StatusOutput {
        let mut snapshot = status(
            self.provider.pulls.borrow().values().cloned().collect(),
            Some(PR),
            &clean,
        );
        snapshot.analysis.fleet.default_branch = self.default_branch.clone();
        snapshot
    }

    fn progress(&self, elapsed: u64) -> SyncProgress {
        let mut progress = SyncProgress::new(&self.snapshot(), Vec::new(), u32::MAX);
        progress.required_runs_now_unix = Some(self.published + elapsed);
        progress.required_runs_grace_secs =
            self.context.config.sync.missing_required_runs_grace_secs;
        progress
    }

    fn tick(
        &self,
        elapsed: u64,
        candidate_limit: u32,
    ) -> Result<(SyncProgress, AutoAdmissionOutput), AppError> {
        self.tick_from(self.snapshot(), elapsed, candidate_limit)
    }

    fn tick_from(
        &self,
        observed: StatusOutput,
        elapsed: u64,
        candidate_limit: u32,
    ) -> Result<(SyncProgress, AutoAdmissionOutput), AppError> {
        let mut progress = SyncProgress::new(&observed, Vec::new(), u32::MAX);
        progress.required_runs_now_unix = Some(self.published + elapsed);
        progress.required_runs_grace_secs =
            self.context.config.sync.missing_required_runs_grace_secs;
        let guard = self
            .context
            .acquire_writer_operation("skip-grace-regression")?;
        let (_, output) = run_auto_admission_with_refresh_limit(
            &self.context,
            observed,
            &self.provider,
            &mut progress,
            Instant::now() + Duration::from_secs(60),
            &crate::command::GithubRequestBudget::new(100),
            &guard,
            candidate_limit,
            |_| Ok(self.snapshot()),
        )?;
        Ok((progress, output))
    }

    fn first_skip(&self, elapsed: u64) -> AutoJoinSkipReceipt {
        // Discovery selected a pending candidate; the fresh provider read now
        // sees its failure. An already-red discovery is excluded before admission.
        let mut discovered = self.provider.pulls.borrow()[&PR].clone();
        discovered.checks = vec![check("required", CheckState::Queued, Some(10))];
        let observed = status(vec![discovered], Some(PR), &clean);
        assert_eq!(observed.admission.next_candidate, Some(PR));
        let (_, first) = self.tick_from(observed, elapsed, 1).unwrap();
        assert_eq!(first.skips.len(), 1, "{first:?}");
        assert_eq!(
            first.skips[0].refusal_kind,
            AutoAdmissionRefusalKind::TerminalCi
        );
        assert_eq!(
            first.skips[0].required_runs.as_ref().unwrap().status,
            RequiredRunsStatus::Failing
        );
        assert_eq!(first.mutations_used, 2);
        assert_eq!(
            *self.provider.calls.borrow(),
            [MutationKind::Comment, MutationKind::AddLabel]
        );
        assert!(self.provider.pulls.borrow()[&PR].has_label(AUTO_ADMISSION_SKIP_LABEL));
        assert_eq!(self.provider.comments.borrow()[&PR].len(), 1);
        first.skips.into_iter().next().unwrap()
    }
}

#[test]
fn terminal_skip_survives_both_grace_crossings_after_own_metadata_writes() {
    // First: normal 20-minute reconciliation after a young-head refusal.
    // Second: our own writes advance updated_at after an already old-head refusal.
    for (first_age, metadata_time, expected_second_age) in [
        (30, "2026-01-01T00:00:20Z", 1_210),
        (1_200, "2026-01-01T00:20:00Z", 30),
    ] {
        let fixture = Fixture::new();
        *fixture.provider.mutation_updated_at.borrow_mut() = Some(metadata_time.to_owned());
        let receipt = fixture.first_skip(first_age);
        let original_comments = fixture.provider.comments.borrow().clone();
        assert_eq!(
            fixture.provider.pulls.borrow()[&PR].updated_at.as_deref(),
            Some(metadata_time)
        );
        let before = receipt.required_runs.as_ref().unwrap();
        assert_eq!(before.head_age_secs, Some(first_age));

        let (mut progress, second) = fixture.tick(1_230, 1).unwrap();
        let after = progress
            .observe_required_runs(&fixture.provider, &repository(), PR)
            .unwrap();
        assert_eq!(after.head_age_secs, Some(expected_second_age));
        assert_ne!(
            before.grace_elapsed, after.grace_elapsed,
            "the regression must cross the boundary"
        );
        assert_eq!(
            second.skips.as_slice(),
            std::slice::from_ref(&receipt),
            "reuse the exact persisted receipt, not a new observation/hash"
        );
        assert_eq!(second.candidates_considered, 0);
        assert_eq!(second.mutations_used, 0);
        assert_eq!(completed_mutation_count(&progress), 0);
        assert!(progress.events.is_empty());
        assert_eq!(
            *fixture.provider.calls.borrow(),
            [MutationKind::Comment, MutationKind::AddLabel]
        );
        assert_eq!(*fixture.provider.comments.borrow(), original_comments);
        assert!(fixture.provider.pulls.borrow()[&PR].has_label(AUTO_ADMISSION_SKIP_LABEL));
        assert!(fixture.provider.rerequests.borrow().is_empty());
        assert!(fixture.provider.workflow_reruns.borrow().is_empty());

        let (_, third) = fixture.tick(2_430, 1).unwrap();
        assert_eq!(third.skips, [receipt]);
        assert_eq!(third.mutations_used, 0);
        assert_eq!(fixture.provider.calls.borrow().len(), 2);
    }
}

#[test]
fn terminal_skip_still_invalidates_real_candidate_and_policy_generations() {
    for change in [
        "head",
        "base",
        "check_state",
        "check_run",
        "default",
        "tail",
        "config",
        "policy",
    ] {
        let mut fixture = Fixture::new();
        let receipt = fixture.first_skip(30);
        let mut candidate = fixture.provider.pulls.borrow()[&PR].clone();
        match change {
            "head" => candidate.head.oid = CommitOid("e".repeat(40)),
            "base" => candidate.base.oid = CommitOid("e".repeat(40)),
            "check_state" => {
                candidate.checks = vec![check("required", CheckState::Queued, Some(11))];
            }
            "check_run" => {
                candidate.checks = vec![check("required", CheckState::Failure, Some(11))];
            }
            "default" => fixture.default_branch.oid = CommitOid("e".repeat(40)),
            "tail" => {
                let mut tail = caravan_member(1, "tail", "main");
                tail.auto_merge = AutoMergeState::squash();
                fixture
                    .provider
                    .pulls
                    .borrow_mut()
                    .insert(tail.number, tail);
            }
            "config" => fixture.context.config.sync.max_candidates_per_tick += 1,
            "policy" => fixture
                .provider
                .require_contexts("main", &["required", "additional"]),
            _ => unreachable!(),
        }
        fixture.provider.pulls.borrow_mut().insert(PR, candidate);
        // Run the real reconciliation phase, stopping before a fresh admission.
        let (_, second) = fixture.tick(1_230, 0).unwrap();
        assert_eq!(second.mutations_used, 2, "{change}");
        assert_eq!(
            &fixture.provider.calls.borrow()[2..],
            [MutationKind::Comment, MutationKind::RemoveLabel],
            "{change}"
        );
        assert!(
            !fixture.provider.pulls.borrow()[&PR].has_label(AUTO_ADMISSION_SKIP_LABEL),
            "{change}"
        );
        assert!(
            fixture.provider.comments.borrow()[&PR].contains(&receipt.comment_body()),
            "retain original evidence: {change}"
        );
        assert!(second.skips.is_empty(), "{change}");
    }
}

#[test]
fn terminal_skip_keeps_incomplete_provider_evidence_fail_closed() {
    let fixture = Fixture::new();
    let receipt = fixture.first_skip(30);
    fixture.provider.partial_contexts("main");
    let error = fixture.tick(1_230, 1).unwrap_err();
    assert_eq!(error.code(), "auto_admission_provider_state_unknown");
    assert_eq!(fixture.provider.calls.borrow().len(), 2);
    assert!(fixture.provider.pulls.borrow()[&PR].has_label(AUTO_ADMISSION_SKIP_LABEL));
    assert_eq!(
        fixture.provider.comments.borrow()[&PR],
        [receipt.comment_body()]
    );
}

#[test]
fn missing_run_grace_still_changes_admission_for_old_commits_newly_published() {
    let fixture = Fixture::new();
    fixture
        .provider
        .pulls
        .borrow_mut()
        .get_mut(&PR)
        .unwrap()
        .checks
        .clear();
    let mut early = fixture.progress(120);
    assert!(
        candidate_local_admission_refusal(&fixture.provider, &mut early, &repository(), PR, None)
            .unwrap()
            .is_none()
    );
    let waiting = early
        .observe_required_runs(&fixture.provider, &repository(), PR)
        .unwrap();
    assert_eq!(waiting.status, RequiredRunsStatus::AwaitingGrace);
    assert_eq!(
        waiting.head_age_secs,
        Some(120),
        "PR publication wins over the preserved 2020 commit time"
    );
    assert_eq!(waiting.recovery, RequiredRunsRecovery::AwaitGrace);

    let mut late = fixture.progress(600);
    let refusal =
        candidate_local_admission_refusal(&fixture.provider, &mut late, &repository(), PR, None)
            .unwrap()
            .unwrap();
    assert_eq!(refusal.kind, AutoAdmissionRefusalKind::RequiredRuns);
    let missing = refusal.required_runs.unwrap();
    assert_eq!(missing.status, RequiredRunsStatus::MissingRequiredRuns);
    assert!(missing.grace_elapsed);
    assert!(!required_runs_generation_matches(&waiting, &missing));
    assert!(fixture.provider.calls.borrow().is_empty());
}
