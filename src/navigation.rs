//! Safe PR and caravan-fleet navigation without overwriting local work.

use std::path::{Path, PathBuf};

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

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
    ensure_safe_worktree(&context.repository_path, &runner)?;
    let status = read::status(context)?;
    let (from_pr, to_pr) = select_destination(&status, scope, direction)?;
    let pull_request = status
        .analysis
        .pull_requests
        .get(&to_pr)
        .ok_or_else(|| missing_pr(to_pr))?;
    checkout_exact(&context.repository_path, "origin", &runner, pull_request)?;
    lock.release()?;
    Ok(NavigationOutput {
        repository: status.repository,
        scope,
        direction,
        from_pr,
        to_pr,
        branch: pull_request.head.name.clone(),
        oid: pull_request.head.oid.clone(),
    })
}

/// Pure destination selection for fixture testing.
pub fn select_destination(
    status: &StatusOutput,
    scope: Scope,
    direction: Direction,
) -> Result<(Option<PrNumber>, PrNumber), AppError> {
    let Some(current) = status.current_pr else {
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
        return Ok((None, destination));
    };
    let current_caravan = status.analysis.fleet.containing(current).ok_or_else(|| {
        AppError::validation(
            "current_pr_not_in_caravan",
            format!("PR #{current} is not an active caravan member"),
        )
    })?;

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
        |destination| Ok((Some(current), destination)),
    )
}

/// Refuse dirty worktrees and in-progress Git operations before changing HEAD.
pub fn ensure_safe_worktree(
    repository: &Path,
    runner: &impl CommandRunner,
) -> Result<(), AppError> {
    let status = run(
        runner,
        CommandSpec::new("git").args(["status", "--porcelain=v1", "--untracked-files=normal"]),
    )?;
    require_success(
        "git_status_failed",
        "could not inspect worktree status",
        &status,
    )?;
    if !status.stdout.is_empty() {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "dirty_worktree",
            "refusing to switch branches because the worktree has tracked or untracked changes",
            Some(json!({ "status": bounded(&status.stdout) })),
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

/// Check out exactly the discovered PR head, refusing to overwrite a local branch.
///
/// Validation, ref creation, switching, and post-verification intentionally stay
/// in one transaction-shaped function so no caller can omit a safety phase.
#[allow(clippy::too_many_lines)]
pub fn checkout_exact(
    _repository: &Path,
    remote: &str,
    runner: &impl CommandRunner,
    pull_request: &PullRequestSnapshot,
) -> Result<(), AppError> {
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
    let advertised = run(
        runner,
        CommandSpec::new("git").args([
            "ls-remote",
            "--refs",
            "--exit-code",
            remote,
            reference.as_str(),
        ]),
    )?;
    require_success(
        "remote_ref_not_found",
        "the PR head branch is not advertised by the remote",
        &advertised,
    )?;
    let advertised_oid = advertised
        .stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find_map(|(oid, found)| (found == reference).then_some(oid))
        .ok_or_else(|| {
            AppError::structured(
                ErrorCategory::ExecutionFailure,
                "ls_remote_output_invalid",
                "git ls-remote omitted the requested PR branch",
                Some(json!({ "reference": reference })),
            )
        })?;
    if advertised_oid != expected {
        return Err(stale_head(pull_request, advertised_oid));
    }

    let local_commit = format!("{reference}^{{commit}}");
    let local = run(
        runner,
        CommandSpec::new("git").args(["rev-parse", "--verify", "--quiet", local_commit.as_str()]),
    )?;
    match local.code {
        Some(0) => {
            let local_oid = local.stdout.trim();
            if local_oid != expected {
                return Err(AppError::structured(
                    ErrorCategory::Validation,
                    "local_branch_diverged",
                    format!("local branch `{branch}` does not equal the discovered PR head"),
                    Some(json!({
                        "branch": branch,
                        "expected_oid": expected,
                        "local_oid": local_oid,
                    })),
                ));
            }
        }
        Some(1) => {
            let refspec = format!("{reference}:{reference}");
            let fetch = run(
                runner,
                CommandSpec::new("git").args([
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    remote,
                    refspec.as_str(),
                ]),
            )?;
            require_success(
                "checkout_fetch_failed",
                "could not create the exact local PR branch",
                &fetch,
            )?;
        }
        _ => {
            return Err(command_failure(
                "local_branch_inspection_failed",
                "could not inspect the local PR branch",
                &local,
            ));
        }
    }

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

    let switch = run(
        runner,
        CommandSpec::new("git").args(["switch", "--quiet", branch]),
    )?;
    require_success(
        "branch_switch_failed",
        "Git refused to switch to the exact PR branch",
        &switch,
    )?;
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
    Ok(())
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
            repository: repository(),
            default_branch: "main".to_owned(),
            current_branch: Some(format!("pr-{current}")),
            current_pr: Some(PrNumber(current)),
            healthy: true,
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
        let error = ensure_safe_worktree(directory.path(), &runner).unwrap_err();
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
        let error = ensure_safe_worktree(directory.path(), &runner).unwrap_err();
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "git_operation_in_progress"
        );
    }

    #[test]
    fn navigation_timeout_preserves_timeout_category_and_evidence() {
        let error = command_run_error(&CommandRunError::Timeout {
            command: CommandSpec::new("git").args(["ls-remote", "origin"]),
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
            merged_at: None,
            updated_at: None,
        };
        let runner = ProcessRunner::in_directory(&checkout);

        checkout_exact(&checkout, "origin", &runner, &pull_request).unwrap();

        assert_eq!(
            git_stdout(&checkout, ["branch", "--show-current"]),
            branch_name
        );
        assert_eq!(git_stdout(&checkout, ["rev-parse", "HEAD"]), head_oid);
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
