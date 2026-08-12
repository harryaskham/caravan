//! Authoritative default-branch materialization for sync.
//!
//! `cara sync` is a repository service. The branch from which an operator
//! invokes it is not policy authority and must never be reset merely so sync can
//! read `.caravan/config.yaml` or execute a repository-relative hook. This
//! module performs one bounded fetch, pins the exact remote-default commit, and
//! creates a detached temporary Git worktree sharing the original common Git
//! directory. Provider/Git reads and hooks then run from that immutable source
//! snapshot while locks, journals, and checkpoints remain shared.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mcp_cli::ErrorCategory;
use serde_json::json;

use crate::command::{CommandRunner, CommandSpec, ProcessRunner};
use crate::config::{CaravanConfig, DEFAULT_CONFIG_PATH};
use crate::{AppContext, AppError};

const FETCH_TIMEOUT_SECS: u64 = 120;
// Local shared-object worktree registration can contend with long-running Git
// readers even when provider commands should retain a short timeout. Keep this
// floor separate from network/API command tuning (bd-4dae44).
const LOCAL_GIT_TIMEOUT_FLOOR_SECS: u64 = 30;
const MATERIALIZATION_TIMEOUT_FLOOR_SECS: u64 = 120;
const WORKTREE_PREFIX: &str = "cara-authoritative-sync";

/// One exact fetched default-branch generation. Revalidate immediately before
/// provider mutation so a moving default branch never silently changes the
/// policy generation under a tick.
#[derive(Debug)]
pub(crate) struct DefaultBranchAuthority {
    repository: PathBuf,
    default_ref: String,
    oid: String,
    invocation_branch: Option<String>,
    invocation_head: String,
    local_timeout: Duration,
}

impl DefaultBranchAuthority {
    /// Restore the caller's exact open-PR identity after discovery runs from the
    /// detached authoritative worktree. `sync --all` does not need it, but a
    /// targeted `sync` must retain the meaning of the branch from which it was
    /// invoked without checking that branch out in the operation worktree.
    pub(crate) fn bind_invocation(
        &self,
        status: &mut crate::read::StatusOutput,
    ) -> Result<(), AppError> {
        status.current_branch.clone_from(&self.invocation_branch);
        let Some(branch) = self.invocation_branch.as_deref() else {
            status.current_pr = None;
            return Ok(());
        };
        let matching = status
            .analysis
            .pull_requests
            .values()
            .filter(|pull| pull.head.name == branch && pull.head.oid.0 == self.invocation_head)
            .map(|pull| pull.number)
            .collect::<Vec<_>>();
        match matching.as_slice() {
            [] => {
                status.current_pr = None;
                Ok(())
            }
            [current] => {
                status.current_pr = Some(*current);
                Ok(())
            }
            _ => Err(authority_error(
                "sync_invocation_pr_ambiguous",
                "the invoking branch/head maps to multiple open pull requests",
                json!({
                    "current_branch": branch,
                    "current_head": self.invocation_head,
                    "pull_requests": matching,
                    "mutated": false,
                    "provider_mutations": 0,
                }),
            )),
        }
    }

    pub(crate) fn revalidate(&self) -> Result<(), AppError> {
        let observed = git_value(
            &self.repository,
            &[
                "rev-parse",
                "--verify",
                &format!("{}^{{commit}}", self.default_ref),
            ],
            self.local_timeout,
            "sync_default_branch_revalidation_failed",
            "could not re-read the fetched default-branch generation",
        )?;
        if observed != self.oid {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "sync_default_branch_moved",
                "the remote-tracking default branch moved after sync materialized its policy",
                Some(json!({
                    "default_branch_ref": self.default_ref,
                    "expected_oid": self.oid,
                    "observed_oid": observed,
                    "mutated": false,
                    "provider_mutations": 0,
                    "retryable": true,
                    "safe_next_action": "rerun the same sync so Cara fetches and materializes one fresh authoritative generation",
                })),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn default_ref(&self) -> &str {
        &self.default_ref
    }
}

/// Context retained for one plan/tick. Dropping it removes only the detached
/// worktree this operation created; the caller checkout is never touched.
pub(crate) struct PreparedSyncContext {
    context: AppContext,
    authority: Option<DefaultBranchAuthority>,
    materialized: Option<MaterializedWorktree>,
}

impl PreparedSyncContext {
    pub(crate) fn context(&self) -> &AppContext {
        debug_assert_eq!(self.materialized.is_some(), self.authority.is_some());
        &self.context
    }

    pub(crate) fn authority(&self) -> Option<&DefaultBranchAuthority> {
        self.authority.as_ref()
    }

    #[cfg(test)]
    fn materialized_path(&self) -> Option<&Path> {
        self.materialized
            .as_ref()
            .map(|worktree| worktree.path.as_path())
    }
}

/// Fetch and materialize authoritative policy unless an explicit config or a
/// reviewed opt-out says this invocation owns its policy directly.
pub(crate) fn prepare(context: &AppContext) -> Result<PreparedSyncContext, AppError> {
    if context.config_path.is_absolute() {
        return Ok(unmaterialized(context.clone()));
    }
    if !context.config.sync.allow_fetch {
        let local = AppContext::load_from_directory(&context.repository_path, None)
            .map_err(|error| config_error_for_sync(&error))?;
        return Ok(unmaterialized(local));
    }

    let timeout = Duration::from_secs(
        context
            .config
            .command_timeout_secs
            .clamp(5, FETCH_TIMEOUT_SECS),
    );
    let overall_sync_budget = Duration::from_secs(context.config.sync.max_duration_secs);
    let local_timeout = authoritative_local_git_timeout(timeout, overall_sync_budget);
    let materialization_timeout = authoritative_worktree_timeout(timeout, overall_sync_budget);
    let repository = context.repository_path.clone();
    let invocation_branch =
        optional_git_value(&repository, &["branch", "--show-current"], local_timeout)?;
    let invocation_head = git_value(
        &repository,
        &["rev-parse", "--verify", "HEAD^{commit}"],
        local_timeout,
        "sync_invocation_head_missing",
        "the invoking checkout HEAD does not resolve to one commit",
    )?;
    let default_ref = resolve_default_ref(&repository, timeout, local_timeout)?;

    // A locally observed explicit opt-out must be honoured before network I/O.
    // Missing/older policy has the new safe default: fetch is allowed.
    if local_default_policy(&repository, &default_ref, local_timeout)?
        .is_some_and(|config| !config.sync.allow_fetch)
    {
        let mut local = AppContext::load_from_directory(&repository, None)
            .map_err(|error| config_error_for_sync(&error))?;
        local.config.sync.allow_fetch = false;
        return Ok(unmaterialized(local));
    }

    materialize_fetched_default(
        repository,
        default_ref,
        invocation_branch,
        invocation_head,
        timeout,
        local_timeout,
        materialization_timeout,
    )
}

fn authoritative_local_git_timeout(
    command_timeout: Duration,
    overall_sync_budget: Duration,
) -> Duration {
    let seconds = command_timeout
        .as_secs()
        .clamp(LOCAL_GIT_TIMEOUT_FLOOR_SECS, FETCH_TIMEOUT_SECS)
        .min(overall_sync_budget.as_secs().max(1));
    Duration::from_secs(seconds)
}

fn authoritative_worktree_timeout(
    command_timeout: Duration,
    overall_sync_budget: Duration,
) -> Duration {
    let seconds = command_timeout
        .as_secs()
        .clamp(MATERIALIZATION_TIMEOUT_FLOOR_SECS, FETCH_TIMEOUT_SECS)
        .min(overall_sync_budget.as_secs().max(1));
    Duration::from_secs(seconds)
}

fn unmaterialized(context: AppContext) -> PreparedSyncContext {
    PreparedSyncContext {
        context,
        authority: None,
        materialized: None,
    }
}

fn materialize_fetched_default(
    repository: PathBuf,
    default_ref: String,
    invocation_branch: Option<String>,
    invocation_head: String,
    timeout: Duration,
    local_timeout: Duration,
    materialization_timeout: Duration,
) -> Result<PreparedSyncContext, AppError> {
    let branch = default_ref.strip_prefix("origin/").ok_or_else(|| {
        authority_error(
            "sync_default_branch_ref_invalid",
            "the recorded default branch is not an origin remote-tracking ref",
            json!({"default_branch_ref": default_ref, "mutated": false}),
        )
    })?;
    require_branch_name(&repository, branch, local_timeout)?;
    let refspec = format!("refs/heads/{branch}:refs/remotes/origin/{branch}");
    crate::sync::progress::emit(
        "policy_fetch",
        format!("fetching exact authoritative {default_ref} without changing the invoking branch"),
    );
    git_success(
        &repository,
        &["fetch", "--no-tags", "origin", &refspec],
        timeout,
        "sync_default_branch_fetch_failed",
        "could not fetch the exact remote default branch for sync",
    )?;
    let oid = git_value(
        &repository,
        &[
            "rev-parse",
            "--verify",
            &format!("{default_ref}^{{commit}}"),
        ],
        local_timeout,
        "sync_default_branch_oid_missing",
        "the fetched default branch does not resolve to a commit",
    )?;

    let path = std::env::temp_dir().join(format!(
        "{WORKTREE_PREFIX}-{}-{}",
        std::process::id(),
        uuid::Uuid::now_v7()
    ));
    let materialized =
        MaterializedWorktree::create(&repository, path, &oid, materialization_timeout)?;
    let loaded = AppContext::load_from_directory(&materialized.path, None)
        .map_err(|error| config_error_for_sync(&error))?;
    crate::sync::progress::emit(
        "policy_fetch",
        format!("materialized {default_ref}@{oid} in a detached Cara-owned worktree"),
    );
    if !loaded.config.sync.allow_fetch {
        return Err(authority_error(
            "sync_default_branch_fetch_disabled",
            "the freshly fetched authoritative policy disables sync fetching",
            json!({
                "default_branch_ref": default_ref,
                "default_branch_oid": oid,
                "mutated": false,
                "provider_mutations": 0,
                "safe_next_action": "invoke with CARA_ALLOW_FETCH=true for this run or enable sync.allow_fetch on the default branch",
            }),
        ));
    }

    Ok(PreparedSyncContext {
        context: loaded,
        authority: Some(DefaultBranchAuthority {
            repository,
            default_ref,
            oid,
            invocation_branch,
            invocation_head,
            local_timeout,
        }),
        materialized: Some(materialized),
    })
}

fn resolve_default_ref(
    repository: &Path,
    timeout: Duration,
    local_timeout: Duration,
) -> Result<String, AppError> {
    if let Ok(reference) = git_value(
        repository,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        local_timeout,
        "sync_default_branch_unknown",
        "could not read the recorded origin default branch",
    ) {
        return Ok(reference);
    }

    let output = git_output(
        repository,
        &["ls-remote", "--symref", "origin", "HEAD"],
        timeout,
    )?;
    if !output.is_success() {
        return Err(command_failure(
            "sync_default_branch_unknown",
            "origin did not report its symbolic default branch",
            &output,
            json!({"mutated": false, "provider_mutations": 0}),
        ));
    }
    let branch = output
        .stdout
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some("ref:"))
                .then(|| fields.next())
                .flatten()
                .and_then(|reference| reference.strip_prefix("refs/heads/"))
        })
        .filter(|branch| !branch.is_empty())
        .ok_or_else(|| {
            authority_error(
                "sync_default_branch_unknown",
                "origin HEAD did not contain one exact refs/heads target",
                json!({"mutated": false, "provider_mutations": 0}),
            )
        })?;
    Ok(format!("origin/{branch}"))
}

fn local_default_policy(
    repository: &Path,
    default_ref: &str,
    timeout: Duration,
) -> Result<Option<CaravanConfig>, AppError> {
    let spec = format!("{default_ref}:{DEFAULT_CONFIG_PATH}");
    let output = git_output(repository, &["show", &spec], timeout)?;
    if !output.is_success() {
        return Ok(None);
    }
    CaravanConfig::parse(&output.stdout)
        .map(Some)
        .map_err(|error| config_error_for_sync(&error))
}

fn require_branch_name(repository: &Path, branch: &str, timeout: Duration) -> Result<(), AppError> {
    git_success(
        repository,
        &["check-ref-format", "--branch", branch],
        timeout,
        "sync_default_branch_ref_invalid",
        "the recorded origin default branch is not a valid branch name",
    )
}

#[derive(Debug)]
struct MaterializedWorktree {
    repository: PathBuf,
    path: PathBuf,
}

impl MaterializedWorktree {
    fn create(
        repository: &Path,
        path: PathBuf,
        oid: &str,
        timeout: Duration,
    ) -> Result<Self, AppError> {
        let path_text = path.to_string_lossy().into_owned();
        let result = git_success(
            repository,
            &["worktree", "add", "--detach", &path_text, oid],
            timeout,
            "sync_default_branch_materialization_failed",
            "could not create a detached authoritative sync worktree",
        );
        if let Err(error) = result {
            let _ = std::fs::remove_dir_all(&path);
            return Err(error);
        }
        Ok(Self {
            repository: repository.to_path_buf(),
            path,
        })
    }
}

impl Drop for MaterializedWorktree {
    fn drop(&mut self) {
        let path = self.path.to_string_lossy().into_owned();
        let runner =
            ProcessRunner::in_directory(&self.repository).with_timeout(Duration::from_secs(10));
        let _ = runner.run(&CommandSpec::new("git").args([
            "-c",
            "core.hooksPath=/dev/null",
            "worktree",
            "remove",
            "--force",
            &path,
        ]));
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn optional_git_value(
    repository: &Path,
    arguments: &[&str],
    timeout: Duration,
) -> Result<Option<String>, AppError> {
    let output = git_output(repository, arguments, timeout)?;
    if !output.is_success() {
        return Err(command_failure(
            "sync_invocation_identity_failed",
            "could not inspect the invoking checkout identity",
            &output,
            json!({"mutated": false, "provider_mutations": 0}),
        ));
    }
    let value = output.stdout.trim();
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

fn git_value(
    repository: &Path,
    arguments: &[&str],
    timeout: Duration,
    code: &str,
    message: &str,
) -> Result<String, AppError> {
    let output = git_output(repository, arguments, timeout)?;
    if !output.is_success() {
        return Err(command_failure(
            code,
            message,
            &output,
            json!({"mutated": false, "provider_mutations": 0}),
        ));
    }
    let value = output.stdout.trim();
    if value.is_empty() {
        return Err(authority_error(
            code,
            message,
            json!({"mutated": false, "provider_mutations": 0}),
        ));
    }
    Ok(value.to_owned())
}

fn git_success(
    repository: &Path,
    arguments: &[&str],
    timeout: Duration,
    code: &str,
    message: &str,
) -> Result<(), AppError> {
    let output = git_output(repository, arguments, timeout)?;
    if output.is_success() {
        return Ok(());
    }
    Err(command_failure(
        code,
        message,
        &output,
        json!({"mutated": false, "provider_mutations": 0}),
    ))
}

fn git_output(
    repository: &Path,
    arguments: &[&str],
    timeout: Duration,
) -> Result<crate::command::CommandOutput, AppError> {
    ProcessRunner::in_directory(repository)
        .with_timeout(timeout)
        .run(
            &CommandSpec::new("git")
                .args(["-c", "core.hooksPath=/dev/null"])
                .args(arguments.iter().copied()),
        )
        .map_err(|error| {
            authority_error(
                "sync_default_branch_git_failed",
                "a bounded local default-branch Git operation did not complete",
                json!({
                    "source": error.to_string(),
                    "mutated": false,
                    "provider_mutations": 0,
                }),
            )
        })
}

fn command_failure(
    code: &str,
    message: &str,
    output: &crate::command::CommandOutput,
    mut details: serde_json::Value,
) -> AppError {
    if let Some(object) = details.as_object_mut() {
        object.insert("git_exit".to_owned(), json!(output.code));
        object.insert("diagnostic".to_owned(), json!(output.stderr.trim()));
    }
    authority_error(code, message, details)
}

fn config_error_for_sync(error: &crate::config::ConfigError) -> AppError {
    use mcp_cli::StructuredError as _;
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        error.details(),
    )
}

fn authority_error(code: &str, message: &str, details: serde_json::Value) -> AppError {
    AppError::structured(ErrorCategory::Validation, code, message, Some(details))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(repository: &Path, arguments: &[&str]) {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(repository)
            .status()
            .unwrap();
        assert!(status.success(), "git {arguments:?}");
    }

    fn output(repository: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(repository)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {arguments:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn write_policy(repository: &Path, max_caravans: u32, allow_fetch: bool) {
        fs::create_dir_all(repository.join(".caravan")).unwrap();
        fs::write(
            repository.join(".caravan/config.yaml"),
            format!(
                "version: 1\nmin_cara_version: \"{}\"\nsync:\n  allow_fetch: {allow_fetch}\n  max_caravans: {max_caravans}\n",
                env!("CARGO_PKG_VERSION")
            ),
        )
        .unwrap();
    }

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let remote = root.path().join("remote.git");
        let checkout = root.path().join("checkout");
        git(root.path(), &["init", "--bare", remote.to_str().unwrap()]);
        git(
            root.path(),
            &[
                "clone",
                remote.to_str().unwrap(),
                checkout.to_str().unwrap(),
            ],
        );
        git(&checkout, &["config", "user.name", "Cara Test"]);
        git(&checkout, &["config", "user.email", "cara@example.test"]);
        git(&checkout, &["checkout", "-b", "main"]);
        write_policy(&checkout, 1, true);
        fs::write(checkout.join("hook.sh"), "old\n").unwrap();
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "-m", "initial"]);
        git(&checkout, &["push", "-u", "origin", "main"]);
        git(&checkout, &["remote", "set-head", "origin", "main"]);
        (root, checkout)
    }

    #[test]
    fn authoritative_worktree_timeout_has_independent_floor_and_cap() {
        assert_eq!(
            authoritative_local_git_timeout(Duration::from_secs(5), Duration::from_secs(480)),
            Duration::from_secs(30)
        );
        assert_eq!(
            authoritative_local_git_timeout(Duration::from_secs(90), Duration::from_secs(480)),
            Duration::from_secs(90)
        );
        assert_eq!(
            authoritative_local_git_timeout(Duration::from_secs(300), Duration::from_secs(480)),
            Duration::from_secs(120)
        );
        assert_eq!(
            authoritative_local_git_timeout(Duration::from_secs(5), Duration::from_secs(20)),
            Duration::from_secs(20)
        );
        assert_eq!(
            authoritative_worktree_timeout(Duration::from_secs(30), Duration::from_secs(480)),
            Duration::from_secs(120)
        );
        assert_eq!(
            authoritative_worktree_timeout(Duration::from_secs(90), Duration::from_secs(480)),
            Duration::from_secs(120)
        );
        assert_eq!(
            authoritative_worktree_timeout(Duration::from_secs(300), Duration::from_secs(480)),
            Duration::from_secs(120)
        );
        assert_eq!(
            authoritative_worktree_timeout(Duration::from_secs(30), Duration::from_secs(45)),
            Duration::from_secs(45),
            "the local floor never exceeds the whole sync budget"
        );
    }

    #[test]
    fn failed_authoritative_worktree_creation_removes_partial_path() {
        let (_root, checkout) = fixture();
        let path = checkout.parent().unwrap().join("failed-materialization");

        let error = MaterializedWorktree::create(
            &checkout,
            path.clone(),
            "not-a-commit",
            Duration::from_secs(60),
        )
        .unwrap_err();

        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "sync_default_branch_materialization_failed"
        );
        assert!(!path.exists());
    }

    #[test]
    fn dirty_old_branch_uses_fetched_default_without_touching_caller() {
        let (_root, checkout) = fixture();
        let old_head = output(&checkout, &["rev-parse", "HEAD"]);
        git(&checkout, &["checkout", "-b", "feature"]);
        git(&checkout, &["checkout", "main"]);
        write_policy(&checkout, 3, true);
        fs::write(checkout.join("hook.sh"), "authoritative\n").unwrap();
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "-m", "new policy"]);
        git(&checkout, &["push", "origin", "main"]);
        git(&checkout, &["checkout", "feature"]);
        fs::write(checkout.join("hook.sh"), "caller dirty tracked content\n").unwrap();
        fs::write(checkout.join("caller-untracked"), "preserve me\n").unwrap();
        let before_status = output(&checkout, &["status", "--porcelain=v1"]);
        let before_head = output(&checkout, &["rev-parse", "HEAD"]);
        assert_eq!(before_head, old_head);

        let context = AppContext::load_from_directory(&checkout, None).unwrap();
        let prepared = prepare(&context).unwrap();
        assert_eq!(prepared.context().config.sync.max_caravans, 3);
        assert_eq!(
            fs::read_to_string(prepared.context().repository_path.join("hook.sh")).unwrap(),
            "authoritative\n"
        );
        assert_ne!(prepared.context().repository_path, checkout);
        assert!(prepared.authority().is_some());
        prepared.authority().unwrap().revalidate().unwrap();
        let materialized = prepared.materialized_path().unwrap().to_path_buf();
        drop(prepared);

        assert!(!materialized.exists());
        assert!(
            !output(&checkout, &["worktree", "list", "--porcelain"])
                .contains(materialized.to_string_lossy().as_ref()),
            "Cara-owned worktree registration is removed on drop"
        );
        assert_eq!(output(&checkout, &["rev-parse", "HEAD"]), before_head);
        assert_eq!(
            output(&checkout, &["status", "--porcelain=v1"]),
            before_status
        );
    }

    #[test]
    fn sync_bootstrap_ignores_malformed_branch_local_policy() {
        let (_root, checkout) = fixture();
        git(&checkout, &["checkout", "-b", "malformed-proposal"]);
        fs::write(
            checkout.join(".caravan/config.yaml"),
            "version: [not valid policy\n",
        )
        .unwrap();
        assert!(
            AppContext::load_from_directory(&checkout, None).is_err(),
            "ordinary branch-local loading still reports malformed policy"
        );

        let seed = AppContext::load_for_sync_from_directory(&checkout, None).unwrap();
        let prepared = prepare(&seed).unwrap();

        assert_eq!(prepared.context().config.sync.max_caravans, 1);
        assert!(prepared.authority().is_some());
    }

    #[test]
    fn linked_invocation_worktree_keeps_its_branch_and_shared_lock_domain() {
        let (root, checkout) = fixture();
        let linked = root.path().join("linked");
        git(
            &checkout,
            &[
                "worktree",
                "add",
                "-b",
                "linked-feature",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        );
        let caller_head = output(&linked, &["rev-parse", "HEAD"]);
        fs::write(linked.join("caller-untracked"), "keep\n").unwrap();
        let caller_status = output(&linked, &["status", "--porcelain=v1"]);
        let caller_common = output(
            &linked,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        );

        let context = AppContext::load_from_directory(&linked, None).unwrap();
        let prepared = prepare(&context).unwrap();
        let prepared_common = output(
            &prepared.context().repository_path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        );

        assert_eq!(prepared_common, caller_common);
        drop(prepared);
        assert_eq!(output(&linked, &["rev-parse", "HEAD"]), caller_head);
        assert_eq!(
            output(&linked, &["status", "--porcelain=v1"]),
            caller_status
        );
    }

    #[test]
    fn explicit_config_and_disabled_fetch_leave_context_unmaterialized() {
        let (_root, checkout) = fixture();
        let explicit = checkout.join("explicit.yaml");
        write_policy(&checkout, 1, false);
        fs::copy(checkout.join(".caravan/config.yaml"), &explicit).unwrap();
        let explicit_context = AppContext::load_from_directory(&checkout, Some(&explicit)).unwrap();
        let prepared = prepare(&explicit_context).unwrap();
        assert!(prepared.authority().is_none());
        assert_eq!(
            prepared.context().repository_path,
            std::fs::canonicalize(&checkout).unwrap()
        );

        let context = AppContext::load_from_directory(&checkout, None).unwrap();
        let prepared = prepare(&context).unwrap();
        assert!(prepared.authority().is_none());
        assert_eq!(
            prepared.context().repository_path,
            std::fs::canonicalize(&checkout).unwrap()
        );
    }

    #[test]
    fn default_movement_after_materialization_refuses() {
        let (_root, checkout) = fixture();
        let context = AppContext::load_from_directory(&checkout, None).unwrap();
        let prepared = prepare(&context).unwrap();
        let authority = prepared.authority().unwrap();
        let original = output(&checkout, &["rev-parse", authority.default_ref()]);
        git(
            &checkout,
            &["commit", "--allow-empty", "-m", "concurrent movement"],
        );
        git(&checkout, &["update-ref", authority.default_ref(), "HEAD"]);

        let error = authority.revalidate().unwrap_err();

        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "sync_default_branch_moved"
        );
        git(
            &checkout,
            &["update-ref", authority.default_ref(), &original],
        );
    }
}
