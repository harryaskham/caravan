//! Live read-only command implementations: status, show, and check.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mcp_cli::{ErrorCategory, StructuredError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::command::CommandRunError;
use crate::github::{DiscoveryError, GitHubDiscovery};
#[cfg(test)]
use crate::graph::analyze_for_actor;
use crate::graph::{
    CompatibilityChecker, GitCompatibilityChecker, GraphAnalysis, analyze_for_actor_bounded,
    analyze_for_actor_with_progress,
};
use crate::model::{
    Caravan, CompatibilityOutcome, CompatibilityReport, GraphProblem, GraphProblemKind, PrNumber,
    PullRequestSnapshot, PullRequestState, RepositoryId,
};
use crate::squash_equivalence::SquashEquivalenceReport;
use crate::{AppContext, AppError, CheckInput};

/// Exact configured merge-actor policy shared by every merge-aware surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HeadMergeStatus {
    /// Who merges a caravan root into the default branch.
    pub actor: crate::model::HeadMergeActor,
    /// What a caravan-owned tick does about a foreign auto-merge request.
    pub external_auto_merge_policy: crate::root_merge::ExternalAutoMergePolicy,
    /// Bounded caravan-owned merges per tick.
    pub max_root_merges_per_tick: u32,
}

impl Default for HeadMergeStatus {
    fn default() -> Self {
        Self {
            actor: crate::model::HeadMergeActor::default(),
            external_auto_merge_policy: crate::root_merge::ExternalAutoMergePolicy::default(),
            max_root_merges_per_tick: crate::config::SyncConfig::default().max_root_merges_per_tick,
        }
    }
}

impl HeadMergeStatus {
    /// Derive the exact policy from configuration.
    #[must_use]
    pub fn from_config(config: &crate::config::SyncConfig) -> Self {
        Self {
            actor: config.resolved_head_merge_actor(),
            external_auto_merge_policy: config.external_auto_merge_policy,
            max_root_merges_per_tick: config.max_root_merges_per_tick,
        }
    }
}

/// Explicit repository policy and rollout action for physical chain rebuilding.

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RebaseOnJoinStatus {
    pub enabled: bool,
    pub state: String,
    pub config_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_action: Option<String>,
}

impl Default for RebaseOnJoinStatus {
    fn default() -> Self {
        Self {
            enabled: false,
            state: "disabled".to_owned(),
            config_path: crate::config::DEFAULT_CONFIG_PATH.to_owned(),
            required_action: Some(format!(
                "set `rebase_on_join: true` in {} after canary review",
                crate::config::DEFAULT_CONFIG_PATH
            )),
        }
    }
}

fn rebase_on_join_status(context: &AppContext) -> RebaseOnJoinStatus {
    let config_path = context.config_path.display().to_string();
    RebaseOnJoinStatus {
        enabled: context.config.rebase_on_join,
        state: if context.config.rebase_on_join {
            "enabled"
        } else {
            "disabled"
        }
        .to_owned(),
        required_action: (!context.config.rebase_on_join).then(|| {
            format!(
                "set `rebase_on_join: true` in {config_path}, commit it, then run `cara check` and `cara sync --all`"
            )
        }),
        config_path,
    }
}

/// Whether the configured provider exposes native Stack reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StackCapability {
    NotProbed,
    Available,
    Unavailable,
    Unknown,
}

impl StackCapability {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotProbed => "not_probed",
            Self::Available => "available",
            Self::Unavailable => "unavailable",
            Self::Unknown => "unknown",
        }
    }
}

/// Whether Cara may mutate through the configured backend in this release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StackMutationSupport {
    Caravan,
    ReadOnlyPreview,
    NativeStack,
}

impl StackMutationSupport {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Caravan => "caravan",
            Self::ReadOnlyPreview => "read_only_preview",
            Self::NativeStack => "native_stack",
        }
    }
}

/// Exact relationship between one provider Stack and Cara's graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StackConsistency {
    Exact,
    Drifted,
    Orphaned,
    Unknown,
}

impl StackConsistency {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Drifted => "drifted",
            Self::Orphaned => "orphaned",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StackBackendProblem {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NativeStackStatus {
    pub stack: crate::github::GitHubStackSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caravan_id: Option<PrNumber>,
    pub consistency: StackConsistency,
    #[serde(default)]
    pub problems: Vec<StackBackendProblem>,
}

/// Read-only configured backend status. The stable Caravan default never probes
/// the GitHub Stacks API; explicit GitHub mode reports capability and drift but
/// remains mutation-blocked until later rollout slices land.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StackBackendStatus {
    pub configured: crate::config::StackType,
    pub capability: StackCapability,
    pub mutation_support: StackMutationSupport,
    #[serde(default)]
    pub native_stacks: Vec<NativeStackStatus>,
    #[serde(default)]
    pub provider_stacks_truncated: bool,
    #[serde(default)]
    pub missing_caravans: Vec<PrNumber>,
    #[serde(default)]
    pub problems: Vec<StackBackendProblem>,
}

impl Default for StackBackendStatus {
    fn default() -> Self {
        Self {
            configured: crate::config::StackType::Caravan,
            capability: StackCapability::NotProbed,
            mutation_support: StackMutationSupport::Caravan,
            native_stacks: Vec::new(),
            provider_stacks_truncated: false,
            missing_caravans: Vec::new(),
            problems: Vec::new(),
        }
    }
}

trait NativeStackInventoryProvider {
    fn native_stack_inventory(
        &self,
        repository: &RepositoryId,
    ) -> Result<crate::github::GitHubStackInventory, crate::github::GitHubStackReadError>;
}

impl<R: crate::command::CommandRunner> NativeStackInventoryProvider
    for crate::github::GitHubMutationAdapter<R>
{
    fn native_stack_inventory(
        &self,
        repository: &RepositoryId,
    ) -> Result<crate::github::GitHubStackInventory, crate::github::GitHubStackReadError> {
        self.native_stack_inventory(repository)
    }
}

fn apply_stack_backend_mutation_policy(
    config: &crate::config::CaravanConfig,
    backend: &StackBackendStatus,
    initialization: &mut crate::initialization::InitializationStatus,
) {
    if config.stack_type != crate::config::StackType::Github {
        return;
    }
    // A failed or unavailable capability probe is never absence, and must stay
    // distinguishable from the ordinary rollout fence so a future rollout
    // cannot open the gate against an unproven provider (bd-a79679).
    let blocker = match backend.capability {
        StackCapability::Unavailable => crate::initialization::InitializationMutationBlocker {
            code: "github_stack_capability_unavailable".to_owned(),
            message: "GitHub native Stacks are unavailable for this repository".to_owned(),
            next: "do not treat unavailability as an absent Stack: restore the native Stack capability, or set stack_type: caravan".to_owned(),
        },
        StackCapability::Unknown => crate::initialization::InitializationMutationBlocker {
            code: "github_stack_capability_unknown".to_owned(),
            message: "GitHub native Stack capability could not be proven".to_owned(),
            next: "resolve the reported capability diagnostic and re-read status; an unproven capability never authorizes a mutation".to_owned(),
        },
        // The allowlist is an independent gate: selecting the backend alone
        // can never enable every repository.
        StackCapability::NotProbed | StackCapability::Available
            if !config.stack_rollout.mutations_opt_in =>
        {
            crate::initialization::InitializationMutationBlocker {
                code: "github_stack_repository_not_opted_in".to_owned(),
                message: "this repository has not explicitly opted into native GitHub Stack mutations".to_owned(),
                next: "record a reviewed stack_rollout.mutations_opt_in with reviewed_by; provider capability and every operation-specific safety gate must also pass".to_owned(),
            }
        }
        StackCapability::NotProbed => crate::initialization::InitializationMutationBlocker {
            code: "github_stack_capability_unknown".to_owned(),
            message: "GitHub native Stack capability was not probed".to_owned(),
            next: "re-read status and prove native Stack capability before mutation".to_owned(),
        },
        StackCapability::Available => {
            // Reviewed repository opt-in plus proven capability opens the
            // executable path. Individual operations still fail closed on
            // exact mapping/generation/policy evidence.
            return;
        }
    };
    initialization.ready = false;
    initialization.mutation_blocker = Some(blocker);
}

fn validate_native_stack_entries(
    stack: &crate::github::GitHubStackSnapshot,
    analysis: &GraphAnalysis,
    problems: &mut Vec<StackBackendProblem>,
    consistency: &mut StackConsistency,
) {
    for (index, entry) in stack.pull_requests.iter().enumerate() {
        let number = PrNumber(entry.number);
        let Some(pull) = analysis.pull_requests.get(&number) else {
            problems.push(StackBackendProblem {
                code: "github_stack_pr_missing".to_owned(),
                message: format!(
                    "Stack #{} PR #{} is absent from Cara discovery",
                    stack.number, number
                ),
            });
            *consistency = StackConsistency::Drifted;
            continue;
        };
        if pull.head.oid != entry.head.sha || pull.head.name != entry.head.ref_name {
            problems.push(StackBackendProblem {
                code: "github_stack_head_drift".to_owned(),
                message: format!(
                    "Stack #{} PR #{} head generation differs from Cara discovery",
                    stack.number, number
                ),
            });
            *consistency = StackConsistency::Drifted;
        }
        let expected_base = if index == 0 {
            analysis.fleet.default_branch.name.as_str()
        } else {
            stack.pull_requests[index - 1].head.ref_name.as_str()
        };
        if pull.base.name != expected_base {
            problems.push(StackBackendProblem {
                code: "github_stack_pr_base_drift".to_owned(),
                message: format!(
                    "Stack #{} PR #{} targets `{}` instead of `{expected_base}`",
                    stack.number, number, pull.base.name
                ),
            });
            *consistency = StackConsistency::Drifted;
        }
    }
}

fn native_stack_status(
    stack: crate::github::GitHubStackSnapshot,
    analysis: &GraphAnalysis,
    represented: &mut BTreeSet<PrNumber>,
) -> NativeStackStatus {
    let members = stack
        .pull_requests
        .iter()
        .map(|pull| PrNumber(pull.number))
        .collect::<Vec<_>>();
    let caravan = members
        .first()
        .and_then(|member| analysis.fleet.containing(*member));
    let mut problems = Vec::new();
    let (caravan_id, mut consistency) = if let Some(caravan) = caravan {
        represented.insert(caravan.id);
        if caravan.members == members {
            (Some(caravan.id), StackConsistency::Exact)
        } else {
            problems.push(StackBackendProblem {
                code: "github_stack_member_order_drift".to_owned(),
                message: format!(
                    "Stack #{} members {:?} do not equal caravan #{} members {:?}",
                    stack.number, members, caravan.id, caravan.members
                ),
            });
            (Some(caravan.id), StackConsistency::Drifted)
        }
    } else {
        problems.push(StackBackendProblem {
            code: "github_stack_orphaned".to_owned(),
            message: format!(
                "Stack #{} does not map to any current Caravan",
                stack.number
            ),
        });
        (None, StackConsistency::Orphaned)
    };
    if stack.base.ref_name != analysis.fleet.default_branch.name {
        problems.push(StackBackendProblem {
            code: "github_stack_base_drift".to_owned(),
            message: format!(
                "Stack #{} targets `{}` instead of `{}`",
                stack.number, stack.base.ref_name, analysis.fleet.default_branch.name
            ),
        });
        consistency = StackConsistency::Drifted;
    }
    validate_native_stack_entries(&stack, analysis, &mut problems, &mut consistency);
    NativeStackStatus {
        stack,
        caravan_id,
        consistency,
        problems,
    }
}

fn stack_backend_status(
    configured: crate::config::StackType,
    provider: &impl NativeStackInventoryProvider,
    repository: &RepositoryId,
    analysis: &GraphAnalysis,
) -> StackBackendStatus {
    if configured == crate::config::StackType::Caravan {
        return StackBackendStatus::default();
    }

    let inventory = match provider.native_stack_inventory(repository) {
        Ok(inventory) => inventory,
        Err(crate::github::GitHubStackReadError::Unavailable { diagnostic }) => {
            return StackBackendStatus {
                configured,
                capability: StackCapability::Unavailable,
                mutation_support: StackMutationSupport::ReadOnlyPreview,
                problems: vec![StackBackendProblem {
                    code: "github_stacks_unavailable".to_owned(),
                    message: diagnostic,
                }],
                ..StackBackendStatus::default()
            };
        }
        Err(error) => {
            return StackBackendStatus {
                configured,
                capability: StackCapability::Unknown,
                mutation_support: StackMutationSupport::ReadOnlyPreview,
                problems: vec![StackBackendProblem {
                    code: "github_stacks_capability_unknown".to_owned(),
                    message: error.to_string(),
                }],
                ..StackBackendStatus::default()
            };
        }
    };

    let mut represented = BTreeSet::new();
    let native_stacks = inventory
        .stacks
        .into_iter()
        .map(|stack| native_stack_status(stack, analysis, &mut represented))
        .collect::<Vec<_>>();

    let missing_caravans = if inventory.truncated {
        Vec::new()
    } else {
        analysis
            .fleet
            .caravans
            .iter()
            .filter(|caravan| caravan.members.len() >= 2 && !represented.contains(&caravan.id))
            .map(|caravan| caravan.id)
            .collect::<Vec<_>>()
    };
    let mut problems = missing_caravans
        .iter()
        .map(|caravan| StackBackendProblem {
            code: "github_stack_absent".to_owned(),
            message: format!("caravan #{caravan} has no provider-native Stack"),
        })
        .collect::<Vec<_>>();
    if inventory.truncated {
        problems.push(StackBackendProblem {
            code: "github_stack_inventory_truncated".to_owned(),
            message: "GitHub returned a full bounded page; repository-wide Stack absence cannot be proven".to_owned(),
        });
    }
    StackBackendStatus {
        configured,
        capability: StackCapability::Available,
        mutation_support: StackMutationSupport::ReadOnlyPreview,
        native_stacks,
        provider_stacks_truncated: inventory.truncated,
        missing_caravans,
        problems,
    }
}

/// Repository-wide live Caravan status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusOutput {
    /// Exact binary that produced this receipt. Queue operators must be able to
    /// prove which build answered when several are installed (bd-0629ce).
    #[serde(default)]
    pub runtime: RuntimeProvenance,
    /// Whether the effective configuration is the repository's policy or one
    /// branch's proposal. Reported only; nothing depends on it yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_provenance: Option<crate::config_provenance::ConfigProvenance>,
    /// Authenticated provider-call counts and latest rate-limit evidence.
    #[serde(default)]
    pub provider_api: crate::model::GitHubApiTelemetry,
    /// Bounded, secret-free provider identity and lineage for active members.
    #[serde(default)]
    pub merge_candidates: Vec<crate::model::MergeCandidateIdentity>,
    #[serde(default)]
    pub merge_candidates_truncated: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_default_oid: Option<crate::model::CommitOid>,
    #[serde(default)]
    pub default_branch_movements: Vec<crate::model::DefaultBranchMovement>,
    /// Successful phase timings make large-repository regressions diagnosable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<StatusTiming>,
    pub repository: RepositoryId,
    /// Explicitly reports enabled/disabled physical chain-rebuild policy.
    #[serde(default)]
    pub rebase_on_join: RebaseOnJoinStatus,
    /// Configured Stack backend, provider capability, and exact read-only drift.
    #[serde(default)]
    pub stack_backend: StackBackendStatus,
    /// Exact configured merge-actor policy for caravan roots. Every surface
    /// that arms, refuses, or performs a merge reads this one fact so the
    /// auto-merge invariant, membership, reshape, pause, and sync can never
    /// disagree about who merges.
    #[serde(default)]
    pub head_merge: HeadMergeStatus,
    /// Exact sync-owned automatic-admission policy and safety bounds.
    #[serde(default)]
    pub auto_admission: AutoAdmissionStatus,
    /// Deterministic physical-apply reserve projection for the configured
    /// deadline: required budget, processable prefix, blocked candidate, and
    /// the safe next action, all visible before any sync refusal.
    #[serde(default)]
    pub sync_budget: crate::sync::SyncBudgetStatus,
    pub default_branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_pr: Option<PrNumber>,
    pub healthy: bool,
    /// Read-only first-use readiness. Status never creates or edits resources.
    pub initialization: crate::initialization::InitializationStatus,
    pub analysis: GraphAnalysis,
    /// Explicit caravan holds. Active and expired holds intentionally suspend
    /// only their exact head auto-merge invariant; stale holds fail closed.
    #[serde(default)]
    pub pauses: Vec<crate::pause::PauseStatus>,
    /// Canonical, nonmutating automatic-admission order derived from GitHub.
    pub admission: AdmissionStatus,
}

/// Which bounded evidence backs a degraded read-only status receipt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StatusPartialEvidenceSource {
    /// A previous complete config-matched snapshot survived a later timeout.
    #[default]
    HistoricalLastGood,
    /// This invocation completed provider discovery and bounded local analysis.
    CurrentBoundedEvidence,
}

/// Typed evidence that read-only status yielded before its dedicated budget was
/// consumed. The evidence source distinguishes a historical fallback from a
/// first-ever status assembled from current provider and structural facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusPartial {
    pub schema_version: u32,
    pub code: String,
    pub exhausted_phase: String,
    pub cursor: String,
    pub elapsed_ms: u64,
    pub deadline_ms: u64,
    pub remaining_ms: u64,
    #[serde(default)]
    pub evidence_source: StatusPartialEvidenceSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_good_age_ms: Option<u64>,
    #[serde(default)]
    pub unknown_fields: Vec<String>,
    #[serde(default)]
    pub attempt_provider_api: crate::model::GitHubApiTelemetry,
    /// Status partials are evidence only and never carry provider mutation.
    pub mutated: bool,
    pub safe_next_action: String,
}

/// CLI/MCP-facing status receipt. Flattening preserves the stable complete-
/// status shape consumed by Cacophony while adding one explicit partial marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusReadReceipt {
    #[serde(flatten)]
    pub output: StatusOutput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_partial: Option<StatusPartial>,
}

/// Timing and bounded-analysis evidence for one read-only status operation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusTiming {
    pub deadline_ms: u64,
    pub total_ms: u64,
    /// Time deliberately withheld from compatibility work so final projection
    /// and serialization cannot overrun the public status deadline.
    #[serde(default)]
    pub completion_reserve_ms: u64,
    #[serde(default)]
    pub compatibility_budget_ms: u64,
    #[serde(default)]
    pub compatibility_analysis: crate::graph::CompatibilityAnalysisProgress,
    pub phases_ms: std::collections::BTreeMap<String, u64>,
}

/// One selectable PR in canonical priority-then-FIFO order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AutoAdmissionStatus {
    pub enabled: bool,
    pub heuristic_version: String,
    /// Admission fence for simultaneously active, non-parked caravans.
    #[serde(default = "default_max_caravans")]
    #[schemars(default = "default_max_caravans")]
    pub max_caravans: u32,
    /// Current non-parked caravans. Existing excess is preserved and converged.
    #[serde(default)]
    pub active_caravans: usize,
    /// Deterministic capacity-consuming caravan IDs.
    #[serde(default)]
    pub active_caravan_ids: Vec<PrNumber>,
    /// Parked caravans retained outside active admission capacity.
    #[serde(default)]
    pub parked_caravans: usize,
    /// Deterministic parked caravan IDs excluded from capacity.
    #[serde(default)]
    pub parked_caravan_ids: Vec<PrNumber>,
    /// Active caravans above the configured fence; never repaired destructively.
    #[serde(default)]
    pub excess_active_caravans: usize,
    /// Whether forming another caravan is currently refused.
    #[serde(default)]
    pub at_caravan_capacity: bool,
    /// First priority/FIFO candidate blocked if it needs a new root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_blocked_root_candidate: Option<PrNumber>,
    pub max_candidates_per_tick: u32,
    pub max_mutations_per_tick: u32,
    pub max_github_requests_per_tick: u32,
    pub max_duration_secs: u64,
}

const fn default_max_caravans() -> u32 {
    1
}

impl Default for AutoAdmissionStatus {
    fn default() -> Self {
        Self {
            enabled: false,
            heuristic_version: crate::sync::AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
            max_caravans: 1,
            active_caravans: 0,
            active_caravan_ids: Vec::new(),
            parked_caravans: 0,
            parked_caravan_ids: Vec::new(),
            excess_active_caravans: 0,
            at_caravan_capacity: false,
            first_blocked_root_candidate: None,
            max_candidates_per_tick: 0,
            max_mutations_per_tick: 0,
            max_github_requests_per_tick: 0,
            max_duration_secs: 0,
        }
    }
}

impl AutoAdmissionStatus {
    #[must_use]
    pub(crate) fn from_config(
        config: &crate::config::SyncConfig,
        analysis: &GraphAnalysis,
        first_candidate: Option<PrNumber>,
    ) -> Self {
        let active_caravan_ids = analysis
            .fleet
            .caravans
            .iter()
            .filter(|caravan| !caravan.parked)
            .map(|caravan| caravan.id)
            .collect::<Vec<_>>();
        let parked_caravan_ids = analysis
            .fleet
            .caravans
            .iter()
            .filter(|caravan| caravan.parked)
            .map(|caravan| caravan.id)
            .collect::<Vec<_>>();
        let active_caravans = active_caravan_ids.len();
        let parked_caravans = parked_caravan_ids.len();
        let max_caravans = usize::try_from(config.max_caravans).unwrap_or(usize::MAX);
        Self {
            enabled: config.actions.join_unlabelled_prs,
            heuristic_version: crate::sync::AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
            max_caravans: config.max_caravans,
            active_caravans,
            active_caravan_ids,
            parked_caravans,
            parked_caravan_ids,
            excess_active_caravans: active_caravans.saturating_sub(max_caravans),
            at_caravan_capacity: active_caravans >= max_caravans,
            first_blocked_root_candidate: (active_caravans >= max_caravans)
                .then_some(first_candidate)
                .flatten(),
            max_candidates_per_tick: config.max_candidates_per_tick,
            max_mutations_per_tick: config.max_mutations_per_tick,
            max_github_requests_per_tick: config.max_github_requests_per_tick,
            max_duration_secs: config.max_duration_secs,
        }
    }
}

/// One selectable PR in canonical priority-then-FIFO order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdmissionCandidate {
    pub pr: PrNumber,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_rank: Option<usize>,
    /// Immutable GitHub creation timestamp used as the FIFO key. Legacy or
    /// synthetic snapshots may omit it and deterministically fall back to PR number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    pub reason: String,
}

/// One ready-looking PR excluded from automation because priority metadata is unsafe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkippedAdmissionCandidate {
    pub pr: PrNumber,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_rank: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    pub reason: String,
}

/// One ready-looking PR excluded from automation because priority metadata is unsafe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RejectedAdmissionCandidate {
    pub pr: PrNumber,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_rank: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Superseded generations are excluded rather than allowed to block their
    /// proven canonical successor. Ambiguous/invalid candidates still block.
    #[serde(default = "default_true")]
    pub blocks_order: bool,
    pub reason: String,
}

const fn default_true() -> bool {
    true
}

/// Resolved GitHub-visible automatic-admission policy and result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdmissionStatus {
    pub policy: String,
    pub priority_labels: Vec<String>,
    /// Cacophony generation classification is provider-derived and remains
    /// visible even when a superseded PR is excluded from FIFO selection.
    #[serde(default)]
    pub generation_integrity: crate::generation::GenerationIntegrityStatus,
    /// Ordered highest priority first, then immutable provider creation time.
    pub candidates: Vec<AdmissionCandidate>,
    /// Exact generations carrying the sync-owned best-effort skip label.
    #[serde(default)]
    pub skipped: Vec<SkippedAdmissionCandidate>,
    #[serde(default)]
    pub rejected: Vec<RejectedAdmissionCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_candidate: Option<PrNumber>,
}

/// Dedicated read-only result for deterministic admission coordination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NextCandidateOutput {
    #[serde(default)]
    pub provider_api: crate::model::GitHubApiTelemetry,
    pub repository: RepositoryId,
    /// Ordering is selection-only: the chosen PR must still pass `check`/`new`
    /// preflight and a failure must not cause an automatic leapfrog.
    pub attempt_contract: String,
    /// Exact command to run next, so a correct FIFO rejection of a later PR is
    /// not mistaken for a queue fault (bd-d7aae7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_admission_command: Option<NextAdmissionCommand>,
    pub admission: AdmissionStatus,
    /// Typed automatic-selection decision for the canonical candidate. This is
    /// the FIFO-bound surface: it is emitted with
    /// `selection = automatic`, and automatic selection never bypasses an
    /// earlier ordered row for either `new` or `join` intent. Compare it with a
    /// `cara check --pr N` receipt's `admission_intent` to see explicit owner
    /// intent evaluated separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automatic_selection: Option<crate::admission::AdmissionIntentDecision>,
}

/// Current PR's ordered caravan view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ShowOutput {
    pub repository: RepositoryId,
    /// Merged PR named by the checked-out branch, when rolling context was recovered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub historical_predecessor: Option<PrNumber>,
    /// Exact bounded-history facts for the merged branch-local predecessor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub historical_pull_request: Option<PullRequestSnapshot>,
    /// Effective active successor used for chain position and navigation.
    pub current_pr: PrNumber,
    pub caravan: Caravan,
    /// Zero-based head-to-tail position.
    pub position: usize,
    pub pull_requests: Vec<PullRequestSnapshot>,
    /// Exact synthetic lineage for these caravan members.
    #[serde(default)]
    pub merge_candidates: Vec<crate::model::MergeCandidateIdentity>,
    pub healthy: bool,
    #[serde(default)]
    pub problems: Vec<GraphProblem>,
}

/// Which eligibility contract `cara check` evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckMode {
    ActiveCaravan,
    NewCaravan,
    JoinTail,
}

/// Mechanical continuation after an exact read-only candidate preflight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CandidateNextAction {
    New,
    Join,
    Repair,
    Wait,
    Reject,
}

/// Successful read-only eligibility/health result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckOutput {
    #[serde(default)]
    pub provider_api: crate::model::GitHubApiTelemetry,
    #[serde(default)]
    pub rebase_on_join: RebaseOnJoinStatus,
    pub mode: CheckMode,
    /// Candidate PR (named `current_pr` for backwards-compatible JSON).
    pub current_pr: PrNumber,
    /// Exact provider candidate facts used by this receipt.
    pub candidate: PullRequestSnapshot,
    /// Provider-visible head repository owner (branch ownership, not PR authorship).
    pub head_repository_owner: String,
    /// Canonical provider merge-candidate identity and freshness from status/show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_candidate: Option<crate::model::MergeCandidateIdentity>,
    /// Whether the candidate was already in the discovered active fleet.
    pub enrolled: bool,
    /// Whether this is the canonical first priority/FIFO admission attempt.
    /// Automatic selection follows this order; an explicit `--pr` selection is
    /// deliberate owner intent and is not blocked by it for either `new` or
    /// `join`.
    pub canonical_candidate: bool,
    /// Non-blocking evidence when explicit intent differs from the automatic
    /// priority/FIFO order. Derived from `admission_intent` so the human note,
    /// the typed decision, and the mutation gate can never disagree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_note: Option<String>,
    /// Typed selection/intent-aware admission-order decision and provenance.
    /// Explicit owner intent — `new` or `join` — is resolved here before FIFO
    /// canonical-candidate rejection; automatic selection stays FIFO-bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_intent: Option<crate::admission::AdmissionIntentDecision>,
    pub next_action: CandidateNextAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caravan_id: Option<PrNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_pr: Option<PrNumber>,
    pub eligible: bool,
    #[serde(default)]
    pub compatibility: Vec<CompatibilityReport>,
    /// Exact squash-equivalence evidence for every non-clean pair above.
    ///
    /// A stacked candidate whose earliest commits already landed as a squash
    /// conflicts against content identical to what it carries; this states,
    /// with exact per-path blob proof, whether that is the case and whether
    /// reconciling it would leave a clean replay. It is evidence only.
    #[serde(default)]
    pub squash_reconciliations: Vec<SquashEquivalenceReport>,
    #[serde(default)]
    pub problems: Vec<GraphProblem>,
    pub initialization: crate::initialization::InitializationStatus,
}

struct CachedStatus {
    inserted: Instant,
    output: StatusOutput,
}

static STATUS_CACHE: OnceLock<Mutex<HashMap<String, CachedStatus>>> = OnceLock::new();
const MAX_STATUS_CACHE_ENTRIES: usize = 64;
const STATUS_CACHE_RETENTION: Duration = Duration::from_secs(3_600);

struct CachedLabels {
    inserted: Instant,
    labels: Vec<crate::github::RepositoryLabel>,
}

static LABEL_CACHE: OnceLock<Mutex<HashMap<String, CachedLabels>>> = OnceLock::new();
const LABEL_CACHE_MAX_AGE: Duration = Duration::from_secs(600);

fn status_cache_key(context: &AppContext) -> String {
    let config = serde_json::to_vec(&context.config).unwrap_or_default();
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in config {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{}:{hash:016x}", context.repository_path.display())
}

/// Return a short-lived read-only status snapshot for polling surfaces.
///
/// Mutating operations never call this function: exact preflight and provider
/// rereads remain authoritative. The cache only coalesces duplicate dashboard
/// or controller refreshes inside one long-lived Cara process.
pub(crate) fn status_cached(
    context: &AppContext,
    max_age: Duration,
) -> Result<StatusOutput, AppError> {
    let key = status_cache_key(context);
    let cache = STATUS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let guard = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = guard.get(&key) {
            let age = cached.inserted.elapsed();
            if age <= max_age {
                let mut output = cached.output.clone();
                output.provider_api.cache_hits = output.provider_api.cache_hits.saturating_add(1);
                output.provider_api.cache_age_ms =
                    Some(u64::try_from(age.as_millis()).unwrap_or(u64::MAX));
                return Ok(output);
            }
        }
    }
    let output = status(context)?;
    let mut guard = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.retain(|_, cached| cached.inserted.elapsed() <= STATUS_CACHE_RETENTION);
    if guard.len() >= MAX_STATUS_CACHE_ENTRIES {
        if let Some(oldest) = guard
            .iter()
            .min_by_key(|(_, cached)| cached.inserted)
            .map(|(key, _)| key.clone())
        {
            guard.remove(&oldest);
        }
    }
    guard.insert(
        key,
        CachedStatus {
            inserted: Instant::now(),
            output: output.clone(),
        },
    );
    Ok(output)
}

/// Invalidate one repository's read-only polling cache after an explicit action.
pub(crate) fn invalidate_status_cache(context: &AppContext) {
    if let Some(cache) = STATUS_CACHE.get() {
        cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&status_cache_key(context));
    }
}

fn repository_labels_cached<R: crate::command::CommandRunner>(
    provider: &crate::github::GitHubMutationAdapter<R>,
    repository: &RepositoryId,
) -> Result<(Vec<crate::github::RepositoryLabel>, bool), crate::github::MutationError> {
    let key = repository.slug();
    let cache = LABEL_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let guard = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = guard.get(&key)
            && cached.inserted.elapsed() <= LABEL_CACHE_MAX_AGE
        {
            return Ok((cached.labels.clone(), true));
        }
    }
    let labels = provider.repository_label_definitions(repository)?;
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            key,
            CachedLabels {
                inserted: Instant::now(),
                labels: labels.clone(),
            },
        );
    Ok((labels, false))
}

fn attach_provider_api(
    error: &AppError,
    provider_api: &crate::model::GitHubApiTelemetry,
) -> AppError {
    let mut details = error.details().unwrap_or_else(|| json!({}));
    if let Some(object) = details.as_object_mut() {
        object.insert("provider_api".to_owned(), json!(provider_api));
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

/// Discover and validate the real current repository without mutation.
/// Exact identity of the running Cara binary.
///
/// Several Cara builds can be installed at once (a reviewed Nix store closure
/// and a cargo-installed binary), and PATH order alone decides which answers.
/// When one of them is broken, a silent crash is indistinguishable from a quiet
/// queue, so every status receipt records exactly which build produced it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeProvenance {
    /// Compiled package version.
    pub version: String,
    /// Fully resolved executable path, symlinks followed where possible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// SHA-256 of the exact running executable, when it is readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_sha256: Option<String>,
    /// True when the executable resolves inside a Nix store path.
    pub nix_store: bool,
}

impl RuntimeProvenance {
    #[must_use]
    pub fn detect() -> Self {
        let executable = std::env::current_exe()
            .ok()
            .map(|path| std::fs::canonicalize(&path).unwrap_or(path));
        let executable_sha256 = executable.as_ref().and_then(|path| {
            let bytes = std::fs::read(path).ok()?;
            Some(format!(
                "sha256:{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(&bytes)
            ))
        });
        let display = executable.as_ref().map(|path| path.display().to_string());
        Self {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            nix_store: display
                .as_deref()
                .is_some_and(|path| path.starts_with("/nix/store/")),
            executable: display,
            executable_sha256,
        }
    }
}

/// Prove an exact ancestry relation from objects already present locally.
///
/// The provider comparison is authoritative when it succeeds. When a ref has
/// been deleted or is otherwise unreachable, the same commits are frequently
/// still in the local object database, and an exact local proof is strictly
/// better than declaring an entire same-stream component unprovable.
fn local_commit_relation(
    repository_path: &std::path::Path,
    base: &crate::model::CommitOid,
    head: &crate::model::CommitOid,
) -> Option<crate::generation::CommitRelation> {
    use crate::command::{CommandRunner, CommandSpec, ProcessRunner};
    let runner = ProcessRunner::in_directory(repository_path);
    let known = |oid: &crate::model::CommitOid| {
        runner
            .run(&CommandSpec::new("git").args([
                "cat-file",
                "-e",
                &format!("{}^{{commit}}", oid.0),
            ]))
            .is_ok_and(|output| output.is_success())
    };
    if !known(base) || !known(head) {
        return None;
    }
    if base == head {
        return Some(crate::generation::CommitRelation::Identical);
    }
    let ancestor = |first: &crate::model::CommitOid, second: &crate::model::CommitOid| {
        runner
            .run(&CommandSpec::new("git").args([
                "merge-base",
                "--is-ancestor",
                first.0.as_str(),
                second.0.as_str(),
            ]))
            .is_ok_and(|output| output.is_success())
    };
    match (ancestor(base, head), ancestor(head, base)) {
        (true, true) => Some(crate::generation::CommitRelation::Identical),
        (true, false) => Some(crate::generation::CommitRelation::Ahead),
        (false, true) => Some(crate::generation::CommitRelation::Behind),
        (false, false) => Some(crate::generation::CommitRelation::Diverged),
    }
}

pub fn status(context: &AppContext) -> Result<StatusOutput, AppError> {
    let budget = std::time::Duration::from_secs(context.config.command_timeout_secs);
    status_with_deadline(context, std::time::Instant::now() + budget)
}

const STATUS_READ_BUDGET: Duration = Duration::from_secs(35);
const STATUS_COMPLETION_RESERVE: Duration = Duration::from_secs(2);
/// Outer CLI subprocess bound. This is independent of the executor's own
/// deadline and leaves Cacophony's 60-second adapter time to collect JSON.
pub const STATUS_COMMAND_WATCHDOG: Duration = Duration::from_secs(40);
/// Compatibility is local but can be quadratic in caravan count. Keep a second
/// in-operation reserve for provider-backed stack projection and final status
/// assembly instead of letting the final merge-tree consume the whole window.
const STATUS_POST_ANALYSIS_RESERVE: Duration = Duration::from_secs(8);
const STATUS_LAST_GOOD_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const STATUS_LAST_GOOD_MAX_BYTES: u64 = 8 * 1024 * 1024;
const STATUS_LAST_GOOD_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedStatus {
    schema_version: u32,
    recorded_unix_ms: u64,
    config_fingerprint: String,
    output: StatusOutput,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StatusWatchdogCheckpoint {
    schema_version: u32,
    config_fingerprint: String,
    receipt: StatusReadReceipt,
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn git_common_dir(repository: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .current_dir(repository)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn status_last_good_path(context: &AppContext) -> Option<PathBuf> {
    Some(
        git_common_dir(&context.repository_path)?
            .join("cara")
            .join("status-last-good-v1.json"),
    )
}

fn persist_last_good_at(path: &Path, context: &AppContext, output: &StatusOutput) {
    let persisted = PersistedStatus {
        schema_version: STATUS_LAST_GOOD_SCHEMA_VERSION,
        recorded_unix_ms: unix_millis(),
        config_fingerprint: status_cache_key(context),
        output: output.clone(),
    };
    let Ok(bytes) = serde_json::to_vec(&persisted) else {
        return;
    };
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > STATUS_LAST_GOOD_MAX_BYTES {
        return;
    }
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    if std::fs::write(&temporary, bytes).is_ok() && std::fs::rename(&temporary, path).is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
}

fn persist_last_good(context: &AppContext, output: &StatusOutput) {
    if let Some(path) = status_last_good_path(context) {
        persist_last_good_at(&path, context, output);
    }
}

fn write_watchdog_checkpoint(path: &Path, context: &AppContext, receipt: &StatusReadReceipt) {
    let checkpoint = StatusWatchdogCheckpoint {
        schema_version: 1,
        config_fingerprint: status_cache_key(context),
        receipt: receipt.clone(),
    };
    let Ok(bytes) = serde_json::to_vec(&checkpoint) else {
        return;
    };
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > STATUS_LAST_GOOD_MAX_BYTES {
        return;
    }
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    if std::fs::write(&temporary, bytes).is_ok() && std::fs::rename(&temporary, path).is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
}

fn load_watchdog_checkpoint(path: &Path, context: &AppContext) -> Option<StatusReadReceipt> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > STATUS_LAST_GOOD_MAX_BYTES {
        return None;
    }
    let checkpoint: StatusWatchdogCheckpoint =
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (checkpoint.schema_version == 1 && checkpoint.config_fingerprint == status_cache_key(context))
        .then_some(checkpoint.receipt)
}

fn load_last_good_at(path: &Path, context: &AppContext) -> Option<PersistedStatus> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > STATUS_LAST_GOOD_MAX_BYTES {
        return None;
    }
    let persisted: PersistedStatus = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    if persisted.schema_version != STATUS_LAST_GOOD_SCHEMA_VERSION
        || persisted.config_fingerprint != status_cache_key(context)
    {
        return None;
    }
    let age_ms = unix_millis().saturating_sub(persisted.recorded_unix_ms);
    (age_ms <= u64::try_from(STATUS_LAST_GOOD_MAX_AGE.as_millis()).unwrap_or(u64::MAX))
        .then_some(persisted)
}

fn load_last_good(context: &AppContext) -> Option<PersistedStatus> {
    load_last_good_at(&status_last_good_path(context)?, context)
}

fn partial_phase(error: &AppError) -> String {
    error
        .details()
        .and_then(|details| {
            details
                .get("phase")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "provider_enumeration".to_owned())
}

fn partial_provider_api(error: &AppError) -> crate::model::GitHubApiTelemetry {
    error
        .details()
        .and_then(|details| details.get("provider_api").cloned())
        .and_then(|telemetry| serde_json::from_value(telemetry).ok())
        .unwrap_or_default()
}

fn recover_partial_status(
    error: &AppError,
    started: Instant,
    deadline: Duration,
    persisted: PersistedStatus,
) -> StatusReadReceipt {
    let now_ms = unix_millis();
    let age_ms = now_ms.saturating_sub(persisted.recorded_unix_ms);
    let elapsed = started.elapsed();
    let mut output = persisted.output;
    output.healthy = false;
    StatusReadReceipt {
        output,
        status_partial: Some(StatusPartial {
            schema_version: 1,
            code: "status_partial".to_owned(),
            exhausted_phase: partial_phase(error),
            cursor: format!("last_good_complete_status:{}", persisted.recorded_unix_ms),
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            deadline_ms: u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
            remaining_ms: u64::try_from(deadline.saturating_sub(elapsed).as_millis())
                .unwrap_or(u64::MAX),
            evidence_source: StatusPartialEvidenceSource::HistoricalLastGood,
            last_good_age_ms: Some(age_ms),
            unknown_fields: vec![format!("current_attempt.{}", partial_phase(error))],
            attempt_provider_api: partial_provider_api(error),
            mutated: false,
            safe_next_action: "retry the read-only status pass after provider latency recovers; sync mutation and post-rewrite reserves were not used or weakened".to_owned(),
        }),
    }
}

fn current_checkpoint_status(
    context: &AppContext,
    snapshot: &crate::model::RepositorySnapshot,
    initialization: &crate::initialization::InitializationStatus,
    provider_api: &crate::model::GitHubApiTelemetry,
    started: Instant,
    deadline: Duration,
) -> StatusReadReceipt {
    let analysis =
        crate::graph::derive_for_actor(snapshot, context.config.sync.resolved_head_merge_actor());
    let admission = resolve_admission(&analysis, &context.config.agent_priority_labels);
    let stack_backend = StackBackendStatus {
        configured: context.config.stack_type,
        ..StackBackendStatus::default()
    };
    let elapsed = started.elapsed();
    let output = StatusOutput {
        runtime: RuntimeProvenance::detect(),
        config_provenance: Some(crate::config_provenance::resolve(
            &context.repository_path,
            &context.config_path,
            context.config_path.is_absolute(),
        )),
        provider_api: provider_api.clone(),
        merge_candidates: snapshot.merge_candidates.clone(),
        merge_candidates_truncated: snapshot.merge_candidates_truncated,
        previous_default_oid: snapshot.previous_default_oid.clone(),
        default_branch_movements: snapshot.default_branch_movements.clone(),
        timing: Some(StatusTiming {
            deadline_ms: u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
            total_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            completion_reserve_ms: u64::try_from(
                STATUS_COMPLETION_RESERVE
                    .saturating_add(STATUS_POST_ANALYSIS_RESERVE)
                    .as_millis(),
            )
            .unwrap_or(u64::MAX),
            compatibility_budget_ms: 0,
            compatibility_analysis: crate::graph::CompatibilityAnalysisProgress {
                candidate_count: snapshot.pull_requests.len(),
                unqueued_candidate_count: analysis.fleet.unqueued.len(),
                caravan_count: analysis.fleet.caravans.len(),
                ..crate::graph::CompatibilityAnalysisProgress::default()
            },
            phases_ms: std::collections::BTreeMap::from([(
                "provider_discovery_checkpoint".to_owned(),
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            )]),
        }),
        repository: snapshot.repository.clone(),
        rebase_on_join: rebase_on_join_status(context),
        stack_backend,
        head_merge: HeadMergeStatus::from_config(&context.config.sync),
        auto_admission: AutoAdmissionStatus::from_config(
            &context.config.sync,
            &analysis,
            admission.next_candidate,
        ),
        sync_budget: crate::sync::SyncBudgetStatus::default(),
        default_branch: snapshot.default_branch.name.clone(),
        current_branch: snapshot.current_branch.clone(),
        current_pr: snapshot.current_pr,
        healthy: false,
        initialization: initialization.clone(),
        analysis,
        pauses: Vec::new(),
        admission,
    };
    StatusReadReceipt {
        output,
        status_partial: Some(StatusPartial {
            schema_version: 2,
            code: "status_partial".to_owned(),
            exhausted_phase: "command_boundary_watchdog".to_owned(),
            cursor: format!(
                "provider_discovery_checkpoint:{}:{}",
                snapshot.repository.slug(),
                unix_millis()
            ),
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            deadline_ms: u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
            remaining_ms: u64::try_from(deadline.saturating_sub(elapsed).as_millis())
                .unwrap_or(u64::MAX),
            evidence_source: StatusPartialEvidenceSource::CurrentBoundedEvidence,
            last_good_age_ms: None,
            unknown_fields: vec![
                "analysis.compatibility".to_owned(),
                "analysis.generation_integrity".to_owned(),
                "stack_backend.provider_projection".to_owned(),
                "pauses".to_owned(),
            ],
            attempt_provider_api: provider_api.clone(),
            mutated: false,
            safe_next_action: "retry read-only status to complete local compatibility analysis; current provider/structural evidence is retained and no sync mutation reserve was used".to_owned(),
        }),
    }
}

fn recover_current_partial_status(
    mut output: StatusOutput,
    started: Instant,
    deadline: Duration,
) -> StatusReadReceipt {
    let elapsed = started.elapsed();
    output.healthy = false;
    let progress = output
        .timing
        .as_ref()
        .map(|timing| &timing.compatibility_analysis);
    let mut unknown_fields = progress.map_or_else(Vec::new, |progress| {
        let mut unknown = progress
            .deferred_analyses
            .iter()
            .map(|analysis| format!("analysis.deferred.{analysis}"))
            .collect::<Vec<_>>();
        unknown.extend(
            progress
                .skipped_analyses
                .iter()
                .map(|analysis| format!("analysis.skipped.{analysis}")),
        );
        if !progress.revision_preparation_complete {
            unknown.push("analysis.revision_preparation".to_owned());
        }
        unknown
    });
    if let Some(progress) = progress {
        if progress.deferred_analyses_truncated > 0 {
            unknown_fields.push(format!(
                "analysis.deferred_additional:{}",
                progress.deferred_analyses_truncated
            ));
        }
        if progress.skipped_analyses_truncated > 0 {
            unknown_fields.push(format!(
                "analysis.skipped_additional:{}",
                progress.skipped_analyses_truncated
            ));
        }
    }
    if unknown_fields.is_empty() {
        unknown_fields.push("analysis.compatibility_unknown".to_owned());
    }
    let cursor = format!(
        "current_bounded_evidence:{}:{}",
        output.repository.slug(),
        unix_millis()
    );
    let attempt_provider_api = output.provider_api.clone();
    StatusReadReceipt {
        output,
        status_partial: Some(StatusPartial {
            schema_version: 2,
            code: "status_partial".to_owned(),
            exhausted_phase: "compatibility_analysis".to_owned(),
            cursor,
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            deadline_ms: u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
            remaining_ms: u64::try_from(deadline.saturating_sub(elapsed).as_millis())
                .unwrap_or(u64::MAX),
            evidence_source: StatusPartialEvidenceSource::CurrentBoundedEvidence,
            last_good_age_ms: None,
            unknown_fields,
            attempt_provider_api,
            mutated: false,
            safe_next_action: "retry the read-only status pass to complete deferred compatibility proofs; current provider evidence is degraded only and sync mutation and post-rewrite reserves were not used or weakened".to_owned(),
        }),
    }
}

/// Run the human/CLI status surface under its own short read budget.
///
/// This path never borrows the sync mutation deadline or post-rewrite reserve.
/// Slow local compatibility yields a useful current-evidence partial even on a
/// first-ever call. Other timeouts retain the config-matched last-good fallback.
pub fn status_resilient(context: &AppContext) -> Result<StatusReadReceipt, AppError> {
    let started = Instant::now();
    let budget = STATUS_READ_BUDGET;
    let operation_budget = budget.saturating_sub(STATUS_COMPLETION_RESERVE);
    match status_with_resilient_deadline(context, started + operation_budget) {
        Ok(output)
            if output
                .timing
                .as_ref()
                .is_some_and(|timing| !timing.compatibility_analysis.complete) =>
        {
            Ok(recover_current_partial_status(output, started, budget))
        }
        Ok(output) => {
            persist_last_good(context, &output);
            Ok(StatusReadReceipt {
                output,
                status_partial: None,
            })
        }
        Err(error) if error.category() == ErrorCategory::Timeout => load_last_good(context)
            .map(|persisted| recover_partial_status(&error, started, budget, persisted))
            .ok_or_else(|| {
                AppError::structured(
                    ErrorCategory::Timeout,
                    "status_partial_unavailable",
                    "read-only status exhausted its dedicated provider budget before any last-good snapshot was available",
                    Some(json!({
                        "phase": partial_phase(&error),
                        "elapsed_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                        "deadline_ms": u64::try_from(budget.as_millis()).unwrap_or(u64::MAX),
                        "provider_api": partial_provider_api(&error),
                        "status_partial": false,
                        "safe_next_action": "retry read-only status after provider latency recovers; do not change sync mutation reserves or add another loop",
                    })),
                )
            }),
        Err(error) => Err(error),
    }
}

/// Recover a typed status envelope after the outer CLI watchdog proves the
/// in-process executor itself failed to return.
pub fn status_watchdog_fallback(
    context: &AppContext,
    checkpoint_path: &Path,
    elapsed: Duration,
) -> Result<StatusReadReceipt, AppError> {
    if let Some(mut receipt) = load_watchdog_checkpoint(checkpoint_path, context) {
        receipt.output.healthy = false;
        if let Some(partial) = receipt.status_partial.as_mut() {
            "command_boundary_watchdog".clone_into(&mut partial.exhausted_phase);
            partial.elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
            partial.deadline_ms =
                u64::try_from(STATUS_COMMAND_WATCHDOG.as_millis()).unwrap_or(u64::MAX);
            partial.remaining_ms = 0;
            "retry read-only status after the wedged compatibility executor is diagnosed; provider/structural checkpoint evidence was retained and no sync mutation reserve was used"
                .clone_into(&mut partial.safe_next_action);
        }
        return Ok(receipt);
    }

    let error = AppError::structured(
        ErrorCategory::Timeout,
        "status_command_watchdog_timeout",
        "the read-only status executor did not return before its command-boundary watchdog",
        Some(json!({
            "phase": "command_boundary_watchdog",
            "elapsed_ms": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            "deadline_ms": u64::try_from(STATUS_COMMAND_WATCHDOG.as_millis()).unwrap_or(u64::MAX),
            "status_partial": false,
        })),
    );
    if let Some(persisted) = load_last_good(context) {
        return Ok(recover_partial_status(
            &error,
            Instant::now()
                .checked_sub(elapsed)
                .unwrap_or_else(Instant::now),
            STATUS_COMMAND_WATCHDOG,
            persisted,
        ));
    }
    Err(AppError::structured(
        ErrorCategory::Timeout,
        "status_partial_unavailable",
        "the read-only status executor exceeded its outer watchdog before provider/structural checkpoint evidence was available",
        Some(json!({
            "phase": "command_boundary_watchdog",
            "elapsed_ms": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            "deadline_ms": u64::try_from(STATUS_COMMAND_WATCHDOG.as_millis()).unwrap_or(u64::MAX),
            "status_partial": false,
            "safe_next_action": "diagnose the wedged status executor; do not change sync mutation reserves, candidate capacity, or loop cadence",
        })),
    ))
}

/// Run status under a caller-supplied absolute deadline. This narrow seam lets
/// orchestration share a future whole-operation budget without changing child APIs.
#[allow(clippy::too_many_lines)]
pub(crate) fn status_with_deadline(
    context: &AppContext,
    operation_deadline: std::time::Instant,
) -> Result<StatusOutput, AppError> {
    status_with_deadline_and_budget(context, operation_deadline, None)
}

fn status_with_resilient_deadline(
    context: &AppContext,
    operation_deadline: std::time::Instant,
) -> Result<StatusOutput, AppError> {
    status_with_discovery_options(context, operation_deadline, None, false, true, None, true)
}

/// Status with one shared exact authenticated GitHub request budget.
pub(crate) fn status_with_deadline_and_budget(
    context: &AppContext,
    operation_deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<StatusOutput, AppError> {
    status_with_discovery_options(
        context,
        operation_deadline,
        github_budget,
        false,
        true,
        None,
        false,
    )
}

/// Status for a fleet-level read that must not depend on the local checkout.
///
/// Admission order and the next candidate are properties of the provider graph.
/// A checkout left on a merged or ambiguous branch — the state Cara itself
/// produces by retiring merged heads — must not stop the queue from advancing.
/// `current_pr` simply resolves to `None`, which every PR-scoped command already
/// treats as a refusal to act rather than a licence to guess.
pub(crate) fn fleet_status(
    context: &AppContext,
    operation_deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<StatusOutput, AppError> {
    status_with_discovery_options(
        context,
        operation_deadline,
        github_budget,
        false,
        false,
        None,
        false,
    )
}

/// Explicit PR-creation discovery permits one safe, advanced, unlabelled
/// historical branch generation to be treated as ancestry rather than current
/// membership. Ordinary status/navigation keeps the strict historical rule.
pub(crate) fn status_for_pr_creation(
    context: &AppContext,
    operation_deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<StatusOutput, AppError> {
    status_with_discovery_options(
        context,
        operation_deadline,
        github_budget,
        true,
        true,
        None,
        false,
    )
}

#[derive(Debug, Clone)]
struct ExpectedCandidateHead {
    pr: PrNumber,
    branch: String,
    oid: crate::model::CommitOid,
}

fn bind_expected_candidate_head(
    snapshot: &mut crate::model::RepositorySnapshot,
    expected: &ExpectedCandidateHead,
) -> Result<(), AppError> {
    let candidate = snapshot
        .pull_requests
        .iter_mut()
        .find(|candidate| candidate.number == expected.pr)
        .ok_or_else(|| {
            AppError::structured(
                ErrorCategory::TargetNotFound,
                "rebase_rediscovery_failed",
                "rewritten PR was absent from post-push provider discovery",
                Some(json!({
                    "pr": expected.pr,
                    "expected_branch": expected.branch,
                    "expected_head_oid": expected.oid,
                    "resumable": true,
                })),
            )
        })?;
    if candidate.head.name != expected.branch {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "rebase_rediscovery_stale",
            "rewritten PR no longer names the exact branch that Cara pushed",
            Some(json!({
                "pr": expected.pr,
                "expected_branch": expected.branch,
                "observed_branch": candidate.head.name,
                "expected_head_oid": expected.oid,
                "resumable": true,
            })),
        ));
    }

    // The complete open-PR list can briefly retain the pre-push head after the
    // exact ref API and Git advertisement already expose Cara's successful
    // lease push. Bind every downstream analysis to that proven generation;
    // compatibility preparation verifies the remote ref again, and mutation
    // preconditions refetch it once more before any membership write.
    candidate.head.oid.clone_from(&expected.oid);
    for fact in snapshot
        .generation_facts
        .iter_mut()
        .filter(|fact| fact.pr == expected.pr)
    {
        fact.provider_head.clone_from(&expected.oid);
    }
    snapshot.current_pr = Some(expected.pr);
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn status_with_discovery_options(
    context: &AppContext,
    operation_deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
    allow_unlabelled_historical_pr_creation: bool,
    require_current_pr_resolution: bool,
    expected_candidate_head: Option<&ExpectedCandidateHead>,
    bounded_compatibility: bool,
) -> Result<StatusOutput, AppError> {
    // Sharing one absolute deadline prevents a large repository from
    // multiplying its budget by provider and compatibility subprocess count.
    let started = std::time::Instant::now();
    let operation_budget = operation_deadline.saturating_duration_since(started);
    let child_timeout = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let provider_runner = crate::command::ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(child_timeout)
        .with_operation_deadline(operation_deadline);
    let provider_runner = github_budget.map_or(provider_runner.clone(), |budget| {
        provider_runner.with_github_request_budget(budget.clone())
    });
    let discovery = GitHubDiscovery::new(provider_runner.clone()).with_options(
        crate::github::DiscoveryOptions {
            allow_unlabelled_historical_pr_creation,
            require_current_pr_resolution,
            repository: context.config.repository.clone(),
            ..crate::github::DiscoveryOptions::default()
        },
    );
    let mut snapshot = discovery.discover().map_err(|error| {
        let mapped =
            if let DiscoveryError::Runner(CommandRunError::Timeout { command, .. }) = &error {
                discovery_timeout_error(
                    &error,
                    discovery_phase(command),
                    // Discovery is the first phase, so its own cost is the
                    // elapsed budget: nothing ran before it to spend any.
                    started.elapsed(),
                    started.elapsed(),
                    operation_budget,
                )
            } else {
                discovery_error(&error)
            };
        attach_provider_api(&mapped, &provider_runner.github_api_telemetry())
    })?;
    if let Some(expected) = expected_candidate_head {
        let provider = crate::github::GitHubMutationAdapter::new(provider_runner.clone());
        provider
            .verify_branch_head(&snapshot.repository, &expected.branch, &expected.oid)
            .map_err(|error| match error {
                crate::github::MutationError::BranchHeadMismatch {
                    branch,
                    expected: expected_oid,
                    actual,
                } => AppError::structured(
                    ErrorCategory::Validation,
                    "rebase_rediscovery_stale",
                    "provider branch moved after Cara's exact physical push",
                    Some(json!({
                        "pr": expected.pr,
                        "branch": branch,
                        "expected_oid": expected_oid,
                        "observed_oid": actual,
                        "resumable": true,
                    })),
                ),
                error => AppError::structured(
                    ErrorCategory::ExecutionFailure,
                    "rebase_rediscovery_refetch_failed",
                    "could not verify the exact post-push provider branch generation",
                    Some(json!({
                        "pr": expected.pr,
                        "error": error.to_string(),
                        "resumable": true,
                    })),
                ),
            })?;
        bind_expected_candidate_head(&mut snapshot, expected)?;
    }
    let discovery_elapsed = started.elapsed();

    // Resolve a minimal provider-backed label/identity read before local
    // compatibility. A large merge graph must never spend the whole first-
    // status budget before Cara has made any provider/auth attempt.
    let label_provider = crate::github::GitHubMutationAdapter::new(provider_runner.clone());
    let (labels, label_cache_hit) = repository_labels_cached(&label_provider, &snapshot.repository)
        .map_err(|error| {
            let mapped = if let crate::github::MutationError::Provider(provider) = &error
                && matches!(
                    provider,
                    DiscoveryError::Runner(CommandRunError::Timeout { .. })
                ) {
                discovery_timeout_error(
                    provider,
                    "repository_label_inventory",
                    started.elapsed().saturating_sub(discovery_elapsed),
                    started.elapsed(),
                    operation_budget,
                )
            } else {
                AppError::structured(
                    mcp_cli::ErrorCategory::ExecutionFailure,
                    "repository_initialization_inventory_failed",
                    error.to_string(),
                    Some(json!({"next": "repair GitHub read access and rerun `cara status`"})),
                )
            };
            attach_provider_api(&mapped, &provider_runner.github_api_telemetry())
        })?;
    let label_inventory_elapsed = started.elapsed();
    let mut initialization = crate::initialization::inspect_labels(
        &labels,
        &context.config.agent_priority_labels,
        context.config.sync.actions.join_unlabelled_prs,
        context.config.sync.terminal_red.action == crate::config::TerminalRedAction::Park,
    );
    if !context.config_existed {
        initialization.ready = false;
        initialization.next = Some("run `cara init` to atomically create .caravan/config.yaml and verify repository readiness".to_owned());
    }
    if bounded_compatibility
        && let Ok(path) = std::env::var("CARA_STATUS_WATCHDOG_CHECKPOINT")
        && !path.trim().is_empty()
    {
        let receipt = current_checkpoint_status(
            context,
            &snapshot,
            &initialization,
            &provider_runner.github_api_telemetry(),
            started,
            operation_budget,
        );
        write_watchdog_checkpoint(Path::new(&path), context, &receipt);
    }
    let compatibility_deadline = if bounded_compatibility {
        operation_deadline
            .checked_sub(STATUS_POST_ANALYSIS_RESERVE)
            .unwrap_or(started)
            .max(started)
    } else {
        operation_deadline
    };
    let checker = GitCompatibilityChecker::new(&context.repository_path, "origin")
        .with_timeout(child_timeout)
        .with_operation_deadline(compatibility_deadline);
    // The auto-merge invariant is gated on the configured merge actor so a
    // repository that deliberately disabled native auto-merge never reports a
    // permanently unsatisfiable problem.
    let analyzed = if bounded_compatibility {
        analyze_for_actor_bounded(
            &snapshot,
            &checker,
            context.config.sync.resolved_head_merge_actor(),
            compatibility_deadline,
        )
    } else {
        analyze_for_actor_with_progress(
            &snapshot,
            &checker,
            context.config.sync.resolved_head_merge_actor(),
        )
    }
    .map_err(|error| {
        if mcp_cli::StructuredError::category(&error) == ErrorCategory::Timeout {
            AppError::structured(
                ErrorCategory::Timeout,
                "github_discovery_timeout",
                "compatibility analysis exceeded the status deadline",
                Some(json!({
                    "stage": "github_discovery",
                    "phase": "compatibility_analysis",
                    "elapsed_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "deadline_ms": u64::try_from(operation_budget.as_millis()).unwrap_or(u64::MAX),
                    "retryable": true,
                    "safe_next_action": "retry `cara status` after restoring Git transport health; status made no mutations",
                    "source": mcp_cli::StructuredError::details(&error),
                })),
            )
        } else {
            error
        }
    })?;
    let mut analysis = analyzed.analysis;
    let compatibility_progress = analyzed.progress;
    let compatibility_complete = compatibility_progress.complete;
    let analysis_elapsed = started.elapsed();

    // Compatibility preparation fetches exact objects used by the preferred
    // local ancestry proof. Keep generation analysis after that fetch while the
    // earlier label read still guarantees a provider attempt before local work.
    let mut generation_facts = snapshot.generation_facts.clone();
    for pr in crate::generation::duplicate_stream_prs(&generation_facts)
        .into_iter()
        .take(32)
    {
        if let Ok(comments) = label_provider.pull_request_comment_bodies(&snapshot.repository, pr) {
            crate::generation::attach_reviewed_supersession_links(
                &mut generation_facts,
                pr,
                &comments,
            );
        }
    }
    let generation_integrity = crate::generation::analyze(&generation_facts, |base, head| {
        // bd-7546ea: exact local objects are an authoritative ancestry proof.
        // A provider comparison can be unreachable (deleted ref, 404) or simply
        // wrong/stale, and reporting a direct parent/child pair as `diverged`
        // dead-ends the owner. Prefer a local proof of ancestry whenever both
        // commits are present; otherwise keep the provider's answer.
        let provider_relation = label_provider.compare_commits(&snapshot.repository, base, head);
        let local = local_commit_relation(&context.repository_path, base, head);
        match (provider_relation, local) {
            // A local ancestry proof is authoritative and overrides an
            // unreachable or contradictory provider answer.
            (
                _,
                Some(
                    relation @ (crate::generation::CommitRelation::Ahead
                    | crate::generation::CommitRelation::Behind
                    | crate::generation::CommitRelation::Identical),
                ),
            )
            | (Err(_), Some(relation))
            | (Ok(relation), _) => relation,
            (Err(error), None) => crate::generation::CommitRelation::Unknown {
                reason: error.to_string(),
            },
        }
    });
    let provider_identity_elapsed = started.elapsed();
    apply_generation_graph_problems(&mut analysis, &generation_integrity);
    let mut stack_backend = stack_backend_status(
        context.config.stack_type,
        &label_provider,
        &snapshot.repository,
        &analysis,
    );
    if context.config.stack_type == crate::config::StackType::Github
        && context.config.stack_rollout.mutations_opt_in
        && stack_backend.capability == StackCapability::Available
    {
        stack_backend.mutation_support = StackMutationSupport::NativeStack;
    }
    apply_stack_backend_mutation_policy(&context.config, &stack_backend, &mut initialization);
    let admission = resolve_admission_with_generation(
        &analysis,
        &context.config.agent_priority_labels,
        generation_integrity,
    );
    let projection_elapsed = started.elapsed();
    let mut provider_api = provider_runner.github_api_telemetry();
    if label_cache_hit {
        provider_api.cache_hits = provider_api.cache_hits.saturating_add(1);
        provider_api.cache_age_ms = LABEL_CACHE
            .get()
            .and_then(|cache| {
                cache.lock().ok().and_then(|cache| {
                    cache
                        .get(&snapshot.repository.slug())
                        .map(|entry| entry.inserted.elapsed())
                })
            })
            .map(|age| u64::try_from(age.as_millis()).unwrap_or(u64::MAX));
    }
    let mut output = StatusOutput {
        head_merge: HeadMergeStatus::from_config(&context.config.sync),
        runtime: RuntimeProvenance::detect(),
        // Reported only: measure how often a checkout is running one branch's
        // proposed policy before anything is allowed to depend on the answer.
        config_provenance: Some(crate::config_provenance::resolve(
            &context.repository_path,
            &context.config_path,
            context.config_path.is_absolute(),
        )),
        provider_api,
        merge_candidates: snapshot.merge_candidates,
        merge_candidates_truncated: snapshot.merge_candidates_truncated,
        previous_default_oid: snapshot.previous_default_oid,
        default_branch_movements: snapshot.default_branch_movements,
        timing: None,
        repository: snapshot.repository,
        rebase_on_join: rebase_on_join_status(context),
        stack_backend,
        auto_admission: AutoAdmissionStatus::from_config(
            &context.config.sync,
            &analysis,
            admission.next_candidate,
        ),
        default_branch: snapshot.default_branch.name,
        current_branch: snapshot.current_branch,
        current_pr: snapshot.current_pr,
        healthy: false,
        initialization,
        analysis,
        pauses: Vec::new(),
        admission,
        sync_budget: crate::sync::SyncBudgetStatus::default(),
    };
    crate::pause::apply_to_status(&context.repository_path, &mut output)?;
    output.sync_budget = crate::sync::project_status(context, &output);
    output.healthy =
        compatibility_complete && output.analysis.healthy() && output.initialization.ready;

    let total = started.elapsed();
    if (!bounded_compatibility || compatibility_complete)
        && std::time::Instant::now() >= operation_deadline
    {
        return Err(AppError::structured(
            ErrorCategory::Timeout,
            "github_discovery_timeout",
            "status deadline expired while finalizing status and paused-caravan projection",
            Some(json!({
                "stage": "github_discovery",
                "phase": "finalize_status",
                "elapsed_ms": u64::try_from(total.as_millis()).unwrap_or(u64::MAX),
                "deadline_ms": u64::try_from(operation_budget.as_millis()).unwrap_or(u64::MAX),
                "retryable": true,
                "safe_next_action": "retry `cara status`; status made no mutations",
            })),
        ));
    }
    let millis =
        |duration: std::time::Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    let post_analysis_reserve = if bounded_compatibility {
        STATUS_POST_ANALYSIS_RESERVE
    } else {
        Duration::ZERO
    };
    output.timing = Some(StatusTiming {
        deadline_ms: millis(operation_budget),
        total_ms: millis(total),
        completion_reserve_ms: millis(if bounded_compatibility {
            STATUS_COMPLETION_RESERVE.saturating_add(STATUS_POST_ANALYSIS_RESERVE)
        } else {
            Duration::ZERO
        }),
        compatibility_budget_ms: millis(
            operation_budget
                .saturating_sub(label_inventory_elapsed)
                .saturating_sub(post_analysis_reserve),
        ),
        compatibility_analysis: compatibility_progress,
        phases_ms: std::collections::BTreeMap::from([
            ("github_discovery".to_owned(), millis(discovery_elapsed)),
            (
                "repository_label_inventory".to_owned(),
                millis(label_inventory_elapsed.saturating_sub(discovery_elapsed)),
            ),
            (
                "compatibility_analysis".to_owned(),
                millis(analysis_elapsed.saturating_sub(label_inventory_elapsed)),
            ),
            (
                "provider_identity".to_owned(),
                millis(provider_identity_elapsed.saturating_sub(analysis_elapsed)),
            ),
            (
                "stack_and_admission_projection".to_owned(),
                millis(projection_elapsed.saturating_sub(provider_identity_elapsed)),
            ),
            (
                "paused_caravan_projection".to_owned(),
                millis(total.saturating_sub(projection_elapsed)),
            ),
        ]),
    });
    Ok(output)
}

/// One actionable admission instruction with the reason it was chosen.
///
/// bd-d7aae7: `next-candidate` already succeeds deterministically and the
/// attempt contract already says to preflight the exact first candidate, but
/// nothing surfaced a single command. Operators therefore read a correct FIFO
/// rejection of a later PR as a queue fault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NextAdmissionCommand {
    /// Exact `cara` invocation to run next.
    pub command: String,
    /// Why this is the next step.
    pub reason: String,
    /// Canonical candidate the command targets, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrNumber>,
}

fn next_admission_command(admission: &AdmissionStatus) -> NextAdmissionCommand {
    admission.next_candidate.map_or_else(
        || {
            let blocked = admission
                .rejected
                .iter()
                .find(|candidate| candidate.blocks_order);
            blocked.map_or_else(
                || NextAdmissionCommand {
                    command: "cara status".to_owned(),
                    reason: "no candidate is currently selectable for automatic admission"
                        .to_owned(),
                    pr: None,
                },
                |blocked| NextAdmissionCommand {
                    command: format!("cara check --pr {}", blocked.pr),
                    reason: format!(
                        "PR #{} blocks canonical order and must be resolved first: {}",
                        blocked.pr, blocked.reason
                    ),
                    pr: Some(blocked.pr),
                },
            )
        },
        |pr| NextAdmissionCommand {
            command: format!("cara check --pr {pr}"),
            reason: format!(
                "PR #{pr} is the canonical first automatic-admission attempt; preflight it before any membership mutation"
            ),
            pr: Some(pr),
        },
    )
}

/// Return the canonical first automatic-admission candidate without mutation.
pub fn next_candidate(context: &AppContext) -> Result<NextCandidateOutput, AppError> {
    // Which PR is next is a property of the provider graph, never of the local
    // checkout, so this read tolerates a merged or ambiguous current branch.
    let budget = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let status = fleet_status(context, std::time::Instant::now() + budget, None)?;
    // The automatic surface is FIFO-bound for both intents. Emitting the typed
    // decision here keeps it comparable with, and distinct from, the explicit
    // owner intent recorded on a `cara check --pr N` receipt.
    let automatic_selection = status
        .admission
        .next_candidate
        .and_then(|pr| status.analysis.pull_requests.get(&pr))
        .map(|candidate| {
            crate::admission::evaluate(
                &status.admission,
                &status.analysis,
                candidate,
                None,
                crate::admission::AdmissionSelection::Automatic,
            )
        });
    let next_admission_command = Some(next_admission_command(&status.admission));
    Ok(NextCandidateOutput {
        provider_api: status.provider_api,
        repository: status.repository,
        next_admission_command,
        attempt_contract: "ordered manual admission attempt only; run `cara check --pr N` for this exact first candidate; on rejection fail closed rather than leapfrogging. The separate opt-in sync-owned greedy policy may persist an exact generation-bound mechanical skip before considering a later candidate".to_owned(),
        admission: status.admission,
        automatic_selection,
    })
}

fn apply_generation_graph_problems(
    analysis: &mut GraphAnalysis,
    integrity: &crate::generation::GenerationIntegrityStatus,
) {
    for finding in &integrity.findings {
        if finding.disposition == crate::generation::GenerationDisposition::CurrentGeneration
            || !analysis
                .pull_requests
                .get(&finding.pr)
                .is_some_and(PullRequestSnapshot::is_active_caravan_member)
        {
            continue;
        }
        let kind = match finding.disposition {
            crate::generation::GenerationDisposition::CurrentGeneration => continue,
            crate::generation::GenerationDisposition::SupersededGeneration => {
                GraphProblemKind::SupersededGeneration
            }
            crate::generation::GenerationDisposition::AmbiguousGeneration => {
                GraphProblemKind::AmbiguousGeneration
            }
            crate::generation::GenerationDisposition::InvalidGenerationMetadata => {
                GraphProblemKind::InvalidGenerationMetadata
            }
        };
        analysis.fleet.problems.push(GraphProblem {
            kind,
            prs: finding.related_prs.clone(),
            message: finding.reason.clone(),
        });
    }
}

/// Resolve configured explicit priority and FIFO from one GitHub snapshot.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn resolve_admission(analysis: &GraphAnalysis, priority_labels: &[String]) -> AdmissionStatus {
    resolve_admission_with_generation(
        analysis,
        priority_labels,
        crate::generation::GenerationIntegrityStatus::default(),
    )
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn resolve_admission_with_generation(
    analysis: &GraphAnalysis,
    priority_labels: &[String],
    generation_integrity: crate::generation::GenerationIntegrityStatus,
) -> AdmissionStatus {
    let ranks: std::collections::BTreeMap<&str, usize> = priority_labels
        .iter()
        .enumerate()
        .map(|(rank, label)| (label.as_str(), rank))
        .collect();
    let mut candidates = Vec::new();
    let mut skipped = Vec::new();
    let mut rejected = Vec::new();

    for number in &analysis.fleet.unqueued {
        let Some(pull_request) = analysis.pull_requests.get(number) else {
            continue;
        };
        let priority_namespace: Vec<&String> = pull_request
            .labels
            .iter()
            .filter(|label| label.starts_with("caravan-priority:"))
            .collect();
        let configured: Vec<(&String, usize)> = priority_namespace
            .iter()
            .filter_map(|label| ranks.get(label.as_str()).map(|rank| (*label, *rank)))
            .collect();
        let invalid: Vec<&String> = priority_namespace
            .iter()
            .copied()
            .filter(|label| !ranks.contains_key(label.as_str()))
            .collect();

        let generation_finding = generation_integrity.finding(*number);
        // A structurally ineligible PR is not an admission attempt at all, so it
        // is reported with exact evidence but never becomes the canonical head
        // of the queue. Only rank-indeterminate priority metadata blocks order,
        // because canonical position genuinely cannot be computed. Eligible
        // candidates whose exact mechanical attempt fails still cannot be
        // leapfrogged: sync owns that generation-bound skip receipt.
        let generation_rejection = generation_finding.and_then(|finding| {
            (finding.disposition != crate::generation::GenerationDisposition::CurrentGeneration)
                .then(|| (finding.reason.clone(), false))
        });
        let rejection = if let Some((reason, blocks_order)) = generation_rejection {
            Some((format!("fail closed: {reason}"), blocks_order))
        } else if !invalid.is_empty() {
            Some((
                format!(
                    "fail closed: unknown priority label(s): {}",
                    invalid
                        .iter()
                        .map(|label| label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                true,
            ))
        } else if configured.len() > 1 {
            Some((
                format!(
                    "fail closed: conflicting priority labels: {}",
                    configured
                        .iter()
                        .map(|(label, _)| label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                true,
            ))
        } else if pull_request.draft {
            Some((
                "not an admission attempt: draft PR must be marked ready first".to_owned(),
                false,
            ))
        } else if pull_request.cross_repository {
            Some((
                "not an admission attempt: fork-only PR cannot be admitted to a caravan".to_owned(),
                false,
            ))
        } else if pull_request.auto_merge.enabled {
            Some((
                "not an admission attempt: candidate already has externally enabled auto-merge"
                    .to_owned(),
                false,
            ))
        } else {
            None
        };
        if let Some((reason, blocks_order)) = rejection {
            rejected.push(RejectedAdmissionCandidate {
                pr: *number,
                // Unknown priority metadata blocks before known attempts: its
                // rank cannot safely be guessed. Otherwise preserve the same
                // configured rank/FIFO key as selectable candidates.
                priority_rank: (configured.len() == 1).then(|| configured[0].1 + 1),
                created_at: pull_request.created_at.clone(),
                blocks_order,
                reason,
            });
            continue;
        }

        let created_at = pull_request.created_at.clone();
        // A red candidate is skipped, not elected, and specifically NOT treated as
        // an order-blocking rejection. Queueing behind red is guaranteed rework:
        // the failure must be fixed, fixing it rewrites the head, and everything
        // stacked behind is rebased anyway. So the queue advances past it and a
        // clean candidate can form the caravan instead of waiting on a red head.
        //
        // This mirrors `validate_candidate`, which refuses the same shape on the
        // `check` path. Both must agree: when only `check` refused, `cara status`
        // still elected the red PR as next_candidate while `cara check --pr N`
        // called it ineligible, and the two surfaces disagreed about the same PR
        // (observed on cacophony PR 2276).
        if !pull_request.has_label("caravan-force") && has_failing_check(pull_request) {
            skipped.push(SkippedAdmissionCandidate {
                pr: *number,
                priority_label: configured.first().map(|(label, _)| (*label).clone()),
                priority_rank: configured.first().map(|(_, rank)| rank + 1),
                created_at,
                reason: "candidate has a failing required check; queueing behind red only guarantees a later rebase, so the queue advances until it is fixed".to_owned(),
            });
            continue;
        }
        if pull_request.has_label("caravan-join-skipped") {
            skipped.push(SkippedAdmissionCandidate {
                pr: *number,
                priority_label: configured.first().map(|(label, _)| (*label).clone()),
                priority_rank: configured.first().map(|(_, rank)| rank + 1),
                created_at,
                // Do not promise revalidation. That holds only when Cara wrote
                // the label AND a matching receipt survives. With the label
                // present and NO receipt, bd-239640 correctly refuses removal,
                // because an absent receipt is indistinguishable from a
                // deliberate operator hold, and the candidate is excluded
                // permanently. Both states reach this line identically, and the
                // receipt lives in PR comments the status snapshot does not
                // carry, so this read cannot tell them apart. State what is
                // known rather than the outcome that is merely likely
                // (bd-3fc019).
                reason: "carries the automatic skip label; Cara removes it only where it can prove it wrote it, so a label with no receipt stays skipped until removed by hand".to_owned(),
            });
            continue;
        }
        // A candidate Cara has already proven cannot merge into the exact
        // default branch is skipped rather than elected. Electing it starves
        // every clean candidate behind it, and no rerun resolves it: only its
        // owner can. This is deliberately NOT the same as the rejected-attempt
        // rule below, which keeps an owner's explicit `cara check --pr N`
        // rejection canonical. A mechanical conflict is not a decision anyone
        // made, so it must not inherit a decision's blocking authority.
        if analysis.fleet.problems.iter().any(|problem| {
            // Must match the kind graph analysis actually emits for an
            // unadmitted candidate. This guard silently died once already by
            // still naming `Incompatible` after the producer moved to
            // `CandidateIncompatible`, which reinstated the head-of-line stall
            // with every test still green.
            problem.kind.is_candidate_scoped() && problem.prs.contains(number)
        }) {
            skipped.push(SkippedAdmissionCandidate {
                pr: *number,
                priority_label: configured.first().map(|(label, _)| (*label).clone()),
                priority_rank: configured.first().map(|(_, rank)| rank + 1),
                created_at,
                reason: "proven mechanically incompatible with the exact current default branch; the owner must reconcile it, and no rerun changes that".to_owned(),
            });
            continue;
        }
        let fifo_reason = created_at.as_ref().map_or_else(
            || format!("provider created_at missing; deterministic PR number #{number} fallback"),
            |created_at| {
                format!("immutable provider created_at {created_at}, PR number #{number} tie-break")
            },
        );
        let (priority_label, priority_rank, reason) = configured.first().map_or_else(
            || {
                (
                    None,
                    None,
                    format!(
                        "no explicit agent priority; FIFO by {fifo_reason}; selection only, check/new preflight required"
                    ),
                )
            },
            |(label, rank)| {
                (
                    Some((*label).clone()),
                    Some(rank + 1),
                    format!(
                        "explicit agent priority `{label}` (rank {}); FIFO by {fifo_reason} within this priority; selection only, check/new preflight required",
                        rank + 1
                    ),
                )
            },
        );
        candidates.push(AdmissionCandidate {
            pr: *number,
            priority_label,
            priority_rank,
            created_at,
            reason,
        });
    }

    // An absent explicit priority sorts after every configured rank. GitHub's
    // immutable creation timestamp is FIFO; PR number deterministically breaks
    // equal timestamps. Missing timestamps form a deterministic fallback group
    // after provider-timestamped candidates, ordered by PR number.
    candidates.sort_by_key(|candidate| {
        (
            candidate.priority_rank.unwrap_or(priority_labels.len() + 1),
            candidate.created_at.is_none(),
            candidate.created_at.clone().unwrap_or_default(),
            candidate.pr,
        )
    });
    skipped.sort_by_key(|candidate| {
        (
            candidate.priority_rank.unwrap_or(priority_labels.len() + 1),
            candidate.created_at.is_none(),
            candidate.created_at.clone().unwrap_or_default(),
            candidate.pr,
        )
    });
    rejected.sort_by_key(|candidate| {
        (
            candidate.priority_rank.unwrap_or(priority_labels.len() + 1),
            candidate.created_at.is_none(),
            candidate.created_at.clone().unwrap_or_default(),
            candidate.pr,
        )
    });
    // Select from candidates and rejected attempts together. A rejected first
    // attempt remains canonical and therefore cannot be silently leapfrogged.
    let next_candidate = candidates
        .iter()
        .map(|candidate| {
            (
                candidate.priority_rank.unwrap_or(priority_labels.len() + 1),
                candidate.created_at.is_none(),
                candidate.created_at.clone().unwrap_or_default(),
                candidate.pr,
            )
        })
        .chain(
            rejected
                .iter()
                .filter(|candidate| candidate.blocks_order)
                .map(|candidate| {
                    (
                        candidate.priority_rank.unwrap_or(priority_labels.len() + 1),
                        candidate.created_at.is_none(),
                        candidate.created_at.clone().unwrap_or_default(),
                        candidate.pr,
                    )
                }),
        )
        .min()
        .map(|(_, _, _, pr)| pr);
    AdmissionStatus {
        policy: "Cacophony-shaped PRs first require unique current generation integrity. Structurally ineligible PRs (draft, fork-only, externally enabled auto-merge, superseded/ambiguous/invalid generation) are reported with exact reasons and excluded from ordering rather than wedging the queue, while unknown or conflicting configured priority labels block because canonical rank cannot be computed. Remaining attempts use explicit agent priority label (configured high to low), then FIFO by immutable provider created_at ascending with PR number ascending as equal-time tie-break; missing created_at falls back deterministically to PR number after timestamped peers; never LIFO; check/new preflight required and an eligible candidate whose exact mechanical attempt fails never causes automatic leapfrogging. This ordering binds automatic selection for both new-caravan and join intent; explicit owner intent naming one exact PR is evaluated separately and may attach ahead of unrelated unjoined rows without changing their canonical order".to_owned(),
        priority_labels: priority_labels.to_vec(),
        generation_integrity,
        candidates,
        skipped,
        rejected,
        next_candidate,
    }
}

/// Return the unique merged Caravan PR named by the local branch when the
/// effective current PR is its active rolling successor.
#[must_use]
pub fn historical_predecessor(status: &StatusOutput) -> Option<PrNumber> {
    let branch = status.current_branch.as_deref()?;
    let mut matches = status.analysis.pull_requests.values().filter(|pull| {
        pull.state == PullRequestState::Merged
            && pull.has_label("caravan")
            && pull.head.repository == status.repository
            && !pull.cross_repository
            && pull.head.name == branch
    });
    let predecessor = matches.next()?.number;
    matches.next().is_none().then_some(predecessor)
}

/// Show the current branch's active caravan and position.
pub fn show(context: &AppContext) -> Result<ShowOutput, AppError> {
    let status = status(context)?;
    let current_pr = status.current_pr.ok_or_else(|| {
        if let Some(predecessor) = historical_predecessor(&status) {
            AppError::structured(
                ErrorCategory::TargetNotFound,
                "historical_successor_not_found",
                format!("merged Caravan PR #{predecessor} has no unique active rolling successor"),
                Some(json!({
                    "historical_predecessor": predecessor,
                    "current_branch": status.current_branch,
                    "fail_closed": true,
                })),
            )
        } else {
            AppError::validation(
                "current_pr_not_found",
                "the current branch has no unique open GitHub pull request",
            )
        }
    })?;
    let caravan = status
        .analysis
        .fleet
        .containing(current_pr)
        .cloned()
        .ok_or_else(|| {
            AppError::validation(
                "current_pr_not_in_caravan",
                format!("PR #{current_pr} is not an active caravan member"),
            )
        })?;
    let position = caravan
        .position(current_pr)
        .expect("containing caravan includes current PR");
    let pull_requests = caravan
        .members
        .iter()
        .filter_map(|number| status.analysis.pull_requests.get(number).cloned())
        .collect();
    let merge_candidates = status
        .merge_candidates
        .iter()
        .filter(|candidate| caravan.members.contains(&candidate.pr))
        .cloned()
        .collect();
    let historical_predecessor = historical_predecessor(&status);
    let historical_pull_request = historical_predecessor
        .and_then(|number| status.analysis.pull_requests.get(&number).cloned());
    Ok(ShowOutput {
        historical_predecessor,
        historical_pull_request,
        repository: status.repository,
        current_pr,
        caravan,
        position,
        pull_requests,
        merge_candidates,
        healthy: status.healthy,
        problems: status.analysis.fleet.problems,
    })
}

/// Check active health or proposed new/join eligibility without mutation.
pub fn check(context: &AppContext, input: &CheckInput) -> Result<CheckOutput, AppError> {
    if input.tail_pr.is_some() && input.head_pr.is_some() {
        return Err(AppError::validation(
            "ambiguous_target",
            "--tail-pr and --head-pr are mutually exclusive",
        ));
    }
    let status = input.pr.map_or_else(
        || status(context),
        |number| status_for_remote_candidate(context, PrNumber(number)),
    )?;
    let checker = GitCompatibilityChecker::new(&context.repository_path, "origin").with_timeout(
        std::time::Duration::from_secs(context.config.command_timeout_secs),
    );
    check_analysis(&status, input, &checker)
}

fn duration_millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// One fleet snapshot with a fresh deadline reserved for exact candidate work.
pub(crate) struct BoundRemoteCandidateStatus {
    pub status: StatusOutput,
    pub exact_deadline: std::time::Instant,
}

/// Rediscover after one successful physical branch push while binding the
/// candidate to the exact new generation from the lease receipt.
///
/// GitHub's complete open-PR list can briefly report the previous head even
/// after the exact ref endpoint and Git advertisement expose the pushed head.
/// This path verifies the branch ref against `expected_oid`, replaces only that
/// candidate generation before compatibility analysis, and leaves every later
/// compatibility/provider precondition to reverify the same new head.
pub(crate) fn status_after_branch_rewrite_with_deadline(
    context: &AppContext,
    number: PrNumber,
    branch: &str,
    expected_oid: &crate::model::CommitOid,
    discovery_deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<BoundRemoteCandidateStatus, AppError> {
    let exact_budget = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let expected = ExpectedCandidateHead {
        pr: number,
        branch: branch.to_owned(),
        oid: expected_oid.clone(),
    };
    let status = status_with_discovery_options(
        context,
        discovery_deadline,
        github_budget,
        false,
        true,
        Some(&expected),
        false,
    )?;
    Ok(BoundRemoteCandidateStatus {
        status,
        exact_deadline: std::time::Instant::now() + exact_budget,
    })
}

/// Discover the fleet and bind one exact remote candidate without checkout mutation.
pub(crate) fn status_for_remote_candidate(
    context: &AppContext,
    number: PrNumber,
) -> Result<StatusOutput, AppError> {
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(context.config.command_timeout_secs);
    Ok(status_for_remote_candidate_with_deadline(context, number, deadline, None)?.status)
}

/// Discover under the caller's fleet deadline, then reserve a fresh exact-candidate budget.
pub(crate) fn status_for_remote_candidate_with_deadline(
    context: &AppContext,
    number: PrNumber,
    discovery_deadline: std::time::Instant,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<BoundRemoteCandidateStatus, AppError> {
    let status = status_with_deadline_and_budget(context, discovery_deadline, github_budget)?;
    bind_remote_candidate_from_status(context, status, number, github_budget)
}

/// Bind one selected PR from already-discovered fleet facts under a fresh budget.
/// This avoids repeating unrelated fleet compatibility during sync-owned admission.
pub(crate) fn bind_remote_candidate_from_status(
    context: &AppContext,
    mut status: StatusOutput,
    number: PrNumber,
    github_budget: Option<&crate::command::GithubRequestBudget>,
) -> Result<BoundRemoteCandidateStatus, AppError> {
    let exact_budget = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let exact_started = std::time::Instant::now();
    let exact_deadline = exact_started + exact_budget;
    // Re-read the selected provider PR after fleet discovery. Comparing the
    // complete snapshot closes the discovery/refetch race before exact Git
    // revision checks; merge compatibility subsequently verifies refs both
    // before and after its fetch.
    let provider_runner = crate::command::ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(exact_budget)
        .with_operation_deadline(exact_deadline);
    let provider_runner = github_budget.map_or(provider_runner.clone(), |budget| {
        provider_runner.with_github_request_budget(budget.clone())
    });
    let provider = crate::github::GitHubMutationAdapter::new(provider_runner.clone());
    let fresh = provider
        .refetch_pull_request(&status.repository, number)
        .map_err(|error| {
            let timeout = matches!(
                &error,
                crate::github::MutationError::Provider(DiscoveryError::Runner(
                    CommandRunError::Timeout { .. }
                ))
            );
            AppError::structured(
                if timeout {
                    ErrorCategory::Timeout
                } else {
                    ErrorCategory::ExecutionFailure
                },
                if timeout {
                    "candidate_refetch_timeout"
                } else {
                    "candidate_refetch_failed"
                },
                error.to_string(),
                Some(json!({
                    "stage": "exact_candidate",
                    "phase": "provider_refetch",
                    "pr": number,
                    "elapsed_ms": duration_millis(exact_started.elapsed()),
                    "deadline_ms": duration_millis(exact_budget),
                    "safe_next_action": "retry the read-only exact-candidate preflight",
                    "mutated": false,
                })),
            )
        })?;
    let refetch_elapsed = exact_started.elapsed();
    let discovered = status.analysis.pull_requests.get(&number).ok_or_else(|| {
        AppError::validation(
            "candidate_not_found",
            format!("PR #{number} is not an open provider candidate"),
        )
    })?;
    require_fresh_candidate(discovered, &fresh)?;
    let identity = GitHubDiscovery::new(provider_runner.clone())
        .merge_candidate_identity(
            &status.repository,
            &fresh,
            Some(&status.analysis.fleet.default_branch),
        )
        .map_err(|error| discovery_error(&error))?;
    status
        .merge_candidates
        .retain(|candidate| candidate.pr != number);
    status.merge_candidates.push(identity);
    status.current_pr = Some(number);
    status
        .provider_api
        .merge(provider_runner.github_api_telemetry());
    let exact_elapsed = exact_started.elapsed();
    let timing = status.timing.get_or_insert_with(StatusTiming::default);
    timing.deadline_ms = timing
        .deadline_ms
        .saturating_add(duration_millis(exact_budget));
    timing.total_ms = timing
        .total_ms
        .saturating_add(duration_millis(exact_elapsed));
    timing.phases_ms.insert(
        "exact_candidate_provider_refetch".to_owned(),
        duration_millis(refetch_elapsed),
    );
    timing.phases_ms.insert(
        "exact_candidate_merge_identity".to_owned(),
        duration_millis(exact_elapsed.saturating_sub(refetch_elapsed)),
    );
    Ok(BoundRemoteCandidateStatus {
        status,
        exact_deadline,
    })
}

fn require_fresh_candidate(
    discovered: &PullRequestSnapshot,
    fresh: &PullRequestSnapshot,
) -> Result<(), AppError> {
    // Check churn, provider timestamps, title, and URL do not invalidate an
    // identity preflight. Every operation-shaping fact does.
    let identity_matches = discovered.number == fresh.number
        && discovered.state == fresh.state
        && discovered.draft == fresh.draft
        && discovered.head == fresh.head
        && discovered.base == fresh.base
        && discovered.cross_repository == fresh.cross_repository
        && discovered.labels == fresh.labels
        && discovered.auto_merge == fresh.auto_merge;
    if identity_matches {
        return Ok(());
    }
    Err(AppError::structured(
        ErrorCategory::Validation,
        "stale_candidate_snapshot",
        format!("PR #{} changed during remote preflight", discovered.number),
        Some(json!({
            "pr": discovered.number,
            "expected_head_oid": discovered.head.oid,
            "actual_head_oid": fresh.head.oid,
            "expected": discovered,
            "actual": fresh,
            "safe_next_action": "retry `cara check --pr` to preflight the new exact provider state",
            "mutated": false,
        })),
    ))
}

/// Who chose the candidate a `check` receipt describes.
///
/// Naming an exact remote PR (`cara check --pr N`, with or without
/// `--tail-pr`/`--head-pr`) is deliberate owner intent for both `new` and
/// `join`. Operating on the checked-out PR is the owner's own PR, where
/// canonical position is evidence only. Neither is the automatic priority/FIFO
/// selection that binds sync and `next-candidate`.
const fn admission_selection(remote: bool) -> crate::admission::AdmissionSelection {
    if remote {
        crate::admission::AdmissionSelection::Explicit
    } else {
        crate::admission::AdmissionSelection::CheckedOut
    }
}

/// Pure/injectable recommendation policy used by `cara check` and fixture tests.
///
/// With no explicit target, a check may recommend joining the one visible,
/// unheld caravan when that exact attachment is eligible. Mutation preflights
/// use [`check_requested_action_analysis`] instead: an explicit `cara new`
/// must continue to prove `new`, never inherit the check command's recommendation.
#[allow(clippy::too_many_lines)]
pub fn check_analysis(
    status: &StatusOutput,
    input: &CheckInput,
    checker: &impl CompatibilityChecker,
) -> Result<CheckOutput, AppError> {
    check_analysis_with_recommendation(status, input, checker, true)
}

/// Prove the action the caller explicitly requested without inferring another
/// membership operation. This is the mutation-side counterpart to
/// [`check_analysis`].
pub(crate) fn check_requested_action_analysis(
    status: &StatusOutput,
    input: &CheckInput,
    checker: &impl CompatibilityChecker,
) -> Result<CheckOutput, AppError> {
    check_analysis_with_recommendation(status, input, checker, false)
}

#[allow(clippy::too_many_lines)]
fn check_analysis_with_recommendation(
    status: &StatusOutput,
    input: &CheckInput,
    checker: &impl CompatibilityChecker,
    recommend_implicit_join: bool,
) -> Result<CheckOutput, AppError> {
    crate::initialization::require_ready(&status.initialization)?;
    let current_pr = input.pr.map(PrNumber).or(status.current_pr).ok_or_else(|| {
        AppError::validation(
            "current_pr_not_found",
            "the selected remote PR was not found and the current branch has no unique open GitHub pull request",
        )
    })?;
    let pull_request = status
        .analysis
        .pull_requests
        .get(&current_pr)
        .ok_or_else(|| {
            AppError::validation(
                "current_pr_missing_from_snapshot",
                format!("PR #{current_pr} was not included in discovery"),
            )
        })?;
    let canonical_candidate = status.admission.next_candidate == Some(current_pr)
        || (input.pr.is_some() && pull_request.draft && status.admission.next_candidate.is_none());
    let admission_rejection = status
        .admission
        .rejected
        .iter()
        .find(|candidate| candidate.pr == current_pr);
    let merge_candidate = status
        .merge_candidates
        .iter()
        .find(|candidate| candidate.pr == current_pr)
        .cloned();
    let candidate_stale = merge_candidate.as_ref().is_some_and(|candidate| {
        candidate.freshness != crate::model::MergeCandidateFreshness::Fresh
    });
    let remote = input.pr.is_some();

    if let Some(caravan) = status.analysis.fleet.containing(current_pr) {
        if (input.tail_pr.is_some() || input.head_pr.is_some()) && !remote {
            return Err(AppError::validation(
                "active_pr_cannot_join",
                format!("PR #{current_pr} is already in caravan #{}", caravan.id),
            ));
        }
        // Active-member checks inspect the active fleet, not quarantined parked
        // caravans. A check on the parked member itself still includes its own
        // exact problems, while an unrelated active caravan is not assigned the
        // parked head's repair work.
        let mut active_problems = status
            .analysis
            .fleet
            .problems
            .iter()
            .filter(|problem| {
                status.analysis.problem_blocks_active_fleet(problem)
                    || problem.prs.contains(&current_pr)
            })
            .cloned()
            .collect::<Vec<_>>();
        if input.tail_pr.is_some() || input.head_pr.is_some() {
            active_problems.push(GraphProblem {
                kind: GraphProblemKind::Unknown,
                prs: vec![current_pr],
                message: format!("candidate is already enrolled in caravan #{}", caravan.id),
            });
        }
        if candidate_stale {
            active_problems.push(GraphProblem {
                kind: GraphProblemKind::Unknown,
                prs: vec![current_pr],
                message: "provider merge-candidate identity is stale or incomplete; wait for the current generation".to_owned(),
            });
        }
        let eligible = status.healthy && active_problems.is_empty();
        let mut enrolled_intent = crate::admission::evaluate(
            &status.admission,
            &status.analysis,
            pull_request,
            None,
            admission_selection(remote),
        );
        enrolled_intent.record_preflight(true, eligible);
        let output = CheckOutput {
            provider_api: status.provider_api.clone(),
            rebase_on_join: status.rebase_on_join.clone(),
            mode: CheckMode::ActiveCaravan,
            current_pr,
            candidate: pull_request.clone(),
            head_repository_owner: pull_request.head.repository.owner.clone(),
            merge_candidate,
            enrolled: true,
            canonical_candidate,
            admission_note: None,
            admission_intent: Some(enrolled_intent),
            next_action: if input.tail_pr.is_some() || input.head_pr.is_some() {
                CandidateNextAction::Reject
            } else if candidate_stale || eligible {
                CandidateNextAction::Wait
            } else {
                CandidateNextAction::Repair
            },
            caravan_id: Some(caravan.id),
            target_pr: None,
            eligible,
            compatibility: status.analysis.compatibility.clone(),
            squash_reconciliations: status.analysis.squash_reconciliations.clone(),
            problems: active_problems,
            initialization: status.initialization.clone(),
        };
        return eligible_or_error(output, remote);
    }

    let explicit_join = input.tail_pr.is_some() || input.head_pr.is_some();
    // A targetless check is a recommendation request, not an explicit `new`.
    // Recommend a join only when the downstream targetless `cara join --pr N`
    // can resolve the same target unambiguously. The targeted recursive pass
    // disables recommendation, so it evaluates the existing coherent join path
    // exactly once. An ineligible join is evidence to try `new`, not the final
    // receipt; every other error still fails closed.
    if recommend_implicit_join && !explicit_join {
        let active_caravans = status
            .analysis
            .fleet
            .caravans
            .iter()
            .filter(|caravan| !caravan.parked)
            .collect::<Vec<_>>();
        if let [target_caravan] = active_caravans.as_slice() {
            let held = status.pauses.iter().any(|pause| {
                pause.state.is_effective() && pause.record.caravan_head == target_caravan.id
            });
            if !held {
                let tail = target_caravan.tail().expect("caravans are non-empty");
                let targeted = CheckInput {
                    pr: input.pr,
                    tail_pr: Some(tail.0),
                    head_pr: None,
                };
                match check_analysis_with_recommendation(status, &targeted, checker, false) {
                    Ok(output) if output.eligible => return Ok(output),
                    Ok(_) => {}
                    Err(error) if error.code() == "check_failed" => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }

    // Seed from active-fleet problems, but never inherit another *unadmitted*
    // candidate's incompatibility or a parked caravan's quarantined repair
    // state. Both remain exact status/scheduler evidence, but neither is an
    // admission target and neither may make an independent clean candidate look
    // like it needs repair.
    let mut problems = status
        .analysis
        .fleet
        .problems
        .iter()
        .filter(|problem| {
            status.analysis.problem_blocks_active_fleet(problem)
                || problem.prs.contains(&current_pr)
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut ordering_note: Option<String> = None;
    validate_candidate(pull_request, &mut problems);
    // In physical membership mode the provider's synthetic merge ref is
    // advisory only: prepare/apply independently fetch and verify the exact PR
    // head plus current default/tail, prove merge-tree compatibility and range
    // topology, and push under force-with-lease. Requiring a fresh synthetic
    // parent here can deadlock the rewrite which would refresh that ref. Virtual
    // mode retains strict provider-candidate freshness.
    let candidate_stale_blocks_admission = candidate_stale && !status.rebase_on_join.enabled;
    if candidate_stale_blocks_admission {
        problems.push(GraphProblem {
            kind: GraphProblemKind::Unknown,
            prs: vec![current_pr],
            message: "provider merge-candidate identity is stale or incomplete; wait for the current generation".to_owned(),
        });
    }
    if let Some(rejected) = admission_rejection {
        problems.push(GraphProblem {
            kind: GraphProblemKind::Unknown,
            prs: vec![current_pr],
            message: rejected.reason.clone(),
        });
    }
    // Intent is resolved before FIFO canonical-candidate rejection. An
    // unresolved, missing, or ambiguous join target fails closed here and never
    // reaches the ordering relaxation below.
    let target_caravan = explicit_join
        .then(|| resolve_target_caravan(status, input))
        .transpose()?;
    let mut admission_intent = crate::admission::evaluate(
        &status.admission,
        &status.analysis,
        pull_request,
        target_caravan,
        admission_selection(remote),
    );
    // Priority/FIFO binds automatic selection. Naming one exact remote PR is
    // deliberate owner intent for `new` and for `join` alike: canonical
    // position becomes evidence, and the candidate is admitted on its own
    // eligibility while every bypassed row is an unrelated unjoined
    // first-admission attempt that keeps its canonical order. A joined row, a
    // base-chain dependency, a rank-indeterminate row, or a candidate that is
    // not itself a current ordered attempt still fails closed.
    if remote && !canonical_candidate {
        let order = status.admission.next_candidate.map_or_else(
            || "no automatic priority/FIFO attempt exists".to_owned(),
            |first| format!("automatic priority/FIFO order would have selected PR #{first} first"),
        );
        // The note is derived from the same typed decision that gates the
        // receipt, so the CLI note, the decision, and mutation always agree.
        ordering_note = Some(format!(
            "explicit {} admission intent for PR #{current_pr}; {order}; {}",
            admission_intent.intent.name(),
            admission_intent.reason,
        ));
        if !admission_intent.order_permits_admission() {
            problems.push(GraphProblem {
                kind: GraphProblemKind::Unknown,
                prs: vec![current_pr],
                message: status.admission.next_candidate.map_or_else(
                    || format!("candidate is not selectable because no canonical admission attempt exists; {}", admission_intent.reason),
                    |first| format!("explicit admission intent fails closed on PR #{first}; {}", admission_intent.reason),
                ),
            });
        }
    }
    let order_admits = canonical_candidate || admission_intent.order_permits_admission();
    let mut reports = Vec::new();
    let mut reconciliations = Vec::new();

    if !explicit_join {
        if !pull_request.cross_repository {
            check_new(
                status,
                pull_request,
                checker,
                &mut reports,
                &mut problems,
                &mut reconciliations,
            )?;
        }
        let eligible = problems.is_empty();
        admission_intent.record_preflight(
            reports
                .iter()
                .all(|report| report.outcome == CompatibilityOutcome::Clean),
            eligible,
        );
        let next_action = candidate_action(
            pull_request,
            &reports,
            &CandidateActionContext {
                eligibility: if eligible {
                    ActionEligibility::Eligible
                } else {
                    ActionEligibility::Ineligible
                },
                target: ActionTarget::New,
                order: if order_admits {
                    ActionOrder::Canonical
                } else {
                    ActionOrder::NonCanonical
                },
                admission: if admission_rejection.is_some() {
                    AdmissionDecision::Rejected
                } else {
                    AdmissionDecision::Accepted
                },
                freshness: if candidate_stale_blocks_admission {
                    CandidateFreshness::Stale
                } else {
                    CandidateFreshness::Fresh
                },
            },
        );
        return eligible_or_error(
            CheckOutput {
                provider_api: status.provider_api.clone(),
                rebase_on_join: status.rebase_on_join.clone(),
                mode: CheckMode::NewCaravan,
                current_pr,
                candidate: pull_request.clone(),
                head_repository_owner: pull_request.head.repository.owner.clone(),
                merge_candidate,
                enrolled: false,
                canonical_candidate,
                admission_note: ordering_note.clone(),
                admission_intent: Some(admission_intent),
                next_action,
                caravan_id: Some(current_pr),
                target_pr: None,
                eligible,
                compatibility: reports,
                squash_reconciliations: reconciliations,
                problems,
                initialization: status.initialization.clone(),
            },
            remote,
        );
    }

    let target_caravan = target_caravan.expect("explicit join resolved its target");
    let tail_number = target_caravan.tail().expect("caravans are non-empty");
    let tail = status
        .analysis
        .pull_requests
        .get(&tail_number)
        .expect("derived tail has a snapshot");
    if !pull_request.cross_repository {
        record_report(
            checker,
            checker.check(&pull_request.head, &tail.head)?,
            vec![tail_number, current_pr],
            "candidate does not merge cleanly after the selected tail",
            &mut reports,
            &mut problems,
            &mut reconciliations,
        )?;
        for caravan in status
            .analysis
            .fleet
            .caravans
            .iter()
            .filter(|caravan| !caravan.parked)
        {
            if caravan.id == target_caravan.id {
                continue;
            }
            let head_number = caravan.head().expect("caravans are non-empty");
            let head = status
                .analysis
                .pull_requests
                .get(&head_number)
                .expect("derived head has a snapshot");
            record_report(
                checker,
                checker.check(&head.head, &pull_request.head)?,
                vec![head_number, current_pr],
                "another caravan head cannot attach after the proposed new tail",
                &mut reports,
                &mut problems,
                &mut reconciliations,
            )?;
        }
    }

    let eligible = problems.is_empty();
    admission_intent.record_preflight(
        reports
            .iter()
            .all(|report| report.outcome == CompatibilityOutcome::Clean),
        eligible,
    );
    let next_action = candidate_action(
        pull_request,
        &reports,
        &CandidateActionContext {
            eligibility: if eligible {
                ActionEligibility::Eligible
            } else {
                ActionEligibility::Ineligible
            },
            target: ActionTarget::Join,
            order: if order_admits {
                ActionOrder::Canonical
            } else {
                ActionOrder::NonCanonical
            },
            admission: if admission_rejection.is_some() {
                AdmissionDecision::Rejected
            } else {
                AdmissionDecision::Accepted
            },
            freshness: if candidate_stale_blocks_admission {
                CandidateFreshness::Stale
            } else {
                CandidateFreshness::Fresh
            },
        },
    );
    eligible_or_error(
        CheckOutput {
            provider_api: status.provider_api.clone(),
            rebase_on_join: status.rebase_on_join.clone(),
            mode: CheckMode::JoinTail,
            current_pr,
            candidate: pull_request.clone(),
            head_repository_owner: pull_request.head.repository.owner.clone(),
            merge_candidate,
            enrolled: false,
            canonical_candidate,
            admission_note: ordering_note.clone(),
            admission_intent: Some(admission_intent),
            next_action,
            caravan_id: Some(target_caravan.id),
            target_pr: Some(tail_number),
            eligible,
            compatibility: reports,
            squash_reconciliations: reconciliations,
            problems,
            initialization: status.initialization.clone(),
        },
        remote,
    )
}

fn check_new(
    status: &StatusOutput,
    pull_request: &PullRequestSnapshot,
    checker: &impl CompatibilityChecker,
    reports: &mut Vec<CompatibilityReport>,
    problems: &mut Vec<GraphProblem>,
    reconciliations: &mut Vec<SquashEquivalenceReport>,
) -> Result<(), AppError> {
    record_report(
        checker,
        checker.check(&pull_request.head, &status.analysis.fleet.default_branch)?,
        vec![pull_request.number],
        "candidate new head does not merge cleanly into the default branch",
        reports,
        problems,
        reconciliations,
    )?;
    for caravan in status
        .analysis
        .fleet
        .caravans
        .iter()
        .filter(|caravan| !caravan.parked)
    {
        let head_number = caravan.head().expect("caravans are non-empty");
        let tail_number = caravan.tail().expect("caravans are non-empty");
        let head = status
            .analysis
            .pull_requests
            .get(&head_number)
            .expect("derived head has a snapshot");
        let tail = status
            .analysis
            .pull_requests
            .get(&tail_number)
            .expect("derived tail has a snapshot");
        record_report(
            checker,
            checker.check(&pull_request.head, &tail.head)?,
            vec![pull_request.number, tail_number],
            "candidate head cannot attach after an existing caravan tail",
            reports,
            problems,
            reconciliations,
        )?;
        record_report(
            checker,
            checker.check(&head.head, &pull_request.head)?,
            vec![head_number, pull_request.number],
            "existing caravan head cannot attach after the candidate tail",
            reports,
            problems,
            reconciliations,
        )?;
    }
    Ok(())
}

fn resolve_target_caravan<'a>(
    status: &'a StatusOutput,
    input: &CheckInput,
) -> Result<&'a Caravan, AppError> {
    let require_active = |caravan: &'a Caravan| {
        if caravan.parked {
            Err(AppError::structured(
                ErrorCategory::Validation,
                "caravan_target_parked",
                format!(
                    "caravan #{} is parked for repair and cannot accept new members",
                    caravan.id
                ),
                Some(json!({
                    "caravan_id": caravan.id,
                    "head_pr": caravan.head(),
                    "transition": "repair the parked head until a sync tick unparks it",
                    "responsible_actor": "the parked head owner or a dispatched repair agent",
                    "alternative": "use `cara new` for independent work instead of targeting the parked caravan",
                    "mutated": false,
                })),
            ))
        } else {
            Ok(caravan)
        }
    };
    if let Some(head) = input.head_pr.map(PrNumber) {
        return status
            .analysis
            .fleet
            .caravan(head)
            .ok_or_else(|| {
                AppError::validation(
                    "caravan_head_not_found",
                    format!("PR #{head} is not a current caravan head"),
                )
            })
            .and_then(require_active);
    }
    if let Some(tail) = input.tail_pr.map(PrNumber) {
        return status
            .analysis
            .fleet
            .caravans
            .iter()
            .find(|caravan| caravan.tail() == Some(tail))
            .ok_or_else(|| {
                AppError::validation(
                    "caravan_tail_not_found",
                    format!("PR #{tail} is not a current caravan tail"),
                )
            })
            .and_then(require_active);
    }
    let active = status
        .analysis
        .fleet
        .caravans
        .iter()
        .filter(|caravan| !caravan.parked)
        .collect::<Vec<_>>();
    match active.as_slice() {
        [caravan] => Ok(caravan),
        [] => Err(AppError::validation(
            "caravan_tail_not_found",
            "there is no active caravan to join; use `cara new` while parked caravans await repair",
        )),
        caravans => Err(AppError::structured(
            ErrorCategory::Validation,
            "ambiguous_caravan_tail",
            "multiple active caravan tails exist; pass --tail-pr or --head-pr",
            Some(json!({
                "candidate_tails": caravans.iter().filter_map(|caravan| caravan.tail()).collect::<Vec<_>>(),
            })),
        )),
    }
}

fn validate_candidate(pull_request: &PullRequestSnapshot, problems: &mut Vec<GraphProblem>) {
    let mut messages = BTreeSet::new();
    if pull_request.state != PullRequestState::Open {
        messages.insert("candidate PR is not open");
    }
    if pull_request.draft {
        messages.insert("candidate PR is still a draft");
    }
    if pull_request.has_label("caravan") {
        messages.insert("candidate PR already has the caravan label");
    }
    if pull_request.has_label("caravan-evicted") {
        messages.insert("candidate PR is evicted; use renew or rejoin");
    }
    if pull_request.auto_merge.enabled {
        messages.insert("candidate PR already has auto-merge enabled");
    }
    if pull_request.cross_repository {
        messages.insert("candidate PR uses a fork-only head branch");
    }
    // Queueing behind red is guaranteed rework. A failing check has to be fixed,
    // fixing it rewrites the head, and every member stacked behind it is then
    // rebased anyway — so admitting a red candidate buys nothing and costs the
    // whole tail a re-stitch. `caravan-force` remains the audited override.
    if !pull_request.has_label("caravan-force") && has_failing_check(pull_request) {
        messages.insert(if has_cancelled_check(pull_request) {
            // Naming the cancellation is the whole point: the reader must not
            // treat this as a code failure and evict or repair on the strength
            // of it.
            "candidate PR has a cancelled check, so its failures may be consequences rather than verdicts; rerun the cancelled jobs before treating this as a code failure"
        } else {
            "candidate PR has a failing required check; fix it before admission rather than stacking work behind it"
        });
    }
    for message in messages {
        problems.push(GraphProblem {
            kind: GraphProblemKind::Unknown,
            prs: vec![pull_request.number],
            message: message.to_owned(),
        });
    }
}

/// Whether any CURRENT check on this exact head is a hard failure.
///
/// Deliberately mirrors the sync-side classification: a missing or empty
/// conclusion is `Unknown`, and Unknown counts as failure rather than success,
/// because an absent result is not a passing result.
///
/// bd-eff1dc: only the latest observation per required-check identity votes. A
/// rollup retains every historical run on the same head, so without this a
/// single old cancelled run blocks admission forever, even while the current
/// run of that same check is green or still in progress.
fn has_failing_check(pull_request: &PullRequestSnapshot) -> bool {
    let (current, _superseded) = crate::model::latest_checks_per_identity(&pull_request.checks);
    current.into_iter().any(|check| {
        matches!(
            check.state,
            crate::model::CheckState::Failure
                | crate::model::CheckState::Cancelled
                | crate::model::CheckState::TimedOut
                | crate::model::CheckState::ActionRequired
        )
    })
}

/// Whether any check was CANCELLED rather than genuinely failing.
///
/// A cancellation cascades: aggregate checks downstream of a cancelled producer
/// conclude `failure` in seconds without building or running anything, so the
/// remaining failures on such a pull request may be consequences rather than
/// verdicts. Cara cannot tell which from the rollup alone, because the aggregate
/// reports only its own conclusion. What it CAN do is say so, so whoever reads
/// the refusal knows to rerun rather than to repair or evict.
///
/// Measured by an operator across three live cases: two of the three failures
/// were spurious in exactly this way (bd-c04d9b).
fn has_cancelled_check(pull_request: &PullRequestSnapshot) -> bool {
    let (current, _superseded) = crate::model::latest_checks_per_identity(&pull_request.checks);
    current
        .into_iter()
        .any(|check| check.state == crate::model::CheckState::Cancelled)
}

/// Record one pairwise report plus, for a non-clean pair, the exact
/// squash-equivalence evidence for the same revisions.
///
/// Collecting it only on conflict keeps healthy preflights free of extra Git
/// work, and it never changes the outcome: a conflict stays a conflict until a
/// separately reviewed operation acts on the proof.
fn record_report(
    checker: &impl CompatibilityChecker,
    report: CompatibilityReport,
    prs: Vec<PrNumber>,
    message: &str,
    reports: &mut Vec<CompatibilityReport>,
    problems: &mut Vec<GraphProblem>,
    reconciliations: &mut Vec<SquashEquivalenceReport>,
) -> Result<(), AppError> {
    if report.outcome != CompatibilityOutcome::Clean {
        problems.push(GraphProblem {
            kind: GraphProblemKind::Incompatible,
            prs,
            message: message.to_owned(),
        });
        if let Some(reconciliation) =
            checker.squash_equivalence(&report.candidate, &report.target)?
        {
            reconciliations.push(reconciliation);
        }
    }
    reports.push(report);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionEligibility {
    Eligible,
    Ineligible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionTarget {
    New,
    Join,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionDecision {
    Accepted,
    Rejected,
}

/// Whether intent-aware admission ordering permits this candidate now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionOrder {
    Canonical,
    NonCanonical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateFreshness {
    Fresh,
    Stale,
}

struct CandidateActionContext {
    eligibility: ActionEligibility,
    target: ActionTarget,
    order: ActionOrder,
    admission: AdmissionDecision,
    freshness: CandidateFreshness,
}

fn candidate_action(
    candidate: &PullRequestSnapshot,
    reports: &[CompatibilityReport],
    context: &CandidateActionContext,
) -> CandidateNextAction {
    if context.order == ActionOrder::NonCanonical {
        return CandidateNextAction::Reject;
    }
    if candidate.draft || context.freshness == CandidateFreshness::Stale {
        return CandidateNextAction::Wait;
    }
    if context.admission == AdmissionDecision::Rejected {
        return CandidateNextAction::Reject;
    }
    if context.eligibility == ActionEligibility::Eligible {
        return match context.target {
            ActionTarget::Join => CandidateNextAction::Join,
            ActionTarget::New => CandidateNextAction::New,
        };
    }
    if candidate.state != PullRequestState::Open
        || candidate.cross_repository
        || candidate.has_label("caravan-evicted")
    {
        return CandidateNextAction::Reject;
    }
    if reports
        .iter()
        .any(|report| report.outcome != CompatibilityOutcome::Clean)
    {
        return CandidateNextAction::Repair;
    }
    CandidateNextAction::Repair
}

fn eligible_or_error(
    output: CheckOutput,
    return_rejection_receipt: bool,
) -> Result<CheckOutput, AppError> {
    if output.eligible || return_rejection_receipt {
        return Ok(output);
    }
    Err(AppError::structured(
        ErrorCategory::Validation,
        "check_failed",
        "the requested Caravan operation is not currently valid",
        Some(serde_json::to_value(&output).unwrap_or_else(|_| json!({}))),
    ))
}

#[allow(clippy::too_many_lines)]
fn discovery_error(error: &DiscoveryError) -> AppError {
    if let DiscoveryError::Runner(CommandRunError::OutputLimit {
        command,
        code,
        stdout,
        stderr,
    }) = error
    {
        return AppError::structured(
            ErrorCategory::ExecutionFailure,
            "command_output_limit",
            error.to_string(),
            Some(json!({
                "stage": "github_command_output",
                "command": command.display(),
                "exit_code": code,
                "stdout": stdout,
                "stderr": stderr,
                "streams_combined": false,
                "mutated": false,
                "resumable": true,
                "safe_next_action": "reduce the bounded provider query/output, then retry; do not parse truncated JSON",
            })),
        );
    }
    if let DiscoveryError::Runner(CommandRunError::GithubRequestBudgetExceeded {
        command,
        limit,
        used,
    }) = error
    {
        return AppError::structured(
            ErrorCategory::ExecutionFailure,
            "github_request_budget_exhausted",
            error.to_string(),
            Some(json!({
                "command": command.display(),
                "limit": limit,
                "used": used,
                "retryable": true,
                "mutated": false,
                "safe_next_action": "rerun the same bounded sync tick to continue from fresh provider state",
            })),
        );
    }
    if let DiscoveryError::Runner(CommandRunError::Timeout {
        command,
        timeout_ms,
        ..
    }) = error
    {
        return discovery_timeout_error(
            error,
            discovery_phase(command),
            // A single command that hit its own timeout: the phase cost and the
            // budget are the same number here, and legitimately so.
            std::time::Duration::from_millis(*timeout_ms),
            std::time::Duration::from_millis(*timeout_ms),
            std::time::Duration::from_millis(*timeout_ms),
        );
    }
    if let DiscoveryError::InvalidJson {
        command,
        message,
        evidence,
    } = error
    {
        return AppError::structured(
            ErrorCategory::ExecutionFailure,
            "github_discovery_failed",
            error.to_string(),
            Some(json!({
                "stage": "github_json_decode",
                "command": command.display(),
                "message": message,
                "stdout": evidence.stdout,
                "stderr": evidence.stderr,
                "streams_combined": false,
                "resumable": true,
                "next": "inspect the separate stdout/stderr excerpts; the message names the exact record where one could be identified. Provider output that is unusual is not necessarily malformed, so confirm the shape before treating it as corruption",
            })),
        );
    }
    if let DiscoveryError::HistoricalCurrentPullRequest {
        reason,
        branch,
        candidates,
    } = error
    {
        return AppError::structured(
            ErrorCategory::Validation,
            format!("historical_current_pr_{reason}"),
            error.to_string(),
            Some(json!({
                "historical_branch": branch,
                "candidates": candidates,
                "reason": reason,
                "fail_closed": true,
                "next": "inspect bounded same-repository PR history and restore an exact, unique retained Caravan branch before retrying",
            })),
        );
    }
    let category = match error {
        DiscoveryError::AmbiguousCurrentPullRequest { .. }
        | DiscoveryError::HistoricalCurrentPullRequest { .. }
        | DiscoveryError::ForkOnlyHead { .. }
        | DiscoveryError::InvalidLimit(_)
        | DiscoveryError::InvalidRepositorySlug(_)
        | DiscoveryError::MissingDefaultBranch
        | DiscoveryError::MissingHeadRepository { .. } => ErrorCategory::Validation,
        DiscoveryError::Runner(_)
        | DiscoveryError::CommandFailed { .. }
        | DiscoveryError::InvalidJson { .. } => ErrorCategory::ExecutionFailure,
    };
    AppError::structured(
        category,
        "github_discovery_failed",
        error.to_string(),
        Some(json!({ "error": format!("{error:?}") })),
    )
}

fn discovery_phase(command: &crate::command::CommandSpec) -> &'static str {
    let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
    if command.program == "git" {
        "current_branch"
    } else if args.starts_with(&["repo", "view"]) {
        "repository_identity"
    } else if args.starts_with(&["api"]) {
        "default_branch_revision"
    } else if args.contains(&"--head") {
        "current_pull_request"
    } else if args.windows(2).any(|pair| pair == ["--state", "merged"]) {
        "historical_caravan_members"
    } else if args.contains(&"--label") {
        "active_caravan_members"
    } else {
        "open_pull_requests_and_checks"
    }
}

/// Build the typed discovery timeout, distinguishing the phase's OWN cost from
/// the total elapsed budget.
///
/// These are different numbers and conflating them sends a reader after the
/// wrong command. The operation deadline expires during whichever phase happens
/// to be running, so reporting total elapsed under that phase's name reads as
/// "this phase took 68 seconds". It cost 0.8s; earlier phases spent the budget.
/// An operator and two agents chased `gh label list` on the strength of exactly
/// that reading (bd-1d6b1a).
fn discovery_timeout_error(
    error: &DiscoveryError,
    phase: &str,
    phase_elapsed: std::time::Duration,
    elapsed: std::time::Duration,
    deadline: std::time::Duration,
) -> AppError {
    let (command, stdout, stderr) = match error {
        DiscoveryError::Runner(CommandRunError::Timeout {
            command,
            stdout,
            stderr,
            ..
        }) => (command.display(), stdout.as_str(), stderr.as_str()),
        _ => ("unknown".to_owned(), "", ""),
    };
    AppError::structured(
        ErrorCategory::Timeout,
        "github_discovery_timeout",
        format!(
            "GitHub discovery exceeded the status deadline during phase `{phase}`; that phase itself took {}ms of {}ms total",
            phase_elapsed.as_millis(),
            elapsed.as_millis()
        ),
        Some(json!({
            "stage": "github_discovery",
            "phase": phase,
            "command": command,
            // The phase that was running when the budget ran out, NOT necessarily
            // the phase that consumed it.
            "phase_elapsed_ms": u64::try_from(phase_elapsed.as_millis()).unwrap_or(u64::MAX),
            "elapsed_ms": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            "deadline_ms": u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
            "timeout_ms": u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
            "stdout": stdout,
            "stderr": stderr,
            "retryable": true,
            "resumable": true,
            "safe_next_action": "retry `cara status` after restoring Git/GitHub transport health; status made no mutations",
            "next": "retry `cara status` after restoring Git/GitHub transport health; status made no mutations",
        })),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Clone)]
    struct LabelRunner(Arc<AtomicUsize>);

    impl crate::command::CommandRunner for LabelRunner {
        fn run(
            &self,
            _command: &crate::command::CommandSpec,
        ) -> Result<crate::command::CommandOutput, crate::command::CommandRunError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(crate::command::CommandOutput {
                code: Some(0),
                stdout: r#"[{"name":"caravan","color":"5319E7","description":"Active"}]"#
                    .to_owned(),
                stderr: String::new(),
            })
        }
    }

    struct FixedStackProvider(
        Result<crate::github::GitHubStackInventory, crate::github::GitHubStackReadError>,
    );

    impl NativeStackInventoryProvider for FixedStackProvider {
        fn native_stack_inventory(
            &self,
            _repository: &RepositoryId,
        ) -> Result<crate::github::GitHubStackInventory, crate::github::GitHubStackReadError>
        {
            self.0.clone()
        }
    }

    struct StackProviderMustNotBeCalled;

    impl NativeStackInventoryProvider for StackProviderMustNotBeCalled {
        fn native_stack_inventory(
            &self,
            _repository: &RepositoryId,
        ) -> Result<crate::github::GitHubStackInventory, crate::github::GitHubStackReadError>
        {
            panic!("the default Caravan backend must not probe GitHub Stacks")
        }
    }

    use crate::model::{AutoMergeState, BranchSnapshot, CommitOid, PullRequestState};

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "harryaskham".to_owned(),
            name: "caravan".to_owned(),
        }
    }

    fn branch(name: &str) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository(),
            name: name.to_owned(),
            oid: CommitOid(format!("{name:0<40}")),
        }
    }

    fn pr(number: u64, head: &str, base: &str, active: bool) -> PullRequestSnapshot {
        PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(number),
            title: format!("PR {number}"),
            url: format!("https://example.invalid/{number}"),
            state: PullRequestState::Open,
            draft: false,
            head: branch(head),
            base: branch(base),
            cross_repository: false,
            labels: if active {
                BTreeSet::from(["caravan".to_owned()])
            } else {
                BTreeSet::new()
            },
            // Absent configuration keeps the historical provider-native actor.
            auto_merge: if active && base == "main" {
                AutoMergeState::squash()
            } else {
                AutoMergeState::disabled()
            },
            checks: Vec::new(),
            created_at: Some(format!("2026-01-01T00:00:{number:02}Z")),
            merged_at: None,
            updated_at: None,
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn snapshot(
        current: PullRequestSnapshot,
        active: Vec<PullRequestSnapshot>,
    ) -> crate::model::RepositorySnapshot {
        let current_number = current.number;
        let mut pull_requests = active;
        if !pull_requests
            .iter()
            .any(|pull_request| pull_request.number == current_number)
        {
            pull_requests.push(current);
        }
        crate::model::RepositorySnapshot {
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            repository: repository(),
            default_branch: branch("main"),
            current_branch: Some("current".to_owned()),
            current_pr: Some(current_number),
            pull_requests,
            generation_facts: Vec::new(),
            observed_at: None,
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn status(current: PullRequestSnapshot, active: Vec<PullRequestSnapshot>) -> StatusOutput {
        let snapshot = snapshot(current, active);
        let checker = clean_checker;
        let analysis =
            analyze_for_actor(&snapshot, &checker, crate::model::HeadMergeActor::default())
                .unwrap();
        StatusOutput {
            config_provenance: None,
            head_merge: HeadMergeStatus::default(),
            runtime: crate::read::RuntimeProvenance::default(),
            provider_api: crate::model::GitHubApiTelemetry::default(),
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            timing: None,
            repository: repository(),
            rebase_on_join: RebaseOnJoinStatus::default(),
            stack_backend: StackBackendStatus::default(),
            auto_admission: AutoAdmissionStatus::default(),
            default_branch: "main".to_owned(),
            current_branch: snapshot.current_branch,
            current_pr: snapshot.current_pr,
            healthy: analysis.healthy(),
            initialization: crate::initialization::InitializationStatus::default(),
            admission: resolve_admission(
                &analysis,
                &crate::config::CaravanConfig::default().agent_priority_labels,
            ),
            analysis,
            pauses: Vec::new(),
            sync_budget: crate::sync::SyncBudgetStatus::default(),
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn clean_checker(
        candidate: &crate::model::BranchSnapshot,
        target: &crate::model::BranchSnapshot,
    ) -> Result<CompatibilityReport, AppError> {
        Ok(CompatibilityReport {
            candidate: candidate.clone(),
            target: target.clone(),
            outcome: CompatibilityOutcome::Clean,
            conflicting_paths: Vec::new(),
            diagnostic: None,
        })
    }

    struct TimeoutAfterChecks {
        calls: AtomicUsize,
        completed_before_timeout: usize,
    }

    impl CompatibilityChecker for TimeoutAfterChecks {
        fn check(
            &self,
            candidate: &crate::model::BranchSnapshot,
            target: &crate::model::BranchSnapshot,
        ) -> Result<CompatibilityReport, AppError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call >= self.completed_before_timeout {
                return Err(AppError::structured(
                    ErrorCategory::Timeout,
                    "compatibility_fixture_timeout",
                    "fixture compatibility analysis reached its bounded deadline",
                    Some(json!({"mutated": false})),
                ));
            }
            clean_checker(candidate, target)
        }
    }

    /// bd-3fc019: the skip reason must not promise an outcome this read cannot
    /// verify.
    ///
    /// A `caravan-join-skipped` label with a matching receipt is revalidated by
    /// sync. The SAME label with no receipt is refused removal by bd-239640,
    /// because an absent receipt cannot be told apart from a deliberate operator
    /// hold, so the candidate is excluded permanently. The receipt lives in PR
    /// comments the status snapshot does not carry, so both reach this line
    /// identically. Reported as recoverable, it cost thirty consecutive watch
    /// cycles of #2245, #2259 and #2314 being called correct while stuck.
    #[test]
    fn a_skip_label_does_not_promise_revalidation_this_read_cannot_verify() {
        let mut candidate = pr(2314, "stuck", "main", false);
        candidate.labels.insert("caravan-join-skipped".to_owned());
        let status = status(candidate.clone(), vec![candidate]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;
        let admission = resolve_admission(&status.analysis, &labels);
        let skipped = admission
            .skipped
            .iter()
            .find(|entry| entry.pr == PrNumber(2314))
            .expect("a labelled candidate is reported as skipped");
        assert!(
            !skipped.reason.contains("revalidates"),
            "must not promise revalidation that an absent receipt prevents: {}",
            skipped.reason
        );
        assert!(
            skipped.reason.contains("prove it wrote it"),
            "must state the condition Cara actually applies: {}",
            skipped.reason
        );
    }
    #[test]
    fn active_check_reports_whole_caravan_health() {
        let active = vec![pr(1, "one", "main", true), pr(2, "two", "one", true)];
        let status = status(active[1].clone(), active);
        let output = check_analysis(&status, &CheckInput::default(), &clean_checker).unwrap();
        assert_eq!(output.mode, CheckMode::ActiveCaravan);
        assert_eq!(output.caravan_id, Some(PrNumber(1)));
        assert!(output.eligible);
    }

    /// bd-c462db: the targetless surface is the recommendation caco consumes.
    /// It must return the complete existing join receipt, not merely flip the
    /// action on a new-caravan receipt.
    #[test]
    fn targetless_check_recommends_the_one_clean_visible_caravan() {
        let candidate = pr(9, "nine", "main", false);
        let status = status(
            candidate.clone(),
            vec![pr(1, "one", "main", true), candidate],
        );

        for input in [
            CheckInput::default(),
            CheckInput {
                pr: Some(9),
                ..CheckInput::default()
            },
        ] {
            let output = check_analysis(&status, &input, &clean_checker)
                .expect("the visible clean tail is recommended");

            assert!(output.eligible, "problems: {:?}", output.problems);
            assert_eq!(output.next_action, CandidateNextAction::Join);
            assert_eq!(output.mode, CheckMode::JoinTail);
            assert_eq!(output.caravan_id, Some(PrNumber(1)));
            assert_eq!(output.target_pr, Some(PrNumber(1)));
            let intent = output.admission_intent.expect("typed join intent");
            assert_eq!(intent.intent, crate::admission::AdmissionIntent::Join);
            assert_eq!(intent.target_caravan, Some(PrNumber(1)));
            assert_eq!(intent.target_tail, Some(PrNumber(1)));
            assert_eq!(output.compatibility.len(), 1);
            assert_eq!(output.compatibility[0].candidate.name, "nine");
            assert_eq!(output.compatibility[0].target.name, "one");
        }
    }

    /// The recommendation is not mutation intent. `cara new` uses this strict
    /// path and must keep proving a new caravan even when check would recommend
    /// the one visible tail.
    #[test]
    fn requested_new_preflight_does_not_inherit_the_join_recommendation() {
        let candidate = pr(9, "nine", "main", false);
        let status = status(
            candidate.clone(),
            vec![pr(1, "one", "main", true), candidate],
        );

        let output =
            check_requested_action_analysis(&status, &CheckInput::default(), &clean_checker)
                .expect("explicit new remains independently admissible");

        assert_eq!(output.next_action, CandidateNextAction::New);
        assert_eq!(output.mode, CheckMode::NewCaravan);
        assert_eq!(output.caravan_id, Some(PrNumber(9)));
        assert_eq!(output.target_pr, None);
        assert_eq!(
            output.admission_intent.expect("typed new intent").intent,
            crate::admission::AdmissionIntent::New
        );
    }

    /// An effective hold makes the otherwise unambiguous target unavailable to
    /// automatic recommendation. Check must preserve the independent new path.
    #[test]
    fn targetless_check_never_recommends_a_held_caravan() {
        let candidate = pr(9, "nine", "main", false);
        let mut status = status(
            candidate.clone(),
            vec![pr(1, "one", "main", true), candidate],
        );
        let head = status.analysis.pull_requests[&PrNumber(1)].clone();
        status.pauses.push(crate::pause::PauseStatus {
            record: crate::pause::PauseRecord {
                version: 1,
                caravan_head: PrNumber(1),
                members: vec![PrNumber(1)],
                expected_head: crate::model::PullRequestPrecondition::from(&head),
                expected_checks: head.checks,
                actor: "operator".to_owned(),
                reason: "incident".to_owned(),
                paused_unix_secs: 1,
                expires_unix_secs: None,
                external_reference: None,
                resume_authorized_by: None,
                recovery: None,
            },
            state: crate::pause::PauseState::Active,
            auto_merge_suspended: true,
            retired_state: None,
            safe_next_action: "resume explicitly".to_owned(),
        });

        let output = check_analysis(&status, &CheckInput::default(), &clean_checker)
            .expect("a hold keeps the independent new path available");

        assert!(output.eligible, "problems: {:?}", output.problems);
        assert_eq!(output.next_action, CandidateNextAction::New);
        assert_eq!(output.mode, CheckMode::NewCaravan);
        assert_eq!(output.target_pr, None);
    }

    /// An incompatible inferred join is not returned as the final answer. The
    /// ordinary new-caravan evaluation still owns the fallback receipt.
    #[test]
    fn rejected_implicit_join_falls_back_to_a_coherent_new_receipt() {
        let candidate = pr(9, "nine", "main", false);
        let status = status(
            candidate.clone(),
            vec![pr(1, "one", "main", true), candidate],
        );
        let checker = |candidate: &crate::model::BranchSnapshot,
                       target: &crate::model::BranchSnapshot| {
            let conflict = candidate.name == "nine" && target.name == "one";
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: if conflict {
                    CompatibilityOutcome::Conflict
                } else {
                    CompatibilityOutcome::Clean
                },
                conflicting_paths: if conflict {
                    vec!["src/lib.rs".to_owned()]
                } else {
                    Vec::new()
                },
                diagnostic: None,
            })
        };

        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(9),
                ..CheckInput::default()
            },
            &checker,
        )
        .expect("remote rejection remains an inspectable fallback receipt");

        assert!(!output.eligible);
        assert_eq!(output.next_action, CandidateNextAction::Repair);
        assert_eq!(output.mode, CheckMode::NewCaravan);
        assert_eq!(output.caravan_id, Some(PrNumber(9)));
        assert_eq!(output.target_pr, None);
        assert_eq!(
            output.admission_intent.expect("typed new fallback").intent,
            crate::admission::AdmissionIntent::New
        );
    }

    #[test]
    fn new_check_proves_both_cross_caravan_attachment_orders() {
        let candidate = pr(9, "nine", "main", false);
        let status = status(candidate, vec![pr(1, "one", "main", true)]);
        // Explicitly avoid unique-tail inference: with >1 caravans default check
        // is new; add a second caravan to exercise all ordered directions.
        let mut status = status;
        status
            .analysis
            .fleet
            .caravans
            .push(Caravan::new(vec![PrNumber(3)]).expect("second caravan"));
        status
            .analysis
            .pull_requests
            .insert(PrNumber(3), pr(3, "three", "main", true));
        let calls = std::cell::RefCell::new(Vec::new());
        let checker = |candidate: &crate::model::BranchSnapshot,
                       target: &crate::model::BranchSnapshot| {
            calls
                .borrow_mut()
                .push((candidate.name.clone(), target.name.clone()));
            clean_checker(candidate, target)
        };
        let output = check_analysis(&status, &CheckInput::default(), &checker).unwrap();
        assert_eq!(output.mode, CheckMode::NewCaravan);
        let calls = calls.into_inner();
        assert!(calls.contains(&("nine".to_owned(), "one".to_owned())));
        assert!(calls.contains(&("one".to_owned(), "nine".to_owned())));
        assert!(calls.contains(&("nine".to_owned(), "three".to_owned())));
        assert!(calls.contains(&("three".to_owned(), "nine".to_owned())));
    }

    #[test]
    fn explicit_head_resolves_join_tail() {
        let candidate = pr(9, "nine", "main", false);
        let status = status(
            candidate,
            vec![pr(1, "one", "main", true), pr(2, "two", "one", true)],
        );
        let output = check_analysis(
            &status,
            &CheckInput {
                pr: None,
                tail_pr: None,
                head_pr: Some(1),
            },
            &clean_checker,
        )
        .unwrap();
        assert_eq!(output.mode, CheckMode::JoinTail);
        assert_eq!(output.caravan_id, Some(PrNumber(1)));
        assert_eq!(output.target_pr, Some(PrNumber(2)));
    }

    #[test]
    fn failed_compatibility_is_a_nonzero_check_error() {
        let candidate = pr(9, "nine", "main", false);
        let status = status(candidate, Vec::new());
        let conflict = |candidate: &crate::model::BranchSnapshot,
                        target: &crate::model::BranchSnapshot| {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: CompatibilityOutcome::Conflict,
                conflicting_paths: vec!["src/lib.rs".to_owned()],
                diagnostic: None,
            })
        };
        let error = check_analysis(&status, &CheckInput::default(), &conflict)
            .expect_err("conflict must fail check");
        assert_eq!(mcp_cli::StructuredError::code(&error), "check_failed");
    }

    #[test]
    fn draft_candidate_fails_before_mutation() {
        let mut candidate = pr(9, "nine", "main", false);
        candidate.draft = true;
        let status = status(candidate, Vec::new());
        let error = check_analysis(&status, &CheckInput::default(), &clean_checker)
            .expect_err("drafts are not ready");
        let details = mcp_cli::StructuredError::details(&error).unwrap();
        assert_eq!(details["eligible"], false);
    }

    #[test]
    fn invalid_provider_json_preserves_separate_stream_evidence() {
        let error = discovery_error(&DiscoveryError::InvalidJson {
            command: crate::command::CommandSpec::new("gh").args(["pr", "list"]),
            message: "control character at line 1".to_owned(),
            evidence: Box::new(crate::github::JsonDecodeEvidence {
                stdout: "{\"bad\":\"\u{1}\"}".to_owned(),
                stderr: "wrapper diagnostic\u{1}".to_owned(),
            }),
        });

        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "github_discovery_failed"
        );
        let details = mcp_cli::StructuredError::details(&error).unwrap();
        assert_eq!(details["stage"], "github_json_decode");
        assert_eq!(details["streams_combined"], false);
        assert_eq!(details["stderr"], "wrapper diagnostic\u{1}");
        assert!(details["stdout"].as_str().unwrap().contains("bad"));
    }

    #[test]
    fn provider_output_limit_is_typed_before_json_decode() {
        let error = discovery_error(&DiscoveryError::Runner(CommandRunError::OutputLimit {
            command: crate::command::CommandSpec::new("gh").args(["pr", "list"]),
            code: Some(0),
            stdout: Box::new(crate::command::StreamCaptureEvidence {
                limit_bytes: 32,
                total_bytes: 100,
                truncated: true,
                prefix: "[{\"number\":".to_owned(),
                suffix: "}]".to_owned(),
            }),
            stderr: Box::new(crate::command::StreamCaptureEvidence {
                limit_bytes: 16,
                total_bytes: 0,
                truncated: false,
                prefix: String::new(),
                suffix: String::new(),
            }),
        }));

        assert_eq!(error.code(), "command_output_limit");
        let details = error.details().unwrap();
        assert_eq!(details["stage"], "github_command_output");
        assert_eq!(details["stdout"]["total_bytes"], 100);
        assert_eq!(details["stdout"]["limit_bytes"], 32);
        assert_eq!(details["streams_combined"], false);
        assert_eq!(details["mutated"], false);
    }

    #[test]
    fn discovery_timeout_preserves_timeout_category_and_evidence() {
        let error = discovery_error(&DiscoveryError::Runner(CommandRunError::Timeout {
            command: crate::command::CommandSpec::new("gh").args(["pr", "list"]),
            process_group_id: None,
            timeout_ms: 500,
            stdout: "partial".to_owned(),
            stderr: "stalled".to_owned(),
        }));

        assert_eq!(
            mcp_cli::StructuredError::category(&error),
            ErrorCategory::Timeout
        );
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "github_discovery_timeout"
        );
        let details = mcp_cli::StructuredError::details(&error).unwrap();
        assert_eq!(details["stage"], "github_discovery");
        assert_eq!(details["phase"], "open_pull_requests_and_checks");
        assert_eq!(details["timeout_ms"], 500);
        assert_eq!(details["stdout"], "partial");
        assert_eq!(details["retryable"], true);
        assert!(
            details["safe_next_action"]
                .as_str()
                .unwrap()
                .contains("no mutations")
        );
    }

    /// The phase that was RUNNING when the budget expired is not necessarily the
    /// phase that CONSUMED it, and reporting one number under the other's name
    /// sends readers after the wrong command. Live case: the label inventory was
    /// blamed for 68 seconds when it costs under one, because earlier phases had
    /// spent the budget (bd-1d6b1a).
    #[test]
    fn a_deadline_error_separates_phase_cost_from_total_elapsed() {
        let provider = DiscoveryError::Runner(CommandRunError::Timeout {
            command: crate::command::CommandSpec::new("gh").args(["label", "list"]),
            process_group_id: None,
            timeout_ms: 250,
            stdout: String::new(),
            stderr: "stalled".to_owned(),
        });

        let error = discovery_timeout_error(
            &provider,
            "repository_label_inventory",
            std::time::Duration::from_millis(819),
            std::time::Duration::from_millis(68_644),
            std::time::Duration::from_secs(60),
        );

        let details = mcp_cli::StructuredError::details(&error).unwrap();
        assert_eq!(details["phase_elapsed_ms"], 819);
        assert_eq!(details["elapsed_ms"], 68_644);
        let message = mcp_cli::StructuredError::message(&error);
        assert!(
            message.contains("819") && message.contains("68644"),
            "the human message must show both, or the cheap phase reads as the expensive one: {message}"
        );
    }

    #[test]
    fn status_deadline_error_reports_total_elapsed_and_phase() {
        let provider = DiscoveryError::Runner(CommandRunError::Timeout {
            command: crate::command::CommandSpec::new("gh").args(["pr", "list"]),
            process_group_id: None,
            timeout_ms: 250,
            stdout: String::new(),
            stderr: "stalled".to_owned(),
        });
        let error = discovery_timeout_error(
            &provider,
            "compatibility_prepare",
            std::time::Duration::from_millis(875),
            std::time::Duration::from_millis(875),
            std::time::Duration::from_secs(1),
        );
        let details = mcp_cli::StructuredError::details(&error).unwrap();
        assert_eq!(details["phase"], "compatibility_prepare");
        assert_eq!(details["elapsed_ms"], 875);
        assert_eq!(details["deadline_ms"], 1_000);
    }

    #[test]
    fn resilient_status_budget_is_read_only_and_not_the_sync_mutation_window() {
        let mut context = crate::AppContext::default();
        context.config.command_timeout_secs = 3_600;
        context.config.sync.max_duration_secs = 3_600;

        assert_eq!(STATUS_READ_BUDGET, std::time::Duration::from_secs(35));
        assert_eq!(STATUS_COMPLETION_RESERVE, std::time::Duration::from_secs(2));
        assert_eq!(
            STATUS_POST_ANALYSIS_RESERVE,
            std::time::Duration::from_secs(8)
        );
        assert_ne!(
            STATUS_READ_BUDGET,
            std::time::Duration::from_secs(context.config.sync.max_duration_secs),
            "read-only status must never borrow the sync mutation deadline"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn first_status_over_eight_candidates_yields_current_evidence_then_can_complete() {
        let active = (1..=12)
            .map(|number| pr(number, &format!("branch-{number}"), "main", true))
            .collect::<Vec<_>>();
        let mut provider_rows = active.clone();
        provider_rows.extend(
            (101..=110).map(|number| pr(number, &format!("candidate-{number}"), "main", false)),
        );
        let repository_snapshot = snapshot(active[8].clone(), provider_rows.clone());
        let checker = TimeoutAfterChecks {
            calls: AtomicUsize::new(0),
            completed_before_timeout: 3,
        };
        let bounded = analyze_for_actor_bounded(
            &repository_snapshot,
            &checker,
            crate::model::HeadMergeActor::default(),
            Instant::now() + Duration::from_secs(1),
        )
        .expect("a local timeout yields bounded current evidence");
        assert!(!bounded.progress.complete);
        assert_eq!(bounded.progress.candidate_count, 22);
        assert_eq!(bounded.progress.unqueued_candidate_count, 10);
        assert!(bounded.progress.planned_analyses > 8);
        assert!(bounded.progress.completed_analyses >= 3);
        assert!(
            bounded.progress.completed_analyses < bounded.progress.planned_analyses,
            "the slow fixture must defer part of the graph"
        );
        assert!(!bounded.progress.deferred_analyses.is_empty());
        assert!(!bounded.progress.skipped_analyses.is_empty());

        let mut output = status(active[8].clone(), provider_rows);
        output.analysis = bounded.analysis;
        output.healthy = false;
        output.provider_api = crate::model::GitHubApiTelemetry::default();
        output.timing = Some(StatusTiming {
            deadline_ms: 33_000,
            total_ms: 30_000,
            completion_reserve_ms: 10_000,
            compatibility_budget_ms: 25_000,
            compatibility_analysis: bounded.progress,
            phases_ms: std::collections::BTreeMap::from([
                ("github_discovery".to_owned(), 1_000),
                ("provider_identity".to_owned(), 1_000),
                ("compatibility_analysis".to_owned(), 28_000),
            ]),
        });

        let receipt = recover_current_partial_status(output, Instant::now(), STATUS_READ_BUDGET);
        let partial = receipt.status_partial.as_ref().expect("typed partial");
        assert!(!receipt.output.healthy);
        assert_eq!(partial.code, "status_partial");
        assert_eq!(
            partial.evidence_source,
            StatusPartialEvidenceSource::CurrentBoundedEvidence
        );
        assert!(partial.cursor.starts_with("current_bounded_evidence:"));
        assert_eq!(partial.last_good_age_ms, None);
        assert!(!partial.unknown_fields.is_empty());
        assert_eq!(partial.attempt_provider_api.calls, 0);
        assert_eq!(partial.attempt_provider_api.authenticated, None);
        assert!(!partial.mutated);
        assert_eq!(
            receipt
                .output
                .timing
                .as_ref()
                .expect("timing")
                .completion_reserve_ms,
            10_000
        );
        assert!(partial.elapsed_ms < 35_000);

        // The Caco adapter consumes a successful-but-unhealthy envelope as a
        // degraded canonical engine, not an unavailable command failure.
        let adapter_payload = serde_json::to_value(&receipt).expect("serializes");
        let canonical_adapter_envelope = json!({
            "status": "success",
            "data": adapter_payload,
        });
        assert_eq!(canonical_adapter_envelope["status"], "success");
        assert_eq!(canonical_adapter_envelope["data"]["healthy"], false);
        assert_eq!(
            canonical_adapter_envelope["data"]["status_partial"]["evidence_source"],
            "current_bounded_evidence"
        );
        assert!(canonical_adapter_envelope["data"]["provider_api"]["authenticated"].is_null());

        let strict_error = analyze_for_actor_with_progress(
            &repository_snapshot,
            &TimeoutAfterChecks {
                calls: AtomicUsize::new(0),
                completed_before_timeout: 0,
            },
            crate::model::HeadMergeActor::default(),
        )
        .expect_err("exact/mutating callers must never consume bounded partial analysis");
        assert_eq!(strict_error.category(), ErrorCategory::Timeout);

        let later = analyze_for_actor_with_progress(
            &repository_snapshot,
            &clean_checker,
            crate::model::HeadMergeActor::default(),
        )
        .expect("a later pass can produce the full snapshot");
        assert!(later.progress.complete);
        assert_eq!(
            later.progress.completed_analyses,
            later.progress.planned_analyses
        );
        assert!(later.progress.deferred_analyses.is_empty());
    }

    #[test]
    fn slow_provider_over_eight_candidates_returns_caco_compatible_status_partial() {
        let active = (1..=12)
            .map(|number| pr(number, &format!("branch-{number}"), "main", true))
            .collect::<Vec<_>>();
        let current = active[8].clone();
        let complete = status(current, active);
        assert!(
            complete.analysis.pull_requests.len() > 8,
            "the last-good fixture must retain a provider set beyond the automatic candidate limit"
        );
        let recorded = unix_millis().saturating_sub(1_250);
        let persisted = PersistedStatus {
            schema_version: STATUS_LAST_GOOD_SCHEMA_VERSION,
            recorded_unix_ms: recorded,
            config_fingerprint: "fixture".to_owned(),
            output: complete,
        };
        let provider_api = crate::model::GitHubApiTelemetry {
            calls: 17,
            graphql_calls: 9,
            rest_calls: 8,
            ..crate::model::GitHubApiTelemetry::default()
        };
        let error = AppError::structured(
            ErrorCategory::Timeout,
            "github_discovery_timeout",
            "slow provider enumeration exhausted the dedicated read budget",
            Some(json!({
                "phase": "open_pull_requests_and_checks",
                "provider_api": provider_api,
            })),
        );

        let receipt = recover_partial_status(
            &error,
            std::time::Instant::now(),
            STATUS_READ_BUDGET,
            persisted,
        );
        let partial = receipt
            .status_partial
            .as_ref()
            .expect("typed partial receipt");
        assert!(!receipt.output.healthy);
        assert_eq!(partial.code, "status_partial");
        assert_eq!(partial.exhausted_phase, "open_pull_requests_and_checks");
        assert!(partial.cursor.starts_with("last_good_complete_status:"));
        assert!(partial.last_good_age_ms.is_some_and(|age| age >= 1_250));
        assert_eq!(partial.attempt_provider_api.calls, 17);
        assert_eq!(partial.deadline_ms, 35_000);
        let expected_repository = receipt.output.repository.clone();
        let adapter_payload = serde_json::to_value(&receipt).expect("status receipt serializes");
        assert_eq!(adapter_payload["healthy"], false);
        assert_eq!(
            adapter_payload["repository"]["owner"],
            expected_repository.owner
        );
        assert_eq!(
            adapter_payload["repository"]["name"],
            expected_repository.name
        );
        assert_eq!(adapter_payload["status_partial"]["code"], "status_partial");
        assert_eq!(
            adapter_payload["status_partial"]["attempt_provider_api"]["calls"],
            17
        );
        let canonical_adapter_envelope = json!({
            "status": "success",
            "data": adapter_payload,
        });
        assert_eq!(
            canonical_adapter_envelope["data"]["repository"]["owner"],
            expected_repository.owner
        );
        assert_eq!(canonical_adapter_envelope["data"]["healthy"], false);
        assert_eq!(
            canonical_adapter_envelope["data"]["status_partial"]["code"],
            "status_partial"
        );
        assert!(
            !partial
                .safe_next_action
                .contains("raise sync.max_duration_secs"),
            "read-only recovery must not prescribe an already-maxed mutation deadline"
        );
    }

    #[test]
    fn partial_unavailable_guidance_never_touches_sync_mutation_bounds() {
        let error = AppError::structured(
            ErrorCategory::Timeout,
            "status_partial_unavailable",
            "read-only status exhausted its dedicated provider budget before any last-good snapshot was available",
            Some(json!({
                "phase": "merge_candidate_identity",
                "status_partial": false,
                "safe_next_action": "retry read-only status after provider latency recovers; do not change sync mutation reserves or add another loop",
            })),
        );
        let details = error.details().expect("typed details");
        let action = details["safe_next_action"].as_str().expect("action");
        assert!(!action.contains("raise sync.max_duration_secs"));
        assert!(!action.contains("lower sync.max_candidates_per_tick"));
        assert!(action.contains("read-only status"));
    }

    #[test]
    fn command_boundary_watchdog_returns_current_provider_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        let context = crate::AppContext {
            repository_path: directory.path().to_path_buf(),
            config_path: directory.path().join(".caravan/config.yaml"),
            config_existed: true,
            config: crate::config::CaravanConfig::default(),
        };
        let path = directory.path().join("watchdog.json");
        let output = status(pr(9, "nine", "main", false), Vec::new());
        let receipt = StatusReadReceipt {
            output,
            status_partial: Some(StatusPartial {
                schema_version: 2,
                code: "status_partial".to_owned(),
                exhausted_phase: "compatibility_analysis".to_owned(),
                cursor: "provider_discovery_checkpoint:fixture".to_owned(),
                elapsed_ms: 10,
                deadline_ms: 35_000,
                remaining_ms: 34_990,
                evidence_source: StatusPartialEvidenceSource::CurrentBoundedEvidence,
                last_good_age_ms: None,
                unknown_fields: vec!["analysis.compatibility".to_owned()],
                attempt_provider_api: crate::model::GitHubApiTelemetry {
                    authenticated: Some(true),
                    calls: 11,
                    ..crate::model::GitHubApiTelemetry::default()
                },
                mutated: false,
                safe_next_action: "complete compatibility".to_owned(),
            }),
        };
        write_watchdog_checkpoint(&path, &context, &receipt);
        assert!(path.exists(), "checkpoint must be written");
        let raw_checkpoint = std::fs::read(&path).expect("checkpoint bytes");
        let _: StatusWatchdogCheckpoint =
            serde_json::from_slice(&raw_checkpoint).expect("checkpoint shape");

        let recovered =
            status_watchdog_fallback(&context, &path, std::time::Duration::from_secs(45))
                .expect("outer watchdog retains current checkpoint evidence");
        let partial = recovered.status_partial.expect("partial");
        assert!(!recovered.output.healthy);
        assert_eq!(partial.exhausted_phase, "command_boundary_watchdog");
        assert_eq!(partial.elapsed_ms, 45_000);
        assert_eq!(partial.deadline_ms, 40_000);
        assert_eq!(partial.remaining_ms, 0);
        assert_eq!(partial.attempt_provider_api.calls, 11);
        assert!(!partial.mutated);
        assert_eq!(recovered.output.repository, receipt.output.repository);
    }

    #[test]
    fn persisted_last_good_is_config_fenced_and_size_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let context = crate::AppContext {
            repository_path: directory.path().to_path_buf(),
            config_path: directory.path().join(".caravan/config.yaml"),
            config_existed: true,
            config: crate::config::CaravanConfig::default(),
        };
        let path = directory.path().join("status.json");
        let output = status(pr(9, "nine", "main", false), Vec::new());
        persist_last_good_at(&path, &context, &output);
        let loaded = load_last_good_at(&path, &context).expect("matching config loads");
        assert_eq!(loaded.output.repository, output.repository);

        let mut changed = context.clone();
        changed.config.command_timeout_secs += 1;
        assert!(
            load_last_good_at(&path, &changed).is_none(),
            "a policy change invalidates stale status sections"
        );
    }

    #[test]
    fn admission_is_fifo_for_equal_and_absent_priority() {
        let mut older = pr(20, "older", "main", false);
        older.created_at = Some("2026-01-01T00:00:01Z".to_owned());
        older.labels.insert("caravan-priority:normal".to_owned());
        let mut newer = pr(10, "newer", "main", false);
        newer.created_at = Some("2026-01-01T00:00:02Z".to_owned());
        newer.labels.insert("caravan-priority:normal".to_owned());
        let no_priority = pr(5, "unprioritized", "main", false);
        let status = status(older, vec![newer, no_priority]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;
        let admission = resolve_admission(&status.analysis, &labels);
        assert_eq!(
            admission
                .candidates
                .iter()
                .map(|candidate| candidate.pr)
                .collect::<Vec<_>>(),
            [PrNumber(20), PrNumber(10), PrNumber(5)]
        );
        assert_eq!(admission.next_candidate, Some(PrNumber(20)));
        assert!(admission.candidates[0].reason.contains("FIFO"));
        assert!(
            admission.candidates[0]
                .reason
                .contains("preflight required")
        );
        assert!(admission.policy.contains("never LIFO"));
        assert!(
            admission
                .policy
                .contains("never causes automatic leapfrogging")
        );
    }

    #[test]
    fn superseded_generation_is_excluded_without_blocking_canonical_successor() {
        let labels = vec!["caravan-priority:high".to_owned()];
        let mut older = pr(2107, "old", "main", false);
        older.labels.insert(labels[0].clone());
        older.created_at = Some("2026-07-23T01:50:32Z".to_owned());
        let mut newer = pr(2123, "new", "main", false);
        newer.labels.insert(labels[0].clone());
        newer.created_at = Some("2026-07-23T18:34:41Z".to_owned());
        let status = status(older.clone(), vec![newer.clone()]);
        let generation = crate::generation::analyze(
            &[
                crate::model::PullRequestGenerationFact {
                    pr: older.number,
                    provider_head: older.head.oid.clone(),
                    created_at: older.created_at.clone(),
                    provenance: Some(crate::model::CacophonyGenerationProvenance {
                        generation: format!("old-pr-g{}", "a".repeat(40)),
                        agent: "android-agent".to_owned(),
                        source_head: crate::model::CommitOid("a".repeat(40)),
                        bead_ids: BTreeSet::from(["bd-c7440c".to_owned()]),
                        stack_base: "main".to_owned(),
                        stack_state: "root".to_owned(),
                    }),
                    metadata_error: None,
                    supersedes: BTreeSet::new(),
                },
                crate::model::PullRequestGenerationFact {
                    pr: newer.number,
                    provider_head: newer.head.oid.clone(),
                    created_at: newer.created_at.clone(),
                    provenance: Some(crate::model::CacophonyGenerationProvenance {
                        generation: format!("new-pr-g{}", "b".repeat(40)),
                        agent: "android-agent".to_owned(),
                        source_head: crate::model::CommitOid("b".repeat(40)),
                        bead_ids: BTreeSet::from(["bd-c7440c".to_owned(), "bd-4734d1".to_owned()]),
                        stack_base: "main".to_owned(),
                        stack_state: "root".to_owned(),
                    }),
                    metadata_error: None,
                    supersedes: BTreeSet::new(),
                },
            ],
            |_base, _head| crate::generation::CommitRelation::Ahead,
        );
        let admission = resolve_admission_with_generation(&status.analysis, &labels, generation);
        assert_eq!(admission.next_candidate, Some(newer.number));
        assert_eq!(admission.candidates[0].pr, newer.number);
        let excluded = admission
            .rejected
            .iter()
            .find(|candidate| candidate.pr == older.number)
            .unwrap();
        assert!(!excluded.blocks_order);
        assert!(excluded.reason.contains("superseded"));
    }

    #[test]
    fn active_superseded_generation_becomes_a_graph_stop() {
        let older = pr(2107, "old", "main", true);
        let newer = pr(2123, "new", "main", false);
        let mut status = status(older.clone(), vec![newer.clone()]);
        let old_fact = crate::model::PullRequestGenerationFact {
            pr: older.number,
            provider_head: older.head.oid.clone(),
            created_at: older.created_at.clone(),
            provenance: Some(crate::model::CacophonyGenerationProvenance {
                generation: format!("old-pr-g{}", "a".repeat(40)),
                agent: "android-agent".to_owned(),
                source_head: crate::model::CommitOid("a".repeat(40)),
                bead_ids: BTreeSet::from(["bd-c7440c".to_owned()]),
                stack_base: "main".to_owned(),
                stack_state: "root".to_owned(),
            }),
            metadata_error: None,
            supersedes: BTreeSet::new(),
        };
        let mut new_fact = old_fact.clone();
        new_fact.pr = newer.number;
        new_fact.provider_head = newer.head.oid.clone();
        new_fact.created_at = newer.created_at.clone();
        new_fact.provenance.as_mut().unwrap().generation = format!("new-pr-g{}", "b".repeat(40));
        new_fact.provenance.as_mut().unwrap().source_head = crate::model::CommitOid("b".repeat(40));
        new_fact.supersedes.insert(older.number);
        let generation = crate::generation::analyze(&[old_fact, new_fact], |_base, _head| {
            unreachable!("reviewed link is complete")
        });
        apply_generation_graph_problems(&mut status.analysis, &generation);
        assert!(status.analysis.fleet.problems.iter().any(|problem| {
            problem.kind == GraphProblemKind::SupersededGeneration
                && problem.prs.contains(&older.number)
        }));
    }

    #[test]
    fn equal_and_missing_created_at_use_pr_number_deterministically() {
        let mut equal_high = pr(20, "equal-high", "main", false);
        equal_high.created_at = Some("2026-01-01T00:00:01Z".to_owned());
        let mut equal_low = pr(10, "equal-low", "main", false);
        equal_low.created_at = Some("2026-01-01T00:00:01Z".to_owned());
        let mut missing_high = pr(40, "missing-high", "main", false);
        missing_high.created_at = None;
        let mut missing_low = pr(30, "missing-low", "main", false);
        missing_low.created_at = None;
        let status = status(equal_high, vec![missing_high, equal_low, missing_low]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;
        let admission = resolve_admission(&status.analysis, &labels);
        assert_eq!(
            admission
                .candidates
                .iter()
                .map(|candidate| candidate.pr)
                .collect::<Vec<_>>(),
            [PrNumber(10), PrNumber(20), PrNumber(30), PrNumber(40)]
        );
        assert!(admission.candidates[2].reason.contains("fallback"));
    }

    #[test]
    fn explicit_priority_deliberately_overrides_fifo() {
        let older = pr(10, "older", "main", false);
        let mut newer = pr(20, "newer", "main", false);
        newer.labels.insert("caravan-priority:high".to_owned());
        let status = status(older, vec![newer]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;
        let admission = resolve_admission(&status.analysis, &labels);
        assert_eq!(admission.next_candidate, Some(PrNumber(20)));
        assert_eq!(admission.candidates[0].priority_rank, Some(1));
    }

    /// Measured on live cacophony: PR 2279 was CLEAN and correctly elected, yet
    /// the forge reported `mergeStateStatus=BLOCKED` for it exactly as it did
    /// for the red PR 2276. BLOCKED means "protection not yet satisfied", which
    /// includes required checks that are merely still running.
    ///
    /// So the forge verdict must NOT gate admission: doing so would refuse every
    /// candidate whose CI has not finished and stall the queue completely. It is
    /// consulted only at merge time, where the checks are already proven green.
    #[test]
    fn a_blocked_forge_verdict_does_not_bar_admission() {
        let mut blocked = pr(20, "clean-but-blocked", "main", false);
        blocked.merge_state_status = Some("BLOCKED".to_owned());
        let mut problems = Vec::new();

        validate_candidate(&blocked, &mut problems);

        assert!(
            problems.is_empty(),
            "a forge BLOCKED verdict must not bar admission, or no candidate with running CI could ever be admitted: {problems:?}"
        );
    }

    /// Two surfaces must not disagree about the same PR. On cacophony PR 2276,
    /// `check --pr 2276` reported ineligible while `status` still elected it as
    /// `next_candidate`, because the CI refusal lived only on the check path.
    /// Ordering now applies the same rule, and skips rather than blocks so the
    /// queue advances to a clean candidate instead of waiting on a red head.
    #[test]
    fn a_red_candidate_is_skipped_so_the_queue_advances() {
        let mut red = pr(10, "red", "main", false);
        red.checks.push(crate::model::CheckSnapshot {
            name: "Rust Check & Lint work".to_owned(),
            state: crate::model::CheckState::Failure,
            provider_state: Some("FAILURE".to_owned()),
            details_url: None,
            ..crate::model::CheckSnapshot::default()
        });
        let green = pr(20, "green", "main", false);
        let status = status(red, vec![green]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;

        let admission = resolve_admission(&status.analysis, &labels);

        assert_eq!(
            admission.next_candidate,
            Some(PrNumber(20)),
            "a red PR must not hold the admission front"
        );
        assert!(
            admission
                .skipped
                .iter()
                .any(|candidate| candidate.pr == PrNumber(10)),
            "the red candidate must be visibly skipped, not silently dropped: {:?}",
            admission.skipped
        );
    }

    /// bd-eff1dc live shape (cacophony PR #2339, head
    /// `9fb22ff9930d621336ee435f29989639072f5ac5`): the same required checks
    /// appear three times on one head because the workflow was cancelled and
    /// retried. Cara reported "candidate has a failing required check" and
    /// refused rejoin while the GitHub UI correctly showed CI in progress.
    ///
    /// The rollup is a lineage: only the newest run of each required check is a
    /// current verdict, so a candidate whose latest runs are green must reach
    /// the admission front.
    #[test]
    fn a_candidate_whose_red_runs_were_superseded_is_admissible() {
        let run = |name: &str,
                   state: crate::model::CheckState,
                   started: &str,
                   completed: Option<&str>| {
            crate::model::CheckSnapshot {
                name: name.to_owned(),
                state,
                provider_kind: Some("CheckRun".to_owned()),
                workflow_name: Some("CI".to_owned()),
                started_at: Some(started.to_owned()),
                completed_at: completed.map(str::to_owned),
                ..crate::model::CheckSnapshot::default()
            }
        };

        let mut retried = pr(10, "retried", "main", false);
        retried.checks = vec![
            run(
                "Public Fast Tests preparation",
                crate::model::CheckState::Cancelled,
                "10:26",
                Some("10:30"),
            ),
            run(
                "Public Fast Tests preparation",
                crate::model::CheckState::Cancelled,
                "10:51",
                Some("10:53"),
            ),
            run(
                "Public Fast Tests preparation",
                crate::model::CheckState::Success,
                "10:57",
                Some("11:05"),
            ),
            run(
                "Check & Lint",
                crate::model::CheckState::Failure,
                "10:50",
                Some("10:52"),
            ),
            run(
                "Check & Lint",
                crate::model::CheckState::Success,
                "11:02",
                Some("11:04"),
            ),
        ];
        let later = pr(20, "later", "main", false);
        let status = status(retried, vec![later]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;

        let admission = resolve_admission(&status.analysis, &labels);

        assert_eq!(
            admission.next_candidate,
            Some(PrNumber(10)),
            "superseded red runs must not hold a green candidate out of the queue: {:?}",
            admission.skipped
        );
        assert!(
            !admission
                .skipped
                .iter()
                .any(|candidate| candidate.pr == PrNumber(10)),
            "the candidate must not be skipped for a rerun failure: {:?}",
            admission.skipped
        );
    }

    /// bd-d376b9 live #2358 shape. The old workflow generation is terminal,
    /// while its replacement has started but has not materialized every
    /// downstream job yet. GitHub supplies year-0001 as the unfinished
    /// completion timestamp. This is pending-for-admission, not red: joining
    /// rewrites the head and starts CI again anyway.
    #[test]
    fn a_candidate_with_a_newer_running_workflow_generation_is_admissible() {
        let run = |name: &str,
                   run_id: u64,
                   state: crate::model::CheckState,
                   started: &str,
                   completed: &str| {
            crate::model::CheckSnapshot {
                name: name.to_owned(),
                state,
                provider_kind: Some("CheckRun".to_owned()),
                workflow_name: Some("CI".to_owned()),
                details_url: Some(format!(
                    "https://github.com/acme/widgets/actions/runs/{run_id}/job/1"
                )),
                started_at: Some(started.to_owned()),
                completed_at: Some(completed.to_owned()),
                ..crate::model::CheckSnapshot::default()
            }
        };

        let mut pending = pr(10, "pending-rerun", "main", false);
        pending.checks = vec![
            run(
                "Check & Lint",
                100,
                crate::model::CheckState::Failure,
                "2026-08-02T09:49:14Z",
                "2026-08-02T09:49:19Z",
            ),
            run(
                "Public Fast Tests preparation",
                100,
                crate::model::CheckState::Cancelled,
                "2026-08-02T09:49:02Z",
                "2026-08-02T09:49:02Z",
            ),
            run(
                "Changed surface admission",
                200,
                crate::model::CheckState::Success,
                "2026-08-02T09:49:50Z",
                "2026-08-02T09:50:28Z",
            ),
            run(
                "Public Fast Tests preparation",
                200,
                crate::model::CheckState::InProgress,
                "2026-08-02T09:50:30Z",
                "0001-01-01T00:00:00Z",
            ),
        ];
        let later = pr(20, "later", "main", false);
        let status = status(pending, vec![later]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;

        let admission = resolve_admission(&status.analysis, &labels);

        assert_eq!(admission.next_candidate, Some(PrNumber(10)));
        assert!(
            !admission
                .skipped
                .iter()
                .any(|candidate| candidate.pr == PrNumber(10)),
            "pending replacement CI must stay eligible: {:?}",
            admission.skipped
        );
    }

    /// The same reduction must not excuse a CURRENT failure: newest red still
    /// refuses, even with older green runs above it in the lineage.
    #[test]
    fn a_current_failure_after_an_earlier_success_still_refuses() {
        let run = |state: crate::model::CheckState, started: &str, completed: &str| {
            crate::model::CheckSnapshot {
                name: "Check & Lint".to_owned(),
                state,
                provider_kind: Some("CheckRun".to_owned()),
                workflow_name: Some("CI".to_owned()),
                started_at: Some(started.to_owned()),
                completed_at: Some(completed.to_owned()),
                ..crate::model::CheckSnapshot::default()
            }
        };

        let mut regressed = pr(10, "regressed", "main", false);
        regressed.checks = vec![
            run(crate::model::CheckState::Success, "10:20", "10:25"),
            run(crate::model::CheckState::Failure, "10:50", "10:52"),
        ];
        let mut problems = Vec::new();

        validate_candidate(&regressed, &mut problems);

        assert!(
            problems
                .iter()
                .any(|problem| problem.message.contains("failing required check")),
            "a newer failure is a verdict, not history: {problems:?}"
        );
    }

    /// bd-c04d9b live shape (caravan #2287): five producers CANCELLED under
    /// capacity pressure, two aggregates then concluding FAILURE in seconds
    /// without building anything. An operator measured two of three such cases
    /// as spurious.
    ///
    /// Cara cannot tell an aggregate-consequence failure from a real one, because
    /// the rollup reports only the aggregate's own conclusion. What it can do is
    /// refuse for the RIGHT STATED REASON, so the reader reruns rather than
    /// repairing or evicting on the strength of a consequence.
    #[test]
    fn a_cancellation_is_named_so_the_reader_reruns_rather_than_evicts() {
        let mut cancelled = pr(2287, "member", "main", false);
        cancelled.checks.push(crate::model::CheckSnapshot {
            name: "Public Fast Tests preparation".to_owned(),
            state: crate::model::CheckState::Cancelled,
            provider_state: Some("CANCELLED".to_owned()),
            details_url: None,
            ..crate::model::CheckSnapshot::default()
        });
        cancelled.checks.push(crate::model::CheckSnapshot {
            name: "Check & Lint".to_owned(),
            state: crate::model::CheckState::Failure,
            provider_state: Some("FAILURE".to_owned()),
            details_url: None,
            ..crate::model::CheckSnapshot::default()
        });
        let mut problems = Vec::new();

        validate_candidate(&cancelled, &mut problems);

        let reason = problems
            .iter()
            .map(|problem| problem.message.clone())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            reason.contains("cancelled") && reason.contains("rerun"),
            "the refusal must name the cancellation and point at a rerun: {reason}"
        );
        assert!(
            !reason.contains("fix it before admission"),
            "it must NOT read as a code failure the owner should repair: {reason}"
        );
    }

    /// Operator ruling from cacophony PR 2276: a failing check MUST bar
    /// admission, because queueing behind red is guaranteed rework. The failure
    /// has to be fixed, fixing it rewrites the head, and every member stacked
    /// behind it is rebased anyway, so admitting red buys nothing and costs the
    /// whole tail a re-stitch.
    #[test]
    fn a_candidate_with_a_failing_check_is_not_admissible() {
        let mut red = pr(10, "red", "main", false);
        red.checks.push(crate::model::CheckSnapshot {
            name: "Rust Check & Lint work".to_owned(),
            state: crate::model::CheckState::Failure,
            provider_state: Some("FAILURE".to_owned()),
            details_url: None,
            ..crate::model::CheckSnapshot::default()
        });
        let mut problems = Vec::new();

        validate_candidate(&red, &mut problems);

        assert!(
            problems
                .iter()
                .any(|problem| problem.message.contains("failing required check")),
            "a red candidate must be refused admission: {problems:?}"
        );
    }

    /// The audited override still works: `caravan-force` is how an operator
    /// takes responsibility for admitting a red head deliberately.
    #[test]
    fn caravan_force_still_admits_a_red_candidate() {
        let mut forced = pr(11, "forced", "main", false);
        forced.labels.insert("caravan-force".to_owned());
        forced.checks.push(crate::model::CheckSnapshot {
            name: "Rust Check & Lint work".to_owned(),
            state: crate::model::CheckState::Failure,
            provider_state: Some("FAILURE".to_owned()),
            details_url: None,
            ..crate::model::CheckSnapshot::default()
        });
        let mut problems = Vec::new();

        validate_candidate(&forced, &mut problems);

        assert!(
            !problems
                .iter()
                .any(|problem| problem.message.contains("failing required check")),
            "caravan-force must remain the audited override: {problems:?}"
        );
    }

    /// A check still running is not a failure. Refusing on pending would stall
    /// every candidate for the duration of its own CI.
    #[test]
    fn a_pending_check_does_not_bar_admission() {
        let mut pending = pr(12, "pending", "main", false);
        pending.checks.push(crate::model::CheckSnapshot {
            name: "Rust Fast Tests work".to_owned(),
            state: crate::model::CheckState::InProgress,
            provider_state: None,
            details_url: None,
            ..crate::model::CheckSnapshot::default()
        });
        let mut problems = Vec::new();

        validate_candidate(&pending, &mut problems);

        assert!(
            !problems
                .iter()
                .any(|problem| problem.message.contains("failing required check")),
            "a pending check is not a failure: {problems:?}"
        );
    }

    /// Live operator report (PR 2234): Cara proved the leading candidate could
    /// not merge into the default branch, reported it, and then elected it
    /// anyway, holding the whole queue behind a PR no rerun can fix. A
    /// mechanical conflict is not a decision anyone made, so unlike an owner's
    /// explicit rejection it must not inherit blocking authority.
    #[test]
    fn a_mechanically_incompatible_candidate_skips_instead_of_holding_the_front() {
        let conflicted = pr(10, "conflicted", "main", false);
        let later = pr(20, "later", "main", false);
        let mut status = status(conflicted, vec![later]);
        status.analysis.fleet.problems.push(GraphProblem {
            kind: GraphProblemKind::CandidateIncompatible,
            prs: vec![PrNumber(10)],
            message: "does not merge cleanly into the current default branch".to_owned(),
        });
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;

        let admission = resolve_admission(&status.analysis, &labels);

        assert_eq!(
            admission.next_candidate,
            Some(PrNumber(20)),
            "a clean later candidate must not be starved by a proven conflict"
        );
        assert!(
            admission
                .skipped
                .iter()
                .any(|candidate| candidate.pr == PrNumber(10)),
            "the conflicting candidate must be reported as skipped, not silently dropped: {:?}",
            admission.skipped
        );
    }

    #[test]
    fn generation_bound_skip_is_excluded_without_blocking_later_candidates() {
        let mut skipped = pr(10, "skipped", "main", false);
        skipped.labels.insert("caravan-join-skipped".to_owned());
        let later = pr(20, "later", "main", false);
        let status = status(skipped, vec![later]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;
        let admission = resolve_admission(&status.analysis, &labels);

        assert_eq!(admission.skipped.len(), 1);
        assert_eq!(admission.skipped[0].pr, PrNumber(10));
        assert_eq!(admission.next_candidate, Some(PrNumber(20)));
        assert_eq!(
            admission
                .candidates
                .iter()
                .map(|candidate| candidate.pr)
                .collect::<Vec<_>>(),
            [PrNumber(20)]
        );
    }

    #[test]
    fn ineligible_candidates_are_reported_without_wedging_eligible_admission() {
        let mut draft = pr(10, "draft", "main", false);
        draft.draft = true;
        let mut fork = pr(20, "fork", "main", false);
        fork.cross_repository = true;
        let mut external = pr(30, "external", "main", false);
        external.auto_merge = AutoMergeState::squash();
        let eligible = pr(40, "eligible", "main", false);
        let status = status(draft, vec![fork, external, eligible]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;

        let admission = resolve_admission(&status.analysis, &labels);

        assert_eq!(
            admission.next_candidate,
            Some(PrNumber(40)),
            "structurally ineligible PRs must not starve an eligible candidate"
        );
        assert_eq!(admission.candidates.len(), 1);
        assert!(
            admission
                .rejected
                .iter()
                .all(|candidate| !candidate.blocks_order
                    && candidate.reason.contains("not an admission attempt"))
        );
        assert_eq!(
            admission
                .rejected
                .iter()
                .map(|candidate| candidate.pr)
                .collect::<Vec<_>>(),
            vec![PrNumber(20), PrNumber(30)],
            "drafts are excluded before admission ordering; fork/auto-merge PRs are reported"
        );
    }

    #[test]
    fn ambiguous_generation_reports_exactly_without_blocking_other_owners() {
        let ambiguous_first = pr(2107, "old", "main", false);
        let ambiguous_second = pr(2109, "newer", "main", false);
        let unrelated = pr(2115, "unrelated", "main", false);
        let status = status(
            ambiguous_first.clone(),
            vec![ambiguous_second.clone(), unrelated.clone()],
        );
        let fact = |pr: &crate::model::PullRequestSnapshot, source: char| {
            crate::model::PullRequestGenerationFact {
                pr: pr.number,
                provider_head: pr.head.oid.clone(),
                created_at: pr.created_at.clone(),
                provenance: Some(crate::model::CacophonyGenerationProvenance {
                    generation: format!("agent/x-pr-g{}", source.to_string().repeat(40)),
                    agent: "android-agent".to_owned(),
                    source_head: crate::model::CommitOid(source.to_string().repeat(40)),
                    bead_ids: BTreeSet::from(["bd-c7440c".to_owned()]),
                    stack_base: "main".to_owned(),
                    stack_state: "root".to_owned(),
                }),
                metadata_error: None,
                supersedes: BTreeSet::new(),
            }
        };
        let generation = crate::generation::analyze(
            &[fact(&ambiguous_first, 'a'), fact(&ambiguous_second, 'b')],
            |_base, _head| crate::generation::CommitRelation::Diverged,
        );
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;

        let admission = resolve_admission_with_generation(&status.analysis, &labels, generation);

        assert_eq!(admission.next_candidate, Some(unrelated.number));
        assert_eq!(admission.rejected.len(), 2);
        assert!(
            admission
                .rejected
                .iter()
                .all(|candidate| !candidate.blocks_order
                    && candidate.reason.contains("divergent or unproved"))
        );
    }

    #[test]
    fn invalid_and_conflicting_priority_labels_fail_closed() {
        let mut unknown = pr(10, "unknown", "main", false);
        unknown
            .labels
            .insert("caravan-priority:surprise".to_owned());
        let mut conflicting = pr(20, "conflicting", "main", false);
        conflicting.labels.extend([
            "caravan-priority:high".to_owned(),
            "caravan-priority:low".to_owned(),
        ]);
        let safe = pr(30, "safe", "main", false);
        let status = status(unknown, vec![conflicting, safe]);
        let labels = crate::config::CaravanConfig::default().agent_priority_labels;
        let admission = resolve_admission(&status.analysis, &labels);
        assert_eq!(
            admission.next_candidate,
            Some(PrNumber(10)),
            "the rejected first attempt must block rather than leapfrog to #30"
        );
        assert_eq!(admission.rejected.len(), 2);
        assert!(
            admission
                .rejected
                .iter()
                .all(|candidate| candidate.reason.contains("fail closed"))
        );
    }

    #[test]
    fn unrelated_check_churn_does_not_stale_candidate_identity() {
        let discovered = pr(9, "nine", "main", false);
        let mut fresh = discovered.clone();
        fresh.checks.push(crate::model::CheckSnapshot {
            name: "ci".to_owned(),
            state: crate::model::CheckState::InProgress,
            provider_state: Some("IN_PROGRESS".to_owned()),
            details_url: None,
            ..crate::model::CheckSnapshot::default()
        });
        fresh.updated_at = Some("2026-01-02T00:00:00Z".to_owned());
        require_fresh_candidate(&discovered, &fresh)
            .expect("check/timestamp churn does not change candidate identity");
    }

    #[test]
    fn stale_head_between_discovery_and_refetch_fails_closed() {
        let discovered = pr(9, "nine", "main", false);
        let mut fresh = discovered.clone();
        fresh.head.oid = crate::model::CommitOid("f".repeat(40));
        let error = require_fresh_candidate(&discovered, &fresh)
            .expect_err("a moved provider head must invalidate the receipt");
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "stale_candidate_snapshot"
        );
        let details = mcp_cli::StructuredError::details(&error).unwrap();
        assert_eq!(details["expected_head_oid"], discovered.head.oid.0);
        assert_eq!(details["actual_head_oid"], fresh.head.oid.0);
        assert_eq!(details["mutated"], false);
    }

    /// bd-7546ea live pair: PR2228 head ccc3af5c is the direct parent of
    /// PR2235 head f0bae700, so an exact local proof must classify it as
    /// strict-prefix ancestry, never as divergence.
    #[test]
    fn local_ancestry_proves_direct_parent_child_instead_of_divergence() {
        let directory = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(directory.path())
                .args(args)
                .output()
                .expect("git fixture command");
            assert!(output.status.success(), "git {args:?} failed");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        run(&["init", "--quiet"]);
        run(&["config", "user.name", "Caravan Test"]);
        run(&["config", "user.email", "caravan@example.invalid"]);
        std::fs::write(directory.path().join("a"), "a\n").unwrap();
        run(&["add", "a"]);
        run(&["commit", "-m", "parent"]);
        let parent = crate::model::CommitOid(run(&["rev-parse", "HEAD"]));
        std::fs::write(directory.path().join("b"), "b\n").unwrap();
        run(&["add", "b"]);
        run(&["commit", "-m", "child"]);
        let child = crate::model::CommitOid(run(&["rev-parse", "HEAD"]));

        assert_eq!(
            local_commit_relation(directory.path(), &parent, &child),
            Some(crate::generation::CommitRelation::Ahead)
        );
        assert_eq!(
            local_commit_relation(directory.path(), &child, &parent),
            Some(crate::generation::CommitRelation::Behind)
        );
        assert_eq!(
            local_commit_relation(directory.path(), &parent, &parent),
            Some(crate::generation::CommitRelation::Identical)
        );
        // An absent object cannot be proven locally and must stay unproved.
        assert_eq!(
            local_commit_relation(
                directory.path(),
                &parent,
                &crate::model::CommitOid("0".repeat(40))
            ),
            None
        );
    }

    /// Reviewed operator resolution choice-019f9d34 (bd-afa02d) narrowed the
    /// relaxation to *intent* rather than blanket queue position. bd-7099e8
    /// restores the reviewed 0.0.9 shape it regressed: naming one exact remote
    /// PR is deliberate owner intent for `new` as well as `join`, so an
    /// unadmitted earlier unrelated row cannot wedge another owner.
    /// bd-d7aae7: next-candidate must name one exact command, and when nothing
    /// is selectable it must point at whatever blocks canonical order rather
    /// than leaving the operator to infer a fault.
    #[test]
    fn next_candidate_names_one_actionable_command() {
        let mut admission = AdmissionStatus {
            policy: String::new(),
            priority_labels: Vec::new(),
            generation_integrity: crate::generation::GenerationIntegrityStatus::default(),
            candidates: Vec::new(),
            skipped: Vec::new(),
            rejected: Vec::new(),
            next_candidate: Some(PrNumber(2113)),
        };

        let next = next_admission_command(&admission);
        assert_eq!(next.command, "cara check --pr 2113");
        assert_eq!(next.pr, Some(PrNumber(2113)));
        assert!(next.reason.contains("canonical first"));

        admission.next_candidate = None;
        admission.rejected.push(RejectedAdmissionCandidate {
            pr: PrNumber(2117),
            priority_rank: None,
            created_at: None,
            blocks_order: true,
            reason: "fail closed: conflicting priority labels".to_owned(),
        });

        let blocked = next_admission_command(&admission);
        assert_eq!(blocked.command, "cara check --pr 2117");
        assert_eq!(blocked.pr, Some(PrNumber(2117)));
        assert!(blocked.reason.contains("blocks canonical order"));

        admission.rejected.clear();
        let idle = next_admission_command(&admission);
        assert_eq!(idle.command, "cara status");
        assert_eq!(idle.pr, None);
    }

    #[test]
    fn explicit_remote_new_intent_is_admitted_with_ordering_evidence() {
        let first = pr(10, "first", "main", false);
        let second = pr(20, "second", "main", false);
        let status = status(first, vec![second]);
        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(20),
                tail_pr: None,
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("explicit remote new intent is admissible");
        assert!(output.eligible, "problems: {:?}", output.problems);
        assert!(output.problems.is_empty());
        assert!(!output.canonical_candidate);
        assert_eq!(output.next_action, CandidateNextAction::New);
        let note = output.admission_note.clone().expect("ordering evidence");
        assert!(note.contains("PR #20"));
        assert!(note.contains("PR #10"));
        let intent = output
            .admission_intent
            .clone()
            .expect("typed decision is emitted");
        assert_eq!(intent.intent, crate::admission::AdmissionIntent::New);
        assert_eq!(
            intent.selection,
            crate::admission::AdmissionSelection::Explicit
        );
        assert_eq!(
            intent.outcome,
            crate::admission::AdmissionOrderOutcome::ExplicitAheadOfUnjoined
        );
        assert_eq!(intent.bypassed_unjoined_prs, vec![PrNumber(10)]);
        assert!(
            note.contains(&intent.reason),
            "the human note is derived from the typed decision so they cannot disagree"
        );
        assert_eq!(
            status.admission.next_candidate,
            Some(PrNumber(10)),
            "the bypassed row keeps its canonical first-admission position"
        );
        let json = serde_json::to_value(&output).expect("remote receipt serializes");
        assert_eq!(json["next_action"], "new");
        assert_eq!(json["candidate"]["number"], 20);
        assert_eq!(json["canonical_candidate"], false);
        assert_eq!(json["admission_intent"]["selection"], "explicit");
        assert!(json.get("merge_candidate").is_none());
    }

    /// The same non-canonical PR is admissible the moment it declares explicit
    /// join intent to a valid live target, while the bypassed row stays ordered.
    #[test]
    fn explicit_remote_join_intent_admits_the_same_noncanonical_pr() {
        let root = pr(1, "root", "main", true);
        let first = pr(10, "first", "main", false);
        let second = pr(20, "second", "main", false);
        let status = status(second, vec![root, first]);
        assert_eq!(status.admission.next_candidate, Some(PrNumber(10)));

        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(20),
                tail_pr: Some(1),
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("explicit join intent is admissible");

        assert!(output.eligible, "problems: {:?}", output.problems);
        assert_eq!(output.next_action, CandidateNextAction::Join);
        assert!(!output.canonical_candidate);
        assert!(output.admission_note.is_some());
        let intent = output.admission_intent.expect("typed decision is emitted");
        assert_eq!(
            intent.outcome,
            crate::admission::AdmissionOrderOutcome::ExplicitAheadOfUnjoined
        );
        assert_eq!(intent.bypassed_unjoined_prs, vec![PrNumber(10)]);
        assert_eq!(
            status.admission.next_candidate,
            Some(PrNumber(10)),
            "the bypassed row keeps its canonical first-admission position"
        );
    }

    /// A local `check` on the owner's own checked-out PR is checked-out owner
    /// selection: canonical order is evidence, never a gate, exactly as it was
    /// in 0.0.8/0.0.9/0.0.10. The typed decision says so instead of claiming a
    /// block the receipt does not apply.
    #[test]
    fn local_checked_out_receipt_reports_order_as_evidence_only() {
        let candidate = pr(2179, "child", "parent", false);
        let status = status(
            candidate,
            vec![
                pr(2050, "unrelated", "main", false),
                pr(2100, "parent", "main", false),
            ],
        );

        let output = check_analysis(&status, &CheckInput::default(), &clean_checker)
            .expect("a local owner check is never gated by canonical order");

        assert!(output.eligible, "problems: {:?}", output.problems);
        assert_eq!(output.next_action, CandidateNextAction::New);
        assert!(
            output.admission_note.is_none(),
            "ordering evidence is a remote-selection note"
        );
        let intent = output.admission_intent.expect("typed decision is emitted");
        assert_eq!(
            intent.selection,
            crate::admission::AdmissionSelection::CheckedOut
        );
        assert_eq!(
            intent.outcome,
            crate::admission::AdmissionOrderOutcome::OwnerSelected
        );
        assert!(
            intent.order_permits_admission(),
            "the decision must agree with the receipt it is bound to"
        );
        assert_eq!(intent.blocking_prs, vec![PrNumber(2100)]);
        assert!(intent.reason.contains("evidence only"));
    }

    #[test]
    fn ineligible_noncanonical_pr_still_fails_closed() {
        let first = pr(10, "first", "main", false);
        let mut draft = pr(20, "second", "main", false);
        draft.draft = true;
        let status = status(first, vec![draft]);
        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(20),
                tail_pr: None,
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("rejection remains an inspectable receipt");
        assert!(!output.eligible);
        assert_ne!(output.next_action, CandidateNextAction::New);
    }

    #[test]
    fn canonical_provider_staleness_returns_wait_receipt() {
        let candidate = pr(9, "nine", "main", false);
        let mut status = status(candidate.clone(), Vec::new());
        status
            .merge_candidates
            .push(crate::model::MergeCandidateIdentity {
                pr: candidate.number,
                provider_updated_at: "2026-01-01T00:00:00Z".to_owned(),
                observed_at: "2026-01-01T00:00:01Z".to_owned(),
                base: candidate.base.clone(),
                head: candidate.head.clone(),
                synthetic: None,
                auto_merge: crate::model::NativeAutoMergeState {
                    enabled: false,
                    merge_method: None,
                    actor: None,
                },
                freshness: crate::model::MergeCandidateFreshness::StaleHead,
                compared_base: None,
                stale_base: false,
                stale_head: true,
                stale_reasons: vec!["synthetic head parent is stale".to_owned()],
            });
        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(9),
                tail_pr: None,
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("provider staleness is an inspectable remote receipt");
        assert!(!output.eligible);
        assert_eq!(output.next_action, CandidateNextAction::Wait);
        assert_eq!(
            output
                .merge_candidate
                .as_ref()
                .map(|identity| identity.freshness),
            Some(crate::model::MergeCandidateFreshness::StaleHead)
        );
    }

    #[test]
    fn physical_admission_uses_exact_git_proof_when_synthetic_base_is_stale() {
        let candidate = pr(9, "nine", "main", false);
        let mut status = status(candidate.clone(), Vec::new());
        status.rebase_on_join = RebaseOnJoinStatus {
            enabled: true,
            state: "enabled".to_owned(),
            config_path: ".caravan/config.yaml".to_owned(),
            required_action: None,
        };
        status
            .merge_candidates
            .push(crate::model::MergeCandidateIdentity {
                pr: candidate.number,
                provider_updated_at: "2026-01-01T00:00:00Z".to_owned(),
                observed_at: "2026-01-01T00:00:01Z".to_owned(),
                base: candidate.base.clone(),
                head: candidate.head.clone(),
                synthetic: None,
                auto_merge: crate::model::NativeAutoMergeState {
                    enabled: false,
                    merge_method: None,
                    actor: None,
                },
                freshness: crate::model::MergeCandidateFreshness::StaleBase,
                compared_base: None,
                stale_base: true,
                stale_head: false,
                stale_reasons: vec!["synthetic base parent is stale".to_owned()],
            });

        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(9),
                tail_pr: None,
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("physical mode owns a fresh exact Git/lease preflight");

        assert!(output.eligible);
        assert_eq!(output.next_action, CandidateNextAction::New);
        assert!(output.problems.is_empty());
        assert_eq!(
            output
                .merge_candidate
                .as_ref()
                .map(|identity| identity.freshness),
            Some(crate::model::MergeCandidateFreshness::StaleBase)
        );
    }

    /// Explicit join intent attaches ahead of an older unrelated unjoined row
    /// while that row keeps its canonical first-admission position.
    #[test]
    fn explicit_join_is_admitted_ahead_of_older_unjoined_fifo_rows() {
        let candidate = pr(2179, "green", "main", false);
        let status = status(
            candidate,
            vec![
                pr(1, "root", "main", true),
                pr(2113, "old-unjoined", "main", false),
            ],
        );
        assert_eq!(status.admission.next_candidate, Some(PrNumber(2113)));

        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2179),
                tail_pr: Some(1),
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("explicit join intent is evaluated before FIFO rejection");

        assert!(output.eligible, "problems: {:?}", output.problems);
        assert_eq!(output.next_action, CandidateNextAction::Join);
        assert_eq!(output.mode, CheckMode::JoinTail);
        assert!(!output.canonical_candidate);
        let intent = output.admission_intent.expect("typed decision is emitted");
        assert_eq!(intent.intent, crate::admission::AdmissionIntent::Join);
        assert_eq!(
            intent.outcome,
            crate::admission::AdmissionOrderOutcome::ExplicitAheadOfUnjoined
        );
        assert_eq!(intent.target_caravan, Some(PrNumber(1)));
        assert_eq!(intent.bypassed_unjoined_prs, vec![PrNumber(2113)]);
        assert!(intent.blocking_prs.is_empty());
        assert!(intent.compatibility_clean && intent.preflight_clean);
        assert!(!intent.provider_mutated && !intent.idempotent);
        assert_eq!(
            status.admission.next_candidate,
            Some(PrNumber(2113)),
            "bypassed FIFO rows keep their canonical order"
        );
    }

    /// FIFO still governs *automatic* selection: the ordered attempt list and
    /// canonical candidate are unchanged by any explicit owner receipt.
    #[test]
    fn explicit_intent_never_reorders_the_automatic_fifo_queue() {
        let candidate = pr(2179, "green", "main", false);
        let status = status(candidate, vec![pr(2113, "old-unjoined", "main", false)]);
        let before = status.admission.clone();

        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2179),
                tail_pr: None,
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("explicit new intent is admissible");

        assert!(output.eligible, "problems: {:?}", output.problems);
        assert_eq!(output.next_action, CandidateNextAction::New);
        assert_eq!(status.admission, before, "admission ordering is read-only");
        assert_eq!(status.admission.next_candidate, Some(PrNumber(2113)));
        assert_eq!(
            status
                .admission
                .candidates
                .iter()
                .map(|row| row.pr)
                .collect::<Vec<_>>(),
            vec![PrNumber(2113), PrNumber(2179)],
            "the canonical order the automatic surface publishes is unchanged"
        );
        // The separate automatic-selection axis still fails closed on the same
        // non-canonical candidate.
        let automatic = crate::admission::evaluate(
            &status.admission,
            &status.analysis,
            &status.analysis.pull_requests[&PrNumber(2179)],
            None,
            crate::admission::AdmissionSelection::Automatic,
        );
        assert_eq!(
            automatic.outcome,
            crate::admission::AdmissionOrderOutcome::BlockedByOrder
        );
        assert!(!automatic.order_permits_admission());
    }

    /// Cacophony generation4 PR2213 A/B shape: zero caravans, one older
    /// unadmitted unrelated row, explicit `cara check --pr 2213`. Reviewed
    /// 0.0.9 returned eligible/new with no problems; 0.0.10 regressed it to
    /// reject. This pins the reviewed shape byte for byte.
    #[test]
    fn cacophony_generation4_pr2213_explicit_new_matches_the_reviewed_ab_shape() {
        let candidate = pr(2213, "generation4", "main", false);
        let status = status(candidate, vec![pr(2113, "old-unjoined", "main", false)]);
        assert_eq!(status.admission.next_candidate, Some(PrNumber(2113)));

        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2213),
                tail_pr: None,
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("reviewed 0.0.9 admitted this explicit intent");

        assert!(output.eligible);
        assert_eq!(output.next_action, CandidateNextAction::New);
        assert!(output.problems.is_empty());
        assert_eq!(output.mode, CheckMode::NewCaravan);
        assert!(!output.canonical_candidate);
        assert!(output.admission_note.is_some());
        let intent = output.admission_intent.expect("typed decision is emitted");
        assert_eq!(
            intent.outcome,
            crate::admission::AdmissionOrderOutcome::ExplicitAheadOfUnjoined
        );
        assert_eq!(intent.bypassed_unjoined_prs, vec![PrNumber(2113)]);
        assert!(intent.blocking_prs.is_empty());
    }

    /// Cacophony PR2215 A/B shape, updated by bd-c462db: with a live caravan,
    /// targetless check now recommends the same coherent join as an explicit
    /// `--tail-pr`; both leave PR #2113 canonical.
    #[test]
    fn cacophony_pr2215_targetless_and_explicit_join_match_the_reviewed_shape() {
        let candidate = pr(2215, "green", "main", false);
        let status = status(
            candidate,
            vec![
                pr(1, "root", "main", true),
                pr(2113, "old-unjoined", "main", false),
            ],
        );
        assert_eq!(status.admission.next_candidate, Some(PrNumber(2113)));

        for (input, expected_action, expected_mode) in [
            (
                CheckInput {
                    pr: Some(2215),
                    tail_pr: None,
                    head_pr: None,
                },
                CandidateNextAction::Join,
                CheckMode::JoinTail,
            ),
            (
                CheckInput {
                    pr: Some(2215),
                    tail_pr: Some(1),
                    head_pr: None,
                },
                CandidateNextAction::Join,
                CheckMode::JoinTail,
            ),
        ] {
            let output = check_analysis(&status, &input, &clean_checker)
                .expect("explicit owner intent is admissible");

            assert!(output.eligible, "problems: {:?}", output.problems);
            assert!(output.problems.is_empty());
            assert_eq!(output.next_action, expected_action);
            assert_eq!(output.mode, expected_mode);
            assert!(!output.canonical_candidate);
            let intent = output.admission_intent.expect("typed decision is emitted");
            assert_eq!(
                intent.outcome,
                crate::admission::AdmissionOrderOutcome::ExplicitAheadOfUnjoined
            );
            assert_eq!(intent.bypassed_unjoined_prs, vec![PrNumber(2113)]);
            assert_eq!(
                status.admission.next_candidate,
                Some(PrNumber(2113)),
                "bypassed FIFO rows keep their canonical order"
            );
        }
    }

    /// Explicit intent still fails closed on rows it may not pass: a base-chain
    /// dependency of the candidate is never bypassed for `new` or `join`.
    #[test]
    fn explicit_new_intent_still_fails_closed_on_a_dependency_row() {
        let candidate = pr(2179, "child", "parent", false);
        let status = status(
            candidate,
            vec![
                pr(2050, "unrelated", "main", false),
                pr(2100, "parent", "main", false),
            ],
        );

        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2179),
                tail_pr: None,
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("non-admissible order is an inspectable receipt");

        assert!(!output.eligible);
        assert_eq!(output.next_action, CandidateNextAction::Reject);
        let note = output.admission_note.clone().expect("ordering evidence");
        let intent = output.admission_intent.expect("typed decision is emitted");
        assert_eq!(
            intent.outcome,
            crate::admission::AdmissionOrderOutcome::BlockedByOrder
        );
        assert_eq!(intent.blocking_prs, vec![PrNumber(2100)]);
        assert_eq!(intent.bypassed_unjoined_prs, vec![PrNumber(2050)]);
        assert!(
            note.contains(&intent.reason),
            "the refusal note names the same blocking reason the decision carries"
        );
        assert!(
            output
                .problems
                .iter()
                .any(|problem| problem.message.contains("fails closed"))
        );
    }

    /// A conflicting explicit join is rejected even though ordering allowed it.
    #[test]
    fn conflicting_explicit_join_is_rejected_with_bound_evidence() {
        let candidate = pr(2179, "green", "main", false);
        let status = status(
            candidate,
            vec![
                pr(1, "root", "main", true),
                pr(2113, "old-unjoined", "main", false),
            ],
        );
        let conflict = |candidate: &crate::model::BranchSnapshot,
                        target: &crate::model::BranchSnapshot| {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: CompatibilityOutcome::Conflict,
                conflicting_paths: vec!["src/lib.rs".to_owned()],
                diagnostic: None,
            })
        };

        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2179),
                tail_pr: Some(1),
                head_pr: None,
            },
            &conflict,
        )
        .expect("remote rejection is an inspectable receipt");

        assert!(!output.eligible);
        assert_eq!(output.next_action, CandidateNextAction::Repair);
        let intent = output.admission_intent.expect("typed decision is emitted");
        assert_eq!(
            intent.outcome,
            crate::admission::AdmissionOrderOutcome::BlockedByPreflight
        );
        assert!(!intent.compatibility_clean);
        assert!(intent.reason.contains("exact preflight rejected"));
    }

    /// A conflicting attachment preflight carries the exact squash-equivalence
    /// evidence for the same revisions, and a clean one carries none.
    #[test]
    fn conflicting_check_carries_squash_equivalence_evidence() {
        struct ReconcilableChecker;
        impl CompatibilityChecker for ReconcilableChecker {
            fn check(
                &self,
                candidate: &crate::model::BranchSnapshot,
                target: &crate::model::BranchSnapshot,
            ) -> Result<CompatibilityReport, AppError> {
                Ok(CompatibilityReport {
                    candidate: candidate.clone(),
                    target: target.clone(),
                    outcome: CompatibilityOutcome::Conflict,
                    conflicting_paths: vec!["src/lib.rs".to_owned()],
                    diagnostic: None,
                })
            }

            fn squash_equivalence(
                &self,
                candidate: &crate::model::BranchSnapshot,
                target: &crate::model::BranchSnapshot,
            ) -> Result<Option<SquashEquivalenceReport>, AppError> {
                Ok(Some(SquashEquivalenceReport {
                    schema_version: 1,
                    candidate: candidate.clone(),
                    target: target.clone(),
                    candidate_oid: candidate.oid.clone(),
                    target_oid: target.oid.clone(),
                    merge_base: None,
                    outcome: crate::squash_equivalence::SquashEquivalenceOutcome::NoEquivalence,
                    before: None,
                    after: None,
                    proven_boundary: None,
                    boundary_tree: None,
                    target_tree: None,
                    commits: Vec::new(),
                    represented_paths: Vec::new(),
                    represented_paths_truncated: false,
                    candidate_commit_count: 2,
                    analyzed_prefix_complete: true,
                    evaluated_boundaries: 0,
                    evaluation_bounded: false,
                    reason: "ordinary three-way divergence".to_owned(),
                    policy: crate::squash_equivalence::SQUASH_EQUIVALENCE_POLICY.to_owned(),
                }))
            }
        }

        let candidate = pr(2227, "tail", "main", false);
        let status = status(candidate, vec![pr(1, "root", "main", true)]);

        let conflicting = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2227),
                tail_pr: Some(1),
                head_pr: None,
            },
            &ReconcilableChecker,
        )
        .expect("rejection is an inspectable receipt");

        assert!(!conflicting.eligible);
        assert!(!conflicting.squash_reconciliations.is_empty());
        assert!(
            conflicting
                .squash_reconciliations
                .iter()
                .all(|report| report.authorized_range_base().is_none()),
            "unproven evidence never authorizes a boundary"
        );

        let clean = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2227),
                tail_pr: Some(1),
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("clean receipt");
        assert!(clean.squash_reconciliations.is_empty());
    }

    /// An unresolved or ambiguous join target fails closed before ordering.
    #[test]
    fn ambiguous_or_missing_join_target_never_relaxes_order() {
        let candidate = pr(2179, "green", "main", false);
        let status = status(
            candidate,
            vec![
                pr(1, "root", "main", true),
                pr(3, "other-root", "main", true),
                pr(2113, "old-unjoined", "main", false),
            ],
        );

        let error = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2179),
                tail_pr: Some(2113),
                head_pr: None,
            },
            &clean_checker,
        )
        .expect_err("an unjoined PR is not a caravan tail");

        assert_eq!(error.code(), "caravan_tail_not_found");
    }

    /// A stale pinned provider candidate still blocks explicit join intent.
    #[test]
    fn stale_candidate_still_blocks_explicit_join_intent() {
        let candidate = pr(2179, "green", "main", false);
        let mut status = status(
            candidate.clone(),
            vec![
                pr(1, "root", "main", true),
                pr(2113, "old-unjoined", "main", false),
            ],
        );
        status
            .merge_candidates
            .push(crate::model::MergeCandidateIdentity {
                pr: candidate.number,
                provider_updated_at: "2026-01-01T00:00:00Z".to_owned(),
                observed_at: "2026-01-01T00:00:01Z".to_owned(),
                base: candidate.base.clone(),
                head: candidate.head.clone(),
                synthetic: None,
                auto_merge: crate::model::NativeAutoMergeState {
                    enabled: false,
                    merge_method: None,
                    actor: None,
                },
                freshness: crate::model::MergeCandidateFreshness::StaleHead,
                compared_base: None,
                stale_base: false,
                stale_head: true,
                stale_reasons: vec!["synthetic head parent is stale".to_owned()],
            });

        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2179),
                tail_pr: Some(1),
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("stale rejection is an inspectable receipt");

        assert!(!output.eligible);
        assert_eq!(output.next_action, CandidateNextAction::Wait);
        let intent = output.admission_intent.expect("typed decision is emitted");
        assert_eq!(
            intent.outcome,
            crate::admission::AdmissionOrderOutcome::BlockedByPreflight
        );
    }

    /// A provider/compatibility failure propagates instead of being bypassed.
    #[test]
    fn provider_failure_during_join_preflight_propagates() {
        let candidate = pr(2179, "green", "main", false);
        let status = status(
            candidate,
            vec![
                pr(1, "root", "main", true),
                pr(2113, "old-unjoined", "main", false),
            ],
        );
        let failing = |_candidate: &crate::model::BranchSnapshot,
                       _target: &crate::model::BranchSnapshot| {
            Err(AppError::structured(
                ErrorCategory::ExecutionFailure,
                "compatibility_provider_failed",
                "injected provider failure",
                None,
            ))
        };

        let error = check_analysis(
            &status,
            &CheckInput {
                pr: Some(2179),
                tail_pr: Some(1),
                head_pr: None,
            },
            &failing,
        )
        .expect_err("provider failure is never an admission bypass");

        assert_eq!(error.code(), "compatibility_provider_failed");
    }

    #[test]
    fn remote_draft_receipt_waits_on_the_canonical_candidate() {
        let mut candidate = pr(9, "nine", "main", false);
        candidate.draft = true;
        let status = status(candidate, Vec::new());
        let output = check_analysis(
            &status,
            &CheckInput {
                pr: Some(9),
                tail_pr: None,
                head_pr: None,
            },
            &clean_checker,
        )
        .expect("remote draft rejection is an inspectable receipt");
        assert!(!output.eligible);
        assert_eq!(output.next_action, CandidateNextAction::Wait);
        assert_eq!(output.candidate.number, PrNumber(9));
    }

    #[test]
    fn repository_label_inventory_is_cached_without_becoming_mutation_state() {
        let repository = repository();
        if let Some(cache) = LABEL_CACHE.get() {
            cache.lock().unwrap().remove(&repository.slug());
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = crate::github::GitHubMutationAdapter::new(LabelRunner(Arc::clone(&calls)));
        let (first, first_hit) = repository_labels_cached(&provider, &repository).unwrap();
        let (second, second_hit) = repository_labels_cached(&provider, &repository).unwrap();
        assert!(!first_hit);
        assert!(second_hit);
        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn short_lived_status_cache_reports_hits_and_invalidates_exact_repository() {
        let directory = tempfile::tempdir().unwrap();
        let context = crate::AppContext {
            repository_path: directory.path().to_path_buf(),
            config_path: directory.path().join(".caravan/config.yaml"),
            config_existed: false,
            config: crate::config::CaravanConfig::default(),
        };
        let key = status_cache_key(&context);
        let expected = status(pr(9, "nine", "main", false), Vec::new());
        STATUS_CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .insert(
                key.clone(),
                CachedStatus {
                    inserted: Instant::now(),
                    output: expected,
                },
            );
        let cached = status_cached(&context, Duration::from_secs(5)).unwrap();
        assert_eq!(cached.provider_api.cache_hits, 1);
        assert!(cached.provider_api.cache_age_ms.is_some());
        invalidate_status_cache(&context);
        assert!(
            !STATUS_CACHE
                .get()
                .unwrap()
                .lock()
                .unwrap()
                .contains_key(&key)
        );
    }

    /// bd-59f6d2: the complete open-PR provider list may briefly retain the
    /// old candidate head after Cara's exact lease push. Post-rewrite status
    /// binds both graph and generation analysis to the verified new head before
    /// compatibility, including the `--create-pr` path whose original selector
    /// was absent.
    #[test]
    fn post_rewrite_binding_replaces_the_created_prs_stale_provider_head() {
        let candidate = pr(9, "candidate", "main", false);
        let old_head = candidate.head.oid.clone();
        let expected = crate::model::CommitOid("rewritten-candidate-head".to_owned());
        let mut snapshot = crate::model::RepositorySnapshot {
            repository: repository(),
            default_branch: branch("main"),
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            current_branch: Some(candidate.head.name.clone()),
            current_pr: Some(candidate.number),
            pull_requests: vec![candidate.clone()],
            generation_facts: vec![crate::model::PullRequestGenerationFact {
                pr: candidate.number,
                provider_head: old_head.clone(),
                created_at: None,
                provenance: None,
                metadata_error: None,
                supersedes: BTreeSet::new(),
            }],
            observed_at: None,
        };

        bind_expected_candidate_head(
            &mut snapshot,
            &ExpectedCandidateHead {
                pr: candidate.number,
                branch: candidate.head.name.clone(),
                oid: expected.clone(),
            },
        )
        .expect("exact post-push generation replaces only the stale head fact");

        assert_ne!(old_head, expected);
        assert_eq!(snapshot.current_pr, Some(candidate.number));
        assert_eq!(snapshot.pull_requests[0].head.oid, expected);
        assert_eq!(snapshot.generation_facts[0].provider_head, expected);
        assert_eq!(snapshot.pull_requests[0].base, candidate.base);
        assert_eq!(snapshot.pull_requests[0].labels, candidate.labels);
    }

    #[test]
    fn caravan_stack_backend_is_default_and_performs_no_capability_probe() {
        let status = status(pr(9, "nine", "main", false), Vec::new());

        let backend = stack_backend_status(
            crate::config::StackType::Caravan,
            &StackProviderMustNotBeCalled,
            &status.repository,
            &status.analysis,
        );

        assert_eq!(backend.configured, crate::config::StackType::Caravan);
        assert_eq!(backend.capability, StackCapability::NotProbed);
        assert_eq!(backend.mutation_support, StackMutationSupport::Caravan);
    }

    #[test]
    fn github_stack_read_model_binds_exact_member_order_bases_and_heads() {
        let root = pr(1, "root", "main", true);
        let child = pr(2, "child", "root", true);
        let status = status(child.clone(), vec![root.clone(), child.clone()]);
        let provider = FixedStackProvider(Ok(crate::github::GitHubStackInventory {
            truncated: false,
            stacks: vec![crate::github::GitHubStackSnapshot {
                id: 9001,
                number: 42,
                node_id: "S_stack".to_owned(),
                base: crate::github::GitHubStackBase {
                    ref_name: "main".to_owned(),
                },
                open: true,
                created_at: "2026-07-31T10:00:00Z".to_owned(),
                pull_requests: vec![
                    crate::github::GitHubStackPullRequest {
                        number: 1,
                        state: "open".to_owned(),
                        draft: false,
                        merged_at: None,
                        head: crate::github::GitHubStackPullRequestHead {
                            ref_name: root.head.name.clone(),
                            sha: root.head.oid.clone(),
                        },
                    },
                    crate::github::GitHubStackPullRequest {
                        number: 2,
                        state: "open".to_owned(),
                        draft: false,
                        merged_at: None,
                        head: crate::github::GitHubStackPullRequestHead {
                            ref_name: child.head.name.clone(),
                            sha: child.head.oid.clone(),
                        },
                    },
                ],
            }],
        }));

        let backend = stack_backend_status(
            crate::config::StackType::Github,
            &provider,
            &status.repository,
            &status.analysis,
        );

        assert_eq!(backend.capability, StackCapability::Available);
        assert_eq!(
            backend.mutation_support,
            StackMutationSupport::ReadOnlyPreview
        );
        assert_eq!(backend.native_stacks[0].caravan_id, Some(PrNumber(1)));
        assert_eq!(
            backend.native_stacks[0].consistency,
            StackConsistency::Exact
        );
        assert!(backend.problems.is_empty());
    }

    #[test]
    fn truncated_stack_inventory_never_infers_absent_caravans() {
        let root = pr(1, "root", "main", true);
        let child = pr(2, "child", "root", true);
        let status = status(child.clone(), vec![root, child]);
        let provider = FixedStackProvider(Ok(crate::github::GitHubStackInventory {
            stacks: Vec::new(),
            truncated: true,
        }));

        let backend = stack_backend_status(
            crate::config::StackType::Github,
            &provider,
            &status.repository,
            &status.analysis,
        );

        assert!(backend.missing_caravans.is_empty());
        assert!(
            backend
                .problems
                .iter()
                .any(|problem| problem.code == "github_stack_inventory_truncated")
        );
    }

    #[test]
    fn github_stack_opt_in_and_capability_open_only_the_native_backend() {
        let available = StackBackendStatus {
            capability: StackCapability::Available,
            ..StackBackendStatus::default()
        };
        let caravan_config = crate::config::CaravanConfig::default();
        let mut opted_in = caravan_config.clone();
        opted_in.stack_type = crate::config::StackType::Github;
        opted_in.stack_rollout.mutations_opt_in = true;
        opted_in.stack_rollout.reviewed_by = "operator".to_owned();
        let mut not_opted_in = opted_in.clone();
        not_opted_in.stack_rollout = crate::config::StackRolloutConfig::default();

        let mut stable = crate::initialization::InitializationStatus::default();
        apply_stack_backend_mutation_policy(&caravan_config, &available, &mut stable);
        assert!(stable.ready);
        assert!(stable.mutation_blocker.is_none());

        // bd-a79679: the allowlist is necessary; proven capability is the
        // independent second gate.
        let mut unlisted = crate::initialization::InitializationStatus::default();
        apply_stack_backend_mutation_policy(&not_opted_in, &available, &mut unlisted);
        assert!(!unlisted.ready);
        assert_eq!(
            unlisted.mutation_blocker.unwrap().code,
            "github_stack_repository_not_opted_in"
        );

        let mut executable = crate::initialization::InitializationStatus::default();
        apply_stack_backend_mutation_policy(&opted_in, &available, &mut executable);
        assert!(executable.ready);
        assert!(executable.mutation_blocker.is_none());

        // An unproven or unavailable capability is never absence, and outranks
        // even an explicit repository opt-in.
        for (capability, code) in [
            (
                StackCapability::Unavailable,
                "github_stack_capability_unavailable",
            ),
            (StackCapability::Unknown, "github_stack_capability_unknown"),
        ] {
            let backend = StackBackendStatus {
                capability,
                ..StackBackendStatus::default()
            };
            let mut blocked = crate::initialization::InitializationStatus::default();
            apply_stack_backend_mutation_policy(&opted_in, &backend, &mut blocked);
            assert!(!blocked.ready);
            assert_eq!(blocked.mutation_blocker.unwrap().code, code);
        }
    }

    #[test]
    fn github_stack_capability_failure_is_typed_without_faking_absence() {
        let status = status(pr(9, "nine", "main", false), Vec::new());
        let provider = FixedStackProvider(Err(crate::github::GitHubStackReadError::Unavailable {
            diagnostic: "Not Found (HTTP 404)".to_owned(),
        }));

        let backend = stack_backend_status(
            crate::config::StackType::Github,
            &provider,
            &status.repository,
            &status.analysis,
        );

        assert_eq!(backend.capability, StackCapability::Unavailable);
        assert_eq!(backend.problems[0].code, "github_stacks_unavailable");
    }

    #[test]
    fn helper_status_keeps_all_pull_requests() {
        let candidate = pr(9, "nine", "main", false);
        let status = status(candidate, vec![pr(1, "one", "main", true)]);
        let numbers: BTreeMap<_, _> = status
            .analysis
            .pull_requests
            .iter()
            .map(|(number, pull_request)| (*number, pull_request.title.clone()))
            .collect();
        assert_eq!(numbers.len(), 2);
    }
}
