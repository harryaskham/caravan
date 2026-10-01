use super::*;
use crate::pr_close::transaction::{CheckpointStore, ReceiptStore};

struct SaveResponseLost {
    memory: MemoryStore,
    fail_once: Cell<bool>,
}
impl ReceiptStore for SaveResponseLost {
    fn load(&self) -> Result<Option<PrCloseRecord>, AppError> {
        self.memory.load()
    }
    fn save(&self, record: &PrCloseRecord) -> Result<(), AppError> {
        self.memory.save(record)?;
        if record.intent.is_some() && self.fail_once.replace(false) {
            return Err(failure("fixture_intent_save_response_lost"));
        }
        Ok(())
    }
}

#[test]
fn durable_intent_without_an_http_attempt_is_never_replayed() {
    let provider = Provider::new();
    let store = SaveResponseLost {
        memory: MemoryStore::default(),
        fail_once: Cell::new(true),
    };
    assert!(
        execute(
            &provider,
            &store,
            &request(),
            "policy",
            WriterMode::LocalOnly
        )
        .is_err()
    );
    let saved = store.load().unwrap().unwrap();
    assert!(saved.intent.is_some());
    assert!(saved.write_returned_success.is_none());
    assert_eq!(provider.requests.get(), 0);
    assert_eq!(
        execute(
            &provider,
            &store,
            &request(),
            "policy",
            WriterMode::LocalOnly
        )
        .unwrap_err()
        .code(),
        "pr_close_indeterminate"
    );
    assert_eq!(provider.requests.get(), 0);
}

struct UnwritableStore;
impl ReceiptStore for UnwritableStore {
    fn load(&self) -> Result<Option<PrCloseRecord>, AppError> {
        Ok(None)
    }
    fn save(&self, _: &PrCloseRecord) -> Result<(), AppError> {
        Err(failure("fixture_storage_unavailable"))
    }
}

#[test]
fn failed_initial_persistence_never_reaches_the_provider() {
    let provider = Provider::new();
    assert!(
        execute(
            &provider,
            &UnwritableStore,
            &request(),
            "policy",
            WriterMode::LocalOnly
        )
        .is_err()
    );
    assert_eq!(provider.reads.get(), 0);
    assert_eq!(provider.requests.get(), 0);
}

fn git(directory: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn private_atomic_receipt_survives_reload_and_linked_worktree() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--quiet"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    );
    let linked = root.path().join("linked");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );
    let store = CheckpointStore::new(&repo, &request().operation_key);
    let provider = Provider::new();
    let result = execute(
        &provider,
        &store,
        &request(),
        "policy",
        WriterMode::LocalOnly,
    )
    .unwrap();
    assert_eq!(result.outcome, CloseOutcome::Closed);
    let retained = CheckpointStore::new(&linked, &request().operation_key)
        .load()
        .unwrap()
        .unwrap();
    assert_eq!(retained.latest_result.unwrap(), result);
    assert_eq!(
        retained.intent_fence_fingerprint.as_deref(),
        Some("fixture-fence-generation")
    );
    let files = std::fs::read_dir(repo.join(".git/caravan/native-stack"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(files.len(), 1, "no temporary files leaked");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            files[0].metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
