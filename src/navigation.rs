//! Safe PR and caravan-fleet navigation without overwriting local work.

use std::path::{Path, PathBuf};

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::command::{CommandOutput, CommandRunError, CommandRunner, CommandSpec, ProcessRunner};
use crate::model::{Caravan, CommitOid, PrNumber, PullRequestSnapshot, RepositoryId};
use crate::operation_lock::OperationLock;
use crate::read::{self, StatusOutput};
use crate::{AppContext, AppError};

/// Direction within a chain or the fleet's deterministic browsing order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Next,
    Previous,
}

/// Navigation scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Caravan,
    Fleet,
}

/// A stale local destination preserved before navigation follows the exact provider head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LocalBranchReconciliation {
    pub branch: String,
    pub previous_oid: CommitOid,
    pub provider_oid: CommitOid,
    /// Internal ref retaining the previous local generation without exposing it as a branch.
    pub backup_ref: String,
}

/// Exact checkout result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NavigationOutput {
    pub repository: RepositoryId,
    pub scope: Scope,
    pub direction: Direction,
    /// Source PR, or `None` when fleet navigation enters from the default branch.
    pub from_pr: Option<PrNumber>,
    pub to_pr: PrNumber,
    pub branch: String,
    pub oid: CommitOid,
    /// Present only when navigation preserved and advanced a stale local destination branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_branch_reconciliation: Option<LocalBranchReconciliation>,
}

/// Read-only fleet list for `cara van list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VanListOutput {
    pub repository: RepositoryId,
    pub caravans: Vec<Caravan>,
}

/// List caravans without taking the mutating operation lock.
pub fn list(context: &AppContext) -> Result<VanListOutput, AppError> {
    let status = read::status(context)?;
    Ok(VanListOutput {
        repository: status.repository,
        caravans: status.analysis.fleet.caravans,
    })
}

/// Check out one exact discovered PR head for a sync decision.
///
/// This is deliberately separate from directional browsing: sync already chose
/// the affected PR and only needs the same clean-worktree and exact-OID safety
/// transaction.
pub fn checkout_decision_snapshot(
    context: &AppContext,
    pull_request: &PullRequestSnapshot,
    operation_deadline: std::time::Instant,
) -> Result<Option<crate::operation_lock::OperationLockRecovery>, AppError> {
    let mut lock = OperationLock::acquire(&context.repository_path, "sync_decision_checkout")?;
    let lock_recovery = lock.recovered_dead_owner().cloned();
    lock.checkpoint(
        "decision_checkout_in_flight",
        serde_json::json!({
            "pr": pull_request.number,
            "head": &pull_request.head,
            "provider_state_indeterminate": false,
        }),
        false,
    )?;
    let runner = ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(std::time::Duration::from_secs(
            context.config.command_timeout_secs,
        ))
        .with_operation_deadline(operation_deadline);
    ensure_safe_worktree(&context.repository_path, &context.config_path, &runner)?;
    // The decision already embeds the exact provider snapshot. checkout_exact
    // verifies the remote branch still advertises that OID, avoiding a third
    // full repository discovery during an already-bounded sync decision.
    let _reconciliation = checkout_exact(
        &context.repository_path,
        &context.config_path,
        "origin",
        &runner,
        pull_request,
    )?;
    lock.release()?;
    Ok(lock_recovery)
}

/// Navigate and check out an exact PR head in the live repository.
pub fn navigate(
    context: &AppContext,
    scope: Scope,
    direction: Direction,
) -> Result<NavigationOutput, AppError> {
    let operation = format!("navigate_{scope:?}_{direction:?}").to_ascii_lowercase();
    let lock = OperationLock::acquire(&context.repository_path, &operation)?;
    let runner = ProcessRunner::in_directory(&context.repository_path).with_timeout(
        std::time::Duration::from_secs(context.config.command_timeout_secs),
    );
    ensure_safe_worktree(&context.repository_path, &context.config_path, &runner)?;
    let status = read::status(context)?;
    let (from_pr, to_pr) = select_destination(&status, scope, direction)?;
    let pull_request = status
        .analysis
        .pull_requests
        .get(&to_pr)
        .ok_or_else(|| missing_pr(to_pr))?;
    let local_branch_reconciliation = checkout_exact(
        &context.repository_path,
        &context.config_path,
        "origin",
        &runner,
        pull_request,
    )?;
    lock.release()?;
    Ok(NavigationOutput {
        repository: status.repository,
        scope,
        direction,
        from_pr,
        to_pr,
        branch: pull_request.head.name.clone(),
        oid: pull_request.head.oid.clone(),
        local_branch_reconciliation,
    })
}

/// Pure destination selection for fixture testing.
pub fn select_destination(
    status: &StatusOutput,
    scope: Scope,
    direction: Direction,
) -> Result<(Option<PrNumber>, PrNumber), AppError> {
    let Some(current) = status.current_pr else {
        return select_without_active_pr(status, scope, direction);
    };
    let historical = read::historical_predecessor(status);
    let current_caravan = status.analysis.fleet.containing(current).ok_or_else(|| {
        AppError::validation(
            "current_pr_not_in_caravan",
            format!("PR #{current} is not an active caravan member"),
        )
    })?;

    // A retained merged branch occupies the historical slot immediately before
    // its recovered active successor. Enter that successor for both chain and
    // fleet `next`; `previous` uses the active successor's deterministic lower
    // boundary/fleet predecessor semantics.
    if historical.is_some() && direction == Direction::Next {
        return Ok((historical, current));
    }

    let destination = match scope {
        Scope::Caravan => {
            let position = current_caravan
                .position(current)
                .expect("containing caravan includes current PR");
            match direction {
                Direction::Next => current_caravan.members.get(position + 1).copied(),
                Direction::Previous => position
                    .checked_sub(1)
                    .and_then(|index| current_caravan.members.get(index).copied()),
            }
        }
        Scope::Fleet => {
            let position = status
                .analysis
                .fleet
                .caravans
                .iter()
                .position(|caravan| caravan.id == current_caravan.id)
                .expect("current caravan is in fleet");
            match direction {
                Direction::Next => status
                    .analysis
                    .fleet
                    .caravans
                    .get(position + 1)
                    .and_then(Caravan::head),
                Direction::Previous => position
                    .checked_sub(1)
                    .and_then(|index| status.analysis.fleet.caravans.get(index))
                    .and_then(Caravan::head),
            }
        }
    };
    destination.map_or_else(
        || {
            Err(AppError::structured(
                ErrorCategory::TargetNotFound,
                "navigation_boundary",
                format!(
                    "PR #{current} is already at the {direction:?} boundary for {scope:?} navigation"
                ),
                Some(json!({
                    "current_pr": current,
                    "scope": scope,
                    "direction": direction,
                })),
            ))
        },
        |destination| Ok((Some(historical.unwrap_or(current)), destination)),
    )
}

fn select_without_active_pr(
    status: &StatusOutput,
    scope: Scope,
    direction: Direction,
) -> Result<(Option<PrNumber>, PrNumber), AppError> {
    if let Some(predecessor) = read::historical_predecessor(status) {
        return Err(historical_boundary_error(
            status,
            predecessor,
            scope,
            direction,
        ));
    }
    if scope != Scope::Fleet
        || status.current_branch.as_deref() != Some(status.default_branch.as_str())
    {
        return Err(AppError::validation(
            "current_pr_not_found",
            "the current branch has no unique open GitHub pull request",
        ));
    }
    if direction == Direction::Previous {
        return Err(AppError::structured(
            ErrorCategory::TargetNotFound,
            "navigation_boundary",
            "the default branch is already before the first caravan in fleet order",
            Some(json!({
                "current_pr": null,
                "current_branch": status.default_branch,
                "scope": scope,
                "direction": direction,
            })),
        ));
    }
    let destination = status
        .analysis
        .fleet
        .caravans
        .first()
        .and_then(Caravan::head)
        .ok_or_else(|| {
            AppError::structured(
                ErrorCategory::TargetNotFound,
                "navigation_boundary",
                "there are no caravan heads to navigate to",
                Some(json!({
                    "current_pr": null,
                    "current_branch": status.default_branch,
                    "scope": scope,
                    "direction": direction,
                })),
            )
        })?;
    Ok((None, destination))
}

fn historical_boundary_error(
    status: &StatusOutput,
    predecessor: PrNumber,
    scope: Scope,
    direction: Direction,
) -> AppError {
    AppError::structured(
        ErrorCategory::TargetNotFound,
        "historical_successor_not_found",
        format!("merged Caravan PR #{predecessor} has no unique active rolling successor"),
        Some(json!({
            "historical_predecessor": predecessor,
            "current_branch": status.current_branch,
            "scope": scope,
            "direction": direction,
            "fail_closed": true,
        })),
    )
}

/// Refuse dirty worktrees and in-progress Git operations before changing HEAD.
pub fn ensure_safe_worktree(
    repository: &Path,
    config_path: &Path,
    runner: &impl CommandRunner,
) -> Result<(), AppError> {
    // Request individual untracked files and NUL delimiters so only the exact
    // validated config can be recognized. In particular, never exempt the
    // `.caravan/` directory or another file beside the config.
    let allowance = local_config_allowance(repository, config_path);
    let status = run(
        runner,
        CommandSpec::new("git").args(["status", "--porcelain=v1", "-z", "--untracked-files=all"]),
    )?;
    require_success(
        "git_status_failed",
        "could not inspect worktree status",
        &status,
    )?;
    let allowed_entry = allowance
        .as_ref()
        .map(|allowed| format!("?? {}", allowed.relative.display()));
    let dirty = status
        .stdout
        .split_terminator('\0')
        .filter(|entry| !entry.is_empty())
        .filter(|entry| allowed_entry.as_deref() != Some(*entry))
        .collect::<Vec<_>>();
    let allowance_still_valid = allowance.as_ref().is_none_or(|allowed| {
        Some(allowed.identity) == file_identity(&allowed.path)
            && crate::config::CaravanConfig::load(&allowed.path).is_ok()
    });
    if !dirty.is_empty() || !allowance_still_valid {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "dirty_worktree",
            "refusing to switch branches because the worktree has tracked or untracked changes",
            Some(json!({ "status": bounded(&status.stdout.replace('\0', "\n")) })),
        ));
    }

    let git_dir = run(
        runner,
        CommandSpec::new("git").args(["rev-parse", "--path-format=absolute", "--git-dir"]),
    )?;
    require_success(
        "git_directory_failed",
        "could not resolve Git metadata directory",
        &git_dir,
    )?;
    let path = resolve_git_dir(repository, git_dir.stdout.trim());
    for marker in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
        "rebase-merge",
        "rebase-apply",
    ] {
        if path.join(marker).exists() {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "git_operation_in_progress",
                format!("refusing to switch branches while Git operation marker `{marker}` exists"),
                Some(json!({ "git_dir": path, "marker": marker })),
            ));
        }
    }
    Ok(())
}

/// Check out exactly the discovered PR head without discarding a stale local ref.
///
/// A non-current local destination that differs from an exact provider snapshot
/// is retained under `refs/cara-backup/navigation/*` and advanced atomically.
/// The checked-out branch and branches owned by another worktree still fail
/// closed. Validation, ref reconciliation, switching, and post-verification stay
/// in one transaction-shaped function so no caller can omit a safety phase.
#[allow(clippy::too_many_lines)]
pub fn checkout_exact(
    repository: &Path,
    config_path: &Path,
    remote: &str,
    runner: &impl CommandRunner,
    pull_request: &PullRequestSnapshot,
) -> Result<Option<LocalBranchReconciliation>, AppError> {
    if pull_request.cross_repository || pull_request.head.repository != pull_request.base.repository
    {
        return Err(AppError::validation(
            "fork_checkout_unsupported",
            "Caravan v1 navigation requires an in-repository PR head branch",
        ));
    }
    if remote.trim().is_empty() || remote.starts_with('-') {
        return Err(AppError::validation(
            "invalid_remote",
            "the navigation remote must be non-empty and must not begin with '-'",
        ));
    }
    let branch = &pull_request.head.name;
    let expected = &pull_request.head.oid.0;
    let branch_check = run(
        runner,
        CommandSpec::new("git").args(["check-ref-format", "--branch", branch]),
    )?;
    require_success(
        "invalid_branch_name",
        "the PR head is not a valid local branch name",
        &branch_check,
    )?;

    let reference = format!("refs/heads/{branch}");
    let advertised_oid = advertised_branch_oid(runner, remote, &reference)?;
    if advertised_oid != *expected {
        return Err(stale_head(pull_request, &advertised_oid));
    }

    let local_commit = format!("{reference}^{{commit}}");
    let local = run(
        runner,
        CommandSpec::new("git").args(["rev-parse", "--verify", "--quiet", local_commit.as_str()]),
    )?;
    let local_branch_reconciliation = match local.code {
        Some(0) if local.stdout.trim() == expected => None,
        Some(0) => Some(reconcile_local_destination(
            repository,
            config_path,
            remote,
            runner,
            pull_request,
            &reference,
            local.stdout.trim(),
        )?),
        Some(1) => {
            fetch_exact_branch_object(runner, remote, &reference, pull_request)?;
            ensure_safe_worktree(repository, config_path, runner)?;
            let transaction = format!("start\ncreate {reference} {expected}\nprepare\ncommit\n");
            let create = run(
                runner,
                CommandSpec::new("git")
                    .args(["update-ref", "--stdin", "--create-reflog"])
                    .stdin(transaction),
            )?;
            require_success(
                "checkout_branch_create_failed",
                "could not create the exact local PR branch",
                &create,
            )?;
            None
        }
        _ => {
            return Err(command_failure(
                "local_branch_inspection_failed",
                "could not inspect the local PR branch",
                &local,
            ));
        }
    };

    let verify = run(
        runner,
        CommandSpec::new("git").args(["rev-parse", "--verify", "--quiet", local_commit.as_str()]),
    )?;
    require_success(
        "local_branch_inspection_failed",
        "could not verify the local PR branch",
        &verify,
    )?;
    if verify.stdout.trim() != expected {
        return Err(stale_head(pull_request, verify.stdout.trim()));
    }

    // Discovery and transport may have taken time. Recheck immediately before
    // changing HEAD, including the exact config inode/content allowance.
    ensure_safe_worktree(repository, config_path, runner)?;
    let switch = run(
        runner,
        CommandSpec::new("git").args(["switch", "--quiet", branch]),
    )?;
    if !switch.is_success() {
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "branch_switch_failed",
            "Git refused to switch to the exact PR branch",
            Some(json!({
                "exit_code": switch.code,
                "stdout": bounded(&switch.stdout),
                "stderr": bounded(&switch.stderr),
                "local_branch_reconciliation": local_branch_reconciliation,
            })),
        ));
    }
    let head = run(
        runner,
        CommandSpec::new("git").args(["rev-parse", "--verify", "HEAD^{commit}"]),
    )?;
    require_success(
        "checkout_verification_failed",
        "could not verify checked-out HEAD",
        &head,
    )?;
    if head.stdout.trim() != expected {
        return Err(stale_head(pull_request, head.stdout.trim()));
    }
    Ok(local_branch_reconciliation)
}

fn advertised_branch_oid(
    runner: &impl CommandRunner,
    remote: &str,
    reference: &str,
) -> Result<String, AppError> {
    let advertised = run(
        runner,
        CommandSpec::new("git").args(["ls-remote", "--refs", "--exit-code", remote, reference]),
    )?;
    require_success(
        "remote_ref_not_found",
        "the PR head branch is not advertised by the remote",
        &advertised,
    )?;
    advertised
        .stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find_map(|(oid, found)| (found == reference).then(|| oid.to_owned()))
        .ok_or_else(|| {
            AppError::structured(
                ErrorCategory::ExecutionFailure,
                "ls_remote_output_invalid",
                "git ls-remote omitted the requested PR branch",
                Some(json!({ "reference": reference })),
            )
        })
}

fn fetch_exact_branch_object(
    runner: &impl CommandRunner,
    remote: &str,
    reference: &str,
    pull_request: &PullRequestSnapshot,
) -> Result<(), AppError> {
    let fetch = run(
        runner,
        CommandSpec::new("git").args([
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--refmap=",
            remote,
            reference,
        ]),
    )?;
    require_success(
        "checkout_fetch_failed",
        "could not fetch the exact provider PR branch",
        &fetch,
    )?;
    let expected = &pull_request.head.oid.0;
    let object = format!("{expected}^{{commit}}");
    let verify = run(
        runner,
        CommandSpec::new("git").args(["cat-file", "-e", object.as_str()]),
    )?;
    require_success(
        "checkout_fetch_failed",
        "the exact provider PR head was absent after fetch",
        &verify,
    )?;
    let advertised = advertised_branch_oid(runner, remote, reference)?;
    if advertised != *expected {
        return Err(stale_head(pull_request, &advertised));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn reconcile_local_destination(
    repository: &Path,
    config_path: &Path,
    remote: &str,
    runner: &impl CommandRunner,
    pull_request: &PullRequestSnapshot,
    reference: &str,
    local_oid: &str,
) -> Result<LocalBranchReconciliation, AppError> {
    let branch = &pull_request.head.name;
    let current = run(
        runner,
        CommandSpec::new("git").args(["symbolic-ref", "--quiet", "HEAD"]),
    )?;
    match current.code {
        Some(0) if current.stdout.trim() == reference => {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "current_local_branch_diverged",
                format!("checked-out branch `{branch}` differs from the exact provider head"),
                Some(json!({
                    "branch": branch,
                    "local_oid": local_oid,
                    "provider_oid": pull_request.head.oid,
                    "mutated": false,
                    "next": "preserve or publish current local work, then retry from another clean branch",
                })),
            ));
        }
        Some(0 | 1) => {}
        _ => {
            return Err(command_failure(
                "current_branch_inspection_failed",
                "could not inspect the current symbolic branch",
                &current,
            ));
        }
    }

    let worktrees = run(
        runner,
        CommandSpec::new("git").args(["worktree", "list", "--porcelain"]),
    )?;
    require_success(
        "worktree_inspection_failed",
        "could not verify destination worktree ownership",
        &worktrees,
    )?;
    let branch_marker = format!("branch {reference}");
    if worktrees.stdout.lines().any(|line| line == branch_marker) {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "local_branch_checked_out_elsewhere",
            format!("local destination branch `{branch}` is checked out in another worktree"),
            Some(json!({
                "branch": branch,
                "local_oid": local_oid,
                "provider_oid": pull_request.head.oid,
                "mutated": false,
                "next": "leave that worktree on another branch, then retry navigation",
            })),
        ));
    }

    fetch_exact_branch_object(runner, remote, reference, pull_request)?;
    ensure_safe_worktree(repository, config_path, runner)?;
    let branch_hash = format!("{:x}", Sha256::digest(branch.as_bytes()));
    let backup_ref = format!(
        "refs/cara-backup/navigation/{}/{}",
        &branch_hash[..16],
        local_oid
    );
    let backup_commit = format!("{backup_ref}^{{commit}}");
    let existing_backup = run(
        runner,
        CommandSpec::new("git").args(["rev-parse", "--verify", "--quiet", backup_commit.as_str()]),
    )?;
    let create_backup = match existing_backup.code {
        Some(0) if existing_backup.stdout.trim() == local_oid => false,
        Some(0) => {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "navigation_backup_ref_collision",
                "the deterministic navigation backup ref resolves to an unexpected object",
                Some(json!({
                    "backup_ref": backup_ref,
                    "expected_oid": local_oid,
                    "actual_oid": existing_backup.stdout.trim(),
                    "mutated": false,
                })),
            ));
        }
        Some(1) => true,
        _ => {
            return Err(command_failure(
                "navigation_backup_inspection_failed",
                "could not inspect the deterministic navigation backup ref",
                &existing_backup,
            ));
        }
    };

    let expected = &pull_request.head.oid.0;
    let transaction = if create_backup {
        format!(
            "start\ncreate {backup_ref} {local_oid}\nupdate {reference} {expected} {local_oid}\nprepare\ncommit\n"
        )
    } else {
        format!("start\nupdate {reference} {expected} {local_oid}\nprepare\ncommit\n")
    };
    let update = run(
        runner,
        CommandSpec::new("git")
            .args(["update-ref", "--stdin", "--create-reflog"])
            .stdin(transaction),
    )?;
    if !update.is_success() {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "local_branch_reconcile_race",
            "the local destination branch changed during its atomic backup/update transaction",
            Some(json!({
                "branch": branch,
                "previous_oid": local_oid,
                "provider_oid": expected,
                "backup_ref": backup_ref,
                "exit_code": update.code,
                "stdout": bounded(&update.stdout),
                "stderr": bounded(&update.stderr),
                "mutated": false,
                "next": "inspect the local branch owner and retry from fresh provider facts",
            })),
        ));
    }
    Ok(LocalBranchReconciliation {
        branch: branch.clone(),
        previous_oid: CommitOid(local_oid.to_owned()),
        provider_oid: pull_request.head.oid.clone(),
        backup_ref,
    })
}

#[allow(clippy::needless_pass_by_value)]
fn run(runner: &impl CommandRunner, command: CommandSpec) -> Result<CommandOutput, AppError> {
    runner
        .run(&command)
        .map_err(|error| command_run_error(&error))
}

fn command_run_error(error: &CommandRunError) -> AppError {
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
            "navigation_command_timeout",
            error.to_string(),
            Some(json!({
                "stage": "navigation",
                "command": command.display(),
                "timeout_ms": timeout_ms,
                "stdout": stdout,
                "stderr": stderr,
                "resumable": true,
                "next": "retry navigation after restoring Git/GitHub transport health",
            })),
        );
    }
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "navigation_command_failed",
        error.to_string(),
        Some(json!({ "error": format!("{error:?}") })),
    )
}

fn require_success(code: &str, message: &str, output: &CommandOutput) -> Result<(), AppError> {
    if output.is_success() {
        Ok(())
    } else {
        Err(command_failure(code, message, output))
    }
}

fn command_failure(code: &str, message: &str, output: &CommandOutput) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        code,
        message,
        Some(json!({
            "exit_code": output.code,
            "stdout": bounded(&output.stdout),
            "stderr": bounded(&output.stderr),
        })),
    )
}

fn stale_head(pull_request: &PullRequestSnapshot, observed: &str) -> AppError {
    AppError::structured(
        ErrorCategory::Validation,
        "stale_pr_head",
        format!("PR #{} moved since discovery", pull_request.number),
        Some(json!({
            "pr": pull_request.number,
            "expected_oid": pull_request.head.oid,
            "observed_oid": observed,
            "resumable": true,
            "next": "rediscover the caravan and retry navigation",
        })),
    )
}

fn missing_pr(number: PrNumber) -> AppError {
    AppError::structured(
        ErrorCategory::TargetNotFound,
        "navigation_pr_not_found",
        format!("PR #{number} is missing from the discovery snapshot"),
        None,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalConfigAllowance {
    path: PathBuf,
    relative: PathBuf,
    identity: FileIdentity,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(not(unix))]
type FileIdentity = (u64, Option<std::time::SystemTime>);

#[cfg(unix)]
fn file_identity(path: &Path) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path).ok()?;
    metadata.file_type().is_file().then_some(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn file_identity(path: &Path) -> Option<FileIdentity> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    metadata
        .file_type()
        .is_file()
        .then(|| (metadata.len(), metadata.modified().ok()))
}

fn local_config_allowance(repository: &Path, config_path: &Path) -> Option<LocalConfigAllowance> {
    let repository = std::fs::canonicalize(repository).ok()?;
    let path = if config_path.is_absolute() {
        config_path.to_path_buf()
    } else {
        repository.join(config_path)
    };
    // `symlink_metadata` rejects a final-component symlink. Canonicalization
    // additionally rejects a parent symlink which escapes the worktree.
    let identity = file_identity(&path)?;
    let canonical = std::fs::canonicalize(&path).ok()?;
    let relative = canonical.strip_prefix(&repository).ok()?.to_path_buf();
    if relative.as_os_str().is_empty() {
        return None;
    }
    crate::config::CaravanConfig::load(&path).ok()?;
    Some(LocalConfigAllowance {
        path,
        relative,
        identity,
    })
}

fn resolve_git_dir(repository: &Path, reported: &str) -> PathBuf {
    let path = PathBuf::from(reported);
    if path.is_absolute() {
        path
    } else {
        repository.join(path)
    }
}

fn bounded(value: &str) -> String {
    const LIMIT: usize = 4_096;
    if value.len() <= LIMIT {
        value.to_owned()
    } else {
        let mut end = LIMIT;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…[truncated]", &value[..end])
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::process::Command;

    use super::*;
    use crate::graph::GraphAnalysis;
    use crate::model::{AutoMergeState, BranchSnapshot, CaravanFleet, CommitOid, PullRequestState};

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "harryaskham".to_owned(),
            name: "caravan".to_owned(),
        }
    }

    fn branch(name: &str, number: u64) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository(),
            name: name.to_owned(),
            oid: CommitOid(format!("{number:040x}")),
        }
    }

    fn pull_request(number: u64, base: &str) -> PullRequestSnapshot {
        PullRequestSnapshot {
            number: PrNumber(number),
            title: format!("PR {number}"),
            url: format!("https://example.invalid/{number}"),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&format!("pr-{number}"), number),
            base: branch(base, 99),
            cross_repository: false,
            labels: BTreeSet::from(["caravan".to_owned()]),
            auto_merge: if base == "main" {
                AutoMergeState::squash()
            } else {
                AutoMergeState::disabled()
            },
            checks: Vec::new(),
            created_at: Some(format!("2026-01-01T00:00:{number:02}Z")),
            merged_at: None,
            updated_at: None,
        }
    }

    fn status(current: u64) -> StatusOutput {
        let pulls = [
            pull_request(1, "main"),
            pull_request(2, "pr-1"),
            pull_request(3, "main"),
        ];
        let pull_requests = pulls
            .into_iter()
            .map(|pull_request| (pull_request.number, pull_request))
            .collect();
        StatusOutput {
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
            current_branch: Some(format!("pr-{current}")),
            current_pr: Some(PrNumber(current)),
            healthy: true,
            initialization: crate::initialization::InitializationStatus::default(),
            admission: crate::read::AdmissionStatus {
                policy: "priority then FIFO".to_owned(),
                priority_labels: Vec::new(),
                generation_integrity: crate::generation::GenerationIntegrityStatus::default(),
                candidates: Vec::new(),
                skipped: Vec::new(),
                rejected: Vec::new(),
                next_candidate: None,
            },
            analysis: GraphAnalysis {
                fleet: CaravanFleet {
                    repository: repository(),
                    default_branch: branch("main", 99),
                    caravans: vec![
                        Caravan::new(vec![PrNumber(1), PrNumber(2)]).unwrap(),
                        Caravan::new(vec![PrNumber(3)]).unwrap(),
                    ],
                    unqueued: Vec::new(),
                    problems: Vec::new(),
                },
                pull_requests,
                compatibility: Vec::new(),
            },
            pauses: Vec::new(),
            sync_budget: crate::sync::SyncBudgetStatus::default(),
        }
    }

    #[test]
    fn selects_chain_and_fleet_destinations_without_wrapping() {
        let first_status = status(1);
        assert_eq!(
            select_destination(&first_status, Scope::Caravan, Direction::Next).unwrap(),
            (Some(PrNumber(1)), PrNumber(2))
        );
        assert_eq!(
            select_destination(&first_status, Scope::Fleet, Direction::Next).unwrap(),
            (Some(PrNumber(1)), PrNumber(3))
        );
        assert!(select_destination(&first_status, Scope::Caravan, Direction::Previous).is_err());
        let tail = status(2);
        assert!(select_destination(&tail, Scope::Caravan, Direction::Next).is_err());
    }

    #[test]
    fn historical_branch_next_enters_recovered_successor() {
        let mut historical = status(1);
        let mut merged = pull_request(9, "main");
        merged.state = PullRequestState::Merged;
        merged.head = branch("old-head", 9);
        merged.merged_at = Some("2026-01-01T00:01:00Z".to_owned());
        historical
            .analysis
            .pull_requests
            .insert(merged.number, merged);
        historical.current_branch = Some("old-head".to_owned());

        assert_eq!(
            select_destination(&historical, Scope::Caravan, Direction::Next).unwrap(),
            (Some(PrNumber(9)), PrNumber(1))
        );
        assert_eq!(
            select_destination(&historical, Scope::Fleet, Direction::Next).unwrap(),
            (Some(PrNumber(9)), PrNumber(1))
        );
    }

    #[test]
    fn historical_branch_without_successor_has_typed_boundary() {
        let mut historical = status(1);
        let mut merged = pull_request(9, "main");
        merged.state = PullRequestState::Merged;
        merged.head = branch("old-head", 9);
        historical
            .analysis
            .pull_requests
            .insert(merged.number, merged);
        historical.current_branch = Some("old-head".to_owned());
        historical.current_pr = None;

        let error = select_destination(&historical, Scope::Caravan, Direction::Next).unwrap_err();
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "historical_successor_not_found"
        );
    }

    #[test]
    fn fleet_next_enters_first_caravan_from_default_branch() {
        let mut default_branch = status(1);
        default_branch.current_branch = Some("main".to_owned());
        default_branch.current_pr = None;

        assert_eq!(
            select_destination(&default_branch, Scope::Fleet, Direction::Next).unwrap(),
            (None, PrNumber(1))
        );
        let previous =
            select_destination(&default_branch, Scope::Fleet, Direction::Previous).unwrap_err();
        assert_eq!(
            mcp_cli::StructuredError::code(&previous),
            "navigation_boundary"
        );
        let caravan =
            select_destination(&default_branch, Scope::Caravan, Direction::Next).unwrap_err();
        assert_eq!(
            mcp_cli::StructuredError::code(&caravan),
            "current_pr_not_found"
        );
    }

    #[test]
    fn non_default_branch_without_pr_cannot_enter_fleet_navigation() {
        let mut feature_branch = status(1);
        feature_branch.current_branch = Some("local-work".to_owned());
        feature_branch.current_pr = None;

        let error = select_destination(&feature_branch, Scope::Fleet, Direction::Next).unwrap_err();
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "current_pr_not_found"
        );
    }

    #[test]
    fn dirty_worktree_is_refused_before_checkout() {
        let directory = tempfile::tempdir().unwrap();
        git(
            directory.path(),
            ["init", "--quiet", "--initial-branch=main"],
        );
        git(directory.path(), ["config", "user.name", "Caravan Test"]);
        git(
            directory.path(),
            ["config", "user.email", "caravan@example.invalid"],
        );
        fs::write(directory.path().join("dirty.txt"), "dirty\n").unwrap();
        let runner = ProcessRunner::in_directory(directory.path());
        let error = ensure_safe_worktree(
            directory.path(),
            &directory.path().join(".caravan/config.yaml"),
            &runner,
        )
        .unwrap_err();
        assert_eq!(mcp_cli::StructuredError::code(&error), "dirty_worktree");
    }

    #[test]
    fn exact_valid_untracked_config_is_allowed_but_neighbors_are_not() {
        let directory = tempfile::tempdir().unwrap();
        git(
            directory.path(),
            ["init", "--quiet", "--initial-branch=main"],
        );
        let config = directory.path().join(".caravan/config.yaml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "{}\n").unwrap();
        let runner = ProcessRunner::in_directory(directory.path());

        ensure_safe_worktree(directory.path(), &config, &runner)
            .expect("the exact validated init-owned config is safe");
        fs::write(directory.path().join(".caravan/notes.txt"), "unrelated\n").unwrap();
        let error = ensure_safe_worktree(directory.path(), &config, &runner).unwrap_err();
        assert_eq!(mcp_cli::StructuredError::code(&error), "dirty_worktree");
    }

    #[test]
    fn tracked_config_is_treated_like_every_other_tracked_file() {
        let directory = tempfile::tempdir().unwrap();
        git(
            directory.path(),
            ["init", "--quiet", "--initial-branch=main"],
        );
        git(directory.path(), ["config", "user.name", "Caravan Test"]);
        git(
            directory.path(),
            ["config", "user.email", "caravan@example.invalid"],
        );
        let config = directory.path().join(".caravan/config.yaml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "{}\n").unwrap();
        git(directory.path(), ["add", ".caravan/config.yaml"]);
        git(
            directory.path(),
            ["commit", "--quiet", "--message", "config"],
        );
        let runner = ProcessRunner::in_directory(directory.path());
        ensure_safe_worktree(directory.path(), &config, &runner).unwrap();

        fs::write(&config, "version: 1\nforce_merge: true\n").unwrap();
        let error = ensure_safe_worktree(directory.path(), &config, &runner).unwrap_err();
        assert_eq!(mcp_cli::StructuredError::code(&error), "dirty_worktree");
    }

    #[test]
    fn config_override_outside_worktree_needs_no_exemption() {
        let directory = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        git(
            directory.path(),
            ["init", "--quiet", "--initial-branch=main"],
        );
        let config = external.path().join("config.yaml");
        fs::write(&config, "{}\n").unwrap();
        let runner = ProcessRunner::in_directory(directory.path());
        ensure_safe_worktree(directory.path(), &config, &runner).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_config_is_never_exempted() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        git(
            directory.path(),
            ["init", "--quiet", "--initial-branch=main"],
        );
        let config = directory.path().join(".caravan/config.yaml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let target = external.path().join("config.yaml");
        fs::write(&target, "{}\n").unwrap();
        symlink(&target, &config).unwrap();
        let runner = ProcessRunner::in_directory(directory.path());
        let error = ensure_safe_worktree(directory.path(), &config, &runner).unwrap_err();
        assert_eq!(mcp_cli::StructuredError::code(&error), "dirty_worktree");
    }

    #[test]
    fn in_progress_git_operation_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        git(
            directory.path(),
            ["init", "--quiet", "--initial-branch=main"],
        );
        let marker = directory.path().join(".git").join("rebase-merge");
        fs::create_dir_all(&marker).unwrap();
        let runner = ProcessRunner::in_directory(directory.path());
        let error = ensure_safe_worktree(
            directory.path(),
            &directory.path().join(".caravan/config.yaml"),
            &runner,
        )
        .unwrap_err();
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "git_operation_in_progress"
        );
    }

    #[test]
    fn navigation_timeout_preserves_timeout_category_and_evidence() {
        let error = command_run_error(&CommandRunError::Timeout {
            command: CommandSpec::new("git").args(["ls-remote", "origin"]),
            process_group_id: None,
            timeout_ms: 750,
            stdout: "partial".to_owned(),
            stderr: "stalled".to_owned(),
        });

        assert_eq!(
            mcp_cli::StructuredError::category(&error),
            ErrorCategory::Timeout
        );
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "navigation_command_timeout"
        );
        let details = mcp_cli::StructuredError::details(&error).unwrap();
        assert_eq!(details["stage"], "navigation");
        assert_eq!(details["timeout_ms"], 750);
    }

    #[test]
    fn checkout_creates_an_exact_local_branch_from_a_clean_clone() {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), ["init", "--quiet", "--initial-branch=main"]);
        git(source.path(), ["config", "user.name", "Caravan Test"]);
        git(
            source.path(),
            ["config", "user.email", "caravan@example.invalid"],
        );
        fs::write(source.path().join("base.txt"), "base\n").unwrap();
        git(source.path(), ["add", "base.txt"]);
        git(source.path(), ["commit", "--quiet", "--message", "base"]);
        let base_oid = git_stdout(source.path(), ["rev-parse", "HEAD"]);
        let branch_name = "dogfood/remote-only";
        git(
            source.path(),
            ["switch", "--quiet", "--create", branch_name],
        );
        fs::write(source.path().join("fixture.txt"), "fixture\n").unwrap();
        git(source.path(), ["add", "fixture.txt"]);
        git(source.path(), ["commit", "--quiet", "--message", "fixture"]);
        let head_oid = git_stdout(source.path(), ["rev-parse", "HEAD"]);
        git(source.path(), ["switch", "--quiet", "main"]);

        let clone_parent = tempfile::tempdir().unwrap();
        let checkout = clone_parent.path().join("checkout");
        let clone = Command::new("git")
            .args([
                "clone",
                "--quiet",
                source.path().to_str().unwrap(),
                checkout.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            clone.status.success(),
            "git clone failed: {}",
            String::from_utf8_lossy(&clone.stderr)
        );
        let missing = Command::new("git")
            .current_dir(&checkout)
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch_name}^{{commit}}"),
            ])
            .output()
            .unwrap();
        assert_eq!(missing.status.code(), Some(1));

        let pull_request = PullRequestSnapshot {
            number: PrNumber(2),
            title: "Remote-only fixture".to_owned(),
            url: "https://example.invalid/2".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: BranchSnapshot {
                repository: repository(),
                name: branch_name.to_owned(),
                oid: CommitOid(head_oid.clone()),
            },
            base: BranchSnapshot {
                repository: repository(),
                name: "main".to_owned(),
                oid: CommitOid(base_oid),
            },
            cross_repository: false,
            labels: BTreeSet::from(["caravan".to_owned()]),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: Some("2026-01-01T00:00:02Z".to_owned()),
            merged_at: None,
            updated_at: None,
        };
        let config = checkout.join(".caravan/config.yaml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "{}\n").unwrap();
        let runner = ProcessRunner::in_directory(&checkout);

        checkout_exact(
            &checkout,
            &checkout.join(".caravan/config.yaml"),
            "origin",
            &runner,
            &pull_request,
        )
        .unwrap();

        assert_eq!(
            git_stdout(&checkout, ["branch", "--show-current"]),
            branch_name
        );
        assert_eq!(git_stdout(&checkout, ["rev-parse", "HEAD"]), head_oid);
        assert_eq!(fs::read_to_string(config).unwrap(), "{}\n");
        assert!(
            fs::read_dir(checkout.join(".caravan"))
                .unwrap()
                .all(|entry| entry.unwrap().file_name() == "config.yaml")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn checkout_preserves_and_advances_a_cara_rewritten_local_destination() {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), ["init", "--quiet", "--initial-branch=main"]);
        git(source.path(), ["config", "user.name", "Caravan Test"]);
        git(
            source.path(),
            ["config", "user.email", "caravan@example.invalid"],
        );
        fs::write(source.path().join("base.txt"), "base\n").unwrap();
        git(source.path(), ["add", "base.txt"]);
        git(source.path(), ["commit", "--quiet", "--message", "base"]);
        let base_oid = git_stdout(source.path(), ["rev-parse", "HEAD"]);
        let branch_name = "stack/rewritten-parent";
        git(
            source.path(),
            ["switch", "--quiet", "--create", branch_name],
        );
        fs::write(source.path().join("fixture.txt"), "old generation\n").unwrap();
        git(source.path(), ["add", "fixture.txt"]);
        git(
            source.path(),
            ["commit", "--quiet", "--message", "old generation"],
        );
        let old_oid = git_stdout(source.path(), ["rev-parse", "HEAD"]);
        git(source.path(), ["switch", "--quiet", "main"]);

        let clone_parent = tempfile::tempdir().unwrap();
        let checkout = clone_parent.path().join("checkout");
        let clone = Command::new("git")
            .args([
                "clone",
                "--quiet",
                source.path().to_str().unwrap(),
                checkout.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            clone.status.success(),
            "git clone failed: {}",
            String::from_utf8_lossy(&clone.stderr)
        );
        git(
            &checkout,
            [
                "switch",
                "--quiet",
                "--track",
                &format!("origin/{branch_name}"),
            ],
        );
        assert_eq!(
            git_stdout(
                &checkout,
                ["rev-parse", &format!("refs/heads/{branch_name}")]
            ),
            old_oid
        );

        git(source.path(), ["switch", "--quiet", branch_name]);
        fs::write(source.path().join("rewritten.txt"), "provider generation\n").unwrap();
        git(source.path(), ["add", "rewritten.txt"]);
        git(
            source.path(),
            ["commit", "--quiet", "--message", "provider rewrite"],
        );
        let provider_oid = git_stdout(source.path(), ["rev-parse", "HEAD"]);

        let pull_request = PullRequestSnapshot {
            number: PrNumber(2),
            title: "Rewritten parent fixture".to_owned(),
            url: "https://example.invalid/2".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: BranchSnapshot {
                repository: repository(),
                name: branch_name.to_owned(),
                oid: CommitOid(provider_oid.clone()),
            },
            base: BranchSnapshot {
                repository: repository(),
                name: "main".to_owned(),
                oid: CommitOid(base_oid),
            },
            cross_repository: false,
            labels: BTreeSet::from(["caravan".to_owned()]),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: Some("2026-01-01T00:00:02Z".to_owned()),
            merged_at: None,
            updated_at: None,
        };
        let config = checkout.join(".caravan/config.yaml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "{}\n").unwrap();
        let runner = ProcessRunner::in_directory(&checkout);

        let current_error = checkout_exact(&checkout, &config, "origin", &runner, &pull_request)
            .expect_err("navigation never rewrites its currently checked-out branch");
        assert_eq!(
            mcp_cli::StructuredError::code(&current_error),
            "current_local_branch_diverged"
        );
        assert_eq!(git_stdout(&checkout, ["rev-parse", "HEAD"]), old_oid);
        assert!(
            git_stdout(
                &checkout,
                [
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/cara-backup/navigation"
                ]
            )
            .is_empty()
        );

        git(&checkout, ["switch", "--quiet", "main"]);
        let reconciliation = checkout_exact(&checkout, &config, "origin", &runner, &pull_request)
            .unwrap()
            .expect("the stale local destination is preserved and advanced");

        assert_eq!(reconciliation.branch, branch_name);
        assert_eq!(reconciliation.previous_oid, CommitOid(old_oid.clone()));
        assert_eq!(reconciliation.provider_oid, CommitOid(provider_oid.clone()));
        assert_eq!(
            git_stdout(&checkout, ["branch", "--show-current"]),
            branch_name
        );
        assert_eq!(git_stdout(&checkout, ["rev-parse", "HEAD"]), provider_oid);
        assert_eq!(
            git_stdout(&checkout, ["rev-parse", &reconciliation.backup_ref]),
            old_oid
        );
        assert_eq!(fs::read_to_string(config).unwrap(), "{}\n");
    }

    fn git_stdout<I, S>(repository: &Path, arguments: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = Command::new("git")
            .current_dir(repository)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn git<I, S>(repository: &Path, arguments: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = Command::new("git")
            .current_dir(repository)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
