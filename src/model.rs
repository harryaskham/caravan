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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AutoMergeState {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_method: Option<MergeMethod>,
}

impl AutoMergeState {
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            merge_method: None,
        }
    }

    #[must_use]
    pub const fn squash() -> Self {
        Self {
            enabled: true,
            merge_method: Some(MergeMethod::Squash),
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

/// Complete discovery result before graph policy is applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepositorySnapshot {
    pub repository: RepositoryId,
    pub default_branch: BranchSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_pr: Option<PrNumber>,
    /// Open PRs plus the bounded recently-merged labelled predecessors needed
    /// to recognize rolling head advancement.
    #[serde(default)]
    pub pull_requests: Vec<PullRequestSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
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

/// Exact PR facts which must still hold immediately before a remote mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PullRequestPrecondition {
    pub number: PrNumber,
    pub state: PullRequestState,
    pub head_oid: CommitOid,
    pub base_ref: String,
    pub base_oid: CommitOid,
    pub labels: BTreeSet<String>,
    pub auto_merge: AutoMergeState,
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
    EnableAutoMerge,
    DisableAutoMerge,
    RerunChecks,
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
    fn event_kind_serializes_to_config_key_spelling() {
        assert_eq!(
            serde_json::to_string(&EventKind::HeadAdvanced).unwrap(),
            "\"head_advanced\""
        );
    }
}
