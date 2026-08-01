//! Single production entry point for repository mutation authority.
//!
//! The first slice preserves the existing local lock exactly. Reserved
//! non-local modes remain closed until remote acquisition and runner propagation
//! are both complete.

use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::Arc;

use mcp_cli::ErrorCategory;
use serde_json::json;

use crate::AppError;
use crate::command::{CommandRunner, FencedCommandRunner, ProcessRunner};
use crate::config::{WriterConfig, WriterMode};
use crate::operation_lock::OperationLock;
use crate::remote_lease::{
    RemoteLeaseAcquire, RemoteLeaseGrant, RemoteLeaseGuard, RemoteWriterLease,
};

#[derive(Debug)]
pub struct WriterOperationGuard {
    local: OperationLock,
    remote: Option<Arc<RemoteLeaseGuard>>,
}

#[derive(Debug, Clone)]
pub enum WriterCommandRunner {
    Local(ProcessRunner),
    Remote(FencedCommandRunner<ProcessRunner, Arc<RemoteLeaseGuard>>),
}

impl CommandRunner for WriterCommandRunner {
    fn run(
        &self,
        command: &crate::command::CommandSpec,
    ) -> Result<crate::command::CommandOutput, crate::command::CommandRunError> {
        match self {
            Self::Local(runner) => runner.run(command),
            Self::Remote(runner) => runner.run(command),
        }
    }

    fn github_api_telemetry(&self) -> crate::model::GitHubApiTelemetry {
        match self {
            Self::Local(runner) => runner.github_api_telemetry(),
            Self::Remote(runner) => runner.github_api_telemetry(),
        }
    }
}

impl WriterOperationGuard {
    pub fn acquire(
        repository: &Path,
        policy: &WriterConfig,
        operation: &str,
    ) -> Result<Self, AppError> {
        match policy.mode {
            WriterMode::LocalOnly => Ok(Self {
                local: OperationLock::acquire(repository, operation)?,
                remote: None,
            }),
            WriterMode::ReadOnly => Err(AppError::structured(
                ErrorCategory::Validation,
                "writer_read_only",
                "writer.mode read_only refuses every mutating operation",
                Some(json!({"operation": operation, "writer_mode": "read_only"})),
            )),
            WriterMode::RemoteFenced => Err(AppError::structured(
                ErrorCategory::Validation,
                "remote_writer_fence_not_active",
                "remote writer fencing is not active until operation-scoped lease propagation lands",
                Some(json!({"operation": operation, "writer_mode": "remote_fenced"})),
            )),
        }
    }

    /// Acquire remote ownership before the existing local lock. If local
    /// acquisition fails, dropping the remote guard attempts exact release.
    pub fn acquire_remote(
        repository: &Path,
        operation: &str,
        backend: Arc<dyn RemoteWriterLease>,
        request: &RemoteLeaseAcquire,
    ) -> Result<Self, AppError> {
        let remote = Arc::new(
            RemoteLeaseGuard::acquire(backend, request)
                .map_err(|error| remote_lease_error(&error))?,
        );
        let local = OperationLock::acquire(repository, operation)?;
        Ok(Self {
            local,
            remote: Some(remote),
        })
    }

    #[must_use]
    pub fn remote_grant(&self) -> Option<&RemoteLeaseGrant> {
        self.remote.as_deref().map(RemoteLeaseGuard::grant)
    }

    /// Apply this operation's exact remote fence to a fully configured runner.
    #[must_use]
    pub fn runner(&self, runner: ProcessRunner) -> WriterCommandRunner {
        match &self.remote {
            Some(remote) => {
                WriterCommandRunner::Remote(FencedCommandRunner::new(runner, Arc::clone(remote)))
            }
            None => WriterCommandRunner::Local(runner),
        }
    }

    /// Release local ownership before dropping exact remote ownership.
    pub fn release(self) -> Result<(), AppError> {
        let Self { local, remote } = self;
        local.release()?;
        drop(remote);
        Ok(())
    }
}

fn remote_lease_error(error: &crate::remote_lease::RemoteLeaseError) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "remote_writer_lease_failed",
        error.to_string(),
        None,
    )
}

impl Deref for WriterOperationGuard {
    type Target = OperationLock;

    fn deref(&self) -> &Self::Target {
        &self.local
    }
}

impl DerefMut for WriterOperationGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.local
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::command::{CommandRunError, CommandSpec};
    use crate::remote_lease::{RemoteLeaseError, RemoteLeaseKey};

    #[derive(Default)]
    struct RecordingLease {
        grant: Mutex<Option<RemoteLeaseGrant>>,
        acquires: AtomicUsize,
        inspects: AtomicUsize,
        releases: AtomicUsize,
    }

    impl RemoteWriterLease for RecordingLease {
        fn acquire(
            &self,
            request: &RemoteLeaseAcquire,
        ) -> Result<RemoteLeaseGrant, RemoteLeaseError> {
            self.acquires.fetch_add(1, Ordering::SeqCst);
            let grant = RemoteLeaseGrant {
                schema_version: 1,
                key: request.key.clone(),
                writer_owner: request.writer_owner.clone(),
                operation_id: request.operation_id.clone(),
                fencing_token: 1,
                heartbeat_due_unix_ms: request.now_unix_ms + request.heartbeat_ms,
                expires_unix_ms: request.now_unix_ms + request.ttl_ms,
                backend_revision: "revision-1".to_owned(),
            };
            *self.grant.lock().unwrap() = Some(grant.clone());
            Ok(grant)
        }

        fn inspect(
            &self,
            _key: &RemoteLeaseKey,
        ) -> Result<Option<RemoteLeaseGrant>, RemoteLeaseError> {
            self.inspects.fetch_add(1, Ordering::SeqCst);
            Ok(self.grant.lock().unwrap().clone())
        }

        fn renew(
            &self,
            _grant: &RemoteLeaseGrant,
            _now_unix_ms: u64,
            _ttl_ms: u64,
            _heartbeat_ms: u64,
        ) -> Result<RemoteLeaseGrant, RemoteLeaseError> {
            Err(RemoteLeaseError::Execution("not used".to_owned()))
        }

        fn release(&self, grant: &RemoteLeaseGrant) -> Result<bool, RemoteLeaseError> {
            self.releases.fetch_add(1, Ordering::SeqCst);
            let mut current = self.grant.lock().unwrap();
            if current.as_ref() == Some(grant) {
                *current = None;
                Ok(true)
            } else {
                Err(RemoteLeaseError::Lost("wrong fence".to_owned()))
            }
        }
    }

    fn init_repository() -> tempfile::TempDir {
        let repository = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(repository.path())
                .status()
                .unwrap()
                .success()
        );
        repository
    }

    fn remote_request() -> RemoteLeaseAcquire {
        let now_unix_ms: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .try_into()
            .unwrap();
        RemoteLeaseAcquire {
            key: RemoteLeaseKey {
                host: "github.com".to_owned(),
                owner: "owner".to_owned(),
                repository: "repo".to_owned(),
                installation_id: Some(42),
            },
            writer_owner: "host-a".to_owned(),
            operation_id: "operation-a".to_owned(),
            now_unix_ms,
            ttl_ms: 60_000,
            heartbeat_ms: 15_000,
        }
    }

    #[test]
    fn local_writer_guard_preserves_exact_operation_lock_exclusion() {
        let repository = init_repository();
        let policy = WriterConfig::default();
        let guard = WriterOperationGuard::acquire(repository.path(), &policy, "sync").unwrap();
        assert_eq!(guard.owner().operation, "sync");
        assert!(OperationLock::acquire(repository.path(), "join").is_err());
        guard.release().unwrap();
        OperationLock::acquire(repository.path(), "join")
            .unwrap()
            .release()
            .unwrap();
    }

    #[test]
    fn remote_ownership_is_acquired_before_local_and_released_on_local_failure() {
        let repository = init_repository();
        let local = OperationLock::acquire(repository.path(), "existing").unwrap();
        let backend = Arc::new(RecordingLease::default());
        let backend_trait: Arc<dyn RemoteWriterLease> = backend.clone();
        assert!(
            WriterOperationGuard::acquire_remote(
                repository.path(),
                "sync",
                backend_trait,
                &remote_request(),
            )
            .is_err()
        );
        assert_eq!(backend.acquires.load(Ordering::SeqCst), 1);
        assert_eq!(backend.releases.load(Ordering::SeqCst), 1);
        local.release().unwrap();
    }

    #[test]
    fn one_remote_guard_fences_multiple_operation_runners() {
        let repository = init_repository();
        let backend = Arc::new(RecordingLease::default());
        let backend_trait: Arc<dyn RemoteWriterLease> = backend.clone();
        let guard = WriterOperationGuard::acquire_remote(
            repository.path(),
            "sync",
            backend_trait,
            &remote_request(),
        )
        .unwrap();
        let read_runner = guard.runner(ProcessRunner::new());
        read_runner.run(&CommandSpec::new("true")).unwrap();
        assert_eq!(backend.inspects.load(Ordering::SeqCst), 0);

        let first = guard.runner(ProcessRunner::new());
        let second = guard.runner(ProcessRunner::new());
        first
            .run(&CommandSpec::new("true").provider_write())
            .unwrap();
        second.run(&CommandSpec::new("true").git_write()).unwrap();
        assert_eq!(backend.inspects.load(Ordering::SeqCst), 2);

        *backend.grant.lock().unwrap() = None;
        assert!(matches!(
            first.run(&CommandSpec::new("true").provider_write()),
            Err(CommandRunError::MutationFenceRefused { .. })
        ));
        assert_eq!(backend.inspects.load(Ordering::SeqCst), 3);
        drop(read_runner);
        drop(first);
        drop(second);
        guard.release().unwrap();
    }

    #[test]
    fn nonlocal_writer_modes_refuse_before_local_lock_creation() {
        let repository = tempfile::tempdir().unwrap();
        for mode in [WriterMode::ReadOnly, WriterMode::RemoteFenced] {
            let policy = WriterConfig {
                mode,
                ..WriterConfig::default()
            };
            assert!(WriterOperationGuard::acquire(repository.path(), &policy, "sync").is_err());
        }
        assert!(
            !repository
                .path()
                .join(".git/caravan/operation.lock")
                .exists()
        );
    }

    #[test]
    fn provider_control_domains_build_one_guarded_runner_per_mutation_boundary() {
        for (name, source, expected) in [
            ("force", include_str!("force.rs"), 1),
            ("force_intent", include_str!("force_intent.rs"), 1),
            ("pause_resume", include_str!("pause.rs"), 2),
            ("priority", include_str!("priority.rs"), 1),
            ("reshape", include_str!("reshape.rs"), 1),
            ("navigation", include_str!("navigation.rs"), 2),
        ] {
            let production = source.split("\n#[cfg(test)]\nmod tests").next().unwrap();
            assert_eq!(
                production.matches("acquire_writer_operation(").count(),
                expected,
                "{name} operation boundary changed"
            );
            assert_eq!(
                production.matches("lock.runner(").count(),
                expected,
                "{name} has an unfenced operation runner"
            );
        }
    }

    #[test]
    fn production_modules_do_not_acquire_operation_lock_directly() {
        for (name, source) in [
            ("force", include_str!("force.rs")),
            ("force_intent", include_str!("force_intent.rs")),
            ("membership", include_str!("membership.rs")),
            ("navigation", include_str!("navigation.rs")),
            ("pause", include_str!("pause.rs")),
            ("priority", include_str!("priority.rs")),
            ("repair", include_str!("repair.rs")),
            ("reshape", include_str!("reshape.rs")),
            ("sync", include_str!("sync.rs")),
            ("sync_plan", include_str!("sync/plan.rs")),
        ] {
            let production = source.split("\n#[cfg(test)]\nmod tests").next().unwrap();
            assert!(
                !production.contains("OperationLock::acquire("),
                "{name} bypasses WriterOperationGuard"
            );
        }
    }
}
