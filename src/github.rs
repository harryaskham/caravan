//! Read-only GitHub repository and pull-request discovery.
//!
//! This module deliberately stops at faithful provider conversion. Graph policy
//! and every GitHub mutation live in downstream lanes.

use std::collections::BTreeMap;

use serde::{Deserialize, de::DeserializeOwned};

use crate::command::{CommandRunError, CommandRunner, CommandSpec, ProcessRunner};
use crate::model::{
    self, AutoMergeState, BranchSnapshot, CheckState, CommitOid, MergeMethod, PrNumber,
    RepositoryId,
};

const PR_JSON_FIELDS: &str = "number,title,state,isDraft,headRefName,headRefOid,headRepository,headRepositoryOwner,isCrossRepository,baseRefName,baseRefOid,labels,autoMergeRequest,statusCheckRollup,mergedAt,url,updatedAt";

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
            Self::InvalidJson { command, message } => {
                write!(
                    formatter,
                    "`{}` returned invalid JSON: {message}",
                    command.display()
                )
            }
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
        for pull_request in open_labeled_prs
            .into_iter()
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
#[serde(rename_all = "camelCase")]
struct PullRequestJson {
    number: u64,
    title: String,
    state: ProviderPullRequestState,
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
        let provider_state = self
            .conclusion
            .or(self.state)
            .or(self.status)
            .filter(|state| !state.is_empty());
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
                labeled_pr_command("acme/widgets", "merged", "caravan", 100, true),
                CommandOutput::success(merged_pr_json()),
            ),
        ]
    }

    fn pr_list_json(number: u64, branch: &str, repository: &str, cross_repo: bool) -> String {
        format!(
            r#"[{{"number":{number},"title":"Queue change {number}","state":"OPEN","isDraft":false,"headRefName":"{branch}","headRefOid":"head-{number}","headRepository":{{"name":"widgets","nameWithOwner":"{repository}"}},"headRepositoryOwner":{{"login":"acme"}},"isCrossRepository":{cross_repo},"baseRefName":"main","baseRefOid":"base-{number}","labels":[{{"name":"caravan"}}],"autoMergeRequest":{{"mergeMethod":"SQUASH","enabledAt":"2026-07-17T10:00:00Z","enabledBy":{{"login":"octocat"}}}},"statusCheckRollup":[{{"__typename":"CheckRun","name":"test","context":null,"status":"COMPLETED","conclusion":"SUCCESS","state":null,"workflowName":"CI","detailsUrl":"https://example.test/check","targetUrl":null}}],"mergedAt":null,"url":"https://example.test/pr/{number}","updatedAt":"2026-07-17T11:00:00Z"}}]"#
        )
    }

    fn merged_pr_json() -> &'static str {
        r#"[{"number":9,"title":"Merged queue change","state":"MERGED","isDraft":false,"headRefName":"old-head","headRefOid":"head-9","headRepository":{"name":"widgets","nameWithOwner":"acme/widgets"},"headRepositoryOwner":{"login":"acme"},"isCrossRepository":false,"baseRefName":"main","baseRefOid":"base-9","labels":[{"name":"caravan"}],"autoMergeRequest":null,"statusCheckRollup":[{"__typename":"StatusContext","name":null,"context":"legacy-ci","status":null,"conclusion":null,"state":"SUCCESS","workflowName":null,"detailsUrl":null,"targetUrl":"https://example.test/status"}],"mergedAt":"2026-07-17T09:00:00Z","url":"https://example.test/pr/9","updatedAt":"2026-07-17T09:00:00Z"}]"#
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
    fn rejects_fork_only_active_caravan_heads() {
        let fork_prs = pr_list_json(14, "fork-feature", "someone/widgets", true);
        let mut calls = successful_discovery_calls(&fork_prs);
        calls.pop(); // active-head validation stops before merged-history discovery
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
    fn percent_encodes_slashes_in_default_branch_api_path() {
        assert_eq!(
            default_branch_command("acme/widgets", "release/next"),
            CommandSpec::new("gh")
                .args(["api", "repos/acme/widgets/git/ref/heads/release%2Fnext",])
        );
    }
}
