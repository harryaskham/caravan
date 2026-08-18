//! Durable, actor-partitioned queue telemetry on an independent orphan branch.
//!
//! The local journal remains Cara's write-ahead source. Publication is additive:
//! callers may report a flush failure, but must never turn it into queue failure.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Args;
use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::command::{CommandRunner, CommandSpec, ProcessRunner};
use crate::journal::JournalRecord;
use crate::{AppContext, AppError};

const SCHEMA_VERSION: u32 = 1;
const ACTOR_DIR: &str = "actors";

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, Args)]
pub struct LogFlushInput {
    /// Stable secret-free writer identity; defaults to `CARA_LOG_ACTOR` or host name.
    #[arg(long)]
    pub actor: Option<String>,
    /// Flush even when the configured interval has not elapsed.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LogFlushOutput {
    pub branch: String,
    pub actor: String,
    pub attempted: bool,
    pub published: bool,
    pub records_seen: usize,
    pub records_added: usize,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_reason: Option<String>,
}

/// Open append format. External watchers may append Actions measurements while
/// Cara remains agnostic about provider-specific collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TelemetryEnvelope {
    pub schema_version: u32,
    pub actor: String,
    pub recorded_unix_ms: u64,
    #[serde(flatten)]
    pub record: TelemetryRecord,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "record_type", rename_all = "snake_case")]
pub enum TelemetryRecord {
    CaraJournal {
        journal: Box<JournalRecord>,
    },
    GithubActions {
        repository: String,
        run_id: u64,
        head_oid: String,
        event: String,
        conclusion: String,
        duration_ms: u64,
        #[serde(default)]
        cancelled_job_ms: u64,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TelemetrySummary {
    pub branch: String,
    pub available: bool,
    pub actors: usize,
    pub records: usize,
    pub native_events: usize,
    pub actions_runs: usize,
    pub actions_cancelled: usize,
    pub actions_wall_ms: u64,
    pub caravan_samples: usize,
    pub median_caravan_members: u64,
    pub max_caravan_members: u64,
    pub admission_refusals: BTreeMap<String, u64>,
    pub completed_queue_prs: usize,
    pub median_queue_ms: u64,
    pub max_queue_ms: u64,
    pub sync_samples: usize,
    pub sync_total_ms: u64,
    pub provider_calls: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_rate_limit_remaining: Option<u64>,
    pub evictions: u64,
    pub parks: u64,
    pub unparks: u64,
}

/// Append one zero-provider-cost sync sample to the local write-ahead journal.
/// Publication remains a separate best-effort sidecar.
pub fn append_sync_sample(context: &AppContext, output: &crate::sync::SyncOutput) {
    let Some(slug) = context.config.repository.as_deref() else {
        return;
    };
    let Some((owner, name)) = slug.split_once('/') else {
        return;
    };
    let mut metadata = BTreeMap::new();
    metadata.insert(
        "tick".to_owned(),
        serde_json::to_value(&output.tick).unwrap_or_default(),
    );
    if let Some(timing) = &output.timing {
        metadata.insert(
            "timing".to_owned(),
            serde_json::to_value(timing).unwrap_or_default(),
        );
    }
    let event = crate::hooks::event(
        crate::model::EventKind::SyncCompleted,
        output.receipt.operation_id.clone(),
        crate::model::RepositoryId {
            owner: owner.to_owned(),
            name: name.to_owned(),
        },
        None,
        Vec::new(),
        None,
        None,
        metadata,
    );
    let _ = crate::journal::append_event(context, &event);
}

#[allow(clippy::too_many_lines)]
pub fn flush(context: &AppContext, input: &LogFlushInput) -> Result<LogFlushOutput, AppError> {
    let config = context.config.log.flush.as_ref().ok_or_else(|| {
        AppError::validation("log_flush_not_configured", "log.flush is not configured")
    })?;
    let actor_source = input.actor.clone().unwrap_or_else(default_actor);
    let actor = sanitize_actor(&actor_source);
    if actor.is_empty() {
        return Err(AppError::validation(
            "log_flush_actor_invalid",
            "log flush actor must contain an ASCII letter or digit",
        ));
    }
    let records = crate::journal::records_for_flush(context)?;
    let runner = ProcessRunner::in_directory(&context.repository_path).with_timeout(
        std::time::Duration::from_secs(context.config.command_timeout_secs),
    );
    let branch_ref = format!("refs/heads/{}", config.branch);
    let max_attempts = config.retries.saturating_add(1);

    for attempt in 1..=max_attempts {
        let old = remote_head(&runner, &branch_ref)?;
        if old.is_some() {
            fetch_branch(&runner, &branch_ref)?;
        }
        if !input.force && !interval_elapsed(&runner, old.as_deref(), config.interval)? {
            return Ok(LogFlushOutput {
                branch: config.branch.clone(),
                actor,
                attempted: false,
                published: false,
                records_seen: records.len(),
                records_added: 0,
                attempts: 0,
                head_before: old.clone(),
                head_after: old,
                skipped_reason: Some("flush interval has not elapsed".to_owned()),
            });
        }
        let path = format!("{ACTOR_DIR}/{actor}.jsonl");
        let existing = old.as_deref().map_or_else(String::new, |oid| {
            git_optional(&runner, ["show", &format!("{oid}:{path}")])
                .ok()
                .flatten()
                .unwrap_or_default()
        });
        let mut known_journals = existing
            .lines()
            .filter_map(|line| serde_json::from_str::<TelemetryEnvelope>(line).ok())
            .filter_map(|envelope| match envelope.record {
                TelemetryRecord::CaraJournal { journal } => {
                    serde_json::to_string(journal.as_ref()).ok()
                }
                TelemetryRecord::GithubActions { .. } => None,
            })
            .collect::<BTreeSet<_>>();
        let now = unix_millis();
        let mut additions = Vec::new();
        for record in &records {
            let line = serde_json::to_string(&TelemetryEnvelope {
                schema_version: SCHEMA_VERSION,
                actor: actor.clone(),
                recorded_unix_ms: now,
                record: TelemetryRecord::CaraJournal {
                    journal: Box::new(record.clone()),
                },
            })
            .map_err(|error| telemetry_error("log_flush_encode_failed", &error.to_string()))?;
            // The recorded time differs between attempts. Deduplicate on the
            // canonical journal payload, not the complete envelope.
            let key = serde_json::to_string(record)
                .map_err(|error| telemetry_error("log_flush_encode_failed", &error.to_string()))?;
            if known_journals.insert(key) {
                additions.push(line);
            }
        }
        if additions.is_empty() {
            return Ok(LogFlushOutput {
                branch: config.branch.clone(),
                actor,
                attempted: true,
                published: false,
                records_seen: records.len(),
                records_added: 0,
                attempts: attempt,
                head_before: old.clone(),
                head_after: old,
                skipped_reason: Some("all local records already published".to_owned()),
            });
        }
        let mut content = existing;
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        for line in &additions {
            content.push_str(line);
            content.push('\n');
        }
        let commit = build_commit(&runner, old.as_deref(), &path, &content, &actor)?;
        if push_with_lease(&runner, &branch_ref, old.as_deref(), &commit)? {
            return Ok(LogFlushOutput {
                branch: config.branch.clone(),
                actor,
                attempted: true,
                published: true,
                records_seen: records.len(),
                records_added: additions.len(),
                attempts: attempt,
                head_before: old,
                head_after: Some(commit),
                skipped_reason: None,
            });
        }
    }
    Err(AppError::structured(
        ErrorCategory::ExecutionFailure,
        "log_flush_push_race_exhausted",
        "log branch changed during every bounded publication attempt",
        Some(json!({"branch": config.branch, "retries": config.retries, "mutated": false})),
    ))
}

#[allow(clippy::too_many_lines)]
pub fn summary(context: &AppContext) -> Result<TelemetrySummary, AppError> {
    let Some(config) = context.config.log.flush.as_ref() else {
        return Ok(TelemetrySummary::default());
    };
    let runner = ProcessRunner::in_directory(&context.repository_path).with_timeout(
        std::time::Duration::from_secs(context.config.command_timeout_secs),
    );
    let branch_ref = format!("refs/heads/{}", config.branch);
    let Some(head) = remote_head(&runner, &branch_ref)? else {
        return Ok(TelemetrySummary {
            branch: config.branch.clone(),
            ..TelemetrySummary::default()
        });
    };
    fetch_branch(&runner, &branch_ref)?;
    let files = git(&runner, ["ls-tree", "-r", "--name-only", &head])?;
    let actor_files = files
        .lines()
        .filter(|path| {
            path.starts_with("actors/")
                && std::path::Path::new(path)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        })
        .collect::<Vec<_>>();
    let mut output = TelemetrySummary {
        branch: config.branch.clone(),
        available: true,
        actors: actor_files.len(),
        ..TelemetrySummary::default()
    };
    let mut sizes = Vec::new();
    let mut joined = BTreeMap::new();
    let mut queue_durations = Vec::new();
    for path in actor_files {
        let contents =
            git_optional(&runner, ["show", &format!("{head}:{path}")])?.unwrap_or_default();
        for line in contents.lines() {
            let Ok(envelope) = serde_json::from_str::<TelemetryEnvelope>(line) else {
                continue;
            };
            output.records += 1;
            match envelope.record {
                TelemetryRecord::GithubActions {
                    conclusion,
                    duration_ms,
                    ..
                } => {
                    output.actions_runs += 1;
                    output.actions_wall_ms = output.actions_wall_ms.saturating_add(duration_ms);
                    if conclusion == "cancelled" {
                        output.actions_cancelled += 1;
                    }
                }
                TelemetryRecord::CaraJournal { journal } => {
                    if let JournalRecord::Event { event, .. } = *journal {
                        output.native_events += 1;
                        if let Some(fleet) = event.fleet {
                            output.caravan_samples += 1;
                            sizes.extend(
                                fleet
                                    .caravans
                                    .into_iter()
                                    .map(|caravan| caravan.members.len() as u64),
                            );
                        }
                        if let Some(code) = event
                            .metadata
                            .get("refusal_code")
                            .and_then(serde_json::Value::as_str)
                        {
                            *output
                                .admission_refusals
                                .entry(code.to_owned())
                                .or_default() += 1;
                        }
                        let timestamp = event.timestamp.parse::<u64>().ok();
                        match event.kind {
                            crate::model::EventKind::PrJoined => {
                                if let Some(timestamp) = timestamp {
                                    for pr in event.prs {
                                        joined.entry(pr).or_insert(timestamp);
                                    }
                                }
                            }
                            crate::model::EventKind::RootMerged => {
                                if let Some(timestamp) = timestamp {
                                    for pr in event.prs {
                                        if let Some(start) = joined.remove(&pr) {
                                            queue_durations.push(timestamp.saturating_sub(start));
                                        }
                                    }
                                }
                            }
                            crate::model::EventKind::Evicted => output.evictions += 1,
                            crate::model::EventKind::CaravanParked => output.parks += 1,
                            crate::model::EventKind::CaravanUnparked => output.unparks += 1,
                            _ => {}
                        }
                        if let Some(total_ms) = metadata_u64(&event.metadata, "timing", "total_ms")
                        {
                            output.sync_samples += 1;
                            output.sync_total_ms = output.sync_total_ms.saturating_add(total_ms);
                        }
                        if let Some(calls) = metadata_u64(&event.metadata, "github_api", "calls") {
                            output.provider_calls = output.provider_calls.saturating_add(calls);
                        }
                        if let Some(remaining) = event
                            .metadata
                            .get("github_api")
                            .and_then(|value| value.get("rate_limit"))
                            .and_then(|value| value.get("remaining"))
                            .and_then(serde_json::Value::as_u64)
                        {
                            output.min_rate_limit_remaining = Some(
                                output
                                    .min_rate_limit_remaining
                                    .map_or(remaining, |current| current.min(remaining)),
                            );
                        }
                    }
                }
            }
        }
    }
    sizes.sort_unstable();
    output.max_caravan_members = sizes.last().copied().unwrap_or(0);
    output.median_caravan_members = sizes
        .get(sizes.len().saturating_sub(1) / 2)
        .copied()
        .unwrap_or(0);
    queue_durations.sort_unstable();
    output.completed_queue_prs = queue_durations.len();
    output.max_queue_ms = queue_durations.last().copied().unwrap_or(0);
    output.median_queue_ms = queue_durations
        .get(queue_durations.len().saturating_sub(1) / 2)
        .copied()
        .unwrap_or(0);
    Ok(output)
}

fn metadata_u64(
    metadata: &BTreeMap<String, serde_json::Value>,
    object: &str,
    field: &str,
) -> Option<u64> {
    metadata
        .get(object)
        .and_then(|value| value.get(field))
        .and_then(serde_json::Value::as_u64)
}

fn build_commit(
    runner: &ProcessRunner,
    parent: Option<&str>,
    path: &str,
    content: &str,
    actor: &str,
) -> Result<String, AppError> {
    let index = temp_index();
    let index_value = index.to_string_lossy().into_owned();
    let base = CommandSpec::new("git")
        .env("GIT_INDEX_FILE", &index_value)
        .env("GIT_AUTHOR_NAME", "Cara Telemetry")
        .env("GIT_AUTHOR_EMAIL", "cara-telemetry@localhost")
        .env("GIT_COMMITTER_NAME", "Cara Telemetry")
        .env("GIT_COMMITTER_EMAIL", "cara-telemetry@localhost");
    run(
        runner,
        base.clone().args(parent.map_or_else(
            || vec!["read-tree".to_owned(), "--empty".to_owned()],
            |oid| vec!["read-tree".to_owned(), oid.to_owned()],
        )),
    )?;
    let blob = run(
        runner,
        base.clone()
            .args(["hash-object", "-w", "--stdin"])
            .stdin(content),
    )?;
    run(
        runner,
        base.clone().args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{},{}", blob.trim(), path),
        ]),
    )?;
    let tree = run(runner, base.clone().arg("write-tree"))?;
    let mut commit = base.args(["commit-tree", tree.trim()]);
    if let Some(parent) = parent {
        commit = commit.args(["-p", parent]);
    }
    commit = commit.args(["-m", &format!("caravan telemetry flush: {actor}")]);
    let result = run(runner, commit)?.trim().to_owned();
    let _ = std::fs::remove_file(index);
    Ok(result)
}

fn push_with_lease(
    runner: &ProcessRunner,
    branch_ref: &str,
    old: Option<&str>,
    commit: &str,
) -> Result<bool, AppError> {
    let lease = format!("--force-with-lease={branch_ref}:{}", old.unwrap_or(""));
    let spec = CommandSpec::new("git")
        .args([
            "push",
            "--porcelain",
            &lease,
            "origin",
            &format!("{commit}:{branch_ref}"),
        ])
        .git_write();
    match runner.run(&spec) {
        Ok(output) if output.code == Some(0) => Ok(true),
        Ok(output)
            if output.stderr.contains("stale info") || output.stderr.contains("rejected") =>
        {
            Ok(false)
        }
        Ok(output) => Err(telemetry_error("log_flush_push_failed", &output.stderr)),
        Err(error) => Err(telemetry_error("log_flush_push_failed", &error.to_string())),
    }
}

fn remote_head(runner: &ProcessRunner, branch_ref: &str) -> Result<Option<String>, AppError> {
    let output = git(runner, ["ls-remote", "--refs", "origin", branch_ref])?;
    Ok(output.split_whitespace().next().map(str::to_owned))
}

fn fetch_branch(runner: &ProcessRunner, branch_ref: &str) -> Result<(), AppError> {
    git(
        runner,
        ["fetch", "--quiet", "--no-tags", "origin", branch_ref],
    )
    .map(|_| ())
}

fn interval_elapsed(
    runner: &ProcessRunner,
    head: Option<&str>,
    interval: u64,
) -> Result<bool, AppError> {
    let Some(head) = head else {
        return Ok(true);
    };
    let timestamp = git(runner, ["show", "-s", "--format=%ct", head])?
        .trim()
        .parse::<u64>()
        .map_err(|error| telemetry_error("log_flush_timestamp_invalid", &error.to_string()))?;
    Ok(unix_secs().saturating_sub(timestamp) >= interval)
}

fn git<const N: usize>(runner: &ProcessRunner, args: [&str; N]) -> Result<String, AppError> {
    run(runner, CommandSpec::new("git").args(args))
}

fn git_optional<const N: usize>(
    runner: &ProcessRunner,
    args: [&str; N],
) -> Result<Option<String>, AppError> {
    match runner.run(&CommandSpec::new("git").args(args)) {
        Ok(output) if output.code == Some(0) => Ok(Some(output.stdout)),
        Ok(_) => Ok(None),
        Err(error) => Err(telemetry_error("log_flush_git_failed", &error.to_string())),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run(runner: &ProcessRunner, spec: CommandSpec) -> Result<String, AppError> {
    match runner.run(&spec) {
        Ok(output) if output.code == Some(0) => Ok(output.stdout),
        Ok(output) => Err(telemetry_error("log_flush_git_failed", &output.stderr)),
        Err(error) => Err(telemetry_error("log_flush_git_failed", &error.to_string())),
    }
}

fn sanitize_actor(actor: &str) -> String {
    actor
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

fn default_actor() -> String {
    std::env::var("CARA_LOG_ACTOR")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "cara".to_owned())
}

fn temp_index() -> PathBuf {
    std::env::temp_dir().join(format!(
        "cara-log-index-{}-{}",
        std::process::id(),
        unix_millis()
    ))
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}
fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn telemetry_error(code: &'static str, message: &str) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        code,
        message,
        Some(json!({"mutated": false})),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;
    use crate::config::{CaravanConfig, LogFlushConfig};
    use crate::model::{CaravanEvent, EventId, EventKind, OperationId, PrNumber, RepositoryId};

    fn git_at(path: &std::path::Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(path)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn flush_creates_actor_partitioned_orphan_branch_and_deduplicates() {
        let repository = TempDir::new().unwrap();
        let remote = TempDir::new().unwrap();
        git_at(remote.path(), &["init", "--bare", "--quiet"]);
        git_at(repository.path(), &["init", "--quiet"]);
        git_at(
            repository.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        let mut config = CaravanConfig::default();
        config.log.flush = Some(LogFlushConfig {
            interval: 1,
            ..LogFlushConfig::default()
        });
        let context = AppContext {
            repository_path: repository.path().to_path_buf(),
            config_path: repository.path().join("config.yaml"),
            config_existed: false,
            config,
        };
        crate::journal::append_event(
            &context,
            &CaravanEvent {
                version: 1,
                event_id: EventId("event-1".to_owned()),
                operation_id: OperationId("operation-1".to_owned()),
                kind: EventKind::HeadAdvanced,
                repository: RepositoryId {
                    owner: "acme".to_owned(),
                    name: "widgets".to_owned(),
                },
                caravan_id: Some(PrNumber(7)),
                prs: vec![PrNumber(7)],
                fleet: None,
                reason: None,
                metadata: BTreeMap::new(),
                timestamp: "1".to_owned(),
            },
        )
        .unwrap();

        let first = flush(
            &context,
            &LogFlushInput {
                actor: Some("host/a".to_owned()),
                force: true,
            },
        )
        .unwrap();
        assert!(first.published);
        assert_eq!(first.records_added, 1);
        let second = flush(
            &context,
            &LogFlushInput {
                actor: Some("host/a".to_owned()),
                force: true,
            },
        )
        .unwrap();
        assert!(!second.published);
        assert_eq!(second.records_added, 0);

        let branch = first.head_after.unwrap();
        let content = Command::new("git")
            .args(["show", &format!("{branch}:actors/host-a.jsonl")])
            .current_dir(repository.path())
            .output()
            .unwrap();
        assert!(content.status.success());
        let lines = String::from_utf8(content.stdout).unwrap();
        assert_eq!(lines.lines().count(), 1);
        let envelope: TelemetryEnvelope = serde_json::from_str(lines.trim()).unwrap();
        assert_eq!(envelope.schema_version, 1);
    }

    #[test]
    fn action_records_are_open_and_secret_free() {
        let envelope = TelemetryEnvelope {
            schema_version: 1,
            actor: "actions-watcher".to_owned(),
            recorded_unix_ms: 42,
            record: TelemetryRecord::GithubActions {
                repository: "acme/widgets".to_owned(),
                run_id: 9,
                head_oid: "abc".to_owned(),
                event: "pull_request".to_owned(),
                conclusion: "cancelled".to_owned(),
                duration_ms: 1_000,
                cancelled_job_ms: 900,
            },
        };
        let encoded = serde_json::to_string(&envelope).unwrap();
        assert!(encoded.contains("github_actions"));
        assert!(!encoded.contains("token"));
    }
}
