//! Cara-owned root promotion and direct squash merge.
//!
//! A caravan root must reach the *default branch*, and only the default branch.
//! Delegating that to provider-native auto-merge made the provider the merge
//! actor while Cara still owned the topology, and the two raced: a root armed
//! while its base was still a merged predecessor branch merged instantly into
//! that predecessor, so its content never reached the default branch at all
//! (live incident: PR2210 merged to `main`, PR2213 then merged into PR2210's
//! already-merged generation branch, and PR2215 inherited the cumulative
//! content plus a dangling base).
//!
//! This module owns the replacement contract: **one merge actor, one ordered
//! fenced transaction per tick**.
//!
//! 1. Re-read the exact current root generation from the provider.
//! 2. If the root does not already target the exact default branch — in
//!    particular when its predecessor is merged — retarget it *first*.
//! 3. Re-read and prove base/ref/head after the retarget, and re-validate
//!    required CI for the *new* merge identity. A head proven green against a
//!    predecessor base is not proven green against the default branch.
//! 4. Only then perform exactly one SQUASH merge, fenced on the exact head.
//! 5. Persist the provider merge receipt with cumulative ancestry proof and
//!    promote the next root by retargeting it to the default branch.
//!
//! Nothing here ever arms provider auto-merge. When
//! [`HeadMergePolicy::NativeAutoMerge`](crate::model::HeadMergePolicy) is
//! explicitly configured the historical arming path in
//! [`crate::root_auto_merge`] still applies, but the caravan-owned policy is the
//! default and is the only one that can prove where a root actually landed.

use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::{
    AutoMergeState, BranchSnapshot, CommitOid, CumulativeTreeProof, MergeMethod, OperationId,
    PrNumber, PullRequestSnapshot, PullRequestState, RepositoryId,
};

/// Stable schema for durable root promotion receipts.
pub const ROOT_PROMOTION_RECEIPT_SCHEMA_VERSION: u32 = 1;

/// Stable schema for durable direct root merge receipts.
pub const ROOT_MERGE_RECEIPT_SCHEMA_VERSION: u32 = 1;

/// Bounded provider confirmation reads per fenced transaction step.
pub const ROOT_MERGE_CONFIRMATION_READS: u32 = 3;

/// Delay between bounded confirmation reads absorbing provider read lag.
pub const ROOT_MERGE_CONFIRMATION_DELAY: Duration = Duration::from_millis(250);

/// Stable owner recorded on every engine-performed promotion/merge.
pub const ROOT_MERGE_OWNER: &str = "caravan-scheduler";

/// Stable component recorded on every engine-performed promotion/merge.
pub const ROOT_MERGE_COMPONENT: &str = "cara sync";

/// Why the promoted root required a base transition this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RootPromotionTrigger {
    /// The exact current root already targets the exact default branch.
    AlreadyOnDefaultBranch,
    /// The root still targets a predecessor branch that is already merged.
    MergedPredecessorRetarget,
    /// The root targets some other non-default branch intended only for child
    /// stacking.
    NonDefaultBaseRetarget,
}

impl RootPromotionTrigger {
    /// Stable human explanation retained on receipts and events.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::AlreadyOnDefaultBranch => {
                "exact current caravan root already targets the default branch"
            }
            Self::MergedPredecessorRetarget => {
                "caravan root still targeted an already-merged predecessor branch; the default branch is the only valid merge target"
            }
            Self::NonDefaultBaseRetarget => {
                "caravan root targeted a non-default branch intended only for child stacking"
            }
        }
    }

    /// Whether this trigger requires a provider write to converge.
    #[must_use]
    pub const fn requires_write(self) -> bool {
        !matches!(self, Self::AlreadyOnDefaultBranch)
    }
}

/// Typed cause when root promotion could not be proven this tick.
///
/// Every one of these fails *before* any merge is attempted, so a root whose
/// base is unproven can never be merged into the wrong target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RootPromotionFailureCause {
    /// The provider accepted the retarget but never exposed the default branch
    /// as the root's base within the bounded confirmation reads.
    BaseRetargetNotObserved,
    /// The root head moved while this tick was promoting it, so the promotion
    /// proof would belong to a superseded generation.
    RootHeadMovedDuringPromotion,
    /// The provider view did not converge with the exact generation this tick
    /// already verified within the bounded confirmation reads.
    StaleProviderView,
}

impl RootPromotionFailureCause {
    /// Stable code embedded in structured error details.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::BaseRetargetNotObserved => "base_retarget_not_observed",
            Self::RootHeadMovedDuringPromotion => "root_head_moved_during_promotion",
            Self::StaleProviderView => "stale_provider_view",
        }
    }

    /// Deterministic next action for a bounded scheduler, never an operator ask.
    #[must_use]
    pub const fn next(self) -> &'static str {
        match self {
            Self::BaseRetargetNotObserved => {
                "rerun the same idempotent bounded sync tick; root promotion converges without operator action"
            }
            Self::RootHeadMovedDuringPromotion => {
                "rerun the same idempotent bounded sync tick against the fresh root generation"
            }
            Self::StaleProviderView => {
                "rerun the same idempotent bounded sync tick once provider reads agree"
            }
        }
    }
}

/// Why a tick declined to merge an otherwise promoted root. These are ordinary
/// bounded waits, not failures: the next tick re-reads and re-decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RootMergeBlock {
    /// Checks on the exact current head are not all successful yet.
    ChecksNotPassing,
    /// Required contexts for the *current* base are not satisfied. A head proven
    /// green against a predecessor base is not proven green against the default
    /// branch, so promotion deliberately re-opens this gate.
    RequiredRunsNotSatisfied,
    /// The root is not an open, non-draft pull request.
    NotOpen,
    /// The exact head is not proven conflict-free with the exact default branch.
    NotConflictFreeWithDefault,
    /// The root already merged before this tick reached the merge step.
    AlreadyMerged,
    /// No cumulative-tree proof was available for this exact generation, so the
    /// tick cannot show that the squash lands the already-validated tree.
    CumulativeTreeUnproven,
    /// The default branch gained content this generation never saw, so the
    /// squash would land a tree CI never validated. The chain revalidates
    /// (physical rebase plus fresh CI) instead of merging.
    CumulativeTreeChanged,
    /// The bounded per-tick merge allowance is spent.
    MergeBudgetReached,
}

impl RootMergeBlock {
    /// Stable code embedded in receipts, steps, and structured details.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ChecksNotPassing => "checks_not_passing",
            Self::RequiredRunsNotSatisfied => "required_runs_not_satisfied",
            Self::NotOpen => "not_open",
            Self::NotConflictFreeWithDefault => "not_conflict_free_with_default",
            Self::AlreadyMerged => "already_merged",
            Self::CumulativeTreeUnproven => "cumulative_tree_unproven",
            Self::CumulativeTreeChanged => "cumulative_tree_changed",
            Self::MergeBudgetReached => "merge_budget_reached",
        }
    }

    /// Stable human explanation retained on the visible no-op step.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::ChecksNotPassing => {
                "exact current root head has no complete successful check set yet"
            }
            Self::RequiredRunsNotSatisfied => {
                "required contexts for the root's exact current base are not satisfied yet"
            }
            Self::NotOpen => "caravan root is not an open, non-draft pull request",
            Self::NotConflictFreeWithDefault => {
                "exact root head is not proven conflict-free with the exact default branch"
            }
            Self::AlreadyMerged => {
                "caravan root is already merged; the next tick advances the root"
            }
            Self::CumulativeTreeUnproven => {
                "no exact cumulative-tree proof for this root generation; the squash is not shown to land the already-validated tree"
            }
            Self::CumulativeTreeChanged => {
                "the default branch gained content this generation never validated; the caravan must revalidate before landing"
            }
            Self::MergeBudgetReached => {
                "bounded caravan-owned merges per tick reached; the next tick continues from fresh provider facts"
            }
        }
    }
}

/// Typed cause when the caravan-owned merge itself must fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RootMergeFailureCause {
    /// The root's observed base is not the exact default branch at merge time.
    /// This is the exact live incident class and is always refused.
    BaseNotDefaultBranch,
    /// The root head moved between verification and the merge, so the merge
    /// would land a generation this tick never validated.
    RootHeadMovedBeforeMerge,
    /// A foreign auto-merge request exists and reviewed policy refuses to race
    /// a second merge actor.
    ForeignAutoMergeActor,
    /// The provider accepted the merge but never exposed a merged root within
    /// the bounded confirmation reads.
    ProviderDidNotPersistMerge,
    /// The provider merged the root into a branch other than the default one.
    MergedIntoUnexpectedBase,
    /// The provider reported a merge whose commit the fetched default branch
    /// does not contain, so the content never reached the default branch. The
    /// root is left open and recoverable and no successor is promoted.
    MergeNotReachableFromDefault,
}

impl RootMergeFailureCause {
    /// Stable code embedded in structured error details.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::BaseNotDefaultBranch => "base_not_default_branch",
            Self::RootHeadMovedBeforeMerge => "root_head_moved_before_merge",
            Self::ForeignAutoMergeActor => "foreign_auto_merge_actor",
            Self::ProviderDidNotPersistMerge => "provider_did_not_persist_merge",
            Self::MergedIntoUnexpectedBase => "merged_into_unexpected_base",
            Self::MergeNotReachableFromDefault => "merge_not_reachable_from_default",
        }
    }

    /// Whether the next bounded tick can converge this without an operator.
    #[must_use]
    pub const fn resumable(self) -> bool {
        !matches!(
            self,
            Self::MergedIntoUnexpectedBase | Self::MergeNotReachableFromDefault
        )
    }

    /// Deterministic next action.
    #[must_use]
    pub const fn next(self) -> &'static str {
        match self {
            Self::BaseNotDefaultBranch => {
                "rerun the same idempotent bounded sync tick; promotion retargets the root to the default branch before any merge"
            }
            Self::RootHeadMovedBeforeMerge => {
                "rerun the same idempotent bounded sync tick against the fresh root generation"
            }
            Self::ForeignAutoMergeActor => {
                "disable the foreign auto-merge request, or set sync.external_auto_merge_policy=\"disable\" so cara converges it, then rerun the same bounded sync tick"
            }
            Self::ProviderDidNotPersistMerge => {
                "rerun the same idempotent bounded sync tick; an already-merged root is observed as such and advances the caravan"
            }
            Self::MergedIntoUnexpectedBase => {
                "inspect the provider merge receipt: the root landed on a branch other than the exact default branch and the caravan must be reconciled before further merges"
            }
            Self::MergeNotReachableFromDefault => {
                "inspect the typed unreachable-merge evidence: the claimed merge commit is not contained by the fetched default branch, so the pull request stays open and recoverable rather than counted as delivered"
            }
        }
    }
}

/// What a tick decided about merging the promoted root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootMergeGate {
    /// Every exact fact required by the caravan-owned merge is proven.
    Eligible,
    /// An ordinary bounded wait; nothing is mutated and nothing failed.
    Wait(RootMergeBlock),
    /// A typed refusal that must fail the tick rather than merge blindly.
    Refuse(RootMergeFailureCause),
}

/// Reviewed policy for a foreign (non-caravan) auto-merge request.
///
/// There must be exactly one merge actor. Either cara converges the foreign
/// request away, or it refuses to race it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExternalAutoMergePolicy {
    /// Disable the foreign request so cara remains the single merge actor.
    #[default]
    Disable,
    /// Refuse the tick and report the foreign actor instead of racing it.
    Refuse,
}

impl ExternalAutoMergePolicy {
    /// Stable code embedded in receipts and structured details.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Refuse => "refuse",
        }
    }
}

/// Auditable engine provenance for one promotion/merge decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RootMergeProvenance {
    /// Stable convergent-state owner. Always [`ROOT_MERGE_OWNER`].
    pub owner: String,
    /// Stable component performing convergence.
    pub component: String,
    /// Operation that owned this decision.
    pub operation_id: OperationId,
    /// Stable explanation retained for controllers and audit surfaces.
    pub reason: String,
    /// Whether this tick performed the provider write proving the receipt.
    pub engine_mutated: bool,
    /// Auto-merge facts observed before the decision. A caravan-owned merge
    /// always records these so a foreign merge actor can never be mistaken for
    /// engine convergence.
    pub observed_auto_merge: AutoMergeState,
}

/// Durable proof that the exact caravan root targets the exact default branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RootPromotionReceipt {
    pub schema_version: u32,
    pub repository: RepositoryId,
    pub caravan_id: PrNumber,
    pub pr: PrNumber,
    /// Exact head the promotion proof belongs to.
    pub head: BranchSnapshot,
    /// Base observed before this tick's decision.
    pub base_before: BranchSnapshot,
    /// Exact base proven after the fenced transaction.
    pub base_after: BranchSnapshot,
    /// Exact default branch name this root must target.
    pub default_branch: String,
    /// Predecessor whose merge promoted this root, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor: Option<PrNumber>,
    /// Whether that predecessor is already merged.
    pub predecessor_merged: bool,
    pub trigger: RootPromotionTrigger,
    /// Bounded provider reads consumed proving the postcondition.
    pub confirmation_reads: u32,
    pub provenance: RootMergeProvenance,
    /// Deterministic hash with this field omitted.
    pub evidence_hash: String,
}

impl RootPromotionReceipt {
    /// Seal the receipt with its deterministic evidence hash.
    #[must_use]
    pub fn finalize_hash(mut self) -> Self {
        self.evidence_hash.clear();
        let material = serde_json::to_vec(&self).expect("root promotion receipt serializes");
        self.evidence_hash = crate::membership::fnv1a64(&material);
        self
    }

    /// Whether the sealed hash still matches the receipt body.
    #[must_use]
    pub fn hash_is_valid(&self) -> bool {
        let mut material = self.clone();
        let expected = material.evidence_hash.clone();
        material.evidence_hash.clear();
        serde_json::to_vec(&material)
            .ok()
            .is_some_and(|bytes| crate::membership::fnv1a64(&bytes) == expected)
    }

    /// Whether this receipt proves the root targets the default branch.
    #[must_use]
    pub fn proves_default_base(&self) -> bool {
        self.base_after.name == self.default_branch
    }
}

/// Cumulative tree/ancestry evidence retained alongside one direct merge.
///
/// The live incident showed why this must be explicit: an already-merged
/// predecessor's content is carried cumulatively by the promoted root, so the
/// root must land exactly once on the default branch, and its children must be
/// *retargeted*, never rewritten or dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RootMergeAncestry {
    /// Exact default-branch generation observed immediately before the merge.
    pub default_before: BranchSnapshot,
    /// Exact default-branch generation observed after the provider merge.
    pub default_after: BranchSnapshot,
    /// Provider-reported merge commit, proven contained by `default_after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_commit: Option<CommitOid>,
    /// Cumulative-tree proof that authorized this landing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_tree: Option<CumulativeTreeProof>,
    /// Merged predecessor whose content this root already carries cumulatively.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor: Option<PrNumber>,
    /// Members that remain in caravan order after this merge. They are
    /// retargeted, never rewritten, so no child content is lost.
    #[serde(default)]
    pub remaining_members: Vec<PrNumber>,
    /// Next root promoted by this tick, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_root: Option<PrNumber>,
    /// Base the next root carried before promotion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_root_base_before: Option<String>,
    /// Base the next root carries after promotion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_root_base_after: Option<String>,
}

/// Durable proof of exactly one caravan-owned squash merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RootMergeReceipt {
    pub schema_version: u32,
    pub repository: RepositoryId,
    pub caravan_id: PrNumber,
    pub pr: PrNumber,
    /// Exact head that was merged, never a superseded generation.
    pub head: BranchSnapshot,
    /// Exact base the merge landed on. Always the default branch.
    pub base: BranchSnapshot,
    pub default_branch: String,
    pub merge_method: MergeMethod,
    /// Provider-observed merged state after bounded confirmation reads.
    pub merged: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<String>,
    pub ancestry: RootMergeAncestry,
    /// Bounded provider reads consumed proving the postcondition.
    pub confirmation_reads: u32,
    pub provenance: RootMergeProvenance,
    /// Deterministic hash with this field omitted.
    pub evidence_hash: String,
}

impl RootMergeReceipt {
    /// Seal the receipt with its deterministic evidence hash.
    #[must_use]
    pub fn finalize_hash(mut self) -> Self {
        self.evidence_hash.clear();
        let material = serde_json::to_vec(&self).expect("root merge receipt serializes");
        self.evidence_hash = crate::membership::fnv1a64(&material);
        self
    }

    /// Whether the sealed hash still matches the receipt body.
    #[must_use]
    pub fn hash_is_valid(&self) -> bool {
        let mut material = self.clone();
        let expected = material.evidence_hash.clone();
        material.evidence_hash.clear();
        serde_json::to_vec(&material)
            .ok()
            .is_some_and(|bytes| crate::membership::fnv1a64(&bytes) == expected)
    }

    /// Whether this receipt proves the root landed on the default branch.
    #[must_use]
    pub fn proves_default_branch_landing(&self) -> bool {
        self.merged && self.base.name == self.default_branch
    }
}

/// Derive the exact promotion transition from observed facts.
#[must_use]
pub fn promotion_trigger(
    observed_base: &str,
    default_branch: &str,
    predecessor_merged: bool,
) -> RootPromotionTrigger {
    if observed_base == default_branch {
        RootPromotionTrigger::AlreadyOnDefaultBranch
    } else if predecessor_merged {
        RootPromotionTrigger::MergedPredecessorRetarget
    } else {
        RootPromotionTrigger::NonDefaultBaseRetarget
    }
}

/// Exact facts a tick must hold before it may merge the promoted root itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootMergeFacts<'a> {
    /// Exact default branch name the root must target.
    pub default_branch: &'a str,
    /// Whether observed checks on the exact head are complete and successful.
    pub checks_passing: bool,
    /// Whether required contexts for the root's *current* base are satisfied.
    pub required_runs_satisfied: bool,
    /// Whether the exact head is proven conflict-free with the default branch.
    pub conflict_free_with_default: bool,
    /// Reviewed policy for a foreign auto-merge request.
    pub external_auto_merge: ExternalAutoMergePolicy,
}

/// Decide whether the caravan itself may merge this exact root generation.
///
/// The base gate is a *refusal*, not a wait: a promoted root whose observed base
/// is not the default branch is the exact live incident, and merging it would
/// land content on an already-merged predecessor instead of the default branch.
#[must_use]
pub fn merge_gate(observed: &PullRequestSnapshot, facts: RootMergeFacts<'_>) -> RootMergeGate {
    if observed.state == PullRequestState::Merged {
        return RootMergeGate::Wait(RootMergeBlock::AlreadyMerged);
    }
    if observed.base.name != facts.default_branch {
        return RootMergeGate::Refuse(RootMergeFailureCause::BaseNotDefaultBranch);
    }
    if observed.auto_merge.enabled && facts.external_auto_merge == ExternalAutoMergePolicy::Refuse {
        return RootMergeGate::Refuse(RootMergeFailureCause::ForeignAutoMergeActor);
    }
    if observed.state != PullRequestState::Open || observed.draft {
        return RootMergeGate::Wait(RootMergeBlock::NotOpen);
    }
    if !facts.conflict_free_with_default {
        return RootMergeGate::Wait(RootMergeBlock::NotConflictFreeWithDefault);
    }
    if !facts.checks_passing {
        return RootMergeGate::Wait(RootMergeBlock::ChecksNotPassing);
    }
    if !facts.required_runs_satisfied {
        return RootMergeGate::Wait(RootMergeBlock::RequiredRunsNotSatisfied);
    }
    RootMergeGate::Eligible
}

/// Build engine provenance for one promotion/merge decision.
#[must_use]
pub fn provenance(
    operation_id: &OperationId,
    reason: &str,
    engine_mutated: bool,
    observed_auto_merge: &AutoMergeState,
) -> RootMergeProvenance {
    RootMergeProvenance {
        owner: ROOT_MERGE_OWNER.to_owned(),
        component: ROOT_MERGE_COMPONENT.to_owned(),
        operation_id: operation_id.clone(),
        reason: reason.to_owned(),
        engine_mutated,
        observed_auto_merge: observed_auto_merge.clone(),
    }
}

/// Build a sealed promotion receipt.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn promotion_receipt(
    repository: &RepositoryId,
    caravan_id: PrNumber,
    observed: &PullRequestSnapshot,
    base_before: BranchSnapshot,
    default_branch: &str,
    predecessor: Option<PrNumber>,
    predecessor_merged: bool,
    trigger: RootPromotionTrigger,
    confirmation_reads: u32,
    provenance: RootMergeProvenance,
) -> RootPromotionReceipt {
    RootPromotionReceipt {
        schema_version: ROOT_PROMOTION_RECEIPT_SCHEMA_VERSION,
        repository: repository.clone(),
        caravan_id,
        pr: observed.number,
        head: observed.head.clone(),
        base_before,
        base_after: observed.base.clone(),
        default_branch: default_branch.to_owned(),
        predecessor,
        predecessor_merged,
        trigger,
        confirmation_reads,
        provenance,
        evidence_hash: String::new(),
    }
    .finalize_hash()
}

/// Build a sealed direct merge receipt.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn merge_receipt(
    repository: &RepositoryId,
    caravan_id: PrNumber,
    observed: &PullRequestSnapshot,
    default_branch: &str,
    ancestry: RootMergeAncestry,
    confirmation_reads: u32,
    provenance: RootMergeProvenance,
) -> RootMergeReceipt {
    RootMergeReceipt {
        schema_version: ROOT_MERGE_RECEIPT_SCHEMA_VERSION,
        repository: repository.clone(),
        caravan_id,
        pr: observed.number,
        head: observed.head.clone(),
        base: observed.base.clone(),
        default_branch: default_branch.to_owned(),
        merge_method: MergeMethod::Squash,
        merged: observed.state == PullRequestState::Merged,
        merged_at: observed.merged_at.clone(),
        ancestry,
        confirmation_reads,
        provenance,
        evidence_hash: String::new(),
    }
    .finalize_hash()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::model::CommitOid;
    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "acme".to_owned(),
            name: "widgets".to_owned(),
        }
    }

    fn branch(name: &str, oid: &str) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository(),
            name: name.to_owned(),
            oid: CommitOid(oid.to_owned()),
        }
    }

    fn snapshot(
        base: &str,
        state: PullRequestState,
        auto_merge: AutoMergeState,
    ) -> PullRequestSnapshot {
        PullRequestSnapshot {
            number: PrNumber(2213),
            title: "root".to_owned(),
            url: "https://example.invalid/2213".to_owned(),
            state,
            draft: false,
            head: branch("agent/root", "head-a"),
            base: branch(base, "base-a"),
            cross_repository: false,
            labels: BTreeSet::from(["caravan".to_owned()]),
            auto_merge,
            checks: Vec::new(),
            created_at: None,
            merged_at: (state == PullRequestState::Merged).then(|| "now".to_owned()),
            updated_at: None,
        }
    }

    fn facts(default_branch: &str) -> RootMergeFacts<'_> {
        RootMergeFacts {
            default_branch,
            checks_passing: true,
            required_runs_satisfied: true,
            conflict_free_with_default: true,
            external_auto_merge: ExternalAutoMergePolicy::Disable,
        }
    }

    #[test]
    fn merged_predecessor_base_is_a_retarget_not_a_merge_target() {
        assert_eq!(
            promotion_trigger("agent/pr2210", "main", true),
            RootPromotionTrigger::MergedPredecessorRetarget
        );
        assert!(RootPromotionTrigger::MergedPredecessorRetarget.requires_write());
        assert_eq!(
            promotion_trigger("agent/stack", "main", false),
            RootPromotionTrigger::NonDefaultBaseRetarget
        );
        assert_eq!(
            promotion_trigger("main", "main", true),
            RootPromotionTrigger::AlreadyOnDefaultBranch
        );
        assert!(!RootPromotionTrigger::AlreadyOnDefaultBranch.requires_write());
    }

    #[test]
    fn a_root_still_based_on_a_predecessor_is_refused_never_merged() {
        // This is exactly the PR2213 incident: green, clean, and pointed at an
        // already-merged predecessor branch.
        let observed = snapshot(
            "agent/pr2210",
            PullRequestState::Open,
            AutoMergeState::disabled(),
        );
        assert_eq!(
            merge_gate(&observed, facts("main")),
            RootMergeGate::Refuse(RootMergeFailureCause::BaseNotDefaultBranch)
        );
    }

    #[test]
    fn promoted_green_root_is_eligible_for_exactly_one_caravan_owned_merge() {
        let observed = snapshot("main", PullRequestState::Open, AutoMergeState::disabled());
        assert_eq!(
            merge_gate(&observed, facts("main")),
            RootMergeGate::Eligible
        );
    }

    #[test]
    fn pending_checks_and_unsatisfied_required_runs_wait_without_failing() {
        let observed = snapshot("main", PullRequestState::Open, AutoMergeState::disabled());
        let mut pending = facts("main");
        pending.checks_passing = false;
        assert_eq!(
            merge_gate(&observed, pending),
            RootMergeGate::Wait(RootMergeBlock::ChecksNotPassing)
        );
        let mut required = facts("main");
        required.required_runs_satisfied = false;
        assert_eq!(
            merge_gate(&observed, required),
            RootMergeGate::Wait(RootMergeBlock::RequiredRunsNotSatisfied)
        );
        let mut conflicted = facts("main");
        conflicted.conflict_free_with_default = false;
        assert_eq!(
            merge_gate(&observed, conflicted),
            RootMergeGate::Wait(RootMergeBlock::NotConflictFreeWithDefault)
        );
    }

    #[test]
    fn an_already_merged_root_waits_for_the_next_root_advance() {
        let observed = snapshot("main", PullRequestState::Merged, AutoMergeState::disabled());
        assert_eq!(
            merge_gate(&observed, facts("main")),
            RootMergeGate::Wait(RootMergeBlock::AlreadyMerged)
        );
    }

    #[test]
    fn foreign_auto_merge_actor_is_refused_under_reviewed_refuse_policy() {
        let observed = snapshot("main", PullRequestState::Open, AutoMergeState::squash());
        let mut refuse = facts("main");
        refuse.external_auto_merge = ExternalAutoMergePolicy::Refuse;
        assert_eq!(
            merge_gate(&observed, refuse),
            RootMergeGate::Refuse(RootMergeFailureCause::ForeignAutoMergeActor)
        );
        // The default reviewed policy converges the foreign request instead.
        assert_eq!(
            merge_gate(&observed, facts("main")),
            RootMergeGate::Eligible
        );
        assert_eq!(
            ExternalAutoMergePolicy::default(),
            ExternalAutoMergePolicy::Disable
        );
    }

    #[test]
    fn receipts_seal_and_prove_where_the_root_landed() {
        let observed = snapshot("main", PullRequestState::Merged, AutoMergeState::disabled());
        let operation = OperationId("operation-1".to_owned());
        let promotion = promotion_receipt(
            &repository(),
            PrNumber(2213),
            &observed,
            branch("agent/pr2210", "base-old"),
            "main",
            Some(PrNumber(2210)),
            true,
            RootPromotionTrigger::MergedPredecessorRetarget,
            2,
            provenance(
                &operation,
                RootPromotionTrigger::MergedPredecessorRetarget.reason(),
                true,
                &AutoMergeState::disabled(),
            ),
        );
        assert!(promotion.hash_is_valid());
        assert!(promotion.proves_default_base());
        assert_eq!(promotion.base_before.name, "agent/pr2210");
        assert_eq!(promotion.provenance.owner, ROOT_MERGE_OWNER);
        assert_eq!(promotion.provenance.component, ROOT_MERGE_COMPONENT);

        let merge = merge_receipt(
            &repository(),
            PrNumber(2213),
            &observed,
            "main",
            RootMergeAncestry {
                default_before: branch("main", "main-1"),
                default_after: branch("main", "main-2"),
                predecessor: Some(PrNumber(2210)),
                remaining_members: vec![PrNumber(2215)],
                next_root: Some(PrNumber(2215)),
                next_root_base_before: Some("agent/pr2213".to_owned()),
                next_root_base_after: Some("main".to_owned()),
                merge_commit: Some(CommitOid("3da6addb".to_owned())),
                cumulative_tree: None,
            },
            1,
            provenance(
                &operation,
                "caravan-owned squash merge",
                true,
                &AutoMergeState::disabled(),
            ),
        );
        assert!(merge.hash_is_valid());
        assert!(merge.proves_default_branch_landing());
        assert_eq!(merge.merge_method, MergeMethod::Squash);
        assert_eq!(merge.ancestry.remaining_members, vec![PrNumber(2215)]);

        let mut tampered = merge.clone();
        tampered.base = branch("agent/pr2210", "base-old");
        assert!(!tampered.hash_is_valid());
        assert!(!tampered.proves_default_branch_landing());
    }

    #[test]
    fn typed_causes_expose_bounded_scheduler_next_actions() {
        for cause in [
            RootPromotionFailureCause::BaseRetargetNotObserved,
            RootPromotionFailureCause::RootHeadMovedDuringPromotion,
            RootPromotionFailureCause::StaleProviderView,
        ] {
            assert!(!cause.code().is_empty());
            assert!(
                cause
                    .next()
                    .starts_with("rerun the same idempotent bounded sync tick")
            );
        }
        for cause in [
            RootMergeFailureCause::BaseNotDefaultBranch,
            RootMergeFailureCause::RootHeadMovedBeforeMerge,
            RootMergeFailureCause::ForeignAutoMergeActor,
            RootMergeFailureCause::ProviderDidNotPersistMerge,
            RootMergeFailureCause::MergedIntoUnexpectedBase,
            RootMergeFailureCause::MergeNotReachableFromDefault,
        ] {
            assert!(!cause.code().is_empty());
            assert!(!cause.next().is_empty());
        }
        assert!(!RootMergeFailureCause::MergedIntoUnexpectedBase.resumable());
        assert!(!RootMergeFailureCause::MergeNotReachableFromDefault.resumable());
        assert!(RootMergeFailureCause::BaseNotDefaultBranch.resumable());
        for block in [
            RootMergeBlock::ChecksNotPassing,
            RootMergeBlock::RequiredRunsNotSatisfied,
            RootMergeBlock::NotOpen,
            RootMergeBlock::NotConflictFreeWithDefault,
            RootMergeBlock::AlreadyMerged,
            RootMergeBlock::CumulativeTreeUnproven,
            RootMergeBlock::CumulativeTreeChanged,
            RootMergeBlock::MergeBudgetReached,
        ] {
            assert!(!block.code().is_empty());
            assert!(!block.reason().is_empty());
        }
    }
}
