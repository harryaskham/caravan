//! Automatic reformation uses active membership, never the retained row count.
//! Raw history remains an exact provider lease, not a fabricated old checkpoint.

use super::{
    AppError, Caravan, GitHubStackTopology, NativeStackRecoveryProvider, RecoveryFacts,
    exact_retained_mapping, json, refusal,
};
use crate::github::{GitHubStackGeneration, GitHubStackSnapshot};

fn merged_prefix_len(stack: &GitHubStackSnapshot) -> Option<usize> {
    let mut prefix = 0;
    let mut open_seen = false;
    let mut seen = std::collections::BTreeSet::new();
    for row in &stack.pull_requests {
        if !seen.insert(row.number) {
            return None;
        }
        if row.state.eq_ignore_ascii_case("open") && row.merged_at.is_none() && !row.draft {
            open_seen = true;
        } else if !open_seen
            && (row.state.eq_ignore_ascii_case("closed")
                || row.state.eq_ignore_ascii_case("merged"))
            && row.merged_at.is_some()
            && !row.draft
        {
            prefix += 1;
        } else {
            return None;
        }
    }
    (open_seen && stack.open).then_some(prefix)
}

/// Refuse unsupported history before the ordinary native rebase path can turn
/// closure reconciliation into source publication. Lifecycle labels run first.
pub(super) fn require_supported_history(facts: &RecoveryFacts<'_>) -> Result<(), AppError> {
    for native in &facts.backend.native_stacks {
        if native.caravan_id.is_none()
            || !native
                .stack
                .pull_requests
                .iter()
                .any(|row| row.state.eq_ignore_ascii_case("closed") || row.merged_at.is_some())
        {
            continue;
        }
        if merged_prefix_len(&native.stack).is_none()
            || native.ancestry.iter().any(|edge| !edge.linear)
        {
            return Err(refusal(
                "github_stack_closed_history_requires_owner",
                "closed members are not active, but this retained topology cannot use automatic merged-prefix append or source rebase",
                json!({"stack": native.stack, "ancestry": native.ancestry,
                    "mutated": false, "source_heads_unchanged": true,
                    "safe_next_action": "preserve the raw history and resolve the exact unmerged/interleaved or non-linear generation through source-preserving owner recovery; no automatic force/rebase is authorized"}),
            ));
        }
    }
    Ok(())
}

pub(super) fn observe(
    facts: &RecoveryFacts<'_>,
    caravan: &Caravan,
    provider: &impl NativeStackRecoveryProvider,
) -> Result<Option<GitHubStackGeneration>, AppError> {
    let intersecting = facts
        .backend
        .native_stacks
        .iter()
        .filter(|native| {
            native
                .stack
                .pull_requests
                .iter()
                .any(|row| caravan.members.iter().any(|pr| pr.0 == row.number))
        })
        .collect::<Vec<_>>();
    let [native] = intersecting.as_slice() else {
        return Ok(None); // The normal planner diagnoses absence/ambiguity.
    };
    let Some(prefix) = merged_prefix_len(&native.stack).filter(|prefix| *prefix > 0) else {
        return Ok(None);
    };
    let active = &native.stack.pull_requests[prefix..];
    if active.len() >= caravan.members.len()
        || !active
            .iter()
            .map(|row| row.number)
            .eq(caravan.members[..active.len()].iter().map(|pr| pr.0))
    {
        return Ok(None);
    }
    let rows = provider
        .native_stack_generation(facts.repository, native.stack.number)
        .map_err(|error| {
            refusal(
                "github_stack_recovery_rows_unproven",
                &error.to_string(),
                json!({"mutated": false}),
            )
        })?
        .ok_or_else(|| {
            refusal(
                "github_stack_recovery_stack_disappeared",
                "retained Stack disappeared during exact observation",
                json!({"mutated": false}),
            )
        })?;
    if !exact_retained_mapping(native, &rows, &rows.topology) {
        return Err(refusal(
            "github_stack_recovery_inventory_changed",
            "fresh retained rows differ from complete inventory",
            json!({"mutated": false}),
        ));
    }
    if facts.backend.native_stacks.iter().any(|other| {
        other.stack.number != rows.number
            && other.stack.pull_requests.iter().any(|row| {
                rows.topology
                    .entries
                    .iter()
                    .any(|entry| entry.pr.0 == row.number)
            })
    }) {
        return Err(refusal(
            "github_stack_recovery_history_intersection",
            "another Stack intersects retained history",
            json!({"mutated": false}),
        ));
    }
    Ok(Some(rows))
}

pub(super) fn target(
    before: &GitHubStackGeneration,
    accepted: &GitHubStackTopology,
) -> Result<GitHubStackTopology, AppError> {
    before
        .recovery_suffix_target(&accepted.base.repository, accepted)
        .map_err(|error| {
            refusal(
                "github_stack_recovery_suffix_invalid",
                &error.to_string(),
                json!({"mutated": false}),
            )
        })
}
