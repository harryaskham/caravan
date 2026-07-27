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
//! members whose exact cumulative ancestry already holds cost no push, no
//! auto-merge disable, and no force invalidation, so completed prefixes make
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
    PHYSICAL_FORCE_INVALIDATION_COMMAND_SLOTS, PHYSICAL_FORCE_INVALIDATION_MUTATIONS,
    PHYSICAL_RECONCILIATION_COMMAND_SLOTS_PER_CARAVAN,
    PHYSICAL_RECONCILIATION_COMMAND_SLOTS_PER_MEMBER,
};

/// Hard upper bound on the capacity search so a pathological configuration
/// cannot turn a deterministic projection into an unbounded loop.
const MAX_CAPACITY_SEARCH_MEMBERS: u64 = 4_096;

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
    /// An exact-generation force intent must be invalidated and possibly
    /// compensated before/after rewriting this member.
    pub force_labelled: bool,
}

impl MemberCost {
    /// Worst-case shape used by projections that cannot plan Git ranges.
    const fn worst_case(pr: PrNumber, auto_merge_enabled: bool, force_labelled: bool) -> Self {
        Self {
            pr,
            pending: true,
            auto_merge_enabled,
            force_labelled,
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
        let auto_merge: u64 = if self.auto_merge_enabled { 1 } else { 0 };
        let force = if self.force_labelled {
            PHYSICAL_FORCE_INVALIDATION_COMMAND_SLOTS
        } else {
            0
        };
        auto_merge.saturating_add(force)
    }

    const fn control_mutations(self) -> u64 {
        if !self.pending {
            return 0;
        }
        let auto_merge: u64 = if self.auto_merge_enabled { 1 } else { 0 };
        let force = if self.force_labelled {
            PHYSICAL_FORCE_INVALIDATION_MUTATIONS
        } else {
            0
        };
        auto_merge.saturating_add(force)
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

/// Deterministic, provider-free projection of the physical apply reserve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncBudgetStatus {
    pub schema_version: u32,
    /// Whether physical chain rebuilding (and therefore this reserve) applies.
    pub rebase_on_join: bool,
    pub deadline_ms: u64,
    pub command_timeout_ms: u64,
    /// Whole-tick deadline expressed in `command_timeout_secs` command slots.
    pub deadline_command_slots: u64,
    /// Largest chain size whose last member is still guaranteed to drain.
    pub max_admissible_members: u64,
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
            schema_version: 1,
            rebase_on_join: false,
            deadline_ms: 0,
            command_timeout_ms: 0,
            deadline_command_slots: 0,
            max_admissible_members: 0,
            caravans: Vec::new(),
            blocked_candidate: None,
            safe_next_action: "physical chain rebuilding is disabled; no apply reserve applies"
                .to_owned(),
        }
    }
}

fn slots_to_duration(context: &AppContext, slots: u64) -> Duration {
    Duration::from_secs(context.config.command_timeout_secs.saturating_mul(slots))
}

/// Whole-tick deadline expressed in `command_timeout_secs` command slots.
pub(super) fn deadline_command_slots(context: &AppContext, deadline: Duration) -> u64 {
    if context.config.command_timeout_secs == 0 {
        return u64::MAX;
    }
    deadline.as_secs() / context.config.command_timeout_secs
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
                        || MemberCost::worst_case(*number, false, false),
                        |pull_request| {
                            MemberCost::worst_case(
                                *number,
                                pull_request.auto_merge.enabled,
                                pull_request.has_label("caravan-force"),
                            )
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

/// Largest chain size whose last member is still guaranteed to drain inside
/// one configured deadline.
///
/// The bound models the hardest ordinary shape: every earlier member already
/// retained, one trailing pending rewrite, and one native auto-merge drop.
pub(super) fn max_admissible_members(context: &AppContext, deadline: Duration) -> u64 {
    let slots = deadline_command_slots(context, deadline);
    let mut admissible = 0;
    let mut size = 1;
    while size <= MAX_CAPACITY_SEARCH_MEMBERS {
        let required = size
            .saturating_sub(1)
            .saturating_mul(PHYSICAL_APPLY_COMMAND_SLOTS_PER_RETAINED_MEMBER)
            .saturating_add(PHYSICAL_APPLY_COMMAND_SLOTS_PER_PENDING_MEMBER)
            .saturating_add(1)
            .saturating_add(PHYSICAL_FIXED_POST_WRITE_COMMAND_SLOTS);
        if required >= slots {
            break;
        }
        admissible = size;
        size += 1;
    }
    admissible
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

/// Build the deterministic status projection for the configured deadline.
fn project(
    context: &AppContext,
    status: &StatusOutput,
    selected: &[Caravan],
    deadline: Duration,
) -> SyncBudgetStatus {
    let rebase_on_join = context.config.rebase_on_join;
    let capacity = max_admissible_members(context, deadline);
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
        let at_capacity = members >= capacity;
        let safe_next_action = if !rebase_on_join {
            "physical chain rebuilding is disabled; no apply reserve applies".to_owned()
        } else if at_capacity {
            format!(
                "let this caravan drain before admitting another member: {members} members already reach the {capacity}-member bound implied by sync.max_duration_secs={}s and command_timeout_secs={}s",
                deadline.as_secs(),
                context.config.command_timeout_secs,
            )
        } else if !admission.makes_progress() {
            "no member can drain inside the configured deadline: raise sync.max_duration_secs or lower a proven-safe command_timeout_secs before the next tick".to_owned()
        } else if !admission.deferred_convergence {
            "one `cara sync --all` tick can apply and converge this caravan".to_owned()
        } else if admission.deferred.is_empty() {
            format!(
                "run `cara sync --all`; one tick applies all {} member(s) and converges them on the next tick without replaying completed rewrites",
                processable_prefix.len(),
            )
        } else {
            format!(
                "run `cara sync --all`; one tick applies {} of {members} member(s) and resumes the rest next tick without replaying completed rewrites",
                processable_prefix.len(),
            )
        };
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
        rebase_on_join
            && !projections.is_empty()
            && projections.iter().all(|projection| projection.at_capacity)
    });
    let safe_next_action = if !rebase_on_join {
        "physical chain rebuilding is disabled; no apply reserve applies".to_owned()
    } else if blocked_candidate.is_some() {
        format!(
            "every caravan holds the {capacity}-member bound implied by the configured deadline; admission stays closed with caravan_budget_capacity_exhausted until a caravan drains"
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
        schema_version: 1,
        rebase_on_join,
        deadline_ms: super::duration_millis(deadline),
        command_timeout_ms: context.config.command_timeout_secs.saturating_mul(1_000),
        deadline_command_slots: deadline_command_slots(context, deadline),
        max_admissible_members: capacity,
        caravans: projections,
        blocked_candidate,
        safe_next_action,
    }
}
