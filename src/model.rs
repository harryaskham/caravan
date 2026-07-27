//! Stable domain model shared by discovery, graph, mutation, sync, hooks, CLI,
//! and MCP layers. These types carry facts and receipts; policy lives in the
//! consuming operations.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A GitHub pull-request number.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct PrNumber(pub u64);

impl Display for PrNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// A GitHub repository identity.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub struct RepositoryId {
    pub owner: String,
    pub name: String,
}

impl RepositoryId {
    #[must_use]
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

impl Display for RepositoryId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.owner, self.name)
    }
}

/// Exact Git object identity. Discovery keeps the provider's full OID string.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct CommitOid(pub String);

impl Display for CommitOid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A named branch at an exact revision in an exact repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BranchSnapshot {
    pub repository: RepositoryId,
    pub name: String,
    pub oid: CommitOid,
}

/// Provider-level PR state normalized for Caravan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

/// Squash is the only merge method Caravan may enable or execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    Squash,
}

/// Exact observed auto-merge state.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AutoMergeState {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_method: Option<MergeMethod>,
    /// Provider actor that enabled native auto-merge, when exposed by GitHub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

// Mutation policy historically guards enabled/method. Actor is observability:
// a provider may redact it between reads without making an otherwise exact
// mutation retry stale.
impl PartialEq for AutoMergeState {
    fn eq(&self, other: &Self) -> bool {
        self.enabled == other.enabled && self.merge_method == other.merge_method
    }
}
impl Eq for AutoMergeState {}

impl AutoMergeState {
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            merge_method: None,
            actor: None,
        }
    }

    #[must_use]
    pub const fn squash() -> Self {
        Self {
            enabled: true,
            merge_method: Some(MergeMethod::Squash),
            actor: None,
        }
    }
}

/// A normalized GitHub check state. Unknown provider values stay unknown rather
/// than being collapsed into success or failure policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Expected,
    Queued,
    InProgress,
    Success,
    Failure,
    Neutral,
    Skipped,
    Cancelled,
    TimedOut,
    ActionRequired,
    Unknown,
}

/// One observed status/check result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckSnapshot {
    pub name: String,
    pub state: CheckState,
    /// Exact provider value when available, especially when normalization yields
    /// `unknown`; policy must never guess from an unrecognized value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details_url: Option<String>,
}

/// Canonical PR fact snapshot. Discovery owns provider conversion; graph and
/// mutation layers consume this without reaching back into raw GraphQL JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PullRequestSnapshot {
    pub number: PrNumber,
    pub title: String,
    pub url: String,
    pub state: PullRequestState,
    #[serde(default)]
    pub draft: bool,
    pub head: BranchSnapshot,
    pub base: BranchSnapshot,
    pub cross_repository: bool,
    #[serde(default)]
    pub labels: BTreeSet<String>,
    pub auto_merge: AutoMergeState,
    #[serde(default)]
    pub checks: Vec<CheckSnapshot>,
    /// Immutable provider creation time used for FIFO admission ordering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl PullRequestSnapshot {
    #[must_use]
    pub fn has_label(&self, label: &str) -> bool {
        self.labels.contains(label)
    }

    #[must_use]
    pub fn is_active_caravan_member(&self) -> bool {
        self.state == PullRequestState::Open
            && self.has_label("caravan")
            && !self.has_label("caravan-evicted")
    }
}

/// Latest provider rate-limit evidence observed during one Cara operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubRateLimit {
    pub cost: u64,
    pub remaining: u64,
    pub reset_at: String,
}

/// One GitHub REST rate-limit resource returned by `GET /rate_limit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubRestRateLimit {
    pub limit: u64,
    pub used: u64,
    pub remaining: u64,
    pub reset_unix_secs: u64,
}

/// Relevant REST and GraphQL budgets used before initialization mutations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubRateLimits {
    pub core: GitHubRestRateLimit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graphql: Option<GitHubRestRateLimit>,
}

/// Secret-free authenticated GitHub command telemetry for one operation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubApiTelemetry {
    pub authenticated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_source: Option<String>,
    pub calls: u64,
    pub graphql_calls: u64,
    pub rest_calls: u64,
    pub gh_cli_calls: u64,
    pub cache_hits: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_age_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<GitHubRateLimit>,
}

impl GitHubApiTelemetry {
    /// Merge telemetry from another runner used by the same bounded operation.
    pub fn merge(&mut self, other: Self) {
        self.authenticated |= other.authenticated;
        if other.auth_source.is_some() {
            self.auth_source = other.auth_source;
        }
        self.calls = self.calls.saturating_add(other.calls);
        self.graphql_calls = self.graphql_calls.saturating_add(other.graphql_calls);
        self.rest_calls = self.rest_calls.saturating_add(other.rest_calls);
        self.gh_cli_calls = self.gh_cli_calls.saturating_add(other.gh_cli_calls);
        self.cache_hits = self.cache_hits.saturating_add(other.cache_hits);
        if other.cache_age_ms.is_some() {
            self.cache_age_ms = other.cache_age_ms;
        }
        if other.rate_limit.is_some() {
            self.rate_limit = other.rate_limit;
        }
    }
}

/// Exact Cacophony-owned PR generation metadata parsed from bounded provider
/// body fields. The source head is the immutable agent generation before any
/// Cara-owned physical rewrite of the provider PR branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CacophonyGenerationProvenance {
    pub generation: String,
    pub agent: String,
    pub source_head: CommitOid,
    #[serde(default)]
    pub bead_ids: BTreeSet<String>,
    pub stack_base: String,
    pub stack_state: String,
}

/// One open PR's bounded generation facts. Missing metadata on ordinary PRs is
/// not an error; a Cacophony-shaped or partially marked PR records an explicit
/// validation error and is never admitted automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PullRequestGenerationFact {
    pub pr: PrNumber,
    pub provider_head: CommitOid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<CacophonyGenerationProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_error: Option<String>,
    /// Exact PRs named by a reviewed provider-visible canonical-generation
    /// link record on this PR. Empty means no such authority was observed.
    #[serde(default)]
    pub supersedes: BTreeSet<PrNumber>,
}

/// Complete discovery result before graph policy is applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepositorySnapshot {
    pub repository: RepositoryId,
    pub default_branch: BranchSnapshot,
    /// Bounded provider evidence for active merge candidates. This is kept
    /// separate from graph policy so JSON/MCP clients can audit exact lineage.
    #[serde(default)]
    pub merge_candidates: Vec<MergeCandidateIdentity>,
    /// Number of additional active members omitted by the provider bound.
    #[serde(default)]
    pub merge_candidates_truncated: usize,
    /// Prior locally observed provider default OID, when a sync receipt exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_default_oid: Option<CommitOid>,
    #[serde(default)]
    pub default_branch_movements: Vec<DefaultBranchMovement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_pr: Option<PrNumber>,
    /// Open PRs plus the bounded recently-merged labelled predecessors needed
    /// to recognize rolling head advancement.
    #[serde(default)]
    pub pull_requests: Vec<PullRequestSnapshot>,
    /// Open provider PR generation facts used by admission integrity policy.
    #[serde(default)]
    pub generation_facts: Vec<PullRequestGenerationFact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
}

/// Provenance classification for a default-branch movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MovementOwnership {
    CaraOwned,
    External,
    /// A Caravan-labelled source is associated with Cara but does not prove
    /// which actor performed the merge without a matching operation receipt.
    Unknown,
}

/// One bounded recent commit that advanced the provider default branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DefaultBranchMovement {
    pub oid: CommitOid,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_pr: Option<PrNumber>,
    pub ownership: MovementOwnership,
}

/// Whether a synthetic provider merge candidate belongs to the currently
/// observed base/head generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MergeCandidateFreshness {
    Fresh,
    StaleHead,
    StaleBase,
    Missing,
    Unknown,
}

/// GitHub's bounded synthetic `refs/pull/<n>/merge` commit lineage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyntheticMergeCandidate {
    pub git_ref: String,
    pub oid: CommitOid,
    pub tree_oid: CommitOid,
    /// Provider order is preserved (GitHub normally emits base then head).
    pub parents: Vec<CommitOid>,
}

/// Provider-native auto-merge ownership, including the actor omitted by the
/// older graph-only auto-merge projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NativeAutoMergeState {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_method: Option<MergeMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

/// Exact identity of one active PR and its current synthetic candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MergeCandidateIdentity {
    pub pr: PrNumber,
    pub provider_updated_at: String,
    pub observed_at: String,
    pub base: BranchSnapshot,
    pub head: BranchSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthetic: Option<SyntheticMergeCandidate>,
    pub auto_merge: NativeAutoMergeState,
    pub freshness: MergeCandidateFreshness,
    /// Exact provider generation this freshness claim was compared against.
    /// For a default-based PR this is the live default tip, not the PR's own
    /// recorded base OID, so a consumer can verify the claim instead of
    /// trusting it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compared_base: Option<BranchSnapshot>,
    /// Independent flags preserve a simultaneous base+head generation mismatch.
    pub stale_base: bool,
    pub stale_head: bool,
    #[serde(default)]
    pub stale_reasons: Vec<String>,
}

/// A single linear caravan, ordered head to tail. `id` must equal `head()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Caravan {
    pub id: PrNumber,
    pub members: Vec<PrNumber>,
}

impl Caravan {
    #[must_use]
    pub fn new(members: Vec<PrNumber>) -> Option<Self> {
        let id = members.first().copied()?;
        Some(Self { id, members })
    }

    #[must_use]
    pub fn head(&self) -> Option<PrNumber> {
        self.members.first().copied()
    }

    #[must_use]
    pub fn tail(&self) -> Option<PrNumber> {
        self.members.last().copied()
    }

    #[must_use]
    pub fn position(&self, number: PrNumber) -> Option<usize> {
        self.members.iter().position(|member| *member == number)
    }
}

/// Which actor merges the caravan root into the default branch.
///
/// Provider-native auto-merge cannot be ordered against caravan-owned topology:
/// a root armed while its base was still a merged predecessor branch merges
/// instantly into that predecessor instead of the default branch. Caravan-owned
/// merging is therefore the intended end state.
///
/// It is nonetheless **not** the default. A runtime upgrade must never silently
/// change who merges a repository's pull requests: a fleet running an existing
/// config would flip merge actors the moment the new binary is deployed, before
/// its operators know the option exists. Absent configuration therefore
/// preserves exactly the historical behaviour, and `caravan` is opted into
/// explicitly once every consumer of that repository's config understands the
/// key.
///
/// The name is deliberately self-describing: `github` never means "do not merge
/// the head", it means "the provider's `autoMergeRequest` is the merge actor".
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum HeadMergeActor {
    /// Cara promotes the root to the default branch and performs the exact
    /// squash merges itself. No caravan member may carry native auto-merge.
    Caravan,
    /// The provider merges the root through native squash auto-merge armed by
    /// the scheduler on the exact root head. This is the backward-compatible
    /// default so an upgrade alone never changes the merge actor.
    #[default]
    Github,
}

impl HeadMergeActor {
    /// Stable code embedded in receipts, problems, and structured details.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Caravan => "caravan",
            Self::Github => "github",
        }
    }

    /// Whether provider-native auto-merge is the configured merge actor.
    #[must_use]
    pub const fn github(self) -> bool {
        matches!(self, Self::Github)
    }

    /// Whether the caravan itself is the single merge actor.
    #[must_use]
    pub const fn caravan(self) -> bool {
        matches!(self, Self::Caravan)
    }
}

/// Structural/fleet problem classes returned by graph validation.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum GraphProblemKind {
    MissingHead,
    MultipleHeads,
    Branching,
    Cycle,
    DanglingBase,
    ActiveAndEvicted,
    DuplicateMember,
    ForkOnlyPredecessor,
    AutoMergeInvariant,
    Incompatible,
    /// A child's base branch name resolves to an active head while a merged
    /// caravan member used the same branch name, so provenance is ambiguous.
    ReusedBranchProvenance,
    SupersededGeneration,
    AmbiguousGeneration,
    InvalidGenerationMetadata,
    Unknown,
}

/// One graph problem with the relevant PRs and evidence summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GraphProblem {
    pub kind: GraphProblemKind,
    #[serde(default)]
    pub prs: Vec<PrNumber>,
    pub message: String,
}

/// Validated repository-level Caravan view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CaravanFleet {
    pub repository: RepositoryId,
    pub default_branch: BranchSnapshot,
    #[serde(default)]
    pub caravans: Vec<Caravan>,
    #[serde(default)]
    pub unqueued: Vec<PrNumber>,
    #[serde(default)]
    pub problems: Vec<GraphProblem>,
}

impl CaravanFleet {
    #[must_use]
    pub fn caravan(&self, id: PrNumber) -> Option<&Caravan> {
        self.caravans.iter().find(|caravan| caravan.id == id)
    }

    #[must_use]
    pub fn containing(&self, number: PrNumber) -> Option<&Caravan> {
        self.caravans
            .iter()
            .find(|caravan| caravan.members.contains(&number))
    }
}

/// Outcome of one isolated Git compatibility test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityOutcome {
    Clean,
    Conflict,
    Unknown,
}

/// Mechanical merge evidence between exact candidate and target revisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CompatibilityReport {
    pub candidate: BranchSnapshot,
    pub target: BranchSnapshot,
    pub outcome: CompatibilityOutcome,
    #[serde(default)]
    pub conflicting_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

/// Exact cumulative-tree evidence for landing one candidate on one target.
///
/// A caravan member is physically rebased onto its predecessor chain *before*
/// CI runs, so the head SHA already carries the cumulative reviewed content.
/// Retargeting a promoted root to the default branch preserves that head SHA
/// and therefore preserves its check history, but it only stays safe while the
/// squash Cara is about to perform lands *exactly* that reviewed tree.
///
/// This proof states that mechanically: the tree `git merge-tree` constructs for
/// candidate-into-target equals the candidate head's own tree. When it holds,
/// the default branch moving underneath is irrelevant — the landed content is
/// byte-identical to what CI already validated. When it does not hold, the
/// target gained content the candidate never saw, and the caravan must
/// revalidate (physical rebase plus fresh CI) instead of merging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CumulativeTreeProof {
    /// Candidate head branch generation, normally the promoted caravan root.
    pub candidate: BranchSnapshot,
    /// Target branch generation, normally the exact default branch.
    pub target: BranchSnapshot,
    /// Tree object of the exact candidate head commit.
    pub candidate_tree: CommitOid,
    /// Tree object `git merge-tree` constructs for candidate-into-target.
    pub merge_result_tree: CommitOid,
    /// Whether the merge result is exactly the already-validated head tree.
    pub identical: bool,
}

impl CumulativeTreeProof {
    /// Stable explanation retained on receipts and structured details.
    #[must_use]
    pub fn reason(&self) -> String {
        if self.identical {
            format!(
                "merging {}@{} into {}@{} yields the exact already-validated head tree {}",
                self.candidate.name,
                self.candidate.oid,
                self.target.name,
                self.target.oid,
                self.candidate_tree,
            )
        } else {
            format!(
                "merging {}@{} into {}@{} yields tree {} instead of the already-validated head tree {}",
                self.candidate.name,
                self.candidate.oid,
                self.target.name,
                self.target.oid,
                self.merge_result_tree,
                self.candidate_tree,
            )
        }
    }
}

/// Exact PR facts which must still hold immediately before a remote mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PullRequestPrecondition {
    pub number: PrNumber,
    pub state: PullRequestState,
    pub head_oid: CommitOid,
    pub base_ref: String,
    pub base_oid: CommitOid,
    pub labels: BTreeSet<String>,
    /// Exact check facts guard mutations, especially explicit hold resume.
    #[serde(default)]
    pub checks: Vec<CheckSnapshot>,
    pub auto_merge: AutoMergeState,
}

impl PullRequestPrecondition {
    /// Compare only facts that authorize topology/label/base/auto-merge writes.
    /// Check progress remains observation state and is guarded separately by
    /// CI-specific commands.
    #[must_use]
    pub fn mutation_identity_eq(&self, other: &Self) -> bool {
        self.number == other.number
            && self.state == other.state
            && self.head_oid == other.head_oid
            && self.base_ref == other.base_ref
            && self.base_oid == other.base_oid
            && self.labels == other.labels
            && self.auto_merge == other.auto_merge
    }
}

impl From<&PullRequestSnapshot> for PullRequestPrecondition {
    fn from(snapshot: &PullRequestSnapshot) -> Self {
        Self {
            number: snapshot.number,
            state: snapshot.state,
            head_oid: snapshot.head.oid.clone(),
            base_ref: snapshot.base.name.clone(),
            base_oid: snapshot.base.oid.clone(),
            labels: snapshot.labels.clone(),
            checks: snapshot.checks.clone(),
            auto_merge: snapshot.auto_merge.clone(),
        }
    }
}

/// Unique operation identifier shared by receipts and emitted events.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct OperationId(pub String);

impl OperationId {
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }
}

impl Default for OperationId {
    fn default() -> Self {
        Self::new()
    }
}

impl Display for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Unique event identifier for hook-side deduplication.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct EventId(pub String);

impl EventId {
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

impl Display for EventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Remote mutation kinds recorded step by step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MutationKind {
    CreatePullRequest,
    SetBase,
    AddLabel,
    RemoveLabel,
    /// One reviewed provider transaction coupling exact-head force intent with
    /// the requested queue-owned squash auto-merge postcondition.
    ForceIntentTransaction,
    /// Durable GitHub-visible explanation of a control-label transition.
    Comment,
    EnableAutoMerge,
    DisableAutoMerge,
    /// Exact planned branch generation pushed under force-with-lease.
    RebaseBranch,
    RerunChecks,
    /// One auditable check-suite rerequest against an unchanged head, used only
    /// to recover required contexts that never started a run.
    RequestCheckSuite,
    SquashMerge,
    Checkout,
}

/// Result of one attempted mutation step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MutationStepState {
    Completed,
    AlreadySatisfied,
}

/// One completed/idempotently-satisfied remote step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MutationStep {
    pub kind: MutationKind,
    pub state: MutationStepState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrNumber>,
    pub summary: String,
}

/// Resumable receipt for a multi-step operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OperationReceipt {
    pub operation_id: OperationId,
    pub operation: String,
    #[serde(default)]
    pub completed_steps: Vec<MutationStep>,
    #[serde(default)]
    pub changed: bool,
}

/// Agent/user decision classes from the normative specification.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    HeadConflict,
    LinkConflict,
    CrossCaravanConflict,
    CiFailure,
    InvalidGraph,
    StalePrecondition,
    UnsafeCheckout,
    HookFailure,
    ForceMergeDenied,
}

/// A single resumable stop requiring a user or external agent decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DecisionPoint {
    pub kind: DecisionKind,
    pub operation_id: OperationId,
    pub repository: RepositoryId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caravan_id: Option<PrNumber>,
    #[serde(default)]
    pub affected_prs: Vec<PrNumber>,
    pub message: String,
    #[serde(default)]
    pub evidence: BTreeMap<String, Value>,
    #[serde(default)]
    pub completed_steps: Vec<MutationStep>,
    pub resumable: bool,
    #[serde(default)]
    pub suggested_actions: Vec<String>,
}

/// Hook/event types. The same snake-case names are keys in config YAML.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    CaravanCreated,
    PrJoined,
    ReadyPrUnqueued,
    SyncFailed,
    JoinFailed,
    EvictionFailed,
    HeadAdvanced,
    Evicted,
    Split,
    CiFailed,
    ForceMergeAttempted,
    ForceMergeCompleted,
    /// Historical: scheduler-owned convergence proved required native SQUASH
    /// auto-merge on the exact current caravan root head. Only emitted under the
    /// explicitly configured [`HeadMergePolicy::NativeAutoMerge`] compatibility
    /// policy; the caravan-owned merge architecture emits [`Self::RootPromoted`]
    /// and [`Self::RootMerged`] instead.
    RootAutoMergeArmed,
    /// The caravan root was proven to target the exact default branch before any
    /// merge was attempted.
    RootPromoted,
    /// The caravan itself squash-merged the exact promoted root head into the
    /// exact default branch.
    RootMerged,
    /// A required context has zero reporting run or check-suite lineage on the
    /// exact current head after the bounded grace period, so the caravan cannot
    /// advance without a visible decision.
    RequiredRunsMissing,
    /// Exactly one auditable check-suite rerequest was issued against an
    /// unchanged head to recover missing required-run coverage.
    RequiredRunsRetriggered,
}

impl std::fmt::Display for EventKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let encoded = serde_json::to_string(self).map_err(|_| std::fmt::Error)?;
        formatter.write_str(encoded.trim_matches('"'))
    }
}

/// Versioned metadata delivered to hooks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CaravanEvent {
    pub version: u32,
    pub event_id: EventId,
    pub operation_id: OperationId,
    pub kind: EventKind,
    pub repository: RepositoryId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caravan_id: Option<PrNumber>,
    #[serde(default)]
    pub prs: Vec<PrNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet: Option<CaravanFleet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    pub timestamp: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "harryaskham".to_owned(),
            name: "caravan".to_owned(),
        }
    }

    fn branch(name: &str, oid: &str) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository(),
            name: name.to_owned(),
            oid: CommitOid(oid.to_owned()),
        }
    }

    fn pull_request() -> PullRequestSnapshot {
        PullRequestSnapshot {
            number: PrNumber(42),
            title: "A queue change".to_owned(),
            url: "https://example.invalid/pull/42".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch("feature", "abc"),
            base: branch("main", "def"),
            cross_repository: false,
            labels: BTreeSet::from(["caravan".to_owned()]),
            auto_merge: AutoMergeState::squash(),
            checks: Vec::new(),
            created_at: Some("2026-01-01T00:00:00Z".to_owned()),
            merged_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn active_member_requires_open_active_and_not_evicted() {
        let mut pr = pull_request();
        assert!(pr.is_active_caravan_member());
        pr.labels.insert("caravan-evicted".to_owned());
        assert!(!pr.is_active_caravan_member());
        pr.labels.remove("caravan-evicted");
        pr.state = PullRequestState::Merged;
        assert!(!pr.is_active_caravan_member());
    }

    #[test]
    fn caravan_identity_is_the_current_head() {
        let caravan =
            Caravan::new(vec![PrNumber(7), PrNumber(8), PrNumber(9)]).expect("non-empty chain");
        assert_eq!(caravan.id, PrNumber(7));
        assert_eq!(caravan.head(), Some(PrNumber(7)));
        assert_eq!(caravan.tail(), Some(PrNumber(9)));
        assert_eq!(caravan.position(PrNumber(8)), Some(1));
        assert!(Caravan::new(Vec::new()).is_none());
    }

    #[test]
    fn precondition_captures_all_mutation_sensitive_pr_facts() {
        let pr = pull_request();
        let precondition = PullRequestPrecondition::from(&pr);
        assert_eq!(precondition.number, PrNumber(42));
        assert_eq!(precondition.head_oid, CommitOid("abc".to_owned()));
        assert_eq!(precondition.base_ref, "main");
        assert_eq!(precondition.labels, pr.labels);
        assert_eq!(precondition.auto_merge, AutoMergeState::squash());
    }

    #[test]
    fn operation_and_event_ids_are_unique_uuid_strings() {
        let first = OperationId::new();
        let second = OperationId::new();
        let event = EventId::new();
        assert_ne!(first, second);
        assert!(uuid::Uuid::parse_str(&first.0).is_ok());
        assert!(uuid::Uuid::parse_str(&event.0).is_ok());
    }

    #[test]
    fn fleet_lookup_finds_ids_and_members() {
        let fleet = CaravanFleet {
            repository: repository(),
            default_branch: branch("main", "def"),
            caravans: vec![Caravan::new(vec![PrNumber(1), PrNumber(2)]).unwrap()],
            unqueued: vec![PrNumber(3)],
            problems: Vec::new(),
        };
        assert_eq!(
            fleet.caravan(PrNumber(1)).unwrap().tail(),
            Some(PrNumber(2))
        );
        assert_eq!(fleet.containing(PrNumber(2)).unwrap().id, PrNumber(1));
        assert!(fleet.containing(PrNumber(3)).is_none());
    }

    #[test]
    fn event_kind_serializes_and_displays_with_canonical_spelling() {
        assert_eq!(
            serde_json::to_string(&EventKind::HeadAdvanced).unwrap(),
            "\"head_advanced\""
        );
        assert_eq!(EventKind::CiFailed.to_string(), "ci_failed");
    }
}
