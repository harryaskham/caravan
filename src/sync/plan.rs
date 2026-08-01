//! Exact no-provider-write sync and first-admission planning.

use super::{
    AUTO_ADMISSION_HEURISTIC_VERSION, AppContext, AppError, AutoCandidateTarget, Caravan,
    CiDisposition, Duration, ErrorCategory, EventKind, GitHubMutationAdapter, Instant,
    PullRequestPrecondition, PullRequestSnapshot, PullRequestState, StatusOutput,
    SyncApplyAdmissionPlan, SyncAutoAdmissionPlan, SyncInput, SyncPlanAction, SyncPlanActionState,
    SyncPlanDecision, SyncPlanOutput, SyncPlanPhase, SyncProgress, SyncProvider,
    evaluate_auto_candidate, head_is_conflict_free_with_default, json, merged_predecessor,
    mutation_error, preflight_repository, prepare_physical_chains, read,
    selected_unpaused_caravans, sync_operation_budget, validate_graph,
};

const MAX_SYNC_PLAN_ACTIONS: usize = 512;

/// Build an exact, bounded sync plan without invoking any provider mutation.
pub fn plan_sync(context: &AppContext, input: &SyncInput) -> Result<SyncPlanOutput, AppError> {
    // A plan cannot fail a mutation, because it performs none. The machinery is
    // shared with the real tick, so a provider READ failure surfaced under the
    // real tick's name: a TLS timeout on `gh api repos/...` was reported as
    // `github_mutation_failed`, which tells the one reader who most needs the
    // truth that a write was attempted (bd-070cdf).
    plan_sync_inner(context, input).map_err(rename_mutation_failure_for_plan)
}

/// Rename a shared-machinery mutation failure for the no-write plan path.
///
/// Only the mutation name is corrected. Every other failure keeps its own
/// identity, because the point is accuracy rather than suppression.
pub(crate) fn rename_mutation_failure_for_plan(error: AppError) -> AppError {
    if mcp_cli::StructuredError::code(&error) != "github_mutation_failed" {
        return error;
    }
    let details = mcp_cli::StructuredError::details(&error);
    AppError::structured(
        mcp_cli::ErrorCategory::ExecutionFailure,
        "plan_provider_unavailable",
        format!("planning could not reach the provider: {error}"),
        Some(details.unwrap_or_else(|| {
            serde_json::json!({
                "mutated": false,
                "provider_writes": 0,
            })
        })),
    )
}

#[allow(clippy::too_many_lines)]
fn plan_sync_inner(context: &AppContext, input: &SyncInput) -> Result<SyncPlanOutput, AppError> {
    let lock = context.acquire_writer_operation("plan-sync")?;
    let started = Instant::now();
    let operation_deadline = started + sync_operation_budget(context);
    let github_budget =
        crate::command::GithubRequestBudget::new(context.config.sync.max_github_requests_per_tick);
    let status =
        read::status_with_deadline_and_budget(context, operation_deadline, Some(&github_budget))?;
    crate::initialization::require_ready(&status.initialization)?;
    // A plan for a tick that cannot start is worse than no plan: it converts
    // "I checked" into false confidence. Tick bounds moved out of config load so
    // a bad budget could not silence read-only surfaces (bd-a4a7e9), which was
    // right, but the preview was never taught that the check had moved. Reported
    // rather than refused, because refusing would re-break the very surface an
    // operator needs in order to diagnose the bad budget (bd-765c65).
    let tick_refusal = context
        .config
        .validate_tick_bounds()
        .err()
        .map(|error| error.to_string());
    let timeout = Duration::from_secs(context.config.command_timeout_secs);
    let runner = crate::command::ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(timeout)
        .with_operation_deadline(operation_deadline)
        .with_github_request_budget(github_budget.clone());
    crate::navigation::ensure_safe_worktree(
        &context.repository_path,
        &context.config_path,
        &runner,
    )?;
    let provider = GitHubMutationAdapter::new(lock.runner(runner));
    let selected = selected_unpaused_caravans(&status, input.all)?;
    let selected_ids = selected
        .iter()
        .map(|caravan| caravan.id)
        .collect::<Vec<_>>();
    let (physical_rebase_plans, physical_apply_admission, mut progress) = if context
        .config
        .rebase_on_join
    {
        let (prepared, progress, admission) = prepare_physical_chains(
            context,
            &status,
            input.all,
            &provider,
            operation_deadline,
            &lock,
        )?;
        let plans = prepared
            .iter()
            .flat_map(|chain| chain.members.iter().map(|item| item.plan.clone()))
            .collect::<Vec<_>>();
        drop(prepared);
        let (capacity, capacity_defect) =
            super::capacity_evidence(context, sync_operation_budget(context));
        let admission = SyncApplyAdmissionPlan {
            admitted_prefix: admission.admitted_prs.clone(),
            deferred_members: admission.deferred.clone(),
            required_ms: super::duration_millis(admission.budget.required),
            complete_graph_required_ms: super::duration_millis(admission.complete_budget.required),
            configured_deadline_ms: super::duration_millis(sync_operation_budget(context)),
            max_admissible_members: capacity,
            capacity_defect,
            deferred_convergence: admission.deferred_convergence,
        };
        (plans, admission, progress)
    } else {
        let mut progress = SyncProgress::new(
            &status,
            selected_ids.clone(),
            context.config.sync.max_mutations_per_tick,
        );
        progress.required_runs_grace_secs = context.config.sync.missing_required_runs_grace_secs;
        progress.required_runs_retrigger_enabled =
            context.config.sync.retrigger_missing_required_runs;
        progress.paused_caravans = status
            .pauses
            .iter()
            .filter(|pause| {
                pause.state.is_effective()
                    && status
                        .analysis
                        .fleet
                        .caravans
                        .iter()
                        .any(|caravan| caravan.id == pause.record.caravan_head)
            })
            .cloned()
            .collect();
        if !selected.is_empty() {
            preflight_repository(&provider, &status, &progress)?;
            validate_graph(&status, &selected, &progress, context.config.force_merge)?;
        }
        (Vec::new(), SyncApplyAdmissionPlan::default(), progress)
    };

    let mut actions = Vec::new();
    let mut decisions = Vec::new();
    let mut would_emit_events = Vec::new();
    for plan in &physical_rebase_plans {
        let deferred_member = physical_apply_admission.deferred_members.contains(&plan.pr);
        push_plan_action(
            &mut actions,
            SyncPlanAction {
                order: 0,
                phase: SyncPlanPhase::PhysicalPreflight,
                state: if plan.already_satisfied {
                    SyncPlanActionState::AlreadySatisfied
                } else if deferred_member {
                    SyncPlanActionState::DeferredUntilRediscovery
                } else {
                    SyncPlanActionState::WouldMutate
                },
                kind: "rebase_branch".to_owned(),
                pr: Some(plan.pr),
                caravan_id: selected
                    .iter()
                    .find(|caravan| caravan.members.contains(&plan.pr))
                    .map(|caravan| caravan.id),
                expected: status
                    .analysis
                    .pull_requests
                    .get(&plan.pr)
                    .map(PullRequestPrecondition::from),
                target: Some(json!({
                    "branch": planned_base_snapshot(&plan.new_base).name,
                    "oid": planned_base_snapshot(&plan.new_base).oid,
                    "new_head_oid": plan.new_head_oid,
                    "lease": plan.lease,
                })),
                reason: if plan.already_satisfied {
                    "exact cumulative ancestry is already satisfied".to_owned()
                } else if deferred_member {
                    "verified plan intentionally deferred: the configured deadline reserves only a bounded prefix this tick"
                        .to_owned()
                } else {
                    "exact retained generation passed conflict and dry-run lease preflight"
                        .to_owned()
                },
            },
        )?;
    }
    let has_physical_write = physical_rebase_plans
        .iter()
        .any(|plan| !plan.already_satisfied);
    for pause in &status.pauses {
        if pause.state.is_effective() {
            push_plan_action(
                &mut actions,
                SyncPlanAction {
                    order: 0,
                    phase: SyncPlanPhase::ProviderConvergence,
                    state: SyncPlanActionState::AlreadySatisfied,
                    kind: "hold_caravan".to_owned(),
                    pr: Some(pause.record.caravan_head),
                    caravan_id: Some(pause.record.caravan_head),
                    expected: None,
                    target: None,
                    reason: format!("explicit {:?} hold prevents sync mutation", pause.state),
                },
            )?;
        }
    }

    for caravan in &selected {
        plan_caravan_convergence(
            &status,
            &provider,
            caravan,
            input,
            context.config.force_merge,
            has_physical_write,
            &mut progress,
            &mut actions,
            &mut decisions,
            &mut would_emit_events,
        )?;
    }

    let auto_admission = plan_auto_admission(
        context,
        &status,
        input,
        has_physical_write || !decisions.is_empty(),
        operation_deadline,
        &mut actions,
        &mut would_emit_events,
    )?;
    would_emit_events.sort();
    would_emit_events.dedup();
    // MEASURED, not asserted. These two fields are the entire safety claim of a
    // dry-run: every caller reads "NO PROVIDER WRITES" and authorises on it.
    // Hardcoding them meant the plan would report zero even if a shared helper
    // had written, because `prepare_physical_chains` runs against a real
    // `GitHubMutationAdapter` and is the same code the mutating tick uses. A
    // guarantee reported as a literal is a claim; counting it makes a violation
    // visible instead of invisible (bd-216da5).
    let provider_writes = super::completed_mutation_count(&progress);
    let output = SyncPlanOutput {
        schema_version: 1,
        tick_refusal,
        mutated: provider_writes > 0,
        provider_writes,
        local_ephemeral_preflight: context.config.rebase_on_join,
        repository: status.repository.clone(),
        default_branch: status.analysis.fleet.default_branch.clone(),
        all: input.all,
        plan_hash: String::new(),
        selected_caravans: selected_ids,
        physical_rebase_plans,
        physical_apply_admission,
        ci: progress.ci,
        actions,
        auto_admission,
        decisions,
        would_emit_events,
        github_requests_used: github_budget.used(),
        status,
    };
    Ok(output.finalize_hash())
}

fn push_plan_action(
    actions: &mut Vec<SyncPlanAction>,
    mut action: SyncPlanAction,
) -> Result<(), AppError> {
    if actions.len() >= MAX_SYNC_PLAN_ACTIONS {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "sync_plan_action_limit",
            "sync plan exceeded its bounded action limit",
            Some(json!({"limit": MAX_SYNC_PLAN_ACTIONS, "mutated": false})),
        ));
    }
    action.order = u32::try_from(actions.len() + 1).unwrap_or(u32::MAX);
    actions.push(action);
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn plan_caravan_convergence(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    caravan: &Caravan,
    input: &SyncInput,
    force_merge: bool,
    deferred: bool,
    progress: &mut SyncProgress,
    actions: &mut Vec<SyncPlanAction>,
    decisions: &mut Vec<SyncPlanDecision>,
    would_emit_events: &mut Vec<EventKind>,
) -> Result<(), AppError> {
    let head = caravan.head().expect("caravan head");
    let head_snapshot = status
        .analysis
        .pull_requests
        .get(&head)
        .expect("selected head has provider facts");
    let expected = Some(PullRequestPrecondition::from(head_snapshot));
    let base_satisfied = head_snapshot.base.name == status.default_branch;
    push_plan_action(
        actions,
        SyncPlanAction {
            order: 0,
            phase: SyncPlanPhase::ProviderConvergence,
            state: if base_satisfied {
                SyncPlanActionState::AlreadySatisfied
            } else if deferred {
                SyncPlanActionState::DeferredUntilRediscovery
            } else {
                SyncPlanActionState::WouldMutate
            },
            kind: "set_base".to_owned(),
            pr: Some(head),
            caravan_id: Some(caravan.id),
            expected: expected.clone(),
            target: Some(
                json!({"branch": status.default_branch, "oid": status.analysis.fleet.default_branch.oid}),
            ),
            reason: if base_satisfied {
                "promoted caravan root already targets the current default branch".to_owned()
            } else {
                "promoted caravan root is retargeted to the exact current default branch before any merge mechanism".to_owned()
            },
        },
    )?;
    if merged_predecessor(status, caravan).is_some() {
        would_emit_events.push(EventKind::HeadAdvanced);
    }
    if deferred {
        for number in caravan.members.iter().copied() {
            let current = &status.analysis.pull_requests[&number];
            push_plan_action(
                actions,
                SyncPlanAction {
                    order: 0,
                    phase: SyncPlanPhase::Rediscovery,
                    state: SyncPlanActionState::DeferredUntilRediscovery,
                    kind: "observe_ci".to_owned(),
                    pr: Some(number),
                    caravan_id: Some(caravan.id),
                    expected: Some(PullRequestPrecondition::from(current)),
                    target: None,
                    reason: "planned branch rewrite changes the CI generation; fresh checks must be observed after apply"
                        .to_owned(),
                },
            )?;
        }
        for number in caravan.members.iter().skip(1).copied() {
            let current = &status.analysis.pull_requests[&number];
            push_plan_action(
                actions,
                SyncPlanAction {
                    order: 0,
                    phase: SyncPlanPhase::Rediscovery,
                    state: SyncPlanActionState::DeferredUntilRediscovery,
                    kind: "disable_auto_merge".to_owned(),
                    pr: Some(number),
                    caravan_id: Some(caravan.id),
                    expected: Some(PullRequestPrecondition::from(current)),
                    target: Some(json!({"enabled": false})),
                    reason:
                        "revalidate the rewritten generation before repairing non-head auto-merge"
                            .to_owned(),
                },
            )?;
        }
        push_plan_action(
            actions,
            SyncPlanAction {
                order: 0,
                phase: SyncPlanPhase::Rediscovery,
                state: SyncPlanActionState::DeferredUntilRediscovery,
                kind: if progress.head_merge_actor.caravan() {
                    "squash_merge".to_owned()
                } else {
                    "enable_squash_auto_merge".to_owned()
                },
                pr: Some(head),
                caravan_id: Some(caravan.id),
                expected,
                target: if progress.head_merge_actor.caravan() {
                    Some(json!({"merge_method": "squash", "base": status.default_branch}))
                } else {
                    Some(json!({"enabled": true, "merge_method": "squash"}))
                },
                reason: if progress.head_merge_actor.caravan() {
                    "revalidate the rewritten head, its cumulative tree, and fresh CI before cara merges it"
                        .to_owned()
                } else {
                    "revalidate rewritten head CI and provider facts before enabling auto-merge"
                        .to_owned()
                },
            },
        )?;
        return Ok(());
    }

    if head_snapshot.has_label("caravan-force") {
        return plan_forced_head(
            status,
            provider,
            caravan,
            head_snapshot,
            force_merge,
            actions,
            decisions,
            would_emit_events,
        );
    }

    let mut stopped = false;
    for number in caravan.members.iter().copied() {
        let observation = progress.observe_ci(provider, &status.repository, number)?;
        let disposition = observation.disposition;
        progress.ci.push(observation.clone());
        push_plan_action(
            actions,
            SyncPlanAction {
                order: 0,
                phase: SyncPlanPhase::ProviderConvergence,
                state: SyncPlanActionState::ReadOnlyObservation,
                kind: "observe_ci".to_owned(),
                pr: Some(number),
                caravan_id: Some(caravan.id),
                expected: status
                    .analysis
                    .pull_requests
                    .get(&number)
                    .map(PullRequestPrecondition::from),
                target: Some(json!({
                    "disposition": disposition,
                    "rerunnable_run_ids": observation.rerunnable_run_ids,
                })),
                reason: "fresh checks and bounded workflow diagnostics are read without mutation"
                    .to_owned(),
            },
        )?;
        if disposition == CiDisposition::Failed {
            if input.rerun_failed && !observation.rerunnable_run_ids.is_empty() {
                push_plan_action(
                    actions,
                    SyncPlanAction {
                        order: 0,
                        phase: SyncPlanPhase::ProviderConvergence,
                        state: if deferred {
                            SyncPlanActionState::DeferredUntilRediscovery
                        } else {
                            SyncPlanActionState::WouldMutate
                        },
                        kind: "rerun_failed_jobs".to_owned(),
                        pr: Some(number),
                        caravan_id: Some(caravan.id),
                        expected: None,
                        target: Some(json!({"run_ids": observation.rerunnable_run_ids})),
                        reason:
                            "only exact current-generation infrastructure failures are rerunnable"
                                .to_owned(),
                    },
                )?;
            }
            decisions.push(SyncPlanDecision {
                code: "ci_failed".to_owned(),
                pr: Some(number),
                reason: "sync would stop at this exact failed CI generation".to_owned(),
                next: "repair source/test failures or rerun only listed infrastructure runs, then plan again"
                    .to_owned(),
            });
            stopped = true;
            break;
        }
    }
    // Planning must reveal a head whose required contexts never started a run;
    // an operator-action stall that only appears after the write barrier would
    // make the dry run a lie. Recovery itself never happens in a plan.
    for number in caravan.members.iter().copied() {
        let assessment = progress.observe_required_runs(provider, &status.repository, number)?;
        push_plan_action(
            actions,
            SyncPlanAction {
                order: 0,
                phase: SyncPlanPhase::ProviderConvergence,
                state: SyncPlanActionState::ReadOnlyObservation,
                kind: "verify_required_runs".to_owned(),
                pr: Some(number),
                caravan_id: Some(caravan.id),
                expected: status
                    .analysis
                    .pull_requests
                    .get(&number)
                    .map(PullRequestPrecondition::from),
                target: Some(json!({
                    "status": assessment.status,
                    "required_contexts": assessment.required_contexts,
                    "missing_contexts": assessment.missing_contexts,
                    "recovery": assessment.recovery,
                })),
                reason:
                    "required contexts are proven to have reporting run lineage on the exact head"
                        .to_owned(),
            },
        )?;
        if let Some(problem) = crate::required_runs::problem(caravan.id, &assessment, None) {
            decisions.push(SyncPlanDecision {
                code: problem.kind.code().to_owned(),
                pr: Some(number),
                reason: problem.message.clone(),
                next: problem.next.clone(),
            });
        }
    }
    if stopped {
        return Ok(());
    }

    let caravan_merges = progress.head_merge_actor.caravan();
    let disarm_members: Vec<_> = if caravan_merges {
        caravan.members.clone()
    } else {
        caravan.members.iter().skip(1).copied().collect()
    };
    for number in disarm_members {
        let current = &status.analysis.pull_requests[&number];
        push_plan_action(
            actions,
            SyncPlanAction {
                order: 0,
                phase: SyncPlanPhase::ProviderConvergence,
                state: if !current.auto_merge.enabled {
                    SyncPlanActionState::AlreadySatisfied
                } else if deferred {
                    SyncPlanActionState::DeferredUntilRediscovery
                } else {
                    SyncPlanActionState::WouldMutate
                },
                kind: "disable_auto_merge".to_owned(),
                pr: Some(number),
                caravan_id: Some(caravan.id),
                expected: Some(PullRequestPrecondition::from(current)),
                target: Some(json!({"enabled": false})),
                reason: if caravan_merges {
                    "cara is the single merge actor; no caravan member may carry a provider auto-merge request"
                        .to_owned()
                } else {
                    "only the caravan head may have squash auto-merge enabled".to_owned()
                },
            },
        )?;
    }
    if caravan_merges {
        let tree_proof = status
            .analysis
            .cumulative_trees
            .iter()
            .find(|proof| proof.candidate == head_snapshot.head);
        let landable = base_satisfied
            && tree_proof.is_some_and(|proof| proof.identical)
            && head_is_conflict_free_with_default(status, head_snapshot);
        push_plan_action(
            actions,
            SyncPlanAction {
                order: 0,
                phase: SyncPlanPhase::ProviderConvergence,
                state: if deferred || !landable {
                    SyncPlanActionState::DeferredUntilRediscovery
                } else {
                    SyncPlanActionState::WouldMutate
                },
                kind: "squash_merge".to_owned(),
                pr: Some(head),
                caravan_id: Some(caravan.id),
                expected: Some(PullRequestPrecondition::from(head_snapshot)),
                target: Some(json!({
                    "merge_method": "squash",
                    "base": status.default_branch,
                    "admin": false,
                    "cumulative_tree": tree_proof,
                })),
                reason: "cara promotes the root to the exact default branch and performs one non-admin squash merge itself, only while its result tree is exactly the already-validated head tree"
                    .to_owned(),
            },
        )?;
        would_emit_events.push(EventKind::RootMerged);
        return Ok(());
    }
    push_plan_action(
        actions,
        SyncPlanAction {
            order: 0,
            phase: SyncPlanPhase::ProviderConvergence,
            state: if crate::root_auto_merge::squash_armed(head_snapshot) {
                SyncPlanActionState::AlreadySatisfied
            } else if deferred {
                SyncPlanActionState::DeferredUntilRediscovery
            } else {
                SyncPlanActionState::WouldMutate
            },
            kind: "enable_squash_auto_merge".to_owned(),
            pr: Some(head),
            caravan_id: Some(caravan.id),
            expected: Some(PullRequestPrecondition::from(head_snapshot)),
            target: Some(json!({"enabled": true, "merge_method": "squash"})),
            reason: "required root squash auto-merge is scheduler-owned convergent state; apply re-reads the exact current root generation and proves the postcondition on the resulting head"
                .to_owned(),
        },
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn plan_forced_head(
    status: &StatusOutput,
    provider: &impl SyncProvider,
    caravan: &Caravan,
    head_snapshot: &PullRequestSnapshot,
    force_merge: bool,
    actions: &mut Vec<SyncPlanAction>,
    decisions: &mut Vec<SyncPlanDecision>,
    would_emit_events: &mut Vec<EventKind>,
) -> Result<(), AppError> {
    let head = caravan.head().expect("caravan head");
    let mechanically_allowed = force_allowed(status, head_snapshot, force_merge);
    let permission = if mechanically_allowed {
        Some(
            provider
                .viewer_permission(&status.repository)
                .map_err(|error| {
                    mutation_error(
                        &error,
                        &SyncProgress::new(status, vec![caravan.id], u32::MAX),
                        Some(head),
                    )
                })?,
        )
    } else {
        None
    };
    let can_force = mechanically_allowed && permission.as_deref() == Some("ADMIN");
    push_plan_action(
        actions,
        SyncPlanAction {
            order: 0,
            phase: SyncPlanPhase::ProviderConvergence,
            state: if can_force {
                SyncPlanActionState::WouldMutate
            } else {
                SyncPlanActionState::WouldStop
            },
            kind: "force_squash_merge".to_owned(),
            pr: Some(head),
            caravan_id: Some(caravan.id),
            expected: Some(PullRequestPrecondition::from(head_snapshot)),
            target: Some(json!({
                "merge_method": "squash",
                "permission": permission,
                "ci": "bypassed_without_observation",
            })),
            reason: "durable PR-scoped caravan-force intent merges immediately once the PR is a mechanically mergeable caravan root"
                .to_owned(),
        },
    )?;
    if can_force {
        would_emit_events.push(EventKind::ForceMergeAttempted);
        would_emit_events.push(EventKind::ForceMergeCompleted);
    } else {
        decisions.push(SyncPlanDecision {
            code: "force_merge_denied".to_owned(),
            pr: Some(head),
            reason: "force intent lacks configured policy, exact clean compatibility, or ADMIN permission"
                .to_owned(),
            next: "repair the exact policy/permission evidence or explicitly revoke durable force intent, then plan again"
                .to_owned(),
        });
    }
    Ok(())
}

fn force_allowed(status: &StatusOutput, head: &PullRequestSnapshot, force_merge: bool) -> bool {
    force_merge
        && head.state == PullRequestState::Open
        && !head.draft
        && head.has_label("caravan-force")
        && head_is_conflict_free_with_default(status, head)
}

#[allow(clippy::too_many_arguments)]
fn plan_auto_admission(
    context: &AppContext,
    status: &StatusOutput,
    input: &SyncInput,
    requires_rediscovery: bool,
    operation_deadline: Instant,
    actions: &mut Vec<SyncPlanAction>,
    would_emit_events: &mut Vec<EventKind>,
) -> Result<SyncAutoAdmissionPlan, AppError> {
    let checker = crate::graph::GitCompatibilityChecker::new(&context.repository_path, "origin")
        .with_timeout(Duration::from_secs(context.config.command_timeout_secs))
        .with_operation_deadline(operation_deadline);
    plan_auto_admission_with_checker(
        context,
        status,
        input,
        requires_rediscovery,
        actions,
        would_emit_events,
        &checker,
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn plan_auto_admission_with_checker(
    context: &AppContext,
    status: &StatusOutput,
    input: &SyncInput,
    requires_rediscovery: bool,
    actions: &mut Vec<SyncPlanAction>,
    would_emit_events: &mut Vec<EventKind>,
    checker: &impl crate::graph::CompatibilityChecker,
) -> Result<SyncAutoAdmissionPlan, AppError> {
    let enabled = context.config.sync.actions.join_unlabelled_prs;
    let mut output = SyncAutoAdmissionPlan {
        enabled,
        heuristic_version: AUTO_ADMISSION_HEURISTIC_VERSION.to_owned(),
        continuation: if enabled && !input.all {
            "requires_sync_all".to_owned()
        } else if enabled {
            "complete".to_owned()
        } else {
            "disabled".to_owned()
        },
        candidate_pr: None,
        target_tail: None,
        tested_tails: Vec::new(),
        compatibility_reasons: Vec::new(),
    };
    if !enabled || !input.all {
        return Ok(output);
    }
    if requires_rediscovery {
        "replan_after_existing_fleet_convergence".clone_into(&mut output.continuation);
        push_plan_action(
            actions,
            SyncPlanAction {
                order: 0,
                phase: SyncPlanPhase::Rediscovery,
                state: SyncPlanActionState::DeferredUntilRediscovery,
                kind: "rediscover_before_auto_admission".to_owned(),
                pr: None,
                caravan_id: None,
                expected: None,
                target: None,
                reason: "auto-admission target generations are not guessed across earlier planned writes or decisions"
                    .to_owned(),
            },
        )?;
        return Ok(output);
    }
    let Some(candidate_pr) = status.admission.next_candidate else {
        if let Some(rejected) = status.admission.rejected.first()
            && let Some(candidate) = status.analysis.pull_requests.get(&rejected.pr)
        {
            output.candidate_pr = Some(rejected.pr);
            "rejected_canonical_candidate".clone_into(&mut output.continuation);
            output.compatibility_reasons = vec![rejected.reason.clone()];
            push_plan_action(
                actions,
                SyncPlanAction {
                    order: 0,
                    phase: SyncPlanPhase::AutoAdmission,
                    state: SyncPlanActionState::WouldStop,
                    kind: "reject_canonical_candidate".to_owned(),
                    pr: Some(rejected.pr),
                    caravan_id: None,
                    expected: Some(PullRequestPrecondition::from(candidate)),
                    target: None,
                    reason: rejected.reason.clone(),
                },
            )?;
        }
        return Ok(output);
    };
    let Some(candidate) = status.analysis.pull_requests.get(&candidate_pr) else {
        return Err(AppError::validation(
            "sync_plan_candidate_missing",
            format!("canonical candidate #{candidate_pr} disappeared from exact status"),
        ));
    };
    if !status
        .admission
        .candidates
        .iter()
        .any(|candidate| candidate.pr == candidate_pr)
    {
        output.candidate_pr = Some(candidate_pr);
        "rejected_canonical_candidate".clone_into(&mut output.continuation);
        output.compatibility_reasons = status
            .admission
            .rejected
            .iter()
            .find(|candidate| candidate.pr == candidate_pr)
            .map_or_else(Vec::new, |candidate| vec![candidate.reason.clone()]);
        push_plan_action(
            actions,
            SyncPlanAction {
                order: 0,
                phase: SyncPlanPhase::AutoAdmission,
                state: SyncPlanActionState::WouldStop,
                kind: "reject_canonical_candidate".to_owned(),
                pr: Some(candidate_pr),
                caravan_id: None,
                expected: Some(PullRequestPrecondition::from(candidate)),
                target: None,
                reason: output.compatibility_reasons.join(" · "),
            },
        )?;
        return Ok(output);
    }
    let evaluation = evaluate_auto_candidate(status, candidate, checker)?;
    output.candidate_pr = Some(candidate_pr);
    output.tested_tails.clone_from(&evaluation.tested_tails);
    output.compatibility_reasons.clone_from(&evaluation.reasons);
    let (kind, target_tail, reason, events) = match evaluation.target {
        AutoCandidateTarget::New => (
            "auto_admission_new",
            None,
            "canonical candidate would form a new caravan",
            vec![EventKind::CaravanCreated],
        ),
        AutoCandidateTarget::Join(tail) => (
            "auto_admission_join",
            Some(tail),
            "canonical candidate would join the first exact compatible tail",
            vec![EventKind::PrJoined],
        ),
        AutoCandidateTarget::Skip => (
            "persist_auto_admission_skip",
            None,
            "no deterministic compatible target; exact generation-bound skip would be recorded",
            Vec::new(),
        ),
    };
    output.target_tail = target_tail;
    "replan_after_first_admission".clone_into(&mut output.continuation);
    would_emit_events.extend(events);
    push_plan_action(
        actions,
        SyncPlanAction {
            order: 0,
            phase: SyncPlanPhase::AutoAdmission,
            state: SyncPlanActionState::WouldMutate,
            kind: kind.to_owned(),
            pr: Some(candidate_pr),
            caravan_id: target_tail.and_then(|tail| {
                status
                    .analysis
                    .fleet
                    .containing(tail)
                    .map(|caravan| caravan.id)
            }),
            expected: Some(PullRequestPrecondition::from(candidate)),
            target: Some(json!({
                "tail_pr": target_tail,
                "tested_tails": evaluation.tested_tails,
                "compatibility_reasons": evaluation.reasons,
            })),
            reason: reason.to_owned(),
        },
    )?;
    Ok(output)
}

fn planned_base_snapshot(
    base: &crate::physical_rebase::PlannedBase,
) -> &crate::model::BranchSnapshot {
    match base {
        crate::physical_rebase::PlannedBase::Remote(branch)
        | crate::physical_rebase::PlannedBase::Simulated(branch) => branch,
    }
}
