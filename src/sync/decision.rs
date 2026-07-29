//! Scheduler classification, decision checkout, dedupe, and wake events.

use super::{
    AppContext, AppError, BTreeMap, BTreeSet, CaravanEvent, CiDisposition, CiObservation,
    DecisionKind, DecisionPoint, EventKind, HeadOfLineBlockKind, HeadOfLineStall, Instant,
    MissingRequiredRunsProblem, PrNumber, PullRequestSnapshot, RepositoryId, SchedulerDisposition,
    SchedulerWakeClass, StatusOutput, StructuredError, SyncCaravanGeneration,
    SyncFailureSchedulerStatus, SyncMemberGeneration, SyncSchedulerStatus, Value, hooks, json,
};

pub(super) fn checkout_for_decision(
    context: &AppContext,
    error: AppError,
    operation_deadline: Instant,
) -> AppError {
    let Some(details) = error.details() else {
        return error;
    };
    let Some(decision_value) = details.get("decision") else {
        return error;
    };
    let Ok(decision) = serde_json::from_value::<DecisionPoint>(decision_value.clone()) else {
        return error;
    };
    let target = decision_checkout_target(&decision);
    let Some(target) = target else {
        return error;
    };
    let Some(pull_request) = decision_checkout_pull_request(&decision, target) else {
        return attach_checkout_evidence(
            &error,
            details,
            json!({
                "state": "skipped",
                "pr": target,
                "error": {
                    "category": "validation",
                    "code": "decision_checkout_snapshot_missing",
                    "message": "the decision did not preserve the affected PR snapshot",
                },
                "next": "rediscover status and check out the affected PR before repairing the decision",
            }),
        );
    };
    let checkout = match crate::navigation::checkout_decision_snapshot(
        context,
        &pull_request,
        operation_deadline,
    ) {
        Ok(lock_recovery) => json!({
            "state": "completed",
            "pr": target,
            "branch": pull_request.head.name,
            "oid": pull_request.head.oid,
            "lock_recovery": lock_recovery,
        }),
        Err(checkout_error) => json!({
            "state": "skipped",
            "pr": target,
            "error": {
                "category": checkout_error.category(),
                "code": checkout_error.code(),
                "message": checkout_error.message(),
                "details": checkout_error.details(),
            },
            "next": "make the local worktree safe, then check out the affected PR before repairing the decision",
        }),
    };
    attach_checkout_evidence(&error, details, checkout)
}

fn attach_checkout_evidence(error: &AppError, mut details: Value, checkout: Value) -> AppError {
    if let Some(object) = details.as_object_mut() {
        object.insert("checkout".to_owned(), checkout);
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

fn decision_checkout_pull_request(
    decision: &DecisionPoint,
    target: PrNumber,
) -> Option<PullRequestSnapshot> {
    decision
        .evidence
        .get("pull_request")
        .and_then(|value| serde_json::from_value::<PullRequestSnapshot>(value.clone()).ok())
        .filter(|pull_request| pull_request.number == target)
        .or_else(|| {
            decision
                .evidence
                .get("pull_requests")
                .and_then(|value| {
                    serde_json::from_value::<Vec<PullRequestSnapshot>>(value.clone()).ok()
                })
                .and_then(|pull_requests| {
                    pull_requests
                        .into_iter()
                        .find(|pull_request| pull_request.number == target)
                })
        })
}

pub(super) fn decision_checkout_target(decision: &DecisionPoint) -> Option<PrNumber> {
    match decision.kind {
        DecisionKind::HeadConflict | DecisionKind::CiFailure => {
            decision.affected_prs.first().copied()
        }
        DecisionKind::LinkConflict => decision.affected_prs.last().copied(),
        _ => None,
    }
}

#[allow(clippy::too_many_lines)]
/// Derive the exact members and candidates that block everything behind them.
///
/// Position, not attractiveness, selects the work: a hook that repairs the
/// cheapest blocked member never moves the front of the queue.
fn head_of_line_stalls(
    status: &StatusOutput,
    ci: &[CiObservation],
    missing_required_runs: &[MissingRequiredRunsProblem],
) -> Vec<HeadOfLineStall> {
    let mut stalls = Vec::new();
    let conflicts = status
        .analysis
        .fleet
        .problems
        .iter()
        .filter(|problem| !problem.prs.is_empty())
        .collect::<Vec<_>>();
    let failed_ci = ci
        .iter()
        .filter(|observation| observation.disposition == CiDisposition::Failed)
        .map(|observation| observation.pr)
        .collect::<BTreeSet<_>>();
    let missing_runs = missing_required_runs
        .iter()
        .map(|problem| problem.pr)
        .collect::<BTreeSet<_>>();

    for caravan in &status.analysis.fleet.caravans {
        let blocking = caravan.members.iter().enumerate().find_map(|(index, pr)| {
            let problem = conflicts
                .iter()
                .find(|problem| problem.prs.contains(pr))
                .copied();
            let kind = if missing_runs.contains(pr) {
                Some(HeadOfLineBlockKind::MissingRequiredRuns)
            } else if failed_ci.contains(pr) {
                Some(HeadOfLineBlockKind::CiFailure)
            } else {
                problem.map(|problem| {
                    if problem.kind == crate::model::GraphProblemKind::Incompatible {
                        HeadOfLineBlockKind::Conflict
                    } else {
                        HeadOfLineBlockKind::InvalidGraph
                    }
                })
            };
            kind.map(|kind| {
                (
                    index,
                    *pr,
                    kind,
                    problem.map(|problem| problem.message.clone()),
                )
            })
        });
        let Some((index, blocking_pr, kind, message)) = blocking else {
            continue;
        };
        let blocked_prs = caravan.members[index + 1..].to_vec();
        let evidence = message.unwrap_or_else(|| match kind {
            HeadOfLineBlockKind::CiFailure => {
                format!("PR #{blocking_pr} has terminal or unknown CI on its exact current head")
            }
            HeadOfLineBlockKind::MissingRequiredRuns => format!(
                "PR #{blocking_pr} has required contexts with no reporting run on its exact head"
            ),
            _ => format!("PR #{blocking_pr} cannot proceed"),
        });
        stalls.push(HeadOfLineStall {
            kind,
            caravan_id: Some(caravan.id),
            blocking_pr,
            position: index + 1,
            blocked_prs,
            evidence,
            remedies: member_remedies(kind, blocking_pr),
            fingerprint: stall_fingerprint(
                status,
                Some(caravan.id),
                blocking_pr,
                kind,
                status
                    .analysis
                    .pull_requests
                    .get(&blocking_pr)
                    .map(|pull| pull.head.oid.0.as_str()),
            ),
        });
    }

    // A rejected canonical admission attempt blocks nothing behind it only when
    // it is explicitly excluded from ordering; a blocking rejection is exactly
    // the head-of-line case for unenrolled work.
    if let Some(rejected) = status
        .admission
        .rejected
        .iter()
        .find(|candidate| candidate.blocks_order)
        && status.admission.next_candidate == Some(rejected.pr)
    {
        let blocked_prs = status
            .admission
            .candidates
            .iter()
            .map(|candidate| candidate.pr)
            .collect::<Vec<_>>();
        stalls.push(HeadOfLineStall {
            kind: HeadOfLineBlockKind::AdmissionRejected,
            caravan_id: None,
            blocking_pr: rejected.pr,
            position: 1,
            blocked_prs,
            evidence: rejected.reason.clone(),
            remedies: vec![
                format!(
                    "resolve the exact reported condition on PR #{}; ordering never leapfrogs an eligible attempt",
                    rejected.pr
                ),
                format!(
                    "or make it ineligible on purpose: `cara priority clear --pr {} --actor A --reason R`, close it, or repair its metadata",
                    rejected.pr
                ),
            ],
            fingerprint: stall_fingerprint(
                status,
                None,
                rejected.pr,
                HeadOfLineBlockKind::AdmissionRejected,
                None,
            ),
        });
    }
    stalls
}

fn member_remedies(kind: HeadOfLineBlockKind, pr: PrNumber) -> Vec<String> {
    match kind {
        HeadOfLineBlockKind::Conflict => vec![
            format!("`cara repair start --pr {pr}` and resolve the exact typed conflict"),
            format!("or release the front: `cara evict --pr {pr} --reason <text>`"),
        ],
        HeadOfLineBlockKind::CiFailure => vec![
            format!("inspect the bounded run/job/step evidence for PR #{pr} and repair the source"),
            "or rerun only listed current-generation infrastructure runs with `cara sync --rerun-failed`".to_owned(),
            format!("or release the front: `cara evict --pr {pr} --reason <text>`"),
        ],
        HeadOfLineBlockKind::MissingRequiredRuns => vec![
            format!("trigger a fresh exact-head run for PR #{pr}; waiting alone never resolves it"),
            format!("or release the front: `cara evict --pr {pr} --reason <text>`"),
        ],
        HeadOfLineBlockKind::InvalidGraph => vec![
            "repair the reported structural graph problem before any further mutation".to_owned(),
            format!("or reshape with `cara split --pr {pr}` / `cara evict --pr {pr} --reason <text>`"),
        ],
        HeadOfLineBlockKind::AdmissionRejected => Vec::new(),
    }
}

fn stall_fingerprint(
    status: &StatusOutput,
    caravan_id: Option<PrNumber>,
    blocking_pr: PrNumber,
    kind: HeadOfLineBlockKind,
    head_oid: Option<&str>,
) -> String {
    let material = serde_json::to_vec(&json!({
        "schema_version": 1,
        "repository": status.repository,
        "caravan_id": caravan_id,
        "blocking_pr": blocking_pr,
        "kind": kind,
        "head_oid": head_oid,
    }))
    .expect("head-of-line fingerprint material serializes");
    crate::membership::fnv1a64(&material)
}

#[allow(clippy::too_many_lines)]
pub(super) fn successful_scheduler_status(
    status: &StatusOutput,
    ci: &[CiObservation],
    paused: &[crate::pause::PauseStatus],
    rebase_on_join: bool,
    required_runs: &[crate::required_runs::RequiredRunsReceipt],
    missing_required_runs: &[MissingRequiredRunsProblem],
) -> SyncSchedulerStatus {
    let ci_by_pr = ci
        .iter()
        .map(|observation| (observation.pr, observation.disposition))
        .collect::<BTreeMap<_, _>>();
    let candidates = status
        .merge_candidates
        .iter()
        .cloned()
        .map(|candidate| (candidate.pr, candidate))
        .collect::<BTreeMap<_, _>>();
    let caravans = status
        .analysis
        .fleet
        .caravans
        .iter()
        .map(|caravan| {
            let members = caravan
                .members
                .iter()
                .filter_map(|number| {
                    status
                        .analysis
                        .pull_requests
                        .get(number)
                        .map(|pull_request| SyncMemberGeneration {
                            pr: *number,
                            head: pull_request.head.clone(),
                            base: pull_request.base.clone(),
                            candidate: candidates.get(number).cloned(),
                            ci: ci_by_pr.get(number).copied(),
                        })
                })
                .collect::<Vec<_>>();
            SyncCaravanGeneration {
                caravan_id: caravan.id,
                root: caravan.head().expect("caravans are non-empty"),
                tail: caravan.tail().expect("caravans are non-empty"),
                members,
            }
        })
        .collect::<Vec<_>>();
    let mut waiting_prs = ci
        .iter()
        .filter(|observation| observation.disposition == CiDisposition::Waiting)
        .map(|observation| observation.pr)
        .collect::<Vec<_>>();
    // A member still inside its bounded missing-run grace period is an ordinary
    // CI wait, not a stall, so it belongs with the other waiting members.
    for receipt in required_runs {
        if receipt.assessment.status == crate::required_runs::RequiredRunsStatus::AwaitingGrace
            && !waiting_prs.contains(&receipt.pr)
        {
            waiting_prs.push(receipt.pr);
        }
    }
    waiting_prs.sort_unstable();
    waiting_prs.dedup();
    let held_caravans = paused
        .iter()
        .map(|pause| pause.record.caravan_head)
        .collect::<Vec<_>>();
    let operator_action = missing_required_runs
        .iter()
        .any(|problem| problem.operator_action_required);
    let head_of_line = head_of_line_stalls(status, ci, missing_required_runs);
    let unknown_provider_state = !missing_required_runs.is_empty() && !operator_action;
    let (disposition, wake_class, reason) = if operator_action {
        (
            SchedulerDisposition::OperatorAction,
            SchedulerWakeClass::OperatorAction,
            "one or more caravan members have required contexts with no reporting run on their exact current head; this never resolves by waiting",
        )
    } else if unknown_provider_state {
        (
            SchedulerDisposition::RetryTick,
            SchedulerWakeClass::RetryTick,
            "required-run coverage could not be proven from a partial provider read; rerun the same bounded tick",
        )
    } else if !waiting_prs.is_empty() {
        (
            SchedulerDisposition::WaitingCi,
            SchedulerWakeClass::None,
            "fresh or pending CI is the only incomplete condition; do not wake a repair actor",
        )
    } else if !head_of_line.is_empty() {
        // Head-of-line blocking never clears by ticking again: the exact front
        // member needs repair, reshape, or eviction.
        (
            SchedulerDisposition::ExternalDecision,
            SchedulerWakeClass::ExternalDecision,
            "the front of a queue cannot proceed; repair, reshape, or evict the exact blocking member named in head_of_line",
        )
    } else if !held_caravans.is_empty() {
        (
            SchedulerDisposition::Held,
            SchedulerWakeClass::None,
            "one or more caravans are intentionally held; only explicit resume may release them",
        )
    } else {
        (
            SchedulerDisposition::Healthy,
            SchedulerWakeClass::None,
            "the exact provider graph and selected root-to-tail generations are converged",
        )
    };
    SyncSchedulerStatus {
        schema_version: 1,
        disposition,
        wake_class,
        rebase_on_join,
        default_branch: status.analysis.fleet.default_branch.clone(),
        caravans,
        waiting_prs,
        held_caravans,
        missing_required_runs: missing_required_runs.to_vec(),
        head_of_line,
        reason: reason.to_owned(),
    }
}

pub(super) fn scheduler_failure_status(error: &AppError) -> SyncFailureSchedulerStatus {
    let error_code = error.code();
    let decision = error.details().and_then(|details| {
        serde_json::from_value::<DecisionPoint>(details.get("decision")?.clone()).ok()
    });
    let (disposition, wake_class, retryable) = match decision.map(|item| item.kind) {
        Some(DecisionKind::StalePrecondition) => (
            SchedulerDisposition::RetryTick,
            SchedulerWakeClass::RetryTick,
            true,
        ),
        Some(DecisionKind::UnsafeCheckout | DecisionKind::HookFailure) => (
            SchedulerDisposition::OperatorAction,
            SchedulerWakeClass::OperatorAction,
            false,
        ),
        Some(_) => (
            SchedulerDisposition::ExternalDecision,
            SchedulerWakeClass::ExternalDecision,
            false,
        ),
        None if matches!(
            error_code.as_str(),
            "rebase_conflict"
                | "rebase_nonlinear_range"
                | "rebase_range_ambiguous"
                | "rebase_empty_patch_range"
                | "rebase_target_history_changed"
                | "rebase_repository_not_owned"
                | "rebase_historical_target_mismatch"
                | "rebase_historical_parent_mismatch"
                | "rebase_historical_source_mismatch"
                | "rebase_unsupported_octopus"
                | "rebase_topology_limit"
                | "rebase_external_merge_parents"
                | "rebase_cousin_history"
                | "rebase_merge_tree_conflict"
                | "rebase_merge_replay_conflict"
                | "rebase_merge_tree_mismatch"
                | "rebase_topology_changed"
                | "rebase_midpoint_head_stale"
                | "rebase_midpoint_pr_missing"
                | "rebase_prepared_object_changed"
                | "rebase_result_invalid"
                | "rebase_worker_panicked"
        ) =>
        {
            (
                SchedulerDisposition::ExternalDecision,
                SchedulerWakeClass::ExternalDecision,
                false,
            )
        }
        // The caravan-owned merge actor classifies by its *typed cause*, not by
        // error code, because one code covers both bounded races and states no
        // rerun can resolve. A Caco-managed cron dispatching repair agents must
        // be able to tell "run me again" from "send somebody who can decide".
        None if error_code == "root_merge_refused" => {
            let resumable = error
                .details()
                .and_then(|details| details.get("resumable").and_then(Value::as_bool))
                .unwrap_or(true);
            if resumable {
                (
                    SchedulerDisposition::RetryTick,
                    SchedulerWakeClass::RetryTick,
                    true,
                )
            } else {
                (
                    SchedulerDisposition::ExternalDecision,
                    SchedulerWakeClass::ExternalDecision,
                    false,
                )
            }
        }
        None if matches!(
            error_code.as_str(),
            "default_branch_not_protected"
                | "physical_sync_budget_insufficient"
                | "rebase_ci_trigger_missing"
                | "repository_not_initialized"
                | "squash_merge_not_enabled"
                | "unsafe_checkout"
        ) =>
        {
            (
                SchedulerDisposition::OperatorAction,
                SchedulerWakeClass::OperatorAction,
                false,
            )
        }
        None => (
            SchedulerDisposition::RetryTick,
            SchedulerWakeClass::RetryTick,
            true,
        ),
    };
    SyncFailureSchedulerStatus {
        schema_version: 1,
        disposition,
        wake_class,
        retryable,
        error_code,
    }
}

fn scheduler_decision_fingerprint(error: &AppError) -> String {
    let details = error.details().unwrap_or_else(|| json!({}));
    if error.code() == "ci_failure"
        && let Some(fingerprint) = details
            .pointer("/decision/evidence/force_intent/failure_fingerprint")
            .and_then(serde_json::Value::as_str)
        && fingerprint.starts_with("fnv1a64:")
    {
        return fingerprint.to_owned();
    }
    let material = serde_json::to_vec(&json!({
        "error_code": error.code(),
        "repository": details.get("repository"),
        "caravan_id": details.get("caravan_id"),
        "affected_prs": details.get("affected_prs"),
        "pr": details.get("pr"),
        "merge_oids": details.get("merge_oids"),
        "rebase_plans": details.get("rebase_plans"),
        "decision": details.get("decision"),
    }))
    .expect("scheduler fingerprint material serializes");
    crate::membership::fnv1a64(&material)
}

pub(super) fn attach_scheduler_failure(
    error: &AppError,
    scheduler_status: &SyncFailureSchedulerStatus,
) -> AppError {
    let mut details = error.details().unwrap_or_else(|| json!({}));
    if let Some(object) = details.as_object_mut() {
        object.insert("scheduler_status".to_owned(), json!(scheduler_status));
        if scheduler_status.wake_class == SchedulerWakeClass::ExternalDecision {
            object.insert(
                "decision_fingerprint".to_owned(),
                json!(scheduler_decision_fingerprint(error)),
            );
        }
    } else {
        details = json!({
            "original_details": details,
            "scheduler_status": scheduler_status,
        });
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

pub(super) fn sync_failed_event(error: &AppError) -> Option<CaravanEvent> {
    let details = error.details()?;
    if let Some(decision) = details
        .get("decision")
        .and_then(|value| serde_json::from_value::<DecisionPoint>(value.clone()).ok())
    {
        let fleet = decision
            .evidence
            .get("fleet")
            .and_then(|value| serde_json::from_value(value.clone()).ok());
        return Some(hooks::event(
            EventKind::SyncFailed,
            decision.operation_id,
            decision.repository,
            decision.caravan_id,
            decision.affected_prs,
            fleet,
            Some(decision.message),
            BTreeMap::from([
                ("error_code".to_owned(), json!(error.code())),
                (
                    "scheduler_status".to_owned(),
                    details.get("scheduler_status").cloned().unwrap_or_default(),
                ),
                (
                    "decision_fingerprint".to_owned(),
                    details
                        .get("decision_fingerprint")
                        .cloned()
                        .unwrap_or_default(),
                ),
            ]),
        ));
    }

    let scheduler_status = serde_json::from_value::<SyncFailureSchedulerStatus>(
        details.get("scheduler_status")?.clone(),
    )
    .ok()?;
    if scheduler_status.wake_class != SchedulerWakeClass::ExternalDecision {
        return None;
    }
    let repository =
        serde_json::from_value::<RepositoryId>(details.get("repository")?.clone()).ok()?;
    let mut prs = BTreeSet::new();
    if let Some(pr) = details
        .get("pr")
        .and_then(|value| serde_json::from_value::<PrNumber>(value.clone()).ok())
    {
        prs.insert(pr);
    }
    if let Some(affected) = details
        .get("affected_prs")
        .and_then(|value| serde_json::from_value::<Vec<PrNumber>>(value.clone()).ok())
    {
        prs.extend(affected);
    }
    if let Some(plans) = details.get("rebase_plans").and_then(|value| {
        serde_json::from_value::<Vec<crate::physical_rebase::RebasePlan>>(value.clone()).ok()
    }) {
        prs.extend(plans.into_iter().map(|plan| plan.pr));
    }
    if let Some(receipts) = details.get("rebase_receipts").and_then(|value| {
        serde_json::from_value::<Vec<crate::physical_rebase::RebaseReceipt>>(value.clone()).ok()
    }) {
        prs.extend(receipts.into_iter().map(|receipt| receipt.pr));
    }
    let caravan_id = details
        .get("caravan_id")
        .and_then(|value| serde_json::from_value::<PrNumber>(value.clone()).ok());
    Some(hooks::event(
        EventKind::SyncFailed,
        hooks::operation_id_from_error(error),
        repository,
        caravan_id,
        prs.into_iter().collect(),
        None,
        Some(error.message()),
        BTreeMap::from([
            ("error_code".to_owned(), json!(error.code())),
            ("scheduler_status".to_owned(), json!(scheduler_status)),
            (
                "provider_invariant".to_owned(),
                json!(details.get("rebase_receipts").is_some()),
            ),
            (
                "decision_fingerprint".to_owned(),
                details
                    .get("decision_fingerprint")
                    .cloned()
                    .unwrap_or_default(),
            ),
        ]),
    ))
}
