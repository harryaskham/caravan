//! Repository-scoped exclusive operation lock.
//!
//! Mutating Caravan operations acquire this lock explicitly. Read-only callers
//! do not need it and can continue concurrently. The lock is a small owner file
//! below Git's common metadata directory, so linked worktrees for one repository
//! share the same exclusion boundary.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppError;
use crate::command::{CommandRunError, CommandRunner, CommandSpec, ProcessRunner};

/// Default age at which an abandoned owner file is reported as stale.
pub const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(30 * 60);
const LOCK_DIRECTORY: &str = "caravan";
const LOCK_FILE: &str = "operation.lock";
const MAX_OWNER_BYTES: u64 = 16 * 1024;
static TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Persisted, non-secret owner evidence for a local operation lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OperationLockOwner {
    /// Metadata schema version.
    pub version: u32,
    /// Process that created the owner file.
    pub pid: u32,
    /// Operation name supplied by the caller.
    pub operation: String,
    /// Creation time as seconds since the Unix epoch.
    pub created_unix_secs: u64,
    /// Unique token used to avoid deleting a replacement owner's lock.
    pub token: String,
}

/// Guard for one repository's mutating Caravan operation.
#[derive(Debug)]
pub struct OperationLock {
    path: PathBuf,
    owner: OperationLockOwner,
    file: Option<File>,
    released: bool,
}

impl OperationLock {
    /// Acquire the default repository-scoped operation lock.
    pub fn acquire(repository: impl AsRef<Path>, operation: &str) -> Result<Self, AppError> {
        Self::acquire_with_stale_after(repository, operation, DEFAULT_STALE_AFTER)
    }

    /// Acquire a lock with an explicit stale-reporting threshold.
    ///
    /// A stale owner file is not removed automatically: callers must first
    /// verify that its process is no longer active, then use an operator-owned
    /// recovery path. This prevents an age-only guess from breaking exclusion.
    pub fn acquire_with_stale_after(
        repository: impl AsRef<Path>,
        operation: &str,
        stale_after: Duration,
    ) -> Result<Self, AppError> {
        if operation.trim().is_empty() || operation.contains(['\n', '\r']) {
            return Err(AppError::validation(
                "invalid_operation_name",
                "a lock operation name must be non-empty and single-line",
            ));
        }

        let path = lock_path(repository.as_ref())?;
        let parent = path.parent().expect("lock path always has a parent");
        fs::create_dir_all(parent).map_err(|error| {
            lock_io_error(
                "operation_lock_directory_failed",
                "could not create Caravan's Git metadata directory",
                &path,
                &error,
            )
        })?;
        let owner = new_owner(operation);

        // If a prior owner drops between create_new and inspection, retry once.
        for attempt in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let encoded = serde_json::to_vec(&owner).map_err(|error| {
                        AppError::structured(
                            ErrorCategory::SerializationError,
                            "operation_lock_encode_failed",
                            format!("could not encode operation lock owner: {error}"),
                            None,
                        )
                    })?;
                    if let Err(error) = file.write_all(&encoded).and_then(|()| file.sync_all()) {
                        drop(file);
                        let _ = fs::remove_file(&path);
                        return Err(lock_io_error(
                            "operation_lock_write_failed",
                            "could not persist operation lock owner evidence",
                            &path,
                            &error,
                        ));
                    }
                    return Ok(Self {
                        path,
                        owner,
                        file: Some(file),
                        released: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    match existing_lock_error(&path, stale_after) {
                        Ok(error) => return Err(error),
                        Err(read_error)
                            if read_error.kind() == io::ErrorKind::NotFound && attempt == 0 => {}
                        Err(read_error) => {
                            return Err(lock_io_error(
                                "operation_lock_inspection_failed",
                                "the operation lock exists but its owner evidence could not be read",
                                &path,
                                &read_error,
                            ));
                        }
                    }
                }
                Err(error) => {
                    return Err(lock_io_error(
                        "operation_lock_create_failed",
                        "could not create the repository operation lock",
                        &path,
                        &error,
                    ));
                }
            }
        }

        Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "operation_lock_race",
            "the operation lock repeatedly changed while it was being acquired",
            Some(json!({ "path": path })),
        ))
    }

    /// Persisted owner evidence for this guard.
    #[must_use]
    pub fn owner(&self) -> &OperationLockOwner {
        &self.owner
    }

    /// Path below Git's common metadata directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Release the lock and report an ownership mismatch instead of deleting a successor.
    pub fn release(mut self) -> Result<(), AppError> {
        self.release_inner()
    }

    fn release_inner(&mut self) -> Result<(), AppError> {
        if self.released {
            return Ok(());
        }
        let existing = read_owner(&self.path).map_err(|error| {
            lock_io_error(
                "operation_lock_release_inspection_failed",
                "could not verify operation lock ownership before release",
                &self.path,
                &error,
            )
        })?;
        if existing.token != self.owner.token {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "operation_lock_owner_changed",
                "refusing to remove an operation lock now owned by another process",
                Some(json!({
                    "path": self.path,
                    "expected_owner": self.owner,
                    "actual_owner": existing,
                })),
            ));
        }

        // Close before unlinking so this also works on platforms that deny
        // deleting an open file.
        drop(self.file.take());
        fs::remove_file(&self.path).map_err(|error| {
            lock_io_error(
                "operation_lock_release_failed",
                "could not remove the repository operation lock",
                &self.path,
                &error,
            )
        })?;
        self.released = true;
        Ok(())
    }
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let owned = read_owner(&self.path).is_ok_and(|owner| owner.token == self.owner.token);
        drop(self.file.take());
        if owned {
            let _ = fs::remove_file(&self.path);
        }
        self.released = true;
    }
}

fn lock_path(repository: &Path) -> Result<PathBuf, AppError> {
    let request =
        CommandSpec::new("git").args(["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    let output = ProcessRunner::in_directory(repository)
        .run(&request)
        .map_err(|error| lock_command_error(&error, repository))?;
    if !output.is_success() {
        return Err(AppError::structured(
            ErrorCategory::TargetNotFound,
            "git_repository_not_found",
            "the operation lock requires a Git repository",
            Some(json!({
                "repository": repository,
                "stderr": bounded_text(&output.stderr),
            })),
        ));
    }
    let common_dir = output.stdout.trim().to_owned();
    if common_dir.is_empty() {
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "git_common_dir_missing",
            "Git did not return its common metadata directory",
            Some(json!({ "repository": repository })),
        ));
    }
    Ok(PathBuf::from(common_dir)
        .join(LOCK_DIRECTORY)
        .join(LOCK_FILE))
}

fn lock_command_error(error: &CommandRunError, repository: &Path) -> AppError {
    if let CommandRunError::Timeout {
        command,
        timeout_ms,
        stdout,
        stderr,
    } = error
    {
        return AppError::structured(
            ErrorCategory::Timeout,
            "operation_lock_command_timeout",
            error.to_string(),
            Some(json!({
                "stage": "operation_lock",
                "command": command.display(),
                "timeout_ms": timeout_ms,
                "stdout": stdout,
                "stderr": stderr,
                "repository": repository,
            })),
        );
    }
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "git_spawn_failed",
        format!("could not execute Git while locating lock metadata: {error}"),
        Some(json!({ "repository": repository })),
    )
}

fn new_owner(operation: &str) -> OperationLockOwner {
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let sequence = TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    OperationLockOwner {
        version: 1,
        pid: std::process::id(),
        operation: operation.to_owned(),
        created_unix_secs: created.as_secs(),
        token: format!("{}-{}-{sequence}", std::process::id(), created.as_nanos()),
    }
}

fn existing_lock_error(path: &Path, stale_after: Duration) -> Result<AppError, io::Error> {
    let metadata = fs::metadata(path)?;
    let age = metadata
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .unwrap_or_default();
    let owner = read_owner(path).ok();
    let owner_value = owner
        .as_ref()
        .and_then(|value| serde_json::to_value(value).ok());

    if age >= stale_after {
        return Ok(AppError::structured(
            ErrorCategory::Validation,
            "stale_operation_lock",
            "a repository operation lock is older than the configured stale threshold",
            Some(json!({
                "path": path,
                "age_secs": age.as_secs(),
                "stale_after_secs": stale_after.as_secs(),
                "owner": owner_value,
                "recovery": "verify that the recorded process is no longer active before removing the owner file",
            })),
        ));
    }

    Ok(AppError::structured(
        ErrorCategory::Validation,
        "operation_lock_contended",
        "another mutating Caravan operation owns this repository",
        Some(json!({
            "path": path,
            "age_secs": age.as_secs(),
            "stale_after_secs": stale_after.as_secs(),
            "owner": owner_value,
            "resumable": true,
            "next": "retry after the current mutating operation releases the lock",
        })),
    ))
}

fn read_owner(path: &Path) -> Result<OperationLockOwner, io::Error> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_OWNER_BYTES).read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn lock_io_error(code: &str, message: &str, path: &Path, error: &io::Error) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        code,
        format!("{message}: {error}"),
        Some(json!({ "path": path })),
    )
}

fn bounded_text(text: &str) -> String {
    const MAX_BYTES: usize = 4_096;
    if text.len() <= MAX_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &text[..end])
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::process::Command;

    use mcp_cli::StructuredError;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn operation_lock_is_shared_under_git_metadata_and_reports_contention() {
        let repository = test_repository();
        let first = OperationLock::acquire(repository.path(), "sync").expect("first lock");
        assert!(first.path().ends_with(".git/caravan/operation.lock"));
        assert!(first.path().is_file());
        assert_eq!(first.owner().operation, "sync");

        let error = OperationLock::acquire(repository.path(), "join")
            .expect_err("second mutating operation must be refused");
        assert_eq!(error.code(), "operation_lock_contended");
        assert_eq!(
            error.details().expect("contention details")["owner"]["operation"],
            "sync"
        );

        first.release().expect("release first lock");
        let second = OperationLock::acquire(repository.path(), "join")
            .expect("lock is reusable after release");
        assert_eq!(second.owner().operation, "join");
    }

    #[test]
    fn old_owner_file_is_reported_as_stale_without_being_removed() {
        let repository = test_repository();
        let first = OperationLock::acquire(repository.path(), "sync").expect("first lock");
        let path = first.path().to_owned();

        let error =
            OperationLock::acquire_with_stale_after(repository.path(), "join", Duration::ZERO)
                .expect_err("zero threshold classifies the existing owner as stale");

        assert_eq!(error.code(), "stale_operation_lock");
        assert!(path.exists(), "stale classification must not reap the lock");
        first.release().expect("owner still releases its own lock");
    }

    #[test]
    fn linked_worktree_uses_the_common_repository_lock() {
        let repository = test_repository();
        let linked = tempfile::tempdir().expect("linked worktree parent");
        let linked_path = linked.path().join("worktree");
        git(
            repository.path(),
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--detach"),
                linked_path.as_os_str(),
            ],
        );
        let first = OperationLock::acquire(repository.path(), "sync").expect("main lock");

        let error = OperationLock::acquire(&linked_path, "join")
            .expect_err("linked worktree must share the common lock");
        assert_eq!(error.code(), "operation_lock_contended");
        assert_eq!(
            first.path(),
            lock_path(&linked_path).expect("linked common lock path")
        );
    }

    fn test_repository() -> TempDir {
        let directory = tempfile::tempdir().expect("temp repository");
        git(
            directory.path(),
            [
                OsStr::new("init"),
                OsStr::new("--quiet"),
                OsStr::new("--initial-branch=main"),
            ],
        );
        git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("Caravan Test"),
            ],
        );
        git(
            directory.path(),
            [
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("caravan@example.invalid"),
            ],
        );
        fs::write(directory.path().join("README"), "test\n").expect("fixture file");
        git(directory.path(), [OsStr::new("add"), OsStr::new("README")]);
        git(
            directory.path(),
            [
                OsStr::new("commit"),
                OsStr::new("--quiet"),
                OsStr::new("--message"),
                OsStr::new("initial"),
            ],
        );
        directory
    }

    fn git<I, S>(repository: &Path, arguments: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = Command::new("git")
            .current_dir(repository)
            .args(arguments)
            .output()
            .expect("run git fixture command");
        assert!(
            output.status.success(),
            "git fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
