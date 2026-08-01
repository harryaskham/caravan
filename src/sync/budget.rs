//! Deterministic modelling of the physical-apply wall-clock reserve.
//!
//! The apply reserve exists so an irreversible root-to-descendant branch
//! rewrite never starts without provably enough wall clock left to finish it.
//! Modelling that reserve as a whole-chain worst case is monotonic: a caravan
//! which outgrows `sync.max_duration_secs` can never drain, and every accepted
//! member raises the reserve again. This module keeps the safety property and
//! removes the deadlock by splitting the reserve in two:
//!
//! * a **hard** reserve covering control mutations, the exact branch-apply
//!   rounds, and the mandatory post-write midpoint verification. Nothing
//!   irreversible starts unless this fits.
//! * a **soft** reserve covering ordinary post-write CI/auto-merge
//!   reconciliation. Reconciliation is idempotent read-then-converge work, so
//!   when only this part cannot fit the tick still applies an exact bounded
//!   prefix and defers convergence to the next tick.
//!
//! Every cost is derived from the operations a tick will actually run:
//! members whose exact cumulative ancestry already holds cost no push or
//! auto-merge disable; durable force labels never add rewrite work, so completed prefixes make
//! each subsequent tick strictly cheaper instead of strictly more expensive.

use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::AppContext;
use crate::model::{Caravan, PrNumber};
use crate::read::StatusOutput;

use super::{
    MAX_PARALLEL_REBASE_CHAINS, PHYSICAL_APPLY_COMMAND_SLOTS_PER_PENDING_MEMBER,
    PHYSICAL_APPLY_COMMAND_SLOTS_PER_RETAINED_MEMBER, PHYSICAL_FIXED_POST_WRITE_COMMAND_SLOTS,
    PHYSICAL_RECONCILIATION_COMMAND_SLOTS_PER_CARAVAN,
    PHYSICAL_RECONCILIATION_COMMAND_SLOTS_PER_MEMBER,
};

/// Hard upper bound on the capacity search so a pathological configuration
/// cannot turn a deterministic projection into an unbounded loop.
const MAX_CAPACITY_SEARCH_MEMBERS: u64 = 4_096;

/// Smallest admission bound that can honestly be enforced as ordinary gating.
///
/// Gating exists to stop a chain growing past the size the configured deadline
/// can still guarantee to drain. A bound below two refuses every join to a
/// caravan that already holds a single member, so no chain could ever form and
/// no amount of draining could reopen admission. That is a configuration
/// defect, not capacity gating, so it is reported as a defect instead of being
/// emitted as a bound (bd-b1c7b7).
pub(super) const MINIMUM_SOUND_CAPACITY: u64 = 2;

/// Wall clock deliberately left unclaimed between admitting a prefix and
/// reserving it at the precommit barrier, so the few microseconds spent
/// deciding can never turn an admitted prefix into a refusal.
const ADMISSION_GUARD: Duration = Duration::from_secs(1);

/// Exact per-member cost inputs for one selected caravan member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MemberCost {
    pub pr: PrNumber,
    /// A branch rewrite is actually required for this exact generation.
    pub pending: bool,
    /// Native auto-merge must be dropped before rewriting this member.
    pub auto_merge_enabled: bool,
}

impl MemberCost {
    /// Worst-case shape used by projections that cannot plan Git ranges.
    const fn worst_case(pr: PrNumber, auto_merge_enabled: bool) -> Self {
        Self {
            pr,
            pending: true,
            auto_merge_enabled,
        }
    }

    const fn apply_slots(self) -> u64 {
        if self.pending {
            PHYSICAL_APPLY_COMMAND_SLOTS_PER_PENDING_MEMBER
        } else {
            PHYSICAL_APPLY_COMMAND_SLOTS_PER_RETAINED_MEMBER
        }
    }

    const fn control_slots(self) -> u64 {
        if !self.pending {
            return 0;
        }
        if self.auto_merge_enabled { 1 } else { 0 }
    }

    const fn control_mutations(self) -> u64 {
        if !self.pending {
            return 0;
        }
        let auto_merge: u64 = if self.auto_merge_enabled { 1 } else { 0 };
        // One marked attribution comment after every actual branch generation
        // write. Durable force itself adds no control mutation.
        auto_merge.saturating_add(1)
    }
}

/// Exact cost inputs for one selected caravan, in root-to-descendant order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ChainCost {
    pub caravan_id: PrNumber,
    pub members: Vec<MemberCost>,
    /// Non-root members carrying provider auto-merge that reconciliation repairs.
    pub externally_armed_non_roots: u64,
}

impl ChainCost {
    fn reconciliation_slots(&self) -> u64 {
        u64::try_from(self.members.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(PHYSICAL_RECONCILIATION_COMMAND_SLOTS_PER_MEMBER)
            .saturating_add(self.externally_armed_non_roots)
            .saturating_add(PHYSICAL_RECONCILIATION_COMMAND_SLOTS_PER_CARAVAN)
    }
}

/// Wall-clock and mutation reserve required by one bounded apply admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PhysicalCommitBudget {
    pub command_slots: u64,
    pub required: Duration,
    pub mutation_reserve: u32,
}

/// Which post-write work a modelled reserve covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReserveScope {
    /// Control, apply, midpoint verification, and full reconciliation.
    Complete,
    /// Control, apply, and midpoint verification only. Ordinary reconciliation
    /// is intentionally deferred to the next tick.
    BoundedPrefix,
}

/// Exact bounded prefix admitted for one tick, plus the deferred remainder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PhysicalApplyAdmission {
    /// Admitted leading member count per selected chain, in selection order.
    pub admitted: Vec<usize>,
    /// Admitted members in exact root-to-descendant apply order.
    pub admitted_prs: Vec<PrNumber>,
    /// Members intentionally left for a later tick, root-to-descendant.
    pub deferred: Vec<PrNumber>,
    /// Reserve required by the admitted prefix under its own scope.
    pub budget: PhysicalCommitBudget,
    /// Reserve the complete graph would have required this tick.
    pub complete_budget: PhysicalCommitBudget,
    /// True when ordinary provider convergence is skipped on purpose.
    pub deferred_convergence: bool,
    /// Pending branch rewrites inside the admitted prefix.
    pub pending_admitted: u64,
}

impl PhysicalApplyAdmission {
    /// A tick makes forward progress when it either finishes the graph or
    /// writes at least one pending member of it.
    pub fn makes_progress(&self) -> bool {
        self.pending_admitted > 0 || self.deferred.is_empty()
    }
}

/// One selected caravan projected against the configured deadline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CaravanBudgetProjection {
    pub caravan_id: PrNumber,
    pub members: Vec<PrNumber>,
    /// Reserve if every member still needs a rewrite (the pre-plan worst case).
    pub required_command_slots: u64,
    pub required_ms: u64,
    /// Reserve once every member's exact ancestry already holds.
    pub retained_command_slots: u64,
    pub retained_ms: u64,
    /// Members this tick could still apply under the worst case, root-first.
    pub processable_prefix: Vec<PrNumber>,
    /// Members the same worst-case tick would defer to a later tick.
    pub deferred: Vec<PrNumber>,
    /// True when the same tick would apply branches now and leave ordinary
    /// CI/auto-merge convergence to the next tick.
    pub deferred_convergence: bool,
    /// True when one more member could no longer be guaranteed to drain.
    pub at_capacity: bool,
    pub safe_next_action: String,
}

/// Why a computed admission bound cannot be enforced as ordinary gating.
///
/// bd-b1c7b7: a non-positive bound used to be emitted as an ordinary capacity
/// refusal whose only suggested remedy was waiting for a caravan to drain.
/// Draining cannot raise a bound derived purely from configuration, so the
/// guidance recommended an action that could never resolve the condition it
/// described. An unsound bound is now a typed defect carrying the arithmetic
/// that produced it and the exact configuration change that repairs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CapacityDefect {
    pub code: String,
    /// Bound the configured arithmetic produced, always below the sound floor.
    pub computed_bound: u64,
    /// Smallest bound that could have been enforced as gating.
    pub minimum_sound_bound: u64,
    pub deadline_ms: u64,
    pub command_timeout_ms: u64,
    /// Wall clock the reserve model prices one planned command at.
    pub reserve_ms_per_command: u64,
    /// Command slots the smallest sound chain would require.
    pub minimum_chain_command_slots: u64,
    /// Deadline that would make the smallest sound chain admissible again.
    pub minimum_deadline_ms: u64,
    pub safe_next_action: String,
}

/// Deterministic disposition of one caravan against the admission bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CapacityGate {
    /// The chain can still accept another member under a sound bound.
    Open { bound: u64 },
    /// Ordinary gating: the chain already holds every admissible member.
    AtCapacity { bound: u64 },
    /// No sound bound exists, so gating cannot be enforced honestly.
    Defect(CapacityDefect),
}

/// Deterministic, provider-free projection of the physical apply reserve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncBudgetStatus {
    pub schema_version: u32,
    /// Whether physical chain rebuilding (and therefore this reserve) applies.
    pub rebase_on_join: bool,
    pub deadline_ms: u64,
    pub command_timeout_ms: u64,
    /// Wall clock the reserve model prices one planned command at. Admission
    /// and the apply reserve share this price; neither uses the worst-case
    /// `command_timeout_secs` slot (bd-b1c7b7).
    pub reserve_ms_per_command: u64,
    /// Whole-tick deadline expressed in reserve-priced command slots.
    pub deadline_command_slots: u64,
    /// Largest chain size whose last member is still guaranteed to drain.
    /// Absent exactly when `capacity_defect` explains why no sound bound
    /// exists; a zero bound is never emitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_admissible_members: Option<u64>,
    /// Typed defect when the configured arithmetic cannot produce a bound that
    /// admission could honestly enforce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_defect: Option<CapacityDefect>,
    #[serde(default)]
    pub caravans: Vec<CaravanBudgetProjection>,
    /// Canonical candidate that admission would refuse at capacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_candidate: Option<PrNumber>,
    pub safe_next_action: String,
}

impl Default for SyncBudgetStatus {
    fn default() -> Self {
        Self {
            schema_version: 2,
            rebase_on_join: false,
            deadline_ms: 0,
            command_timeout_ms: 0,
            reserve_ms_per_command: 0,
            deadline_command_slots: 0,
            max_admissible_members: None,
            capacity_defect: None,
            caravans: Vec::new(),
            blocked_candidate: None,
            safe_next_action: "physical chain rebuilding is disabled; no apply reserve applies"
                .to_owned(),
        }
    }
}

/// Seconds reserved per planned provider command.
///
/// bd-5528e6: reserving the full `command_timeout_secs` for every slot assumes
/// every command consumes its entire timeout, which is not a realistic plan but
/// a worst case. On a six-member caravan that reserve (32 slots x 120s) exceeds
/// the whole configured run, so sync refuses before mutation and the caravan can
/// never converge, even for cheap base-retarget and auto-merge arming. Each
/// command is still individually bounded by `command_timeout_secs`, the whole
/// tick is still bounded by the operation deadline, and a mid-apply timeout is
/// an already-handled resumable path -- so a proportional reserve is strictly
/// safer than guaranteeing zero progress.
pub(super) fn reserve_secs_per_command(context: &AppContext) -> u64 {
    context
        .config
        .command_timeout_secs
        .min(context.config.sync.reserve_secs_per_command)
}

fn slots_to_duration(context: &AppContext, slots: u64) -> Duration {
    Duration::from_secs(reserve_secs_per_command(context).saturating_mul(slots))
}

/// Worst-case duration retained purely as evidence in refusal receipts.
pub(super) fn slots_to_worst_case_duration(context: &AppContext, slots: u64) -> Duration {
    Duration::from_secs(context.config.command_timeout_secs.saturating_mul(slots))
}

/// Whole-tick deadline expressed in reserve-priced command slots.
///
/// bd-b1c7b7: this used to price every slot at the full `command_timeout_secs`
/// while the apply reserve priced the identical slots proportionally, so the
/// two models disagreed by orders of magnitude on the same chain and raising a
/// proven-safe command timeout silently closed admission. Both now share
/// `reserve_secs_per_command`, which is itself capped by `command_timeout_secs`.
pub(super) fn deadline_command_slots(context: &AppContext, deadline: Duration) -> u64 {
    let secs_per_slot = reserve_secs_per_command(context);
    if secs_per_slot == 0 {
        return u64::MAX;
    }
    deadline.as_secs() / secs_per_slot
}

/// Worst-case chain costs derived from discovery alone: every member is
/// assumed to still require a rewrite because status never plans Git ranges.
pub(super) fn chain_costs_from_status(
    status: &StatusOutput,
    selected: &[Caravan],
) -> Vec<ChainCost> {
    selected
        .iter()
        .map(|caravan| {
            let members = caravan
                .members
                .iter()
                .map(|number| {
                    status.analysis.pull_requests.get(number).map_or_else(
                        || MemberCost::worst_case(*number, false),
                        |pull_request| {
                            MemberCost::worst_case(*number, pull_request.auto_merge.enabled)
                        },
                    )
                })
                .collect::<Vec<_>>();
            ChainCost {
                caravan_id: caravan.id,
                externally_armed_non_roots: externally_armed_non_roots(status, caravan),
                members,
            }
        })
        .collect()
}

pub(super) fn externally_armed_non_roots(status: &StatusOutput, caravan: &Caravan) -> u64 {
    caravan
        .members
        .iter()
        .skip(1)
        .filter(|number| {
            status
                .analysis
                .pull_requests
                .get(number)
                .is_some_and(|pull_request| pull_request.auto_merge.enabled)
        })
        .count()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Bounded apply rounds for a per-chain prefix vector, modelling the bounded
/// parallelism actually used across independent caravans.
fn apply_rounds(chains: &[ChainCost], admitted: &[usize]) -> u64 {
    chains
        .chunks(MAX_PARALLEL_REBASE_CHAINS)
        .enumerate()
        .map(|(batch, group)| {
            group
                .iter()
                .enumerate()
                .map(|(offset, chain)| {
                    let index = batch * MAX_PARALLEL_REBASE_CHAINS + offset;
                    chain
                        .members
                        .iter()
                        .take(admitted.get(index).copied().unwrap_or(0))
                        .map(|member| member.apply_slots())
                        .fold(0_u64, u64::saturating_add)
                })
                .max()
                .unwrap_or(0)
        })
        .fold(0_u64, u64::saturating_add)
}

pub(super) fn budget_for(
    context: &AppContext,
    chains: &[ChainCost],
    admitted: &[usize],
    scope: ReserveScope,
) -> PhysicalCommitBudget {
    let admitted_members = || {
        chains.iter().enumerate().flat_map(|(index, chain)| {
            chain
                .members
                .iter()
                .take(admitted.get(index).copied().unwrap_or(0))
        })
    };
    let control_slots = admitted_members()
        .map(|member| member.control_slots())
        .fold(0_u64, u64::saturating_add);
    let control_mutations = admitted_members()
        .map(|member| member.control_mutations())
        .fold(0_u64, u64::saturating_add);
    let pending_writes = admitted_members()
        .filter(|member| member.pending)
        .count()
        .try_into()
        .unwrap_or(u64::MAX);
    let reconciliation = match scope {
        ReserveScope::Complete => chains
            .iter()
            .map(ChainCost::reconciliation_slots)
            .fold(0_u64, u64::saturating_add),
        ReserveScope::BoundedPrefix => 0,
    };
    let command_slots = control_slots
        .saturating_add(apply_rounds(chains, admitted))
        .saturating_add(reconciliation)
        .saturating_add(PHYSICAL_FIXED_POST_WRITE_COMMAND_SLOTS);
    let mutation_reserve = control_mutations
        .saturating_add(pending_writes)
        .saturating_add(reconciliation);
    PhysicalCommitBudget {
        command_slots,
        required: slots_to_duration(context, command_slots),
        mutation_reserve: u32::try_from(mutation_reserve).unwrap_or(u32::MAX),
    }
}

fn complete_prefix(chains: &[ChainCost]) -> Vec<usize> {
    chains.iter().map(|chain| chain.members.len()).collect()
}

/// Complete-graph reserve, retained so evidence can always contrast the exact
/// prefix actually admitted with the reserve the whole graph would need.
pub(super) fn complete_budget(context: &AppContext, chains: &[ChainCost]) -> PhysicalCommitBudget {
    budget_for(
        context,
        chains,
        &complete_prefix(chains),
        ReserveScope::Complete,
    )
}

fn deferred_members(chains: &[ChainCost], admitted: &[usize]) -> Vec<PrNumber> {
    chains
        .iter()
        .enumerate()
        .flat_map(|(index, chain)| {
            chain
                .members
                .iter()
                .skip(admitted.get(index).copied().unwrap_or(0))
                .map(|member| member.pr)
        })
        .collect()
}

pub(super) fn admitted_members(chains: &[ChainCost], admitted: &[usize]) -> Vec<PrNumber> {
    chains
        .iter()
        .enumerate()
        .flat_map(|(index, chain)| {
            chain
                .members
                .iter()
                .take(admitted.get(index).copied().unwrap_or(0))
                .map(|member| member.pr)
        })
        .collect()
}

fn pending_in_prefix(chains: &[ChainCost], admitted: &[usize]) -> u64 {
    chains
        .iter()
        .enumerate()
        .map(|(index, chain)| {
            chain
                .members
                .iter()
                .take(admitted.get(index).copied().unwrap_or(0))
                .filter(|member| member.pending)
                .count()
                .try_into()
                .unwrap_or(u64::MAX)
        })
        .fold(0_u64, u64::saturating_add)
}

fn fits(budget: PhysicalCommitBudget, remaining: Duration) -> bool {
    budget.required.saturating_add(ADMISSION_GUARD) < remaining
}

/// Admit the largest exact root-to-descendant prefix that provably fits the
/// remaining wall clock.
///
/// Growth is deterministic: chains are extended round-robin in selection order
/// and every chain is extended strictly root-to-descendant, so no caravan is
/// reordered, split, or evicted to make the reserve fit.
pub(super) fn admit_physical_prefix(
    context: &AppContext,
    chains: &[ChainCost],
    remaining: Duration,
) -> PhysicalApplyAdmission {
    let complete = complete_budget(context, chains);
    let full = complete_prefix(chains);
    if fits(complete, remaining) {
        return PhysicalApplyAdmission {
            deferred: Vec::new(),
            pending_admitted: pending_in_prefix(chains, &full),
            admitted_prs: admitted_members(chains, &full),
            admitted: full,
            budget: complete,
            complete_budget: complete,
            deferred_convergence: false,
        };
    }

    let mut admitted = vec![0_usize; chains.len()];
    let mut budget = budget_for(context, chains, &admitted, ReserveScope::BoundedPrefix);
    let mut saturated = vec![false; chains.len()];
    while saturated.iter().any(|done| !done) {
        for index in 0..chains.len() {
            if saturated[index] {
                continue;
            }
            if admitted[index] >= chains[index].members.len() {
                saturated[index] = true;
                continue;
            }
            admitted[index] += 1;
            let candidate = budget_for(context, chains, &admitted, ReserveScope::BoundedPrefix);
            if fits(candidate, remaining) {
                budget = candidate;
            } else {
                admitted[index] -= 1;
                saturated[index] = true;
            }
        }
    }

    let deferred = deferred_members(chains, &admitted);
    let pending_admitted = pending_in_prefix(chains, &admitted);
    // A prefix covering the whole graph only fell short of the complete reserve
    // by its deferrable reconciliation. When it also carries no pending write
    // there is nothing irreversible to protect, so ordinary convergence runs
    // under the same deadline instead of deferring forever: reconciliation is
    // idempotent read-then-converge work whose interruption costs a rerun, not
    // a half-applied chain.
    let deferred_convergence = !deferred.is_empty() || pending_admitted > 0;
    PhysicalApplyAdmission {
        admitted_prs: admitted_members(chains, &admitted),
        admitted,
        deferred,
        budget,
        complete_budget: complete,
        deferred_convergence,
        pending_admitted,
    }
}

/// Command slots the hardest ordinary shape of a `size`-member chain needs:
/// every earlier member already retained, one trailing pending rewrite, and
/// one native auto-merge drop.
const fn capacity_command_slots(size: u64) -> u64 {
    size.saturating_sub(1)
        .saturating_mul(PHYSICAL_APPLY_COMMAND_SLOTS_PER_RETAINED_MEMBER)
        .saturating_add(PHYSICAL_APPLY_COMMAND_SLOTS_PER_PENDING_MEMBER)
        .saturating_add(1)
        .saturating_add(PHYSICAL_FIXED_POST_WRITE_COMMAND_SLOTS)
}

/// Raw bound the configured deadline implies under the actual-work reserve.
fn computed_capacity(context: &AppContext, deadline: Duration) -> u64 {
    let slots = deadline_command_slots(context, deadline);
    let mut admissible = 0;
    let mut size = 1;
    while size <= MAX_CAPACITY_SEARCH_MEMBERS {
        if capacity_command_slots(size) >= slots {
            break;
        }
        admissible = size;
        size += 1;
    }
    admissible
}

/// Smallest deadline under which the smallest sound chain is admissible.
fn minimum_sound_deadline(context: &AppContext) -> Duration {
    let slots = capacity_command_slots(MINIMUM_SOUND_CAPACITY).saturating_add(1);
    Duration::from_secs(slots.saturating_mul(reserve_secs_per_command(context)))
}

fn capacity_defect(
    context: &AppContext,
    deadline: Duration,
    computed_bound: u64,
) -> CapacityDefect {
    let minimum_deadline = minimum_sound_deadline(context);
    CapacityDefect {
        code: "sync_budget_capacity_unsound".to_owned(),
        computed_bound,
        minimum_sound_bound: MINIMUM_SOUND_CAPACITY,
        deadline_ms: super::duration_millis(deadline),
        command_timeout_ms: context.config.command_timeout_secs.saturating_mul(1_000),
        reserve_ms_per_command: reserve_secs_per_command(context).saturating_mul(1_000),
        minimum_chain_command_slots: capacity_command_slots(MINIMUM_SOUND_CAPACITY),
        minimum_deadline_ms: super::duration_millis(minimum_deadline),
        safe_next_action: format!(
            "raise sync.max_duration_secs to at least {}s (currently {}s), or lower sync.reserve_secs_per_command (currently {}s) to a proven-safe value: the configured deadline implies a {computed_bound}-member bound, below the {MINIMUM_SOUND_CAPACITY}-member floor admission can enforce, and draining a caravan can never raise a bound derived from configuration alone",
            minimum_deadline.as_secs(),
            deadline.as_secs(),
            reserve_secs_per_command(context),
        ),
    }
}

/// A bound that gates a caravan holding fewer than two members contradicts
/// itself: such a chain cannot shrink into admissibility, so no drain can
/// clear the gate. Reported as a defect rather than silently closing joins.
fn capacity_contradiction(
    context: &AppContext,
    deadline: Duration,
    members: u64,
    bound: u64,
) -> CapacityDefect {
    CapacityDefect {
        code: "sync_budget_capacity_contradiction".to_owned(),
        safe_next_action: format!(
            "admission reported a {members}-member caravan at a {bound}-member bound, which no drain can ever clear; treat this as a defect and raise sync.max_duration_secs to at least {}s, or lower sync.reserve_secs_per_command (currently {}s), before relying on capacity gating",
            minimum_sound_deadline(context).as_secs(),
            reserve_secs_per_command(context),
        ),
        ..capacity_defect(context, deadline, bound)
    }
}

/// Largest chain size whose last member is still guaranteed to drain inside
/// one configured deadline, or the typed defect that makes the configured
/// arithmetic unusable as an admission bound.
///
/// The bound models the hardest ordinary shape: every earlier member already
/// retained, one trailing pending rewrite, and one native auto-merge drop. It
/// is priced with the same actual-work reserve that produces `required_ms`, so
/// admission and the apply reserve can never disagree about the same chain.
pub(super) fn admission_capacity(
    context: &AppContext,
    deadline: Duration,
) -> Result<u64, CapacityDefect> {
    let computed = computed_capacity(context, deadline);
    if computed < MINIMUM_SOUND_CAPACITY {
        return Err(capacity_defect(context, deadline, computed));
    }
    Ok(computed)
}

/// Bound-plus-defect evidence pair, so refusal receipts never carry a zero or
/// otherwise unsound `max_admissible_members`.
pub(super) fn capacity_evidence(
    context: &AppContext,
    deadline: Duration,
) -> (Option<u64>, Option<CapacityDefect>) {
    match admission_capacity(context, deadline) {
        Ok(bound) => (Some(bound), None),
        Err(defect) => (None, Some(defect)),
    }
}

/// Deterministic disposition of one caravan of `members` members against the
/// configured admission bound.
pub(super) fn capacity_gate(
    context: &AppContext,
    deadline: Duration,
    members: u64,
) -> CapacityGate {
    match admission_capacity(context, deadline) {
        Ok(bound) => gate_for_bound(context, deadline, members, bound),
        Err(defect) => CapacityGate::Defect(defect),
    }
}

/// Configured hard batch bound, independent of the physical apply reserve.
///
/// `None` preserves the historical dynamic-capacity-only model, so the default
/// `stack_type: caravan` path is unchanged. GitHub mode defaults to eight
/// because one native Stack is a bounded atomic merge batch.
#[must_use]
pub(crate) fn configured_batch_bound(context: &AppContext) -> Option<u64> {
    context.config.effective_max_caravan_length().map(u64::from)
}

/// Classify one caravan against an explicit bound.
///
/// Gating a caravan that holds fewer than `MINIMUM_SOUND_CAPACITY` members is a
/// self-evident contradiction: such a chain cannot shrink into admissibility,
/// so no drain can ever clear the gate. It is reported as a typed defect rather
/// than quietly closing admission (bd-b1c7b7). A sound bound can never produce
/// this state; the guard exists so a future arithmetic regression fails loudly
/// instead of silently stalling the queue.
pub(super) fn gate_for_bound(
    context: &AppContext,
    deadline: Duration,
    members: u64,
    bound: u64,
) -> CapacityGate {
    // A configured batch bound is authoritative policy, not derived arithmetic:
    // reaching it is ordinary fullness at any chain size and can always be
    // cleared by draining or by using another caravan.
    if let Some(batch) = configured_batch_bound(context) {
        if members >= batch {
            return CapacityGate::AtCapacity {
                bound: batch.min(bound),
            };
        }
    }
    if members < bound {
        CapacityGate::Open { bound }
    } else if members < MINIMUM_SOUND_CAPACITY {
        CapacityGate::Defect(capacity_contradiction(context, deadline, members, bound))
    } else {
        CapacityGate::AtCapacity { bound }
    }
}

/// Deterministic status projection for the configured deadline, computed from
/// discovery alone so an operator sees the reserve before any sync refusal.
#[must_use]
pub fn project_status(context: &AppContext, status: &StatusOutput) -> SyncBudgetStatus {
    let deadline = super::sync_operation_budget(context);
    let selected = status
        .analysis
        .fleet
        .caravans
        .iter()
        .filter(|caravan| !caravan.parked)
        .filter(|caravan| {
            !status
                .pauses
                .iter()
                .any(|pause| pause.state.is_effective() && pause.record.caravan_head == caravan.id)
        })
        .cloned()
        .collect::<Vec<_>>();
    project(context, status, &selected, deadline)
}

/// Safe next action for one projected caravan.
///
/// Gating guidance is only ever emitted for a sound bound; under a defect the
/// text names the configuration change that repairs it, never a drain that
/// provably cannot (bd-b1c7b7).
fn caravan_next_action(
    context: &AppContext,
    deadline: Duration,
    members: u64,
    gate: &CapacityGate,
    admission: &PhysicalApplyAdmission,
) -> String {
    if !context.config.rebase_on_join {
        return "physical chain rebuilding is disabled; no apply reserve applies".to_owned();
    }
    let admitted = admission.admitted_prs.len();
    match gate {
        CapacityGate::Defect(defect) => format!(
            "admission capacity is unsound and no drain can repair it: {}",
            defect.safe_next_action,
        ),
        CapacityGate::AtCapacity { bound } => format!(
            "let this caravan drain before admitting another member: {members} members already reach the {bound}-member bound implied by sync.max_duration_secs={}s and sync.reserve_secs_per_command={}s",
            deadline.as_secs(),
            reserve_secs_per_command(context),
        ),
        CapacityGate::Open { .. } if !admission.makes_progress() => {
            "no member can drain inside the configured deadline: raise sync.max_duration_secs or lower a proven-safe sync.reserve_secs_per_command before the next tick".to_owned()
        }
        CapacityGate::Open { .. } if !admission.deferred_convergence => {
            "one `cara sync --all` tick can apply and converge this caravan".to_owned()
        }
        CapacityGate::Open { .. } if admission.deferred.is_empty() => format!(
            "run `cara sync --all`; one tick applies all {admitted} member(s) and converges them on the next tick without replaying completed rewrites"
        ),
        CapacityGate::Open { .. } => format!(
            "run `cara sync --all`; one tick applies {admitted} of {members} member(s) and resumes the rest next tick without replaying completed rewrites"
        ),
    }
}

/// Build the deterministic status projection for the configured deadline.
fn project(
    context: &AppContext,
    status: &StatusOutput,
    selected: &[Caravan],
    deadline: Duration,
) -> SyncBudgetStatus {
    let rebase_on_join = context.config.rebase_on_join;
    let batch_bound = configured_batch_bound(context);
    let capacity = admission_capacity(context, deadline);
    let mut projections = Vec::with_capacity(selected.len());
    for caravan in selected {
        let chains = chain_costs_from_status(status, std::slice::from_ref(caravan));
        let worst_case = complete_budget(context, &chains);
        let retained = chains
            .iter()
            .map(|chain| ChainCost {
                caravan_id: chain.caravan_id,
                members: chain
                    .members
                    .iter()
                    .map(|member| MemberCost {
                        pending: false,
                        ..*member
                    })
                    .collect(),
                externally_armed_non_roots: chain.externally_armed_non_roots,
            })
            .collect::<Vec<_>>();
        let retained_budget = complete_budget(context, &retained);
        let admission = admit_physical_prefix(context, &chains, deadline);
        let processable_prefix = admission.admitted_prs.clone();
        let members = u64::try_from(caravan.members.len()).unwrap_or(u64::MAX);
        let gate = capacity_gate(context, deadline, members);
        let at_capacity = matches!(gate, CapacityGate::AtCapacity { .. });
        let safe_next_action = caravan_next_action(context, deadline, members, &gate, &admission);
        projections.push(CaravanBudgetProjection {
            caravan_id: caravan.id,
            members: caravan.members.clone(),
            required_command_slots: worst_case.command_slots,
            required_ms: super::duration_millis(worst_case.required),
            retained_command_slots: retained_budget.command_slots,
            retained_ms: super::duration_millis(retained_budget.required),
            processable_prefix,
            deferred: admission.deferred,
            deferred_convergence: admission.deferred_convergence,
            at_capacity,
            safe_next_action,
        });
    }
    let blocked_candidate = status.admission.next_candidate.filter(|_| {
        (rebase_on_join || batch_bound.is_some())
            && !projections.is_empty()
            && projections.iter().all(|projection| projection.at_capacity)
    });
    let safe_next_action = if let Some(bound) = batch_bound.filter(|_| blocked_candidate.is_some())
    {
        format!(
            "every caravan holds the configured {bound}-member batch bound (max_caravan_length); admission opens another caravan rather than extending a full batch"
        )
    } else if !rebase_on_join {
        "physical chain rebuilding is disabled; no apply reserve applies".to_owned()
    } else if let Err(defect) = &capacity {
        format!(
            "admission capacity is unsound, so joins fail loudly with {} instead of being quietly gated; draining cannot repair it: {}",
            defect.code, defect.safe_next_action,
        )
    } else if blocked_candidate.is_some() {
        let bound = capacity.as_ref().copied().unwrap_or_default();
        format!(
            "every caravan holds the {bound}-member bound implied by the configured deadline; admission stays closed with caravan_budget_capacity_exhausted until a caravan drains"
        )
    } else if projections
        .iter()
        .any(|projection| projection.deferred_convergence)
    {
        "run `cara sync --all`; bounded prefixes apply now and the remaining members plus ordinary convergence resume on the next tick".to_owned()
    } else {
        "run `cara sync --all`; every selected caravan fits one tick".to_owned()
    };
    SyncBudgetStatus {
        schema_version: 2,
        rebase_on_join,
        deadline_ms: super::duration_millis(deadline),
        command_timeout_ms: context.config.command_timeout_secs.saturating_mul(1_000),
        reserve_ms_per_command: reserve_secs_per_command(context).saturating_mul(1_000),
        deadline_command_slots: deadline_command_slots(context, deadline),
        max_admissible_members: match (batch_bound, capacity.as_ref().copied().ok()) {
            (Some(batch), Some(dynamic)) if rebase_on_join => Some(batch.min(dynamic)),
            (Some(batch), _) => Some(batch),
            (None, dynamic) => dynamic,
        },
        capacity_defect: capacity.err().filter(|_| rebase_on_join),
        caravans: projections,
        blocked_candidate,
        safe_next_action,
    }
}
