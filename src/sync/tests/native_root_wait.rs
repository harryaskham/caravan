//! bd-4eccb7: a waiting native root is a terminal, bounded tick outcome.
use super::*;

fn waiting_stack(count: u64, state: CheckState) -> (StatusOutput, FakeProvider) {
    let mut pulls = (1..=count)
        .map(|number| {
            caravan_member(
                number,
                &format!("branch-{number}"),
                &if number == 1 {
                    "main".to_owned()
                } else {
                    format!("branch-{}", number - 1)
                },
            )
        })
        .collect::<Vec<_>>();
    pulls[0].checks = vec![check("build-test", state, None)];
    let mut status = caravan_status(pulls.clone(), Some(PrNumber(1)), true);
    enable_native_backend(&mut status);
    let members = status.analysis.fleet.caravans[0].members.clone();
    let provider = FakeProvider::with_pull_requests(pulls);
    *provider.native_stack.borrow_mut() = Some(native_generation(&status, 42, &members));
    (status, provider)
}

fn prefix(
    status: &StatusOutput,
    provider: &FakeProvider,
    native: NativeSyncContext,
) -> SyncProgress {
    let policy = RequiredRunsPolicy::from_config(&native.config.sync);
    execute_native_stack_prefix_first(
        status,
        provider,
        true,
        false,
        false,
        64,
        policy,
        Some(native),
    )
    .expect("waiting does not fail or open a membership envelope")
    .expect("waiting must survive the prefix pass rather than fall through to repeated analysis")
}

#[test]
fn waiting_root_cost_is_independent_of_suffix_length() {
    for state in [
        CheckState::InProgress,
        CheckState::Cancelled,
        CheckState::Failure,
    ] {
        let mut root_reads = None;
        for count in [2, 5, 25] {
            let (status, provider) = waiting_stack(count, state);
            let before = provider.pulls.borrow().clone();
            let (_directory, _config, native) = github_native_fixture();
            let progress = prefix(&status, &provider, native);
            assert!(!progress.operation_receipt().changed);
            assert!(provider.calls.borrow().is_empty());
            assert!(
                provider
                    .refetches
                    .borrow()
                    .iter()
                    .all(|pr| *pr == PrNumber(1))
            );
            assert!(
                provider
                    .lineage_reads
                    .borrow()
                    .iter()
                    .all(|pr| *pr == PrNumber(1))
            );
            let reads = provider.refetches.borrow().len();
            assert_eq!(*root_reads.get_or_insert(reads), reads);
            assert_eq!(*provider.native_stack_intersection_reads.borrow(), 1);
            assert_eq!(*provider.native_stack_reads.borrow(), 0);
            assert_eq!(progress.ci.len(), 1);
            assert_eq!(progress.required_runs.len(), 1);
            assert_eq!(*provider.pulls.borrow(), before);
            let wait = &progress.native_stack_waits[0];
            assert_eq!(wait.stack_number, 42);
            assert_eq!(wait.blocking_pr, PrNumber(1));
            assert!(
                wait.blockers
                    .contains(&crate::github::GitHubStackMergeBlocker::RequiredChecksNotReady)
            );
            assert_eq!(
                wait.deferred_members,
                (2..=count).map(PrNumber).collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn waiting_tick_preserves_typed_ci_wait_and_defers_admission_without_progress_claim() {
    let (status, provider) = waiting_stack(5, CheckState::InProgress);
    let (directory, config, native) = github_native_fixture();
    let progress = prefix(&status, &provider, native);
    let mut context = AppContext {
        repository_path: directory.path().to_path_buf(),
        config,
        ..AppContext::default()
    };
    context.config.sync.actions.join_unlabelled_prs = true;
    let mut writer = context
        .acquire_writer_operation("native-wait-test")
        .unwrap();
    let started = Instant::now();
    let output = root_first_output(
        &context,
        &SyncInput {
            all: true,
            ..SyncInput::default()
        },
        started,
        started + Duration::from_secs(30),
        Duration::ZERO,
        Duration::ZERO,
        progress,
        status,
        None,
        &mut writer,
    )
    .unwrap();
    assert!(!output.receipt.changed);
    assert!(!output.tick.changed);
    assert_eq!(
        output.scheduler_status.disposition,
        SchedulerDisposition::WaitingCi
    );
    assert_eq!(output.scheduler_status.wake_class, SchedulerWakeClass::None);
    assert_eq!(output.scheduler_status.waiting_prs, vec![PrNumber(1)]);
    assert_eq!(output.scheduler_status.native_stack_waits.len(), 1);
    assert!(!output.scheduler_status.reason.contains("advanced"));
    assert_eq!(
        output.auto_admission.continuation,
        AutoAdmissionContinuation::RequiresConvergedFleet
    );
    assert_eq!(output.auto_admission.candidates_considered, 0);
    assert!(output.auto_admission.joins.is_empty());
    assert!(output.native_stack_land.is_empty());
    assert!(output.rebase_receipts.is_empty());
    assert!(output.provider_receipts.is_empty());
    let encoded = serde_json::to_value(&output.scheduler_status).unwrap();
    assert_eq!(encoded["native_stack_waits"][0]["blocking_pr"], 1);
    // Omitted descendants are explicitly unobserved, never passing by default.
    assert!(
        output.scheduler_status.caravans[0].members[1..]
            .iter()
            .all(|member| member.ci.is_none())
    );
}

#[test]
fn existing_landing_checkpoint_is_not_hidden_by_the_root_wait_fast_path() {
    let (status, provider) = waiting_stack(2, CheckState::InProgress);
    let (directory, _config, native) = github_native_fixture();
    let generation = provider.native_stack.borrow().clone().unwrap();
    let evidence = generation
        .topology
        .entries
        .iter()
        .cloned()
        .map(|generation| crate::github::GitHubStackMergeEntryEvidence {
            generation,
            blockers: Vec::new(),
        })
        .collect::<Vec<_>>();
    let plan = crate::github::plan_github_stack_ready_prefix(&generation, &evidence)
        .unwrap()
        .direct_squash_plan("previous-tick".to_owned(), "operator".to_owned())
        .unwrap();
    let checkpoint =
        GitHubMutationAdapter::<crate::command::ProcessRunner>::native_stack_land_begin(
            &status.repository,
            &plan,
        );
    crate::stack_checkpoint::write(directory.path(), "land-42", &checkpoint).unwrap();
    let progress = prefix(&status, &provider, native);
    assert!(progress.native_stack_waits.is_empty());
    assert_eq!(progress.native_stack_land, vec![checkpoint]);
    assert!(provider.calls.borrow().is_empty());
}

#[test]
fn waiting_native_root_does_not_starve_independent_ready_singleton() {
    let (original, provider) = waiting_stack(2, CheckState::InProgress);
    let ready = caravan_member(3, "independent", "main");
    provider
        .pulls
        .borrow_mut()
        .insert(ready.number, ready.clone());
    let mut pulls = original
        .analysis
        .pull_requests
        .values()
        .cloned()
        .collect::<Vec<_>>();
    pulls.push(ready);
    let mut status = caravan_status(pulls, Some(PrNumber(1)), true);
    enable_native_backend(&mut status);
    let (_directory, _config, native) = github_native_fixture();
    let progress = prefix(&status, &provider, native);
    assert_eq!(progress.native_stack_waits.len(), 1);
    assert_eq!(progress.native_stack_waits[0].blocking_pr, PrNumber(1));
    assert_eq!(progress.root_merge.len(), 1);
    assert_eq!(progress.root_merge[0].pr, PrNumber(3));
    assert_eq!(
        progress.synchronized_caravans,
        vec![PrNumber(1), PrNumber(3)]
    );
    assert_eq!(
        provider.pulls.borrow()[&PrNumber(2)].state,
        PullRequestState::Open
    );
    assert!(!provider.refetches.borrow().contains(&PrNumber(2)));
}
