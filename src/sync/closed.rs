//! Explicit metadata-only terminal cleanup, independent of native Stack health.

use super::{
    AUTO_ADMISSION_HEURISTIC_VERSION, AppError, CLOSED_LABEL, ClosedLifecycleReconciliation,
    ErrorCategory, PARKED_LABEL, PrNumber, PullRequestPrecondition, StatusOutput,
    SyncApplyAdmissionPlan, SyncAutoAdmissionPlan, SyncInput, SyncPlanAction, SyncPlanActionState,
    SyncPlanOutput, SyncPlanPhase, SyncProvider, json, reconcile_closed_lifecycle,
};

pub(super) fn validate_input(input: &SyncInput, applying: bool) -> Result<(), AppError> {
    let invalid = match input.closed_pr {
        Some(pr) => {
            pr == 0
                || input.all
                || input.rerun_failed
                || (applying && input.expected_closed_head.is_none())
        }
        None => input.expected_closed_head.is_some(),
    };
    if invalid {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "closed_member_input_invalid",
            "use --closed-pr alone; application requires --expected-closed-head from a fresh preview",
            Some(json!({"mutated": false})),
        ));
    }
    Ok(())
}

pub(super) fn selected(status: &StatusOutput, input: &SyncInput) -> Result<StatusOutput, AppError> {
    let number = PrNumber(input.closed_pr.expect("scoped caller"));
    let planned = status.analysis.pull_requests.get(&number).ok_or_else(|| {
        AppError::structured(
            ErrorCategory::Validation,
            "closed_member_not_found",
            "selected PR is absent from the authoritative lifecycle inventory",
            Some(json!({"pr": number, "mutated": false})),
        )
    })?;
    if !planned.is_closed_unmerged()
        || input
            .expected_closed_head
            .as_ref()
            .is_some_and(|head| *head != planned.head.oid.0)
    {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "closed_member_identity_changed",
            "selected PR must remain CLOSED and unmerged at the exact expected head",
            Some(json!({"pr": number, "observed": planned, "mutated": false})),
        ));
    }
    let mut scoped = status.clone();
    scoped.analysis.pull_requests.retain(|pr, _| *pr == number);
    Ok(scoped)
}

pub(super) fn reconcile(
    status: &StatusOutput,
    input: &SyncInput,
    provider: &impl SyncProvider,
) -> Result<ClosedLifecycleReconciliation, AppError> {
    validate_input(input, true)?;
    // The shared transaction refetches this exact snapshot, checks head/base/state/
    // labels again immediately before replacement, and verifies provider readback.
    reconcile_closed_lifecycle(&selected(status, input)?, provider)
}

pub(super) fn plan(
    status: StatusOutput,
    input: &SyncInput,
    requests: u32,
    tick_refusal: Option<String>,
) -> Result<SyncPlanOutput, AppError> {
    validate_input(input, false)?;
    let scoped = selected(&status, input)?;
    let pr = scoped
        .analysis
        .pull_requests
        .values()
        .next()
        .expect("selected row");
    let mut labels = pr.labels.clone();
    labels.remove("caravan");
    labels.remove(PARKED_LABEL);
    labels.insert(CLOSED_LABEL.to_owned());
    let changed = labels != pr.labels;
    Ok(SyncPlanOutput {
        schema_version: 1,
        tick_refusal,
        mutated: false,
        provider_writes: 0,
        local_ephemeral_preflight: false,
        repository: status.repository.clone(),
        default_branch: status.analysis.fleet.default_branch.clone(),
        all: false,
        plan_hash: String::new(),
        selected_caravans: Vec::new(),
        physical_rebase_plans: Vec::new(),
        physical_apply_admission: SyncApplyAdmissionPlan::default(),
        ci: Vec::new(),
        actions: vec![SyncPlanAction {
            order: 1,
            phase: SyncPlanPhase::ProviderConvergence,
            state: if changed {
                SyncPlanActionState::WouldMutate
            } else {
                SyncPlanActionState::AlreadySatisfied
            },
            kind: "reconcile_closed_member_labels".to_owned(),
            pr: Some(pr.number),
            caravan_id: None,
            expected: Some(PullRequestPrecondition::from(pr)),
            target: Some(json!({"labels": labels, "expected_closed_head": pr.head.oid})),
            reason: "metadata-only: preserve every other PR, branch and queue operation".to_owned(),
        }],
        auto_admission: SyncAutoAdmissionPlan {
            enabled: false,
            heuristic_version: AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
            continuation: "explicit closed-member cleanup never admits work".to_owned(),
            fleet_capacity_refusal: None,
            candidate_pr: None,
            target_tail: None,
            tested_tails: Vec::new(),
            compatibility_reasons: Vec::new(),
        },
        decisions: Vec::new(),
        would_emit_events: Vec::new(),
        github_requests_used: requests,
        status,
    }
    .finalize_hash())
}
