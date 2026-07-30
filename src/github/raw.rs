//! Raw GitHub JSON transport types and their conversions into Caravan model
//! values (bd-fcd5c9).
//!
//! These types exist only to name the provider's wire shape so the adapter can
//! decode it exactly once, at the boundary. They are deliberately separate from
//! discovery and mutation logic: a provider response shape changing is a
//! transport concern, not a semantics concern, and keeping the seam explicit
//! means a wire-format change never reads like a behaviour change.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;

use super::{DiscoveryError, WorkflowRunSnapshot, normalize_check_state, repository_id};
use crate::model;
use crate::model::{
    AutoMergeState, BranchSnapshot, CommitOid, MergeMethod, PrNumber, RepositoryId,
};

#[derive(Debug, Deserialize)]
pub(super) struct BaseHistoryResponse {
    pub(super) data: BaseHistoryData,
}

#[derive(Debug, Deserialize)]
pub(super) struct BaseHistoryData {
    pub(super) repository: BaseHistoryRepository,
}

#[derive(Debug, Deserialize)]
pub(super) struct BaseHistoryRepository {
    #[serde(flatten)]
    pub(super) pulls: BTreeMap<String, BaseHistoryPull>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BaseHistoryPull {
    pub(super) timeline_items: BaseHistoryTimeline,
}

#[derive(Debug, Deserialize)]
pub(super) struct BaseHistoryTimeline {
    #[serde(default)]
    pub(super) nodes: Vec<BaseHistoryNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BaseHistoryNode {
    pub(super) previous_ref_name: Option<String>,
    #[allow(dead_code)]
    pub(super) current_ref_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RepositoryJson {
    pub(super) name_with_owner: String,
    pub(super) default_branch_ref: Option<NamedRefJson>,
}

#[derive(Debug, Deserialize)]
pub(super) struct NamedRefJson {
    pub(super) name: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct GitRefJson {
    pub(super) object: GitObjectJson,
}

#[derive(Debug, Deserialize)]
pub(super) struct GitObjectJson {
    pub(super) sha: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct MergeCandidatesResponse {
    pub(super) data: MergeCandidatesData,
}

#[derive(Debug, Deserialize)]
pub(super) struct MergeCandidatesData {
    pub(super) repository: MergeCandidatesRepository,
}

#[derive(Debug, Deserialize)]
pub(super) struct MergeCandidatesRepository {
    #[serde(rename = "defaultBranchRef")]
    pub(super) default_branch_ref: Option<GraphDefaultBranchRefJson>,
    #[serde(flatten)]
    pub(super) candidates: BTreeMap<String, Option<GraphCommitJson>>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ForceTransactionIdsResponse {
    pub(super) data: ForceTransactionIdsData,
}

#[derive(Debug, Deserialize)]
pub(super) struct ForceTransactionIdsData {
    pub(super) repository: ForceTransactionIdsRepository,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ForceTransactionIdsRepository {
    pub(super) pull_request: Option<GraphNodeId>,
    pub(super) label: Option<GraphNodeId>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphNodeId {
    pub(super) id: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphCommitJson {
    pub(super) oid: String,
    pub(super) tree: GraphOidJson,
    pub(super) parents: GraphParentsJson,
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphParentsJson {
    pub(super) nodes: Vec<GraphOidJson>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphOidJson {
    pub(super) oid: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphDefaultBranchRefJson {
    pub(super) target: GraphHistoryTargetJson,
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphHistoryTargetJson {
    pub(super) history: GraphHistoryJson,
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphHistoryJson {
    pub(super) nodes: Vec<GraphHistoryCommitJson>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GraphHistoryCommitJson {
    pub(super) oid: String,
    pub(super) committed_date: String,
    pub(super) author: Option<GraphAuthorJson>,
    pub(super) associated_pull_requests: GraphPullRequestsJson,
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphAuthorJson {
    pub(super) name: Option<String>,
    pub(super) user: Option<GraphUserJson>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphUserJson {
    pub(super) login: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphPullRequestsJson {
    pub(super) nodes: Vec<GraphPullRequestJson>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphPullRequestJson {
    pub(super) number: u64,
    pub(super) labels: GraphLabelsJson,
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphLabelsJson {
    pub(super) nodes: Vec<LabelJson>,
}

#[derive(Debug, Deserialize)]
pub(super) struct BranchSettingsJson {
    pub(super) protected: bool,
}

/// One bounded page is authoritative: a commit with a hundred check suites is
/// self-evidently not missing required runs, and a single request keeps the
/// pathological path cheap.
#[derive(Debug, Deserialize)]
pub(super) struct CheckSuiteListJson {
    #[serde(default)]
    pub(super) check_suites: Vec<CheckSuiteJson>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CheckSuiteJson {
    pub(super) id: u64,
    pub(super) head_sha: String,
    #[serde(default)]
    pub(super) status: Option<String>,
    #[serde(default)]
    pub(super) conclusion: Option<String>,
    #[serde(default)]
    pub(super) app: Option<CheckSuiteAppJson>,
    #[serde(default)]
    pub(super) rerequestable: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CheckSuiteAppJson {
    #[serde(default)]
    pub(super) slug: String,
}

impl From<CheckSuiteJson> for crate::required_runs::CheckSuiteLineage {
    fn from(suite: CheckSuiteJson) -> Self {
        let app_slug = suite.app.map(|app| app.slug).unwrap_or_default();
        // A suite with no owning app exposes no safe rerequest primitive, so
        // policy must fall back to a typed operator problem instead of guessing.
        let rerequestable = suite.rerequestable.unwrap_or(!app_slug.is_empty());
        Self {
            id: suite.id,
            head_sha: suite.head_sha,
            status: suite.status.unwrap_or_default(),
            conclusion: suite.conclusion.unwrap_or_default(),
            app_slug,
            rerequestable,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct WorkflowRunListJson {
    #[serde(default)]
    pub(super) workflow_runs: Vec<HeadWorkflowRunJson>,
}

#[derive(Debug, Deserialize)]
pub(super) struct HeadWorkflowRunJson {
    pub(super) id: u64,
    #[serde(default)]
    pub(super) check_suite_id: u64,
    #[serde(default)]
    pub(super) name: String,
    pub(super) head_sha: String,
    #[serde(default)]
    pub(super) status: Option<String>,
    #[serde(default)]
    pub(super) conclusion: Option<String>,
    #[serde(default)]
    pub(super) event: String,
}

impl From<HeadWorkflowRunJson> for crate::required_runs::WorkflowRunLineage {
    fn from(run: HeadWorkflowRunJson) -> Self {
        Self {
            run_id: run.id,
            check_suite_id: run.check_suite_id,
            workflow_name: run.name,
            head_sha: run.head_sha,
            status: run.status.unwrap_or_default(),
            conclusion: run.conclusion.unwrap_or_default(),
            event: run.event,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct CommitDetailJson {
    pub(super) commit: CommitMetadataJson,
}

#[derive(Debug, Deserialize)]
pub(super) struct CommitMetadataJson {
    pub(super) committer: CommitActorJson,
}

#[derive(Debug, Deserialize)]
pub(super) struct CommitActorJson {
    pub(super) date: String,
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
pub(super) struct BranchProtectionJson {
    pub(super) required_status_checks: Option<RequiredStatusChecksJson>,
    pub(super) required_pull_request_reviews: Option<RequiredPullRequestReviewsJson>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RequiredStatusChecksJson {
    #[serde(default)]
    pub(super) contexts: Vec<String>,
    #[serde(default)]
    pub(super) checks: Vec<RequiredCheckJson>,
    pub(super) strict: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct RequiredCheckJson {
    pub(super) context: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct RequiredPullRequestReviewsJson {
    pub(super) required_approving_review_count: u64,
}

#[derive(Debug, Deserialize)]
pub(super) struct RepositorySettingsJson {
    pub(super) allow_auto_merge: bool,
    pub(super) allow_squash_merge: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MergeCommitJson {
    #[serde(default)]
    pub(super) merge_commit: Option<MergeCommitOidJson>,
}

#[derive(Debug, Deserialize)]
pub(super) struct MergeCommitOidJson {
    pub(super) oid: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RepositoryPermissionJson {
    pub(super) viewer_permission: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct RateLimitResponseJson {
    pub(super) resources: RateLimitResourcesJson,
}

#[derive(Debug, Deserialize)]
pub(super) struct RateLimitResourcesJson {
    pub(super) core: RateLimitResourceJson,
    #[serde(default)]
    pub(super) graphql: Option<RateLimitResourceJson>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RateLimitResourceJson {
    pub(super) limit: u64,
    pub(super) used: u64,
    pub(super) remaining: u64,
    pub(super) reset: u64,
}

impl From<RateLimitResourceJson> for model::GitHubRestRateLimit {
    fn from(resource: RateLimitResourceJson) -> Self {
        Self {
            limit: resource.limit,
            used: resource.used,
            remaining: resource.remaining,
            reset_unix_secs: resource.reset,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkflowRunJson {
    pub(super) database_id: u64,
    pub(super) head_sha: String,
    pub(super) status: String,
    pub(super) conclusion: String,
    pub(super) event: String,
    pub(super) name: String,
    pub(super) workflow_name: String,
    pub(super) url: String,
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
pub(super) struct CommitComparisonJson {
    pub(super) status: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct WorkflowRunDetailsJson {
    pub(super) id: u64,
    pub(super) head_sha: String,
    pub(super) status: String,
    pub(super) conclusion: Option<String>,
    pub(super) event: String,
    pub(super) name: String,
    pub(super) html_url: String,
    #[serde(default)]
    pub(super) pull_requests: Vec<WorkflowRunPullRequestJson>,
}

#[derive(Debug, Deserialize)]
pub(super) struct WorkflowRunPullRequestJson {
    pub(super) number: u64,
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
pub(super) struct GenerationPullRequestJson {
    pub(super) number: u64,
    #[serde(default)]
    pub(super) body: String,
    pub(super) head_ref_name: String,
    pub(super) head_ref_oid: String,
    pub(super) created_at: String,
}

impl GenerationPullRequestJson {
    pub(super) fn generation_fact(&self) -> model::PullRequestGenerationFact {
        crate::generation::parse_generation_fact(
            PrNumber(self.number),
            CommitOid(self.head_ref_oid.clone()),
            &self.head_ref_name,
            Some(self.created_at.clone()),
            &self.body,
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PullRequestJson {
    pub(super) number: u64,
    pub(super) title: String,
    #[serde(default)]
    pub(super) body: String,
    pub(super) state: ProviderPullRequestState,
    pub(super) is_draft: bool,
    pub(super) head_ref_name: String,
    pub(super) head_ref_oid: String,
    pub(super) head_repository: Option<HeadRepositoryJson>,
    pub(super) head_repository_owner: Option<RepositoryOwnerJson>,
    pub(super) is_cross_repository: bool,
    pub(super) base_ref_name: String,
    pub(super) base_ref_oid: String,
    #[serde(default)]
    pub(super) labels: Vec<LabelJson>,
    pub(super) auto_merge_request: Option<AutoMergeJson>,
    #[serde(default)]
    pub(super) status_check_rollup: Vec<CheckJson>,
    pub(super) created_at: String,
    pub(super) merged_at: Option<String>,
    pub(super) url: String,
    pub(super) updated_at: String,
    /// Forge verdict on mergeability. Optional because the history query does
    /// not request it and older captured fixtures predate the field.
    #[serde(default)]
    pub(super) merge_state_status: Option<String>,
}

impl PullRequestJson {
    pub(super) fn generation_fact(&self) -> model::PullRequestGenerationFact {
        crate::generation::parse_generation_fact(
            PrNumber(self.number),
            CommitOid(self.head_ref_oid.clone()),
            &self.head_ref_name,
            Some(self.created_at.clone()),
            &self.body,
        )
    }

    pub(super) fn into_snapshot(
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
                actor: request.enabled_by.map(|actor| actor.login),
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
            merge_state_status: self.merge_state_status,
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
pub(super) enum ProviderPullRequestState {
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
pub(super) struct HeadRepositoryJson {
    pub(super) name: Option<String>,
    pub(super) name_with_owner: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RepositoryOwnerJson {
    pub(super) login: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct IssueCommentJson {
    pub(super) body: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct LabelJson {
    pub(super) name: String,
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
pub(super) struct AutoMergeJson {
    pub(super) merge_method: String,
    #[serde(default)]
    pub(super) enabled_by: Option<RepositoryOwnerJson>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CheckJson {
    #[serde(rename = "__typename")]
    pub(super) kind: String,
    pub(super) name: Option<String>,
    pub(super) context: Option<String>,
    pub(super) status: Option<String>,
    pub(super) conclusion: Option<String>,
    pub(super) state: Option<String>,
    pub(super) workflow_name: Option<String>,
    pub(super) details_url: Option<String>,
    pub(super) target_url: Option<String>,
}

impl CheckJson {
    pub(super) fn into_snapshot(self) -> model::CheckSnapshot {
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
