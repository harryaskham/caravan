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
use serde_json::{Value, json};

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
    /// Last durable operation phase/receipt checkpoint written by the owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<OperationLockCheckpoint>,
}

/// Compact durable evidence used to recover safely after client termination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OperationLockCheckpoint {
    pub phase: String,
    pub updated_unix_ms: u64,
    /// Operation-specific compact receipt and exact provider precondition facts.
    pub evidence: Value,
    /// True while a provider write may have completed without an after receipt.
    pub provider_state_indeterminate: bool,
}

/// Read-only lock inspection returned by CLI and MCP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OperationLockStatus {
    pub path: String,
    pub present: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<OperationLockOwner>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_alive: Option<bool>,
    /// A subsequent mutating operation may clean this exact proven-dead owner
    /// immediately, without waiting for the stale-age floor.
    pub auto_recoverable: bool,
    pub stale_after_secs: u64,
    pub stale: bool,
}

/// Receipt from guarded stale-lock recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OperationLockRecovery {
    pub path: String,
    pub removed_owner: OperationLockOwner,
    pub age_secs: u64,
    pub owner_alive: bool,
    pub token_verified: bool,
}

/// Guard for one repository's mutating Caravan operation.
#[derive(Debug)]
pub struct OperationLock {
    path: PathBuf,
    owner: OperationLockOwner,
    file: Option<File>,
    recovered_dead_owner: Option<OperationLockRecovery>,
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
        let mut recovered_dead_owner = None;

        // A process killed by its client cannot run Drop. Prove its owner PID
        // dead and revalidate the exact owner token before removing that fresh
        // orphan; live or ambiguous ownership remains fail-closed.
        for attempt in 0..3 {
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
                        recovered_dead_owner,
                        released: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    match recover_dead_owner_for_acquire(&path)? {
                        DeadOwnerProbe::Recovered(receipt) => {
                            recovered_dead_owner.get_or_insert(receipt);
                        }
                        DeadOwnerProbe::Gone if attempt < 2 => {}
                        DeadOwnerProbe::Gone => {
                            return Err(AppError::structured(
                                ErrorCategory::ExecutionFailure,
                                "operation_lock_race",
                                "the operation lock repeatedly changed during dead-owner inspection",
                                Some(json!({ "path": path })),
                            ));
                        }
                        DeadOwnerProbe::Live => match existing_lock_error(&path, stale_after) {
                            Ok(error) => return Err(error),
                            Err(read_error)
                                if read_error.kind() == io::ErrorKind::NotFound && attempt < 2 => {}
                            Err(read_error) => {
                                return Err(lock_io_error(
                                    "operation_lock_inspection_failed",
                                    "the operation lock exists but its owner evidence could not be read",
                                    &path,
                                    &read_error,
                                ));
                            }
                        },
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

    /// Exact receipt when acquisition cleaned a proven-dead prior owner.
    #[must_use]
    pub fn recovered_dead_owner(&self) -> Option<&OperationLockRecovery> {
        self.recovered_dead_owner.as_ref()
    }

    /// Persist a compact phase/receipt checkpoint under the same exact owner
    /// token. A torn checkpoint remains fail-closed as invalid owner evidence.
    pub fn checkpoint(
        &mut self,
        phase: impl Into<String>,
        evidence: Value,
        provider_state_indeterminate: bool,
    ) -> Result<(), AppError> {
        let existing = read_owner(&self.path).map_err(|error| {
            lock_io_error(
                "operation_lock_checkpoint_inspection_failed",
                "could not verify operation lock ownership before checkpoint",
                &self.path,
                &error,
            )
        })?;
        if existing.token != self.owner.token {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "operation_lock_owner_changed",
                "refusing to checkpoint an operation lock now owned by another process",
                Some(json!({
                    "path": self.path,
                    "expected_owner": self.owner,
                    "actual_owner": existing,
                })),
            ));
        }
        let updated_unix_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        self.owner.checkpoint = Some(OperationLockCheckpoint {
            phase: phase.into(),
            updated_unix_ms,
            evidence,
            provider_state_indeterminate,
        });
        let encoded = serde_json::to_vec(&self.owner).map_err(|error| {
            AppError::structured(
                ErrorCategory::SerializationError,
                "operation_lock_checkpoint_encode_failed",
                format!("could not encode operation lock checkpoint: {error}"),
                None,
            )
        })?;
        if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_OWNER_BYTES {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "operation_lock_checkpoint_too_large",
                "operation lock checkpoint exceeds the bounded owner-file limit",
                Some(json!({
                    "path": self.path,
                    "bytes": encoded.len(),
                    "max_bytes": MAX_OWNER_BYTES,
                })),
            ));
        }
        if self.file.is_none() {
            return Err(AppError::structured(
                ErrorCategory::ExecutionFailure,
                "operation_lock_checkpoint_file_closed",
                "cannot checkpoint a released operation lock",
                Some(json!({ "path": self.path })),
            ));
        }
        self.persist_checkpoint_bytes(&encoded)
    }

    #[cfg(unix)]
    fn persist_checkpoint_bytes(&mut self, encoded: &[u8]) -> Result<(), AppError> {
        let temporary = self.path.with_extension(format!(
            "checkpoint-{}-{}.tmp",
            self.owner.pid,
            TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let write_result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(encoded)?;
            file.sync_all()
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary);
            return Err(lock_io_error(
                "operation_lock_checkpoint_write_failed",
                "could not persist operation lock checkpoint temporary file",
                &temporary,
                &error,
            ));
        }
        let latest = read_owner(&self.path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            lock_io_error(
                "operation_lock_checkpoint_inspection_failed",
                "could not revalidate owner before atomic checkpoint",
                &self.path,
                &error,
            )
        })?;
        if latest.token != self.owner.token {
            let _ = fs::remove_file(&temporary);
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "operation_lock_owner_changed",
                "operation lock owner changed before atomic checkpoint; refusing replacement",
                Some(json!({ "expected_owner": self.owner, "actual_owner": latest })),
            ));
        }
        fs::rename(&temporary, &self.path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            lock_io_error(
                "operation_lock_checkpoint_write_failed",
                "could not atomically replace operation lock checkpoint",
                &self.path,
                &error,
            )
        })?;
        self.file = Some(
            OpenOptions::new()
                .write(true)
                .open(&self.path)
                .map_err(|error| {
                    lock_io_error(
                        "operation_lock_checkpoint_reopen_failed",
                        "could not reopen operation lock after atomic checkpoint",
                        &self.path,
                        &error,
                    )
                })?,
        );
        Ok(())
    }

    #[cfg(not(unix))]
    fn persist_checkpoint_bytes(&mut self, encoded: &[u8]) -> Result<(), AppError> {
        use std::io::{Seek, SeekFrom};

        let file = self.file.as_mut().expect("checked open checkpoint file");
        let write_result = (|| -> io::Result<()> {
            file.seek(SeekFrom::Start(0))?;
            file.set_len(0)?;
            file.write_all(encoded)?;
            file.sync_all()
        })();
        write_result.map_err(|error| {
            lock_io_error(
                "operation_lock_checkpoint_write_failed",
                "could not persist operation lock checkpoint",
                &self.path,
                &error,
            )
        })
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

/// Inspect the repository operation lock and verify whether its owner PID is alive.
pub fn inspect_lock(
    repository: impl AsRef<Path>,
    stale_after: Duration,
) -> Result<OperationLockStatus, AppError> {
    let path = lock_path(repository.as_ref())?;
    let stale_after_secs = stale_after.as_secs();
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(OperationLockStatus {
                path: path.display().to_string(),
                present: false,
                owner: None,
                age_secs: None,
                owner_alive: None,
                auto_recoverable: false,
                stale_after_secs,
                stale: false,
            });
        }
        Err(error) => {
            return Err(lock_io_error(
                "operation_lock_inspection_failed",
                "could not inspect the operation lock",
                &path,
                &error,
            ));
        }
    };
    let owner = read_owner(&path).map_err(|error| {
        lock_io_error(
            "operation_lock_owner_invalid",
            "could not read operation lock owner evidence",
            &path,
            &error,
        )
    })?;
    let age_secs = lock_age_secs(&metadata, &owner);
    let owner_alive = process_is_alive(owner.pid)?;
    Ok(OperationLockStatus {
        path: path.display().to_string(),
        present: true,
        owner: Some(owner),
        age_secs: Some(age_secs),
        owner_alive: Some(owner_alive),
        auto_recoverable: !owner_alive,
        stale_after_secs,
        stale: age_secs >= stale_after_secs,
    })
}

/// Remove one operation lock only after age, dead-owner, and exact-token checks.
pub fn recover_stale_lock(
    repository: impl AsRef<Path>,
    stale_after: Duration,
    expected_token: &str,
) -> Result<OperationLockRecovery, AppError> {
    if expected_token.trim().is_empty() {
        return Err(AppError::validation(
            "operation_lock_token_required",
            "recovery requires the exact token returned by `cara lock status`",
        ));
    }
    let repository = repository.as_ref();
    let status = inspect_lock(repository, stale_after)?;
    if !status.present {
        return Err(AppError::structured(
            ErrorCategory::TargetNotFound,
            "operation_lock_not_found",
            "there is no Caravan operation lock to recover",
            Some(json!({ "path": status.path })),
        ));
    }
    let owner = status.owner.expect("present lock has owner evidence");
    let age_secs = status.age_secs.expect("present lock has age");
    if owner.token != expected_token {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "operation_lock_token_mismatch",
            "the supplied recovery token does not match the current lock owner",
            Some(json!({ "expected_token": expected_token, "actual_owner": owner })),
        ));
    }
    if !status.stale {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "operation_lock_not_stale",
            "the operation lock has not reached the required recovery age",
            Some(json!({
                "age_secs": age_secs,
                "stale_after_secs": stale_after.as_secs(),
                "owner": owner,
            })),
        ));
    }
    if status.owner_alive == Some(true) {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "operation_lock_owner_alive",
            "refusing to recover an operation lock whose recorded PID is alive",
            Some(json!({ "owner": owner })),
        ));
    }

    // Re-read immediately before unlinking. A changed owner/token means another
    // recovery or operation won the race; fail closed and delete nothing.
    let path = lock_path(repository)?;
    let latest = read_owner(&path).map_err(|error| {
        lock_io_error(
            "operation_lock_recovery_inspection_failed",
            "could not re-read owner evidence before recovery",
            &path,
            &error,
        )
    })?;
    if latest != owner || latest.token != expected_token {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "operation_lock_owner_changed",
            "operation lock ownership changed during recovery; refusing removal",
            Some(json!({ "inspected_owner": owner, "latest_owner": latest })),
        ));
    }
    if process_is_alive(latest.pid)? {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "operation_lock_owner_alive",
            "the recorded PID became live during recovery; refusing removal",
            Some(json!({ "owner": latest })),
        ));
    }
    fs::remove_file(&path).map_err(|error| {
        lock_io_error(
            "operation_lock_recovery_failed",
            "could not remove the verified-stale operation lock",
            &path,
            &error,
        )
    })?;
    Ok(OperationLockRecovery {
        path: path.display().to_string(),
        removed_owner: latest,
        age_secs,
        owner_alive: false,
        token_verified: true,
    })
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
        ..
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
        checkpoint: None,
    }
}

fn lock_age_secs(metadata: &fs::Metadata, owner: &OperationLockOwner) -> u64 {
    let wall_age = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|now| now.as_secs().saturating_sub(owner.created_unix_secs));
    wall_age.unwrap_or_else(|| {
        metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .unwrap_or_default()
            .as_secs()
    })
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> Result<bool, AppError> {
    let raw_pid = i32::try_from(pid).map_err(|_| {
        AppError::structured(
            ErrorCategory::Validation,
            "operation_lock_pid_invalid",
            "the operation lock owner PID is outside the platform range",
            Some(json!({ "pid": pid })),
        )
    })?;
    let pid = rustix::process::Pid::from_raw(raw_pid).ok_or_else(|| {
        AppError::structured(
            ErrorCategory::Validation,
            "operation_lock_pid_invalid",
            "the operation lock owner PID must be positive",
            Some(json!({ "pid": pid })),
        )
    })?;
    // POSIX kill(pid, 0) returns EPERM when the process exists but the
    // caller lacks permission. Existence is all recovery needs to refuse.
    match rustix::process::test_kill_process(pid) {
        Ok(()) | Err(rustix::io::Errno::PERM) => Ok(true),
        Err(rustix::io::Errno::SRCH) => Ok(false),
        Err(error) => Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "operation_lock_process_probe_failed",
            format!("could not determine whether operation lock owner PID exists: {error}"),
            Some(json!({ "pid": pid.as_raw_nonzero().get() })),
        )),
    }
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> Result<bool, AppError> {
    let filter = format!("PID eq {pid}");
    let request = CommandSpec::new("tasklist").args(["/FI", &filter, "/FO", "CSV", "/NH"]);
    let output = ProcessRunner::new()
        .with_timeout(Duration::from_secs(5))
        .run(&request)
        .map_err(|error| lock_command_error(&error, Path::new(".")))?;
    if !output.is_success() {
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "operation_lock_process_probe_failed",
            "could not determine whether the operation lock owner PID is alive",
            Some(json!({ "pid": pid, "stderr": bounded_text(&output.stderr) })),
        ));
    }
    Ok(output.stdout.contains(&pid.to_string()))
}

enum DeadOwnerProbe {
    Gone,
    Live,
    Recovered(OperationLockRecovery),
}

fn recover_dead_owner_for_acquire(path: &Path) -> Result<DeadOwnerProbe, AppError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(DeadOwnerProbe::Gone),
        Err(error) => {
            return Err(lock_io_error(
                "operation_lock_inspection_failed",
                "could not inspect the existing operation lock",
                path,
                &error,
            ));
        }
    };
    let owner = match read_owner(path) {
        Ok(owner) => owner,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(DeadOwnerProbe::Gone),
        Err(error) => {
            return Err(lock_io_error(
                "operation_lock_owner_invalid",
                "could not read existing operation lock owner evidence",
                path,
                &error,
            ));
        }
    };
    if process_is_alive(owner.pid)? {
        return Ok(DeadOwnerProbe::Live);
    }

    // Re-read both bytes and liveness immediately before unlinking. A reused
    // PID, replacement token, or changed owner is ambiguous and must not be
    // removed automatically.
    let latest = match read_owner(path) {
        Ok(owner) => owner,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(DeadOwnerProbe::Gone),
        Err(error) => {
            return Err(lock_io_error(
                "operation_lock_recovery_inspection_failed",
                "could not re-read dead owner evidence before cleanup",
                path,
                &error,
            ));
        }
    };
    if latest != owner || latest.token != owner.token {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "operation_lock_owner_changed",
            "operation lock ownership changed during dead-owner cleanup; refusing removal",
            Some(json!({ "inspected_owner": owner, "latest_owner": latest })),
        ));
    }
    if process_is_alive(latest.pid)? {
        return Ok(DeadOwnerProbe::Live);
    }
    fs::remove_file(path).map_err(|error| {
        lock_io_error(
            "operation_lock_recovery_failed",
            "could not remove a proven-dead operation lock",
            path,
            &error,
        )
    })?;
    let age_secs = lock_age_secs(&metadata, &latest);
    Ok(DeadOwnerProbe::Recovered(OperationLockRecovery {
        path: path.display().to_string(),
        removed_owner: latest,
        age_secs,
        owner_alive: false,
        token_verified: true,
    }))
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
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

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
    fn status_reports_absent_and_live_owner_without_mutation() {
        let repository = test_repository();
        let absent = inspect_lock(repository.path(), DEFAULT_STALE_AFTER).unwrap();
        assert!(!absent.present);

        let lock = OperationLock::acquire(repository.path(), "sync").unwrap();
        let status = inspect_lock(repository.path(), Duration::ZERO).unwrap();
        assert!(status.present);
        assert!(status.stale);
        assert_eq!(status.owner_alive, Some(true));
        assert!(!status.auto_recoverable);
        assert_eq!(status.owner.as_ref().unwrap().token, lock.owner().token);

        let error = recover_stale_lock(repository.path(), Duration::ZERO, &lock.owner().token)
            .expect_err("a live owner must never be reaped");
        assert_eq!(error.code(), "operation_lock_owner_alive");
        assert!(lock.path().exists());
        lock.release().unwrap();
    }

    #[test]
    fn recovery_requires_dead_old_owner_and_exact_token() {
        let repository = test_repository();
        let path = lock_path(repository.path()).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let owner = OperationLockOwner {
            version: 1,
            pid: 999_999,
            operation: "split".to_owned(),
            created_unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .saturating_sub(3_600),
            token: "dead-owner-token".to_owned(),
            checkpoint: None,
        };
        fs::write(&path, serde_json::to_vec(&owner).unwrap()).unwrap();

        let status = inspect_lock(repository.path(), Duration::from_secs(1_800)).unwrap();
        assert!(status.stale);
        assert_eq!(status.owner_alive, Some(false));
        assert!(status.auto_recoverable);
        let wrong = recover_stale_lock(repository.path(), Duration::ZERO, "wrong-token")
            .expect_err("wrong token must fail closed");
        assert_eq!(wrong.code(), "operation_lock_token_mismatch");
        assert!(path.exists());

        let receipt =
            recover_stale_lock(repository.path(), Duration::from_secs(1_800), &owner.token)
                .unwrap();
        assert_eq!(receipt.removed_owner, owner);
        assert!(receipt.token_verified);
        assert!(!receipt.owner_alive);
        assert!(!path.exists());
    }

    #[test]
    fn checkpoint_persists_phase_and_provider_indeterminacy_under_owner_token() {
        let repository = test_repository();
        let mut lock = OperationLock::acquire(repository.path(), "sync").unwrap();

        lock.checkpoint(
            "provider_write_in_flight",
            json!({ "operation_id": "op-1", "pr": 2008 }),
            true,
        )
        .unwrap();

        let persisted = read_owner(lock.path()).unwrap();
        assert_eq!(persisted.token, lock.owner().token);
        let checkpoint = persisted.checkpoint.unwrap();
        assert_eq!(checkpoint.phase, "provider_write_in_flight");
        assert_eq!(checkpoint.evidence["pr"], 2008);
        assert!(checkpoint.provider_state_indeterminate);
        lock.release().unwrap();
    }

    #[test]
    fn acquisition_recovers_a_fresh_proven_dead_owner_with_exact_receipt() {
        let repository = test_repository();
        let path = lock_path(repository.path()).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let dead_owner = OperationLockOwner {
            version: 1,
            pid: 999_999,
            operation: "sync_decision_checkout".to_owned(),
            created_unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            token: "fresh-dead-owner-token".to_owned(),
            checkpoint: Some(OperationLockCheckpoint {
                phase: "provider_write_in_flight".to_owned(),
                updated_unix_ms: 1,
                evidence: json!({ "pr": 2008 }),
                provider_state_indeterminate: true,
            }),
        };
        fs::write(&path, serde_json::to_vec(&dead_owner).unwrap()).unwrap();
        let status = inspect_lock(repository.path(), DEFAULT_STALE_AFTER).unwrap();
        assert!(!status.stale);
        assert_eq!(status.owner_alive, Some(false));
        assert!(status.auto_recoverable);

        let lock = OperationLock::acquire(repository.path(), "sync")
            .expect("a proven-dead owner must not block until the stale floor");

        let recovery = lock
            .recovered_dead_owner()
            .expect("dead-owner recovery receipt");
        assert_eq!(recovery.removed_owner, dead_owner);
        assert!(recovery.token_verified);
        assert!(!recovery.owner_alive);
        assert_eq!(read_owner(lock.path()).unwrap(), *lock.owner());
        lock.release().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn sigterm_during_decision_checkout_preserves_checkpoint_and_recovers() {
        let repository = test_repository();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "operation_lock::tests::lock_child_fixture",
                "--nocapture",
            ])
            .env("CARA_LOCK_CHILD_REPOSITORY", repository.path())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn exact lock-owner test process");
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            assert!(
                reader.read_line(&mut line).unwrap() > 0,
                "child exited before lock"
            );
            if line.contains("LOCK_READY") {
                break;
            }
        }
        let path = lock_path(repository.path()).unwrap();
        let owner = read_owner(&path).unwrap();
        assert_eq!(owner.operation, "sync_decision_checkout");
        assert_eq!(
            owner.checkpoint.as_ref().unwrap().phase,
            "decision_checkout_in_flight"
        );

        let status = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .expect("send SIGTERM");
        assert!(status.success());
        let exit = child.wait().unwrap();
        assert!(!exit.success());
        let stranded = inspect_lock(repository.path(), DEFAULT_STALE_AFTER).unwrap();
        assert!(!stranded.stale);
        assert_eq!(stranded.owner_alive, Some(false));
        assert!(stranded.auto_recoverable);

        let lock = OperationLock::acquire(repository.path(), "sync").unwrap();
        let recovery = lock.recovered_dead_owner().unwrap();
        assert_eq!(recovery.removed_owner.token, owner.token);
        assert_eq!(
            recovery.removed_owner.checkpoint.as_ref().unwrap().phase,
            "decision_checkout_in_flight"
        );
        lock.release().unwrap();
    }

    #[test]
    fn lock_child_fixture() {
        let Ok(repository) = std::env::var("CARA_LOCK_CHILD_REPOSITORY") else {
            return;
        };
        let mut lock = OperationLock::acquire(repository, "sync_decision_checkout").unwrap();
        lock.checkpoint(
            "decision_checkout_in_flight",
            json!({ "pr": 2008, "head_oid": "fixture" }),
            false,
        )
        .unwrap();
        println!("LOCK_READY");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_secs(30));
        lock.release().unwrap();
    }

    #[test]
    fn terminated_client_owner_is_recovered_below_the_stale_floor() {
        let repository = test_repository();
        let path = lock_path(repository.path()).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut child = Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("spawn lock owner fixture");
        let dead_owner = OperationLockOwner {
            version: 1,
            pid: child.id(),
            operation: "sync_decision_checkout".to_owned(),
            created_unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            token: "terminated-client-token".to_owned(),
            checkpoint: None,
        };
        fs::write(&path, serde_json::to_vec(&dead_owner).unwrap()).unwrap();
        child.kill().expect("terminate fixture owner");
        child.wait().expect("reap fixture owner");

        let lock =
            OperationLock::acquire_with_stale_after(repository.path(), "sync", DEFAULT_STALE_AFTER)
                .expect("terminated owner is cleaned without waiting thirty minutes");

        assert_eq!(
            lock.recovered_dead_owner().unwrap().removed_owner,
            dead_owner
        );
        lock.release().unwrap();
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
