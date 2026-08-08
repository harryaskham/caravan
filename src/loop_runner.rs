//! Foreground `cara loop` driver over canonical `sync --all` ticks.
//!
//! The loop owns no queue cursor: each iteration rediscovers GitHub through the
//! regular sync implementation. A failed tick is bounded evidence, not a stop
//! condition: configured hooks consume its canonical event and the driver keeps
//! ticking so provider races, moved default branches, and external decisions
//! converge without operator restarts. Only an explicit signal ends the loop.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::hooks::{self, HookDelivery};
use crate::model::{CaravanEvent, EventKind};
use crate::sync::SyncOutput;
use crate::{AppContext, AppError, LoopInput, SyncInput};

const SLEEP_SLICE: Duration = Duration::from_millis(100);

/// One complete sync-all tick plus hook delivery status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoopTickOutput {
    pub sync: SyncOutput,
    #[serde(default)]
    pub events: Vec<CaravanEvent>,
    #[serde(default)]
    pub hook_deliveries: Vec<HookDelivery>,
}

/// Bounded evidence for one failed foreground tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoopTickFailure {
    pub tick: u64,
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_class: Option<String>,
    pub retryable: bool,
    #[serde(default)]
    pub hook_deliveries: Vec<HookDelivery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_error: Option<String>,
    pub next: String,
}

/// Bounded summary returned when `--once` completes or a signal stops the loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoopOutput {
    pub ticks: u64,
    #[serde(default)]
    pub failed_ticks: u64,
    #[serde(default)]
    pub consecutive_failures: u64,
    pub stopped_by_signal: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tick: Option<LoopTickOutput>,
    /// Most recent bounded failures, oldest first.
    #[serde(default)]
    pub recent_failures: Vec<LoopTickFailure>,
}

const MAX_RETAINED_FAILURES: usize = 20;

/// Run one tick or a signal-aware foreground loop.
///
/// `observe` is invoked after each successful tick and `observe_failure` after
/// each failed tick so the CLI can stream human progress without making the
/// unbounded process an MCP tool. The unbounded loop never exits on a domain
/// failure; it records bounded evidence, dispatches hooks, and ticks again.
pub fn run(
    context: &AppContext,
    input: &LoopInput,
    mut observe: impl FnMut(&LoopTickOutput),
    mut observe_failure: impl FnMut(&AppError, &LoopTickFailure),
) -> Result<LoopOutput, AppError> {
    let interval_secs = input
        .interval_secs
        .unwrap_or(context.config.loop_config.interval_secs);
    if interval_secs == 0 {
        return Err(AppError::validation(
            "invalid_loop_interval",
            "loop interval must be at least one second",
        ));
    }

    if input.once {
        let output = tick(context)?;
        observe(&output);
        return Ok(LoopOutput {
            ticks: 1,
            failed_ticks: 0,
            consecutive_failures: 0,
            stopped_by_signal: false,
            last_tick: Some(output),
            recent_failures: Vec::new(),
        });
    }

    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = Arc::clone(&stop);
    ctrlc::set_handler(move || signal_stop.store(true, Ordering::SeqCst)).map_err(|error| {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "loop_signal_handler_failed",
            format!("could not install loop signal handler: {error}"),
            None,
        )
    })?;
    drive(
        &stop,
        Duration::from_secs(interval_secs),
        || tick(context),
        |output| observe(output),
        |error, failure| observe_failure(error, failure),
        |error| dispatch_failure_hooks(context, error),
    )
}

/// Deliver canonical events attached to a failed tick without stopping the loop.
fn dispatch_failure_hooks(
    context: &AppContext,
    error: &AppError,
) -> Result<Vec<HookDelivery>, AppError> {
    let events = hooks::events_from_error(error);
    if events.is_empty() {
        return Ok(Vec::new());
    }
    let prepared = crate::sync_authority::prepare(context)?;
    hooks::dispatch_events(prepared.context(), &events)
}

/// Run one canonical sync-all tick including ordinary hook delivery.
pub fn tick(context: &AppContext) -> Result<LoopTickOutput, AppError> {
    let prepared = crate::sync_authority::prepare(context)?;
    let context = prepared.context();
    match crate::sync::sync_prepared(
        context,
        &SyncInput {
            all: true,
            rerun_failed: false,
            dry_run: false,
        },
        prepared.authority(),
    ) {
        Ok(sync) => {
            let mut events = sync.events.clone();
            let ready_events = ready_unqueued_events(&sync);
            let mut hook_deliveries = sync.hook_deliveries.clone();
            // Repository-relative hooks run from the same fetched source
            // snapshot whose policy authorized this tick, never the caller's
            // arbitrary branch.
            hook_deliveries.extend(hooks::dispatch_events(context, &ready_events)?);
            events.extend(ready_events);
            Ok(LoopTickOutput {
                sync,
                events,
                hook_deliveries,
            })
        }
        Err(error) => Err(error),
    }
}

/// Drive bounded ticks until an explicit stop signal.
///
/// The signature keeps `Result` so future fatal driver conditions stay typed,
/// while ordinary domain failures are recorded and retried rather than
/// returned.
#[allow(clippy::unnecessary_wraps)]
fn drive(
    stop: &AtomicBool,
    interval: Duration,
    mut tick: impl FnMut() -> Result<LoopTickOutput, AppError>,
    mut observe: impl FnMut(&LoopTickOutput),
    mut observe_failure: impl FnMut(&AppError, &LoopTickFailure),
    mut deliver_failure_hooks: impl FnMut(&AppError) -> Result<Vec<HookDelivery>, AppError>,
) -> Result<LoopOutput, AppError> {
    let mut ticks = 0_u64;
    let mut failed_ticks = 0_u64;
    let mut consecutive_failures = 0_u64;
    let mut last_tick = None;
    let mut recent_failures: Vec<LoopTickFailure> = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        let started = Instant::now();
        ticks += 1;
        match tick() {
            Ok(output) => {
                consecutive_failures = 0;
                observe(&output);
                last_tick = Some(output);
            }
            Err(error) => {
                failed_ticks += 1;
                consecutive_failures += 1;
                let (hook_deliveries, hook_error) = match deliver_failure_hooks(&error) {
                    Ok(deliveries) => (deliveries, None),
                    Err(hook_error) => (Vec::new(), Some(hook_error.to_string())),
                };
                let failure = tick_failure(ticks, &error, hook_deliveries, hook_error);
                observe_failure(&error, &failure);
                if recent_failures.len() == MAX_RETAINED_FAILURES {
                    recent_failures.remove(0);
                }
                recent_failures.push(failure);
            }
        }
        let deadline = started + interval;
        while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            thread::sleep(SLEEP_SLICE.min(remaining));
        }
    }
    Ok(LoopOutput {
        ticks,
        failed_ticks,
        consecutive_failures,
        stopped_by_signal: true,
        last_tick,
        recent_failures,
    })
}

fn tick_failure(
    tick: u64,
    error: &AppError,
    hook_deliveries: Vec<HookDelivery>,
    hook_error: Option<String>,
) -> LoopTickFailure {
    use mcp_cli::StructuredError as _;
    let scheduler = error
        .details()
        .and_then(|details| details.get("scheduler_status").cloned());
    let field = |name: &str| {
        scheduler
            .as_ref()
            .and_then(|status| status.get(name))
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    };
    let retryable = scheduler
        .as_ref()
        .and_then(|status| status.get("retryable"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let next = if retryable {
        "provider generation race; the next tick rediscovers exact facts and retries".to_owned()
    } else {
        "external decision or operator action is required; the loop keeps ticking and converges once it is resolved"
            .to_owned()
    };
    LoopTickFailure {
        tick,
        code: error.code(),
        message: error.message(),
        disposition: field("disposition"),
        wake_class: field("wake_class"),
        retryable,
        hook_deliveries,
        hook_error,
        next,
    }
}

fn ready_unqueued_events(output: &SyncOutput) -> Vec<CaravanEvent> {
    let mut events = Vec::new();
    for (position, candidate) in output.status.admission.candidates.iter().enumerate() {
        let mut metadata = BTreeMap::new();
        metadata.insert("admission_position".to_owned(), serde_json::json!(position));
        metadata.insert(
            "requires_membership_preflight".to_owned(),
            serde_json::json!(true),
        );
        metadata.insert(
            "rejection_policy".to_owned(),
            serde_json::json!(if output.auto_admission.enabled {
                "sync-owned greedy admission may persist an exact generation-bound skip before considering a later candidate"
            } else {
                "fail closed without automatic leapfrogging"
            }),
        );
        metadata.insert(
            "auto_admission".to_owned(),
            serde_json::json!({
                "enabled": output.auto_admission.enabled,
                "heuristic_version": output.auto_admission.heuristic_version,
                "continuation": output.auto_admission.continuation,
            }),
        );
        metadata.insert(
            "admission_reason".to_owned(),
            serde_json::json!(candidate.reason),
        );
        metadata.insert(
            "priority_label".to_owned(),
            serde_json::json!(candidate.priority_label),
        );
        metadata.insert(
            "ordered_candidates".to_owned(),
            serde_json::json!(
                output
                    .status
                    .admission
                    .candidates
                    .iter()
                    .map(|candidate| candidate.pr)
                    .collect::<Vec<_>>()
            ),
        );
        events.push(hooks::event(
            EventKind::ReadyPrUnqueued,
            output.receipt.operation_id.clone(),
            output.status.repository.clone(),
            None,
            vec![candidate.pr],
            Some(output.status.analysis.fleet.clone()),
            Some(candidate.reason.clone()),
            metadata,
        ));
    }
    events
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::model::{CaravanFleet, CommitOid, OperationId, OperationReceipt, RepositoryId};
    use crate::read::StatusOutput;

    #[allow(clippy::too_many_lines)]
    fn tick_output() -> LoopTickOutput {
        let repository = RepositoryId {
            owner: "harryaskham".to_owned(),
            name: "caravan".to_owned(),
        };
        LoopTickOutput {
            sync: SyncOutput {
                tick: crate::sync::SyncTickReceipt {
                    schema_version: 1,
                    verb: "sync".to_owned(),
                    caravans: 0,
                    unqueued: 0,
                    synchronized: 0,
                    joins: 0,
                    changed: false,
                },
                receipt: OperationReceipt {
                    operation_id: OperationId::new(),
                    operation: "sync".to_owned(),
                    completed_steps: Vec::new(),
                    changed: false,
                },
                auto_admission: crate::sync::AutoAdmissionOutput::default(),
                scheduler_status: crate::sync::SyncSchedulerStatus {
                    schema_version: 1,
                    disposition: crate::sync::SchedulerDisposition::Healthy,
                    wake_class: crate::sync::SchedulerWakeClass::None,
                    rebase_on_join: false,
                    default_branch: crate::model::BranchSnapshot {
                        repository: repository.clone(),
                        name: "main".to_owned(),
                        oid: CommitOid("a".repeat(40)),
                    },
                    caravans: Vec::new(),
                    waiting_prs: Vec::new(),
                    held_caravans: Vec::new(),
                    missing_required_runs: Vec::new(),
                    head_of_line: Vec::new(),
                    reason: "test fixture converged".to_owned(),
                },
                timing: None,
                lock_recovery: None,
                provider_receipts: Vec::new(),
                closed_lifecycle_transitions: Vec::new(),
                root_auto_merge: Vec::new(),
                root_promotion: Vec::new(),
                root_merge: Vec::new(),
                native_stack_land: Vec::new(),
                required_runs: Vec::new(),
                rebase_plans: Vec::new(),
                rebase_receipts: Vec::new(),
                historical_predecessor: None,
                synchronized_caravans: Vec::new(),
                head_advancements: Vec::new(),
                ci: Vec::new(),
                events: Vec::new(),
                hook_deliveries: Vec::new(),
                status: StatusOutput {
                    config_provenance: None,
                    head_merge: crate::read::HeadMergeStatus::default(),
                    runtime: crate::read::RuntimeProvenance::default(),
                    provider_api: crate::model::GitHubApiTelemetry::default(),
                    merge_candidates: Vec::new(),
                    merge_candidates_truncated: 0,
                    previous_default_oid: None,
                    default_branch_movements: Vec::new(),
                    timing: None,
                    repository: repository.clone(),
                    rebase_on_join: crate::read::RebaseOnJoinStatus::default(),
                    stack_backend: crate::read::StackBackendStatus::default(),
                    auto_admission: crate::read::AutoAdmissionStatus::default(),
                    default_branch: "main".to_owned(),
                    current_branch: None,
                    current_pr: None,
                    healthy: true,
                    initialization: crate::initialization::InitializationStatus::default(),
                    admission: crate::read::AdmissionStatus {
                        policy: "priority then FIFO".to_owned(),
                        priority_labels: Vec::new(),
                        generation_integrity: crate::generation::GenerationIntegrityStatus::default(
                        ),
                        candidates: Vec::new(),
                        skipped: Vec::new(),
                        rejected: Vec::new(),
                        next_candidate: None,
                    },
                    analysis: crate::graph::GraphAnalysis {
                        fleet: CaravanFleet {
                            repository: repository.clone(),
                            default_branch: crate::model::BranchSnapshot {
                                repository,
                                name: "main".to_owned(),
                                oid: CommitOid("a".repeat(40)),
                            },
                            caravans: Vec::new(),
                            unqueued: Vec::new(),
                            problems: Vec::new(),
                            history: crate::model::CaravanHistory::default(),
                        },
                        pull_requests: BTreeMap::new(),
                        compatibility: Vec::new(),
                        cumulative_trees: Vec::new(),
                        squash_reconciliations: Vec::new(),
                    },
                    pauses: Vec::new(),
                    sync_budget: crate::sync::SyncBudgetStatus::default(),
                },
                paused_caravans: Vec::new(),
            },
            events: Vec::new(),
            hook_deliveries: Vec::new(),
        }
    }

    #[test]
    fn driver_stops_cleanly_after_signal_flag() {
        let stop = AtomicBool::new(false);
        let mut calls = 0_u8;
        let output = drive(
            &stop,
            Duration::from_millis(1),
            || {
                calls += 1;
                if calls == 2 {
                    stop.store(true, Ordering::SeqCst);
                }
                Ok(tick_output())
            },
            |_| {},
            |_, _| {},
            |_| Ok(Vec::new()),
        )
        .unwrap();

        assert_eq!(output.ticks, 2);
        assert_eq!(output.failed_ticks, 0);
        assert!(output.stopped_by_signal);
    }

    #[test]
    fn retryable_and_decision_failures_keep_ticking_with_bounded_evidence() {
        let stop = AtomicBool::new(false);
        let mut calls = 0_u8;
        let mut hook_calls = 0_u8;
        let mut observed = Vec::new();
        let output = drive(
            &stop,
            Duration::from_millis(1),
            || {
                calls += 1;
                match calls {
                    1 => Err(AppError::structured(
                        ErrorCategory::Validation,
                        "rebase_stale_lease",
                        "remote branch moved since discovery",
                        Some(serde_json::json!({
                            "scheduler_status": {
                                "disposition": "retry_tick",
                                "wake_class": "retry_tick",
                                "retryable": true,
                            }
                        })),
                    )),
                    2 => Err(AppError::structured(
                        ErrorCategory::Validation,
                        "ci_failure",
                        "PR #7 has unresolved CI failure",
                        Some(serde_json::json!({
                            "scheduler_status": {
                                "disposition": "external_decision",
                                "wake_class": "external_decision",
                                "retryable": false,
                            }
                        })),
                    )),
                    _ => {
                        stop.store(true, Ordering::SeqCst);
                        Ok(tick_output())
                    }
                }
            },
            |_| {},
            |_, failure| observed.push(failure.clone()),
            |_| {
                hook_calls += 1;
                Ok(Vec::new())
            },
        )
        .expect("domain failures never end the foreground loop");

        assert_eq!(output.ticks, 3);
        assert_eq!(output.failed_ticks, 2);
        assert_eq!(output.consecutive_failures, 0);
        assert_eq!(hook_calls, 2);
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0].code, "rebase_stale_lease");
        assert!(observed[0].retryable);
        assert_eq!(observed[0].wake_class.as_deref(), Some("retry_tick"));
        assert_eq!(observed[1].code, "ci_failure");
        assert!(!observed[1].retryable);
        assert!(output.last_tick.is_some());
        assert_eq!(output.recent_failures.len(), 2);
    }

    #[test]
    fn retained_failure_evidence_stays_bounded() {
        let stop = AtomicBool::new(false);
        let mut calls = 0_u32;
        let output = drive(
            &stop,
            Duration::from_millis(1),
            || {
                calls += 1;
                if calls > 40 {
                    stop.store(true, Ordering::SeqCst);
                }
                Err::<LoopTickOutput, _>(AppError::validation("stale_precondition", "race"))
            },
            |_| {},
            |_, _| {},
            |_| Ok(Vec::new()),
        )
        .unwrap();

        assert_eq!(output.recent_failures.len(), MAX_RETAINED_FAILURES);
        assert_eq!(output.consecutive_failures, output.failed_ticks);
        assert!(output.last_tick.is_none());
        assert_eq!(
            output.recent_failures.last().unwrap().tick,
            output.ticks,
            "retained evidence keeps the newest failing tick"
        );
    }
}
