//! Single production entry point for repository mutation authority.
//!
//! The first slice preserves the existing local lock exactly. Reserved
//! non-local modes remain closed until remote acquisition and runner propagation
//! are both complete.

use std::ops::{Deref, DerefMut};
use std::path::Path;

use mcp_cli::ErrorCategory;
use serde_json::json;

use crate::AppError;
use crate::config::{WriterConfig, WriterMode};
use crate::operation_lock::OperationLock;

#[derive(Debug)]
pub struct WriterOperationGuard {
    local: OperationLock,
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

    /// Explicitly release the existing local guard. The remote-enabled successor
    /// will release local ownership before exact best-effort remote ownership.
    pub fn release(self) -> Result<(), AppError> {
        self.local.release()
    }
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
    use super::*;

    #[test]
    fn local_writer_guard_preserves_exact_operation_lock_exclusion() {
        let repository = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(repository.path())
                .status()
                .unwrap()
                .success()
        );
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
    fn nonlocal_writer_modes_refuse_before_local_lock_creation() {
        let repository = tempfile::tempdir().unwrap();
        for mode in [WriterMode::ReadOnly, WriterMode::RemoteFenced] {
            let policy = WriterConfig { mode };
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
