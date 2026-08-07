//! Policy-to-provider bridge for native GitHub Stack landing.
//!
//! Cara owns candidacy, holds, compatibility, generation integrity, and CI
//! authority; the Stack adapters own exact provider calls. This module is the
//! narrow seam between them: it converts already-computed status facts into the
//! provider planner's blocker vocabulary. It derives no new policy, performs no
//! provider access, and never decides whether native mode is permitted — the
//! initialization fence still owns that.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::github::{
    GitHubStackEntryGeneration, GitHubStackGeneration, GitHubStackMergeBlocker,
    GitHubStackMergeEntryEvidence,
};
use crate::model::{
    Caravan, CompatibilityOutcome, CompatibilityReport, MergeCandidateFreshness,
    MergeCandidateIdentity, PrNumber, PullRequestSnapshot, PullRequestState,
};
use crate::read::StatusOutput;

const FORCE_LABEL: &str = "caravan-force";

/// Caller-owned CI verdict for one exact entry generation.
///
/// CI authority stays in the sync lane, which already reasons about required
/// runs, supersession, and grace windows. This bridge only records the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackEntryCi {
    Ready,
    NotReady,
}

/// The exact status facts this bridge is allowed to consider.
///
/// Taking a narrow view rather than the whole status keeps the seam honest: it
/// cannot quietly start depending on scheduler state it does not own.
#[derive(Debug, Clone, Copy)]
pub struct StackPolicyFacts<'a> {
    pub pull_requests: &'a BTreeMap<PrNumber, PullRequestSnapshot>,
    /// Exact provider merge refs. Native readiness is candidate-scoped: source
    /// heads remain immutable and are never made cumulative by force-pushing.
    pub merge_candidates: &'a BTreeMap<PrNumber, MergeCandidateIdentity>,
    pub compatibility: &'a [CompatibilityReport],
    /// Members of every caravan under an effective hold.
    pub held_members: &'a BTreeSet<PrNumber>,
}

/// Collect held caravan members from exact status.
#[must_use]
pub fn held_caravan_members(status: &StatusOutput) -> BTreeSet<PrNumber> {
    let mut held = BTreeSet::new();
    for pause in status
        .pauses
        .iter()
        .filter(|pause| pause.state.is_effective())
    {
        if let Some(caravan) = status
            .analysis
            .fleet
            .caravans
            .iter()
            .find(|caravan| caravan.id == pause.record.caravan_head)
        {
            held.extend(caravan.members.iter().copied());
        }
    }
    held
}

/// Convert exact status facts into provider merge-readiness evidence.
///
/// Every returned entry preserves its exact provider generation, so the planner
/// still fails closed if the Stack it reads disagrees with this evidence.
#[must_use]
pub fn stack_merge_evidence(
    facts: StackPolicyFacts<'_>,
    stack: &GitHubStackGeneration,
    ci: &dyn Fn(PrNumber) -> StackEntryCi,
) -> Vec<GitHubStackMergeEntryEvidence> {
    stack
        .topology
        .entries
        .iter()
        .map(|entry| GitHubStackMergeEntryEvidence {
            generation: entry.clone(),
            blockers: entry_blockers(facts, stack, entry, ci(entry.pr)),
        })
        .collect()
}

fn entry_blockers(
    facts: StackPolicyFacts<'_>,
    stack: &GitHubStackGeneration,
    entry: &GitHubStackEntryGeneration,
    ci: StackEntryCi,
) -> Vec<GitHubStackMergeBlocker> {
    let mut blockers = Vec::new();
    if !stack.open {
        blockers.push(GitHubStackMergeBlocker::StackClosed);
    }
    if !entry.stack_state.eq_ignore_ascii_case("open") {
        blockers.push(GitHubStackMergeBlocker::StackEntryNotOpen);
    }

    match facts.pull_requests.get(&entry.pr) {
        // Absent discovery is never "fine": the provider generation cannot be
        // compared against anything Cara actually reasoned about.
        None => blockers.push(GitHubStackMergeBlocker::GraphInexact),
        Some(pull) => {
            if pull.state != PullRequestState::Open {
                blockers.push(GitHubStackMergeBlocker::PullRequestNotOpen);
            }
            if pull.draft {
                blockers.push(GitHubStackMergeBlocker::Draft);
            }
            if pull.head != entry.head || pull.base != entry.base {
                blockers.push(GitHubStackMergeBlocker::GraphInexact);
            }
            // Native Stack merge documents no administrator bypass, so durable
            // force intent cannot be honored here. Refusing is the only honest
            // outcome; silently landing a forced entry would misrepresent it.
            if pull.labels.contains(FORCE_LABEL) {
                blockers.push(GitHubStackMergeBlocker::ForceUnsupported);
            }
        }
    }

    if let Some(blocker) = synthetic_candidate_blocker(facts, entry) {
        blockers.push(blocker);
    }
    if facts.held_members.contains(&entry.pr) {
        blockers.push(GitHubStackMergeBlocker::Held);
    }
    if mechanically_blocked(facts, entry) {
        blockers.push(GitHubStackMergeBlocker::MechanicallyBlocked);
    }
    if ci == StackEntryCi::NotReady {
        blockers.push(GitHubStackMergeBlocker::RequiredChecksNotReady);
    }
    blockers
}

/// A non-clean compatibility verdict for this exact candidate/target pair.
///
/// Unknown or absent evidence is not treated as blocked here: the planner
/// re-derives provider truth, and a missing pair is reported through graph
/// problems rather than silently converted into a merge refusal.
fn mechanically_blocked(facts: StackPolicyFacts<'_>, entry: &GitHubStackEntryGeneration) -> bool {
    facts.compatibility.iter().any(|report| {
        report.candidate == entry.head
            && report.target == entry.base
            && report.outcome != CompatibilityOutcome::Clean
    })
}

/// Require the exact current two-parent provider candidate for this immutable
/// source generation. The first parent is the Stack entry's exact base (current
/// main for the root, predecessor source head for a child); the second is the
/// immutable source head. This is the cumulative CI identity. A stale/missing
/// candidate waits for provider regeneration and never authorizes a source push.
fn synthetic_candidate_blocker(
    facts: StackPolicyFacts<'_>,
    entry: &GitHubStackEntryGeneration,
) -> Option<GitHubStackMergeBlocker> {
    let Some(candidate) = facts.merge_candidates.get(&entry.pr) else {
        return Some(GitHubStackMergeBlocker::SyntheticCandidateMissing);
    };
    if candidate.base != entry.base || candidate.head != entry.head {
        return Some(GitHubStackMergeBlocker::SyntheticCandidateStale);
    }
    let Some(synthetic) = candidate.synthetic.as_ref() else {
        return Some(GitHubStackMergeBlocker::SyntheticCandidateMissing);
    };
    if candidate.freshness != MergeCandidateFreshness::Fresh
        || candidate.compared_base.as_ref() != Some(&entry.base)
    {
        return Some(GitHubStackMergeBlocker::SyntheticCandidateStale);
    }
    if synthetic.parents.as_slice() != [entry.base.oid.clone(), entry.head.oid.clone()] {
        return Some(GitHubStackMergeBlocker::SyntheticCandidateTopologyMismatch);
    }
    None
}

/// Which landing path a caravan must use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "route")]
pub enum StackLandingRoute {
    /// The stable default: Cara owns the landing exactly as before.
    CaravanOwned,
    /// A one-member native caravan has no representable provider Stack. Cara
    /// validates and lands its exact root through the ordinary owned path.
    SingletonCaravanOwned,
    /// One exact provider Stack, reached only through the lock-fenced
    /// transaction in `github::stack_land`.
    NativeStack { stack_number: u64 },
}

/// Decide the landing path for one caravan, failing closed on every gate.
///
/// The default backend returns immediately without consulting capability,
/// rollout opt-in, or provider Stack state, so a repository that never selected
/// `stack_type: github` is unaffected by any of this logic.
///
/// This never opens the workflow fence. `initialization::require_ready` still
/// refuses native mutation ahead of any caller reaching this decision; routing
/// exists so the remaining rollout step is exactly one reviewed change rather
/// than new untested wiring.
pub fn route_landing(
    config: &crate::config::CaravanConfig,
    backend: &crate::read::StackBackendStatus,
    caravan: &Caravan,
) -> Result<StackLandingRoute, crate::AppError> {
    if config.stack_type != crate::config::StackType::Github {
        return Ok(StackLandingRoute::CaravanOwned);
    }
    if backend.capability != crate::read::StackCapability::Available {
        return Err(route_refusal(
            "github_stack_capability_not_proven",
            format!(
                "native landing requires a proven Stack capability, observed `{}`",
                backend.capability.code()
            ),
            "an unproven capability is never absence; resolve it or set stack_type: caravan",
            true,
        ));
    }
    if !config.stack_rollout.mutations_opt_in {
        return Err(route_refusal(
            "github_stack_repository_not_opted_in",
            "native landing requires an explicit reviewed repository opt-in".to_owned(),
            "record stack_rollout.mutations_opt_in with reviewed_by; opting in alone still does not enable mutations",
            false,
        ));
    }
    if backend.provider_stacks_truncated {
        return Err(route_refusal(
            "github_stack_inventory_truncated",
            "a truncated Stack inventory cannot prove which Stack owns this caravan".to_owned(),
            "re-read status once the provider returns a complete inventory page",
            true,
        ));
    }

    let matching = backend
        .native_stacks
        .iter()
        .filter(|native| native.caravan_id == Some(caravan.id))
        .collect::<Vec<_>>();
    if caravan.members.len() == 1 && matching.is_empty() {
        return Ok(StackLandingRoute::SingletonCaravanOwned);
    }
    let [native] = matching.as_slice() else {
        return Err(mapping_refusal(caravan, matching.len()));
    };
    if native.consistency != crate::read::StackConsistency::Exact {
        return Err(route_refusal(
            "github_stack_generation_drifted",
            format!(
                "caravan #{} Stack #{} is `{}`, not exact",
                caravan.id,
                native.stack.number,
                native.consistency.code()
            ),
            "reconcile the drifted Stack generation before landing",
            true,
        ));
    }
    Ok(StackLandingRoute::NativeStack {
        stack_number: native.stack.number,
    })
}

fn route_refusal(code: &str, message: String, next: &str, retryable: bool) -> crate::AppError {
    crate::AppError::structured(
        mcp_cli::ErrorCategory::Validation,
        code,
        message,
        Some(serde_json::json!({
            "mutated": false,
            "retryable": retryable,
            "safe_next_action": next,
        })),
    )
}

fn mapping_refusal(caravan: &Caravan, mapping_count: usize) -> crate::AppError {
    crate::AppError::structured(
        mcp_cli::ErrorCategory::Validation,
        "github_stack_caravan_mapping_ambiguous",
        format!(
            "{}-member caravan #{} maps to {mapping_count} provider Stacks; exactly one is required",
            caravan.members.len(),
            caravan.id,
        ),
        Some(serde_json::json!({
            "caravan_id": caravan.id,
            "members": caravan.members,
            "member_count": caravan.members.len(),
            "mapping_count": mapping_count,
            "mutated": false,
            "retryable": false,
            "safe_next_action": "resolve the provider/Caravan mapping before landing; only an authoritative singleton with zero mappings may use the reviewed singleton route",
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::GitHubStackTopology;
    use crate::model::{
        AutoMergeState, BranchSnapshot, CommitOid, RepositoryId, SyntheticMergeCandidate,
    };

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

    fn entry(position: u32, number: u64, base: BranchSnapshot) -> GitHubStackEntryGeneration {
        GitHubStackEntryGeneration {
            position,
            pr: PrNumber(number),
            stack_state: "open".to_owned(),
            pull_request_state: PullRequestState::Open,
            draft: false,
            merged_at: None,
            base,
            head: branch(&format!("head-{number}"), &format!("{number}aaaaa")),
        }
    }

    fn stack() -> GitHubStackGeneration {
        let base = branch("main", "base000");
        let root = entry(0, 101, base.clone());
        let child = entry(1, 102, root.head.clone());
        GitHubStackGeneration {
            id: 1,
            number: 42,
            node_id: "S_bridge".to_owned(),
            open: true,
            created_at: "2026-08-01T09:00:00Z".to_owned(),
            topology: GitHubStackTopology {
                base,
                entries: vec![root, child],
            },
        }
    }

    fn pull_requests(stack: &GitHubStackGeneration) -> BTreeMap<PrNumber, PullRequestSnapshot> {
        stack
            .topology
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.pr,
                    PullRequestSnapshot {
                        number: entry.pr,
                        title: format!("PR {}", entry.pr),
                        url: String::new(),
                        state: PullRequestState::Open,
                        draft: false,
                        head: entry.head.clone(),
                        base: entry.base.clone(),
                        cross_repository: false,
                        labels: BTreeSet::from(["caravan".to_owned()]),
                        auto_merge: AutoMergeState::disabled(),
                        checks: Vec::new(),
                        created_at: None,
                        merged_at: None,
                        updated_at: None,
                        merge_state_status: None,
                    },
                )
            })
            .collect()
    }

    fn merge_candidates(
        stack: &GitHubStackGeneration,
    ) -> BTreeMap<PrNumber, MergeCandidateIdentity> {
        stack
            .topology
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.pr,
                    MergeCandidateIdentity {
                        pr: entry.pr,
                        provider_updated_at: "2026-08-01T09:00:00Z".to_owned(),
                        observed_at: "2026-08-01T09:00:01Z".to_owned(),
                        base: entry.base.clone(),
                        head: entry.head.clone(),
                        synthetic: Some(SyntheticMergeCandidate {
                            git_ref: format!("refs/pull/{}/merge", entry.pr),
                            oid: CommitOid(format!("candidate-{}", entry.pr)),
                            tree_oid: CommitOid(format!("tree-{}", entry.pr)),
                            parents: vec![entry.base.oid.clone(), entry.head.oid.clone()],
                        }),
                        auto_merge: crate::model::NativeAutoMergeState {
                            enabled: false,
                            merge_method: None,
                            actor: None,
                        },
                        freshness: MergeCandidateFreshness::Fresh,
                        compared_base: Some(entry.base.clone()),
                        stale_base: false,
                        stale_head: false,
                        stale_reasons: Vec::new(),
                    },
                )
            })
            .collect()
    }

    fn facts<'a>(
        pull_requests: &'a BTreeMap<PrNumber, PullRequestSnapshot>,
        merge_candidates: &'a BTreeMap<PrNumber, MergeCandidateIdentity>,
        compatibility: &'a [CompatibilityReport],
        held: &'a BTreeSet<PrNumber>,
    ) -> StackPolicyFacts<'a> {
        StackPolicyFacts {
            pull_requests,
            merge_candidates,
            compatibility,
            held_members: held,
        }
    }

    fn ready(_: PrNumber) -> StackEntryCi {
        StackEntryCi::Ready
    }

    #[test]
    fn a_clean_exact_stack_reports_no_policy_blockers() {
        let stack = stack();
        let pulls = pull_requests(&stack);
        let candidates = merge_candidates(&stack);
        let held = BTreeSet::new();
        let evidence = stack_merge_evidence(facts(&pulls, &candidates, &[], &held), &stack, &ready);

        assert_eq!(evidence.len(), 2);
        assert!(evidence.iter().all(|entry| entry.blockers.is_empty()));
        assert_eq!(
            evidence
                .iter()
                .map(|entry| entry.generation.clone())
                .collect::<Vec<_>>(),
            stack.topology.entries
        );
    }

    #[test]
    fn provider_drift_from_cara_discovery_is_never_silently_accepted() {
        let stack = stack();
        let mut pulls = pull_requests(&stack);
        pulls.get_mut(&PrNumber(102)).expect("child").head = branch("head-102", "moved0");
        pulls.remove(&PrNumber(101));
        let candidates = merge_candidates(&stack);
        let held = BTreeSet::new();

        let evidence = stack_merge_evidence(facts(&pulls, &candidates, &[], &held), &stack, &ready);

        assert!(
            evidence[0]
                .blockers
                .contains(&GitHubStackMergeBlocker::GraphInexact),
            "an entry absent from discovery cannot be compared against anything"
        );
        assert!(
            evidence[1]
                .blockers
                .contains(&GitHubStackMergeBlocker::GraphInexact)
        );
    }

    #[test]
    fn native_readiness_requires_exact_fresh_cumulative_candidates() {
        let stack = stack();
        let pulls = pull_requests(&stack);
        let held = BTreeSet::new();

        let missing = BTreeMap::new();
        let evidence = stack_merge_evidence(facts(&pulls, &missing, &[], &held), &stack, &ready);
        assert!(evidence.iter().all(|entry| {
            entry
                .blockers
                .contains(&GitHubStackMergeBlocker::SyntheticCandidateMissing)
        }));

        let mut stale = merge_candidates(&stack);
        stale.get_mut(&PrNumber(101)).expect("root").freshness = MergeCandidateFreshness::StaleBase;
        let evidence = stack_merge_evidence(facts(&pulls, &stale, &[], &held), &stack, &ready);
        assert!(
            evidence[0]
                .blockers
                .contains(&GitHubStackMergeBlocker::SyntheticCandidateStale)
        );

        let mut malformed = merge_candidates(&stack);
        malformed
            .get_mut(&PrNumber(102))
            .expect("child")
            .synthetic
            .as_mut()
            .expect("candidate")
            .parents
            .reverse();
        let evidence = stack_merge_evidence(facts(&pulls, &malformed, &[], &held), &stack, &ready);
        assert!(
            evidence[1]
                .blockers
                .contains(&GitHubStackMergeBlocker::SyntheticCandidateTopologyMismatch)
        );
    }

    #[test]
    fn durable_force_intent_is_refused_because_native_merge_has_no_admin_bypass() {
        let stack = stack();
        let mut pulls = pull_requests(&stack);
        pulls
            .get_mut(&PrNumber(101))
            .expect("root")
            .labels
            .insert(FORCE_LABEL.to_owned());
        let candidates = merge_candidates(&stack);
        let held = BTreeSet::new();

        let evidence = stack_merge_evidence(facts(&pulls, &candidates, &[], &held), &stack, &ready);

        assert!(
            evidence[0]
                .blockers
                .contains(&GitHubStackMergeBlocker::ForceUnsupported)
        );
        assert!(evidence[1].blockers.is_empty());
    }

    #[test]
    fn conflict_hold_draft_closed_and_ci_verdicts_map_to_exact_blockers() {
        let stack = stack();
        let mut pulls = pull_requests(&stack);
        let root = pulls.get_mut(&PrNumber(101)).expect("root");
        root.draft = true;
        root.state = PullRequestState::Closed;
        let compatibility = vec![CompatibilityReport {
            candidate: stack.topology.entries[1].head.clone(),
            target: stack.topology.entries[1].base.clone(),
            outcome: CompatibilityOutcome::Conflict,
            conflicting_paths: vec!["src/lib.rs".to_owned()],
            diagnostic: None,
        }];
        let candidates = merge_candidates(&stack);
        let held = BTreeSet::from([PrNumber(102)]);

        let evidence = stack_merge_evidence(
            facts(&pulls, &candidates, &compatibility, &held),
            &stack,
            &|pr| {
                if pr == PrNumber(101) {
                    StackEntryCi::NotReady
                } else {
                    StackEntryCi::Ready
                }
            },
        );

        for blocker in [
            GitHubStackMergeBlocker::PullRequestNotOpen,
            GitHubStackMergeBlocker::Draft,
            GitHubStackMergeBlocker::RequiredChecksNotReady,
        ] {
            assert!(evidence[0].blockers.contains(&blocker), "{blocker:?}");
        }
        for blocker in [
            GitHubStackMergeBlocker::Held,
            GitHubStackMergeBlocker::MechanicallyBlocked,
        ] {
            assert!(evidence[1].blockers.contains(&blocker), "{blocker:?}");
        }
    }

    #[test]
    fn a_closed_stack_or_non_open_entry_blocks_every_affected_position() {
        let mut stack = stack();
        stack.open = false;
        stack.topology.entries[1].stack_state = "queued".to_owned();
        let pulls = pull_requests(&stack);
        let candidates = merge_candidates(&stack);
        let held = BTreeSet::new();

        let evidence = stack_merge_evidence(facts(&pulls, &candidates, &[], &held), &stack, &ready);

        assert!(evidence.iter().all(|entry| {
            entry
                .blockers
                .contains(&GitHubStackMergeBlocker::StackClosed)
        }));
        assert!(
            evidence[1]
                .blockers
                .contains(&GitHubStackMergeBlocker::StackEntryNotOpen)
        );
        assert!(
            !evidence[0]
                .blockers
                .contains(&GitHubStackMergeBlocker::StackEntryNotOpen)
        );
    }

    fn backend(
        capability: crate::read::StackCapability,
        natives: Vec<crate::read::NativeStackStatus>,
    ) -> crate::read::StackBackendStatus {
        crate::read::StackBackendStatus {
            configured: crate::config::StackType::Github,
            capability,
            mutation_support: crate::read::StackMutationSupport::ReadOnlyPreview,
            native_stacks: natives,
            provider_stacks_truncated: false,
            missing_caravans: Vec::new(),
            problems: Vec::new(),
        }
    }

    fn native(
        caravan_id: Option<PrNumber>,
        consistency: crate::read::StackConsistency,
    ) -> crate::read::NativeStackStatus {
        crate::read::NativeStackStatus {
            stack: crate::github::GitHubStackSnapshot {
                id: 1,
                number: 42,
                node_id: "S_route".to_owned(),
                base: crate::github::GitHubStackBase {
                    ref_name: "main".to_owned(),
                },
                open: true,
                created_at: "2026-08-01T09:00:00Z".to_owned(),
                pull_requests: Vec::new(),
            },
            caravan_id,
            consistency,
            problems: Vec::new(),
        }
    }

    fn github_config() -> crate::config::CaravanConfig {
        let mut config = crate::config::CaravanConfig::default();
        config.stack_type = crate::config::StackType::Github;
        config.stack_rollout.mutations_opt_in = true;
        config.stack_rollout.reviewed_by = "operator".to_owned();
        config
    }

    fn caravan(members: &[u64]) -> Caravan {
        Caravan::new(members.iter().copied().map(PrNumber).collect()).expect("non-empty caravan")
    }

    #[test]
    fn the_default_backend_routes_to_caravan_without_consulting_any_gate() {
        // Deliberately hostile native state: the default path must ignore it.
        let hostile = backend(
            crate::read::StackCapability::Unavailable,
            vec![native(
                Some(PrNumber(101)),
                crate::read::StackConsistency::Drifted,
            )],
        );
        let route = route_landing(
            &crate::config::CaravanConfig::default(),
            &hostile,
            &caravan(&[101, 102]),
        )
        .expect("the stable default never fails on native state");
        assert_eq!(route, StackLandingRoute::CaravanOwned);
    }

    #[test]
    fn an_exact_opted_in_available_stack_routes_natively() {
        let route = route_landing(
            &github_config(),
            &backend(
                crate::read::StackCapability::Available,
                vec![native(
                    Some(PrNumber(101)),
                    crate::read::StackConsistency::Exact,
                )],
            ),
            &caravan(&[101, 102]),
        )
        .expect("an exact opted-in Stack routes natively");
        assert_eq!(route, StackLandingRoute::NativeStack { stack_number: 42 });
    }

    #[test]
    fn authoritative_singleton_without_provider_stack_uses_owned_route() {
        let route = route_landing(
            &github_config(),
            &backend(crate::read::StackCapability::Available, Vec::new()),
            &caravan(&[101]),
        )
        .expect("provider cannot represent a one-member Stack");

        assert_eq!(route, StackLandingRoute::SingletonCaravanOwned);
    }

    #[test]
    fn singleton_with_one_exact_stack_keeps_lock_fenced_native_route() {
        let route = route_landing(
            &github_config(),
            &backend(
                crate::read::StackCapability::Available,
                vec![native(
                    Some(PrNumber(101)),
                    crate::read::StackConsistency::Exact,
                )],
            ),
            &caravan(&[101]),
        )
        .expect("one exact mapping remains native");

        assert_eq!(route, StackLandingRoute::NativeStack { stack_number: 42 });
    }

    #[test]
    fn singleton_multiple_and_multi_member_absence_fail_closed_consistently() {
        for (caravan, natives, mappings) in [
            (
                caravan(&[101]),
                vec![
                    native(Some(PrNumber(101)), crate::read::StackConsistency::Exact),
                    native(Some(PrNumber(101)), crate::read::StackConsistency::Exact),
                ],
                2,
            ),
            (caravan(&[101, 102]), Vec::new(), 0),
        ] {
            let error = route_landing(
                &github_config(),
                &backend(crate::read::StackCapability::Available, natives),
                &caravan,
            )
            .expect_err("only singleton+zero is a valid absent mapping");
            assert_eq!(
                mcp_cli::StructuredError::code(&error),
                "github_stack_caravan_mapping_ambiguous"
            );
            let details =
                mcp_cli::StructuredError::details(&error).expect("typed mapping evidence");
            assert_eq!(details["member_count"], caravan.members.len());
            assert_eq!(details["mapping_count"], mappings);
            assert_eq!(details["mutated"], false);
            assert_eq!(details["retryable"], false);
        }
    }

    #[test]
    fn every_native_gate_fails_closed_with_its_exact_code() {
        let exact = vec![native(
            Some(PrNumber(101)),
            crate::read::StackConsistency::Exact,
        )];

        for capability in [
            crate::read::StackCapability::Unavailable,
            crate::read::StackCapability::Unknown,
            crate::read::StackCapability::NotProbed,
        ] {
            let error = route_landing(
                &github_config(),
                &backend(capability, exact.clone()),
                &caravan(&[101, 102]),
            )
            .expect_err("an unproven capability can never route natively");
            assert_eq!(
                mcp_cli::StructuredError::code(&error),
                "github_stack_capability_not_proven"
            );
        }

        let mut unlisted = github_config();
        unlisted.stack_rollout = crate::config::StackRolloutConfig::default();
        let error = route_landing(
            &unlisted,
            &backend(crate::read::StackCapability::Available, exact.clone()),
            &caravan(&[101, 102]),
        )
        .expect_err("a repository without an opt-in can never route natively");
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "github_stack_repository_not_opted_in"
        );

        let mut truncated = backend(crate::read::StackCapability::Available, exact.clone());
        truncated.provider_stacks_truncated = true;
        let error = route_landing(&github_config(), &truncated, &caravan(&[101, 102]))
            .expect_err("a truncated inventory proves nothing about ownership");
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "github_stack_inventory_truncated"
        );

        for natives in [
            Vec::new(),
            vec![
                native(Some(PrNumber(101)), crate::read::StackConsistency::Exact),
                native(Some(PrNumber(101)), crate::read::StackConsistency::Exact),
            ],
        ] {
            let error = route_landing(
                &github_config(),
                &backend(crate::read::StackCapability::Available, natives),
                &caravan(&[101, 102]),
            )
            .expect_err("ambiguous or absent mapping is never resolved by choosing one");
            assert_eq!(
                mcp_cli::StructuredError::code(&error),
                "github_stack_caravan_mapping_ambiguous"
            );
        }

        for consistency in [
            crate::read::StackConsistency::Drifted,
            crate::read::StackConsistency::Orphaned,
            crate::read::StackConsistency::Unknown,
        ] {
            let error = route_landing(
                &github_config(),
                &backend(
                    crate::read::StackCapability::Available,
                    vec![native(Some(PrNumber(101)), consistency)],
                ),
                &caravan(&[101, 102]),
            )
            .expect_err("a non-exact generation can never land");
            assert_eq!(
                mcp_cli::StructuredError::code(&error),
                "github_stack_generation_drifted"
            );
        }
    }
}
