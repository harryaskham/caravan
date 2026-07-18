//! Foreground `cara loop` driver over canonical `sync --all` ticks.
//!
//! The loop owns no queue cursor: each iteration rediscovers GitHub through the
//! regular sync implementation. Any sync decision/error stops the foreground
//! loop after configured hooks consume its canonical event.

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

/// Bounded summary returned when `--once` completes or a signal stops the loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoopOutput {
    pub ticks: u64,
    pub stopped_by_signal: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tick: Option<LoopTickOutput>,
}

/// Run one tick or a signal-aware foreground loop.
///
/// `observe` is invoked after each successful tick so the CLI can stream human
/// progress without making the unbounded process an MCP tool.
pub fn run(
    context: &AppContext,
    input: &LoopInput,
    mut observe: impl FnMut(&LoopTickOutput),
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
            stopped_by_signal: false,
            last_tick: Some(output),
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
    )
}

fn tick(context: &AppContext) -> Result<LoopTickOutput, AppError> {
    match crate::sync::sync(
        context,
        &SyncInput {
            all: true,
            rerun_failed: false,
        },
    ) {
        Ok(sync) => {
            let mut events = sync.events.clone();
            let ready_events = ready_unqueued_events(&sync);
            let mut hook_deliveries = sync.hook_deliveries.clone();
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

fn drive(
    stop: &AtomicBool,
    interval: Duration,
    mut tick: impl FnMut() -> Result<LoopTickOutput, AppError>,
    mut observe: impl FnMut(&LoopTickOutput),
) -> Result<LoopOutput, AppError> {
    let mut ticks = 0_u64;
    let mut last_tick = None;
    while !stop.load(Ordering::SeqCst) {
        let started = Instant::now();
        let output = tick()?;
        observe(&output);
        ticks += 1;
        last_tick = Some(output);
        let deadline = started + interval;
        while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            thread::sleep(SLEEP_SLICE.min(remaining));
        }
    }
    Ok(LoopOutput {
        ticks,
        stopped_by_signal: true,
        last_tick,
    })
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
            serde_json::json!("fail closed without automatic leapfrogging"),
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

    use mcp_cli::StructuredError;

    use super::*;
    use crate::model::{CaravanFleet, CommitOid, OperationId, OperationReceipt, RepositoryId};
    use crate::read::StatusOutput;

    fn tick_output() -> LoopTickOutput {
        let repository = RepositoryId {
            owner: "harryaskham".to_owned(),
            name: "caravan".to_owned(),
        };
        LoopTickOutput {
            sync: SyncOutput {
                receipt: OperationReceipt {
                    operation_id: OperationId::new(),
                    operation: "sync".to_owned(),
                    completed_steps: Vec::new(),
                    changed: false,
                },
                provider_receipts: Vec::new(),
                synchronized_caravans: Vec::new(),
                head_advancements: Vec::new(),
                ci: Vec::new(),
                events: Vec::new(),
                hook_deliveries: Vec::new(),
                status: StatusOutput {
                    timing: None,
                    repository: repository.clone(),
                    default_branch: "main".to_owned(),
                    current_branch: None,
                    current_pr: None,
                    healthy: true,
                    initialization: crate::initialization::InitializationStatus::default(),
                    admission: crate::read::AdmissionStatus {
                        policy: "priority then FIFO".to_owned(),
                        priority_labels: Vec::new(),
                        candidates: Vec::new(),
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
                        },
                        pull_requests: BTreeMap::new(),
                        compatibility: Vec::new(),
                    },
                },
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
        )
        .unwrap();

        assert_eq!(output.ticks, 2);
        assert!(output.stopped_by_signal);
    }

    #[test]
    fn decision_error_stops_without_another_tick() {
        let stop = AtomicBool::new(false);
        let mut calls = 0_u8;
        let error = drive(
            &stop,
            Duration::from_millis(1),
            || {
                calls += 1;
                Err::<LoopTickOutput, _>(AppError::validation("decision", "stop"))
            },
            |_| {},
        )
        .unwrap_err();

        assert_eq!(error.code(), "decision");
        assert_eq!(calls, 1);
    }
}
