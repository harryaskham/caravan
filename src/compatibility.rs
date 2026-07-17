//! Worktree-free Git compatibility checks.
//!
//! GitHub discovery supplies canonical [`BranchSnapshot`] values. This module
//! fetches and verifies those exact revisions, then asks `git merge-tree` to
//! construct merge evidence without checking out or rewriting either branch.

use std::path::Path;
use std::process::{Command, Output};

use mcp_cli::ErrorCategory;
use serde_json::json;

use crate::AppError;
use crate::model::{BranchSnapshot, CommitOid, CompatibilityOutcome, CompatibilityReport};

/// Check a child PR against its declared predecessor.
pub fn check_adjacent(
    repository: impl AsRef<Path>,
    remote: &str,
    child: &BranchSnapshot,
    predecessor: &BranchSnapshot,
) -> Result<CompatibilityReport, AppError> {
    check_compatibility(repository, remote, child, predecessor)
}

/// Check a caravan head against the current default branch.
pub fn check_head_to_default(
    repository: impl AsRef<Path>,
    remote: &str,
    head: &BranchSnapshot,
    default_branch: &BranchSnapshot,
) -> Result<CompatibilityReport, AppError> {
    check_compatibility(repository, remote, head, default_branch)
}

/// Check the ordered attachment of one caravan head after another caravan's tail.
pub fn check_cross_caravan(
    repository: impl AsRef<Path>,
    remote: &str,
    other_head: &BranchSnapshot,
    tail: &BranchSnapshot,
) -> Result<CompatibilityReport, AppError> {
    check_compatibility(repository, remote, other_head, tail)
}

/// Fetch exact candidate/target revisions and construct their merge without touching the worktree.
pub fn check_compatibility(
    repository: impl AsRef<Path>,
    remote: &str,
    candidate: &BranchSnapshot,
    target: &BranchSnapshot,
) -> Result<CompatibilityReport, AppError> {
    if candidate.repository != target.repository {
        return Err(AppError::validation(
            "cross_repository_compatibility_unsupported",
            "Caravan v1 compatibility checks require candidate and target branches in one repository",
        ));
    }

    let repository = repository.as_ref();
    let target_oid = resolve_branch_snapshot(repository, remote, target)?;
    let candidate_oid = resolve_branch_snapshot(repository, remote, candidate)?;
    let output = git_output(
        repository,
        [
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            "-z",
            target_oid.0.as_str(),
            candidate_oid.0.as_str(),
        ],
    )?;
    let outcome = match output.status.code() {
        Some(0) => CompatibilityOutcome::Clean,
        Some(1) => CompatibilityOutcome::Conflict,
        code => {
            return Err(git_failure(
                "merge_tree_failed",
                "git merge-tree could not construct compatibility evidence",
                code,
                &output,
            ));
        }
    };

    let mut fields = output.stdout.split(|byte| *byte == 0);
    let merge_tree_oid = fields
        .next()
        .filter(|field| !field.is_empty())
        .map(bytes_to_text)
        .ok_or_else(|| {
            malformed_git_output(
                "merge_tree_output_invalid",
                "git merge-tree did not return a result tree",
                &output,
            )
        })?;
    if !valid_full_oid(&merge_tree_oid) {
        return Err(malformed_git_output(
            "merge_tree_output_invalid",
            "git merge-tree returned an invalid result tree object ID",
            &output,
        ));
    }

    let conflicting_paths = fields
        .filter(|field| !field.is_empty())
        .map(bytes_to_text)
        .collect::<Vec<_>>();
    if outcome == CompatibilityOutcome::Clean && !conflicting_paths.is_empty() {
        return Err(malformed_git_output(
            "merge_tree_output_invalid",
            "git merge-tree reported conflict paths with a clean exit status",
            &output,
        ));
    }

    Ok(CompatibilityReport {
        candidate: candidate.clone(),
        target: target.clone(),
        outcome,
        conflicting_paths,
        diagnostic: Some(format!("git merge-tree result {merge_tree_oid}")),
    })
}

/// Fetch and verify one canonical branch snapshot without updating local refs.
pub fn resolve_branch_snapshot(
    repository: impl AsRef<Path>,
    remote: &str,
    snapshot: &BranchSnapshot,
) -> Result<CommitOid, AppError> {
    let repository = repository.as_ref();
    validate_remote(remote)?;
    validate_expected_oid(&snapshot.oid.0)?;
    let reference = branch_reference(repository, &snapshot.name)?;
    require_advertised_oid(repository, remote, &reference, &snapshot.oid.0)?;

    let fetch = git_output(
        repository,
        [
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--refmap=",
            remote,
            reference.as_str(),
        ],
    )?;
    if !fetch.status.success() {
        return Err(git_failure(
            "git_fetch_failed",
            "Git could not fetch the requested exact remote ref",
            fetch.status.code(),
            &fetch,
        ));
    }

    // A branch may move between discovery and fetch. Rechecking prevents a
    // caller from silently validating a newer remote head than it requested.
    require_advertised_oid(repository, remote, &reference, &snapshot.oid.0)?;
    resolve_local_commit(repository, &snapshot.oid.0)
}

fn validate_remote(remote: &str) -> Result<(), AppError> {
    if remote.trim().is_empty() || remote.starts_with('-') || contains_line_break(remote) {
        return Err(AppError::validation(
            "invalid_remote",
            "a Git remote must be non-empty, single-line, and must not begin with '-'",
        ));
    }
    Ok(())
}

fn validate_expected_oid(expected_oid: &str) -> Result<(), AppError> {
    if !valid_full_oid(expected_oid) {
        return Err(AppError::validation(
            "invalid_expected_oid",
            "an expected Git object ID must be a full 40- or 64-digit hexadecimal value",
        ));
    }
    Ok(())
}

fn branch_reference(repository: &Path, branch: &str) -> Result<String, AppError> {
    if branch.trim().is_empty() || contains_line_break(branch) {
        return Err(AppError::validation(
            "invalid_branch_name",
            "a branch name must be non-empty and single-line",
        ));
    }
    let reference = format!("refs/heads/{branch}");
    let output = git_output(repository, ["check-ref-format", reference.as_str()])?;
    if !output.status.success() {
        return Err(AppError::validation(
            "invalid_branch_name",
            format!("`{branch}` is not a valid Git branch name"),
        ));
    }
    Ok(reference)
}

fn require_advertised_oid(
    repository: &Path,
    remote: &str,
    reference: &str,
    expected_oid: &str,
) -> Result<(), AppError> {
    let output = git_output(
        repository,
        ["ls-remote", "--refs", "--exit-code", remote, reference],
    )?;
    if !output.status.success() {
        return Err(AppError::structured(
            ErrorCategory::TargetNotFound,
            "remote_ref_not_found",
            format!("remote ref `{reference}` is not advertised by `{remote}`"),
            Some(json!({
                "remote": remote,
                "reference": reference,
                "stderr": bounded_text(&output.stderr),
            })),
        ));
    }

    let advertised = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find_map(|(oid, found_ref)| (found_ref == reference).then(|| oid.to_owned()))
        .ok_or_else(|| {
            malformed_git_output(
                "ls_remote_output_invalid",
                "git ls-remote succeeded without returning the requested ref",
                &output,
            )
        })?;
    if advertised != expected_oid {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "stale_remote_revision",
            format!("remote ref `{reference}` moved since discovery"),
            Some(json!({
                "remote": remote,
                "reference": reference,
                "expected_oid": expected_oid,
                "advertised_oid": advertised,
                "resumable": true,
                "next": "rediscover the GitHub graph and retry with its new exact revision",
            })),
        ));
    }
    Ok(())
}

fn resolve_local_commit(repository: &Path, expected_oid: &str) -> Result<CommitOid, AppError> {
    let commit_expression = format!("{expected_oid}^{{commit}}");
    let output = git_output(
        repository,
        [
            "rev-parse",
            "--verify",
            "--end-of-options",
            commit_expression.as_str(),
        ],
    )?;
    if !output.status.success() {
        return Err(AppError::structured(
            ErrorCategory::TargetNotFound,
            "revision_not_found",
            format!("Git object `{expected_oid}` does not resolve to a commit"),
            Some(json!({
                "expected_oid": expected_oid,
                "stderr": bounded_text(&output.stderr),
            })),
        ));
    }
    let oid = parse_single_oid("git rev-parse", &output)?;
    if oid != expected_oid {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "resolved_revision_mismatch",
            "Git resolved a different commit than the canonical branch snapshot",
            Some(json!({
                "expected_oid": expected_oid,
                "resolved_oid": oid,
            })),
        ));
    }
    Ok(CommitOid(oid))
}

fn git_output<I, S>(repository: &Path, arguments: I) -> Result<Output, AppError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    Command::new("git")
        .current_dir(repository)
        .args(arguments)
        .output()
        .map_err(|error| {
            AppError::structured(
                ErrorCategory::ExecutionFailure,
                "git_spawn_failed",
                format!("could not execute Git: {error}"),
                Some(json!({ "repository": repository })),
            )
        })
}

fn parse_single_oid(command: &str, output: &Output) -> Result<String, AppError> {
    let oid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if valid_full_oid(&oid) {
        Ok(oid)
    } else {
        Err(malformed_git_output(
            "git_oid_output_invalid",
            format!("{command} did not return one full object ID"),
            output,
        ))
    }
}

fn valid_full_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn contains_line_break(value: &str) -> bool {
    value.contains(['\n', '\r'])
}

fn git_failure(
    code: &str,
    message: impl Into<String>,
    exit_code: Option<i32>,
    output: &Output,
) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        code,
        message,
        Some(json!({
            "exit_code": exit_code,
            "stdout": bounded_text(&output.stdout),
            "stderr": bounded_text(&output.stderr),
        })),
    )
}

fn malformed_git_output(code: &str, message: impl Into<String>, output: &Output) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        code,
        message,
        Some(json!({
            "stdout": bounded_text(&output.stdout),
            "stderr": bounded_text(&output.stderr),
        })),
    )
}

fn bounded_text(bytes: &[u8]) -> String {
    const MAX_BYTES: usize = 4_096;
    let end = bytes.len().min(MAX_BYTES);
    let mut text = String::from_utf8_lossy(&bytes[..end]).into_owned();
    if bytes.len() > MAX_BYTES {
        text.push_str("…[truncated]");
    }
    text
}

fn bytes_to_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::process::Command;

    use mcp_cli::StructuredError;
    use tempfile::TempDir;

    use super::*;
    use crate::model::RepositoryId;

    struct TestRepo {
        directory: TempDir,
    }

    impl TestRepo {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("temp repository");
            git(
                directory.path(),
                ["init", "--quiet", "--initial-branch=main"],
            );
            git(directory.path(), ["config", "user.name", "Caravan Test"]);
            git(
                directory.path(),
                ["config", "user.email", "caravan@example.invalid"],
            );
            git(
                directory.path(),
                [
                    "remote",
                    "add",
                    "fixture",
                    directory.path().to_str().expect("utf-8 fixture path"),
                ],
            );
            Self { directory }
        }

        fn path(&self) -> &Path {
            self.directory.path()
        }

        fn commit_file(&self, path: &str, contents: &str, message: &str) -> String {
            let full_path = self.path().join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).expect("create parent");
            }
            fs::write(full_path, contents).expect("write fixture");
            git(self.path(), ["add", "--", path]);
            git(self.path(), ["commit", "--quiet", "--message", message]);
            rev_parse(self.path(), "HEAD")
        }

        fn switch(&self, branch: &str, start: &str) {
            git(
                self.path(),
                ["switch", "--quiet", "--create", branch, start],
            );
        }

        fn branch(name: &str, oid: &str) -> BranchSnapshot {
            BranchSnapshot {
                repository: RepositoryId {
                    owner: "harryaskham".to_owned(),
                    name: "caravan".to_owned(),
                },
                name: name.to_owned(),
                oid: CommitOid(oid.to_owned()),
            }
        }
    }

    #[test]
    fn clean_adjacent_check_leaves_head_and_worktree_unchanged() {
        let repository = TestRepo::new();
        let base = repository.commit_file("base.txt", "base\n", "base");
        repository.switch("predecessor", &base);
        let predecessor = repository.commit_file("parent.txt", "parent\n", "parent");
        repository.switch("child", &predecessor);
        let child = repository.commit_file("child.txt", "child\n", "child");
        let before_head = rev_parse(repository.path(), "HEAD");
        let before_status = git_stdout(repository.path(), ["status", "--porcelain=v1"]);

        let report = check_adjacent(
            repository.path(),
            "fixture",
            &TestRepo::branch("child", &child),
            &TestRepo::branch("predecessor", &predecessor),
        )
        .expect("clean compatibility report");

        assert_eq!(report.outcome, CompatibilityOutcome::Clean);
        assert!(report.conflicting_paths.is_empty());
        assert_eq!(report.candidate.name, "child");
        assert_eq!(report.target.name, "predecessor");
        assert_eq!(rev_parse(repository.path(), "HEAD"), before_head);
        assert_eq!(
            git_stdout(repository.path(), ["status", "--porcelain=v1"]),
            before_status
        );
    }

    #[test]
    fn conflicting_head_to_default_check_returns_paths_without_checkout() {
        let repository = TestRepo::new();
        let base = repository.commit_file("shared.txt", "base\n", "base");
        repository.switch("feature", &base);
        let head = repository.commit_file("shared.txt", "feature\n", "feature");
        git(repository.path(), ["switch", "--quiet", "main"]);
        let default = repository.commit_file("shared.txt", "default\n", "default");
        let before_head = rev_parse(repository.path(), "HEAD");
        let before_contents = fs::read_to_string(repository.path().join("shared.txt"))
            .expect("read worktree fixture");

        let report = check_head_to_default(
            repository.path(),
            "fixture",
            &TestRepo::branch("feature", &head),
            &TestRepo::branch("main", &default),
        )
        .expect("conflict is canonical evidence, not an execution error");

        assert_eq!(report.outcome, CompatibilityOutcome::Conflict);
        assert_eq!(report.conflicting_paths, ["shared.txt"]);
        assert_eq!(report.candidate.name, "feature");
        assert_eq!(report.target.name, "main");
        assert_eq!(rev_parse(repository.path(), "HEAD"), before_head);
        assert_eq!(
            fs::read_to_string(repository.path().join("shared.txt"))
                .expect("read worktree after check"),
            before_contents
        );
    }

    #[test]
    fn cross_caravan_check_preserves_head_to_tail_order() {
        let repository = TestRepo::new();
        let base = repository.commit_file("base.txt", "base\n", "base");
        repository.switch("tail", &base);
        let tail = repository.commit_file("tail.txt", "tail\n", "tail");
        repository.switch("other-head", &base);
        let head = repository.commit_file("head.txt", "head\n", "head");

        let report = check_cross_caravan(
            repository.path(),
            "fixture",
            &TestRepo::branch("other-head", &head),
            &TestRepo::branch("tail", &tail),
        )
        .expect("ordered cross-caravan report");

        assert_eq!(report.outcome, CompatibilityOutcome::Clean);
        assert_eq!(report.candidate.name, "other-head");
        assert_eq!(report.target.name, "tail");
    }

    #[test]
    fn exact_remote_revision_is_fetched_without_updating_refs() {
        let source = TestRepo::new();
        let expected = source.commit_file("source.txt", "remote\n", "remote commit");
        let consumer = TestRepo::new();
        git(
            consumer.path(),
            [
                "remote",
                "add",
                "source",
                source.path().to_str().expect("utf-8 fixture path"),
            ],
        );
        let refs_before = git_stdout(consumer.path(), ["show-ref"]);

        let resolved = resolve_branch_snapshot(
            consumer.path(),
            "source",
            &TestRepo::branch("main", &expected),
        )
        .expect("fetch exact remote revision");

        assert_eq!(resolved, CommitOid(expected));
        assert_eq!(git_stdout(consumer.path(), ["show-ref"]), refs_before);
    }

    #[test]
    fn moved_remote_revision_returns_stale_evidence() {
        let source = TestRepo::new();
        let actual = source.commit_file("source.txt", "remote\n", "remote commit");
        let consumer = TestRepo::new();
        git(
            consumer.path(),
            [
                "remote",
                "add",
                "source",
                source.path().to_str().expect("utf-8 fixture path"),
            ],
        );
        let wrong = if actual.starts_with('0') {
            "1".repeat(actual.len())
        } else {
            "0".repeat(actual.len())
        };

        let error =
            resolve_branch_snapshot(consumer.path(), "source", &TestRepo::branch("main", &wrong))
                .expect_err("stale discovery must not validate a different remote head");

        assert_eq!(error.code(), "stale_remote_revision");
        let details = error.details().expect("stale details");
        assert_eq!(details["advertised_oid"], actual);
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

    fn git_stdout<I, S>(repository: &Path, arguments: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = Command::new("git")
            .current_dir(repository)
            .args(arguments)
            .output()
            .expect("run git fixture command");
        if output.status.success() {
            String::from_utf8_lossy(&output.stdout).into_owned()
        } else if output.status.code() == Some(1) && output.stdout.is_empty() {
            String::new()
        } else {
            panic!(
                "git fixture failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    fn rev_parse(repository: &Path, revision: &str) -> String {
        git_stdout(
            repository,
            ["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
        )
        .trim()
        .to_owned()
    }
}
