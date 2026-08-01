//! Durable local checkpoints for multi-call native Stack transactions.
//!
//! GitHub Stack merge and reshape are not one provider call. Checkpoints live
//! under the Git common directory so they survive process crashes and linked
//! worktrees, but never enter source control. They contain no credentials.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;

use crate::command::{CommandRunner, CommandSpec, ProcessRunner};
use crate::{AppError, ErrorCategory};

const MAX_CHECKPOINT_BYTES: u64 = 1024 * 1024;

pub(crate) fn load<T: DeserializeOwned>(
    repository: &Path,
    key: &str,
) -> Result<Option<T>, AppError> {
    let path = checkpoint_path(repository, key)?;
    match fs::metadata(&path) {
        Ok(metadata) if metadata.len() > MAX_CHECKPOINT_BYTES => {
            return Err(storage_error(
                "native_stack_checkpoint_too_large",
                "native Stack checkpoint exceeds the 1 MiB safety bound",
                &path,
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(storage_error(
                "native_stack_checkpoint_read_failed",
                &error.to_string(),
                &path,
            ));
        }
        Ok(_) => {}
    }
    let bytes = fs::read(&path).map_err(|error| {
        storage_error(
            "native_stack_checkpoint_read_failed",
            &error.to_string(),
            &path,
        )
    })?;
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        storage_error("native_stack_checkpoint_invalid", &error.to_string(), &path)
    })
}

pub(crate) fn write<T: Serialize>(repository: &Path, key: &str, value: &T) -> Result<(), AppError> {
    let path = checkpoint_path(repository, key)?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        storage_error(
            "native_stack_checkpoint_encode_failed",
            &error.to_string(),
            &path,
        )
    })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CHECKPOINT_BYTES {
        return Err(storage_error(
            "native_stack_checkpoint_too_large",
            "native Stack checkpoint exceeds the 1 MiB safety bound",
            &path,
        ));
    }
    fs::create_dir_all(path.parent().expect("checkpoint parent")).map_err(|error| {
        storage_error(
            "native_stack_checkpoint_write_failed",
            &error.to_string(),
            &path,
        )
    })?;
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    fs::write(&temporary, bytes).map_err(|error| {
        storage_error(
            "native_stack_checkpoint_write_failed",
            &error.to_string(),
            &temporary,
        )
    })?;
    fs::rename(&temporary, &path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        storage_error(
            "native_stack_checkpoint_write_failed",
            &error.to_string(),
            &path,
        )
    })
}

pub(crate) fn remove(repository: &Path, key: &str) -> Result<(), AppError> {
    let path = checkpoint_path(repository, key)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(storage_error(
            "native_stack_checkpoint_remove_failed",
            &error.to_string(),
            &path,
        )),
    }
}

fn checkpoint_path(repository: &Path, key: &str) -> Result<PathBuf, AppError> {
    if key.is_empty()
        || key.len() > 128
        || !key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(AppError::validation(
            "native_stack_checkpoint_key_invalid",
            "native Stack checkpoint key must be 1..=128 ASCII alphanumeric, dash, or underscore characters",
        ));
    }
    let output = ProcessRunner::in_directory(repository)
        .run(&CommandSpec::new("git").args([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ]))
        .map_err(|error| {
            AppError::structured(
                ErrorCategory::ExecutionFailure,
                "native_stack_checkpoint_storage_discovery_failed",
                error.to_string(),
                None,
            )
        })?;
    if !output.is_success() {
        return Err(AppError::structured(
            ErrorCategory::TargetNotFound,
            "git_repository_not_found",
            "native Stack checkpoint state requires a Git repository",
            Some(json!({"stderr": output.stderr})),
        ));
    }
    Ok(PathBuf::from(output.stdout.trim())
        .join("caravan")
        .join("native-stack")
        .join(format!("{key}.json")))
}

fn storage_error(code: &str, message: &str, path: &Path) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        code,
        message.to_owned(),
        Some(json!({
            "path": path,
            "resumable": true,
            "mutated": false,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_cli::StructuredError;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct Fixture {
        phase: String,
        value: u64,
    }

    fn repository() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let output = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        directory
    }

    #[test]
    fn checkpoint_survives_process_boundaries_and_removes_idempotently() {
        let repository = repository();
        let expected = Fixture {
            phase: "unstacked".to_owned(),
            value: 42,
        };

        assert_eq!(
            load::<Fixture>(repository.path(), "reshape-evict-42").unwrap(),
            None
        );
        write(repository.path(), "reshape-evict-42", &expected).unwrap();
        assert_eq!(
            load::<Fixture>(repository.path(), "reshape-evict-42").unwrap(),
            Some(expected)
        );
        remove(repository.path(), "reshape-evict-42").unwrap();
        remove(repository.path(), "reshape-evict-42").unwrap();
        assert_eq!(
            load::<Fixture>(repository.path(), "reshape-evict-42").unwrap(),
            None
        );
    }

    #[test]
    fn unsafe_keys_and_oversized_payloads_fail_before_writing() {
        let repository = repository();
        assert_eq!(
            load::<Fixture>(repository.path(), "../escape")
                .unwrap_err()
                .code(),
            "native_stack_checkpoint_key_invalid"
        );
        let oversized = "x".repeat(usize::try_from(MAX_CHECKPOINT_BYTES).unwrap() + 1);
        assert_eq!(
            write(repository.path(), "too-large", &oversized)
                .unwrap_err()
                .code(),
            "native_stack_checkpoint_too_large"
        );
    }
}
