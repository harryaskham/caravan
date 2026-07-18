//! Read-only GitHub repository and pull-request discovery.
//!
//! This module deliberately stops at faithful provider conversion. Graph policy
//! and every GitHub mutation live in downstream lanes.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::command::{CommandOutput, CommandRunError, CommandRunner, CommandSpec, ProcessRunner};
use crate::model::{
    self, AutoMergeState, BranchSnapshot, CheckState, CommitOid, MergeMethod, MutationKind,
    PrNumber, PullRequestPrecondition, RepositoryId,
};

const PR_JSON_FIELDS: &str = "number,title,state,isDraft,headRefName,headRefOid,headRepository,headRepositoryOwner,isCrossRepository,baseRefName,baseRefOid,labels,autoMergeRequest,statusCheckRollup,createdAt,mergedAt,url,updatedAt";
const WORKFLOW_RUN_JSON_FIELDS: &str =
    "databaseId,headSha,status,conclusion,event,name,workflowName,url";

/// Limits and label used by one discovery pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryOptions {
    /// Label identifying active caravan members.
    pub label: String,
    /// Maximum active labelled PRs to fetch.
    pub open_limit: usize,
    /// Maximum recently updated merged labelled PRs to fetch.
    pub merged_limit: usize,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            label: "caravan".to_owned(),
            open_limit: 1_000,
            merged_limit: 100,
        }
    }
}

/// Bounded, separately captured evidence from a provider JSON decode failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonDecodeEvidence {
    pub stdout: String,
    pub stderr: String,
}

/// Typed failure from repository discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryError {
    /// The runner could not execute or decode a command.
    Runner(CommandRunError),
    /// A command returned a non-zero status.
    CommandFailed {
        /// Requested command.
        command: CommandSpec,
        /// Exit code, absent when terminated by a signal.
        code: Option<i32>,
        /// Trimmed diagnostic output.
        stderr: String,
    },
    /// A successful `gh` command returned an unexpected JSON shape.
    InvalidJson {
        /// Requested command.
        command: CommandSpec,
        /// Serde diagnostic.
        message: String,
        /// Boxed to keep the frequently returned discovery error compact.
        evidence: Box<JsonDecodeEvidence>,
    },
    /// `gh repo view` reported no default branch.
    MissingDefaultBranch,
    /// A repository slug was not canonical `owner/name` form.
    InvalidRepositorySlug(String),
    /// More than one open PR maps to the current local branch.
    AmbiguousCurrentPullRequest {
        /// Current branch name.
        branch: String,
        /// Candidate PR numbers.
        candidates: Vec<u64>,
    },
    /// A PR has no resolvable head repository.
    MissingHeadRepository {
        /// Affected PR number.
        pr: u64,
    },
    /// An active labelled PR uses a fork-only head branch.
    ForkOnlyHead {
        /// Affected PR number.
        pr: u64,
        /// Head repository reported by GitHub.
        head_repository: String,
        /// Required base repository.
        base_repository: String,
    },
    /// A query limit was zero.
    InvalidLimit(&'static str),
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runner(error) => error.fmt(formatter),
            Self::CommandFailed {
                command,
                code,
                stderr,
            } => write!(
                formatter,
                "`{}` failed with status {}: {stderr}",
                command.display(),
                code.map_or_else(|| "signal".to_owned(), |code| code.to_string())
            ),
            Self::InvalidJson {
                command, message, ..
            } => write!(
                formatter,
                "`{}` returned invalid JSON: {message}",
                command.display()
            ),
            Self::MissingDefaultBranch => write!(formatter, "repository has no default branch"),
            Self::InvalidRepositorySlug(slug) => {
                write!(formatter, "repository slug `{slug}` is not owner/name")
            }
            Self::AmbiguousCurrentPullRequest { branch, candidates } => write!(
                formatter,
                "branch `{branch}` maps to multiple open pull requests: {candidates:?}"
            ),
            Self::MissingHeadRepository { pr } => {
                write!(formatter, "PR #{pr} has no resolvable head repository")
            }
            Self::ForkOnlyHead {
                pr,
                head_repository,
                base_repository,
            } => write!(
                formatter,
                "active labelled PR #{pr} uses fork-only head repository `{head_repository}`; Caravan predecessors must use `{base_repository}`"
            ),
            Self::InvalidLimit(name) => {
                write!(formatter, "discovery limit `{name}` must be positive")
            }
        }
    }
}

impl std::error::Error for DiscoveryError {}

impl From<CommandRunError> for DiscoveryError {
    fn from(error: CommandRunError) -> Self {
        Self::Runner(error)
    }
}

/// Non-interactive inputs for `gh pr create --fill`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CreatePullRequestInput {
    /// Head branch, optionally in `owner:branch` form.
    pub head: String,
    /// Base branch in the selected repository.
    pub base: String,
    /// Whether to open the PR as a draft.
    #[serde(default)]
    pub draft: bool,
}

/// Provider workflow-run facts used to select an exact failed run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowRunSnapshot {
    /// GitHub Actions database identifier.
    pub database_id: u64,
    /// PRs GitHub associates with this run. Empty for list projections that do
    /// not expose association; the exact rerun read always populates it.
    #[serde(default)]
    pub pull_requests: Vec<PrNumber>,
    /// Exact head commit for the run.
    pub head_sha: String,
    /// Provider execution status, preserved verbatim.
    pub status: String,
    /// Provider conclusion, preserved verbatim.
    pub conclusion: String,
    /// Trigger event.
    pub event: String,
    /// Run name.
    pub name: String,
    /// Workflow name.
    pub workflow_name: String,
    /// Canonical browser URL.
    pub url: String,
}

/// Exact provider receipt for one primitive PR mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubMutationReceipt {
    /// Primitive provider operation performed.
    pub kind: MutationKind,
    /// Exact PR facts immediately before mutation; absent only for creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<model::PullRequestSnapshot>,
    /// Exact PR facts refetched after the provider command completed.
    pub after: model::PullRequestSnapshot,
    /// Trimmed non-secret provider output, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_output: Option<String>,
}

/// Typed failure from an optimistic provider primitive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationError {
    /// Command execution, JSON conversion, or provider failure.
    Provider(DiscoveryError),
    /// Exact PR facts no longer match the caller's precondition.
    StalePrecondition {
        /// Facts supplied by the caller.
        expected: Box<PullRequestPrecondition>,
        /// Fresh facts observed immediately before mutation.
        actual: Box<PullRequestPrecondition>,
        /// Stable field names that changed.
        changed_fields: Vec<String>,
    },
    /// `gh pr create` succeeded without identifying the created PR.
    MissingCreatedPullRequest {
        /// Trimmed provider output.
        provider_output: String,
    },
    /// The authenticated actor cannot perform an administrator merge.
    PermissionDenied {
        /// Required repository permission.
        required: String,
        /// Permission reported by GitHub.
        actual: String,
    },
    /// A branch moved after an exact compatibility proof.
    BranchHeadMismatch {
        /// Branch name that moved.
        branch: String,
        /// Exact revision used by the caller's proof.
        expected: CommitOid,
        /// Fresh provider revision.
        actual: CommitOid,
    },
    /// A requested Actions run is not associated with the selected PR.
    RunPullRequestMismatch {
        /// Workflow run ID.
        run_id: u64,
        /// Selected PR that must own the run.
        expected_pr: PrNumber,
        /// PR numbers reported by GitHub for the run.
        actual_prs: Vec<PrNumber>,
    },
    /// A requested Actions run belongs to another commit.
    RunHeadMismatch {
        /// Workflow run ID.
        run_id: u64,
        /// PR head expected by the caller.
        expected_head: String,
        /// Head observed on the run.
        actual_head: String,
    },
    /// A requested Actions run is not currently failed.
    RunNotFailed {
        /// Workflow run ID.
        run_id: u64,
        /// Provider conclusion.
        conclusion: String,
    },
}

impl std::fmt::Display for MutationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Provider(error) => error.fmt(formatter),
            Self::StalePrecondition { changed_fields, .. } => write!(
                formatter,
                "pull-request precondition changed: {}",
                changed_fields.join(", ")
            ),
            Self::MissingCreatedPullRequest { provider_output } => write!(
                formatter,
                "gh created a pull request but returned no PR URL: {provider_output}"
            ),
            Self::PermissionDenied { required, actual } => write!(
                formatter,
                "repository permission `{actual}` cannot perform admin merge; `{required}` required"
            ),
            Self::BranchHeadMismatch {
                branch,
                expected,
                actual,
            } => write!(
                formatter,
                "branch `{branch}` moved from {expected} to {actual} after preflight"
            ),
            Self::RunPullRequestMismatch {
                run_id,
                expected_pr,
                actual_prs,
            } => write!(
                formatter,
                "workflow run {run_id} belongs to PRs {actual_prs:?}, expected PR {expected_pr}"
            ),
            Self::RunHeadMismatch {
                run_id,
                expected_head,
                actual_head,
            } => write!(
                formatter,
                "workflow run {run_id} belongs to {actual_head}, expected {expected_head}"
            ),
            Self::RunNotFailed { run_id, conclusion } => write!(
                formatter,
                "workflow run {run_id} is not failed (conclusion: {conclusion})"
            ),
        }
    }
}

impl std::error::Error for MutationError {}

impl From<DiscoveryError> for MutationError {
    fn from(error: DiscoveryError) -> Self {
        Self::Provider(error)
    }
}

/// Policy-free optimistic mutation primitives over authenticated `gh`.
#[derive(Debug, Clone)]
pub struct GitHubMutationAdapter<R> {
    runner: R,
}

impl GitHubMutationAdapter<ProcessRunner> {
    /// Execute provider commands from the process's current repository.
    #[must_use]
    pub fn current_directory() -> Self {
        Self::new(ProcessRunner::new())
    }
}

impl<R: CommandRunner> GitHubMutationAdapter<R> {
    /// Construct an adapter over an injected command runner.
    #[must_use]
    pub const fn new(runner: R) -> Self {
        Self { runner }
    }

    /// Resolve only repository identity and default-branch name. This bounded
    /// initialization read deliberately performs no PR or Git-ref discovery.
    pub fn repository_identity(&self) -> Result<(RepositoryId, String), MutationError> {
        let repository_json: RepositoryJson = self.json(repository_command())?;
        let default_branch = repository_json
            .default_branch_ref
            .ok_or(DiscoveryError::MissingDefaultBranch)?
            .name;
        let repository = repository_id(&repository_json.name_with_owner)?;
        Ok((repository, default_branch))
    }

    /// Verify that a branch still points at an exact preflight revision.
    pub fn verify_branch_head(
        &self,
        repository: &RepositoryId,
        branch: &str,
        expected: &CommitOid,
    ) -> Result<(), MutationError> {
        let reference: GitRefJson =
            self.json(default_branch_command(&repository.slug(), branch))?;
        let actual = CommitOid(reference.object.sha);
        if &actual != expected {
            return Err(MutationError::BranchHeadMismatch {
                branch: branch.to_owned(),
                expected: expected.clone(),
                actual,
            });
        }
        Ok(())
    }

    /// Whether GitHub reports protection on a repository branch.
    pub fn branch_is_protected(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<bool, MutationError> {
        let branch: BranchSettingsJson = self.json(branch_settings_command(repository, branch))?;
        Ok(branch.protected)
    }

    /// Exact classic default-branch check/review requirements used by
    /// repository initialization. A protected branch with no such requirement
    /// deliberately returns an empty policy rather than being considered ready.
    pub fn default_branch_policy(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<DefaultBranchPolicy, MutationError> {
        let policy: BranchProtectionJson =
            self.json(branch_protection_command(repository, branch))?;
        let mut required_status_checks = policy
            .required_status_checks
            .as_ref()
            .map(|checks| checks.contexts.clone())
            .unwrap_or_default();
        if let Some(checks) = policy.required_status_checks.as_ref() {
            required_status_checks.extend(checks.checks.iter().map(|check| check.context.clone()));
        }
        required_status_checks.sort();
        required_status_checks.dedup();
        Ok(DefaultBranchPolicy {
            required_status_checks,
            strict_status_checks: policy
                .required_status_checks
                .as_ref()
                .is_some_and(|checks| checks.strict),
            required_approving_review_count: policy
                .required_pull_request_reviews
                .map_or(0, |reviews| reviews.required_approving_review_count),
        })
    }

    /// Whether repository settings permit GitHub auto-merge.
    pub fn repository_allows_auto_merge(
        &self,
        repository: &RepositoryId,
    ) -> Result<bool, MutationError> {
        let settings: RepositorySettingsJson =
            self.json(repository_settings_command(repository))?;
        Ok(settings.allow_auto_merge && settings.allow_squash_merge)
    }

    /// List repository label names for mutation preflight.
    pub fn repository_labels(
        &self,
        repository: &RepositoryId,
    ) -> Result<BTreeSet<String>, MutationError> {
        Ok(self
            .repository_label_definitions(repository)?
            .into_iter()
            .map(|label| label.name)
            .collect())
    }

    /// List exact repository label metadata for initialization verification.
    pub fn repository_label_definitions(
        &self,
        repository: &RepositoryId,
    ) -> Result<Vec<RepositoryLabel>, MutationError> {
        self.json(repository_labels_command(repository))
    }

    /// Return the authenticated actor's repository permission.
    pub fn repository_permission(
        &self,
        repository: &RepositoryId,
    ) -> Result<String, MutationError> {
        let permission: RepositoryPermissionJson =
            self.json(repository_permission_command(repository))?;
        Ok(permission.viewer_permission)
    }

    /// Create one repository label. Callers must re-read exact metadata after
    /// this command because provider timeout and concurrent creation are
    /// intentionally treated as indeterminate.
    pub fn create_repository_label(
        &self,
        repository: &RepositoryId,
        name: &str,
        color: &str,
        description: &str,
    ) -> Result<(), MutationError> {
        self.checked(create_repository_label_command(
            repository,
            name,
            color,
            description,
        ))?;
        Ok(())
    }

    /// Refetch one PR by number without applying policy.
    pub fn refetch_pull_request(
        &self,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<model::PullRequestSnapshot, MutationError> {
        self.refetch_selector(repository, &number.to_string())
    }

    /// Refetch and compare every mutation-sensitive fact.
    pub fn verify_precondition(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<model::PullRequestSnapshot, MutationError> {
        let actual_snapshot = self.refetch_pull_request(repository, expected.number)?;
        let actual = PullRequestPrecondition::from(&actual_snapshot);
        let changed_fields = changed_precondition_fields(expected, &actual);
        if changed_fields.is_empty() {
            Ok(actual_snapshot)
        } else {
            Err(MutationError::StalePrecondition {
                expected: Box::new(expected.clone()),
                actual: Box::new(actual),
                changed_fields,
            })
        }
    }

    /// Create a PR non-interactively with commit-derived title/body and refetch it.
    pub fn create_pull_request(
        &self,
        repository: &RepositoryId,
        input: &CreatePullRequestInput,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        let command = create_pull_request_command(repository, input);
        let output = self.checked(command)?;
        let Some(url) = output
            .stdout
            .split_whitespace()
            .find(|token| token.starts_with("https://"))
        else {
            return Err(MutationError::MissingCreatedPullRequest {
                provider_output: output.stdout.trim().to_owned(),
            });
        };
        let after = self.refetch_selector(repository, url)?;
        Ok(GitHubMutationReceipt {
            kind: MutationKind::CreatePullRequest,
            before: None,
            after,
            provider_output: trimmed_provider_output(&output),
        })
    }

    /// Change a PR's base branch after exact precondition verification.
    pub fn set_base(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        base: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.mutate_pull_request(
            repository,
            expected,
            MutationKind::SetBase,
            edit_pull_request_command(repository, expected.number, "--base", base),
        )
    }

    /// Add one label after exact precondition verification.
    pub fn add_label(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.mutate_pull_request(
            repository,
            expected,
            MutationKind::AddLabel,
            edit_pull_request_command(repository, expected.number, "--add-label", label),
        )
    }

    /// Remove one label after exact precondition verification.
    pub fn remove_label(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.mutate_pull_request(
            repository,
            expected,
            MutationKind::RemoveLabel,
            edit_pull_request_command(repository, expected.number, "--remove-label", label),
        )
    }

    /// Enable squash auto-merge after exact precondition verification.
    pub fn enable_squash_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.mutate_pull_request(
            repository,
            expected,
            MutationKind::EnableAutoMerge,
            auto_merge_command(repository, expected.number, false),
        )
    }

    /// Disable auto-merge after exact precondition verification.
    pub fn disable_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.mutate_pull_request(
            repository,
            expected,
            MutationKind::DisableAutoMerge,
            auto_merge_command(repository, expected.number, true),
        )
    }

    /// List failed Actions runs for the exact PR head after verification.
    pub fn failed_runs_for_pull_request(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<Vec<WorkflowRunSnapshot>, MutationError> {
        let before = self.verify_precondition(repository, expected)?;
        let runs: Vec<WorkflowRunJson> =
            self.json(failed_runs_command(repository, before.head.oid.0.as_str()))?;
        Ok(runs
            .into_iter()
            .map(Into::into)
            .filter(|run: &WorkflowRunSnapshot| run.head_sha == before.head.oid.0)
            .collect())
    }

    /// Rerun failed jobs for one exact Actions run after PR verification.
    pub fn rerun_failed_run(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        run_id: u64,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        let run: WorkflowRunSnapshot = self
            .json::<WorkflowRunDetailsJson>(workflow_run_command(repository, run_id))?
            .into();
        let before = self.verify_precondition(repository, expected)?;
        if !run.pull_requests.contains(&expected.number) {
            return Err(MutationError::RunPullRequestMismatch {
                run_id,
                expected_pr: expected.number,
                actual_prs: run.pull_requests,
            });
        }
        if run.head_sha != before.head.oid.0 {
            return Err(MutationError::RunHeadMismatch {
                run_id,
                expected_head: before.head.oid.0,
                actual_head: run.head_sha,
            });
        }
        if !run.conclusion.eq_ignore_ascii_case("failure") {
            return Err(MutationError::RunNotFailed {
                run_id,
                conclusion: run.conclusion,
            });
        }
        let output = self.checked(rerun_failed_command(repository, run_id))?;
        let after = self.refetch_pull_request(repository, expected.number)?;
        Ok(GitHubMutationReceipt {
            kind: MutationKind::RerunChecks,
            before: Some(before),
            after,
            provider_output: trimmed_provider_output(&output),
        })
    }

    /// Force-squash one PR only when GitHub reports administrator permission.
    pub fn admin_squash_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        let permission: RepositoryPermissionJson =
            self.json(repository_permission_command(repository))?;
        if permission.viewer_permission != "ADMIN" {
            return Err(MutationError::PermissionDenied {
                required: "ADMIN".to_owned(),
                actual: permission.viewer_permission,
            });
        }
        self.mutate_pull_request(
            repository,
            expected,
            MutationKind::SquashMerge,
            admin_squash_merge_command(repository, expected.number),
        )
    }

    fn mutate_pull_request(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        kind: MutationKind,
        command: CommandSpec,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        let before = self.verify_precondition(repository, expected)?;
        let output = self.checked(command)?;
        let after = self.refetch_pull_request(repository, expected.number)?;
        Ok(GitHubMutationReceipt {
            kind,
            before: Some(before),
            after,
            provider_output: trimmed_provider_output(&output),
        })
    }

    fn refetch_selector(
        &self,
        repository: &RepositoryId,
        selector: &str,
    ) -> Result<model::PullRequestSnapshot, MutationError> {
        let pull_request: PullRequestJson =
            self.json(pull_request_command(repository, selector))?;
        pull_request.into_snapshot(repository).map_err(Into::into)
    }

    fn checked(&self, command: CommandSpec) -> Result<CommandOutput, MutationError> {
        let output = self.runner.run(&command).map_err(DiscoveryError::from)?;
        if output.is_success() {
            Ok(output)
        } else {
            Err(DiscoveryError::CommandFailed {
                command,
                code: output.code,
                stderr: output.stderr.trim().to_owned(),
            }
            .into())
        }
    }

    fn json<T: DeserializeOwned>(&self, command: CommandSpec) -> Result<T, MutationError> {
        let output = self.checked(command.clone())?;
        serde_json::from_str(&output.stdout)
            .map_err(|error| DiscoveryError::InvalidJson {
                command,
                message: error.to_string(),
                evidence: Box::new(JsonDecodeEvidence {
                    stdout: diagnostic_excerpt(&output.stdout),
                    stderr: diagnostic_excerpt(&output.stderr),
                }),
            })
            .map_err(Into::into)
    }
}

fn changed_precondition_fields(
    expected: &PullRequestPrecondition,
    actual: &PullRequestPrecondition,
) -> Vec<String> {
    let mut changed = Vec::new();
    if expected.number != actual.number {
        changed.push("number".to_owned());
    }
    if expected.state != actual.state {
        changed.push("state".to_owned());
    }
    if expected.head_oid != actual.head_oid {
        changed.push("head_oid".to_owned());
    }
    if expected.base_ref != actual.base_ref {
        changed.push("base_ref".to_owned());
    }
    if expected.base_oid != actual.base_oid {
        changed.push("base_oid".to_owned());
    }
    if expected.labels != actual.labels {
        changed.push("labels".to_owned());
    }
    if expected.auto_merge != actual.auto_merge {
        changed.push("auto_merge".to_owned());
    }
    changed
}

fn trimmed_provider_output(output: &CommandOutput) -> Option<String> {
    let value = output.stdout.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn diagnostic_excerpt(value: &str) -> String {
    const EDGE_BYTES: usize = 4 * 1024;
    if value.len() <= EDGE_BYTES * 2 {
        return value.to_owned();
    }
    let mut prefix_end = EDGE_BYTES;
    while !value.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }
    let mut suffix_start = value.len() - EDGE_BYTES;
    while !value.is_char_boundary(suffix_start) {
        suffix_start += 1;
    }
    format!(
        "{}\n...[{} bytes omitted]...\n{}",
        &value[..prefix_end],
        suffix_start.saturating_sub(prefix_end),
        &value[suffix_start..]
    )
}

/// Read-only adapter over authenticated `gh` and local `git`.
#[derive(Debug, Clone)]
pub struct GitHubDiscovery<R> {
    runner: R,
    options: DiscoveryOptions,
}

impl GitHubDiscovery<ProcessRunner> {
    /// Discover from the process's current repository.
    #[must_use]
    pub fn current_directory() -> Self {
        Self::new(ProcessRunner::new())
    }
}

impl<R: CommandRunner> GitHubDiscovery<R> {
    /// Construct discovery with default label and bounds.
    #[must_use]
    pub fn new(runner: R) -> Self {
        Self {
            runner,
            options: DiscoveryOptions::default(),
        }
    }

    /// Override label and result bounds.
    #[must_use]
    pub fn with_options(mut self, options: DiscoveryOptions) -> Self {
        self.options = options;
        self
    }

    /// Run a complete, internally consistent read-only discovery pass.
    pub fn discover(&self) -> Result<model::RepositorySnapshot, DiscoveryError> {
        if self.options.open_limit == 0 {
            return Err(DiscoveryError::InvalidLimit("open_limit"));
        }
        if self.options.merged_limit == 0 {
            return Err(DiscoveryError::InvalidLimit("merged_limit"));
        }

        let repository_json: RepositoryJson = self.json(repository_command())?;
        let default_branch_name = repository_json
            .default_branch_ref
            .ok_or(DiscoveryError::MissingDefaultBranch)?
            .name;
        let repository = repository_id(&repository_json.name_with_owner)?;
        let current_branch = self.current_branch()?;
        let default_ref: GitRefJson = self.json(default_branch_command(
            &repository.slug(),
            &default_branch_name,
        ))?;
        let default_branch = BranchSnapshot {
            repository: repository.clone(),
            name: default_branch_name,
            oid: CommitOid(default_ref.object.sha),
        };

        let current_pr = match &current_branch {
            Some(branch) => {
                let mut matches = self
                    .pull_requests(current_pr_command(&repository.slug(), branch), &repository)?;
                match matches.len() {
                    0 => None,
                    1 => matches.pop(),
                    _ => {
                        return Err(DiscoveryError::AmbiguousCurrentPullRequest {
                            branch: branch.clone(),
                            candidates: matches.iter().map(|pr| pr.number.0).collect(),
                        });
                    }
                }
            }
            None => None,
        };

        let open_labeled_prs = self.pull_requests(
            labeled_pr_command(
                &repository.slug(),
                "open",
                &self.options.label,
                self.options.open_limit,
                false,
            ),
            &repository,
        )?;
        validate_active_heads(&repository, &open_labeled_prs)?;
        // Status and ready-PR hooks need every open PR, not only members that
        // already carry the active label. The labelled query remains separate
        // so active-member bounds and fork validation stay explicit.
        let all_open_prs = self.pull_requests(
            open_pr_command(&repository.slug(), self.options.open_limit),
            &repository,
        )?;
        let recently_merged_labeled_prs = self.pull_requests(
            labeled_pr_command(
                &repository.slug(),
                "merged",
                &self.options.label,
                self.options.merged_limit,
                true,
            ),
            &repository,
        )?;

        let current_pr_number = current_pr.as_ref().map(|pr| pr.number);
        let mut pull_requests = BTreeMap::new();
        for pull_request in all_open_prs
            .into_iter()
            .chain(open_labeled_prs)
            .chain(recently_merged_labeled_prs)
            .chain(current_pr)
        {
            pull_requests
                .entry(pull_request.number)
                .or_insert(pull_request);
        }

        Ok(model::RepositorySnapshot {
            repository,
            default_branch,
            current_branch,
            current_pr: current_pr_number,
            pull_requests: pull_requests.into_values().collect(),
            observed_at: None,
        })
    }

    fn current_branch(&self) -> Result<Option<String>, DiscoveryError> {
        let command = current_branch_command();
        let output = self.runner.run(&command)?;
        if output.code == Some(1) {
            return Ok(None);
        }
        if !output.is_success() {
            return Err(DiscoveryError::CommandFailed {
                command,
                code: output.code,
                stderr: output.stderr.trim().to_owned(),
            });
        }
        let branch = output.stdout.trim();
        if branch.is_empty() {
            Ok(None)
        } else {
            Ok(Some(branch.to_owned()))
        }
    }

    fn pull_requests(
        &self,
        command: CommandSpec,
        repository: &RepositoryId,
    ) -> Result<Vec<model::PullRequestSnapshot>, DiscoveryError> {
        self.json::<Vec<PullRequestJson>>(command)?
            .into_iter()
            .map(|pull_request| pull_request.into_snapshot(repository))
            .collect()
    }

    fn json<T: DeserializeOwned>(&self, command: CommandSpec) -> Result<T, DiscoveryError> {
        let output = self.runner.run(&command)?;
        if !output.is_success() {
            return Err(DiscoveryError::CommandFailed {
                command,
                code: output.code,
                stderr: output.stderr.trim().to_owned(),
            });
        }
        serde_json::from_str(&output.stdout).map_err(|error| DiscoveryError::InvalidJson {
            command,
            message: error.to_string(),
            evidence: Box::new(JsonDecodeEvidence {
                stdout: diagnostic_excerpt(&output.stdout),
                stderr: diagnostic_excerpt(&output.stderr),
            }),
        })
    }
}

fn validate_active_heads(
    repository: &RepositoryId,
    pull_requests: &[model::PullRequestSnapshot],
) -> Result<(), DiscoveryError> {
    for pull_request in pull_requests {
        if pull_request.cross_repository || pull_request.head.repository != *repository {
            return Err(DiscoveryError::ForkOnlyHead {
                pr: pull_request.number.0,
                head_repository: pull_request.head.repository.slug(),
                base_repository: repository.slug(),
            });
        }
    }
    Ok(())
}

fn repository_id(slug: &str) -> Result<RepositoryId, DiscoveryError> {
    let Some((owner, name)) = slug.split_once('/') else {
        return Err(DiscoveryError::InvalidRepositorySlug(slug.to_owned()));
    };
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return Err(DiscoveryError::InvalidRepositorySlug(slug.to_owned()));
    }
    Ok(RepositoryId {
        owner: owner.to_owned(),
        name: name.to_owned(),
    })
}

fn repository_command() -> CommandSpec {
    CommandSpec::new("gh").args(["repo", "view", "--json", "nameWithOwner,defaultBranchRef"])
}

fn current_branch_command() -> CommandSpec {
    CommandSpec::new("git").args(["symbolic-ref", "--quiet", "--short", "HEAD"])
}

fn default_branch_command(repository: &str, branch: &str) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        format!(
            "repos/{repository}/git/ref/heads/{}",
            encode_path_segment(branch)
        ),
    ])
}

fn current_pr_command(repository: &str, branch: &str) -> CommandSpec {
    CommandSpec::new("gh").args([
        "pr",
        "list",
        "--repo",
        repository,
        "--state",
        "open",
        "--head",
        branch,
        "--limit",
        "2",
        "--json",
        PR_JSON_FIELDS,
    ])
}

fn open_pr_command(repository: &str, limit: usize) -> CommandSpec {
    CommandSpec::new("gh").args([
        "pr".to_owned(),
        "list".to_owned(),
        "--repo".to_owned(),
        repository.to_owned(),
        "--state".to_owned(),
        "open".to_owned(),
        "--limit".to_owned(),
        limit.to_string(),
        "--json".to_owned(),
        PR_JSON_FIELDS.to_owned(),
    ])
}

fn pull_request_command(repository: &RepositoryId, selector: &str) -> CommandSpec {
    CommandSpec::new("gh").args([
        "pr",
        "view",
        selector,
        "--repo",
        repository.slug().as_str(),
        "--json",
        PR_JSON_FIELDS,
    ])
}

fn create_pull_request_command(
    repository: &RepositoryId,
    input: &CreatePullRequestInput,
) -> CommandSpec {
    let mut command = CommandSpec::new("gh").args([
        "pr".to_owned(),
        "create".to_owned(),
        "--repo".to_owned(),
        repository.slug(),
        "--head".to_owned(),
        input.head.clone(),
        "--base".to_owned(),
        input.base.clone(),
        "--fill".to_owned(),
    ]);
    if input.draft {
        command = command.arg("--draft");
    }
    command
}

fn edit_pull_request_command(
    repository: &RepositoryId,
    number: PrNumber,
    flag: &str,
    value: &str,
) -> CommandSpec {
    CommandSpec::new("gh").args([
        "pr".to_owned(),
        "edit".to_owned(),
        number.to_string(),
        "--repo".to_owned(),
        repository.slug(),
        flag.to_owned(),
        value.to_owned(),
    ])
}

fn auto_merge_command(repository: &RepositoryId, number: PrNumber, disable: bool) -> CommandSpec {
    let command = CommandSpec::new("gh").args([
        "pr".to_owned(),
        "merge".to_owned(),
        number.to_string(),
        "--repo".to_owned(),
        repository.slug(),
    ]);
    if disable {
        command.arg("--disable-auto")
    } else {
        command.args(["--auto", "--squash"])
    }
}

fn failed_runs_command(repository: &RepositoryId, head_oid: &str) -> CommandSpec {
    CommandSpec::new("gh").args([
        "run".to_owned(),
        "list".to_owned(),
        "--repo".to_owned(),
        repository.slug(),
        "--commit".to_owned(),
        head_oid.to_owned(),
        "--status".to_owned(),
        "failure".to_owned(),
        "--limit".to_owned(),
        "100".to_owned(),
        "--json".to_owned(),
        WORKFLOW_RUN_JSON_FIELDS.to_owned(),
    ])
}

fn workflow_run_command(repository: &RepositoryId, run_id: u64) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        format!("repos/{}/actions/runs/{run_id}", repository.slug()),
    ])
}

fn rerun_failed_command(repository: &RepositoryId, run_id: u64) -> CommandSpec {
    CommandSpec::new("gh").args([
        "run".to_owned(),
        "rerun".to_owned(),
        run_id.to_string(),
        "--repo".to_owned(),
        repository.slug(),
        "--failed".to_owned(),
    ])
}

fn branch_settings_command(repository: &RepositoryId, branch: &str) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        format!(
            "repos/{}/branches/{}",
            repository.slug(),
            encode_path_segment(branch)
        ),
    ])
}

fn branch_protection_command(repository: &RepositoryId, branch: &str) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        format!(
            "repos/{}/branches/{}/protection",
            repository.slug(),
            encode_path_segment(branch)
        ),
    ])
}

fn repository_settings_command(repository: &RepositoryId) -> CommandSpec {
    CommandSpec::new("gh").args(["api".to_owned(), format!("repos/{}", repository.slug())])
}

fn repository_labels_command(repository: &RepositoryId) -> CommandSpec {
    CommandSpec::new("gh").args([
        "label".to_owned(),
        "list".to_owned(),
        "--repo".to_owned(),
        repository.slug(),
        "--limit".to_owned(),
        "1000".to_owned(),
        "--json".to_owned(),
        "name,color,description".to_owned(),
    ])
}

fn create_repository_label_command(
    repository: &RepositoryId,
    name: &str,
    color: &str,
    description: &str,
) -> CommandSpec {
    CommandSpec::new("gh").args([
        "label".to_owned(),
        "create".to_owned(),
        name.to_owned(),
        "--repo".to_owned(),
        repository.slug(),
        "--color".to_owned(),
        color.to_owned(),
        "--description".to_owned(),
        description.to_owned(),
    ])
}

fn repository_permission_command(repository: &RepositoryId) -> CommandSpec {
    CommandSpec::new("gh").args([
        "repo".to_owned(),
        "view".to_owned(),
        repository.slug(),
        "--json".to_owned(),
        "viewerPermission".to_owned(),
    ])
}

fn admin_squash_merge_command(repository: &RepositoryId, number: PrNumber) -> CommandSpec {
    CommandSpec::new("gh").args([
        "pr".to_owned(),
        "merge".to_owned(),
        number.to_string(),
        "--repo".to_owned(),
        repository.slug(),
        "--admin".to_owned(),
        "--squash".to_owned(),
    ])
}

fn labeled_pr_command(
    repository: &str,
    state: &str,
    label: &str,
    limit: usize,
    most_recently_updated: bool,
) -> CommandSpec {
    let mut command = CommandSpec::new("gh").args([
        "pr".to_owned(),
        "list".to_owned(),
        "--repo".to_owned(),
        repository.to_owned(),
        "--state".to_owned(),
        state.to_owned(),
        "--label".to_owned(),
        label.to_owned(),
        "--limit".to_owned(),
        limit.to_string(),
        "--json".to_owned(),
        PR_JSON_FIELDS.to_owned(),
    ]);
    if most_recently_updated {
        command = command.args(["--search", "sort:updated-desc"]);
    }
    command
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    encoded
}

fn normalize_check_state(provider_state: Option<&str>) -> CheckState {
    match provider_state {
        Some("EXPECTED") => CheckState::Expected,
        Some("QUEUED" | "REQUESTED" | "WAITING") => CheckState::Queued,
        Some("IN_PROGRESS" | "PENDING") => CheckState::InProgress,
        Some("SUCCESS") => CheckState::Success,
        Some("FAILURE" | "ERROR" | "STARTUP_FAILURE" | "STALE") => CheckState::Failure,
        Some("NEUTRAL") => CheckState::Neutral,
        Some("SKIPPED") => CheckState::Skipped,
        Some("CANCELLED" | "CANCELED") => CheckState::Cancelled,
        Some("TIMED_OUT") => CheckState::TimedOut,
        Some("ACTION_REQUIRED") => CheckState::ActionRequired,
        _ => CheckState::Unknown,
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryJson {
    name_with_owner: String,
    default_branch_ref: Option<NamedRefJson>,
}

#[derive(Debug, Deserialize)]
struct NamedRefJson {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GitRefJson {
    object: GitObjectJson,
}

#[derive(Debug, Deserialize)]
struct GitObjectJson {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct BranchSettingsJson {
    protected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DefaultBranchPolicy {
    #[serde(default)]
    pub required_status_checks: Vec<String>,
    pub strict_status_checks: bool,
    pub required_approving_review_count: u64,
}

impl DefaultBranchPolicy {
    #[must_use]
    pub fn ready(&self) -> bool {
        !self.required_status_checks.is_empty() || self.required_approving_review_count > 0
    }
}

#[derive(Debug, Deserialize)]
struct BranchProtectionJson {
    required_status_checks: Option<RequiredStatusChecksJson>,
    required_pull_request_reviews: Option<RequiredPullRequestReviewsJson>,
}

#[derive(Debug, Deserialize)]
struct RequiredStatusChecksJson {
    #[serde(default)]
    contexts: Vec<String>,
    #[serde(default)]
    checks: Vec<RequiredCheckJson>,
    strict: bool,
}

#[derive(Debug, Deserialize)]
struct RequiredCheckJson {
    context: String,
}

#[derive(Debug, Deserialize)]
struct RequiredPullRequestReviewsJson {
    required_approving_review_count: u64,
}

#[derive(Debug, Deserialize)]
struct RepositorySettingsJson {
    allow_auto_merge: bool,
    allow_squash_merge: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryPermissionJson {
    viewer_permission: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRunJson {
    database_id: u64,
    head_sha: String,
    status: String,
    conclusion: String,
    event: String,
    name: String,
    workflow_name: String,
    url: String,
}

impl From<WorkflowRunJson> for WorkflowRunSnapshot {
    fn from(run: WorkflowRunJson) -> Self {
        Self {
            database_id: run.database_id,
            pull_requests: Vec::new(),
            head_sha: run.head_sha,
            status: run.status,
            conclusion: run.conclusion,
            event: run.event,
            name: run.name,
            workflow_name: run.workflow_name,
            url: run.url,
        }
    }
}

#[derive(Debug, Deserialize)]
struct WorkflowRunDetailsJson {
    id: u64,
    head_sha: String,
    status: String,
    conclusion: Option<String>,
    event: String,
    name: String,
    html_url: String,
    #[serde(default)]
    pull_requests: Vec<WorkflowRunPullRequestJson>,
}

#[derive(Debug, Deserialize)]
struct WorkflowRunPullRequestJson {
    number: u64,
}

impl From<WorkflowRunDetailsJson> for WorkflowRunSnapshot {
    fn from(run: WorkflowRunDetailsJson) -> Self {
        Self {
            database_id: run.id,
            pull_requests: run
                .pull_requests
                .into_iter()
                .map(|pull_request| PrNumber(pull_request.number))
                .collect(),
            head_sha: run.head_sha,
            status: run.status,
            conclusion: run.conclusion.unwrap_or_default(),
            event: run.event,
            workflow_name: run.name.clone(),
            name: run.name,
            url: run.html_url,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestJson {
    number: u64,
    title: String,
    state: ProviderPullRequestState,
    is_draft: bool,
    head_ref_name: String,
    head_ref_oid: String,
    head_repository: Option<HeadRepositoryJson>,
    head_repository_owner: Option<RepositoryOwnerJson>,
    is_cross_repository: bool,
    base_ref_name: String,
    base_ref_oid: String,
    #[serde(default)]
    labels: Vec<LabelJson>,
    auto_merge_request: Option<AutoMergeJson>,
    #[serde(default)]
    status_check_rollup: Vec<CheckJson>,
    created_at: String,
    merged_at: Option<String>,
    url: String,
    updated_at: String,
}

impl PullRequestJson {
    fn into_snapshot(
        self,
        base_repository: &RepositoryId,
    ) -> Result<model::PullRequestSnapshot, DiscoveryError> {
        let head_repository = self
            .head_repository
            .as_ref()
            .and_then(|repository| {
                repository.name_with_owner.clone().or_else(|| {
                    self.head_repository_owner
                        .as_ref()
                        .zip(repository.name.as_ref())
                        .map(|(owner, name)| format!("{}/{name}", owner.login))
                })
            })
            .map_or_else(
                || {
                    if self.is_cross_repository {
                        Err(DiscoveryError::MissingHeadRepository { pr: self.number })
                    } else {
                        Ok(base_repository.clone())
                    }
                },
                |slug| repository_id(&slug),
            )?;
        let auto_merge = self
            .auto_merge_request
            .map_or_else(AutoMergeState::disabled, |request| AutoMergeState {
                enabled: true,
                merge_method: (request.merge_method == "SQUASH").then_some(MergeMethod::Squash),
            });

        Ok(model::PullRequestSnapshot {
            number: PrNumber(self.number),
            title: self.title,
            url: self.url,
            state: self.state.into(),
            draft: self.is_draft,
            head: BranchSnapshot {
                repository: head_repository,
                name: self.head_ref_name,
                oid: CommitOid(self.head_ref_oid),
            },
            base: BranchSnapshot {
                repository: base_repository.clone(),
                name: self.base_ref_name,
                oid: CommitOid(self.base_ref_oid),
            },
            cross_repository: self.is_cross_repository,
            labels: self.labels.into_iter().map(|label| label.name).collect(),
            auto_merge,
            checks: self
                .status_check_rollup
                .into_iter()
                .map(CheckJson::into_snapshot)
                .collect(),
            created_at: Some(self.created_at),
            merged_at: self.merged_at,
            updated_at: Some(self.updated_at),
        })
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum ProviderPullRequestState {
    Open,
    Closed,
    Merged,
}

impl From<ProviderPullRequestState> for model::PullRequestState {
    fn from(state: ProviderPullRequestState) -> Self {
        match state {
            ProviderPullRequestState::Open => Self::Open,
            ProviderPullRequestState::Closed => Self::Closed,
            ProviderPullRequestState::Merged => Self::Merged,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeadRepositoryJson {
    name: Option<String>,
    name_with_owner: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RepositoryOwnerJson {
    login: String,
}

#[derive(Debug, Deserialize)]
struct LabelJson {
    name: String,
}

/// Exact repository-label metadata used by initialization preconditions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepositoryLabel {
    pub name: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AutoMergeJson {
    merge_method: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckJson {
    #[serde(rename = "__typename")]
    kind: String,
    name: Option<String>,
    context: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    state: Option<String>,
    workflow_name: Option<String>,
    details_url: Option<String>,
    target_url: Option<String>,
}

impl CheckJson {
    fn into_snapshot(self) -> model::CheckSnapshot {
        let provider_state = [self.conclusion, self.state, self.status]
            .into_iter()
            .flatten()
            .find(|state| !state.is_empty());
        model::CheckSnapshot {
            name: self
                .name
                .or(self.context)
                .or(self.workflow_name)
                .unwrap_or(self.kind),
            state: normalize_check_state(provider_state.as_deref()),
            provider_state,
            details_url: self.details_url.or(self.target_url),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;
    use crate::command::CommandOutput;

    struct FakeRunner {
        calls: RefCell<VecDeque<(CommandSpec, CommandOutput)>>,
    }

    impl FakeRunner {
        fn new(calls: Vec<(CommandSpec, CommandOutput)>) -> Self {
            Self {
                calls: RefCell::new(calls.into()),
            }
        }

        fn assert_exhausted(&self) {
            assert!(
                self.calls.borrow().is_empty(),
                "not all fake calls were used"
            );
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
            let (expected, output) = self
                .calls
                .borrow_mut()
                .pop_front()
                .expect("unexpected subprocess call");
            assert_eq!(&expected, command);
            Ok(output)
        }
    }

    fn successful_discovery_calls(open_prs: &str) -> Vec<(CommandSpec, CommandOutput)> {
        vec![
            (
                repository_command(),
                CommandOutput::success(
                    r#"{"nameWithOwner":"acme/widgets","defaultBranchRef":{"name":"main"}}"#,
                ),
            ),
            (
                current_branch_command(),
                CommandOutput::success("feature/widget\n"),
            ),
            (
                default_branch_command("acme/widgets", "main"),
                CommandOutput::success(r#"{"object":{"sha":"default-sha"}}"#),
            ),
            (
                current_pr_command("acme/widgets", "feature/widget"),
                CommandOutput::success(pr_list_json(12, "feature/widget", "acme/widgets", false)),
            ),
            (
                labeled_pr_command("acme/widgets", "open", "caravan", 1_000, false),
                CommandOutput::success(open_prs),
            ),
            (
                open_pr_command("acme/widgets", 1_000),
                CommandOutput::success(open_prs),
            ),
            (
                labeled_pr_command("acme/widgets", "merged", "caravan", 100, true),
                CommandOutput::success(merged_pr_json()),
            ),
        ]
    }

    fn pr_list_json(number: u64, branch: &str, repository: &str, cross_repo: bool) -> String {
        format!(
            r#"[{{"number":{number},"title":"Queue change {number}","state":"OPEN","isDraft":false,"headRefName":"{branch}","headRefOid":"head-{number}","headRepository":{{"name":"widgets","nameWithOwner":"{repository}"}},"headRepositoryOwner":{{"login":"acme"}},"isCrossRepository":{cross_repo},"baseRefName":"main","baseRefOid":"base-{number}","labels":[{{"name":"caravan"}}],"autoMergeRequest":{{"mergeMethod":"SQUASH","enabledAt":"2026-07-17T10:00:00Z","enabledBy":{{"login":"octocat"}}}},"statusCheckRollup":[{{"__typename":"CheckRun","name":"test","context":null,"status":"COMPLETED","conclusion":"SUCCESS","state":null,"workflowName":"CI","detailsUrl":"https://example.test/check","targetUrl":null}}],"createdAt":"2026-07-17T10:00:00Z","mergedAt":null,"url":"https://example.test/pr/{number}","updatedAt":"2026-07-17T11:00:00Z"}}]"#
        )
    }

    fn merged_pr_json() -> &'static str {
        r#"[{"number":9,"title":"Merged queue change","state":"MERGED","isDraft":false,"headRefName":"old-head","headRefOid":"head-9","headRepository":{"name":"widgets","nameWithOwner":"acme/widgets"},"headRepositoryOwner":{"login":"acme"},"isCrossRepository":false,"baseRefName":"main","baseRefOid":"base-9","labels":[{"name":"caravan"}],"autoMergeRequest":null,"statusCheckRollup":[{"__typename":"StatusContext","name":null,"context":"legacy-ci","status":null,"conclusion":null,"state":"SUCCESS","workflowName":null,"detailsUrl":null,"targetUrl":"https://example.test/status"}],"createdAt":"2026-07-17T08:00:00Z","mergedAt":"2026-07-17T09:00:00Z","url":"https://example.test/pr/9","updatedAt":"2026-07-17T09:00:00Z"}]"#
    }

    fn pr_object_json(number: u64, branch: &str, repository: &str) -> String {
        let list = pr_list_json(number, branch, repository, false);
        list[1..list.len() - 1].to_owned()
    }

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "acme".to_owned(),
            name: "widgets".to_owned(),
        }
    }

    fn precondition(number: u64) -> PullRequestPrecondition {
        PullRequestPrecondition {
            number: PrNumber(number),
            state: model::PullRequestState::Open,
            head_oid: CommitOid(format!("head-{number}")),
            base_ref: "main".to_owned(),
            base_oid: CommitOid(format!("base-{number}")),
            labels: std::collections::BTreeSet::from(["caravan".to_owned()]),
            auto_merge: AutoMergeState::squash(),
        }
    }

    #[test]
    fn initialization_identity_never_queries_prs_or_moving_refs() {
        let runner = FakeRunner::new(vec![(
            repository_command(),
            CommandOutput::success(
                r#"{"nameWithOwner":"acme/widgets","defaultBranchRef":{"name":"main"}}"#,
            ),
        )]);
        let adapter = GitHubMutationAdapter::new(runner);

        let (repository, default_branch) = adapter.repository_identity().unwrap();

        assert_eq!(repository.slug(), "acme/widgets");
        assert_eq!(default_branch, "main");
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn discovers_canonical_repository_and_pull_request_snapshots() {
        let open_prs = pr_list_json(12, "feature/widget", "acme/widgets", false);
        let runner = FakeRunner::new(successful_discovery_calls(&open_prs));
        let discovery = GitHubDiscovery::new(runner);

        let snapshot = discovery.discover().expect("discovery succeeds");

        assert_eq!(snapshot.repository.slug(), "acme/widgets");
        assert_eq!(snapshot.default_branch.oid, CommitOid("default-sha".into()));
        assert_eq!(snapshot.current_branch.as_deref(), Some("feature/widget"));
        assert_eq!(snapshot.current_pr, Some(PrNumber(12)));
        let open = snapshot
            .pull_requests
            .iter()
            .find(|pull_request| pull_request.number == PrNumber(12))
            .expect("open PR is present");
        assert_eq!(open.head.oid, CommitOid("head-12".into()));
        assert_eq!(open.base.oid, CommitOid("base-12".into()));
        assert_eq!(open.auto_merge, AutoMergeState::squash());
        assert_eq!(open.created_at.as_deref(), Some("2026-07-17T10:00:00Z"));
        assert_eq!(open.checks[0].state, CheckState::Success);
        assert_eq!(open.checks[0].provider_state.as_deref(), Some("SUCCESS"));
        let merged = snapshot
            .pull_requests
            .iter()
            .find(|pull_request| pull_request.number == PrNumber(9))
            .expect("merged predecessor is present");
        assert_eq!(merged.state, model::PullRequestState::Merged);
        assert_eq!(merged.checks[0].state, CheckState::Success);
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn preserves_unknown_provider_check_values() {
        let open_prs =
            pr_list_json(12, "feature/widget", "acme/widgets", false).replace("SUCCESS", "WOBBLY");
        let runner = FakeRunner::new(successful_discovery_calls(&open_prs));
        let discovery = GitHubDiscovery::new(runner);

        let snapshot = discovery.discover().expect("discovery succeeds");
        let check = &snapshot
            .pull_requests
            .iter()
            .find(|pull_request| pull_request.number == PrNumber(12))
            .expect("open PR is present")
            .checks[0];

        assert_eq!(check.state, CheckState::Unknown);
        assert_eq!(check.provider_state.as_deref(), Some("WOBBLY"));
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn empty_check_conclusion_falls_back_to_nonempty_status() {
        let open_prs = pr_list_json(12, "feature/widget", "acme/widgets", false)
            .replace("\"status\":\"COMPLETED\"", "\"status\":\"QUEUED\"")
            .replace("\"conclusion\":\"SUCCESS\"", "\"conclusion\":\"\"");
        let runner = FakeRunner::new(successful_discovery_calls(&open_prs));
        let discovery = GitHubDiscovery::new(runner);

        let snapshot = discovery.discover().expect("discovery succeeds");
        let check = &snapshot
            .pull_requests
            .iter()
            .find(|pull_request| pull_request.number == PrNumber(12))
            .expect("open PR is present")
            .checks[0];

        assert_eq!(check.state, CheckState::Queued);
        assert_eq!(check.provider_state.as_deref(), Some("QUEUED"));
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn rejects_fork_only_active_caravan_heads() {
        let fork_prs = pr_list_json(14, "fork-feature", "someone/widgets", true);
        let mut calls = successful_discovery_calls(&fork_prs);
        calls.pop(); // merged-history query
        calls.pop(); // all-open query; active-head validation stops before both
        let runner = FakeRunner::new(calls);
        let discovery = GitHubDiscovery::new(runner);

        let error = discovery
            .discover()
            .expect_err("fork-only predecessor branches are unsupported");

        assert_eq!(
            error,
            DiscoveryError::ForkOnlyHead {
                pr: 14,
                head_repository: "someone/widgets".to_owned(),
                base_repository: "acme/widgets".to_owned(),
            }
        );
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn detached_head_is_represented_without_a_current_pr_query() {
        let runner = FakeRunner::new(vec![
            (
                repository_command(),
                CommandOutput::success(
                    r#"{"nameWithOwner":"acme/widgets","defaultBranchRef":{"name":"main"}}"#,
                ),
            ),
            (current_branch_command(), CommandOutput::failure(1, "")),
            (
                default_branch_command("acme/widgets", "main"),
                CommandOutput::success(r#"{"object":{"sha":"default-sha"}}"#),
            ),
            (
                labeled_pr_command("acme/widgets", "open", "caravan", 1_000, false),
                CommandOutput::success("[]"),
            ),
            (
                open_pr_command("acme/widgets", 1_000),
                CommandOutput::success("[]"),
            ),
            (
                labeled_pr_command("acme/widgets", "merged", "caravan", 100, true),
                CommandOutput::success("[]"),
            ),
        ]);
        let discovery = GitHubDiscovery::new(runner);

        let snapshot = discovery.discover().expect("detached discovery succeeds");

        assert_eq!(snapshot.current_branch, None);
        assert_eq!(snapshot.current_pr, None);
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn stale_precondition_stops_before_provider_mutation() {
        let repository = repository();
        let expected = precondition(12);
        let actual =
            pr_object_json(12, "feature/widget", "acme/widgets").replace("head-12", "changed-head");
        let runner = FakeRunner::new(vec![(
            pull_request_command(&repository, "12"),
            CommandOutput::success(actual),
        )]);
        let adapter = GitHubMutationAdapter::new(runner);

        let error = adapter
            .set_base(&repository, &expected, "develop")
            .expect_err("stale PR must not be edited");

        assert!(matches!(
            error,
            MutationError::StalePrecondition { changed_fields, .. }
                if changed_fields == ["head_oid"]
        ));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn base_edit_refetches_exact_before_and_after_facts() {
        let repository = repository();
        let expected = precondition(12);
        let before = pr_object_json(12, "feature/widget", "acme/widgets");
        let after = before
            .replace("\"baseRefName\":\"main\"", "\"baseRefName\":\"develop\"")
            .replace(
                "\"baseRefOid\":\"base-12\"",
                "\"baseRefOid\":\"develop-oid\"",
            );
        let runner = FakeRunner::new(vec![
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(before),
            ),
            (
                edit_pull_request_command(&repository, PrNumber(12), "--base", "develop"),
                CommandOutput::success("https://example.test/pr/12\n"),
            ),
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(after),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let receipt = adapter
            .set_base(&repository, &expected, "develop")
            .expect("base edit succeeds");

        assert_eq!(receipt.kind, MutationKind::SetBase);
        assert_eq!(receipt.before.as_ref().unwrap().base.name, "main");
        assert_eq!(receipt.after.base.name, "develop");
        assert_eq!(receipt.after.base.oid, CommitOid("develop-oid".to_owned()));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn create_pull_request_uses_fill_and_refetches_returned_url() {
        let repository = repository();
        let input = CreatePullRequestInput {
            head: "feature/widget".to_owned(),
            base: "main".to_owned(),
            draft: true,
        };
        let url = "https://example.test/pr/12";
        let runner = FakeRunner::new(vec![
            (
                create_pull_request_command(&repository, &input),
                CommandOutput::success(format!("{url}\n")),
            ),
            (
                pull_request_command(&repository, url),
                CommandOutput::success(pr_object_json(12, "feature/widget", "acme/widgets")),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let receipt = adapter
            .create_pull_request(&repository, &input)
            .expect("PR creation succeeds");

        assert_eq!(receipt.kind, MutationKind::CreatePullRequest);
        assert_eq!(receipt.before, None);
        assert_eq!(receipt.after.number, PrNumber(12));
        assert_eq!(receipt.provider_output.as_deref(), Some(url));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn failed_workflow_runs_preserve_provider_state_and_exact_head() {
        let repository = repository();
        let expected = precondition(12);
        let runner = FakeRunner::new(vec![
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pr_object_json(12, "feature/widget", "acme/widgets")),
            ),
            (
                failed_runs_command(&repository, "head-12"),
                CommandOutput::success(
                    r#"[{"databaseId":99,"headSha":"head-12","status":"completed","conclusion":"failure","event":"pull_request","name":"CI","workflowName":"CI","url":"https://example.test/run/99"}]"#,
                ),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let runs = adapter
            .failed_runs_for_pull_request(&repository, &expected)
            .expect("failed runs are discovered");

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].database_id, 99);
        assert_eq!(runs[0].status, "completed");
        assert_eq!(runs[0].conclusion, "failure");
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn rerun_failed_run_verifies_run_and_pr_before_mutation() {
        let repository = repository();
        let expected = precondition(12);
        let pull_request = pr_object_json(12, "feature/widget", "acme/widgets");
        let runner = FakeRunner::new(vec![
            (
                workflow_run_command(&repository, 99),
                CommandOutput::success(
                    r#"{"id":99,"head_sha":"head-12","status":"completed","conclusion":"failure","event":"pull_request","name":"CI","html_url":"https://example.test/run/99","pull_requests":[{"number":12}]}"#,
                ),
            ),
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pull_request.clone()),
            ),
            (
                rerun_failed_command(&repository, 99),
                CommandOutput::success(""),
            ),
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pull_request),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let receipt = adapter
            .rerun_failed_run(&repository, &expected, 99)
            .expect("failed run reruns");

        assert_eq!(receipt.kind, MutationKind::RerunChecks);
        assert_eq!(receipt.before.unwrap().number, PrNumber(12));
        assert_eq!(receipt.after.number, PrNumber(12));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn rerun_failed_run_rejects_another_pull_requests_run() {
        let repository = repository();
        let expected = precondition(12);
        let runner = FakeRunner::new(vec![
            (
                workflow_run_command(&repository, 99),
                CommandOutput::success(
                    r#"{"id":99,"head_sha":"head-12","status":"completed","conclusion":"failure","event":"pull_request","name":"CI","html_url":"https://example.test/run/99","pull_requests":[{"number":7}]}"#,
                ),
            ),
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pr_object_json(12, "feature/widget", "acme/widgets")),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let error = adapter
            .rerun_failed_run(&repository, &expected, 99)
            .expect_err("another PR's run must not rerun");

        assert!(matches!(
            error,
            MutationError::RunPullRequestMismatch {
                run_id: 99,
                expected_pr: PrNumber(12),
                actual_prs,
            } if actual_prs == vec![PrNumber(7)]
        ));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn admin_merge_fails_closed_without_admin_permission() {
        let repository = repository();
        let expected = precondition(12);
        let runner = FakeRunner::new(vec![(
            repository_permission_command(&repository),
            CommandOutput::success(r#"{"viewerPermission":"WRITE"}"#),
        )]);
        let adapter = GitHubMutationAdapter::new(runner);

        let error = adapter
            .admin_squash_merge(&repository, &expected)
            .expect_err("non-admin merge is rejected");

        assert_eq!(
            error,
            MutationError::PermissionDenied {
                required: "ADMIN".to_owned(),
                actual: "WRITE".to_owned(),
            }
        );
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn mutation_command_builders_keep_user_values_as_separate_arguments() {
        let repository = repository();
        assert_eq!(
            edit_pull_request_command(&repository, PrNumber(12), "--add-label", "caravan queue"),
            CommandSpec::new("gh").args([
                "pr",
                "edit",
                "12",
                "--repo",
                "acme/widgets",
                "--add-label",
                "caravan queue",
            ])
        );
        assert_eq!(
            edit_pull_request_command(
                &repository,
                PrNumber(12),
                "--remove-label",
                "caravan-evicted"
            ),
            CommandSpec::new("gh").args([
                "pr",
                "edit",
                "12",
                "--repo",
                "acme/widgets",
                "--remove-label",
                "caravan-evicted",
            ])
        );
        assert_eq!(
            auto_merge_command(&repository, PrNumber(12), false),
            CommandSpec::new("gh").args([
                "pr",
                "merge",
                "12",
                "--repo",
                "acme/widgets",
                "--auto",
                "--squash",
            ])
        );
        assert_eq!(
            auto_merge_command(&repository, PrNumber(12), true),
            CommandSpec::new("gh").args([
                "pr",
                "merge",
                "12",
                "--repo",
                "acme/widgets",
                "--disable-auto",
            ])
        );
        assert_eq!(
            admin_squash_merge_command(&repository, PrNumber(12)),
            CommandSpec::new("gh").args([
                "pr",
                "merge",
                "12",
                "--repo",
                "acme/widgets",
                "--admin",
                "--squash",
            ])
        );
    }

    #[test]
    fn json_decoder_uses_only_stdout_and_preserves_stderr_on_failure() {
        let valid_command = CommandSpec::new("gh").args(["fixture", "valid"]);
        let valid_runner = FakeRunner::new(vec![(
            valid_command.clone(),
            CommandOutput {
                code: Some(0),
                stdout: r#"{"value":7}"#.to_owned(),
                stderr: "\u{1b}[33mwrapper notice\u{1b}[0m\u{1}".to_owned(),
            },
        )]);
        let valid_adapter = GitHubMutationAdapter::new(valid_runner);
        let value: serde_json::Value = valid_adapter
            .json(valid_command)
            .expect("stderr cannot contaminate JSON stdout");
        assert_eq!(value["value"], 7);
        valid_adapter.runner.assert_exhausted();

        let malformed_command = CommandSpec::new("gh").args(["fixture", "malformed"]);
        let malformed_runner = FakeRunner::new(vec![(
            malformed_command.clone(),
            CommandOutput {
                code: Some(0),
                stdout: "{\"value\":\u{1}}".to_owned(),
                stderr: "wrapper diagnostic".to_owned(),
            },
        )]);
        let malformed_adapter = GitHubMutationAdapter::new(malformed_runner);
        let error = malformed_adapter
            .json::<serde_json::Value>(malformed_command)
            .expect_err("control-contaminated stdout must fail closed");
        assert!(matches!(
            error,
            MutationError::Provider(DiscoveryError::InvalidJson {
                evidence,
                ..
            }) if evidence.stdout.contains('\u{1}')
                && evidence.stderr == "wrapper diagnostic"
        ));
        malformed_adapter.runner.assert_exhausted();
    }

    #[test]
    fn json_decoder_reports_nonzero_stderr_without_parsing_stdout() {
        let command = CommandSpec::new("gh").args(["fixture", "failed"]);
        let runner = FakeRunner::new(vec![(
            command.clone(),
            CommandOutput {
                code: Some(23),
                stdout: r#"{"would":"parse"}"#.to_owned(),
                stderr: "provider failed".to_owned(),
            },
        )]);
        let adapter = GitHubMutationAdapter::new(runner);

        let error = adapter
            .json::<serde_json::Value>(command)
            .expect_err("nonzero command must fail before JSON decode");

        assert!(matches!(
            error,
            MutationError::Provider(DiscoveryError::CommandFailed {
                code: Some(23),
                stderr,
                ..
            }) if stderr == "provider failed"
        ));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn exact_branch_head_verification_fails_closed_on_movement() {
        let runner = FakeRunner::new(vec![(
            default_branch_command("acme/widgets", "main"),
            CommandOutput::success(r#"{"object":{"sha":"new-main"}}"#),
        )]);
        let adapter = GitHubMutationAdapter::new(runner);

        let error = adapter
            .verify_branch_head(&repository(), "main", &CommitOid("old-main".to_owned()))
            .expect_err("moved branch is stale");

        assert!(matches!(
            error,
            MutationError::BranchHeadMismatch {
                branch,
                expected,
                actual,
            } if branch == "main"
                && expected == CommitOid("old-main".to_owned())
                && actual == CommitOid("new-main".to_owned())
        ));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn default_branch_policy_preserves_required_check_and_review_evidence() {
        let repository = repository();
        let runner = FakeRunner::new(vec![(
            branch_protection_command(&repository, "main"),
            CommandOutput::success(
                r#"{"required_status_checks":{"strict":false,"contexts":["Check & Lint"],"checks":[{"context":"Fast Tests (unit)","app_id":1}]},"required_pull_request_reviews":{"required_approving_review_count":2}}"#,
            ),
        )]);
        let adapter = GitHubMutationAdapter::new(runner);
        let policy = adapter.default_branch_policy(&repository, "main").unwrap();
        assert_eq!(
            policy.required_status_checks,
            ["Check & Lint", "Fast Tests (unit)"]
        );
        assert!(!policy.strict_status_checks);
        assert_eq!(policy.required_approving_review_count, 2);
        assert!(policy.ready());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn percent_encodes_slashes_in_default_branch_api_path() {
        assert_eq!(
            default_branch_command("acme/widgets", "release/next"),
            CommandSpec::new("gh")
                .args(["api", "repos/acme/widgets/git/ref/heads/release%2Fnext",])
        );
    }
}
