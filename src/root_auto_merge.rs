//! Scheduler-owned convergent squash auto-merge for the admitted caravan root.
//!
//! Native squash auto-merge on the admitted caravan root is *required* queue
//! state, not an operator convenience. GitHub silently drops
//! `autoMergeRequest` whenever the root's head or base generation is rewritten
//! (physical rebase, head advance, retarget), so a scheduler that arms once and
//! then trusts a cached list view degrades until somebody re-arms by hand.
//!
//! This module owns the exact facts a tick must prove instead:
//!
//! - the *exact current* root head observed by a fresh single-PR provider read
//!   rather than a possibly stale list view;
//! - a durable [`RootAutoMergeReceipt`] proving squash auto-merge on that
//!   resulting head, carrying engine provenance so a controller can tell
//!   scheduler convergence apart from human repair;
//! - a typed [`RootAutoMergeFailureCause`] whenever arming cannot be proven, so
//!   retry stays inside bounded sync policy.
//!
//! The complementary rule lives in discovery: native auto-merge on an
//! *unadmitted* candidate makes that candidate structurally ineligible. Only
//! the admitted root requires native SQUASH auto-merge.

use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::{
    AutoMergeState, BranchSnapshot, CommitOid, MergeMethod, OperationId, PrNumber,
    PullRequestSnapshot, RepositoryId,
};

/// Stable schema for durable root auto-merge receipts.
pub const ROOT_AUTO_MERGE_RECEIPT_SCHEMA_VERSION: u32 = 1;

/// Bounded provider confirmation reads per arming attempt inside one tick.
pub const ROOT_AUTO_MERGE_CONFIRMATION_READS: u32 = 3;

/// Bounded arming attempts inside one tick before the typed failure is
/// reported and retry is deferred to the next bounded sync tick.
pub const ROOT_AUTO_MERGE_ARMING_ATTEMPTS: u32 = 2;

/// Delay between bounded confirmation reads absorbing provider read lag.
pub const ROOT_AUTO_MERGE_CONFIRMATION_DELAY: Duration = Duration::from_millis(250);

/// Stable owner recorded on every engine-performed arming.
pub const ROOT_AUTO_MERGE_OWNER: &str = "caravan-scheduler";

/// Stable component recorded on every engine-performed arming.
pub const ROOT_AUTO_MERGE_COMPONENT: &str = "cara sync";

/// Why a tick had to converge required root arming. The trigger is derived from
/// exact observed facts, never guessed, so a controller can distinguish routine
/// idempotent replay from a real durability transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RootAutoMergeTrigger {
    /// The exact current root head already carries SQUASH auto-merge.
    IdempotentReplay,
    /// The root was newly admitted or newly became the head of its caravan.
    RootAdmitted,
    /// The root's branch generation was rewritten (rebase/force-push).
    RootHeadRewritten,
    /// The root's base moved, for example retarget onto the default branch
    /// after a merged predecessor advanced the rolling head.
    RootBaseAdvanced,
    /// Arming was previously proven on this exact generation and then observed
    /// disabled without any caravan-owned write.
    ExternallyDisarmed,
    /// Auto-merge is enabled with a non-squash merge method.
    NonSquashMethod,
}

impl RootAutoMergeTrigger {
    /// Stable human explanation retained on receipts and events.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::IdempotentReplay => {
                "exact current root head already carries required squash auto-merge"
            }
            Self::RootAdmitted => "newly admitted caravan root requires squash auto-merge",
            Self::RootHeadRewritten => {
                "root head generation was rewritten; provider drops auto-merge on rewrite"
            }
            Self::RootBaseAdvanced => {
                "root base advanced; provider drops auto-merge on base transition"
            }
            Self::ExternallyDisarmed => {
                "required root auto-merge was disabled outside scheduler convergence"
            }
            Self::NonSquashMethod => "root auto-merge is armed with a non-squash merge method",
        }
    }

    /// Whether this trigger requires a provider write to converge.
    #[must_use]
    pub const fn requires_write(self) -> bool {
        !matches!(self, Self::IdempotentReplay)
    }
}

/// Typed cause when convergent root arming could not be proven this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RootAutoMergeFailureCause {
    /// The provider accepted the mutation but never exposed squash auto-merge
    /// on the resulting head within the bounded confirmation reads.
    ProviderDidNotPersistArming,
    /// The root head moved again while this tick was arming it, so the proof
    /// would belong to a superseded generation.
    RootHeadMovedDuringArming,
    /// The provider list view disagreed with the exact single-PR read and did
    /// not converge within the bounded confirmation reads.
    StaleProviderView,
}

impl RootAutoMergeFailureCause {
    /// Stable code embedded in structured error details.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ProviderDidNotPersistArming => "provider_did_not_persist_arming",
            Self::RootHeadMovedDuringArming => "root_head_moved_during_arming",
            Self::StaleProviderView => "stale_provider_view",
        }
    }

    /// Deterministic next action for a bounded scheduler, never an operator ask.
    #[must_use]
    pub const fn next(self) -> &'static str {
        match self {
            Self::ProviderDidNotPersistArming => {
                "rerun the same idempotent bounded sync tick; root arming converges without operator action"
            }
            Self::RootHeadMovedDuringArming => {
                "rerun the same idempotent bounded sync tick against the fresh root generation"
            }
            Self::StaleProviderView => {
                "rerun the same idempotent bounded sync tick once provider reads agree"
            }
        }
    }
}

/// Auditable engine provenance for one root arming decision.
///
/// Live incidents showed provider state alone cannot distinguish engine
/// convergence from human repair, so every decision records who converged it,
/// under which operation, from which observed state, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RootAutoMergeProvenance {
    /// Stable convergent-state owner. Always [`ROOT_AUTO_MERGE_OWNER`].
    pub owner: String,
    /// Stable component performing convergence.
    pub component: String,
    /// Operation that owned this convergence decision.
    pub operation_id: OperationId,
    /// Exact derived transition that required convergence.
    pub trigger: RootAutoMergeTrigger,
    /// Stable explanation retained for controllers and audit surfaces.
    pub reason: String,
    /// Auto-merge state observed before this tick's decision.
    pub observed_before: AutoMergeState,
    /// Provider actor exposed on the observed auto-merge request, when any.
    /// A foreign actor on an armed root is retained rather than erased so an
    /// external re-arm never looks like engine convergence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_actor: Option<String>,
    /// Whether this tick performed the provider write proving the receipt.
    pub engine_armed: bool,
}

/// Durable proof that the exact current caravan root head carries required
/// native SQUASH auto-merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RootAutoMergeReceipt {
    pub schema_version: u32,
    pub repository: RepositoryId,
    pub caravan_id: PrNumber,
    pub pr: PrNumber,
    /// Exact resulting head the proof belongs to, never a pre-rebase generation.
    pub head: BranchSnapshot,
    /// Exact base observed alongside the proven head.
    pub base: BranchSnapshot,
    pub merge_method: MergeMethod,
    /// Fresh post-mutation auto-merge facts re-read from the provider.
    pub observed_after: AutoMergeState,
    /// Bounded provider reads consumed proving the postcondition.
    pub confirmation_reads: u32,
    /// Bounded arming attempts consumed this tick.
    pub arming_attempts: u32,
    pub provenance: RootAutoMergeProvenance,
    /// Deterministic hash with this field omitted.
    pub evidence_hash: String,
}

impl RootAutoMergeReceipt {
    /// Seal the receipt with its deterministic evidence hash.
    #[must_use]
    pub fn finalize_hash(mut self) -> Self {
        self.evidence_hash.clear();
        let material = serde_json::to_vec(&self).expect("root auto-merge receipt serializes");
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
}

/// Whether an observed snapshot carries the required native SQUASH auto-merge.
#[must_use]
pub fn squash_armed(snapshot: &PullRequestSnapshot) -> bool {
    snapshot.auto_merge.enabled && snapshot.auto_merge.merge_method == Some(MergeMethod::Squash)
}

/// Derive why convergence is required from exact observed facts.
///
/// `discovery` is the generation this tick started from, `rewritten_head` is the
/// exact generation this tick's own scheduler rebase published for the root (if
/// any), and `observed` is the authoritative fresh single-PR read. A
/// scheduler-owned rewrite or a base transition is preferred over a bare
/// "externally disarmed" claim so provenance never blames an operator for a
/// provider-side rewrite drop.
#[must_use]
pub fn classify_trigger(
    discovery: Option<&PullRequestSnapshot>,
    observed: &PullRequestSnapshot,
    rewritten_head: Option<&CommitOid>,
    proven_generation: Option<&CommitOid>,
) -> RootAutoMergeTrigger {
    if squash_armed(observed) {
        return RootAutoMergeTrigger::IdempotentReplay;
    }
    if observed.auto_merge.enabled {
        return RootAutoMergeTrigger::NonSquashMethod;
    }
    if rewritten_head.is_some_and(|oid| oid == &observed.head.oid) {
        return RootAutoMergeTrigger::RootHeadRewritten;
    }
    let Some(discovery) = discovery else {
        return RootAutoMergeTrigger::RootAdmitted;
    };
    if discovery.head.oid != observed.head.oid {
        return RootAutoMergeTrigger::RootHeadRewritten;
    }
    if discovery.base.name != observed.base.name || discovery.base.oid != observed.base.oid {
        return RootAutoMergeTrigger::RootBaseAdvanced;
    }
    if proven_generation.is_some_and(|oid| oid == &observed.head.oid)
        || discovery.auto_merge.enabled
    {
        return RootAutoMergeTrigger::ExternallyDisarmed;
    }
    RootAutoMergeTrigger::RootAdmitted
}

/// Build a sealed receipt for one converged root generation.
#[must_use]
pub fn receipt(
    repository: &RepositoryId,
    caravan_id: PrNumber,
    observed: &PullRequestSnapshot,
    provenance: RootAutoMergeProvenance,
    confirmation_reads: u32,
    arming_attempts: u32,
) -> RootAutoMergeReceipt {
    RootAutoMergeReceipt {
        schema_version: ROOT_AUTO_MERGE_RECEIPT_SCHEMA_VERSION,
        repository: repository.clone(),
        caravan_id,
        pr: observed.number,
        head: observed.head.clone(),
        base: observed.base.clone(),
        merge_method: MergeMethod::Squash,
        observed_after: observed.auto_merge.clone(),
        confirmation_reads,
        arming_attempts,
        provenance,
        evidence_hash: String::new(),
    }
    .finalize_hash()
}

/// Build engine provenance for one convergence decision.
#[must_use]
pub fn provenance(
    operation_id: &OperationId,
    trigger: RootAutoMergeTrigger,
    observed_before: &AutoMergeState,
    engine_armed: bool,
) -> RootAutoMergeProvenance {
    RootAutoMergeProvenance {
        owner: ROOT_AUTO_MERGE_OWNER.to_owned(),
        component: ROOT_AUTO_MERGE_COMPONENT.to_owned(),
        operation_id: operation_id.clone(),
        trigger,
        reason: trigger.reason().to_owned(),
        observed_actor: observed_before.actor.clone(),
        observed_before: observed_before.clone(),
        engine_armed,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::model::PullRequestState;

    fn branch(name: &str, oid: &str) -> BranchSnapshot {
        BranchSnapshot {
            repository: RepositoryId {
                owner: "acme".to_owned(),
                name: "widgets".to_owned(),
            },
            name: name.to_owned(),
            oid: CommitOid(oid.to_owned()),
        }
    }

    fn snapshot(head_oid: &str, base_oid: &str, auto_merge: AutoMergeState) -> PullRequestSnapshot {
        PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(2208),
            title: "root".to_owned(),
            url: "https://example.invalid/2208".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch("root", head_oid),
            base: branch("main", base_oid),
            cross_repository: false,
            labels: BTreeSet::from(["caravan".to_owned()]),
            auto_merge,
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn armed_root_is_idempotent_replay() {
        let observed = snapshot("head-a", "base-a", AutoMergeState::squash());
        assert_eq!(
            classify_trigger(Some(&observed), &observed, None, None),
            RootAutoMergeTrigger::IdempotentReplay
        );
        assert!(!RootAutoMergeTrigger::IdempotentReplay.requires_write());
    }

    #[test]
    fn scheduler_rewrite_is_attributed_to_the_engine_not_an_operator() {
        let discovery = snapshot("head-b", "base-a", AutoMergeState::disabled());
        let observed = snapshot("head-b", "base-a", AutoMergeState::disabled());
        assert_eq!(
            classify_trigger(
                Some(&discovery),
                &observed,
                Some(&CommitOid("head-b".to_owned())),
                Some(&CommitOid("head-b".to_owned()))
            ),
            RootAutoMergeTrigger::RootHeadRewritten
        );
    }

    #[test]
    fn rewritten_head_is_preferred_over_external_disarm() {
        let discovery = snapshot("head-a", "base-a", AutoMergeState::squash());
        let observed = snapshot("head-b", "base-a", AutoMergeState::disabled());
        assert_eq!(
            classify_trigger(
                Some(&discovery),
                &observed,
                None,
                Some(&CommitOid("head-a".to_owned()))
            ),
            RootAutoMergeTrigger::RootHeadRewritten
        );
    }

    #[test]
    fn base_transition_is_distinguished_from_external_disarm() {
        let discovery = snapshot("head-a", "base-a", AutoMergeState::squash());
        let observed = snapshot("head-a", "base-b", AutoMergeState::disabled());
        assert_eq!(
            classify_trigger(Some(&discovery), &observed, None, None),
            RootAutoMergeTrigger::RootBaseAdvanced
        );
    }

    #[test]
    fn same_generation_disarm_is_external() {
        let discovery = snapshot("head-a", "base-a", AutoMergeState::squash());
        let observed = snapshot("head-a", "base-a", AutoMergeState::disabled());
        assert_eq!(
            classify_trigger(
                Some(&discovery),
                &observed,
                None,
                Some(&CommitOid("head-a".to_owned()))
            ),
            RootAutoMergeTrigger::ExternallyDisarmed
        );
    }

    #[test]
    fn unknown_previous_generation_is_admission() {
        let observed = snapshot("head-a", "base-a", AutoMergeState::disabled());
        assert_eq!(
            classify_trigger(None, &observed, None, None),
            RootAutoMergeTrigger::RootAdmitted
        );
    }

    #[test]
    fn non_squash_arming_is_reported_exactly() {
        let observed = snapshot(
            "head-a",
            "base-a",
            AutoMergeState {
                enabled: true,
                merge_method: None,
                actor: Some("octocat".to_owned()),
            },
        );
        assert_eq!(
            classify_trigger(Some(&observed), &observed, None, None),
            RootAutoMergeTrigger::NonSquashMethod
        );
        assert!(!squash_armed(&observed));
    }

    #[test]
    fn receipts_seal_and_retain_engine_provenance() {
        let observed = snapshot("head-b", "base-a", AutoMergeState::squash());
        let operation = OperationId("operation-1".to_owned());
        let sealed = receipt(
            &RepositoryId {
                owner: "acme".to_owned(),
                name: "widgets".to_owned(),
            },
            PrNumber(2208),
            &observed,
            provenance(
                &operation,
                RootAutoMergeTrigger::RootHeadRewritten,
                &AutoMergeState::disabled(),
                true,
            ),
            2,
            1,
        );
        assert!(sealed.hash_is_valid());
        assert_eq!(sealed.head.oid, CommitOid("head-b".to_owned()));
        assert_eq!(sealed.provenance.owner, ROOT_AUTO_MERGE_OWNER);
        assert_eq!(sealed.provenance.component, ROOT_AUTO_MERGE_COMPONENT);
        assert!(sealed.provenance.engine_armed);
        assert_eq!(sealed.provenance.operation_id, operation);

        let mut tampered = sealed.clone();
        tampered.provenance.engine_armed = false;
        assert!(!tampered.hash_is_valid());
    }

    #[test]
    fn failure_causes_expose_bounded_scheduler_next_actions() {
        for cause in [
            RootAutoMergeFailureCause::ProviderDidNotPersistArming,
            RootAutoMergeFailureCause::RootHeadMovedDuringArming,
            RootAutoMergeFailureCause::StaleProviderView,
        ] {
            assert!(!cause.code().is_empty());
            assert!(
                cause
                    .next()
                    .starts_with("rerun the same idempotent bounded sync tick"),
                "{} must defer retry to bounded sync policy",
                cause.code()
            );
        }
    }
}
