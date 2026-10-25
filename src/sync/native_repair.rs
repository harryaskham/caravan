//! Same-tick native repair when a join becomes visible after discovery.

use super::{
    AppContext, AppError, AutoAdmissionOutput, Instant, StatusOutput, StructuredError, SyncOutput,
    SyncProgress, WriterOperationGuard, dedupe_hook_events, json,
};

pub(super) enum NativeConvergence<T> {
    Converged(T),
    Repaired(Box<crate::native_stack_rebase::SyncDriftRepair>),
}

/// The native prefix pass returns immediately after any write. Its routing
/// refusal therefore proves this pass wrote nothing, even if earlier phases
/// of the tick have receipts. A fresh Stack generation can expose a concurrent
/// join that initial discovery missed. Rediscover once, run the ordinary sealed
/// repair planner, then retry routing once if no repair was needed. Never
/// normalize the mismatched generation or retry an ambiguous/partial write.
pub(super) fn native_prefix_with_rediscovery<T>(
    status: &mut StatusOutput,
    mut converge: impl FnMut(&StatusOutput) -> Result<T, AppError>,
    rediscover: impl FnOnce() -> Result<StatusOutput, AppError>,
    repair: impl FnOnce(
        &StatusOutput,
    ) -> Result<Option<crate::native_stack_rebase::SyncDriftRepair>, AppError>,
) -> Result<NativeConvergence<T>, AppError> {
    match converge(status) {
        Ok(progress) => return Ok(NativeConvergence::Converged(progress)),
        Err(error)
            if error.code() == "github_stack_generation_drifted"
                && error
                    .details()
                    .and_then(|details| details.get("mutated").cloned())
                    == Some(json!(false)) => {}
        Err(error) => return Err(error),
    }
    *status = rediscover()?;
    if let Some(repaired) = repair(status)? {
        return Ok(NativeConvergence::Repaired(Box::new(repaired)));
    }
    converge(status).map(NativeConvergence::Converged)
}

/// Repair newly visible native drift before post-admission convergence, using
/// the same order, policy gates, plan hashes and leases as the initial pass.
pub(super) fn repair_after_join(
    context: &AppContext,
    status: &StatusOutput,
    writer: &WriterOperationGuard,
    deadline: Instant,
    budget: &crate::command::GithubRequestBudget,
) -> Result<Option<crate::native_stack_rebase::SyncDriftRepair>, AppError> {
    for kind in [
        crate::native_stack_rebase::SyncRepairKind::ClosedHistory,
        crate::native_stack_rebase::SyncRepairKind::RoutineDrift,
    ] {
        if let Some(repair) = crate::native_stack_rebase::repair_drift_for_sync(
            context,
            status,
            kind,
            writer,
            deadline,
            Some(budget),
        )? {
            return Ok(Some(repair));
        }
    }
    Ok(None)
}

/// A repair ends the tick, not its earlier admission/parking work. Keep every
/// prior receipt and hook event rather than reporting a misleading repair-only
/// tick after a successful join. Old CI remains historical evidence only; the
/// repair result requires fresh CI and never lands the rewritten generation.
pub(super) fn retain_prior_progress(
    output: &mut SyncOutput,
    mut prior: SyncProgress,
    admission: AutoAdmissionOutput,
) {
    fn prepend<T>(target: &mut Vec<T>, mut before: Vec<T>) {
        before.append(target);
        *target = before;
    }
    prepend(&mut output.receipt.completed_steps, prior.steps);
    prepend(&mut output.provider_receipts, prior.provider_receipts);
    prepend(&mut output.root_auto_merge, prior.root_auto_merge);
    prepend(&mut output.root_promotion, prior.root_promotion);
    prepend(&mut output.root_merge, prior.root_merge);
    prepend(
        &mut output.ci_generation_dispatches,
        prior.ci_generation_dispatches,
    );
    prepend(&mut output.native_stack_land, prior.native_stack_land);
    prepend(&mut output.required_runs, prior.required_runs);
    prepend(&mut output.rebase_plans, prior.rebase_plans);
    prepend(&mut output.rebase_receipts, prior.rebase_receipts);
    prepend(&mut output.paused_caravans, prior.paused_caravans);
    prepend(&mut output.head_advancements, prior.head_advancements);
    prepend(&mut output.ci, prior.ci);
    prepend(&mut output.events, prior.events);
    dedupe_hook_events(&mut output.events);
    output
        .synchronized_caravans
        .append(&mut prior.synchronized_caravans);
    output.synchronized_caravans.sort_unstable();
    output.synchronized_caravans.dedup();
    output.tick.synchronized = output.synchronized_caravans.len();
    output.tick.joins = admission.joins.len();
    output.auto_admission = admission;
}
