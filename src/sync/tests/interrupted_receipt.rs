//! Hermetic real-process interruption of a write-ahead native merge receipt.
use super::*;
use crate::command::{CommandRunner, CommandSpec, ProcessRunner};
use crate::github::{GitHubStackLandCheckpoint, GitHubStackLandPhase, GitHubStackMergeStatus};
use crate::sync::receipt::{self, ReceiptInput};

fn submitting(status: &StatusOutput, operation_id: &OperationId) -> GitHubStackLandCheckpoint {
    let generation = native_generation(status, 42, &[PrNumber(1), PrNumber(2)]);
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
        .direct_squash_plan(format!("{operation_id}:stack:42"), "fixture")
        .unwrap();
    let mut checkpoint =
        GitHubMutationAdapter::<ProcessRunner>::native_stack_land_begin(&status.repository, &plan);
    checkpoint.phase = GitHubStackLandPhase::Submitting;
    checkpoint.branch_lock = Some(crate::github::GitHubStackBranchLockGeneration {
        id: 42,
        node_id: "R_fixture".to_owned(),
        name: "fixture-lock".to_owned(),
        repository: status.repository.clone(),
        source: status.repository.slug(),
        source_type: "Repository".to_owned(),
        target: "branch".to_owned(),
        enforcement: "active".to_owned(),
        selected_pull_requests: vec![PrNumber(1), PrNumber(2)],
        selected_refs: vec!["refs/heads/root".to_owned(), "refs/heads/child".to_owned()],
        current_user_can_bypass: "never".to_owned(),
        created_at: String::new(),
        updated_at: String::new(),
    });
    checkpoint.reseal()
}

fn status() -> StatusOutput {
    let mut status = caravan_status(
        vec![
            caravan_member(1, "root", "main"),
            caravan_member(2, "child", "root"),
        ],
        None,
        true,
    );
    enable_native_backend(&mut status);
    status
}

#[test]
fn interrupted_child() {
    let Some(path) = std::env::var_os("CARA_RECEIPT_INTERRUPTION_FIXTURE") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let _lock = crate::operation_lock::OperationLock::acquire(&path, "sync").unwrap();
    let _scope = receipt::Scope::begin(&path).unwrap();
    let id = receipt::operation_id();
    let checkpoint = submitting(&status(), &id);
    receipt::save_native(&path, "land-42", &checkpoint).unwrap();
    // Same seam production runs immediately before spawn; terminate after the
    // write-ahead marker, without unwind, return receipt, or lock cleanup.
    let _attempt = receipt::before_spawn(
        &CommandSpec::new("fixture-provider")
            .args(["PUT", "merge-async"])
            .provider_write(),
    )
    .unwrap();
    std::fs::write(path.join("ready"), id.0).unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn killed_writer_keeps_readable_identity_and_reconciles_without_resubmitting() {
    let (_directory, config, native) = github_native_fixture();
    let path = &native.repository_path;
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "sync::tests::interrupted_receipt::interrupted_child",
            "--nocapture",
        ])
        .env("CARA_RECEIPT_INTERRUPTION_FIXTURE", path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.join("ready").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    // Always reap the fixture, including on readiness failure.
    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    let id =
        std::fs::read_to_string(path.join("ready")).expect("child persisted before interruption");
    // Caco cron parses complete root JSON values from its merged output spool;
    // a prose-only ID would still become operation_id=unknown on restart.
    let startup: serde_json::Value = String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .expect("complete structured startup receipt before provider write");
    assert_eq!(startup["operation_id"], id);
    assert_eq!(startup["phase"], "sync_started");
    let input = ReceiptInput {
        operation_id: Some(id.clone()),
    };
    let read = receipt::inspect(path, &input).unwrap();
    assert_eq!(read.operations[0].state, "unfinished");
    assert_eq!(read.operations[0].writes[0].state, "may_have_run");
    assert_eq!(read.native_merges[0].outcome, "unknown");
    assert_eq!(
        read.native_merges[0].checkpoint.phase,
        GitHubStackLandPhase::Submitting
    );
    assert_eq!(read.native_merges[0].request.method, "PUT");
    assert_eq!(
        read.native_merges[0].request.path,
        format!("repos/{}/pulls/2/merge-async", repository().slug())
    );
    assert_eq!(
        read.native_merges[0].request.ordered_pull_requests,
        vec![PrNumber(1), PrNumber(2)]
    );
    assert_eq!(read.native_merges[0].request.ordered_heads.len(), 2);
    let context = AppContext {
        repository_path: path.clone(),
        config,
        ..AppContext::default()
    };
    let status = status();
    let provider =
        FakeProvider::with_pull_requests(status.analysis.pull_requests.values().cloned().collect());
    // Dead-owner cleanup cannot erase the separate operation receipt.
    let _replacement = crate::operation_lock::OperationLock::acquire(path, "sync").unwrap();
    reconcile_pending_native_stack_landing_checkpoints(&context, &status, &provider).unwrap();
    let unknown = receipt::inspect(path, &input).unwrap();
    assert_eq!(
        unknown.native_merges[0].checkpoint.phase,
        GitHubStackLandPhase::Released
    );
    assert_eq!(unknown.native_merges[0].outcome, "unknown");
    assert!(
        provider.calls.borrow().is_empty(),
        "no merge, audit, or other write is replayed"
    );
    assert_eq!(*provider.released_reobservations.borrow(), 1);
    // Later typed terminal provider results update the same immutable identity.
    // Transport/provenance validation is covered by the adapter tests.
    for (terminal, outcome) in [
        (GitHubStackMergeStatus::Merged, "applied"),
        (GitHubStackMergeStatus::Failed, "not_applied"),
        (GitHubStackMergeStatus::Indeterminate, "unknown"),
    ] {
        receipt::save_native(path, "land-42", &unknown.native_merges[0].checkpoint).unwrap();
        *provider.released_reobserve_status.borrow_mut() = Some(terminal);
        reconcile_pending_native_stack_landing_checkpoints(&context, &status, &provider).unwrap();
        let read = receipt::inspect(path, &input).unwrap();
        assert_eq!(read.operations[0].operation_id.0, id);
        assert_eq!(read.native_merges.len(), 1);
        assert_eq!(read.native_merges[0].outcome, outcome);
        assert_eq!(
            read.native_merges[0].checkpoint.plan,
            unknown.native_merges[0].checkpoint.plan
        );
        assert!(provider.calls.borrow().is_empty());
    }
    // Even retiring the scheduling cursor cannot erase exact evidence.
    crate::stack_checkpoint::remove(path, "land-42").unwrap();
    assert_eq!(
        receipt::inspect(path, &input).unwrap().native_merges.len(),
        1
    );
}

#[test]
fn indeterminate_merge_error_reports_the_durable_identity_and_readable_receipt() {
    let (_directory, _config, native) = github_native_fixture();
    let scope = receipt::Scope::begin(&native.repository_path).unwrap();
    let id = receipt::operation_id();
    let status = status();
    let provider =
        FakeProvider::with_pull_requests(status.analysis.pull_requests.values().cloned().collect());
    let submitting = submitting(&status, &id);
    receipt::save_native(&native.repository_path, "land-42", &submitting).unwrap();
    let _attempt =
        receipt::before_spawn(&CommandSpec::new("fixture-provider").provider_write()).unwrap();
    let terminal =
        GitHubMutationAdapter::<ProcessRunner>::native_stack_land_abandon_uncertain_submission(
            &status.repository,
            &submitting,
        )
        .unwrap();
    let released = provider
        .native_stack_land_release_for_sync(&status.repository, &terminal)
        .unwrap();
    receipt::save_native(&native.repository_path, "land-42", &released).unwrap();
    let mut progress = SyncProgress::new(&status, vec![PrNumber(1)], 64);
    let error = scope
        .finish(progress.drain_native_stack(
            &provider,
            &status,
            &status.analysis.fleet.caravans[0],
            42,
            &native,
        ))
        .unwrap_err();
    assert_eq!(error.code(), "github_stack_merge_indeterminate");
    let details = error.details().unwrap();
    assert_eq!(details["operation_id"], id.0);
    assert_eq!(details["provider_write_may_have_occurred"], true);
    assert!(details["receipt_command"].as_str().unwrap().contains(&id.0));
    let read = receipt::inspect(
        &native.repository_path,
        &ReceiptInput {
            operation_id: Some(id.0),
        },
    )
    .unwrap();
    assert_eq!(read.operations[0].state, "failed");
    assert_eq!(read.native_merges[0].outcome, "unknown");
    assert_eq!(read.native_merges[0].checkpoint, released);
    assert!(provider.calls.borrow().is_empty());
}

#[test]
fn prewrite_failure_and_spawn_failure_are_not_indeterminate() {
    let (_directory, _config, native) = github_native_fixture();
    for spawn in [false, true] {
        let scope = receipt::Scope::begin(&native.repository_path).unwrap();
        let id = receipt::operation_id();
        if spawn {
            let error = ProcessRunner::in_directory(&native.repository_path)
                .run(&CommandSpec::new("/nonexistent/cara-fixture-provider").provider_write())
                .unwrap_err();
            assert!(matches!(
                error,
                crate::command::CommandRunError::Spawn { .. }
            ));
        }
        let error = scope
            .finish::<()>(Err(AppError::validation(
                "fixture_preflight",
                "not submitted",
            )))
            .unwrap_err();
        assert_eq!(error.details().unwrap()["operation_id"], id.0);
        assert_eq!(
            error.details().unwrap()["provider_write_may_have_occurred"],
            false
        );
        let read = receipt::inspect(
            &native.repository_path,
            &ReceiptInput {
                operation_id: Some(id.0),
            },
        )
        .unwrap();
        assert_eq!(read.operations[0].state, "failed_before_provider_write");
    }
}

#[test]
fn process_runner_persists_before_write_and_does_not_store_secrets() {
    let (_directory, _config, native) = github_native_fixture();
    let scope = receipt::Scope::begin(&native.repository_path).unwrap();
    let id = receipt::operation_id();
    let key = format!(".git/caravan/native-stack/sync-{id}.json");
    let output = ProcessRunner::in_directory(&native.repository_path)
        .run(
            &CommandSpec::new("sh")
                .args(["-c", "grep -q may_have_run \"$1\"", "fixture", &key])
                .env("CARA_FIXTURE_SECRET", "never-persist-this-secret")
                .stdin("never-persist-this-input")
                .provider_write(),
        )
        .unwrap();
    assert!(output.is_success());
    scope.finish(Ok(())).unwrap();
    let read = receipt::inspect(
        &native.repository_path,
        &ReceiptInput {
            operation_id: Some(id.0),
        },
    )
    .unwrap();
    assert_eq!(read.operations[0].writes[0].state, "returned");
    assert_eq!(read.operations[0].writes[0].exit_code, Some(0));
    assert!(
        !serde_json::to_string(&read)
            .unwrap()
            .contains("never-persist")
    );
}

#[test]
fn unavailable_receipt_storage_refuses_before_subprocess() {
    let (_directory, _config, native) = github_native_fixture();
    let scope = receipt::Scope::begin(&native.repository_path).unwrap();
    let id = receipt::operation_id();
    let file = native
        .repository_path
        .join(format!(".git/caravan/native-stack/sync-{id}.json"));
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    let result = ProcessRunner::in_directory(&native.repository_path).run(
        &CommandSpec::new("sh")
            .args(["-c", "touch should-not-exist"])
            .provider_write(),
    );
    assert!(matches!(
        result,
        Err(crate::command::CommandRunError::MutationFenceRefused { .. })
    ));
    assert!(!native.repository_path.join("should-not-exist").exists());
    drop(scope);
}
