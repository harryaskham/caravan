//! GitHub repository discovery and policy-free provider primitives.
//!
//! This module deliberately stops at faithful provider conversion plus exact,
//! optimistic mutation adapters. Graph, scheduling, admission, and merge policy
//! live in downstream lanes.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::command::{CommandOutput, CommandRunError, CommandRunner, CommandSpec, ProcessRunner};
use crate::model::{
    self, BranchSnapshot, CheckState, CommitOid, MergeMethod, MutationKind, PrNumber,
    PullRequestPrecondition, RepositoryId,
};

const PR_JSON_FIELDS: &str = "number,title,body,state,isDraft,mergeStateStatus,headRefName,headRefOid,headRepository,headRepositoryOwner,isCrossRepository,baseRefName,baseRefOid,labels,autoMergeRequest,statusCheckRollup,createdAt,mergedAt,url,updatedAt";
// Merged predecessors are graph history, never CI candidates. Omitting the
// rollup prevents old check suites from dominating provider response size.
const PR_HISTORY_JSON_FIELDS: &str = "number,title,state,isDraft,headRefName,headRefOid,headRepository,headRepositoryOwner,isCrossRepository,baseRefName,baseRefOid,labels,autoMergeRequest,createdAt,mergedAt,url,updatedAt";
const GENERATION_PR_JSON_FIELDS: &str = "number,body,headRefName,headRefOid,createdAt";
const WORKFLOW_RUN_JSON_FIELDS: &str =
    "databaseId,headSha,status,conclusion,event,name,workflowName,url";
/// Keeps JSON/MCP output and GraphQL cost bounded on pathological repositories.
const MERGE_CANDIDATE_LIMIT: usize = 100;
const PARKED_LABEL: &str = "caravan-parked";
const CLOSED_LABEL: &str = "caravan-closed";

/// Limits and label used by one discovery pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryOptions {
    /// Label identifying active caravan members.
    pub label: String,
    /// Maximum active labelled PRs to fetch.
    pub open_limit: usize,
    /// Maximum recently updated merged labelled PRs to fetch.
    pub merged_limit: usize,
    /// Treat one exact merged unlabelled branch generation as ancestry evidence
    /// for an explicitly requested fresh PR creation.
    pub allow_unlabelled_historical_pr_creation: bool,
    /// Require the current branch to resolve to an exact caravan pull request.
    ///
    /// Fleet-level reads (admission order, status, the next candidate) are
    /// properties of the provider graph and must not fail because the local
    /// checkout happens to sit on a merged or ambiguous branch — a state Cara
    /// itself produces by retiring merged heads. Those callers set this off and
    /// receive `current_pr: None` plus the retained reason.
    ///
    /// PR-scoped mutations keep it on, so an ambiguous branch still yields the
    /// precise typed refusal rather than a bare "no current PR". Turning it off
    /// is never a safety relaxation: every PR-scoped command resolves its
    /// subject as `input.pr.or(current_pr).ok_or(current_pr_not_found)`, so an
    /// unresolved branch refuses to mutate rather than mutating the wrong PR.
    pub require_current_pr_resolution: bool,
    /// Restrict unlabelled candidate discovery to one exact PR while retaining
    /// every active labelled caravan member and lightweight generation facts.
    /// Exact admission must not pay for full check-rollup discovery across an
    /// unrelated open-PR fleet (bd-b915a6).
    pub focus_pr: Option<PrNumber>,
    /// Include merged/closed lifecycle snapshots in the returned graph. Human
    /// status and cleanup reads need them; a mutating sync hot pass defers them
    /// so historical rows cannot starve known active work (bd-488224).
    pub include_historical_pull_requests: bool,
    /// Exact `owner/name` from configuration. Managed checkouts point at a local
    /// daemon mirror, so git-remote inference cannot name the repository and
    /// `gh` has no other input (bd-ff639b).
    pub repository: Option<String>,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            label: "caravan".to_owned(),
            open_limit: 1_000,
            merged_limit: 100,
            allow_unlabelled_historical_pr_creation: false,
            // Strict by default: a caller that has not declared itself a
            // fleet-level read keeps the precise historical diagnostics.
            require_current_pr_resolution: true,
            focus_pr: None,
            include_historical_pull_requests: true,
            repository: None,
        }
    }
}

/// One bounded provider-native Stack inventory page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackInventory {
    pub stacks: Vec<GitHubStackSnapshot>,
    /// Exactly one page of 100 Stacks is read. A full page cannot prove that no
    /// later page exists, so callers must surface truncation rather than infer
    /// repository-wide completeness.
    pub truncated: bool,
}

/// Read-only GitHub Stack identity, ordered bottom to top.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackSnapshot {
    pub id: u64,
    pub number: u64,
    pub node_id: String,
    pub base: GitHubStackBase,
    pub open: bool,
    pub created_at: String,
    #[serde(default)]
    pub pull_requests: Vec<GitHubStackPullRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackBase {
    #[serde(rename = "ref")]
    pub ref_name: String,
}

/// Minimal PR generation supplied by the GitHub Stacks REST resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackPullRequest {
    pub number: u64,
    pub state: String,
    pub draft: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<String>,
    pub head: GitHubStackPullRequestHead,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackPullRequestHead {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: CommitOid,
}

/// Typed capability/read failure. `Unavailable` is intentionally distinct from
/// auth, transport, rate-limit, and response-shape ambiguity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHubStackReadError {
    Unavailable {
        diagnostic: String,
    },
    Runner(CommandRunError),
    CommandFailed {
        code: Option<i32>,
        diagnostic: String,
    },
    InvalidJson {
        message: String,
    },
}

impl std::fmt::Display for GitHubStackReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { diagnostic } => write!(
                formatter,
                "GitHub native Stacks are unavailable for this repository: {diagnostic}"
            ),
            Self::Runner(error) => error.fmt(formatter),
            Self::CommandFailed { code, diagnostic } => write!(
                formatter,
                "GitHub Stack inventory failed with status {}: {diagnostic}",
                code.map_or_else(|| "signal".to_owned(), |code| code.to_string())
            ),
            Self::InvalidJson { message } => {
                write!(
                    formatter,
                    "GitHub Stack inventory returned invalid JSON: {message}"
                )
            }
        }
    }
}

impl std::error::Error for GitHubStackReadError {}

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
    /// A branch that names historical PR state could not be mapped safely.
    HistoricalCurrentPullRequest {
        /// Stable fail-closed reason for CLI/JSON/MCP diagnostics.
        reason: &'static str,
        /// Current local branch.
        branch: String,
        /// Relevant PR candidates, in deterministic order.
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
            Self::HistoricalCurrentPullRequest {
                reason,
                branch,
                candidates,
            } => write!(
                formatter,
                "historical branch `{branch}` is unsafe ({reason}); candidates: {candidates:?}"
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

/// Durable explanation attached to every Caravan control-label transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ControlLabelAudit {
    pub operation: String,
    pub marker: String,
    pub before_labels: BTreeSet<String>,
    pub after_labels: BTreeSet<String>,
    pub actor: String,
    pub reason: String,
    pub reason_source: String,
    pub compatibility_evidence: String,
    pub clean_squash_evidence: String,
    pub admission_priority_basis: String,
}

/// Build a deterministic transition identity from GitHub-authoritative facts.
/// A second transition on the same head receives a different marker, while an
/// exact retry finds the same marker in PR comments.
#[must_use]
pub fn control_label_marker(
    operation: &str,
    number: PrNumber,
    head_oid: &CommitOid,
    before: &BTreeSet<String>,
    after: &BTreeSet<String>,
) -> String {
    use std::fmt::Write as _;
    let controls = |labels: &BTreeSet<String>| {
        labels
            .iter()
            .filter(|label| {
                matches!(
                    label.as_str(),
                    "caravan" | "caravan-evicted" | "caravan-force" | "caravan-join-skipped"
                ) || label.starts_with("caravan-priority:")
            })
            .cloned()
            .collect::<Vec<_>>()
            .join("|")
    };
    let transition = format!("{}->{}", controls(before), controls(after));
    let mut fingerprint = String::with_capacity(transition.len() * 2);
    for byte in transition.bytes() {
        write!(&mut fingerprint, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("v2:{operation}:{number}:{}:{fingerprint}", head_oid.0)
}

impl ControlLabelAudit {
    /// Render stable Markdown. The marker is intentionally GitHub-visible so
    /// retries can deduplicate without process-local state.
    #[must_use]
    pub fn body(&self) -> String {
        format!(
            "<!-- caravan-control-label-audit:{} -->\n### Caravan queue transition: `{}`\n\n- **Labels:** `{}` → `{}`\n- **Actor/source:** {}\n- **Reason:** {} ({})\n- **Compatibility:** {}\n- **Clean squash:** {}\n- **Admission priority:** {}\n",
            self.marker,
            self.operation,
            self.before_labels
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            self.after_labels
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            self.actor,
            self.reason,
            self.reason_source,
            self.compatibility_evidence,
            self.clean_squash_evidence,
            self.admission_priority_basis,
        )
    }

    #[must_use]
    pub fn visible_marker(&self) -> String {
        format!("<!-- caravan-control-label-audit:{} -->", self.marker)
    }
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
    /// A requested check suite belongs to another commit, so retriggering it
    /// would report against a superseded generation.
    CheckSuiteHeadMismatch {
        /// Check suite ID.
        check_suite_id: u64,
        /// PR head expected by the caller.
        expected_head: String,
        /// Head observed on the check suite.
        actual_head: String,
    },
    /// A reviewed single-request provider transition returned without the
    /// complete requested postcondition. Before/after facts make a partial
    /// GraphQL response explicit and resumable rather than guessed.
    AtomicTransactionIncomplete {
        operation: String,
        before: Box<model::PullRequestSnapshot>,
        after: Box<model::PullRequestSnapshot>,
        desired_label_present: bool,
        desired_squash_auto_merge: bool,
        provider_error: Option<String>,
    },
    /// One provider node required to construct an exact GraphQL mutation was
    /// absent from the fresh repository lookup.
    MissingProviderResource { resource: String },
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
            Self::CheckSuiteHeadMismatch {
                check_suite_id,
                expected_head,
                actual_head,
            } => write!(
                formatter,
                "check suite {check_suite_id} belongs to {actual_head}, expected unchanged head {expected_head}"
            ),
            Self::AtomicTransactionIncomplete {
                operation,
                desired_label_present,
                desired_squash_auto_merge,
                provider_error,
                ..
            } => write!(
                formatter,
                "atomic provider transaction `{operation}` did not converge (label_present={desired_label_present}, squash_auto_merge={desired_squash_auto_merge}): {}",
                provider_error
                    .as_deref()
                    .unwrap_or("provider returned an incomplete postcondition")
            ),
            Self::MissingProviderResource { resource } => {
                write!(formatter, "provider resource `{resource}` is unavailable")
            }
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
    /// Exact `owner/name` from configuration, when the checkout cannot supply
    /// it. Provider identity is never guessed from a mirror remote.
    configured_repository: Option<String>,
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
        Self {
            runner,
            configured_repository: None,
        }
    }

    /// Name the exact repository instead of inferring it from git remotes.
    #[must_use]
    pub fn with_configured_repository(mut self, slug: Option<String>) -> Self {
        self.configured_repository = slug;
        self
    }

    /// Read one bounded page of provider-native Stacks without mutating Stack,
    /// PR, branch, or local tracking state.
    pub fn native_stack_inventory(
        &self,
        repository: &RepositoryId,
    ) -> Result<GitHubStackInventory, GitHubStackReadError> {
        let command = native_stack_list_command(repository);
        let output = self
            .runner
            .run(&command)
            .map_err(GitHubStackReadError::Runner)?;
        if !output.is_success() {
            let diagnostic = output.stderr.trim().to_owned();
            if diagnostic.contains("HTTP 404") {
                return Err(GitHubStackReadError::Unavailable { diagnostic });
            }
            return Err(GitHubStackReadError::CommandFailed {
                code: output.code,
                diagnostic,
            });
        }
        let stacks: Vec<GitHubStackSnapshot> =
            serde_json::from_str(&output.stdout).map_err(|error| {
                GitHubStackReadError::InvalidJson {
                    message: error.to_string(),
                }
            })?;
        Ok(GitHubStackInventory {
            truncated: stacks.len() == 100,
            stacks,
        })
    }

    /// Resolve only repository identity and default-branch name. This bounded
    /// initialization read deliberately performs no PR or Git-ref discovery.
    pub fn repository_identity(&self) -> Result<(RepositoryId, String), MutationError> {
        let repository_json: RepositoryJson =
            self.json(repository_command(self.configured_repository.as_deref()))?;
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

    /// Whether repository settings permit squash merging at all.
    ///
    /// This is the only merge capability a caravan-owned tick needs: Cara
    /// performs the squash itself, so a repository that deliberately disabled
    /// native auto-merge must still synchronize normally.
    pub fn repository_allows_squash_merge(
        &self,
        repository: &RepositoryId,
    ) -> Result<bool, MutationError> {
        let settings: RepositorySettingsJson =
            self.json(repository_settings_command(repository))?;
        Ok(settings.allow_squash_merge)
    }

    /// Exact current head revision of one repository branch.
    pub fn branch_head_oid(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<CommitOid, MutationError> {
        let reference: GitRefJson =
            self.json(default_branch_command(&repository.slug(), branch))?;
        Ok(CommitOid(reference.object.sha))
    }

    /// Exact immutable commit/tree/parent identity from provider Git storage.
    pub fn commit_identity(
        &self,
        repository: &RepositoryId,
        expected_oid: &CommitOid,
    ) -> Result<model::GitCommitIdentity, MutationError> {
        let detail: CommitDetailJson =
            self.json(commit_command(repository, expected_oid.0.as_str()))?;
        let actual_oid = if detail.sha.is_empty() {
            expected_oid.clone()
        } else {
            CommitOid(detail.sha)
        };
        if actual_oid != *expected_oid {
            return Err(MutationError::MissingProviderResource {
                resource: format!("commit identity {expected_oid} resolved as {actual_oid}"),
            });
        }
        let tree = detail
            .commit
            .tree
            .ok_or_else(|| MutationError::MissingProviderResource {
                resource: format!("commit {expected_oid} tree identity"),
            })?;
        Ok(model::GitCommitIdentity {
            oid: actual_oid,
            tree_oid: CommitOid(tree.sha),
            parents: detail
                .parents
                .into_iter()
                .map(|parent| CommitOid(parent.sha))
                .collect(),
        })
    }

    /// Exact provider merge commit for one merged pull request, when exposed.
    pub fn merge_commit_oid(
        &self,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<Option<CommitOid>, MutationError> {
        let merged: MergeCommitJson = self.json(merge_commit_command(repository, number))?;
        Ok(merged
            .merge_commit
            .map(|commit| CommitOid(commit.oid))
            .filter(|oid| !oid.0.is_empty()))
    }

    /// Squash-merge one pull request as an ordinary authenticated actor.
    ///
    /// Deliberately *not* `--admin`: administrator bypass exists for landing
    /// non-green forced heads, and reusing it for routine caravan landings would
    /// silently downgrade branch protection. The exact head is fenced by the
    /// provider through `--match-head-commit`, so a generation that moved during
    /// the tick cannot be merged.
    pub fn squash_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.mutate_pull_request(
            repository,
            expected,
            MutationKind::SquashMerge,
            squash_merge_command(repository, expected.number, &expected.head_oid),
        )
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

    /// Read exact REST core and GraphQL rate-limit resources before mutation.
    pub fn rate_limits(&self) -> Result<model::GitHubRateLimits, MutationError> {
        let response: RateLimitResponseJson = self.json(rate_limit_command())?;
        Ok(model::GitHubRateLimits {
            core: response.resources.core.into(),
            graphql: response.resources.graphql.map(Into::into),
        })
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

    /// Return bounded structured generation metadata for every open provider PR.
    /// Parsing is faithful; same-stream/supersession policy lives in the
    /// generation domain module.
    pub fn open_generation_facts(
        &self,
        repository: &RepositoryId,
    ) -> Result<Vec<model::PullRequestGenerationFact>, MutationError> {
        let pulls: Vec<GenerationPullRequestJson> =
            self.json(open_generation_pr_command(&repository.slug(), 1_000))?;
        Ok(pulls
            .iter()
            .map(GenerationPullRequestJson::generation_fact)
            .collect())
    }

    /// Compare two exact commit OIDs without changing refs or local worktrees.
    pub fn compare_commits(
        &self,
        repository: &RepositoryId,
        base: &CommitOid,
        head: &CommitOid,
    ) -> Result<crate::generation::CommitRelation, MutationError> {
        let comparison: CommitComparisonJson =
            self.json(compare_commits_command(repository, base, head))?;
        Ok(match comparison.status.as_str() {
            "ahead" => crate::generation::CommitRelation::Ahead,
            "behind" => crate::generation::CommitRelation::Behind,
            "identical" => crate::generation::CommitRelation::Identical,
            "diverged" => crate::generation::CommitRelation::Diverged,
            other => crate::generation::CommitRelation::Unknown {
                reason: format!("provider returned unknown compare status `{other}`"),
            },
        })
    }

    /// Refetch one PR by number without applying policy.
    pub fn refetch_pull_request(
        &self,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<model::PullRequestSnapshot, MutationError> {
        self.refetch_selector(repository, &number.to_string())
    }

    /// Refetch and compare mutation-authority facts. Check/CI progress is
    /// intentionally excluded so queued→running churn cannot stale unrelated
    /// base/label/auto-merge operations.
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

    /// Refetch and compare mutation authority plus exact observed checks for
    /// CI-specific operations such as diagnostics and rerun.
    pub fn verify_precondition_with_checks(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<model::PullRequestSnapshot, MutationError> {
        let actual_snapshot = self.refetch_pull_request(repository, expected.number)?;
        let actual = PullRequestPrecondition::from(&actual_snapshot);
        let changed_fields = changed_precondition_fields_with_checks(expected, &actual);
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

    /// Post a control-label audit comment, or return an already-satisfied
    /// receipt when its deterministic marker is present in GitHub comments.
    pub fn ensure_control_label_comment(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        audit: &ControlLabelAudit,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        let before = self.verify_precondition(repository, expected)?;
        let comment_pages: Vec<Vec<IssueCommentJson>> =
            self.json(issue_comments_command(repository, expected.number))?;
        let marker = audit.visible_marker();
        let marker_prefix = format!(
            "<!-- caravan-control-label-audit:v2:{}:{}:{}:",
            audit.operation, expected.number, expected.head_oid.0
        );
        let after_state = format!(
            " → `{}`",
            audit
                .after_labels
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
        let latest_audit = comment_pages
            .iter()
            .flatten()
            .rev()
            .find(|comment| comment.body.contains("<!-- caravan-control-label-audit:"));
        let already_visible = latest_audit.is_some_and(|comment| {
            comment.body.contains(&marker)
                || (comment.body.contains(&marker_prefix) && comment.body.contains(&after_state))
        });
        let provider_output = if already_visible {
            Some(format!("existing GitHub comment {marker}"))
        } else {
            let output = self.checked(comment_pull_request_command(
                repository,
                expected.number,
                &audit.body(),
            ))?;
            trimmed_provider_output(&output).or(Some(marker))
        };
        let after = self.refetch_pull_request(repository, expected.number)?;
        Ok(GitHubMutationReceipt {
            kind: MutationKind::Comment,
            before: Some(before),
            after,
            provider_output,
        })
    }

    /// Return bounded GitHub-visible PR comment bodies in provider order.
    pub fn pull_request_comment_bodies(
        &self,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<Vec<String>, MutationError> {
        let pages: Vec<Vec<IssueCommentJson>> =
            self.json(issue_comments_command(repository, number))?;
        Ok(pages
            .into_iter()
            .flatten()
            .map(|comment| comment.body)
            .collect())
    }

    /// Post one deterministically marked comment, or prove it already exists.
    pub fn ensure_marked_comment(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        marker: &str,
        body: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        let before = self.verify_precondition(repository, expected)?;
        let comments = self.pull_request_comment_bodies(repository, expected.number)?;
        let provider_output = if comments.iter().any(|comment| comment.contains(marker)) {
            Some(format!("existing GitHub comment {marker}"))
        } else {
            let output = self.checked(comment_pull_request_command(
                repository,
                expected.number,
                body,
            ))?;
            trimmed_provider_output(&output).or_else(|| Some(marker.to_owned()))
        };
        let after = self.refetch_pull_request(repository, expected.number)?;
        Ok(GitHubMutationReceipt {
            kind: MutationKind::Comment,
            before: Some(before),
            after,
            provider_output,
        })
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

    /// Replace the complete label set in one provider write after an exact
    /// precondition read. This is the safe primitive for lifecycle transitions
    /// that would otherwise expose an add/remove partial state between calls.
    pub fn replace_labels(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        labels: &BTreeSet<String>,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.mutate_pull_request(
            repository,
            expected,
            MutationKind::SetLabels,
            replace_labels_command(repository, expected.number, labels),
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

    /// Converge one exact label state and, when requested, squash auto-merge in
    /// one GraphQL mutation request after a check-sensitive PR precondition.
    ///
    /// The provider may report GraphQL errors after applying an earlier aliased
    /// field, so this primitive always refetches and either proves the complete
    /// postcondition or returns explicit before/after partial-state evidence.
    pub fn atomic_label_and_squash_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
        desired_label_present: bool,
        ensure_squash_auto_merge: bool,
    ) -> Result<(GitHubMutationReceipt, bool), MutationError> {
        let before = self.verify_precondition_with_checks(repository, expected)?;
        let label_change = before.has_label(label) != desired_label_present;
        let squash_ready = before.auto_merge.enabled
            && before.auto_merge.merge_method == Some(MergeMethod::Squash);
        let auto_merge_change = ensure_squash_auto_merge && !squash_ready;
        if !label_change && !auto_merge_change {
            return Ok((
                GitHubMutationReceipt {
                    kind: MutationKind::ForceIntentTransaction,
                    before: Some(before.clone()),
                    after: before,
                    provider_output: Some(
                        "atomic provider force postcondition already satisfied".to_owned(),
                    ),
                },
                false,
            ));
        }

        let ids: ForceTransactionIdsResponse = self.json(force_transaction_ids_command(
            repository,
            expected.number,
            label,
        ))?;
        let pull_request_id = ids
            .data
            .repository
            .pull_request
            .map(|pull| pull.id)
            .ok_or_else(|| MutationError::MissingProviderResource {
                resource: format!("pull_request:{}", expected.number),
            })?;
        let label_id = if label_change {
            Some(
                ids.data
                    .repository
                    .label
                    .map(|label| label.id)
                    .ok_or_else(|| MutationError::MissingProviderResource {
                        resource: format!("label:{label}"),
                    })?,
            )
        } else {
            None
        };
        let command = force_transaction_command(
            &pull_request_id,
            label_id.as_deref(),
            desired_label_present,
            auto_merge_change,
        );
        let output = self.runner.run(&command).map_err(DiscoveryError::from)?;
        let after = self.refetch_pull_request(repository, expected.number)?;
        let label_converged = after.has_label(label) == desired_label_present;
        let auto_merge_converged = !ensure_squash_auto_merge
            || (after.auto_merge.enabled
                && after.auto_merge.merge_method == Some(MergeMethod::Squash));
        let provider_error = (!output.is_success()).then(|| {
            format!(
                "status {}: {}",
                output
                    .code
                    .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                diagnostic_excerpt(&output.stderr)
            )
        });
        if !label_converged || !auto_merge_converged {
            return Err(MutationError::AtomicTransactionIncomplete {
                operation: "label_and_squash_auto_merge".to_owned(),
                before: Box::new(before),
                after: Box::new(after),
                desired_label_present,
                desired_squash_auto_merge: ensure_squash_auto_merge,
                provider_error,
            });
        }
        let provider_output = if let Some(error) = provider_error {
            Some(format!(
                "provider reported `{error}`, but exact refetch proved the complete postcondition"
            ))
        } else {
            let value = output.stdout.trim();
            (!value.is_empty()).then(|| diagnostic_excerpt(value))
        };
        Ok((
            GitHubMutationReceipt {
                kind: MutationKind::ForceIntentTransaction,
                before: Some(before),
                after,
                provider_output,
            },
            true,
        ))
    }

    /// List failed Actions runs for the exact PR head after verification.
    pub fn failed_runs_for_pull_request(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<Vec<WorkflowRunSnapshot>, MutationError> {
        let before = self.verify_precondition_with_checks(repository, expected)?;
        let runs: Vec<WorkflowRunJson> =
            self.json(failed_runs_command(repository, before.head.oid.0.as_str()))?;
        Ok(runs
            .into_iter()
            .map(Into::into)
            .filter(|run: &WorkflowRunSnapshot| run.head_sha == before.head.oid.0)
            .collect())
    }

    /// Fetch bounded structured job/step evidence for selected failed runs.
    pub fn failed_run_diagnostics(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        run_ids: &[u64],
    ) -> Result<crate::ci::WorkflowFailureDiagnostics, MutationError> {
        self.verify_precondition_with_checks(repository, expected)?;
        crate::ci::diagnose_failed_runs(&self.runner, repository, expected, run_ids)
            .map_err(Into::into)
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
        let before = self.verify_precondition_with_checks(repository, expected)?;
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

    /// Exact protection-declared required contexts for one arbitrary branch.
    ///
    /// An unprotected branch deliberately returns an empty *complete* read: it
    /// requires nothing, so nothing about it can stall a caravan. A protection
    /// endpoint the token may not read returns an explicitly *partial* read, so
    /// required-run policy reports unknown provider state instead of inventing a
    /// missing context from a permission error.
    pub fn branch_required_contexts(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<crate::required_runs::RequiredContextsRead, MutationError> {
        let settings: BranchSettingsJson =
            match self.json::<BranchSettingsJson>(branch_settings_command(repository, branch)) {
                Ok(settings) => settings,
                Err(_) => return Ok(crate::required_runs::RequiredContextsRead::partial(branch)),
            };
        if !settings.protected {
            return Ok(crate::required_runs::RequiredContextsRead::unprotected(
                branch,
            ));
        }
        let Ok(policy) =
            self.json::<BranchProtectionJson>(branch_protection_command(repository, branch))
        else {
            return Ok(crate::required_runs::RequiredContextsRead::partial(branch));
        };
        let mut contexts = policy
            .required_status_checks
            .as_ref()
            .map(|checks| checks.contexts.clone())
            .unwrap_or_default();
        if let Some(checks) = policy.required_status_checks.as_ref() {
            contexts.extend(checks.checks.iter().map(|check| check.context.clone()));
        }
        Ok(crate::required_runs::RequiredContextsRead {
            branch: branch.to_owned(),
            protected: true,
            contexts,
            complete: true,
        }
        .normalized())
    }

    /// Check-suite and workflow-run lineage for the exact verified PR head.
    ///
    /// Each sub-read degrades independently: a refused or unparsable response
    /// marks the lineage incomplete rather than being reported as an absence.
    pub fn head_run_lineage(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<crate::required_runs::HeadRunLineage, MutationError> {
        let before = self.verify_precondition_with_checks(repository, expected)?;
        let head_sha = before.head.oid.0.clone();
        let mut complete = true;
        let check_suites = self
            .json::<CheckSuiteListJson>(check_suites_command(repository, head_sha.as_str()))
            .map_or_else(
                |_| {
                    complete = false;
                    Vec::new()
                },
                |list| {
                    list.check_suites
                        .into_iter()
                        .map(Into::into)
                        .collect::<Vec<_>>()
                },
            );
        let workflow_runs = self
            .json::<WorkflowRunListJson>(head_runs_command(repository, head_sha.as_str()))
            .map_or_else(
                |_| {
                    complete = false;
                    Vec::new()
                },
                |list| {
                    list.workflow_runs
                        .into_iter()
                        .map(Into::into)
                        .collect::<Vec<_>>()
                },
            );
        let head_committed_at = self
            .json::<CommitDetailJson>(commit_command(repository, head_sha.as_str()))
            .ok()
            .map(|commit| commit.commit.committer.date);
        if head_committed_at.is_none() {
            complete = false;
        }
        Ok(crate::required_runs::HeadRunLineage {
            head_sha,
            check_suites,
            workflow_runs,
            head_committed_at,
            complete,
        }
        .bounded())
    }

    /// Request exactly one check suite again on the *unchanged* verified head.
    ///
    /// The suite is re-read first and refused unless it belongs to the exact
    /// current head, so a superseded generation can never be retriggered. No
    /// head, base, branch, or membership fact is touched.
    pub fn rerequest_check_suite(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        check_suite_id: u64,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        let before = self.verify_precondition_with_checks(repository, expected)?;
        let suite: CheckSuiteJson = self.json(check_suite_command(repository, check_suite_id))?;
        if suite.head_sha != before.head.oid.0 {
            return Err(MutationError::CheckSuiteHeadMismatch {
                check_suite_id,
                expected_head: before.head.oid.0.clone(),
                actual_head: suite.head_sha,
            });
        }
        let output = self.checked(rerequest_check_suite_command(repository, check_suite_id))?;
        let after = self.refetch_pull_request(repository, expected.number)?;
        Ok(GitHubMutationReceipt {
            kind: MutationKind::RequestCheckSuite,
            before: Some(before),
            after,
            provider_output: trimmed_provider_output(&output),
        })
    }

    /// Authenticated repository permission used by force-policy evidence.
    pub fn viewer_permission(&self, repository: &RepositoryId) -> Result<String, MutationError> {
        let permission: RepositoryPermissionJson =
            self.json(repository_permission_command(repository))?;
        Ok(permission.viewer_permission)
    }

    /// Force-squash one PR only when GitHub reports administrator permission.
    pub fn admin_squash_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        let permission = self.viewer_permission(repository)?;
        if permission != "ADMIN" {
            return Err(MutationError::PermissionDenied {
                required: "ADMIN".to_owned(),
                actual: permission,
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

pub(crate) fn changed_precondition_fields(
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
    if expected.merged_at != actual.merged_at {
        changed.push("merged_at".to_owned());
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

fn changed_precondition_fields_with_checks(
    expected: &PullRequestPrecondition,
    actual: &PullRequestPrecondition,
) -> Vec<String> {
    let mut changed = changed_precondition_fields(expected, actual);
    if comparable_checks(expected) != comparable_checks(actual) {
        changed.push("checks".to_owned());
    }
    changed
}

/// The decision-relevant projection of a check list.
///
/// bd-eff1dc added provider run timestamps so admission can tell a superseded
/// run from the current one. Those timestamps are evidence, not policy, and
/// they move constantly: a running check gains a `completedAt` seconds later.
/// Comparing them here would turn ordinary CI progress into a stale-precondition
/// refusal during exactly the retry storms this lineage work exists to survive.
/// Everything the old fence compared is still compared.
fn comparable_checks(
    precondition: &PullRequestPrecondition,
) -> Vec<(&str, model::CheckState, Option<&str>, Option<&str>)> {
    precondition
        .checks
        .iter()
        .map(|check| {
            (
                check.name.as_str(),
                check.state,
                check.provider_state.as_deref(),
                check.details_url.as_deref(),
            )
        })
        .collect()
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

    /// Resolve canonical provider identity/freshness for one exact PR snapshot.
    /// This shares the status/show schema and performs no checkout or mutation.
    /// Resolve canonical provider identity/freshness for one exact PR snapshot.
    /// This shares the status/show schema and performs no checkout or mutation.
    /// `default_branch` supplies the live tip a default-based candidate must be
    /// compared against; omit it only when that tip is genuinely unknown.
    pub fn merge_candidate_identity(
        &self,
        repository: &RepositoryId,
        pull_request: &model::PullRequestSnapshot,
        default_branch: Option<&BranchSnapshot>,
    ) -> Result<model::MergeCandidateIdentity, DiscoveryError> {
        let observed_at = provider_observed_at();
        let (mut identities, _) = self.merge_candidate_identities(
            repository,
            std::slice::from_ref(pull_request),
            &observed_at,
            default_branch,
        )?;
        identities.pop().ok_or_else(|| DiscoveryError::InvalidJson {
            command: merge_candidates_command(repository, std::slice::from_ref(pull_request)),
            message: "provider omitted the requested candidate identity".to_owned(),
            evidence: Box::new(JsonDecodeEvidence {
                stdout: String::new(),
                stderr: String::new(),
            }),
        })
    }

    /// Run a complete, internally consistent read-only discovery pass.
    #[allow(clippy::too_many_lines)]
    pub fn discover(&self) -> Result<model::RepositorySnapshot, DiscoveryError> {
        if self.options.open_limit == 0 {
            return Err(DiscoveryError::InvalidLimit("open_limit"));
        }
        if self.options.merged_limit == 0 {
            return Err(DiscoveryError::InvalidLimit("merged_limit"));
        }

        let repository_json: RepositoryJson =
            self.json(repository_command(self.options.repository.as_deref()))?;
        let default_branch_name = repository_json
            .default_branch_ref
            .ok_or(DiscoveryError::MissingDefaultBranch)?
            .name;
        let repository = repository_id(&repository_json.name_with_owner)?;
        let current_branch = self.current_branch()?;
        let previous_default_oid = self.previous_default_oid(&default_branch_name)?;
        let default_ref: GitRefJson = self.json(default_branch_command(
            &repository.slug(),
            &default_branch_name,
        ))?;
        let default_branch = BranchSnapshot {
            repository: repository.clone(),
            name: default_branch_name,
            oid: CommitOid(default_ref.object.sha),
        };

        // One bounded snapshot supplies current-branch lookup, active members,
        // admission candidates, and all live check rollups. Do not re-fetch the
        // same expensive rollups through current/labelled provider queries.
        let (all_open_prs, generation_facts) = if let Some(focus_pr) = self.options.focus_pr {
            let (mut active, _active_facts) = self.pull_requests_with_generation(
                labeled_pr_command(
                    &repository.slug(),
                    "open",
                    &self.options.label,
                    self.options.open_limit,
                    false,
                ),
                &repository,
            )?;
            let exact: PullRequestJson =
                self.json(pull_request_command(&repository, &focus_pr.0.to_string()))?;
            let exact = exact.into_snapshot(&repository)?;
            if !active
                .iter()
                .any(|pull_request| pull_request.number == focus_pr)
            {
                active.push(exact);
            }
            let generation_rows: Vec<GenerationPullRequestJson> =
                self.json(open_generation_pr_command(&repository.slug(), 1_000))?;
            let generation_facts = generation_rows
                .iter()
                .map(GenerationPullRequestJson::generation_fact)
                .collect();
            (active, generation_facts)
        } else {
            self.pull_requests_with_generation(
                open_pr_command(&repository.slug(), self.options.open_limit),
                &repository,
            )?
        };
        let current_pr = match &current_branch {
            Some(branch) => {
                let mut matches = all_open_prs
                    .iter()
                    .filter(|pull_request| pull_request.head.name == *branch)
                    .cloned()
                    .collect::<Vec<_>>();
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
        let open_labeled_prs = all_open_prs
            .iter()
            .filter(|pull_request| pull_request.labels.contains(&self.options.label))
            .cloned()
            .collect::<Vec<_>>();
        validate_active_heads(&repository, &open_labeled_prs)?;
        let observed_at = provider_observed_at();
        let bounded_members = open_labeled_prs
            .iter()
            .take(MERGE_CANDIDATE_LIMIT)
            .cloned()
            .collect::<Vec<_>>();
        let merge_candidates_truncated =
            open_labeled_prs.len().saturating_sub(bounded_members.len());
        let (merge_candidates, default_branch_movements) = self.merge_candidate_identities(
            &repository,
            &bounded_members,
            &observed_at,
            Some(&default_branch),
        )?;
        // bd-cd3be9: generation integrity must see recently merged siblings.
        // Facts built only from open PRs cannot prove that a candidate is
        // strictly contained by work that already landed, so a superseded
        // candidate stayed a live ordered admission attempt forever.
        let (recently_merged_labeled_prs, merged_generation_facts) = self
            .pull_requests_with_generation(
                labeled_pr_command(
                    &repository.slug(),
                    "merged",
                    &self.options.label,
                    self.options.merged_limit,
                    true,
                ),
                &repository,
            )?;
        // Closed lifecycle rows remain provider history, but never active graph
        // membership. Read every lifecycle label independently so an interrupted
        // or older transition (active-only, parked-only, or terminal-only) is
        // still discoverable and can be converged by the trusted sync adapter.
        // `--state closed` also returns merged rows; retaining terminal-labelled
        // merged rows lets sync remove an invalid `caravan-closed` label without
        // ever treating landed work as closed-unmerged.
        let closed_lifecycle_prs =
            if self.options.focus_pr.is_some() || !self.options.include_historical_pull_requests {
                // Exact candidate admission never converges closed lifecycle rows.
                // Keep those three provider scans on fleet status/sync only; the
                // lightweight open+merged generation facts above retain every
                // supersession/lineage proof the candidate guard consumes.
                Vec::new()
            } else {
                let (closed_active_prs, _closed_active_generation_facts) = self
                    .pull_requests_with_generation(
                        labeled_pr_command(
                            &repository.slug(),
                            "closed",
                            &self.options.label,
                            self.options.merged_limit,
                            true,
                        ),
                        &repository,
                    )?;
                let (closed_parked_prs, _closed_parked_generation_facts) = self
                    .pull_requests_with_generation(
                        labeled_pr_command(
                            &repository.slug(),
                            "closed",
                            PARKED_LABEL,
                            self.options.merged_limit,
                            true,
                        ),
                        &repository,
                    )?;
                let (closed_terminal_prs, _closed_terminal_generation_facts) = self
                    .pull_requests_with_generation(
                        labeled_pr_command(
                            &repository.slug(),
                            "closed",
                            CLOSED_LABEL,
                            self.options.merged_limit,
                            true,
                        ),
                        &repository,
                    )?;
                closed_active_prs
                    .into_iter()
                    .chain(closed_parked_prs)
                    .chain(closed_terminal_prs)
                    .filter(|pull_request| {
                        pull_request.is_closed_unmerged() || pull_request.has_label(CLOSED_LABEL)
                    })
                    .collect::<Vec<_>>()
            };
        let generation_facts = generation_facts
            .into_iter()
            .chain(merged_generation_facts)
            .collect::<Vec<_>>();

        let historical_pull_requests = self
            .options
            .include_historical_pull_requests
            .then(|| {
                recently_merged_labeled_prs
                    .into_iter()
                    .chain(closed_lifecycle_prs)
            })
            .into_iter()
            .flatten();
        let mut pull_requests = BTreeMap::new();
        for pull_request in all_open_prs
            .into_iter()
            .chain(open_labeled_prs)
            .chain(historical_pull_requests)
            .chain(current_pr.clone())
        {
            pull_requests
                .entry(pull_request.number)
                .or_insert(pull_request);
        }

        let current_pr_number = self.resolve_current_pr(
            &repository,
            &default_branch.name,
            current_branch.as_deref(),
            current_pr,
            &mut pull_requests,
        )?;

        Ok(model::RepositorySnapshot {
            repository,
            default_branch,
            merge_candidates,
            merge_candidates_truncated,
            previous_default_oid,
            default_branch_movements,
            current_branch,
            current_pr: current_pr_number,
            pull_requests: pull_requests.into_values().collect(),
            generation_facts,
            observed_at: None,
        })
    }

    fn resolve_current_pr(
        &self,
        repository: &RepositoryId,
        default_branch: &str,
        current_branch: Option<&str>,
        current_pr: Option<model::PullRequestSnapshot>,
        pulls: &mut BTreeMap<PrNumber, model::PullRequestSnapshot>,
    ) -> Result<Option<PrNumber>, DiscoveryError> {
        if let Some(current) = current_pr {
            return Ok(Some(current.number));
        }
        let Some(branch) = current_branch.filter(|branch| *branch != default_branch) else {
            return Ok(None);
        };
        // A fleet-level read must not fail because the local checkout sits on a
        // merged or ambiguous branch. Resolving "which PR do you mean" is only
        // required for PR-scoped work, and declining to guess is fail-closed:
        // every such command refuses with `current_pr_not_found` rather than
        // substituting a different subject.
        let resolved = match self.resolve_historical_current_pr(repository, branch, pulls) {
            Ok(resolved) => resolved,
            Err(error) => {
                if self.options.require_current_pr_resolution {
                    return Err(error);
                }
                return Ok(None);
            }
        };
        let Some((historical, successor)) = resolved else {
            return Ok(None);
        };
        pulls.entry(historical.number).or_insert(historical);
        Ok(successor)
    }

    fn merge_candidate_identities(
        &self,
        repository: &RepositoryId,
        pull_requests: &[model::PullRequestSnapshot],
        observed_at: &str,
        default_branch: Option<&BranchSnapshot>,
    ) -> Result<
        (
            Vec<model::MergeCandidateIdentity>,
            Vec<model::DefaultBranchMovement>,
        ),
        DiscoveryError,
    > {
        let command = merge_candidates_command(repository, pull_requests);
        let response: MergeCandidatesResponse = self.json(command)?;
        let candidates = pull_requests
            .iter()
            .enumerate()
            .map(|(index, pull_request)| {
                let commit = response
                    .data
                    .repository
                    .candidates
                    .get(&format!("c{index}"))
                    .and_then(Option::as_ref);
                let synthetic = commit.map(|commit| model::SyntheticMergeCandidate {
                    git_ref: format!("refs/pull/{}/merge", pull_request.number),
                    oid: CommitOid(commit.oid.clone()),
                    tree_oid: CommitOid(commit.tree.oid.clone()),
                    parents: commit
                        .parents
                        .nodes
                        .iter()
                        .map(|parent| CommitOid(parent.oid.clone()))
                        .collect(),
                });
                let (freshness, compared_base, stale_base, stale_head, stale_reasons) =
                    classify_candidate(
                        synthetic.as_ref(),
                        &pull_request.base,
                        &pull_request.head.oid,
                        default_branch,
                    );
                Ok(model::MergeCandidateIdentity {
                    pr: pull_request.number,
                    provider_updated_at: pull_request.updated_at.clone().unwrap_or_default(),
                    observed_at: observed_at.to_owned(),
                    base: pull_request.base.clone(),
                    head: pull_request.head.clone(),
                    synthetic,
                    auto_merge: model::NativeAutoMergeState {
                        enabled: pull_request.auto_merge.enabled,
                        merge_method: pull_request.auto_merge.merge_method,
                        actor: pull_request.auto_merge.actor.clone(),
                    },
                    freshness,
                    compared_base,
                    stale_base,
                    stale_head,
                    stale_reasons,
                })
            })
            .collect::<Result<Vec<_>, DiscoveryError>>()?;
        let movements = response
            .data
            .repository
            .default_branch_ref
            .map(|reference| reference.target.history.nodes)
            .unwrap_or_default()
            .into_iter()
            .map(|commit| {
                let source = commit.associated_pull_requests.nodes.into_iter().next();
                let cara_owned = source.as_ref().is_some_and(|pull| {
                    pull.labels
                        .nodes
                        .iter()
                        .any(|label| label.name == "caravan")
                });
                model::DefaultBranchMovement {
                    oid: CommitOid(commit.oid),
                    timestamp: commit.committed_date,
                    actor: commit
                        .author
                        .and_then(|author| author.user.map(|user| user.login).or(author.name)),
                    source_pr: source.map(|pull| PrNumber(pull.number)),
                    ownership: if cara_owned {
                        // A label proves queue association, not the merge actor.
                        // Status may upgrade this only with a matching durable
                        // Cara operation receipt; never falsely claim ownership.
                        model::MovementOwnership::Unknown
                    } else {
                        model::MovementOwnership::External
                    },
                }
            })
            .collect();
        Ok((candidates, movements))
    }

    fn previous_default_oid(&self, branch: &str) -> Result<Option<CommitOid>, DiscoveryError> {
        let command = previous_default_command(branch);
        let output = self.runner.run(&command)?;
        if output.code == Some(1) || output.code == Some(128) {
            return Ok(None);
        }
        if !output.is_success() {
            return Err(DiscoveryError::CommandFailed {
                command,
                code: output.code,
                stderr: output.stderr.trim().to_owned(),
            });
        }
        let oid = output.stdout.trim();
        Ok((!oid.is_empty()).then(|| CommitOid(oid.to_owned())))
    }

    /// Resolve an exact retained merged branch through bounded Caravan history.
    /// The returned number is the active rolling successor, not the merged PR;
    /// callers can recover the predecessor from `current_branch` and the
    /// included merged snapshots for explicit receipts.
    #[allow(clippy::too_many_lines)]
    fn resolve_historical_current_pr(
        &self,
        repository: &RepositoryId,
        branch: &str,
        pulls: &BTreeMap<PrNumber, model::PullRequestSnapshot>,
    ) -> Result<Option<(model::PullRequestSnapshot, Option<PrNumber>)>, DiscoveryError> {
        let mut history = self.pull_requests(
            branch_pr_history_command(&repository.slug(), branch, self.options.merged_limit),
            repository,
        )?;
        history.retain(|pull| pull.head.name == branch);
        history.sort_by_key(|pull| pull.number);
        let candidates = history.iter().map(|pull| pull.number.0).collect::<Vec<_>>();
        if history.is_empty() {
            return Ok(None);
        }

        if let Some(current) =
            self.resolve_exact_open_reused_branch(repository, branch, &history)?
        {
            return Ok(Some((current.clone(), Some(current.number))));
        }

        if history.len() != 1 {
            return Err(DiscoveryError::HistoricalCurrentPullRequest {
                reason: "branch_reuse_ambiguous",
                branch: branch.to_owned(),
                candidates,
            });
        }
        let historical = &history[0];
        let fail = |reason| DiscoveryError::HistoricalCurrentPullRequest {
            reason,
            branch: branch.to_owned(),
            candidates: vec![historical.number.0],
        };
        if historical.state != model::PullRequestState::Merged {
            return Err(fail("closed_unmerged"));
        }
        if !historical.has_label(&self.options.label) {
            if !self.options.allow_unlabelled_historical_pr_creation {
                return Err(fail("missing_caravan_label"));
            }
            if historical.cross_repository || historical.head.repository != *repository {
                return Err(fail("fork_only_head"));
            }
            self.validate_reused_historical_head(repository, branch, historical)?;
            return Ok(None);
        }
        if historical.cross_repository || historical.head.repository != *repository {
            return Err(fail("fork_only_head"));
        }

        self.validate_historical_head(repository, branch, historical)?;

        let mut base_history = BTreeMap::new();
        let mut current = historical.number;
        loop {
            let predecessor = pulls.get(&current).unwrap_or(historical);
            let mut successors = pulls
                .values()
                .filter(|candidate| {
                    candidate.number != current
                        && candidate.has_label(&self.options.label)
                        && candidate.head.repository == *repository
                        && !candidate.cross_repository
                        && candidate.base.name == predecessor.head.name
                })
                .map(|candidate| candidate.number)
                .collect::<Vec<_>>();
            if successors.is_empty() {
                if base_history.is_empty() {
                    base_history = self.base_ref_history(repository, pulls.keys().copied())?;
                }
                successors = pulls
                    .values()
                    .filter(|candidate| {
                        candidate.number != current
                            && candidate.has_label(&self.options.label)
                            && candidate.head.repository == *repository
                            && !candidate.cross_repository
                            && base_history
                                .get(&candidate.number)
                                .is_some_and(|names| names.contains(&predecessor.head.name))
                    })
                    .map(|candidate| candidate.number)
                    .collect();
            }
            successors.sort_unstable();
            successors.dedup();
            match successors.as_slice() {
                [] => return Ok(Some((historical.clone(), None))),
                [successor] => {
                    let successor_pull = pulls.get(successor).expect("successor came from pulls");
                    if successor_pull.state == model::PullRequestState::Open
                        && successor_pull.is_active_caravan_member()
                    {
                        return Ok(Some((historical.clone(), Some(*successor))));
                    }
                    if successor_pull.state != model::PullRequestState::Merged {
                        return Err(fail("successor_not_active_or_merged"));
                    }
                    current = *successor;
                }
                _ => {
                    return Err(DiscoveryError::HistoricalCurrentPullRequest {
                        reason: "ambiguous_successor",
                        branch: branch.to_owned(),
                        candidates: successors.iter().map(|number| number.0).collect(),
                    });
                }
            }
        }
    }

    fn resolve_exact_open_reused_branch(
        &self,
        repository: &RepositoryId,
        branch: &str,
        history: &[model::PullRequestSnapshot],
    ) -> Result<Option<model::PullRequestSnapshot>, DiscoveryError> {
        // The bounded all-open rollup can omit a recently opened PR when a
        // repository is at its provider limit. Before treating branch text as
        // retained merged history, give one exact open same-repository head
        // precedence. Duplicate open reuse remains ambiguous even when only one
        // candidate happens to match the local OID.
        let open = history
            .iter()
            .filter(|pull| pull.state == model::PullRequestState::Open)
            .cloned()
            .collect::<Vec<_>>();
        if open.is_empty() {
            return Ok(None);
        }
        let open_candidates = open.iter().map(|pull| pull.number.0).collect::<Vec<_>>();
        let fail = |reason| DiscoveryError::HistoricalCurrentPullRequest {
            reason,
            branch: branch.to_owned(),
            candidates: open_candidates.clone(),
        };
        if open.len() != 1 {
            return Err(fail("open_branch_reuse_ambiguous"));
        }
        let current = &open[0];
        if current.cross_repository || current.head.repository != *repository {
            return Err(fail("fork_only_open_head"));
        }
        let local_oid = self.command_text(current_head_oid_command())?;
        let remote: GitRefJson = self.json(historical_head_command(&repository.slug(), branch))?;
        if current.head.oid.0 != local_oid || remote.object.sha != local_oid {
            return Err(fail("exact_open_head_mismatch"));
        }
        let conflicting_history = history.iter().any(|pull| {
            pull.number != current.number
                && (pull.has_label(&self.options.label) || pull.has_label("caravan-evicted"))
        });
        if conflicting_history {
            return Err(fail("historical_membership_conflict"));
        }
        Ok(Some(current.clone()))
    }

    fn validate_reused_historical_head(
        &self,
        repository: &RepositoryId,
        branch: &str,
        historical: &model::PullRequestSnapshot,
    ) -> Result<(), DiscoveryError> {
        let fail = |reason| DiscoveryError::HistoricalCurrentPullRequest {
            reason,
            branch: branch.to_owned(),
            candidates: vec![historical.number.0],
        };
        let local_oid = self.command_text(current_head_oid_command())?;
        let remote: GitRefJson = self.json(historical_head_command(&repository.slug(), branch))?;
        if local_oid == historical.head.oid.0 || remote.object.sha == historical.head.oid.0 {
            return Err(fail("unchanged_merged_head"));
        }
        if local_oid != remote.object.sha {
            return Err(fail("unpublished_generation"));
        }
        let ancestry = self.runner.run(&CommandSpec::new("git").args([
            "merge-base",
            "--is-ancestor",
            historical.head.oid.0.as_str(),
            local_oid.as_str(),
        ]))?;
        if !ancestry.is_success() {
            return Err(fail("historical_head_not_ancestor"));
        }
        Ok(())
    }

    fn validate_historical_head(
        &self,
        repository: &RepositoryId,
        branch: &str,
        historical: &model::PullRequestSnapshot,
    ) -> Result<(), DiscoveryError> {
        let fail = |reason| DiscoveryError::HistoricalCurrentPullRequest {
            reason,
            branch: branch.to_owned(),
            candidates: vec![historical.number.0],
        };
        let local_oid = self.command_text(current_head_oid_command())?;
        let remote: GitRefJson = self
            .json(historical_head_command(&repository.slug(), branch))
            .map_err(|error| match &error {
                DiscoveryError::CommandFailed { code, stderr, .. }
                    if *code == Some(1) && stderr.contains("404") =>
                {
                    fail("deleted_head")
                }
                _ => error,
            })?;
        if local_oid != historical.head.oid.0 || remote.object.sha != historical.head.oid.0 {
            return Err(fail("stale_oid"));
        }
        Ok(())
    }

    fn base_ref_history(
        &self,
        repository: &RepositoryId,
        numbers: impl Iterator<Item = PrNumber>,
    ) -> Result<BTreeMap<PrNumber, BTreeSet<String>>, DiscoveryError> {
        let numbers = numbers.collect::<Vec<_>>();
        if numbers.is_empty() {
            return Ok(BTreeMap::new());
        }
        let response: BaseHistoryResponse =
            self.json(base_history_command(repository, &numbers))?;
        let mut result = BTreeMap::new();
        for (alias, pull) in response.data.repository.pulls {
            let Some(number) = alias.strip_prefix('p').and_then(|value| value.parse().ok()) else {
                continue;
            };
            let names = pull
                .timeline_items
                .nodes
                .into_iter()
                .filter_map(|node| node.previous_ref_name)
                .collect();
            result.insert(PrNumber(number), names);
        }
        Ok(result)
    }

    fn command_text(&self, command: CommandSpec) -> Result<String, DiscoveryError> {
        let output = self.runner.run(&command)?;
        if !output.is_success() {
            return Err(DiscoveryError::CommandFailed {
                command,
                code: output.code,
                stderr: output.stderr.trim().to_owned(),
            });
        }
        Ok(output.stdout.trim().to_owned())
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
        Ok(self.pull_requests_with_generation(command, repository)?.0)
    }

    fn pull_requests_with_generation(
        &self,
        command: CommandSpec,
        repository: &RepositoryId,
    ) -> Result<
        (
            Vec<model::PullRequestSnapshot>,
            Vec<model::PullRequestGenerationFact>,
        ),
        DiscoveryError,
    > {
        let pulls = self.pull_request_records(command)?;
        let generation_facts = pulls.iter().map(PullRequestJson::generation_fact).collect();
        let snapshots = pulls
            .into_iter()
            .map(|pull_request| pull_request.into_snapshot(repository))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((snapshots, generation_facts))
    }

    /// Decode a PR list record-by-record so one unusable record cannot discard
    /// the other twenty-seven.
    ///
    /// A single closed pull request took out every read of the repository:
    /// discovery is phase one of a tick, so nothing downstream ran at all. The
    /// error also reported only a byte column, and its guidance named GitHub's
    /// normal output as malformed provider stdout, which sent the reader off to
    /// repair the wrong thing. A skipped record is now named by PR number
    /// wherever the payload still yields one (bd-4e0430).
    fn pull_request_records(
        &self,
        command: CommandSpec,
    ) -> Result<Vec<PullRequestJson>, DiscoveryError> {
        let rows = self.json::<Vec<serde_json::Value>>(command.clone())?;
        let mut records = Vec::with_capacity(rows.len());
        let mut skipped = Vec::new();
        for row in rows {
            let number = row.get("number").and_then(serde_json::Value::as_u64);
            match serde_json::from_value::<PullRequestJson>(row) {
                Ok(record) => records.push(record),
                Err(error) => skipped.push((number, error.to_string())),
            }
        }
        // Every record unusable is a real provider or schema break rather than
        // one odd row, so that still fails closed.
        if records.is_empty() && !skipped.is_empty() {
            return Err(DiscoveryError::InvalidJson {
                command,
                message: format!(
                    "no pull request record could be decoded; first failure {}",
                    describe_skipped_record(&skipped[0])
                ),
                evidence: Box::new(JsonDecodeEvidence {
                    stdout: diagnostic_excerpt(&skipped_summary(&skipped)),
                    stderr: String::new(),
                }),
            });
        }
        Ok(records)
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

/// Name one skipped provider record by PR number where the payload supplied one.
fn describe_skipped_record(skipped: &(Option<u64>, String)) -> String {
    match skipped.0 {
        Some(number) => format!("PR #{number}: {}", skipped.1),
        None => format!("record without a number field: {}", skipped.1),
    }
}

/// Bounded summary of every skipped record, for evidence.
fn skipped_summary(skipped: &[(Option<u64>, String)]) -> String {
    skipped
        .iter()
        .map(describe_skipped_record)
        .collect::<Vec<_>>()
        .join("; ")
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

fn native_stack_list_command(repository: &RepositoryId) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api",
        &format!("repos/{}/stacks?per_page=100&page=1", repository.slug()),
    ])
}

fn repository_command(configured: Option<&str>) -> CommandSpec {
    // `gh repo view` infers identity from git remotes and takes the repository
    // POSITIONALLY: `-R`/`--repo` are rejected, and `GH_REPO` is not honoured.
    // A managed checkout whose origin is a local daemon mirror therefore has
    // nothing to infer from, and Cara died before reaching the queue at all
    // (bd-ff639b). Naming the repository explicitly is the only input that was
    // ever missing.
    let spec = CommandSpec::new("gh").args(["repo", "view"]);
    let spec = match configured {
        Some(slug) => spec.args([slug]),
        None => spec,
    };
    spec.args(["--json", "nameWithOwner,defaultBranchRef"])
}

fn merge_candidates_command(
    repository: &RepositoryId,
    pull_requests: &[model::PullRequestSnapshot],
) -> CommandSpec {
    let aliases = pull_requests
        .iter()
        .enumerate()
        .map(|(index, pull_request)| format!(
            "c{index}: object(expression:\"refs/pull/{}/merge\") {{ ... on Commit {{ oid tree {{ oid }} parents(first: 2) {{ nodes {{ oid }} }} }} }}",
            pull_request.number
        ))
        .collect::<Vec<_>>()
        .join(" ");
    let query = format!(
        "query($owner:String!,$name:String!) {{ rateLimit {{ cost remaining resetAt }} repository(owner:$owner,name:$name) {{ {aliases} defaultBranchRef {{ target {{ ... on Commit {{ history(first:20) {{ nodes {{ oid committedDate author {{ name user {{ login }} }} associatedPullRequests(first:5) {{ nodes {{ number labels(first:20) {{ nodes {{ name }} }} }} }} }} }} }} }} }} }} }}"
    );
    CommandSpec::new("gh").args([
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={query}"),
        "-F".to_owned(),
        format!("owner={}", repository.owner),
        "-F".to_owned(),
        format!("name={}", repository.name),
    ])
}

fn provider_observed_at() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or_else(
            |_| "unix:0".to_owned(),
            |duration| format!("unix:{}", duration.as_secs()),
        )
}

/// Classify one synthetic merge candidate against the exact provider
/// generations it must match.
///
/// `default_branch` is the live tip of the repository default branch when it is
/// known. A pull request based on the default branch is compared against that
/// tip, never against its own recorded `base.oid`: GitHub keeps serving the
/// base generation a PR was opened/last-synced against, so comparing against it
/// would report `stale_base=false` for a base that has been superseded for
/// hours. The compared generation is returned so receipts can prove the claim.
fn classify_candidate(
    candidate: Option<&model::SyntheticMergeCandidate>,
    base: &BranchSnapshot,
    head: &CommitOid,
    default_branch: Option<&BranchSnapshot>,
) -> (
    model::MergeCandidateFreshness,
    Option<BranchSnapshot>,
    bool,
    bool,
    Vec<String>,
) {
    let compared_base = default_branch
        .filter(|default| default.name == base.name && default.repository == base.repository)
        .map_or_else(|| base.clone(), Clone::clone);
    let mut reasons = Vec::new();
    let superseded_base = compared_base.oid != base.oid;
    if superseded_base {
        reasons.push(format!(
            "recorded pull-request base {} is superseded by the current {} tip {}",
            base.oid, compared_base.name, compared_base.oid
        ));
    }
    let Some(candidate) = candidate else {
        reasons.push("synthetic merge ref is unavailable".to_owned());
        return (
            model::MergeCandidateFreshness::Missing,
            Some(compared_base),
            superseded_base,
            false,
            reasons,
        );
    };
    if candidate.parents.len() != 2 {
        reasons.push(format!(
            "synthetic candidate has {} parents; expected 2",
            candidate.parents.len()
        ));
        return (
            model::MergeCandidateFreshness::Unknown,
            Some(compared_base),
            superseded_base,
            false,
            reasons,
        );
    }
    let stale_base = superseded_base || candidate.parents[0] != compared_base.oid;
    let stale_head = candidate.parents[1] != *head;
    if candidate.parents[0] != compared_base.oid {
        reasons.push(format!(
            "first parent {} does not match current base {}",
            candidate.parents[0], compared_base.oid
        ));
    }
    if stale_head {
        reasons.push(format!(
            "second parent {} does not match current head {}",
            candidate.parents[1], head
        ));
    }
    let freshness = if stale_head {
        model::MergeCandidateFreshness::StaleHead
    } else if stale_base {
        model::MergeCandidateFreshness::StaleBase
    } else {
        model::MergeCandidateFreshness::Fresh
    };
    (
        freshness,
        Some(compared_base),
        stale_base,
        stale_head,
        reasons,
    )
}

fn previous_default_command(branch: &str) -> CommandSpec {
    CommandSpec::new("git").args([
        "rev-parse".to_owned(),
        "--verify".to_owned(),
        format!("refs/remotes/origin/{branch}"),
    ])
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
    command.provider_write()
}

fn edit_pull_request_command(
    repository: &RepositoryId,
    number: PrNumber,
    flag: &str,
    value: &str,
) -> CommandSpec {
    CommandSpec::new("gh")
        .args([
            "pr".to_owned(),
            "edit".to_owned(),
            number.to_string(),
            "--repo".to_owned(),
            repository.slug(),
            flag.to_owned(),
            value.to_owned(),
        ])
        .provider_write()
}

fn replace_labels_command(
    repository: &RepositoryId,
    number: PrNumber,
    labels: &BTreeSet<String>,
) -> CommandSpec {
    let payload = serde_json::to_string(&serde_json::json!({"labels": labels}))
        .expect("label replacement payload serializes");
    CommandSpec::new("gh")
        .args([
            "api".to_owned(),
            "--method".to_owned(),
            "PUT".to_owned(),
            format!("repos/{}/issues/{number}/labels", repository.slug()),
            "--input".to_owned(),
            "-".to_owned(),
        ])
        .stdin(payload)
        .provider_write()
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
    .provider_write()
}

fn force_transaction_ids_command(
    repository: &RepositoryId,
    number: PrNumber,
    label: &str,
) -> CommandSpec {
    let query = "query($owner:String!,$name:String!,$number:Int!,$label:String!){repository(owner:$owner,name:$name){pullRequest(number:$number){id} label(name:$label){id}}}";
    CommandSpec::new("gh").args([
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={query}"),
        "-F".to_owned(),
        format!("owner={}", repository.owner),
        "-F".to_owned(),
        format!("name={}", repository.name),
        "-F".to_owned(),
        format!("number={number}"),
        "-F".to_owned(),
        format!("label={label}"),
    ])
}

fn force_transaction_command(
    pull_request_id: &str,
    label_id: Option<&str>,
    desired_label_present: bool,
    enable_squash_auto_merge: bool,
) -> CommandSpec {
    let mut fields = Vec::new();
    // GraphQL guarantees top-level mutation fields execute serially, but not
    // rollback across aliases. Enable queue-safe auto-merge first so any prefix
    // failure can never leave force intent armed without its reviewed holding
    // postcondition.
    if enable_squash_auto_merge {
        fields.push("forceAutoMerge:enablePullRequestAutoMerge(input:{pullRequestId:$pullRequestId,mergeMethod:SQUASH}){clientMutationId}".to_owned());
    }
    if label_id.is_some() {
        let mutation = if desired_label_present {
            "addLabelsToLabelable"
        } else {
            "removeLabelsFromLabelable"
        };
        fields.push(format!(
            "forceLabel:{mutation}(input:{{labelableId:$pullRequestId,labelIds:[$labelId]}}){{clientMutationId}}"
        ));
    }
    let variables = if label_id.is_some() {
        "$pullRequestId:ID!,$labelId:ID!"
    } else {
        "$pullRequestId:ID!"
    };
    let query = format!("mutation({variables}){{{}}}", fields.join(" "));
    let mut command = CommandSpec::new("gh").args([
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={query}"),
        "-f".to_owned(),
        format!("pullRequestId={pull_request_id}"),
    ]);
    if let Some(label_id) = label_id {
        command = command.args(["-f".to_owned(), format!("labelId={label_id}")]);
    }
    command.provider_write()
}

fn open_generation_pr_command(repository: &str, limit: usize) -> CommandSpec {
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
        GENERATION_PR_JSON_FIELDS.to_owned(),
    ])
}

fn compare_commits_command(
    repository: &RepositoryId,
    base: &CommitOid,
    head: &CommitOid,
) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        format!(
            "repos/{}/compare/{}...{}",
            repository.slug(),
            base.0,
            head.0
        ),
    ])
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
    CommandSpec::new("gh")
        .args([
            "run".to_owned(),
            "rerun".to_owned(),
            run_id.to_string(),
            "--repo".to_owned(),
            repository.slug(),
            "--failed".to_owned(),
        ])
        .provider_write()
}

fn check_suites_command(repository: &RepositoryId, head_oid: &str) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        format!(
            "repos/{}/commits/{head_oid}/check-suites?per_page=100",
            repository.slug()
        ),
    ])
}

fn head_runs_command(repository: &RepositoryId, head_oid: &str) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        format!(
            "repos/{}/actions/runs?head_sha={head_oid}&per_page=100",
            repository.slug()
        ),
    ])
}

fn commit_command(repository: &RepositoryId, head_oid: &str) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        format!("repos/{}/commits/{head_oid}", repository.slug()),
    ])
}

fn check_suite_command(repository: &RepositoryId, check_suite_id: u64) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        format!("repos/{}/check-suites/{check_suite_id}", repository.slug()),
    ])
}

fn rerequest_check_suite_command(repository: &RepositoryId, check_suite_id: u64) -> CommandSpec {
    CommandSpec::new("gh")
        .args([
            "api".to_owned(),
            "--method".to_owned(),
            "POST".to_owned(),
            format!(
                "repos/{}/check-suites/{check_suite_id}/rerequest",
                repository.slug()
            ),
        ])
        .provider_write()
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

fn rate_limit_command() -> CommandSpec {
    CommandSpec::new("gh").args(["api", "rate_limit"])
}

fn create_repository_label_command(
    repository: &RepositoryId,
    name: &str,
    color: &str,
    description: &str,
) -> CommandSpec {
    CommandSpec::new("gh")
        .args([
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
        .provider_write()
}

fn issue_comments_command(repository: &RepositoryId, number: PrNumber) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        format!(
            "repos/{}/issues/{number}/comments?per_page=100",
            repository.slug()
        ),
        "--paginate".to_owned(),
        "--slurp".to_owned(),
    ])
}

fn comment_pull_request_command(
    repository: &RepositoryId,
    number: PrNumber,
    body: &str,
) -> CommandSpec {
    CommandSpec::new("gh")
        .args([
            "pr".to_owned(),
            "comment".to_owned(),
            number.to_string(),
            "--repo".to_owned(),
            repository.slug(),
            "--body".to_owned(),
            body.to_owned(),
        ])
        .provider_write()
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
    CommandSpec::new("gh")
        .args([
            "pr".to_owned(),
            "merge".to_owned(),
            number.to_string(),
            "--repo".to_owned(),
            repository.slug(),
            "--admin".to_owned(),
            "--squash".to_owned(),
        ])
        .provider_write()
}

fn squash_merge_command(
    repository: &RepositoryId,
    number: PrNumber,
    head: &CommitOid,
) -> CommandSpec {
    CommandSpec::new("gh")
        .args([
            "pr".to_owned(),
            "merge".to_owned(),
            number.to_string(),
            "--repo".to_owned(),
            repository.slug(),
            "--squash".to_owned(),
            "--match-head-commit".to_owned(),
            head.0.clone(),
        ])
        .provider_write()
}

fn merge_commit_command(repository: &RepositoryId, number: PrNumber) -> CommandSpec {
    CommandSpec::new("gh").args([
        "pr".to_owned(),
        "view".to_owned(),
        number.to_string(),
        "--repo".to_owned(),
        repository.slug(),
        "--json".to_owned(),
        "mergeCommit".to_owned(),
    ])
}

fn branch_pr_history_command(repository: &str, branch: &str, limit: usize) -> CommandSpec {
    CommandSpec::new("gh").args([
        "pr",
        "list",
        "--repo",
        repository,
        "--state",
        "all",
        "--head",
        branch,
        "--limit",
        &limit.to_string(),
        "--json",
        PR_HISTORY_JSON_FIELDS,
    ])
}

fn current_head_oid_command() -> CommandSpec {
    CommandSpec::new("git").args(["rev-parse", "HEAD"])
}

fn historical_head_command(repository: &str, branch: &str) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        format!(
            "repos/{repository}/git/ref/heads/{}",
            encode_path_segment(branch)
        ),
    ])
}

fn base_history_command(repository: &RepositoryId, numbers: &[PrNumber]) -> CommandSpec {
    let selections = numbers
        .iter()
        .map(|number| format!("p{}: pullRequest(number:{}) {{ timelineItems(last:100, itemTypes:[BASE_REF_CHANGED_EVENT]) {{ nodes {{ ... on BaseRefChangedEvent {{ previousRefName currentRefName }} }} }} }}", number.0, number.0))
        .collect::<Vec<_>>()
        .join(" ");
    let query = format!(
        "query {{ repository(owner:\"{}\", name:\"{}\") {{ {selections} }} }}",
        repository.owner, repository.name
    );
    CommandSpec::new("gh").args(["api", "graphql", "-f", &format!("query={query}")])
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
        if state == "merged" {
            PR_HISTORY_JSON_FIELDS.to_owned()
        } else {
            PR_JSON_FIELDS.to_owned()
        },
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
    let normalized = provider_state.map(str::to_ascii_uppercase);
    match normalized.as_deref() {
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

mod raw;
mod stack;
mod stack_land;
mod stack_lock;
mod stack_merge;
mod stack_reshape;

// The transport layer is an implementation seam, not a new public surface:
// re-export exactly what was public before the split so no downstream path
// changes (bd-fcd5c9).
pub use raw::*;
pub use stack::*;
pub use stack_land::*;
pub use stack_lock::*;
pub use stack_merge::*;
pub use stack_reshape::*;

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;
    use crate::command::CommandOutput;
    use crate::model::AutoMergeState;

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
                .unwrap_or_else(|| panic!("unexpected subprocess call: {command:?}"));
            assert_eq!(&expected, command);
            Ok(output)
        }
    }

    fn successful_discovery_calls(open_prs: &str) -> Vec<(CommandSpec, CommandOutput)> {
        let pulls = serde_json::from_str::<Vec<PullRequestJson>>(open_prs)
            .unwrap()
            .into_iter()
            .map(|pull| pull.into_snapshot(&repository()).unwrap())
            .collect::<Vec<_>>();
        let mut candidates = pulls
            .iter()
            .enumerate()
            .map(|(index, pull)| {
                (
                    format!("c{index}"),
                    serde_json::json!({
                        "oid": format!("merge-{}", pull.number),
                        "tree": {"oid": format!("tree-{}", pull.number)},
                        "parents": {"nodes": [
                            {"oid": pull.base.oid.0}, {"oid": pull.head.oid.0}
                        ]}
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        candidates.insert(
            "defaultBranchRef".to_owned(),
            serde_json::json!({
                "target": {"history": {"nodes": [{
                    "oid": "default-sha",
                    "committedDate": "2026-07-18T10:00:00Z",
                    "author": {"name": "Outside User", "user": {"login": "outside"}},
                    "associatedPullRequests": {"nodes": []}
                }]}}
            }),
        );
        vec![
            (
                repository_command(None),
                CommandOutput::success(
                    r#"{"nameWithOwner":"acme/widgets","defaultBranchRef":{"name":"main"}}"#,
                ),
            ),
            (
                current_branch_command(),
                CommandOutput::success("feature/widget\n"),
            ),
            (
                previous_default_command("main"),
                CommandOutput::success("previous-default-sha\n"),
            ),
            (
                default_branch_command("acme/widgets", "main"),
                CommandOutput::success(r#"{"object":{"sha":"default-sha"}}"#),
            ),
            (
                open_pr_command("acme/widgets", 1_000),
                CommandOutput::success(open_prs),
            ),
            (
                merge_candidates_command(&repository(), &pulls),
                CommandOutput::success(
                    serde_json::json!({"data": {"repository": candidates}}).to_string(),
                ),
            ),
            (
                labeled_pr_command("acme/widgets", "merged", "caravan", 100, true),
                CommandOutput::success(merged_pr_json()),
            ),
            // Closed lifecycle rows are read by every lifecycle label so
            // interrupted and already-terminal transitions remain visible.
            (
                labeled_pr_command("acme/widgets", "closed", "caravan", 100, true),
                CommandOutput::success("[]"),
            ),
            (
                labeled_pr_command("acme/widgets", "closed", PARKED_LABEL, 100, true),
                CommandOutput::success("[]"),
            ),
            (
                labeled_pr_command("acme/widgets", "closed", CLOSED_LABEL, 100, true),
                CommandOutput::success("[]"),
            ),
        ]
    }

    #[test]
    fn sync_hot_discovery_defers_closed_and_merged_snapshots() {
        let open = pr_list_json(1, "feature/widget", "acme/widgets", false);
        let mut calls = successful_discovery_calls(&open);
        calls.truncate(calls.len().saturating_sub(3));
        let discovery =
            GitHubDiscovery::new(FakeRunner::new(calls)).with_options(DiscoveryOptions {
                include_historical_pull_requests: false,
                require_current_pr_resolution: false,
                ..DiscoveryOptions::default()
            });

        let snapshot = discovery.discover().expect("hot discovery");

        assert_eq!(
            snapshot
                .pull_requests
                .iter()
                .map(|pull_request| pull_request.number)
                .collect::<Vec<_>>(),
            vec![PrNumber(1)]
        );
        assert!(
            snapshot
                .generation_facts
                .iter()
                .any(|fact| fact.pr == PrNumber(9)),
            "lightweight merged generation facts remain available"
        );
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn focused_candidate_discovery_avoids_unrelated_full_rollups_bd_b915a6() {
        let active = pr_list_json(1, "active", "acme/widgets", false);
        let exact_list = pr_list_json(2, "candidate", "acme/widgets", false)
            .replace(r#""labels":[{"name":"caravan"}]"#, r#""labels":[]"#);
        let unrelated = pr_list_json(99, "unrelated", "acme/widgets", false);
        let exact = exact_list[1..exact_list.len() - 1].to_owned();
        let generation_rows = format!(
            "[{},{},{}]",
            &active[1..active.len() - 1],
            &exact_list[1..exact_list.len() - 1],
            &unrelated[1..unrelated.len() - 1],
        );
        let mut calls = successful_discovery_calls(&active);
        calls[1] = (current_branch_command(), CommandOutput::success("main\n"));
        calls.splice(
            4..5,
            [
                (
                    labeled_pr_command("acme/widgets", "open", "caravan", 1_000, false),
                    CommandOutput::success(active),
                ),
                (
                    pull_request_command(&repository(), "2"),
                    CommandOutput::success(exact),
                ),
                (
                    open_generation_pr_command("acme/widgets", 1_000),
                    CommandOutput::success(generation_rows),
                ),
            ],
        );
        calls.truncate(calls.len().saturating_sub(3));
        let discovery =
            GitHubDiscovery::new(FakeRunner::new(calls)).with_options(DiscoveryOptions {
                focus_pr: Some(PrNumber(2)),
                require_current_pr_resolution: false,
                ..DiscoveryOptions::default()
            });

        let snapshot = discovery.discover().expect("focused discovery");
        let numbers = snapshot
            .pull_requests
            .iter()
            .map(|pull_request| pull_request.number)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            numbers,
            BTreeSet::from([PrNumber(1), PrNumber(2), PrNumber(9)])
        );
        assert!(
            snapshot
                .generation_facts
                .iter()
                .any(|fact| fact.pr == PrNumber(99))
        );
        assert!(
            snapshot
                .pull_requests
                .iter()
                .all(|pull_request| pull_request.number != PrNumber(99))
        );
        discovery.runner.assert_exhausted();
    }

    fn pr_list_json(number: u64, branch: &str, repository: &str, cross_repo: bool) -> String {
        format!(
            r#"[{{"number":{number},"title":"Queue change {number}","state":"OPEN","isDraft":false,"headRefName":"{branch}","headRefOid":"head-{number}","headRepository":{{"name":"widgets","nameWithOwner":"{repository}"}},"headRepositoryOwner":{{"login":"acme"}},"isCrossRepository":{cross_repo},"baseRefName":"main","baseRefOid":"base-{number}","labels":[{{"name":"caravan"}}],"autoMergeRequest":{{"mergeMethod":"SQUASH","enabledAt":"2026-07-17T10:00:00Z","enabledBy":{{"login":"octocat"}}}},"statusCheckRollup":[{{"__typename":"CheckRun","name":"test","context":null,"status":"COMPLETED","conclusion":"SUCCESS","state":null,"workflowName":"CI","detailsUrl":"https://example.test/check","targetUrl":null}}],"createdAt":"2026-07-17T10:00:00Z","mergedAt":null,"url":"https://example.test/pr/{number}","updatedAt":"2026-07-17T11:00:00Z"}}]"#
        )
    }

    fn merged_pr_json() -> &'static str {
        r#"[{"number":9,"title":"Merged queue change","state":"MERGED","isDraft":false,"headRefName":"old-head","headRefOid":"head-9","headRepository":{"name":"widgets","nameWithOwner":"acme/widgets"},"headRepositoryOwner":{"login":"acme"},"isCrossRepository":false,"baseRefName":"main","baseRefOid":"base-9","labels":[{"name":"caravan"}],"autoMergeRequest":null,"createdAt":"2026-07-17T08:00:00Z","mergedAt":"2026-07-17T09:00:00Z","url":"https://example.test/pr/9","updatedAt":"2026-07-17T09:00:00Z"}]"#
    }

    fn closed_pr_json(number: u64, label: &str) -> String {
        let mut rows: Vec<serde_json::Value> = serde_json::from_str(&pr_list_json(
            number,
            &format!("closed-{number}"),
            "acme/widgets",
            false,
        ))
        .unwrap();
        rows[0]["state"] = serde_json::json!("CLOSED");
        rows[0]["mergedAt"] = serde_json::Value::Null;
        rows[0]["labels"] = serde_json::json!([{"name": label}]);
        serde_json::to_string(&rows).unwrap()
    }

    fn pr_object_json(number: u64, branch: &str, repository: &str) -> String {
        let list = pr_list_json(number, branch, repository, false);
        list[1..list.len() - 1].to_owned()
    }

    fn large_open_pr_fixture() -> String {
        let pulls = (101..=130)
            .map(|number| {
                let branch = format!("feature-{number}");
                let mut value: serde_json::Value =
                    serde_json::from_str(&pr_object_json(number, &branch, "acme/widgets")).unwrap();
                let checks = value["statusCheckRollup"].as_array_mut().unwrap();
                checks.push(checks[0].clone());
                if number > 101 {
                    value["baseRefName"] = serde_json::json!(format!("feature-{}", number - 1));
                }
                value
            })
            .collect::<Vec<_>>();
        serde_json::to_string(&pulls).unwrap()
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
            merged_at: None,
            head_oid: CommitOid(format!("head-{number}")),
            base_ref: "main".to_owned(),
            base_oid: CommitOid(format!("base-{number}")),
            labels: std::collections::BTreeSet::from(["caravan".to_owned()]),
            checks: vec![model::CheckSnapshot {
                name: "test".to_owned(),
                state: model::CheckState::Success,
                provider_state: Some("SUCCESS".to_owned()),
                details_url: Some("https://example.test/check".to_owned()),
                ..crate::model::CheckSnapshot::default()
            }],
            auto_merge: AutoMergeState::squash(),
        }
    }

    #[test]
    fn initialization_identity_never_queries_prs_or_moving_refs() {
        let runner = FakeRunner::new(vec![(
            repository_command(None),
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
        assert_eq!(open.auto_merge.actor.as_deref(), Some("octocat"));
        assert_eq!(open.created_at.as_deref(), Some("2026-07-17T10:00:00Z"));
        assert_eq!(
            snapshot.previous_default_oid,
            Some(CommitOid("previous-default-sha".into()))
        );
        assert_eq!(
            snapshot.merge_candidates[0].freshness,
            model::MergeCandidateFreshness::StaleBase,
            "the fixture PR records base-12 while main is default-sha; a superseded base is never fresh"
        );
        assert!(snapshot.merge_candidates[0].stale_base);
        assert_eq!(
            snapshot.merge_candidates[0]
                .compared_base
                .as_ref()
                .map(|base| base.oid.clone()),
            Some(CommitOid("default-sha".into())),
            "receipts expose the live default tip the claim was compared against"
        );
        assert_eq!(
            snapshot.merge_candidates[0].auto_merge.actor.as_deref(),
            Some("octocat")
        );
        assert_eq!(
            snapshot.default_branch_movements[0].ownership,
            model::MovementOwnership::External
        );
        assert_eq!(open.checks[0].state, CheckState::Success);
        assert_eq!(open.checks[0].provider_state.as_deref(), Some("SUCCESS"));
        let merged = snapshot
            .pull_requests
            .iter()
            .find(|pull_request| pull_request.number == PrNumber(9))
            .expect("merged predecessor is present");
        assert_eq!(merged.state, model::PullRequestState::Merged);
        assert!(merged.checks.is_empty());
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn discovery_retains_active_parked_and_terminal_closed_lifecycle_rows() {
        let mut calls = successful_discovery_calls("[]");
        calls[7].1 = CommandOutput::success(closed_pr_json(20, "caravan"));
        calls[8].1 = CommandOutput::success(closed_pr_json(21, PARKED_LABEL));
        calls[9].1 = CommandOutput::success(closed_pr_json(22, CLOSED_LABEL));
        // The checkout branch has no matching PR, so fleet discovery attempts
        // one bounded historical lookup after lifecycle inventory.
        calls.push((
            branch_pr_history_command("acme/widgets", "feature/widget", 100),
            CommandOutput::success("[]"),
        ));
        let discovery = GitHubDiscovery::new(FakeRunner::new(calls));

        let snapshot = discovery
            .discover()
            .expect("closed lifecycle history resolves");

        let observed = snapshot
            .pull_requests
            .iter()
            .filter(|pull_request| pull_request.is_closed_unmerged())
            .map(|pull_request| pull_request.number)
            .collect::<Vec<_>>();
        assert_eq!(observed, [PrNumber(20), PrNumber(21), PrNumber(22)]);
        assert_eq!(
            snapshot.pull_requests.len(),
            4,
            "merged history is preserved too"
        );
        discovery.runner.assert_exhausted();
    }

    fn historical_discovery_calls(
        open_prs: &str,
        branch_history: String,
    ) -> Vec<(CommandSpec, CommandOutput)> {
        let mut calls = successful_discovery_calls(open_prs);
        calls[1] = (
            current_branch_command(),
            CommandOutput::success("old-head\n"),
        );
        calls.push((
            branch_pr_history_command("acme/widgets", "old-head", 100),
            CommandOutput::success(branch_history),
        ));
        calls
    }

    #[test]
    fn merged_current_branch_resolves_direct_active_successor() {
        let open = pr_list_json(10, "next", "acme/widgets", false)
            .replace("\"baseRefName\":\"main\"", "\"baseRefName\":\"old-head\"");
        let mut calls = historical_discovery_calls(&open, merged_pr_json().to_owned());
        calls.extend([
            (
                current_head_oid_command(),
                CommandOutput::success("head-9\n"),
            ),
            (
                historical_head_command("acme/widgets", "old-head"),
                CommandOutput::success(r#"{"object":{"sha":"head-9"}}"#),
            ),
        ]);
        let discovery = GitHubDiscovery::new(FakeRunner::new(calls));

        let snapshot = discovery.discover().expect("historical context resolves");

        assert_eq!(snapshot.current_pr, Some(PrNumber(10)));
        assert!(
            snapshot
                .pull_requests
                .iter()
                .any(|pull| pull.number == PrNumber(9))
        );
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn merged_current_branch_resolves_successor_after_child_retarget() {
        let mut calls = historical_discovery_calls(
            &pr_list_json(10, "next", "acme/widgets", false),
            merged_pr_json().to_owned(),
        );
        calls.extend([
            (current_head_oid_command(), CommandOutput::success("head-9\n")),
            (
                historical_head_command("acme/widgets", "old-head"),
                CommandOutput::success(r#"{"object":{"sha":"head-9"}}"#),
            ),
            (
                base_history_command(&repository(), &[PrNumber(9), PrNumber(10)]),
                CommandOutput::success(r#"{"data":{"repository":{"p9":{"timelineItems":{"nodes":[]}},"p10":{"timelineItems":{"nodes":[{"previousRefName":"old-head","currentRefName":"main"}]}}}}}"#),
            ),
        ]);
        let discovery = GitHubDiscovery::new(FakeRunner::new(calls));

        let snapshot = discovery.discover().expect("retarget history resolves");

        assert_eq!(snapshot.current_pr, Some(PrNumber(10)));
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn merged_middle_branch_follows_history_to_active_rolling_head() {
        let merged_ten = pr_list_json(10, "next", "acme/widgets", false)
            .replace("\"state\":\"OPEN\"", "\"state\":\"MERGED\"")
            .replace("\"mergedAt\":null", "\"mergedAt\":\"2026-07-17T12:00:00Z\"");
        let merged = format!(
            "[{},{}]",
            &merged_pr_json()[1..merged_pr_json().len() - 1],
            &merged_ten[1..merged_ten.len() - 1]
        );
        let mut calls = historical_discovery_calls(
            &pr_list_json(11, "latest", "acme/widgets", false),
            merged_pr_json().to_owned(),
        );
        calls[6].1 = CommandOutput::success(merged);
        calls.extend([
            (
                current_head_oid_command(),
                CommandOutput::success("head-9\n"),
            ),
            (
                historical_head_command("acme/widgets", "old-head"),
                CommandOutput::success(r#"{"object":{"sha":"head-9"}}"#),
            ),
            (
                base_history_command(
                    &repository(),
                    &[PrNumber(9), PrNumber(10), PrNumber(11)],
                ),
                CommandOutput::success(r#"{"data":{"repository":{"p9":{"timelineItems":{"nodes":[]}},"p10":{"timelineItems":{"nodes":[{"previousRefName":"old-head","currentRefName":"main"}]}},"p11":{"timelineItems":{"nodes":[{"previousRefName":"next","currentRefName":"main"}]}}}}}"#),
            ),
        ]);
        let discovery = GitHubDiscovery::new(FakeRunner::new(calls));

        let snapshot = discovery.discover().expect("middle history resolves");

        assert_eq!(snapshot.current_pr, Some(PrNumber(11)));
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn merged_current_branch_without_successor_is_explicit_context() {
        let mut calls = historical_discovery_calls("[]", merged_pr_json().to_owned());
        calls.extend([
            (
                current_head_oid_command(),
                CommandOutput::success("head-9\n"),
            ),
            (
                historical_head_command("acme/widgets", "old-head"),
                CommandOutput::success(r#"{"object":{"sha":"head-9"}}"#),
            ),
            (
                base_history_command(&repository(), &[PrNumber(9)]),
                CommandOutput::success(
                    r#"{"data":{"repository":{"p9":{"timelineItems":{"nodes":[]}}}}}"#,
                ),
            ),
        ]);
        let discovery = GitHubDiscovery::new(FakeRunner::new(calls));

        let snapshot = discovery
            .discover()
            .expect("history is valid without a successor");

        assert_eq!(snapshot.current_pr, None);
        assert_eq!(snapshot.current_branch.as_deref(), Some("old-head"));
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn deleted_historical_branch_fails_closed() {
        let mut calls = historical_discovery_calls("[]", merged_pr_json().to_owned());
        calls.extend([
            (
                current_head_oid_command(),
                CommandOutput::success("head-9\n"),
            ),
            (
                historical_head_command("acme/widgets", "old-head"),
                CommandOutput::failure(1, "HTTP 404"),
            ),
        ]);
        let discovery = GitHubDiscovery::new(FakeRunner::new(calls));

        let error = discovery.discover().expect_err("deleted branch is unsafe");

        assert!(matches!(
            error,
            DiscoveryError::HistoricalCurrentPullRequest {
                reason: "deleted_head",
                ..
            }
        ));
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn closed_unmerged_historical_branch_fails_closed() {
        let closed = merged_pr_json()
            .replace("\"state\":\"MERGED\"", "\"state\":\"CLOSED\"")
            .replace("\"mergedAt\":\"2026-07-17T09:00:00Z\"", "\"mergedAt\":null");
        let calls = historical_discovery_calls("[]", closed);
        let discovery = GitHubDiscovery::new(FakeRunner::new(calls));

        let error = discovery
            .discover()
            .expect_err("unmerged closure is unsafe");

        assert!(matches!(
            error,
            DiscoveryError::HistoricalCurrentPullRequest {
                reason: "closed_unmerged",
                ..
            }
        ));
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn explicit_pr_creation_accepts_one_advanced_unlabelled_merged_branch() {
        let unlabelled =
            merged_pr_json().replace(r#""labels":[{"name":"caravan"}]"#, r#""labels":[]"#);
        let mut calls = historical_discovery_calls("[]", unlabelled);
        calls.extend([
            (
                current_head_oid_command(),
                CommandOutput::success("new-head\n"),
            ),
            (
                historical_head_command("acme/widgets", "old-head"),
                CommandOutput::success(r#"{"object":{"sha":"new-head"}}"#),
            ),
            (
                CommandSpec::new("git").args(["merge-base", "--is-ancestor", "head-9", "new-head"]),
                CommandOutput::success(""),
            ),
        ]);
        let discovery =
            GitHubDiscovery::new(FakeRunner::new(calls)).with_options(DiscoveryOptions {
                allow_unlabelled_historical_pr_creation: true,
                ..DiscoveryOptions::default()
            });
        let snapshot = discovery
            .discover()
            .expect("advanced historical branch is fresh PR ancestry");
        assert_eq!(snapshot.current_pr, None);
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn unique_exact_open_head_precedes_unlabelled_merged_branch_history() {
        let historical =
            merged_pr_json().replace(r#""labels":[{"name":"caravan"}]"#, r#""labels":[]"#);
        let open = pr_list_json(12, "old-head", "acme/widgets", false)
            .replace(r#""labels":[{"name":"caravan"}]"#, r#""labels":[]"#);
        let history = format!(
            "[{},{}]",
            &historical[1..historical.len() - 1],
            &open[1..open.len() - 1]
        );
        let mut calls = historical_discovery_calls("[]", history);
        calls.extend([
            (
                current_head_oid_command(),
                CommandOutput::success("head-12\n"),
            ),
            (
                historical_head_command("acme/widgets", "old-head"),
                CommandOutput::success(r#"{"object":{"sha":"head-12"}}"#),
            ),
        ]);
        let discovery = GitHubDiscovery::new(FakeRunner::new(calls));

        let snapshot = discovery
            .discover()
            .expect("one exact open generation wins over old branch text");

        assert_eq!(snapshot.current_pr, Some(PrNumber(12)));
        let current = snapshot
            .pull_requests
            .iter()
            .find(|pull| pull.number == PrNumber(12))
            .expect("exact open PR retained");
        assert_eq!(current.state, model::PullRequestState::Open);
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn multiple_open_reuses_remain_ambiguous_before_oid_checks() {
        let first = pr_list_json(12, "old-head", "acme/widgets", false);
        let second = pr_list_json(13, "old-head", "acme/widgets", false);
        let history = format!(
            "[{},{}]",
            &first[1..first.len() - 1],
            &second[1..second.len() - 1]
        );
        let calls = historical_discovery_calls("[]", history);
        let discovery = GitHubDiscovery::new(FakeRunner::new(calls));

        let error = discovery
            .discover()
            .expect_err("duplicate open reuse is unsafe");

        assert!(matches!(
            error,
            DiscoveryError::HistoricalCurrentPullRequest {
                reason: "open_branch_reuse_ambiguous",
                ..
            }
        ));
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn unique_open_reuse_refuses_provider_remote_head_mismatch() {
        let open = pr_list_json(12, "old-head", "acme/widgets", false)
            .replace(r#""labels":[{"name":"caravan"}]"#, r#""labels":[]"#);
        let mut calls = historical_discovery_calls("[]", open);
        calls.extend([
            (
                current_head_oid_command(),
                CommandOutput::success("head-12\n"),
            ),
            (
                historical_head_command("acme/widgets", "old-head"),
                CommandOutput::success(r#"{"object":{"sha":"different-head"}}"#),
            ),
        ]);
        let discovery = GitHubDiscovery::new(FakeRunner::new(calls));

        let error = discovery
            .discover()
            .expect_err("remote branch movement invalidates reused head identity");

        assert!(matches!(
            error,
            DiscoveryError::HistoricalCurrentPullRequest {
                reason: "exact_open_head_mismatch",
                ..
            }
        ));
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn exact_open_reuse_refuses_conflicting_historical_membership() {
        let historical = merged_pr_json();
        let open = pr_list_json(12, "old-head", "acme/widgets", false)
            .replace(r#""labels":[{"name":"caravan"}]"#, r#""labels":[]"#);
        let history = format!(
            "[{},{}]",
            &historical[1..historical.len() - 1],
            &open[1..open.len() - 1]
        );
        let mut calls = historical_discovery_calls("[]", history);
        calls.extend([
            (
                current_head_oid_command(),
                CommandOutput::success("head-12\n"),
            ),
            (
                historical_head_command("acme/widgets", "old-head"),
                CommandOutput::success(r#"{"object":{"sha":"head-12"}}"#),
            ),
        ]);
        let discovery = GitHubDiscovery::new(FakeRunner::new(calls));

        let error = discovery
            .discover()
            .expect_err("retained caravan branch history remains authoritative");

        assert!(matches!(
            error,
            DiscoveryError::HistoricalCurrentPullRequest {
                reason: "historical_membership_conflict",
                ..
            }
        ));
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn reused_historical_branch_fails_closed_before_oid_checks() {
        let duplicate = format!(
            "[{},{}]",
            &merged_pr_json()[1..merged_pr_json().len() - 1],
            &merged_pr_json()[1..merged_pr_json().len() - 1]
                .replace("\"number\":9", "\"number\":8")
        );
        let calls = historical_discovery_calls("[]", duplicate);
        let discovery = GitHubDiscovery::new(FakeRunner::new(calls));

        let error = discovery.discover().expect_err("branch reuse is ambiguous");

        assert!(matches!(
            error,
            DiscoveryError::HistoricalCurrentPullRequest {
                reason: "branch_reuse_ambiguous",
                ..
            }
        ));
        discovery.runner.assert_exhausted();
    }

    /// GitHub returns an explicit null rollup for a pull request that never ran
    /// checks, and `#[serde(default)]` covers a MISSING field, not a null one.
    ///
    /// Closed pull requests were not fetched before dissolution detection, so the
    /// shape could not occur. The first closed member with no checks broke
    /// `cara status` ENTIRELY, not just the new capability: no caravans, no
    /// candidates, no problems. Measured live, exactly one of 28 closed labelled
    /// pull requests had a null rollup, and it was the dissolved member the
    /// feature exists to report (bd-54222d).
    #[test]
    fn a_null_check_rollup_deserializes_as_no_checks() {
        let payload = r#"[{"number":2308,"title":"t","body":"","state":"CLOSED","isDraft":false,"headRefName":"b","headRefOid":"abc","headRepository":{"name":"widgets","nameWithOwner":"acme/widgets"},"headRepositoryOwner":{"login":"acme"},"isCrossRepository":false,"baseRefName":"main","baseRefOid":"def","labels":[{"name":"caravan"}],"autoMergeRequest":null,"statusCheckRollup":null,"createdAt":"2026-07-30T10:00:00Z","mergedAt":null,"url":"https://example.test/pr/2308","updatedAt":"2026-07-30T11:00:00Z"}]"#;

        let parsed = serde_json::from_str::<Vec<PullRequestJson>>(payload)
            .expect("a null rollup is not malformed: refusing it costs every read-only surface");

        assert_eq!(parsed.len(), 1);
        assert!(
            parsed[0].status_check_rollup.is_empty(),
            "a null rollup means no checks, not an error"
        );
    }

    /// bd-4e0430: one closed pull request with an unusable shape aborted the
    /// entire discovery phase, so no admission, chain planning, or merge ran at
    /// all. A 28-record response must not be all-or-nothing, and the failure
    /// must name the record rather than only a byte column.
    #[test]
    fn one_unusable_record_is_skipped_and_named_while_the_rest_survive() {
        let good = pr_list_json(2300, "topic", "acme/widgets", false);
        let mut rows: Vec<serde_json::Value> = serde_json::from_str(&good).unwrap();
        let mut broken = rows[0].clone();
        broken["number"] = serde_json::json!(2308);
        broken["headRefOid"] = serde_json::json!(null);
        rows.push(broken);

        let mut records = Vec::new();
        let mut skipped = Vec::new();
        for row in rows {
            let number = row.get("number").and_then(serde_json::Value::as_u64);
            match serde_json::from_value::<PullRequestJson>(row) {
                Ok(record) => records.push(record),
                Err(error) => skipped.push((number, error.to_string())),
            }
        }

        assert_eq!(records.len(), 1, "the usable record still arrives");
        assert_eq!(skipped.len(), 1);
        let described = describe_skipped_record(&skipped[0]);
        assert!(
            described.starts_with("PR #2308:"),
            "the reader is told which record, not just a byte column: {described}"
        );
        assert!(skipped_summary(&skipped).contains("PR #2308"));
    }

    /// bd-cd3be9: generation facts must include recently merged labelled PRs,
    /// or a candidate strictly contained by already-landed work stays a live
    /// ordered admission attempt forever.
    #[test]
    fn generation_facts_include_recently_merged_labelled_prs() {
        let repository = repository();
        let open_json = pr_list_json(12, "feature/widget", "acme/widgets", false);
        let open_pulls: Vec<model::PullRequestSnapshot> =
            serde_json::from_str::<Vec<PullRequestJson>>(&open_json)
                .unwrap()
                .into_iter()
                .map(|pull_request| pull_request.into_snapshot(&repository).unwrap())
                .collect();
        let runner = FakeRunner::new(vec![
            (
                repository_command(None),
                CommandOutput::success(
                    r#"{"nameWithOwner":"acme/widgets","defaultBranchRef":{"name":"main"}}"#,
                ),
            ),
            (current_branch_command(), CommandOutput::failure(1, "")),
            (
                previous_default_command("main"),
                CommandOutput::failure(128, "unknown revision"),
            ),
            (
                default_branch_command("acme/widgets", "main"),
                CommandOutput::success(r#"{"object":{"sha":"default-sha"}}"#),
            ),
            (
                open_pr_command("acme/widgets", 1_000),
                CommandOutput::success(open_json.clone()),
            ),
            (
                merge_candidates_command(&repository, &open_pulls),
                CommandOutput::success(r#"{"data":{"repository":{"defaultBranchRef":null}}}"#),
            ),
            (
                labeled_pr_command("acme/widgets", "merged", "caravan", 100, true),
                CommandOutput::success(merged_pr_json()),
            ),
            (
                labeled_pr_command("acme/widgets", "closed", "caravan", 100, true),
                CommandOutput::success("[]"),
            ),
            (
                labeled_pr_command("acme/widgets", "closed", PARKED_LABEL, 100, true),
                CommandOutput::success("[]"),
            ),
            (
                labeled_pr_command("acme/widgets", "closed", CLOSED_LABEL, 100, true),
                CommandOutput::success("[]"),
            ),
        ]);
        let discovery = GitHubDiscovery::new(runner);

        let snapshot = discovery.discover().expect("discovery succeeds");

        let generation_prs = snapshot
            .generation_facts
            .iter()
            .map(|fact| fact.pr)
            .collect::<Vec<_>>();
        assert!(
            generation_prs.contains(&PrNumber(9)),
            "merged labelled PR must contribute a generation fact: {generation_prs:?}"
        );
        assert!(generation_prs.contains(&PrNumber(12)));
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn large_repository_uses_one_open_rollup_and_minimal_history_query() {
        let open_prs = large_open_pr_fixture();
        let mut calls = successful_discovery_calls(&open_prs);
        calls.push((
            branch_pr_history_command("acme/widgets", "feature/widget", 100),
            CommandOutput::success("[]"),
        ));
        assert_eq!(
            calls.len(),
            11,
            "non-PR branches add only one bounded history lookup beyond lifecycle discovery"
        );
        let merged_command = &calls[6].0;
        let projection = merged_command.args.last().unwrap();
        assert!(!projection.contains("statusCheckRollup"));

        let runner = FakeRunner::new(calls);
        let discovery = GitHubDiscovery::new(runner);
        let snapshot = discovery.discover().expect("large discovery succeeds");

        assert_eq!(snapshot.pull_requests.len(), 31);
        assert_eq!(
            snapshot
                .pull_requests
                .iter()
                .filter(|pull_request| pull_request.state == model::PullRequestState::Open)
                .count(),
            30
        );
        assert!(
            snapshot
                .pull_requests
                .iter()
                .filter(|pr| pr.state == model::PullRequestState::Open)
                .all(|pr| pr.checks.len() == 2)
        );
        assert!(
            snapshot
                .pull_requests
                .iter()
                .find(|pr| pr.state == model::PullRequestState::Merged)
                .unwrap()
                .checks
                .is_empty()
        );
        discovery.runner.assert_exhausted();
    }

    #[test]
    fn candidate_freshness_preserves_simultaneous_base_and_head_staleness() {
        let candidate = model::SyntheticMergeCandidate {
            git_ref: "refs/pull/12/merge".to_owned(),
            oid: CommitOid("merge".to_owned()),
            tree_oid: CommitOid("tree".to_owned()),
            parents: vec![
                CommitOid("old-base".to_owned()),
                CommitOid("old-head".to_owned()),
            ],
        };

        let (freshness, compared_base, stale_base, stale_head, reasons) = classify_candidate(
            Some(&candidate),
            &BranchSnapshot {
                repository: repository(),
                name: "main".to_owned(),
                oid: CommitOid("base".to_owned()),
            },
            &CommitOid("head".to_owned()),
            None,
        );

        assert_eq!(freshness, model::MergeCandidateFreshness::StaleHead);
        assert_eq!(
            compared_base.map(|base| base.oid),
            Some(CommitOid("base".to_owned()))
        );
        assert!(stale_base);
        assert!(stale_head);
        assert_eq!(reasons.len(), 2);
    }

    #[test]
    fn superseded_default_base_is_never_reported_fresh() {
        let repository = repository();
        let recorded_base = BranchSnapshot {
            repository: repository.clone(),
            name: "main".to_owned(),
            oid: CommitOid("1a09ceec".to_owned()),
        };
        let current_default = BranchSnapshot {
            repository,
            name: "main".to_owned(),
            oid: CommitOid("9d6278a1".to_owned()),
        };
        // The provider still serves the PR's original base generation and a
        // synthetic candidate built on it, exactly like live PR #2210.
        let candidate = model::SyntheticMergeCandidate {
            git_ref: "refs/pull/2210/merge".to_owned(),
            oid: CommitOid("merge".to_owned()),
            tree_oid: CommitOid("tree".to_owned()),
            parents: vec![
                CommitOid("1a09ceec".to_owned()),
                CommitOid("head".to_owned()),
            ],
        };

        let (freshness, compared_base, stale_base, stale_head, reasons) = classify_candidate(
            Some(&candidate),
            &recorded_base,
            &CommitOid("head".to_owned()),
            Some(&current_default),
        );

        assert_eq!(freshness, model::MergeCandidateFreshness::StaleBase);
        assert!(stale_base, "a superseded base can never be reported fresh");
        assert!(!stale_head);
        assert_eq!(
            compared_base.map(|base| base.oid),
            Some(CommitOid("9d6278a1".to_owned())),
            "receipts expose the exact generation the claim was compared against"
        );
        assert!(
            reasons
                .iter()
                .any(|reason| reason.contains("superseded by the current main tip"))
        );

        // The same candidate on the live tip stays fresh.
        let fresh_base = BranchSnapshot {
            oid: CommitOid("1a09ceec".to_owned()),
            ..current_default.clone()
        };
        let (freshness, _, stale_base, _, reasons) = classify_candidate(
            Some(&candidate),
            &fresh_base,
            &CommitOid("head".to_owned()),
            Some(&fresh_base),
        );
        assert_eq!(freshness, model::MergeCandidateFreshness::Fresh);
        assert!(!stale_base);
        assert!(reasons.is_empty());
    }

    #[test]
    fn missing_synthetic_candidate_still_reports_a_superseded_base() {
        let repository = repository();
        let recorded_base = BranchSnapshot {
            repository: repository.clone(),
            name: "main".to_owned(),
            oid: CommitOid("old".to_owned()),
        };
        let current_default = BranchSnapshot {
            repository,
            name: "main".to_owned(),
            oid: CommitOid("new".to_owned()),
        };

        let (freshness, compared_base, stale_base, stale_head, reasons) = classify_candidate(
            None,
            &recorded_base,
            &CommitOid("head".to_owned()),
            Some(&current_default),
        );

        assert_eq!(freshness, model::MergeCandidateFreshness::Missing);
        assert!(stale_base);
        assert!(!stale_head);
        assert_eq!(
            compared_base.map(|base| base.oid),
            Some(CommitOid("new".to_owned()))
        );
        assert_eq!(reasons.len(), 2);
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
        // Active-head validation stops immediately after the open inventory,
        // before merge-candidate, merged-history, or three closed-lifecycle reads.
        calls.truncate(5);
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
                repository_command(None),
                CommandOutput::success(
                    r#"{"nameWithOwner":"acme/widgets","defaultBranchRef":{"name":"main"}}"#,
                ),
            ),
            (current_branch_command(), CommandOutput::failure(1, "")),
            (
                previous_default_command("main"),
                CommandOutput::failure(128, "unknown revision"),
            ),
            (
                default_branch_command("acme/widgets", "main"),
                CommandOutput::success(r#"{"object":{"sha":"default-sha"}}"#),
            ),
            (
                open_pr_command("acme/widgets", 1_000),
                CommandOutput::success("[]"),
            ),
            (
                merge_candidates_command(&repository(), &[]),
                CommandOutput::success(r#"{"data":{"repository":{"defaultBranchRef":null}}}"#),
            ),
            (
                labeled_pr_command("acme/widgets", "merged", "caravan", 100, true),
                CommandOutput::success("[]"),
            ),
            (
                labeled_pr_command("acme/widgets", "closed", "caravan", 100, true),
                CommandOutput::success("[]"),
            ),
            (
                labeled_pr_command("acme/widgets", "closed", PARKED_LABEL, 100, true),
                CommandOutput::success("[]"),
            ),
            (
                labeled_pr_command("acme/widgets", "closed", CLOSED_LABEL, 100, true),
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
    fn merge_timestamp_is_exact_mutation_authority() {
        let mut expected = precondition(12);
        expected.state = model::PullRequestState::Closed;
        expected.merged_at = None;
        let mut actual = expected.clone();
        actual.merged_at = Some("2026-08-05T12:00:00Z".to_owned());

        assert_eq!(
            changed_precondition_fields(&expected, &actual),
            ["merged_at"]
        );
        assert!(!expected.mutation_identity_eq(&actual));
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
    fn check_only_churn_does_not_stale_unrelated_provider_mutation() {
        let repository = repository();
        let expected = precondition(12);
        let before = pr_object_json(12, "feature/widget", "acme/widgets").replace(
            r#""status":"COMPLETED","conclusion":"SUCCESS""#,
            r#""status":"IN_PROGRESS","conclusion":null"#,
        );
        let after = before
            .replace(r#""baseRefName":"main""#, r#""baseRefName":"develop""#)
            .replace(r#""baseRefOid":"base-12""#, r#""baseRefOid":"develop-oid""#);
        let runner = FakeRunner::new(vec![
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(before.clone()),
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

        let receipt = adapter.set_base(&repository, &expected, "develop").unwrap();

        assert_eq!(receipt.kind, MutationKind::SetBase);
        assert_eq!(
            receipt.before.unwrap().checks[0].state,
            CheckState::InProgress
        );
        assert_eq!(receipt.after.base.name, "develop");
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn ci_specific_precondition_still_rejects_check_churn() {
        let repository = repository();
        let expected = precondition(12);
        let before = pr_object_json(12, "feature/widget", "acme/widgets").replace(
            r#""status":"COMPLETED","conclusion":"SUCCESS""#,
            r#""status":"IN_PROGRESS","conclusion":null"#,
        );
        let runner = FakeRunner::new(vec![(
            pull_request_command(&repository, "12"),
            CommandOutput::success(before),
        )]);
        let adapter = GitHubMutationAdapter::new(runner);

        let error = adapter
            .verify_precondition_with_checks(&repository, &expected)
            .unwrap_err();

        assert!(matches!(
            error,
            MutationError::StalePrecondition { changed_fields, .. }
                if changed_fields == ["checks"]
        ));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn complete_label_replacement_is_one_exact_provider_write() {
        let repository = repository();
        let mut expected = precondition(12);
        expected.labels.insert("caravan-parked".to_owned());
        let desired = BTreeSet::from(["caravan-closed".to_owned()]);
        let before = pr_object_json(12, "feature/widget", "acme/widgets").replace(
            r#""labels":[{"name":"caravan"}]"#,
            r#""labels":[{"name":"caravan"},{"name":"caravan-parked"}]"#,
        );
        let after = before.replace(
            r#""labels":[{"name":"caravan"},{"name":"caravan-parked"}]"#,
            r#""labels":[{"name":"caravan-closed"}]"#,
        );
        let runner = FakeRunner::new(vec![
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(before),
            ),
            (
                replace_labels_command(&repository, PrNumber(12), &desired),
                CommandOutput::success(r#"[{"name":"caravan-closed"}]"#),
            ),
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(after),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let receipt = adapter
            .replace_labels(&repository, &expected, &desired)
            .expect("complete label replacement succeeds");

        assert_eq!(receipt.kind, MutationKind::SetLabels);
        assert_eq!(receipt.before.unwrap().labels, expected.labels);
        assert_eq!(receipt.after.labels, desired);
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

    fn audit(marker: &str) -> ControlLabelAudit {
        ControlLabelAudit {
            operation: "new".to_owned(),
            marker: marker.to_owned(),
            before_labels: BTreeSet::new(),
            after_labels: BTreeSet::from(["caravan".to_owned()]),
            actor: "cara test actor".to_owned(),
            reason: "eligible admission".to_owned(),
            reason_source: "deterministic policy".to_owned(),
            compatibility_evidence: "clean".to_owned(),
            clean_squash_evidence: "squash enabled".to_owned(),
            admission_priority_basis: "FIFO".to_owned(),
        }
    }

    #[test]
    fn control_label_markers_dedupe_exact_retries_but_distinguish_same_head_transitions() {
        let before = BTreeSet::new();
        let active = BTreeSet::from(["caravan".to_owned()]);
        let prioritized =
            BTreeSet::from(["caravan".to_owned(), "caravan-priority:high".to_owned()]);
        let head = CommitOid("same-head".to_owned());

        let first = control_label_marker("new", PrNumber(12), &head, &before, &active);
        let retry = control_label_marker("new", PrNumber(12), &head, &before, &active);
        let second = control_label_marker("new", PrNumber(12), &head, &active, &prioritized);

        assert_eq!(first, retry);
        assert_ne!(first, second);
    }

    #[test]
    fn control_label_comment_posts_marked_durable_reason() {
        let repository = repository();
        let expected = precondition(12);
        let audit = audit("v1:new:12:head-12");
        let pull = pr_object_json(12, "feature/widget", "acme/widgets");
        let runner = FakeRunner::new(vec![
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pull.clone()),
            ),
            (
                issue_comments_command(&repository, PrNumber(12)),
                CommandOutput::success("[[]]"),
            ),
            (
                comment_pull_request_command(&repository, PrNumber(12), &audit.body()),
                CommandOutput::success("https://example.test/comment/1"),
            ),
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pull),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let receipt = adapter
            .ensure_control_label_comment(&repository, &expected, &audit)
            .unwrap();

        assert_eq!(receipt.kind, MutationKind::Comment);
        assert!(audit.body().contains("Admission priority:** FIFO"));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn control_label_comment_dedupes_from_github_visible_marker() {
        let repository = repository();
        let expected = precondition(12);
        let audit = audit("v1:new:12:head-12");
        let pull = pr_object_json(12, "feature/widget", "acme/widgets");
        let comments = format!(r#"[[{{"body":"{}"}}]]"#, audit.visible_marker());
        let runner = FakeRunner::new(vec![
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pull.clone()),
            ),
            (
                issue_comments_command(&repository, PrNumber(12)),
                CommandOutput::success(comments),
            ),
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pull),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let receipt = adapter
            .ensure_control_label_comment(&repository, &expected, &audit)
            .unwrap();

        assert!(
            receipt
                .provider_output
                .unwrap()
                .starts_with("existing GitHub comment")
        );
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn generic_marked_comment_dedupes_exact_skip_receipt() {
        let repository = repository();
        let expected = precondition(12);
        let pull = pr_object_json(12, "feature/widget", "acme/widgets");
        let marker = "<!-- caravan-auto-join-skip-receipt:abcd -->";
        let comments = serde_json::to_string(&vec![vec![serde_json::json!({
            "body": format!("{marker}\nexisting receipt"),
        })]])
        .unwrap();
        let runner = FakeRunner::new(vec![
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pull.clone()),
            ),
            (
                issue_comments_command(&repository, PrNumber(12)),
                CommandOutput::success(comments),
            ),
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pull),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let receipt = adapter
            .ensure_marked_comment(&repository, &expected, marker, "replacement")
            .unwrap();

        assert!(
            receipt
                .provider_output
                .unwrap()
                .starts_with("existing GitHub comment")
        );
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn control_label_comment_dedupes_partial_retry_with_changed_local_before_snapshot() {
        let repository = repository();
        let expected = precondition(12);
        let mut first = audit("");
        first.marker = control_label_marker(
            "new",
            PrNumber(12),
            &expected.head_oid,
            &BTreeSet::new(),
            &first.after_labels,
        );
        let mut retry = first.clone();
        retry.before_labels = retry.after_labels.clone();
        retry.marker = control_label_marker(
            "new",
            PrNumber(12),
            &expected.head_oid,
            &retry.before_labels,
            &retry.after_labels,
        );
        assert_ne!(first.marker, retry.marker);
        let comments = serde_json::to_string(&vec![vec![serde_json::json!({
            "body": first.body(),
        })]])
        .unwrap();
        let pull = pr_object_json(12, "feature/widget", "acme/widgets");
        let runner = FakeRunner::new(vec![
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pull.clone()),
            ),
            (
                issue_comments_command(&repository, PrNumber(12)),
                CommandOutput::success(comments),
            ),
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pull),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let receipt = adapter
            .ensure_control_label_comment(&repository, &expected, &retry)
            .unwrap();

        assert!(
            receipt
                .provider_output
                .unwrap()
                .starts_with("existing GitHub comment")
        );
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn reviewed_force_state_uses_one_graphql_mutation_and_refetches_complete_postcondition() {
        let repository = repository();
        let mutation = force_transaction_command("PR_node", Some("LABEL_node"), true, true);
        let rendered = mutation.display();
        assert!(
            rendered.find("forceAutoMerge").unwrap() < rendered.find("forceLabel").unwrap(),
            "safe serial prefix enables holding auto-merge before arming force intent"
        );
        let mut expected = precondition(12);
        expected.auto_merge = AutoMergeState::disabled();
        let mut before: serde_json::Value =
            serde_json::from_str(&pr_object_json(12, "feature/widget", "acme/widgets")).unwrap();
        before["autoMergeRequest"] = serde_json::Value::Null;
        let mut after = before.clone();
        after["labels"] = serde_json::json!([{"name": "caravan"}, {"name": "caravan-force"}]);
        after["autoMergeRequest"] = serde_json::json!({
            "mergeMethod": "SQUASH",
            "enabledAt": "2026-07-17T10:00:00Z",
            "enabledBy": {"login": "octocat"}
        });
        let ids = serde_json::json!({"data": {"repository": {
            "pullRequest": {"id": "PR_node"},
            "label": {"id": "LABEL_node"}
        }}})
        .to_string();
        let runner = FakeRunner::new(vec![
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(before.to_string()),
            ),
            (
                force_transaction_ids_command(&repository, PrNumber(12), "caravan-force"),
                CommandOutput::success(ids),
            ),
            (
                force_transaction_command("PR_node", Some("LABEL_node"), true, true),
                CommandOutput::success(r#"{"data":{"forceLabel":{},"forceAutoMerge":{}}}"#),
            ),
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(after.to_string()),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let (receipt, performed) = adapter
            .atomic_label_and_squash_auto_merge(&repository, &expected, "caravan-force", true, true)
            .unwrap();

        assert!(performed);
        assert_eq!(receipt.kind, MutationKind::ForceIntentTransaction);
        assert!(receipt.after.has_label("caravan-force"));
        assert_eq!(receipt.after.auto_merge, AutoMergeState::squash());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn reviewed_force_partial_graphql_result_is_explicit_and_resumable() {
        let repository = repository();
        let mut expected = precondition(12);
        expected.auto_merge = AutoMergeState::disabled();
        let mut before: serde_json::Value =
            serde_json::from_str(&pr_object_json(12, "feature/widget", "acme/widgets")).unwrap();
        before["autoMergeRequest"] = serde_json::Value::Null;
        let mut partial = before.clone();
        partial["autoMergeRequest"] = serde_json::json!({
            "mergeMethod": "SQUASH",
            "enabledAt": "2026-07-17T10:00:00Z",
            "enabledBy": {"login": "octocat"}
        });
        let ids = serde_json::json!({"data": {"repository": {
            "pullRequest": {"id": "PR_node"},
            "label": {"id": "LABEL_node"}
        }}})
        .to_string();
        let runner = FakeRunner::new(vec![
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(before.to_string()),
            ),
            (
                force_transaction_ids_command(&repository, PrNumber(12), "caravan-force"),
                CommandOutput::success(ids),
            ),
            (
                force_transaction_command("PR_node", Some("LABEL_node"), true, true),
                CommandOutput::failure(1, "second mutation field failed"),
            ),
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(partial.to_string()),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let error = adapter
            .atomic_label_and_squash_auto_merge(&repository, &expected, "caravan-force", true, true)
            .unwrap_err();

        assert!(matches!(
            error,
            MutationError::AtomicTransactionIncomplete {
                desired_label_present: true,
                desired_squash_auto_merge: true,
                ref after,
                ..
            } if !after.has_label("caravan-force") && after.auto_merge.enabled
        ));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn generation_facts_parse_provider_body_and_compare_exact_source_commits() {
        let repository = repository();
        let source = "a".repeat(40);
        let generation = format!("agent/example-pr-g{source}");
        let mut pull: serde_json::Value =
            serde_json::from_str(&pr_object_json(12, &generation, "acme/widgets")).unwrap();
        pull["headRefOid"] = serde_json::json!("b".repeat(40));
        pull["body"] = serde_json::json!(format!(
            "Beads: bd-c7440c\n\nCacophony-Generation: `{generation}`\nCacophony-Agent: `agent-a`\nCacophony-Head: `{source}`\nCacophony-Stack-Base: `main`\nCacophony-Stack-State: `root`"
        ));
        let head = CommitOid("c".repeat(40));
        let runner = FakeRunner::new(vec![
            (
                open_generation_pr_command(&repository.slug(), 1_000),
                CommandOutput::success(serde_json::json!([pull]).to_string()),
            ),
            (
                compare_commits_command(&repository, &CommitOid(source.clone()), &head),
                CommandOutput::success(r#"{"status":"ahead"}"#),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let facts = adapter.open_generation_facts(&repository).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].provider_head, CommitOid("b".repeat(40)));
        assert_eq!(
            facts[0].provenance.as_ref().unwrap().source_head,
            CommitOid(source.clone())
        );
        assert_eq!(
            adapter
                .compare_commits(&repository, &CommitOid(source), &head)
                .unwrap(),
            crate::generation::CommitRelation::Ahead
        );
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
    fn branch_required_contexts_merge_legacy_and_typed_declarations() {
        let repository = repository();
        let runner = FakeRunner::new(vec![
            (
                branch_settings_command(&repository, "main"),
                CommandOutput::success(r#"{"protected":true}"#),
            ),
            (
                branch_protection_command(&repository, "main"),
                CommandOutput::success(
                    r#"{"required_status_checks":{"strict":false,"contexts":["Check & Lint"],"checks":[{"context":"Fast Tests (unit)"},{"context":"Check & Lint"}]}}"#,
                ),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let read = adapter
            .branch_required_contexts(&repository, "main")
            .expect("protection read succeeds");

        assert!(read.protected);
        assert!(read.complete);
        assert_eq!(
            read.contexts,
            vec!["Check & Lint".to_owned(), "Fast Tests (unit)".to_owned()]
        );
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn an_unprotected_branch_is_a_complete_empty_requirement() {
        let repository = repository();
        let runner = FakeRunner::new(vec![(
            branch_settings_command(&repository, "caravan/2210"),
            CommandOutput::success(r#"{"protected":false}"#),
        )]);
        let adapter = GitHubMutationAdapter::new(runner);

        let read = adapter
            .branch_required_contexts(&repository, "caravan/2210")
            .expect("an unprotected branch is readable");

        assert!(!read.protected);
        assert!(read.complete);
        assert!(read.contexts.is_empty());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn an_unreadable_protection_endpoint_is_partial_not_empty() {
        let repository = repository();
        let runner = FakeRunner::new(vec![
            (
                branch_settings_command(&repository, "main"),
                CommandOutput::success(r#"{"protected":true}"#),
            ),
            (
                branch_protection_command(&repository, "main"),
                CommandOutput {
                    code: Some(1),
                    stdout: String::new(),
                    stderr: "HTTP 403: Resource not accessible by integration".to_owned(),
                },
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let read = adapter
            .branch_required_contexts(&repository, "main")
            .expect("a refused read degrades instead of failing the tick");

        assert!(read.protected);
        assert!(
            !read.complete,
            "a permission error must never look like an absence of requirements"
        );
        assert!(read.contexts.is_empty());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn head_run_lineage_preserves_exact_suite_run_and_commit_facts() {
        let repository = repository();
        let expected = precondition(12);
        let runner = FakeRunner::new(vec![
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pr_object_json(12, "feature/widget", "acme/widgets")),
            ),
            (
                check_suites_command(&repository, "head-12"),
                CommandOutput::success(
                    r#"{"total_count":1,"check_suites":[{"id":4242,"head_sha":"head-12","status":"completed","conclusion":"cancelled","app":{"slug":"github-actions"}}]}"#,
                ),
            ),
            (
                head_runs_command(&repository, "head-12"),
                CommandOutput::success(
                    r#"{"total_count":1,"workflow_runs":[{"id":30222268397,"check_suite_id":4242,"name":"CI","head_sha":"head-12","status":"completed","conclusion":null,"event":"pull_request"}]}"#,
                ),
            ),
            (
                commit_command(&repository, "head-12"),
                CommandOutput::success(
                    r#"{"commit":{"committer":{"date":"2026-07-26T22:03:00Z"}}}"#,
                ),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let lineage = adapter
            .head_run_lineage(&repository, &expected)
            .expect("lineage read succeeds");

        assert!(lineage.complete);
        assert_eq!(lineage.head_sha, "head-12");
        assert_eq!(lineage.check_suites.len(), 1);
        assert_eq!(lineage.check_suites[0].id, 4242);
        assert_eq!(lineage.check_suites[0].conclusion, "cancelled");
        assert!(lineage.check_suites[0].rerequestable);
        assert_eq!(lineage.workflow_runs[0].run_id, 30_222_268_397);
        assert_eq!(
            lineage.workflow_runs[0].conclusion, "",
            "a null conclusion must stay empty rather than being guessed"
        );
        assert_eq!(
            lineage.head_committed_at.as_deref(),
            Some("2026-07-26T22:03:00Z")
        );
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn a_refused_lineage_sub_read_marks_the_whole_read_partial() {
        let repository = repository();
        let expected = precondition(12);
        let runner = FakeRunner::new(vec![
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pr_object_json(12, "feature/widget", "acme/widgets")),
            ),
            (
                check_suites_command(&repository, "head-12"),
                CommandOutput {
                    code: Some(1),
                    stdout: String::new(),
                    stderr: "HTTP 502".to_owned(),
                },
            ),
            (
                head_runs_command(&repository, "head-12"),
                CommandOutput::success(r#"{"total_count":0,"workflow_runs":[]}"#),
            ),
            (
                commit_command(&repository, "head-12"),
                CommandOutput::success(
                    r#"{"commit":{"committer":{"date":"2026-07-26T22:03:00Z"}}}"#,
                ),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let lineage = adapter
            .head_run_lineage(&repository, &expected)
            .expect("a partial read is reported, not fatal");

        assert!(!lineage.complete);
        assert!(lineage.check_suites.is_empty());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn rerequest_check_suite_refuses_a_superseded_generation() {
        let repository = repository();
        let expected = precondition(12);
        let runner = FakeRunner::new(vec![
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pr_object_json(12, "feature/widget", "acme/widgets")),
            ),
            (
                check_suite_command(&repository, 4242),
                CommandOutput::success(
                    r#"{"id":4242,"head_sha":"pre-rebase-generation","status":"completed","conclusion":"cancelled","app":{"slug":"github-actions"}}"#,
                ),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let error = adapter
            .rerequest_check_suite(&repository, &expected, 4242)
            .expect_err("a foreign generation must never be retriggered");

        assert!(matches!(
            error,
            MutationError::CheckSuiteHeadMismatch {
                check_suite_id: 4242,
                ref expected_head,
                ref actual_head,
            } if expected_head == "head-12" && actual_head == "pre-rebase-generation"
        ));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn rerequest_check_suite_posts_exactly_once_against_the_unchanged_head() {
        let repository = repository();
        let expected = precondition(12);
        let runner = FakeRunner::new(vec![
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pr_object_json(12, "feature/widget", "acme/widgets")),
            ),
            (
                check_suite_command(&repository, 4242),
                CommandOutput::success(
                    r#"{"id":4242,"head_sha":"head-12","status":"completed","conclusion":"cancelled","app":{"slug":"github-actions"}}"#,
                ),
            ),
            (
                rerequest_check_suite_command(&repository, 4242),
                CommandOutput::success("{}"),
            ),
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pr_object_json(12, "feature/widget", "acme/widgets")),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let receipt = adapter
            .rerequest_check_suite(&repository, &expected, 4242)
            .expect("the unchanged head is retriggerable");

        assert_eq!(receipt.kind, MutationKind::RequestCheckSuite);
        assert_eq!(receipt.after.head.oid, CommitOid("head-12".to_owned()));
        assert_eq!(
            receipt.before.expect("before facts").head.oid,
            receipt.after.head.oid,
            "recovery must never change the head it is recovering"
        );
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn required_run_command_builders_stay_read_scoped_and_exact() {
        let repository = repository();
        assert_eq!(
            check_suites_command(&repository, "head-12"),
            CommandSpec::new("gh").args([
                "api",
                "repos/acme/widgets/commits/head-12/check-suites?per_page=100",
            ])
        );
        assert_eq!(
            head_runs_command(&repository, "head-12"),
            CommandSpec::new("gh").args([
                "api",
                "repos/acme/widgets/actions/runs?head_sha=head-12&per_page=100",
            ])
        );
        assert_eq!(
            rerequest_check_suite_command(&repository, 4242),
            CommandSpec::new("gh")
                .args([
                    "api",
                    "--method",
                    "POST",
                    "repos/acme/widgets/check-suites/4242/rerequest",
                ])
                .provider_write()
        );
    }

    #[test]
    fn live_check_suite_payloads_decode_foreign_apps_and_null_conclusions() {
        // Faithful shape of `repos/harryaskham/cacophony/commits/79abc31d…/
        // check-suites`: unrelated keys, three foreign-app suites queued, one
        // cancelled Actions suite, one in-progress Actions suite.
        let repository = repository();
        let expected = precondition(12);
        let runner = FakeRunner::new(vec![
            (
                pull_request_command(&repository, "12"),
                CommandOutput::success(pr_object_json(12, "feature/widget", "acme/widgets")),
            ),
            (
                check_suites_command(&repository, "head-12"),
                CommandOutput::success(
                    r#"{"total_count":5,"check_suites":[
                        {"id":81895334808,"node_id":"x","head_branch":"agent/x","head_sha":"head-12","status":"queued","conclusion":null,"rerequestable":true,"runs_rerequestable":false,"app":{"id":1210556,"slug":"cursor","name":"Cursor"}},
                        {"id":81895334871,"head_sha":"head-12","status":"queued","conclusion":null,"rerequestable":true,"app":{"slug":"claude"}},
                        {"id":81895334923,"head_sha":"head-12","status":"queued","conclusion":null,"rerequestable":false,"app":{"slug":"aviator-app"}},
                        {"id":81895339455,"head_sha":"head-12","status":"completed","conclusion":"cancelled","rerequestable":true,"app":{"slug":"github-actions"}},
                        {"id":81895922485,"head_sha":"head-12","status":"in_progress","conclusion":null,"rerequestable":true,"app":{"slug":"github-actions"}}
                    ]}"#,
                ),
            ),
            (
                head_runs_command(&repository, "head-12"),
                CommandOutput::success(
                    r#"{"total_count":2,"workflow_runs":[
                        {"id":30222268397,"check_suite_id":81895922485,"name":"CI","head_sha":"head-12","status":"in_progress","conclusion":null,"event":"pull_request"},
                        {"id":30222037735,"check_suite_id":81895339455,"name":"CI","head_sha":"head-12","status":"completed","conclusion":"cancelled","event":"pull_request"}
                    ]}"#,
                ),
            ),
            (
                commit_command(&repository, "head-12"),
                CommandOutput::success(
                    r#"{"sha":"head-12","commit":{"author":{"date":"2026-07-26T15:27:43Z"},"committer":{"name":"Caravan","date":"2026-07-26T15:27:43Z"}}}"#,
                ),
            ),
        ]);
        let adapter = GitHubMutationAdapter::new(runner);

        let lineage = adapter
            .head_run_lineage(&repository, &expected)
            .expect("the live payload shape decodes");

        assert!(lineage.complete);
        assert_eq!(lineage.check_suites.len(), 5);
        assert_eq!(lineage.workflow_runs.len(), 2);
        assert_eq!(
            lineage.head_committed_at.as_deref(),
            Some("2026-07-26T15:27:43Z"),
            "a rebase can preserve the original commit date"
        );
        // The provider's own `rerequestable` flag is authoritative and the
        // lowest rerequestable suite on the exact head is selected.
        assert_eq!(
            crate::required_runs::rerequestable_suite(Some(&lineage), "head-12"),
            Some(81_895_334_808)
        );
        assert!(
            !lineage.check_suites[2].rerequestable,
            "a suite the provider refuses to rerequest must never be selected"
        );
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
    fn every_provider_mutation_builder_is_explicitly_marked() {
        let repository = repository();
        let commands = vec![
            create_pull_request_command(
                &repository,
                &CreatePullRequestInput {
                    head: "feature".to_owned(),
                    base: "main".to_owned(),
                    draft: false,
                },
            ),
            edit_pull_request_command(&repository, PrNumber(12), "--add-label", "caravan"),
            auto_merge_command(&repository, PrNumber(12), false),
            force_transaction_command("pull-id", Some("label-id"), true, true),
            rerun_failed_command(&repository, 42),
            rerequest_check_suite_command(&repository, 42),
            create_repository_label_command(&repository, "caravan", "ffffff", "managed"),
            comment_pull_request_command(&repository, PrNumber(12), "receipt"),
            admin_squash_merge_command(&repository, PrNumber(12)),
            squash_merge_command(&repository, PrNumber(12), &CommitOid("head".to_owned())),
        ];
        for command in commands {
            assert_eq!(
                command.intent(),
                crate::command::CommandIntent::ProviderWrite
            );
            assert_eq!(
                command.inferred_write_intent(),
                Some(crate::command::CommandIntent::ProviderWrite),
                "{}",
                command.display()
            );
        }
        assert_eq!(
            force_transaction_ids_command(&repository, PrNumber(12), "caravan").intent(),
            crate::command::CommandIntent::Read
        );
    }

    #[test]
    fn mutation_command_builders_keep_user_values_as_separate_arguments() {
        let repository = repository();
        assert_eq!(
            edit_pull_request_command(&repository, PrNumber(12), "--add-label", "caravan queue"),
            CommandSpec::new("gh")
                .args([
                    "pr",
                    "edit",
                    "12",
                    "--repo",
                    "acme/widgets",
                    "--add-label",
                    "caravan queue",
                ])
                .provider_write()
        );
        assert_eq!(
            edit_pull_request_command(
                &repository,
                PrNumber(12),
                "--remove-label",
                "caravan-evicted"
            ),
            CommandSpec::new("gh")
                .args([
                    "pr",
                    "edit",
                    "12",
                    "--repo",
                    "acme/widgets",
                    "--remove-label",
                    "caravan-evicted",
                ])
                .provider_write()
        );
        assert_eq!(
            auto_merge_command(&repository, PrNumber(12), false),
            CommandSpec::new("gh")
                .args([
                    "pr",
                    "merge",
                    "12",
                    "--repo",
                    "acme/widgets",
                    "--auto",
                    "--squash",
                ])
                .provider_write()
        );
        assert_eq!(
            auto_merge_command(&repository, PrNumber(12), true),
            CommandSpec::new("gh")
                .args([
                    "pr",
                    "merge",
                    "12",
                    "--repo",
                    "acme/widgets",
                    "--disable-auto",
                ])
                .provider_write()
        );
        assert_eq!(
            admin_squash_merge_command(&repository, PrNumber(12)),
            CommandSpec::new("gh")
                .args([
                    "pr",
                    "merge",
                    "12",
                    "--repo",
                    "acme/widgets",
                    "--admin",
                    "--squash",
                ])
                .provider_write()
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
    fn rate_limit_probe_preserves_core_and_graphql_resources() {
        let runner = FakeRunner::new(vec![(
            rate_limit_command(),
            CommandOutput::success(
                r#"{"resources":{"core":{"limit":5000,"used":5003,"remaining":0,"reset":1784584831},"graphql":{"limit":5000,"used":1,"remaining":4999,"reset":1784584800},"search":{"limit":30,"used":0,"remaining":30,"reset":1784584800}}}"#,
            ),
        )]);
        let adapter = GitHubMutationAdapter::new(runner);

        let limits = adapter.rate_limits().unwrap();

        assert_eq!(limits.core.limit, 5_000);
        assert_eq!(limits.core.used, 5_003);
        assert_eq!(limits.core.remaining, 0);
        assert_eq!(limits.core.reset_unix_secs, 1_784_584_831);
        assert_eq!(limits.graphql.unwrap().remaining, 4_999);
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn native_stack_inventory_decodes_one_bounded_read_only_page() {
        let command = native_stack_list_command(&repository());
        let runner = FakeRunner::new(vec![(
            command,
            CommandOutput::success(
                r#"[{"id":9876543,"number":42,"node_id":"S_stack","base":{"ref":"main"},"open":true,"created_at":"2026-07-31T10:00:00Z","pull_requests":[{"number":101,"state":"open","draft":false,"merged_at":null,"head":{"ref":"root","sha":"aaa111"}},{"number":102,"state":"open","draft":false,"merged_at":null,"head":{"ref":"child","sha":"bbb222"}}]}]"#,
            ),
        )]);
        let adapter = GitHubMutationAdapter::new(runner);

        let inventory = adapter
            .native_stack_inventory(&repository())
            .expect("documented Stack resource decodes");

        assert!(!inventory.truncated);
        assert_eq!(inventory.stacks[0].number, 42);
        assert_eq!(inventory.stacks[0].pull_requests[1].head.sha.0, "bbb222");
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn native_stack_inventory_distinguishes_unavailable_from_unknown() {
        let command = native_stack_list_command(&repository());
        let unavailable = GitHubMutationAdapter::new(FakeRunner::new(vec![(
            command.clone(),
            CommandOutput::failure(1, "gh: Not Found (HTTP 404)"),
        )]));
        assert!(matches!(
            unavailable.native_stack_inventory(&repository()),
            Err(GitHubStackReadError::Unavailable { .. })
        ));

        let unknown = GitHubMutationAdapter::new(FakeRunner::new(vec![(
            command,
            CommandOutput::failure(1, "gh: API rate limit exceeded (HTTP 403)"),
        )]));
        assert!(matches!(
            unknown.native_stack_inventory(&repository()),
            Err(GitHubStackReadError::CommandFailed { .. })
        ));
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

#[cfg(test)]
mod configured_repository_tests {
    use super::*;

    /// bd-ff639b: a managed agent checkout points at a local daemon mirror, so
    /// `gh` can infer nothing. The repository must be named positionally —
    /// `-R`/`--repo` are rejected by `gh repo view` and `GH_REPO` is ignored.
    #[test]
    fn configured_repository_is_passed_positionally_before_json_selection() {
        let inferred = repository_command(None);
        assert_eq!(
            inferred.args,
            vec![
                "repo".to_owned(),
                "view".to_owned(),
                "--json".to_owned(),
                "nameWithOwner,defaultBranchRef".to_owned(),
            ]
        );

        let configured = repository_command(Some("harryaskham/cacophony"));
        assert_eq!(
            configured.args,
            vec![
                "repo".to_owned(),
                "view".to_owned(),
                "harryaskham/cacophony".to_owned(),
                "--json".to_owned(),
                "nameWithOwner,defaultBranchRef".to_owned(),
            ],
            "the slug must precede --json; gh takes the repository positionally"
        );
        assert!(
            !configured
                .args
                .iter()
                .any(|arg| arg == "-R" || arg == "--repo"),
            "gh 2.96 rejects -R/--repo on `repo view`"
        );
    }
}
