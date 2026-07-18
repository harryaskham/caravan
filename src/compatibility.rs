//! Worktree-free Git compatibility checks.
//!
//! GitHub discovery supplies canonical [`BranchSnapshot`] values. This module
//! fetches and verifies those exact revisions, then asks `git merge-tree` to
//! construct merge evidence without checking out or rewriting either branch.

use std::path::Path;
use std::time::Duration;

use mcp_cli::ErrorCategory;
use serde_json::json;

use crate::AppError;
use crate::command::{
    CommandOutput, CommandRunError, CommandRunner, CommandSpec, DEFAULT_COMMAND_TIMEOUT,
    ProcessRunner,
};
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
    check_compatibility_with_timeout(
        repository,
        remote,
        candidate,
        target,
        DEFAULT_COMMAND_TIMEOUT,
    )
}

/// Run the compatibility primitive with an explicit per-command hard deadline.
pub fn check_compatibility_with_timeout(
    repository: impl AsRef<Path>,
    remote: &str,
    candidate: &BranchSnapshot,
    target: &BranchSnapshot,
    timeout: Duration,
) -> Result<CompatibilityReport, AppError> {
    let runner = ProcessRunner::in_directory(repository).with_timeout(timeout);
    check_compatibility_with_runner(&runner, remote, candidate, target)
}

fn check_compatibility_with_runner(
    runner: &impl CommandRunner,
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

    let target_oid = resolve_branch_snapshot_with_runner(runner, remote, target)?;
    let candidate_oid = resolve_branch_snapshot_with_runner(runner, remote, candidate)?;
    check_resolved_compatibility_with_runner(runner, candidate, target, &candidate_oid, &target_oid)
}

/// Construct one merge report from revisions already validated and fetched by
/// a bounded graph preparation phase.
pub(crate) fn check_resolved_compatibility_with_runner(
    runner: &impl CommandRunner,
    candidate: &BranchSnapshot,
    target: &BranchSnapshot,
    candidate_oid: &CommitOid,
    target_oid: &CommitOid,
) -> Result<CompatibilityReport, AppError> {
    let output = git_output(
        runner,
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
    let outcome = match output.code {
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

    let mut fields = output.stdout.as_bytes().split(|byte| *byte == 0);
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
    resolve_branch_snapshot_with_timeout(repository, remote, snapshot, DEFAULT_COMMAND_TIMEOUT)
}

/// Resolve a branch snapshot with an explicit per-command hard deadline.
pub fn resolve_branch_snapshot_with_timeout(
    repository: impl AsRef<Path>,
    remote: &str,
    snapshot: &BranchSnapshot,
    timeout: Duration,
) -> Result<CommitOid, AppError> {
    let runner = ProcessRunner::in_directory(repository).with_timeout(timeout);
    resolve_branch_snapshot_with_runner(&runner, remote, snapshot)
}

pub(crate) fn resolve_branch_snapshot_with_runner(
    runner: &impl CommandRunner,
    remote: &str,
    snapshot: &BranchSnapshot,
) -> Result<CommitOid, AppError> {
    validate_remote(remote)?;
    validate_expected_oid(&snapshot.oid.0)?;
    let reference = branch_reference(runner, &snapshot.name)?;
    require_advertised_oid(runner, remote, &reference, &snapshot.oid.0)?;

    let fetch = git_output(
        runner,
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
    if !fetch.is_success() {
        return Err(git_failure(
            "git_fetch_failed",
            "Git could not fetch the requested exact remote ref",
            fetch.code,
            &fetch,
        ));
    }

    // A branch may move between discovery and fetch. Rechecking prevents a
    // caller from silently validating a newer remote head than it requested.
    require_advertised_oid(runner, remote, &reference, &snapshot.oid.0)?;
    resolve_local_commit(runner, &snapshot.oid.0)
}

/// Validate and fetch a complete unique branch set with a constant three
/// network subprocesses, then verify every object in one local batch.
pub(crate) fn prepare_branch_snapshots_with_runner(
    runner: &impl CommandRunner,
    remote: &str,
    snapshots: &[BranchSnapshot],
) -> Result<std::collections::BTreeMap<(String, String), CommitOid>, AppError> {
    validate_remote(remote)?;
    let mut branches = Vec::with_capacity(snapshots.len());
    for snapshot in snapshots {
        validate_expected_oid(&snapshot.oid.0)?;
        branches.push((
            branch_reference(runner, &snapshot.name)?,
            snapshot.oid.0.clone(),
            snapshot.name.clone(),
        ));
    }
    let references = branches
        .iter()
        .map(|(reference, _, _)| reference.clone())
        .collect::<Vec<_>>();
    verify_advertised_batch(runner, remote, &branches, &references)?;

    let fetch = git_output(
        runner,
        [
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--refmap=",
            remote,
        ]
        .into_iter()
        .chain(references.iter().map(String::as_str)),
    )?;
    if !fetch.is_success() {
        return Err(git_failure(
            "git_fetch_failed",
            "Git could not fetch the requested exact remote refs",
            fetch.code,
            &fetch,
        ));
    }
    verify_advertised_batch(runner, remote, &branches, &references)?;

    let input = branches.iter().fold(String::new(), |mut input, (_, oid, _)| {
        use std::fmt::Write as _;
        writeln!(input, "{oid}^{{commit}}").expect("writing to String cannot fail");
        input
    });
    let command = CommandSpec::new("git")
        .args(["cat-file", "--batch-check=%(objectname) %(objecttype)"])
        .stdin(input);
    let output = runner
        .run(&command)
        .map_err(|error| command_run_error(&error))?;
    if !output.is_success() {
        return Err(git_failure(
            "revision_not_found",
            "Git could not verify prepared commit objects",
            output.code,
            &output,
        ));
    }
    let lines = output.stdout.lines().collect::<Vec<_>>();
    if lines.len() != branches.len() {
        return Err(malformed_git_output(
            "git_object_batch_invalid",
            "git cat-file returned an unexpected number of objects",
            &output,
        ));
    }
    let mut prepared = std::collections::BTreeMap::new();
    for ((_, expected, name), line) in branches.into_iter().zip(lines) {
        let mut fields = line.split_whitespace();
        if fields.next() != Some(expected.as_str()) || fields.next() != Some("commit") {
            return Err(malformed_git_output(
                "git_object_batch_invalid",
                "git cat-file did not resolve an expected commit",
                &output,
            ));
        }
        prepared.insert((name, expected.clone()), CommitOid(expected));
    }
    Ok(prepared)
}

fn verify_advertised_batch(
    runner: &impl CommandRunner,
    remote: &str,
    branches: &[(String, String, String)],
    references: &[String],
) -> Result<(), AppError> {
    let output = git_output(
        runner,
        ["ls-remote", "--refs", "--exit-code", remote]
            .into_iter()
            .chain(references.iter().map(String::as_str)),
    )?;
    if !output.is_success() {
        return Err(git_failure(
            "remote_ref_not_found",
            "Git did not advertise every prepared branch",
            output.code,
            &output,
        ));
    }
    let advertised = output
        .stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(oid, reference)| (reference, oid))
        .collect::<std::collections::BTreeMap<_, _>>();
    for (reference, expected, _) in branches {
        match advertised.get(reference.as_str()) {
            Some(actual) if *actual == expected => {}
            Some(actual) => {
                return Err(AppError::structured(
                    ErrorCategory::Validation,
                    "stale_remote_revision",
                    format!("remote ref `{reference}` moved since discovery"),
                    Some(
                        json!({"reference": reference, "expected_oid": expected, "advertised_oid": actual, "resumable": true}),
                    ),
                ));
            }
            None => {
                return Err(AppError::structured(
                    ErrorCategory::TargetNotFound,
                    "remote_ref_not_found",
                    format!("remote ref `{reference}` is not advertised by `{remote}`"),
                    Some(json!({"remote": remote, "reference": reference})),
                ));
            }
        }
    }
    Ok(())
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

fn branch_reference(runner: &impl CommandRunner, branch: &str) -> Result<String, AppError> {
    if branch.trim().is_empty() || contains_line_break(branch) {
        return Err(AppError::validation(
            "invalid_branch_name",
            "a branch name must be non-empty and single-line",
        ));
    }
    let reference = format!("refs/heads/{branch}");
    let output = git_output(runner, ["check-ref-format", reference.as_str()])?;
    if !output.is_success() {
        return Err(AppError::validation(
            "invalid_branch_name",
            format!("`{branch}` is not a valid Git branch name"),
        ));
    }
    Ok(reference)
}

fn require_advertised_oid(
    runner: &impl CommandRunner,
    remote: &str,
    reference: &str,
    expected_oid: &str,
) -> Result<(), AppError> {
    let output = git_output(
        runner,
        ["ls-remote", "--refs", "--exit-code", remote, reference],
    )?;
    if !output.is_success() {
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

    let advertised = output
        .stdout
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

fn resolve_local_commit(
    runner: &impl CommandRunner,
    expected_oid: &str,
) -> Result<CommitOid, AppError> {
    let commit_expression = format!("{expected_oid}^{{commit}}");
    let output = git_output(
        runner,
        [
            "rev-parse",
            "--verify",
            "--end-of-options",
            commit_expression.as_str(),
        ],
    )?;
    if !output.is_success() {
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

fn git_output<I, S>(runner: &impl CommandRunner, arguments: I) -> Result<CommandOutput, AppError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    runner
        .run(&CommandSpec::new("git").args(arguments))
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
        let subcommand = command.args.first().map_or("unknown", String::as_str);
        return AppError::structured(
            ErrorCategory::Timeout,
            "git_compatibility_timeout",
            error.to_string(),
            Some(json!({
                "stage": format!("git_compatibility:{subcommand}"),
                "command": command.display(),
                "timeout_ms": timeout_ms,
                "stdout": stdout,
                "stderr": stderr,
                "resumable": true,
                "next": "restore Git transport health, rediscover exact revisions, and retry",
            })),
        );
    }
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "git_command_failed",
        error.to_string(),
        Some(json!({ "error": format!("{error:?}") })),
    )
}

fn parse_single_oid(command: &str, output: &CommandOutput) -> Result<String, AppError> {
    let oid = output.stdout.trim().to_owned();
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
    output: &CommandOutput,
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

fn malformed_git_output(
    code: &str,
    message: impl Into<String>,
    output: &CommandOutput,
) -> AppError {
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

    struct CountingRunner {
        inner: ProcessRunner,
        commands: std::cell::RefCell<Vec<CommandSpec>>,
    }

    impl CommandRunner for CountingRunner {
        fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
            self.commands.borrow_mut().push(command.clone());
            self.inner.run(command)
        }
    }

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
    fn batch_preparation_has_constant_network_calls_and_one_merge_per_report() {
        let repository = TestRepo::new();
        let main = repository.commit_file("base.txt", "base\n", "base");
        repository.switch("one", &main);
        let one = repository.commit_file("one.txt", "one\n", "one");
        repository.switch("two", &one);
        let two = repository.commit_file("two.txt", "two\n", "two");
        let branches = [
            TestRepo::branch("main", &main),
            TestRepo::branch("one", &one),
            TestRepo::branch("two", &two),
        ];
        let runner = CountingRunner {
            inner: ProcessRunner::in_directory(repository.path()),
            commands: std::cell::RefCell::new(Vec::new()),
        };
        let prepared = prepare_branch_snapshots_with_runner(&runner, "fixture", &branches).unwrap();
        check_resolved_compatibility_with_runner(
            &runner,
            &branches[1],
            &branches[0],
            prepared.get(&("one".to_owned(), one.clone())).unwrap(),
            prepared.get(&("main".to_owned(), main)).unwrap(),
        )
        .unwrap();
        check_resolved_compatibility_with_runner(
            &runner,
            &branches[2],
            &branches[1],
            prepared.get(&("two".to_owned(), two)).unwrap(),
            prepared.get(&("one".to_owned(), one.clone())).unwrap(),
        )
        .unwrap();

        let commands = runner.commands.borrow();
        let network = commands
            .iter()
            .filter(|command| {
                matches!(
                    command.args.first().map(String::as_str),
                    Some("ls-remote" | "fetch")
                )
            })
            .count();
        let merge_reports = commands
            .iter()
            .filter(|command| command.args.first().is_some_and(|arg| arg == "merge-tree"))
            .count();
        assert_eq!(network, 3, "two advertised snapshots plus one batch fetch");
        assert_eq!(merge_reports, 2);
        assert_eq!(commands.len(), branches.len() + 4 + merge_reports);
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

    struct TimeoutRunner;

    impl CommandRunner for TimeoutRunner {
        fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
            Err(CommandRunError::Timeout {
                command: command.clone(),
                timeout_ms: 250,
                stdout: "partial".to_owned(),
                stderr: "transport stalled".to_owned(),
            })
        }
    }

    #[test]
    fn direct_git_timeout_is_structured_with_stage_and_evidence() {
        let error = resolve_branch_snapshot_with_runner(
            &TimeoutRunner,
            "origin",
            &TestRepo::branch("main", &"a".repeat(40)),
        )
        .expect_err("direct Git timeout must not become a generic execution error");

        assert_eq!(error.category(), ErrorCategory::Timeout);
        assert_eq!(error.code(), "git_compatibility_timeout");
        let details = error.details().expect("timeout details");
        assert_eq!(details["stage"], "git_compatibility:check-ref-format");
        assert_eq!(details["timeout_ms"], 250);
        assert_eq!(details["stdout"], "partial");
        assert_eq!(details["stderr"], "transport stalled");
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
