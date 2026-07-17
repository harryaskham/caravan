//! Versioned, bounded hook delivery for canonical Caravan events.
//!
//! Domain operations own event construction. This module only serializes those
//! secret-free facts, executes configured commands, and reports delivery. It
//! deliberately persists no cursor or deduplication authority: repeated ticks
//! may deliver another event and long-lived coordinators must own an external
//! lock or dedupe record.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mcp_cli::{ErrorCategory, StructuredError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::command::{CommandRunError, CommandRunner, CommandSpec, ProcessRunner};
use crate::config::HookConfig;
use crate::model::{
    CaravanEvent, CaravanFleet, EventId, EventKind, OperationId, PrNumber, RepositoryId,
};
use crate::{AppContext, AppError};

/// Event schema accepted by the v1 hook dispatcher.
pub const HOOK_EVENT_VERSION: u32 = 1;
const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Stable outcome of one configured hook delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HookDeliveryState {
    Succeeded,
    Failed,
    TimedOut,
}

/// Bounded, secret-free hook delivery status exposed by CLI/MCP outputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HookDelivery {
    pub event_id: EventId,
    pub kind: EventKind,
    pub state: HookDeliveryState,
    pub blocking: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

impl HookDelivery {
    fn succeeded(
        event: &CaravanEvent,
        hook: &HookConfig,
        output: &crate::command::CommandOutput,
    ) -> Self {
        Self {
            event_id: event.event_id.clone(),
            kind: event.kind,
            state: HookDeliveryState::Succeeded,
            blocking: hook.blocking,
            exit_code: output.code,
            stdout_bytes: output.stdout.len(),
            stderr_bytes: output.stderr.len(),
        }
    }

    fn failed(
        event: &CaravanEvent,
        hook: &HookConfig,
        state: HookDeliveryState,
        exit_code: Option<i32>,
        stdout_bytes: usize,
        stderr_bytes: usize,
    ) -> Self {
        Self {
            event_id: event.event_id.clone(),
            kind: event.kind,
            state,
            blocking: hook.blocking,
            exit_code,
            stdout_bytes,
            stderr_bytes,
        }
    }
}

/// Construct one canonical, secret-free v1 hook event.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn event(
    kind: EventKind,
    operation_id: OperationId,
    repository: RepositoryId,
    caravan_id: Option<PrNumber>,
    prs: Vec<PrNumber>,
    fleet: Option<CaravanFleet>,
    reason: Option<String>,
    metadata: BTreeMap<String, serde_json::Value>,
) -> CaravanEvent {
    CaravanEvent {
        version: HOOK_EVENT_VERSION,
        event_id: EventId::new(),
        operation_id,
        kind,
        repository,
        caravan_id,
        prs,
        fleet,
        reason,
        metadata,
        timestamp: unix_millis().to_string(),
    }
}

/// Attach canonical events to an existing typed domain error so JSON/MCP
/// callers can correlate the exact objects delivered to failure hooks.
#[must_use]
pub fn attach_events(error: AppError, events: &[CaravanEvent]) -> AppError {
    if events.is_empty() {
        return error;
    }
    let mut details = error.details().unwrap_or_else(|| json!({}));
    if let Some(object) = details.as_object_mut() {
        object.insert("events".to_owned(), json!(events));
    } else {
        details = json!({
            "original_details": details,
            "events": events,
        });
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

/// Reuse an operation ID already carried by a domain error, or allocate one
/// for a preflight failure that occurred before domain execution state existed.
#[must_use]
pub fn operation_id_from_error(error: &AppError) -> OperationId {
    let Some(details) = error.details() else {
        return OperationId::new();
    };
    [
        details.get("operation_id"),
        details
            .get("operation_receipt")
            .and_then(|receipt| receipt.get("operation_id")),
        details
            .get("decision")
            .and_then(|decision| decision.get("operation_id")),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| serde_json::from_value::<OperationId>(value.clone()).ok())
    .unwrap_or_else(OperationId::new)
}

/// Attach best-effort hook status to an existing typed domain error.
#[must_use]
pub fn attach_deliveries(error: AppError, deliveries: &[HookDelivery]) -> AppError {
    if deliveries.is_empty() {
        return error;
    }
    let mut details = error.details().unwrap_or_else(|| json!({}));
    if let Some(object) = details.as_object_mut() {
        object.insert("hook_deliveries".to_owned(), json!(deliveries));
    } else {
        details = json!({
            "original_details": details,
            "hook_deliveries": deliveries,
        });
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

/// Recover canonical events embedded in a typed domain error.
///
/// Sync decisions place the same event objects in decision evidence; preserving
/// those IDs lets hooks deduplicate the failed tick rather than receiving a
/// newly invented wrapper event.
#[must_use]
pub fn events_from_error(error: &AppError) -> Vec<CaravanEvent> {
    let Some(details) = error.details() else {
        return Vec::new();
    };
    let mut values = Vec::new();
    let decision_evidence = details
        .get("decision")
        .and_then(|decision| decision.get("evidence"));
    if let Some(event) = decision_evidence.and_then(|evidence| evidence.get("event")) {
        values.push(event.clone());
    }
    if let Some(events) = details.get("events").and_then(serde_json::Value::as_array) {
        values.extend(events.iter().cloned());
    }
    if let Some(events) = decision_evidence
        .and_then(|evidence| evidence.get("events"))
        .and_then(serde_json::Value::as_array)
    {
        values.extend(events.iter().cloned());
    }
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .filter_map(|value| serde_json::from_value::<CaravanEvent>(value).ok())
        .filter(|event| seen.insert(event.event_id.clone()))
        .collect()
}

/// Deliver every configured event in order.
///
/// Unconfigured events are omitted. A best-effort failure is returned as a
/// delivery status and later events continue. A blocking failure stops delivery
/// with typed `hook_failure` evidence; the domain mutation that emitted the
/// event is never rolled back.
pub fn dispatch_events(
    context: &AppContext,
    events: &[CaravanEvent],
) -> Result<Vec<HookDelivery>, AppError> {
    let mut deliveries = Vec::new();
    let mut journaled = BTreeSet::new();
    for event in events {
        if journaled.insert(event.event_id.clone()) {
            crate::journal::append_event(context, event)?;
        }
        let Some(hook) = context.config.hook(event.kind) else {
            continue;
        };
        let delivery = dispatch_event(&context.repository_path, hook, event)?;
        crate::journal::append_delivery(context, event, &delivery)?;
        let failed = delivery.state != HookDeliveryState::Succeeded;
        deliveries.push(delivery.clone());
        if failed && hook.blocking {
            return Err(hook_failure(event, &delivery, &deliveries));
        }
    }
    Ok(deliveries)
}

fn dispatch_event(
    repository: &Path,
    hook: &HookConfig,
    event: &CaravanEvent,
) -> Result<HookDelivery, AppError> {
    validate_event(event)?;
    let payload = serde_json::to_string(event).map_err(|error| {
        AppError::structured(
            ErrorCategory::SerializationError,
            "hook_event_encode_failed",
            format!("could not encode hook event: {error}"),
            Some(json!({ "event_id": event.event_id })),
        )
    })?;
    if payload.len() > MAX_EVENT_BYTES {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "hook_event_too_large",
            "hook event exceeds the bounded one-megabyte delivery limit",
            Some(json!({
                "event_id": event.event_id,
                "bytes": payload.len(),
                "max_bytes": MAX_EVENT_BYTES,
            })),
        ));
    }

    let request = shell_request(&hook.command)
        .env("CARA_EVENT", event_kind_name(event.kind))
        .env("CARA_EVENT_ID", event.event_id.0.clone())
        .env("CARA_OPERATION_ID", event.operation_id.0.clone())
        .env("CARA_REPOSITORY", event.repository.slug())
        .env(
            "CARA_CARAVAN_ID",
            event
                .caravan_id
                .map_or_else(String::new, |id| id.to_string()),
        )
        .env(
            "CARA_PRS",
            event
                .prs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
        )
        .stdin(format!("{payload}\n"));
    let runner = ProcessRunner::in_directory(repository)
        .with_timeout(Duration::from_secs(hook.timeout_secs));
    let result = runner.run(&request);
    let delivery = match result {
        Ok(output) if output.is_success() => HookDelivery::succeeded(event, hook, &output),
        Ok(output) => HookDelivery::failed(
            event,
            hook,
            HookDeliveryState::Failed,
            output.code,
            output.stdout.len(),
            output.stderr.len(),
        ),
        Err(CommandRunError::Timeout { stdout, stderr, .. }) => HookDelivery::failed(
            event,
            hook,
            HookDeliveryState::TimedOut,
            None,
            stdout.len(),
            stderr.len(),
        ),
        Err(_) => HookDelivery::failed(event, hook, HookDeliveryState::Failed, None, 0, 0),
    };
    Ok(delivery)
}

fn validate_event(event: &CaravanEvent) -> Result<(), AppError> {
    if event.version != HOOK_EVENT_VERSION {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "unsupported_hook_event_version",
            format!(
                "hook event version {} is unsupported; expected {}",
                event.version, HOOK_EVENT_VERSION
            ),
            Some(json!({
                "event_id": event.event_id,
                "found": event.version,
                "supported": HOOK_EVENT_VERSION,
            })),
        ));
    }
    Ok(())
}

fn hook_failure(
    event: &CaravanEvent,
    delivery: &HookDelivery,
    deliveries: &[HookDelivery],
) -> AppError {
    let category = if delivery.state == HookDeliveryState::TimedOut {
        ErrorCategory::Timeout
    } else {
        ErrorCategory::ExecutionFailure
    };
    AppError::structured(
        category,
        "hook_failure",
        format!("blocking {:?} hook delivery failed", event.kind),
        Some(json!({
            "event": event,
            "delivery": delivery,
            "deliveries": deliveries,
            "resumable": true,
            "next": "repair the configured blocking hook, then rerun the same Caravan command",
            "rollback": "remote mutations already recorded by the event are not rolled back",
        })),
    )
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(unix)]
fn shell_request(command: &str) -> CommandSpec {
    CommandSpec::new("sh").args(["-c", command])
}

#[cfg(windows)]
fn shell_request(command: &str) -> CommandSpec {
    CommandSpec::new("cmd").args(["/C", command])
}

fn event_kind_name(kind: EventKind) -> String {
    serde_json::to_string(&kind)
        .expect("event kind serializes")
        .trim_matches('"')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use mcp_cli::StructuredError;

    use super::*;
    use crate::config::CaravanConfig;
    use crate::model::{OperationId, PrNumber, RepositoryId};

    fn event(kind: EventKind) -> CaravanEvent {
        CaravanEvent {
            version: HOOK_EVENT_VERSION,
            event_id: EventId::new(),
            operation_id: OperationId::new(),
            kind,
            repository: RepositoryId {
                owner: "harryaskham".to_owned(),
                name: "caravan".to_owned(),
            },
            caravan_id: Some(PrNumber(7)),
            prs: vec![PrNumber(7), PrNumber(8)],
            fleet: None,
            reason: Some("fixture".to_owned()),
            metadata: BTreeMap::new(),
            timestamp: "fixture-time".to_owned(),
        }
    }

    fn context(repository: &Path, hook: HookConfig) -> AppContext {
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository)
            .status()
            .expect("git init runs");
        assert!(status.success());
        let mut config = CaravanConfig::default();
        config.hooks.insert(EventKind::SyncFailed, hook);
        AppContext {
            repository_path: repository.to_path_buf(),
            config_path: repository.join("config.yaml"),
            config_existed: true,
            config,
        }
    }

    #[test]
    fn error_event_extraction_reads_singular_decision_event_with_exact_ids() {
        let event = event(EventKind::CiFailed);
        let error = AppError::structured(
            ErrorCategory::Validation,
            "ci_failure",
            "failed",
            Some(json!({
                "decision": {"evidence": {"event": event.clone()}},
            })),
        );

        let extracted = events_from_error(&error);

        assert_eq!(extracted, vec![event]);
    }

    #[test]
    fn error_event_extraction_deduplicates_singular_and_array_forms() {
        let event = event(EventKind::CiFailed);
        let error = AppError::structured(
            ErrorCategory::Validation,
            "ci_failure",
            "failed",
            Some(json!({
                "events": [event.clone()],
                "decision": {"evidence": {
                    "event": event.clone(),
                    "events": [event.clone()],
                }},
            })),
        );

        let extracted = events_from_error(&error);

        assert_eq!(extracted, vec![event]);
    }

    #[test]
    fn singular_ci_decision_dispatches_ci_failed_once() {
        let repository = tempfile::tempdir().unwrap();
        let event_path = repository.path().join("events.txt");
        let hook = HookConfig {
            command: format!(
                "printf '%s|%s|%s\\n' \"$CARA_EVENT\" \"$CARA_EVENT_ID\" \"$CARA_OPERATION_ID\" >> '{}'",
                event_path.display()
            ),
            timeout_secs: 5,
            blocking: true,
        };
        let mut context = context(repository.path(), hook.clone());
        context.config.hooks.insert(EventKind::CiFailed, hook);
        let event = event(EventKind::CiFailed);
        let error = AppError::structured(
            ErrorCategory::Validation,
            "ci_failure",
            "failed",
            Some(json!({
                "decision": {"evidence": {"event": event.clone()}},
            })),
        );

        let extracted = events_from_error(&error);
        let deliveries = dispatch_events(&context, &extracted).unwrap();

        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].kind, EventKind::CiFailed);
        assert_eq!(deliveries[0].event_id, event.event_id);
        assert_eq!(
            fs::read_to_string(event_path).unwrap(),
            format!("ci_failed|{}|{}\n", event.event_id.0, event.operation_id.0)
        );
    }

    #[test]
    fn failure_event_attachment_and_operation_identity_are_machine_visible() {
        let operation_id = OperationId::new();
        let original = AppError::structured(
            ErrorCategory::Validation,
            "join_rejected",
            "failed",
            Some(json!({ "operation_id": operation_id })),
        );
        let event = event(EventKind::JoinFailed);

        assert_eq!(operation_id_from_error(&original), operation_id);
        let attached = attach_events(original, std::slice::from_ref(&event));
        assert_eq!(
            attached.details().expect("details")["events"][0]["event_id"],
            json!(event.event_id)
        );
    }

    #[test]
    fn configured_hook_receives_versioned_json_and_secret_free_context() {
        let repository = tempfile::tempdir().unwrap();
        let event_path = repository.path().join("event.json");
        let context_path = repository.path().join("context.txt");
        let command = format!(
            "cat > '{}'; printf '%s|%s|%s|%s' \"$CARA_EVENT\" \"$CARA_REPOSITORY\" \"$CARA_CARAVAN_ID\" \"$CARA_PRS\" > '{}'",
            event_path.display(),
            context_path.display(),
        );
        let context = context(
            repository.path(),
            HookConfig {
                command,
                timeout_secs: 5,
                blocking: true,
            },
        );
        let event = event(EventKind::SyncFailed);

        let deliveries = dispatch_events(&context, std::slice::from_ref(&event)).unwrap();

        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].state, HookDeliveryState::Succeeded);
        let received: CaravanEvent =
            serde_json::from_str(&fs::read_to_string(event_path).unwrap()).unwrap();
        assert_eq!(received, event);
        assert_eq!(
            fs::read_to_string(context_path).unwrap(),
            "sync_failed|harryaskham/caravan|7|7,8"
        );
    }

    #[test]
    fn best_effort_failure_is_reported_without_failing_the_operation() {
        let repository = tempfile::tempdir().unwrap();
        let context = context(
            repository.path(),
            HookConfig {
                command: "exit 17".to_owned(),
                timeout_secs: 5,
                blocking: false,
            },
        );

        let deliveries = dispatch_events(&context, &[event(EventKind::SyncFailed)]).unwrap();

        assert_eq!(deliveries[0].state, HookDeliveryState::Failed);
        assert_eq!(deliveries[0].exit_code, Some(17));
    }

    #[test]
    fn blocking_failure_returns_typed_hook_failure_without_output_content() {
        let repository = tempfile::tempdir().unwrap();
        let context = context(
            repository.path(),
            HookConfig {
                command: "printf secret >&2; exit 9".to_owned(),
                timeout_secs: 5,
                blocking: true,
            },
        );

        let error = dispatch_events(&context, &[event(EventKind::SyncFailed)]).unwrap_err();

        assert_eq!(error.code(), "hook_failure");
        let details = error.details().unwrap();
        assert_eq!(details["delivery"]["stderr_bytes"], 6);
        assert!(!details.to_string().contains("secret"));
        assert_eq!(
            details["rollback"],
            "remote mutations already recorded by the event are not rolled back"
        );
    }

    #[test]
    fn external_lock_can_make_repeated_delivery_a_successful_noop() {
        let repository = tempfile::tempdir().unwrap();
        let command =
            "if mkdir coordinator.lock 2>/dev/null; then printf first >> coordinator.log; fi";
        let context = context(
            repository.path(),
            HookConfig {
                command: command.to_owned(),
                timeout_secs: 5,
                blocking: true,
            },
        );
        let event = event(EventKind::SyncFailed);

        let first = dispatch_events(&context, std::slice::from_ref(&event)).unwrap();
        let second = dispatch_events(&context, std::slice::from_ref(&event)).unwrap();

        assert_eq!(first[0].state, HookDeliveryState::Succeeded);
        assert_eq!(second[0].state, HookDeliveryState::Succeeded);
        assert_eq!(
            fs::read_to_string(repository.path().join("coordinator.log")).unwrap(),
            "first"
        );
    }
}
