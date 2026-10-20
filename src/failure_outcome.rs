//! Typed outcomes for queue failures, with process-only refusals flagged
//! (bd-7e0aa1).
//!
//! On 2026-10-20 seven Cacophony completions published and enrolled their PRs
//! correctly and were still refused by a receipt check (`force_intent: none`
//! versus an allowlist of `absent`). Nothing distinguished that refusal from a
//! real failure, so it looked like broken work all day. Every failure Cara
//! journals, and every external error envelope a caller passes in, is
//! classified here into one stable class, with the exact refusing field or
//! step, and with `process_only` set when provider truth shows the PR
//! published, enrolled or merged anyway.
//!
//! Outputs carry only allowlisted identifiers (error codes, field and
//! predicate names, PR numbers, timestamps). Messages, argv, environment,
//! provider bodies and tokens never appear.

use std::collections::BTreeMap;

use clap::Args;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{EventKind, PrNumber};
use crate::{AppContext, AppError};

/// Current wire version of [`FailureOutcome`] and [`FailureSummary`].
pub const FAILURE_OUTCOME_SCHEMA_VERSION: u32 = 1;

/// Journal records scanned for a summary.
const SUMMARY_SCAN_LIMIT: usize = 2_000;
/// Recent examples retained per summary.
const SUMMARY_EXAMPLES: usize = 10;
const MAX_IDENTIFIER_BYTES: usize = 96;

/// Queue surface that produced the failure.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum FailureSurface {
    Sync,
    Join,
    Admission,
    Readiness,
    Publication,
    Eviction,
    Ci,
    #[default]
    Unknown,
}

impl FailureSurface {
    fn parse(value: &str) -> Self {
        match value {
            "sync" => Self::Sync,
            "join" | "new" | "renew" | "rejoin" | "membership" => Self::Join,
            "admission" | "admit" => Self::Admission,
            "readiness" | "ci_admission_gate" => Self::Readiness,
            "publication" | "reintegration" | "completion" => Self::Publication,
            "eviction" | "evict" => Self::Eviction,
            "ci" => Self::Ci,
            _ => Self::Unknown,
        }
    }
}

/// Stable failure class. `process_refusal` is a receipt, invariant or
/// bookkeeping step refusing, as opposed to the provider, the budget, time, or
/// the source itself.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    ProviderError,
    BudgetRefusal,
    Timeout,
    ConflictOrTestFailure,
    ProcessRefusal,
    Unknown,
}

impl FailureClass {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ProviderError => "provider_error",
            Self::BudgetRefusal => "budget_refusal",
            Self::Timeout => "timeout",
            Self::ConflictOrTestFailure => "conflict_or_test_failure",
            Self::ProcessRefusal => "process_refusal",
            Self::Unknown => "unknown",
        }
    }
}

/// One classified failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FailureOutcome {
    pub schema_version: u32,
    pub surface: FailureSurface,
    pub class: FailureClass,
    /// Exact allowlisted error code.
    pub code: String,
    /// Exact field, predicate or stage that refused, when one is named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusing_step: Option<String>,
    pub retryable: bool,
    /// `true` when provider truth shows the operation's PR published,
    /// enrolled or merged although this refusal fired; `false` when provider
    /// truth shows it did not; absent when unproven.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_only: Option<bool>,
    /// Tracking bug for a known process refusal or known failure mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bug: Option<String>,
    /// A process refusal no known bug covers. File one.
    pub needs_bug: bool,
}

/// Classify one failure from its code and structured details. The message is
/// read only to find allowlisted identifiers; it is never copied.
#[must_use]
pub fn classify(
    surface: FailureSurface,
    code: &str,
    message: &str,
    details: Option<&Value>,
) -> FailureOutcome {
    let code = identifier(code).unwrap_or_else(|| "unknown".to_owned());
    let mut text = String::from(message);
    if let Some(details) = details {
        collect_strings(details, &mut text, 0);
    }
    let lower = text.to_ascii_lowercase();
    let app = details.and_then(|details| find_key(details, "github_app_refusal", 0));
    let app_category = app
        .and_then(|refusal| refusal.get("category"))
        .and_then(Value::as_str);
    let readiness_predicate = after(&lower, "predicate=");
    let receipt_field = after(&lower, "receipt invariant mismatch: ");
    let class = classify_class(&code, &lower, app_category, readiness_predicate.as_deref());
    let refusing_step = receipt_field
        .or(readiness_predicate)
        .or_else(|| {
            app.and_then(|refusal| refusal.get("stage"))
                .and_then(Value::as_str)
                .and_then(identifier)
        })
        .or_else(|| {
            details
                .and_then(|details| details.get("stage"))
                .and_then(Value::as_str)
                .and_then(identifier)
        });
    let retryable = details
        .and_then(|details| find_key(details, "retryable", 0))
        .and_then(Value::as_bool)
        .filter(|_| class != FailureClass::ProcessRefusal)
        .unwrap_or(matches!(
            class,
            FailureClass::BudgetRefusal | FailureClass::Timeout | FailureClass::ProviderError
        ));
    let bug = known_bug(&code, refusing_step.as_deref(), class);
    FailureOutcome {
        schema_version: FAILURE_OUTCOME_SCHEMA_VERSION,
        surface,
        class,
        needs_bug: class == FailureClass::ProcessRefusal && bug.is_none(),
        code,
        refusing_step,
        retryable,
        process_only: None,
        bug,
    }
}

fn classify_class(
    code: &str,
    lower: &str,
    app_category: Option<&str>,
    readiness_predicate: Option<&str>,
) -> FailureClass {
    if let Some(category) = app_category {
        return match category {
            "timeout" | "deadline_exhausted" => FailureClass::Timeout,
            "request_budget" => FailureClass::BudgetRefusal,
            _ => FailureClass::ProviderError,
        };
    }
    let has = |needles: &[&str]| needles.iter().any(|needle| code.contains(needle));
    if has(&["budget", "rate_limit"]) || lower.contains("rate limit") {
        return FailureClass::BudgetRefusal;
    }
    if has(&["timeout", "deadline", "timed_out"]) {
        return FailureClass::Timeout;
    }
    if has(&[
        "receipt",
        "invariant",
        "provenance",
        "dead_letter",
        "rolled_back",
        "materialization",
        "published_unjoined",
        "backend_unhealthy",
        "validation_failed",
        "handoff",
    ]) || matches!(
        readiness_predicate,
        Some("deferred_label_absent" | "member_label")
    ) {
        return FailureClass::ProcessRefusal;
    }
    if has(&[
        "conflict",
        "ci_failed",
        "terminal_red",
        "required_check",
        "incompatible",
        "rebase_required",
        "diverged",
        "source_rewrite_required",
        "test_failed",
    ]) {
        return FailureClass::ConflictOrTestFailure;
    }
    if has(&["github", "provider", "discovery", "graphql", "http"])
        || lower.contains("something went wrong while executing your query")
    {
        return FailureClass::ProviderError;
    }
    FailureClass::Unknown
}

/// Known failure modes and the bug that tracks their cause.
fn known_bug(code: &str, step: Option<&str>, class: FailureClass) -> Option<String> {
    let bug = match (code, step) {
        (_, Some("force_intent")) => "caravan:bd-78d4aa cacophony:bd-5666aa",
        ("receipt_validation_failed" | "cara_receipt_invalid", _) => "cacophony:bd-282421",
        ("membership_envelope_rolled_back" | "cara_materialization_failed", _) => {
            "caravan:bd-420191"
        }
        (_, Some("deferred_label_absent" | "member_label")) => "caravan:bd-06e059",
        (code, _) if code.starts_with("github_stack_recovery_") => "caravan:bd-4d4621",
        (code, _)
            if code.contains("native_stack_rebase_required")
                || code.contains("source_rewrite_required") =>
        {
            "caravan:bd-7ff76d"
        }
        (_, Some(stage))
            if class != FailureClass::ProcessRefusal
                && matches!(stage, "credential_broker" | "repository_probe") =>
        {
            "caravan:bd-096b1e"
        }
        _ => return None,
    };
    Some(bug.to_owned())
}

fn find_key<'a>(value: &'a Value, key: &str, depth: usize) -> Option<&'a Value> {
    if depth > 6 {
        return None;
    }
    match value {
        Value::Object(map) => map.get(key).or_else(|| {
            map.values()
                .find_map(|child| find_key(child, key, depth + 1))
        }),
        Value::Array(items) => items
            .iter()
            .take(64)
            .find_map(|child| find_key(child, key, depth + 1)),
        _ => None,
    }
}

fn collect_strings(value: &Value, into: &mut String, depth: usize) {
    if depth > 6 || into.len() > 64 * 1024 {
        return;
    }
    match value {
        Value::String(text) => {
            into.push('\n');
            into.push_str(text);
        }
        Value::Object(map) => map
            .values()
            .for_each(|child| collect_strings(child, into, depth + 1)),
        Value::Array(items) => items
            .iter()
            .take(64)
            .for_each(|child| collect_strings(child, into, depth + 1)),
        _ => {}
    }
}

/// The allowlisted identifier immediately after `marker`, if any.
fn after(text: &str, marker: &str) -> Option<String> {
    let start = text.find(marker)? + marker.len();
    identifier(&text[start..])
}

/// Longest leading `[a-z0-9_.-]` run, bounded; never arbitrary text.
fn identifier(text: &str) -> Option<String> {
    let value: String = text
        .chars()
        .take_while(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '_' | '.' | '-')
        })
        .take(MAX_IDENTIFIER_BYTES)
        .collect();
    (!value.is_empty()).then_some(value)
}

/// Mark `process_only` from a provider-truth membership verdict.
#[must_use]
pub fn with_provider_truth(
    mut outcome: FailureOutcome,
    proof: &crate::membership_proof::MembershipProofOutput,
) -> FailureOutcome {
    if outcome.class == FailureClass::ProcessRefusal {
        outcome.process_only = Some(proof.succeeded);
    }
    outcome
}

/// Classify an external JSON error envelope: Cara's `{status:error,error:{..}}`,
/// a Caco reintegration/dead-letter record, or any object with `code`.
#[must_use]
pub fn classify_envelope(surface: FailureSurface, envelope: &Value) -> FailureOutcome {
    let error = envelope.get("error").filter(|value| value.is_object());
    let source = error.unwrap_or(envelope);
    let code = source
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message = source
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let surface = if surface == FailureSurface::Unknown {
        envelope
            .get("surface")
            .and_then(Value::as_str)
            .map_or(FailureSurface::Unknown, FailureSurface::parse)
    } else {
        surface
    };
    classify(surface, code, message, Some(source))
}

/// Read-only classification request for one external error envelope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Args)]
pub struct FailureClassifyInput {
    /// Path to one JSON error envelope (Cara `--json` error output or a Caco
    /// reintegration/dead-letter record).
    #[arg(long)]
    pub error_file: std::path::PathBuf,
    /// Surface that produced it: sync, join, admission, readiness,
    /// publication, eviction or ci.
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    /// PR whose provider truth decides `process_only` (four light reads).
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    /// Exact head the operation published; required with `--pr`.
    #[arg(long, requires = "pr")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_head: Option<String>,
}

/// Classify one envelope, proving `process_only` from provider truth when a
/// PR is named.
///
/// # Errors
///
/// Unreadable or non-JSON input, or a failed provider proof.
pub fn classify_file(
    context: &AppContext,
    input: &FailureClassifyInput,
) -> Result<FailureOutcome, AppError> {
    let bytes = std::fs::read(&input.error_file).map_err(|error| {
        AppError::validation(
            "failure_classify_input_unreadable",
            format!("could not read --error-file: {}", error.kind()),
        )
    })?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(AppError::validation(
            "failure_classify_input_too_large",
            "--error-file exceeds 4 MiB",
        ));
    }
    let envelope: Value = serde_json::from_slice(&bytes).map_err(|_| {
        AppError::validation(
            "failure_classify_input_invalid",
            "--error-file is not one JSON value",
        )
    })?;
    let surface = input
        .surface
        .as_deref()
        .map_or(FailureSurface::Unknown, FailureSurface::parse);
    let outcome = classify_envelope(surface, &envelope);
    let Some(pr) = input.pr else {
        return Ok(outcome);
    };
    if outcome.class != FailureClass::ProcessRefusal {
        return Ok(outcome);
    }
    let proof = crate::membership_proof::prove(
        context,
        &crate::membership_proof::MembershipProofInput {
            pr,
            expected_head: input.expected_head.clone(),
            expected_base_ref: None,
        },
    )?;
    Ok(with_provider_truth(outcome, &proof))
}

/// One recent journalled failure, without any message text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FailureExample {
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caravan_id: Option<PrNumber>,
    #[serde(default)]
    pub prs: Vec<PrNumber>,
    pub outcome: FailureOutcome,
}

/// Counts and recent examples of journalled failures for one repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FailureSummary {
    pub schema_version: u32,
    pub scanned_records: usize,
    pub failures: usize,
    /// Count per class code.
    pub by_class: BTreeMap<String, u64>,
    /// Count per `surface/class/code[/refusing_step]`.
    pub by_signature: BTreeMap<String, u64>,
    /// Process refusals whose PR provider truth shows enrolled or merged.
    pub process_only: u64,
    /// Process refusals that no known bug covers.
    pub needs_bug: u64,
    /// Newest first, bounded.
    pub recent: Vec<FailureExample>,
}

fn surface_for(kind: EventKind) -> Option<FailureSurface> {
    match kind {
        EventKind::SyncFailed => Some(FailureSurface::Sync),
        EventKind::JoinFailed => Some(FailureSurface::Join),
        EventKind::AdmissionSkipped => Some(FailureSurface::Admission),
        EventKind::EvictionFailed => Some(FailureSurface::Eviction),
        EventKind::CiFailed => Some(FailureSurface::Ci),
        _ => None,
    }
}

/// Summarize journalled failures. `enrolled_or_merged` answers provider truth
/// for one PR from already-read status facts (`None` when unknown).
#[must_use]
pub fn summarize(
    records: &[crate::journal::JournalRecord],
    enrolled_or_merged: &dyn Fn(PrNumber) -> Option<bool>,
) -> FailureSummary {
    let mut summary = FailureSummary {
        schema_version: FAILURE_OUTCOME_SCHEMA_VERSION,
        scanned_records: records.len(),
        ..FailureSummary::default()
    };
    for record in records.iter().rev() {
        let crate::journal::JournalRecord::Event { event, .. } = record else {
            continue;
        };
        let Some(surface) = surface_for(event.kind) else {
            continue;
        };
        let code = event
            .metadata
            .get("error_code")
            .and_then(Value::as_str)
            .or_else(|| event.metadata.get("code").and_then(Value::as_str))
            .unwrap_or(match event.kind {
                EventKind::CiFailed => "ci_failed",
                _ => "unknown",
            });
        let details = Value::Object(
            event
                .metadata
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        );
        let mut outcome = classify(
            surface,
            code,
            event.reason.as_deref().unwrap_or_default(),
            Some(&details),
        );
        if outcome.class == FailureClass::ProcessRefusal {
            let verdicts = event
                .prs
                .iter()
                .filter_map(|pr| enrolled_or_merged(*pr))
                .collect::<Vec<_>>();
            if !verdicts.is_empty() {
                outcome.process_only = Some(verdicts.iter().any(|verdict| *verdict));
            }
        }
        summary.failures += 1;
        *summary
            .by_class
            .entry(outcome.class.code().to_owned())
            .or_default() += 1;
        let signature = match &outcome.refusing_step {
            Some(step) => format!(
                "{}/{}/{}/{step}",
                serde_json::to_value(outcome.surface)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                outcome.class.code(),
                outcome.code
            ),
            None => format!(
                "{}/{}/{}",
                serde_json::to_value(outcome.surface)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                outcome.class.code(),
                outcome.code
            ),
        };
        *summary.by_signature.entry(signature).or_default() += 1;
        if outcome.process_only == Some(true) {
            summary.process_only += 1;
        }
        if outcome.needs_bug {
            summary.needs_bug += 1;
        }
        if summary.recent.len() < SUMMARY_EXAMPLES {
            summary.recent.push(FailureExample {
                timestamp: event.timestamp.clone(),
                caravan_id: event.caravan_id,
                prs: event.prs.clone(),
                outcome,
            });
        }
    }
    summary
}

/// Journal-backed summary using already-read status facts for provider truth.
/// Zero provider cost. Returns `None` when the journal cannot be read.
#[must_use]
pub fn journal_summary(
    context: &AppContext,
    status: Option<&crate::read::StatusOutput>,
) -> Option<FailureSummary> {
    let snapshot = crate::journal::snapshot(
        context,
        &crate::journal::LogInput {
            limit: SUMMARY_SCAN_LIMIT,
            ..crate::journal::LogInput::default()
        },
    )
    .ok()?;
    let truth = |pr: PrNumber| -> Option<bool> {
        let status = status?;
        let snapshot = status.analysis.pull_requests.get(&pr)?;
        Some(match snapshot.state {
            crate::model::PullRequestState::Merged => true,
            crate::model::PullRequestState::Closed => false,
            crate::model::PullRequestState::Open => {
                snapshot.has_label("caravan") && status.analysis.fleet.containing(pr).is_some()
            }
        })
    };
    Some(summarize(&snapshot.records, &truth))
}

/// Read-only journal failure summary request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Args)]
pub struct FailuresInput {}

/// Journal failure summary without status facts (zero provider cost).
///
/// # Errors
///
/// Journal read failures.
pub fn failures(context: &AppContext, _input: &FailuresInput) -> Result<FailureSummary, AppError> {
    let snapshot = crate::journal::snapshot(
        context,
        &crate::journal::LogInput {
            limit: SUMMARY_SCAN_LIMIT,
            ..crate::journal::LogInput::default()
        },
    )?;
    Ok(summarize(&snapshot.records, &|_| None))
}

#[cfg(test)]
mod tests;
