//! Explicit, lease-protected physical branch chaining.
//!
//! This module is deliberately separate from compatibility checks: enabling
//! `rebase_on_join` authorizes history rewriting.  Every rewrite is first
//! completed in a detached temporary worktree, and the only remote mutation is
//! an exact force-with-lease push.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppError;
use crate::command::{CommandOutput, CommandRunner, CommandSpec, ProcessRunner};
use crate::model::{BranchSnapshot, CommitOid, PrNumber, PullRequestSnapshot, RepositoryId};

/// Shared child and whole-operation limits for one prepared generation.
#[derive(Debug, Clone, Copy)]
pub struct RebaseExecutionBudget {
    pub command_timeout: Duration,
    pub operation_deadline: Option<Instant>,
}

impl RebaseExecutionBudget {
    #[must_use]
    pub fn new(command_timeout: Duration) -> Self {
        Self {
            command_timeout,
            operation_deadline: None,
        }
    }

    #[must_use]
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.operation_deadline = Some(deadline);
        self
    }
}

/// Whether a planned base already exists remotely or is a retained simulated parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "branch", rename_all = "snake_case")]
pub enum PlannedBase {
    Remote(BranchSnapshot),
    Simulated(BranchSnapshot),
}

impl PlannedBase {
    fn branch(&self) -> &BranchSnapshot {
        match self {
            Self::Remote(branch) | Self::Simulated(branch) => branch,
        }
    }
}

/// Exact source used to retain and verify the candidate-only range boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlannedRangeBase {
    RemoteBranch {
        branch: BranchSnapshot,
    },
    PullRequestHead {
        pr: PrNumber,
        branch: BranchSnapshot,
    },
}

impl PlannedRangeBase {
    fn branch(&self) -> &BranchSnapshot {
        match self {
            Self::RemoteBranch { branch } | Self::PullRequestHead { branch, .. } => branch,
        }
    }
}

/// Serializable immutable plan produced once and pushed without recomputation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RebasePlan {
    pub pr: PrNumber,
    pub branch: String,
    pub old_head_oid: CommitOid,
    pub old_base_oid: CommitOid,
    pub range_source: PlannedRangeBase,
    pub new_base: PlannedBase,
    pub new_head_oid: CommitOid,
    pub new_tree_oid: CommitOid,
    pub commit_count: usize,
    pub ci_trigger_workflows: Vec<String>,
    pub lease: String,
    pub already_satisfied: bool,
}

/// A plan plus the exact temporary object/worktree generation that created it.
pub struct PreparedRebase {
    pub plan: RebasePlan,
    worktree: TemporaryWorktree,
}

/// Auditable receipt for one physical branch rewrite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RebaseReceipt {
    pub pr: PrNumber,
    pub branch: String,
    pub old_head_oid: CommitOid,
    pub new_head_oid: CommitOid,
    pub old_base_oid: CommitOid,
    pub new_base_branch: String,
    pub new_base_oid: CommitOid,
    pub new_tree_oid: CommitOid,
    pub commit_count: usize,
    /// Exact-default workflow files proven able to run for this PR base on
    /// both force-push (`synchronize`) and base edit (`edited`).
    pub ci_trigger_workflows: Vec<String>,
    /// The exact optimistic lease passed to Git.
    pub lease: String,
    /// True when no push was needed because the exact ancestry already held.
    pub already_satisfied: bool,
}

/// Rebase `candidate`'s candidate-only linear series onto `new_base`.
///
/// Both snapshots must belong to the base repository. The candidate's exact
/// provider base is the lower range boundary; refusing merge commits and a
/// non-ancestor boundary prevents accidentally forcing an ambiguous patch.
pub fn rewrite_candidate(
    repository_path: &Path,
    repository: &RepositoryId,
    candidate: &PullRequestSnapshot,
    new_base: &BranchSnapshot,
    workflow_source: &BranchSnapshot,
    timeout: Duration,
) -> Result<RebaseReceipt, AppError> {
    let prepared = prepare_candidate(
        repository_path,
        repository,
        candidate,
        PlannedRangeBase::RemoteBranch {
            branch: candidate.base.clone(),
        },
        PlannedBase::Remote(new_base.clone()),
        workflow_source,
        RebaseExecutionBudget::new(timeout),
    )?;
    apply_prepared(&prepared)
}

/// Materialize one exact rebase generation and retain its worktree through apply.
#[allow(clippy::too_many_lines)]
pub fn prepare_candidate(
    repository_path: &Path,
    repository: &RepositoryId,
    candidate: &PullRequestSnapshot,
    range_source: PlannedRangeBase,
    new_base: PlannedBase,
    workflow_source: &BranchSnapshot,
    budget: RebaseExecutionBudget,
) -> Result<PreparedRebase, AppError> {
    let target = new_base.branch();
    let range_branch = range_source.branch();
    if candidate.cross_repository
        || candidate.head.repository != *repository
        || candidate.base.repository != *repository
        || range_branch.repository != *repository
        || range_branch.oid != candidate.base.oid
        || target.repository != *repository
        || workflow_source.repository != *repository
    {
        return Err(decision(
            "rebase_repository_not_owned",
            "cumulative rebase requires candidate, old base, and new base branches in the owned base repository",
            json!({"pr": candidate.number, "resumable": true}),
        ));
    }
    validate_branch(&candidate.head.name)?;
    for oid in [
        &candidate.head.oid,
        &candidate.base.oid,
        &target.oid,
        &workflow_source.oid,
    ] {
        validate_oid(oid)?;
    }

    let runner = process_runner(
        repository_path,
        budget.command_timeout,
        budget.operation_deadline,
    );
    fetch_exact(&runner, "origin", &candidate.head)?;
    match &range_source {
        PlannedRangeBase::RemoteBranch { branch } => fetch_exact(&runner, "origin", branch)?,
        PlannedRangeBase::PullRequestHead { pr, branch } => {
            fetch_exact_pull_request_head(&runner, "origin", *pr, branch)?;
        }
    }
    match &new_base {
        PlannedBase::Remote(branch)
            if branch.name != candidate.base.name || branch.oid != candidate.base.oid =>
        {
            fetch_exact(&runner, "origin", branch)?;
        }
        PlannedBase::Simulated(branch) => {
            require_success(
                &runner,
                CommandSpec::new("git").args([
                    "cat-file",
                    "-e",
                    &format!("{}^{{commit}}", branch.oid),
                ]),
                "rebase_simulated_parent_missing",
                "planned parent object is not retained locally",
            )?;
        }
        PlannedBase::Remote(_) => {}
    }
    if workflow_source.name != candidate.base.name || workflow_source.oid != candidate.base.oid {
        fetch_exact(&runner, "origin", workflow_source)?;
    }
    let ci_trigger_workflows = preflight_ci_triggers(&runner, workflow_source, &target.name)?;

    let merge_bases = require_success(
        &runner,
        CommandSpec::new("git").args([
            "merge-base",
            "--all",
            candidate.base.oid.0.as_str(),
            candidate.head.oid.0.as_str(),
        ]),
        "rebase_range_ambiguous",
        "the exact candidate/base merge base could not be determined",
    )?;
    let boundaries = merge_bases.stdout.lines().collect::<Vec<_>>();
    if boundaries.len() != 1 {
        return Err(decision(
            "rebase_range_ambiguous",
            "candidate and exact provider base do not have one unambiguous merge base",
            json!({"pr": candidate.number, "merge_bases": boundaries, "resumable": true}),
        ));
    }
    let range_base = CommitOid(boundaries[0].to_owned());
    validate_oid(&range_base)?;
    let merges = run(
        &runner,
        CommandSpec::new("git").args([
            "rev-list",
            "--merges",
            &format!("{}..{}", range_base.0, candidate.head.oid.0),
        ]),
    )?;
    if !merges.is_success() || !merges.stdout.trim().is_empty() {
        return Err(decision(
            "rebase_nonlinear_range",
            "candidate-only history contains a merge commit; no audited merge-preserving rewrite strategy is implemented",
            json!({"pr": candidate.number, "merge_oids": merges.stdout.lines().collect::<Vec<_>>(), "resumable": true}),
        ));
    }
    let count_output = require_success(
        &runner,
        CommandSpec::new("git").args([
            "rev-list",
            "--count",
            &format!("{}..{}", range_base.0, candidate.head.oid.0),
        ]),
        "rebase_range_count_failed",
        "could not count the candidate-only commit range",
    )?;
    let commit_count = count_output.stdout.trim().parse::<usize>().map_err(|_| {
        decision(
            "rebase_range_count_failed",
            "Git returned an invalid candidate-only commit count",
            json!({"output": count_output.stdout}),
        )
    })?;
    if commit_count == 0 {
        return Err(decision(
            "rebase_empty_patch_range",
            "candidate has no commits beyond its exact old base",
            json!({"pr": candidate.number, "resumable": true}),
        ));
    }

    let worktree = TemporaryWorktree::create(
        repository_path,
        &candidate.head.oid,
        budget.command_timeout,
        budget.operation_deadline,
    )?;
    let worktree_runner = process_runner(
        &worktree.path,
        budget.command_timeout,
        budget.operation_deadline,
    );
    let rebase = run(
        &worktree_runner,
        CommandSpec::new("git")
            .args([
                "-c",
                "commit.gpgSign=false",
                "rebase",
                "--onto",
                target.oid.0.as_str(),
                range_base.0.as_str(),
                candidate.head.oid.0.as_str(),
            ])
            .env("GIT_TERMINAL_PROMPT", "0"),
    )?;
    if !rebase.is_success() {
        let conflicts = run(
            &worktree_runner,
            CommandSpec::new("git").args(["diff", "--name-only", "--diff-filter=U"]),
        )
        .map(|output| output.stdout.lines().map(str::to_owned).collect::<Vec<_>>())
        .unwrap_or_default();
        return Err(decision(
            "rebase_conflict",
            "candidate-only commits do not rebase cleanly; no remote or provider mutation was attempted",
            json!({"pr": candidate.number, "conflicting_paths": conflicts, "stderr": rebase.stderr, "resumable": true, "next": "repair the branch and rerun the same command"}),
        ));
    }
    let new_head = rev_parse(&worktree_runner, "HEAD")?;
    let new_tree = rev_parse(&worktree_runner, "HEAD^{tree}")?;
    let lease = format!(
        "--force-with-lease=refs/heads/{}:{}",
        candidate.head.name, candidate.head.oid.0
    );
    let destination = format!("HEAD:refs/heads/{}", candidate.head.name);

    // This checks authentication, branch rules, and the exact lease without
    // updating the branch. It occurs only after the complete conflict simulation.
    require_success(
        &worktree_runner,
        CommandSpec::new("git").args([
            "push",
            "--dry-run",
            lease.as_str(),
            "origin",
            destination.as_str(),
        ]),
        "rebase_push_preflight_failed",
        "push permission, branch ownership, or exact lease preflight failed",
    )?;

    let already_satisfied = new_head == candidate.head.oid;
    let plan = RebasePlan {
        pr: candidate.number,
        branch: candidate.head.name.clone(),
        old_head_oid: candidate.head.oid.clone(),
        old_base_oid: range_base,
        range_source,
        new_base,
        new_head_oid: new_head,
        new_tree_oid: new_tree,
        commit_count,
        ci_trigger_workflows,
        lease,
        already_satisfied,
    };
    Ok(PreparedRebase { plan, worktree })
}

/// Recheck the exact planned object, remote old head, permission, and lease without writing.
pub fn verify_prepared(prepared: &PreparedRebase) -> Result<(), AppError> {
    let runner = process_runner(
        &prepared.worktree.path,
        prepared.worktree.timeout,
        prepared.worktree.operation_deadline,
    );
    let retained_head = rev_parse(&runner, "HEAD")?;
    if retained_head != prepared.plan.new_head_oid {
        return Err(decision(
            "rebase_prepared_object_changed",
            "retained prepared rebase no longer resolves to its planned head",
            json!({"plan": prepared.plan, "retained_head": retained_head, "resumable": true}),
        ));
    }
    verify_remote_head(
        &runner,
        "origin",
        &prepared.plan.branch,
        &prepared.plan.old_head_oid,
    )?;
    let destination = format!("HEAD:refs/heads/{}", prepared.plan.branch);
    require_success(
        &runner,
        CommandSpec::new("git").args([
            "push",
            "--dry-run",
            prepared.plan.lease.as_str(),
            "origin",
            destination.as_str(),
        ]),
        "rebase_push_preflight_failed",
        "push permission, branch ownership, or exact lease preflight failed",
    )?;
    Ok(())
}

/// Push the exact retained planned object under its original old-head lease.
pub fn apply_prepared(prepared: &PreparedRebase) -> Result<RebaseReceipt, AppError> {
    verify_prepared(prepared)?;
    let runner = process_runner(
        &prepared.worktree.path,
        prepared.worktree.timeout,
        prepared.worktree.operation_deadline,
    );
    match &prepared.plan.new_base {
        PlannedBase::Remote(target) | PlannedBase::Simulated(target) => {
            verify_remote_head(&runner, "origin", &target.name, &target.oid)?;
        }
    }
    if !prepared.plan.already_satisfied {
        let runner = process_runner(
            &prepared.worktree.path,
            prepared.worktree.timeout,
            prepared.worktree.operation_deadline,
        );
        let destination = format!("HEAD:refs/heads/{}", prepared.plan.branch);
        require_success(
            &runner,
            CommandSpec::new("git").args([
                "push",
                prepared.plan.lease.as_str(),
                "origin",
                destination.as_str(),
            ]),
            "rebase_stale_lease",
            "exact force-with-lease push failed; the branch may have moved and was not overwritten",
        )?;
    }
    Ok(RebaseReceipt {
        pr: prepared.plan.pr,
        branch: prepared.plan.branch.clone(),
        old_head_oid: prepared.plan.old_head_oid.clone(),
        new_head_oid: prepared.plan.new_head_oid.clone(),
        old_base_oid: prepared.plan.old_base_oid.clone(),
        new_base_branch: prepared.plan.new_base.branch().name.clone(),
        new_base_oid: prepared.plan.new_base.branch().oid.clone(),
        new_tree_oid: prepared.plan.new_tree_oid.clone(),
        commit_count: prepared.plan.commit_count,
        ci_trigger_workflows: prepared.plan.ci_trigger_workflows.clone(),
        lease: prepared.plan.lease.clone(),
        already_satisfied: prepared.plan.already_satisfied,
    })
}

fn preflight_ci_triggers(
    runner: &impl CommandRunner,
    source: &BranchSnapshot,
    target_branch: &str,
) -> Result<Vec<String>, AppError> {
    let listing = require_success(
        runner,
        CommandSpec::new("git").args([
            "ls-tree",
            "-r",
            "--name-only",
            source.oid.0.as_str(),
            ".github/workflows",
        ]),
        "rebase_ci_preflight_failed",
        "could not inspect workflows at the exact default revision",
    )?;
    let mut supported = Vec::new();
    for path in listing.stdout.lines().filter(|path| {
        Path::new(path).extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("yml") || extension.eq_ignore_ascii_case("yaml")
        })
    }) {
        let object = format!("{}:{path}", source.oid.0);
        let content = require_success(
            runner,
            CommandSpec::new("git").args(["show", object.as_str()]),
            "rebase_ci_preflight_failed",
            "could not read a workflow at the exact default revision",
        )?;
        let Ok(document) = serde_yaml::from_str::<serde_yaml::Value>(&content.stdout) else {
            continue;
        };
        let Some(root) = document.as_mapping() else {
            continue;
        };
        let Some(on) = yaml_get(root, "on").and_then(serde_yaml::Value::as_mapping) else {
            continue;
        };
        let Some(pull_request) = yaml_get(on, "pull_request") else {
            continue;
        };
        let Some(policy) = pull_request.as_mapping() else {
            // A null/scalar pull_request uses GitHub's default activity types,
            // which omit `edited` and therefore cannot prove the base retarget.
            continue;
        };
        let types = yaml_strings(yaml_get(policy, "types"));
        if !["opened", "synchronize", "reopened", "edited", "labeled"]
            .iter()
            .all(|required| types.iter().any(|item| item == required))
        {
            continue;
        }
        let branches = yaml_strings(yaml_get(policy, "branches"));
        if !branches.is_empty() || yaml_get(policy, "branches-ignore").is_some() {
            continue;
        }
        supported.push(path.to_owned());
    }
    if supported.is_empty() {
        return Err(decision(
            "rebase_ci_trigger_missing",
            "no exact-default pull_request workflow can run for the selected parent base on synchronize, edited, and labeled events",
            json!({
                "target_base": target_branch,
                "workflow_source_oid": source.oid,
                "required_types": ["opened", "synchronize", "reopened", "edited", "labeled"],
                "next": "enable a dedicated stack/full pull_request workflow without a branches filter and include types opened, synchronize, reopened, edited, labeled, and unlabeled; gate jobs on main base or the caravan PR label",
                "resumable": true
            }),
        ));
    }
    Ok(supported)
}

fn yaml_get<'a>(mapping: &'a serde_yaml::Mapping, key: &str) -> Option<&'a serde_yaml::Value> {
    mapping.get(serde_yaml::Value::String(key.to_owned()))
}

fn yaml_strings(value: Option<&serde_yaml::Value>) -> Vec<String> {
    match value {
        Some(serde_yaml::Value::String(value)) => vec![value.clone()],
        Some(serde_yaml::Value::Sequence(values)) => values
            .iter()
            .filter_map(serde_yaml::Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// Reverify one exact remote branch snapshot at a whole-plan write barrier.
pub fn verify_branch_snapshot(
    repository_path: &Path,
    snapshot: &BranchSnapshot,
    timeout: Duration,
) -> Result<(), AppError> {
    validate_branch(&snapshot.name)?;
    validate_oid(&snapshot.oid)?;
    let runner = ProcessRunner::in_directory(repository_path).with_timeout(timeout);
    verify_remote_head(&runner, "origin", &snapshot.name, &snapshot.oid)
}

fn verify_remote_head(
    runner: &impl CommandRunner,
    remote: &str,
    branch: &str,
    expected: &CommitOid,
) -> Result<(), AppError> {
    let reference = format!("refs/heads/{branch}");
    let advertised = require_success(
        runner,
        CommandSpec::new("git").args(["ls-remote", "--refs", remote, reference.as_str()]),
        "rebase_remote_head_unavailable",
        "could not verify the exact remote branch head",
    )?;
    let actual = advertised
        .stdout
        .split_whitespace()
        .next()
        .unwrap_or_default();
    if actual != expected.0 {
        return Err(decision(
            "rebase_stale_lease",
            "remote branch moved since complete plan preflight",
            json!({"branch": branch, "expected_oid": expected, "actual_oid": actual, "resumable": true}),
        ));
    }
    Ok(())
}

fn fetch_exact_pull_request_head(
    runner: &impl CommandRunner,
    remote: &str,
    pr: PrNumber,
    snapshot: &BranchSnapshot,
) -> Result<(), AppError> {
    let reference = format!("refs/pull/{pr}/head");
    let advertised = require_success(
        runner,
        CommandSpec::new("git").args(["ls-remote", remote, reference.as_str()]),
        "rebase_remote_head_unavailable",
        "could not verify the merged predecessor pull-request head",
    )?;
    let actual = advertised
        .stdout
        .split_whitespace()
        .next()
        .unwrap_or_default();
    if actual != snapshot.oid.0 {
        return Err(decision(
            "rebase_stale_lease",
            "merged predecessor pull-request head moved or does not match the retained boundary",
            json!({"pr": pr, "expected_oid": snapshot.oid, "actual_oid": actual, "resumable": true}),
        ));
    }
    require_success(
        runner,
        CommandSpec::new("git").args([
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--refmap=",
            remote,
            reference.as_str(),
        ]),
        "rebase_exact_fetch_failed",
        "could not fetch the exact merged predecessor pull-request head",
    )?;
    Ok(())
}

fn fetch_exact(
    runner: &impl CommandRunner,
    remote: &str,
    snapshot: &BranchSnapshot,
) -> Result<(), AppError> {
    let reference = format!("refs/heads/{}", snapshot.name);
    let advertised = require_success(
        runner,
        CommandSpec::new("git").args(["ls-remote", "--refs", remote, reference.as_str()]),
        "rebase_remote_head_unavailable",
        "could not verify the exact remote branch head",
    )?;
    let actual = advertised
        .stdout
        .split_whitespace()
        .next()
        .unwrap_or_default();
    if actual != snapshot.oid.0 {
        return Err(decision(
            "rebase_stale_lease",
            "remote branch moved since discovery; refusing to simulate or force it",
            json!({"branch": snapshot.name, "expected_oid": snapshot.oid, "actual_oid": actual, "resumable": true}),
        ));
    }
    require_success(
        runner,
        CommandSpec::new("git").args([
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--refmap=",
            remote,
            reference.as_str(),
        ]),
        "rebase_exact_fetch_failed",
        "could not fetch the exact advertised branch object",
    )?;
    // Close the fetch race before any simulation or push.
    let after = require_success(
        runner,
        CommandSpec::new("git").args(["ls-remote", "--refs", remote, reference.as_str()]),
        "rebase_remote_head_unavailable",
        "could not reverify the exact remote branch head",
    )?;
    if after.stdout.split_whitespace().next().unwrap_or_default() != snapshot.oid.0 {
        return Err(decision(
            "rebase_stale_lease",
            "remote branch moved during exact fetch",
            json!({"branch": snapshot.name, "resumable": true}),
        ));
    }
    Ok(())
}

fn rev_parse(runner: &impl CommandRunner, revision: &str) -> Result<CommitOid, AppError> {
    let output = require_success(
        runner,
        CommandSpec::new("git").args(["rev-parse", "--verify", revision]),
        "rebase_result_invalid",
        "could not resolve the simulated rebase result",
    )?;
    let oid = CommitOid(output.stdout.trim().to_owned());
    validate_oid(&oid)?;
    Ok(oid)
}

fn validate_branch(branch: &str) -> Result<(), AppError> {
    if branch.is_empty() || branch.starts_with('-') || branch.contains(['\n', '\r', '\0']) {
        return Err(decision(
            "rebase_branch_invalid",
            "candidate head branch is not safe to pass to Git",
            json!({"branch": branch}),
        ));
    }
    Ok(())
}

fn validate_oid(oid: &CommitOid) -> Result<(), AppError> {
    if !matches!(oid.0.len(), 40 | 64) || !oid.0.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(decision(
            "rebase_oid_invalid",
            "provider returned an invalid full object ID",
            json!({"oid": oid}),
        ));
    }
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn run(runner: &impl CommandRunner, command: CommandSpec) -> Result<CommandOutput, AppError> {
    runner.run(&command).map_err(|error| {
        decision(
            "rebase_command_failed",
            "could not run isolated cumulative rebase command",
            json!({"command": command.display(), "source": error.to_string(), "resumable": true}),
        )
    })
}

#[allow(clippy::needless_pass_by_value)]
fn require_success(
    runner: &impl CommandRunner,
    command: CommandSpec,
    code: &'static str,
    message: &'static str,
) -> Result<CommandOutput, AppError> {
    let output = run(runner, command.clone())?;
    if output.is_success() {
        Ok(output)
    } else {
        Err(decision(
            code,
            message,
            json!({"command": command.display(), "exit_code": output.code, "stderr": output.stderr, "resumable": true}),
        ))
    }
}

fn decision(code: &'static str, message: &'static str, details: serde_json::Value) -> AppError {
    AppError::structured(ErrorCategory::Validation, code, message, Some(details))
}

fn process_runner(
    directory: &Path,
    timeout: Duration,
    operation_deadline: Option<Instant>,
) -> ProcessRunner {
    let runner = ProcessRunner::in_directory(directory).with_timeout(timeout);
    operation_deadline.map_or(runner.clone(), |deadline| {
        runner.with_operation_deadline(deadline)
    })
}

struct TemporaryWorktree {
    repository: PathBuf,
    path: PathBuf,
    timeout: Duration,
    operation_deadline: Option<Instant>,
}

impl TemporaryWorktree {
    fn create(
        repository: &Path,
        head: &CommitOid,
        timeout: Duration,
        operation_deadline: Option<Instant>,
    ) -> Result<Self, AppError> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("caravan-rebase-{}-{nonce}", std::process::id()));
        let runner = process_runner(repository, timeout, operation_deadline);
        require_success(
            &runner,
            CommandSpec::new("git").args([
                "worktree",
                "add",
                "--quiet",
                "--detach",
                path.to_string_lossy().as_ref(),
                head.0.as_str(),
            ]),
            "rebase_worktree_failed",
            "could not create an isolated temporary worktree",
        )?;
        Ok(Self {
            repository: repository.to_path_buf(),
            path,
            timeout,
            operation_deadline,
        })
    }
}

impl Drop for TemporaryWorktree {
    fn drop(&mut self) {
        let runner = ProcessRunner::in_directory(&self.repository).with_timeout(self.timeout);
        let _ = runner.run(&CommandSpec::new("git").args([
            "worktree",
            "remove",
            "--force",
            self.path.to_string_lossy().as_ref(),
        ]));
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::process::Command;

    use super::*;
    use crate::model::{AutoMergeState, PullRequestState};

    fn git(directory: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(directory)
            .args(arguments)
            .output()
            .expect("run git fixture command");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("UTF-8 git output")
            .trim()
            .to_owned()
    }

    struct Fixture {
        _root: tempfile::TempDir,
        clone: PathBuf,
        repository: RepositoryId,
        old_main: CommitOid,
        new_main: CommitOid,
        feature: CommitOid,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("remote.git");
        git(root.path(), &["init", "--bare", bare.to_str().unwrap()]);
        let clone = root.path().join("clone");
        git(
            root.path(),
            &["clone", bare.to_str().unwrap(), clone.to_str().unwrap()],
        );
        git(&clone, &["config", "user.name", "Caravan Test"]);
        git(&clone, &["config", "user.email", "caravan@example.invalid"]);
        git(&clone, &["checkout", "-b", "main"]);
        std::fs::write(clone.join("base"), "base\n").unwrap();
        std::fs::create_dir_all(clone.join(".github/workflows")).unwrap();
        std::fs::write(
            clone.join(".github/workflows/stack.yml"),
            "on:\n  pull_request:\n    types: [opened, synchronize, reopened, edited, labeled, unlabeled]\njobs: {}\n",
        )
        .unwrap();
        git(&clone, &["add", "base", ".github/workflows/stack.yml"]);
        git(&clone, &["commit", "-m", "base"]);
        let old_main = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "-u", "origin", "main"]);

        git(&clone, &["checkout", "-b", "feature"]);
        std::fs::write(clone.join("feature"), "candidate\n").unwrap();
        git(&clone, &["add", "feature"]);
        git(&clone, &["commit", "-m", "candidate"]);
        let feature = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "-u", "origin", "feature"]);

        git(&clone, &["checkout", "main"]);
        std::fs::write(clone.join("parent"), "parent\n").unwrap();
        git(&clone, &["add", "parent"]);
        git(&clone, &["commit", "-m", "parent"]);
        let new_main = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "origin", "main"]);
        git(&clone, &["checkout", "feature"]);

        Fixture {
            _root: root,
            clone,
            repository: RepositoryId {
                owner: "owner".to_owned(),
                name: "repo".to_owned(),
            },
            old_main,
            new_main,
            feature,
        }
    }

    fn branch(repository: &RepositoryId, name: &str, oid: &CommitOid) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository.clone(),
            name: name.to_owned(),
            oid: oid.clone(),
        }
    }

    fn remote_range(candidate: &PullRequestSnapshot) -> PlannedRangeBase {
        PlannedRangeBase::RemoteBranch {
            branch: candidate.base.clone(),
        }
    }

    #[test]
    fn rewrites_under_exact_lease_without_touching_caller_worktree() {
        let fixture = fixture();
        let before_head = git(&fixture.clone, &["rev-parse", "HEAD"]);
        let before_status = git(&fixture.clone, &["status", "--porcelain"]);
        let candidate = PullRequestSnapshot {
            number: crate::model::PrNumber(7),
            title: "candidate".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &fixture.feature),
            base: branch(&fixture.repository, "main", &fixture.new_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let receipt = rewrite_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            &branch(&fixture.repository, "main", &fixture.new_main),
            &branch(&fixture.repository, "main", &fixture.new_main),
            Duration::from_secs(10),
        )
        .unwrap();

        assert_eq!(receipt.old_head_oid, fixture.feature);
        assert_eq!(receipt.old_base_oid, fixture.old_main);
        assert_eq!(receipt.new_base_oid, fixture.new_main);
        assert_ne!(receipt.new_head_oid, receipt.old_head_oid);
        assert_eq!(receipt.commit_count, 1);
        assert!(receipt.lease.ends_with(&fixture.feature.0));
        assert_eq!(git(&fixture.clone, &["rev-parse", "HEAD"]), before_head);
        assert_eq!(
            git(&fixture.clone, &["status", "--porcelain"]),
            before_status
        );
        assert_eq!(
            git(
                &fixture.clone,
                &["ls-remote", "origin", "refs/heads/feature"]
            )
            .split_whitespace()
            .next(),
            Some(receipt.new_head_oid.0.as_str())
        );
        git(
            &fixture.clone,
            &[
                "merge-base",
                "--is-ancestor",
                &fixture.new_main.0,
                &receipt.new_head_oid.0,
            ],
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn five_member_plan_reuses_exact_simulated_parent_generations() {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("remote.git");
        git(root.path(), &["init", "--bare", bare.to_str().unwrap()]);
        let clone = root.path().join("clone");
        git(
            root.path(),
            &["clone", bare.to_str().unwrap(), clone.to_str().unwrap()],
        );
        git(&clone, &["config", "user.name", "Caravan Test"]);
        git(&clone, &["config", "user.email", "caravan@example.invalid"]);
        git(&clone, &["checkout", "-b", "main"]);
        std::fs::create_dir_all(clone.join(".github/workflows")).unwrap();
        std::fs::write(clone.join("base"), "base\n").unwrap();
        std::fs::write(
            clone.join(".github/workflows/stack.yml"),
            "on:\n  pull_request:\n    types: [opened, synchronize, reopened, edited, labeled, unlabeled]\njobs: {}\n",
        )
        .unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-m", "base"]);
        let main = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "-u", "origin", "main"]);

        let names = ["a", "b", "c", "d", "e"];
        let pr_numbers = [1972, 1962, 1959, 1958, 1946];
        let mut old_heads = Vec::new();
        let mut parent = "main";
        for name in names {
            git(&clone, &["checkout", "-b", name, parent]);
            std::fs::write(clone.join(name), format!("{name}\n")).unwrap();
            git(&clone, &["add", name]);
            git(&clone, &["commit", "-m", name]);
            old_heads.push(CommitOid(git(&clone, &["rev-parse", "HEAD"])));
            git(&clone, &["push", "-u", "origin", name]);
            parent = name;
        }
        git(&clone, &["checkout", "a"]);
        std::fs::write(clone.join("a-repair"), "repair\n").unwrap();
        git(&clone, &["add", "a-repair"]);
        git(&clone, &["commit", "-m", "repair a"]);
        let repaired_a = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "origin", "a"]);

        let repository = RepositoryId {
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        };
        let mut candidates = Vec::new();
        for (index, name) in names.into_iter().enumerate() {
            let (base_name, base_oid) = if index == 0 {
                ("main", main.clone())
            } else if index == 1 {
                ("a", repaired_a.clone())
            } else {
                (names[index - 1], old_heads[index - 1].clone())
            };
            let head_oid = if index == 0 {
                repaired_a.clone()
            } else {
                old_heads[index].clone()
            };
            candidates.push(PullRequestSnapshot {
                number: PrNumber(pr_numbers[index]),
                title: name.to_owned(),
                url: format!("https://example.invalid/{}", pr_numbers[index]),
                state: PullRequestState::Open,
                draft: false,
                head: branch(&repository, name, &head_oid),
                base: branch(&repository, base_name, &base_oid),
                cross_repository: false,
                labels: BTreeSet::from(["caravan".to_owned()]),
                auto_merge: AutoMergeState::disabled(),
                checks: Vec::new(),
                created_at: None,
                merged_at: None,
                updated_at: None,
            });
        }

        let default = branch(&repository, "main", &main);
        let mut target = PlannedBase::Remote(default.clone());
        let mut prepared = Vec::new();
        for candidate in &candidates {
            let item = prepare_candidate(
                &clone,
                &repository,
                candidate,
                remote_range(candidate),
                target,
                &default,
                RebaseExecutionBudget::new(Duration::from_secs(10)),
            )
            .unwrap();
            target = PlannedBase::Simulated(branch(
                &repository,
                &candidate.head.name,
                &item.plan.new_head_oid,
            ));
            prepared.push(item);
        }
        assert_eq!(
            git(&clone, &["ls-remote", "origin", "refs/heads/e"]),
            format!("{}\trefs/heads/e", old_heads[4])
        );
        for item in &prepared {
            verify_prepared(item).unwrap();
        }
        let receipts = prepared
            .iter()
            .map(|item| apply_prepared(item).unwrap())
            .collect::<Vec<_>>();
        for pair in receipts.windows(2) {
            let status = std::process::Command::new("git")
                .current_dir(&clone)
                .args([
                    "merge-base",
                    "--is-ancestor",
                    pair[0].new_head_oid.0.as_str(),
                    pair[1].new_head_oid.0.as_str(),
                ])
                .status()
                .unwrap();
            assert!(status.success());
        }
        assert_eq!(receipts.len(), 5);
        assert_eq!(receipts[4].branch, "e");

        // A merged/deleted predecessor remains an exact range boundary through
        // GitHub's durable pull-request head ref, so B can be promoted to head.
        drop(prepared);
        git(&clone, &["checkout", "main"]);
        git(
            &clone,
            &["reset", "--hard", receipts[0].new_head_oid.0.as_str()],
        );
        git(&clone, &["push", "origin", "main"]);
        git(
            root.path(),
            &[
                "--git-dir",
                bare.to_str().unwrap(),
                "update-ref",
                "refs/pull/1972/head",
                receipts[0].new_head_oid.0.as_str(),
            ],
        );
        git(&clone, &["push", "origin", "--delete", "a"]);
        let mut promoted = candidates[1].clone();
        promoted.head.oid = receipts[1].new_head_oid.clone();
        promoted.base.oid = receipts[0].new_head_oid.clone();
        let promoted_plan = prepare_candidate(
            &clone,
            &repository,
            &promoted,
            PlannedRangeBase::PullRequestHead {
                pr: PrNumber(1972),
                branch: promoted.base.clone(),
            },
            PlannedBase::Remote(branch(&repository, "main", &receipts[0].new_head_oid)),
            &branch(&repository, "main", &receipts[0].new_head_oid),
            RebaseExecutionBudget::new(Duration::from_secs(10)),
        )
        .expect("merged predecessor PR ref retains exact range");
        assert_eq!(promoted_plan.plan.pr, PrNumber(1962));
    }

    #[test]
    fn planning_conflict_never_pushes_the_remote_branch() {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("remote.git");
        git(root.path(), &["init", "--bare", bare.to_str().unwrap()]);
        let clone = root.path().join("clone");
        git(
            root.path(),
            &["clone", bare.to_str().unwrap(), clone.to_str().unwrap()],
        );
        git(&clone, &["config", "user.name", "Caravan Test"]);
        git(&clone, &["config", "user.email", "caravan@example.invalid"]);
        git(&clone, &["checkout", "-b", "main"]);
        std::fs::create_dir_all(clone.join(".github/workflows")).unwrap();
        std::fs::write(clone.join("shared"), "base\n").unwrap();
        std::fs::write(
            clone.join(".github/workflows/stack.yml"),
            "on:\n  pull_request:\n    types: [opened, synchronize, reopened, edited, labeled, unlabeled]\njobs: {}\n",
        )
        .unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-m", "base"]);
        git(&clone, &["push", "-u", "origin", "main"]);
        git(&clone, &["checkout", "-b", "feature"]);
        std::fs::write(clone.join("shared"), "feature\n").unwrap();
        git(&clone, &["commit", "-am", "feature"]);
        let feature = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "-u", "origin", "feature"]);
        git(&clone, &["checkout", "main"]);
        std::fs::write(clone.join("shared"), "parent\n").unwrap();
        git(&clone, &["commit", "-am", "parent"]);
        let main = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "origin", "main"]);

        let repository = RepositoryId {
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        };
        let candidate = PullRequestSnapshot {
            number: PrNumber(7),
            title: "feature".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&repository, "feature", &feature),
            base: branch(&repository, "main", &main),
            cross_repository: false,
            labels: BTreeSet::from(["caravan".to_owned()]),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let before = git(&clone, &["ls-remote", "origin", "refs/heads/feature"]);
        let error = prepare_candidate(
            &clone,
            &repository,
            &candidate,
            remote_range(&candidate),
            PlannedBase::Remote(branch(&repository, "main", &main)),
            &branch(&repository, "main", &main),
            RebaseExecutionBudget::new(Duration::from_secs(10)),
        )
        .err()
        .expect("conflict must fail planning");
        assert_eq!(mcp_cli::StructuredError::code(&error), "rebase_conflict");
        assert_eq!(
            mcp_cli::StructuredError::details(&error).unwrap()["conflicting_paths"],
            serde_json::json!(["shared"])
        );
        assert_eq!(
            git(&clone, &["ls-remote", "origin", "refs/heads/feature"]),
            before
        );
    }

    #[test]
    fn apply_time_lease_race_preserves_the_external_head() {
        let fixture = fixture();
        let candidate = PullRequestSnapshot {
            number: PrNumber(7),
            title: "candidate".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &fixture.feature),
            base: branch(&fixture.repository, "main", &fixture.new_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let prepared = prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            remote_range(&candidate),
            PlannedBase::Remote(branch(&fixture.repository, "main", &fixture.new_main)),
            &branch(&fixture.repository, "main", &fixture.new_main),
            RebaseExecutionBudget::new(Duration::from_secs(10)),
        )
        .unwrap();
        std::fs::write(fixture.clone.join("external-race"), "race\n").unwrap();
        git(&fixture.clone, &["add", "external-race"]);
        git(&fixture.clone, &["commit", "-m", "external race"]);
        let external = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "origin", "feature"]);

        let error = apply_prepared(&prepared).expect_err("lease race must fail");

        assert_eq!(mcp_cli::StructuredError::code(&error), "rebase_stale_lease");
        assert!(
            git(
                &fixture.clone,
                &["ls-remote", "origin", "refs/heads/feature"]
            )
            .starts_with(&external.0)
        );
    }

    #[test]
    fn stale_snapshot_is_typed_and_never_overwrites_remote() {
        let fixture = fixture();
        let candidate = PullRequestSnapshot {
            number: crate::model::PrNumber(7),
            title: String::new(),
            url: String::new(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &CommitOid("0".repeat(40))),
            base: branch(&fixture.repository, "main", &fixture.new_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let error = rewrite_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            &branch(&fixture.repository, "main", &fixture.new_main),
            &branch(&fixture.repository, "main", &fixture.new_main),
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert_eq!(mcp_cli::StructuredError::code(&error), "rebase_stale_lease");
        assert_eq!(
            git(
                &fixture.clone,
                &["ls-remote", "origin", "refs/heads/feature"]
            )
            .split_whitespace()
            .next(),
            Some(fixture.feature.0.as_str())
        );
    }
}
