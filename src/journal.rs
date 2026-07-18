//! Concurrent, bounded, append-only storage for canonical events and hook receipts.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Args;
use fs2::FileExt;
use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::command::{CommandRunError, CommandRunner, CommandSpec, ProcessRunner};
use crate::config::JournalConfig;
use crate::hooks::HookDelivery;
use crate::model::{CaravanEvent, EventId, EventKind, OperationId, PrNumber};
use crate::{AppContext, AppError};

const JOURNAL_DIR: &str = "caravan";
const JOURNAL_FILE: &str = "events-v1.jsonl";
const LOCK_FILE: &str = "events-v1.lock";
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 10_000;
const MAX_RECORD_BYTES: usize = 1024 * 1024;
const JOURNAL_VERSION: u32 = 1;

/// One durable journal entry. Hook process output is intentionally absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "record_type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum JournalRecord {
    Event {
        version: u32,
        event: CaravanEvent,
    },
    HookDelivery {
        version: u32,
        event_id: EventId,
        operation_id: OperationId,
        kind: EventKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caravan_id: Option<PrNumber>,
        #[serde(default)]
        prs: Vec<PrNumber>,
        timestamp: String,
        delivery: HookDelivery,
    },
}

impl JournalRecord {
    fn kind(&self) -> EventKind {
        match self {
            Self::Event { event, .. } => event.kind,
            Self::HookDelivery { kind, .. } => *kind,
        }
    }

    fn timestamp(&self) -> &str {
        match self {
            Self::Event { event, .. } => &event.timestamp,
            Self::HookDelivery { timestamp, .. } => timestamp,
        }
    }

    fn contains_pr(&self, pr: PrNumber) -> bool {
        match self {
            Self::Event { event, .. } => event.caravan_id == Some(pr) || event.prs.contains(&pr),
            Self::HookDelivery {
                caravan_id, prs, ..
            } => *caravan_id == Some(pr) || prs.contains(&pr),
        }
    }

    fn event_id(&self) -> &EventId {
        match self {
            Self::Event { event, .. } => &event.event_id,
            Self::HookDelivery { event_id, .. } => event_id,
        }
    }
}

/// Bounded filters shared by `cara log` and the MCP `log` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Args)]
pub struct LogInput {
    /// Maximum number of newest matching records returned.
    #[arg(long, default_value_t = DEFAULT_LIMIT)]
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Event kind in `snake_case` form.
    #[arg(long)]
    #[serde(default)]
    pub kind: Option<String>,
    /// Include events concerning this PR (delivery records have no duplicated PR payload).
    #[arg(long)]
    #[serde(default)]
    pub pr: Option<u64>,
    /// Inclusive event timestamp lower bound (Unix milliseconds).
    #[arg(long)]
    #[serde(default)]
    pub since: Option<String>,
    /// Inclusive event timestamp upper bound (Unix milliseconds).
    #[arg(long)]
    #[serde(default)]
    pub until: Option<String>,
}

const fn default_limit() -> usize {
    DEFAULT_LIMIT
}

impl Default for LogInput {
    fn default() -> Self {
        Self {
            limit: DEFAULT_LIMIT,
            kind: None,
            pr: None,
            since: None,
            until: None,
        }
    }
}

/// Deterministically ordered bounded journal snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LogOutput {
    pub records: Vec<JournalRecord>,
    pub limit: usize,
    pub matching_records: usize,
    pub truncated: bool,
}

/// Append an event before any configured hook is started.
pub fn append_event(context: &AppContext, event: &CaravanEvent) -> Result<(), AppError> {
    append(
        &context.repository_path,
        context.config.command_timeout_secs,
        &context.config.journal,
        &JournalRecord::Event {
            version: JOURNAL_VERSION,
            event: event.clone(),
        },
    )
}

/// Append secret-free delivery status after a hook exits.
pub fn append_delivery(
    context: &AppContext,
    event: &CaravanEvent,
    delivery: &HookDelivery,
) -> Result<(), AppError> {
    append(
        &context.repository_path,
        context.config.command_timeout_secs,
        &context.config.journal,
        &JournalRecord::HookDelivery {
            version: JOURNAL_VERSION,
            event_id: event.event_id.clone(),
            operation_id: event.operation_id.clone(),
            kind: event.kind,
            caravan_id: event.caravan_id,
            prs: event.prs.clone(),
            timestamp: unix_millis(),
            delivery: delivery.clone(),
        },
    )
}

/// Read a bounded snapshot. A torn final JSONL record is safely ignored.
pub fn snapshot(context: &AppContext, input: &LogInput) -> Result<LogOutput, AppError> {
    validate_input(input)?;
    let kind = input.kind.as_deref().map(parse_kind).transpose()?;
    let paths = paths(
        &context.repository_path,
        context.config.command_timeout_secs,
    )?;
    fs::create_dir_all(&paths.directory)
        .map_err(|error| io_error("journal_directory_failed", &error))?;
    let lock = open_lock(&paths.lock)?;
    FileExt::lock_shared(&lock).map_err(|error| io_error("journal_lock_failed", &error))?;
    let result = read_all_locked(&paths, context.config.journal.max_archives).map(|records| {
        let filtered: Vec<_> = records
            .into_iter()
            .filter(|record| kind.is_none_or(|kind| record.kind() == kind))
            .filter(|record| input.pr.is_none_or(|pr| record.contains_pr(PrNumber(pr))))
            .filter(|record| {
                input
                    .since
                    .as_deref()
                    .is_none_or(|since| record.timestamp() >= since)
            })
            .filter(|record| {
                input
                    .until
                    .as_deref()
                    .is_none_or(|until| record.timestamp() <= until)
            })
            .collect();
        let matching_records = filtered.len();
        let start = matching_records.saturating_sub(input.limit);
        LogOutput {
            records: filtered.into_iter().skip(start).collect(),
            limit: input.limit,
            matching_records,
            truncated: start > 0,
        }
    });
    let _ = FileExt::unlock(&lock);
    result
}

/// Poll the journal until `stop` is set, first emitting the requested existing tail.
pub fn follow(
    context: &AppContext,
    input: &LogInput,
    stop: &std::sync::atomic::AtomicBool,
    mut observe: impl FnMut(&JournalRecord),
) -> Result<(), AppError> {
    validate_input(input)?;
    let mut poll = input.clone();
    poll.limit = MAX_LIMIT;
    let baseline = snapshot(context, &poll)?.records;
    let tail_start = baseline.len().saturating_sub(input.limit);
    for record in &baseline[tail_start..] {
        observe(record);
    }
    let mut seen = BTreeMap::<String, usize>::new();
    for record in baseline {
        let key = serde_json::to_string(&record).map_err(|error| encode_error(&error))?;
        *seen.entry(key).or_default() += 1;
    }
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(200));
        let records = snapshot(context, &poll)?.records;
        let mut observed = BTreeMap::<String, usize>::new();
        for record in records {
            let key = serde_json::to_string(&record).map_err(|error| encode_error(&error))?;
            let occurrence = observed.entry(key.clone()).or_default();
            *occurrence += 1;
            if *occurrence > seen.get(&key).copied().unwrap_or_default() {
                observe(&record);
            }
        }
        // Replacing rather than accumulating keeps follower memory bounded by
        // the current retained snapshot across arbitrarily many rotations.
        seen = observed;
    }
    Ok(())
}

fn append(
    repository: &Path,
    command_timeout_secs: u64,
    config: &JournalConfig,
    record: &JournalRecord,
) -> Result<(), AppError> {
    let mut payload = serde_json::to_vec(record).map_err(|error| encode_error(&error))?;
    if payload.len() > MAX_RECORD_BYTES {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "journal_record_too_large",
            "journal record exceeds the one-megabyte limit",
            Some(json!({ "bytes": payload.len(), "max_bytes": MAX_RECORD_BYTES })),
        ));
    }
    payload.push(b'\n');
    let paths = paths(repository, command_timeout_secs)?;
    fs::create_dir_all(&paths.directory)
        .map_err(|error| io_error("journal_directory_failed", &error))?;
    let lock = open_lock(&paths.lock)?;
    FileExt::lock_exclusive(&lock).map_err(|error| io_error("journal_lock_failed", &error))?;
    let result = (|| {
        recover_truncated_tail(&paths.active)?;
        if matches!(record, JournalRecord::Event { .. })
            && read_all_locked(&paths, config.max_archives)?
                .iter()
                .any(|existing| {
                    matches!(existing, JournalRecord::Event { .. })
                        && existing.event_id() == record.event_id()
                })
        {
            return Ok(());
        }
        let active_len = fs::metadata(&paths.active).map_or(0, |metadata| metadata.len());
        if active_len > 0 && active_len.saturating_add(payload.len() as u64) > config.max_bytes {
            rotate(&paths, config.max_archives)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.active)
            .map_err(|error| io_error("journal_open_failed", &error))?;
        file.write_all(&payload)
            .and_then(|()| file.sync_data())
            .map_err(|error| io_error("journal_append_failed", &error))
    })();
    let _ = FileExt::unlock(&lock);
    result
}

fn recover_truncated_tail(path: &Path) -> Result<(), AppError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("journal_read_failed", &error)),
    };
    if bytes.is_empty() || bytes.last() == Some(&b'\n') {
        return Ok(());
    }
    let valid_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_len(valid_len as u64))
        .map_err(|error| io_error("journal_truncation_recovery_failed", &error))
}

fn rotate(paths: &JournalPaths, max_archives: u32) -> Result<(), AppError> {
    if max_archives == 0 {
        return fs::remove_file(&paths.active)
            .or_else(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(|error| io_error("journal_rotation_failed", &error));
    }
    let oldest = archive_path(&paths.active, max_archives);
    if oldest.exists() {
        fs::remove_file(&oldest).map_err(|error| io_error("journal_rotation_failed", &error))?;
    }
    for index in (1..max_archives).rev() {
        let from = archive_path(&paths.active, index);
        if from.exists() {
            fs::rename(&from, archive_path(&paths.active, index + 1))
                .map_err(|error| io_error("journal_rotation_failed", &error))?;
        }
    }
    fs::rename(&paths.active, archive_path(&paths.active, 1))
        .map_err(|error| io_error("journal_rotation_failed", &error))
}

fn read_all_locked(
    paths: &JournalPaths,
    max_archives: u32,
) -> Result<Vec<JournalRecord>, AppError> {
    let mut records = Vec::new();
    for index in (1..=max_archives).rev() {
        read_file(&archive_path(&paths.active, index), &mut records)?;
    }
    read_file(&paths.active, &mut records)?;
    Ok(records)
}

fn read_file(path: &Path, records: &mut Vec<JournalRecord>) -> Result<(), AppError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("journal_read_failed", &error)),
    };
    let length = file
        .metadata()
        .map_err(|error| io_error("journal_read_failed", &error))?
        .len();
    let mut reader = BufReader::new(file);
    let mut consumed = 0_u64;
    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|error| io_error("journal_read_failed", &error))?;
        if bytes == 0 {
            break;
        }
        consumed += bytes as u64;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(line.trim_end()) {
            Ok(record) => records.push(record),
            Err(_) if consumed == length && !line.ends_with('\n') => break,
            Err(error) => {
                return Err(AppError::structured(
                    ErrorCategory::SerializationError,
                    "journal_record_invalid",
                    format!(
                        "invalid complete journal record in {}: {error}",
                        path.display()
                    ),
                    None,
                ));
            }
        }
    }
    Ok(())
}

fn validate_input(input: &LogInput) -> Result<(), AppError> {
    if input.limit == 0 || input.limit > MAX_LIMIT {
        return Err(AppError::validation(
            "invalid_log_limit",
            format!("log limit must be between 1 and {MAX_LIMIT}"),
        ));
    }
    if let (Some(since), Some(until)) = (&input.since, &input.until) {
        if since > until {
            return Err(AppError::validation(
                "invalid_log_time_range",
                "--since must not be after --until",
            ));
        }
    }
    Ok(())
}

fn parse_kind(value: &str) -> Result<EventKind, AppError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|_| {
        AppError::validation(
            "invalid_event_kind",
            format!("unknown event kind `{value}`"),
        )
    })
}

struct JournalPaths {
    directory: PathBuf,
    active: PathBuf,
    lock: PathBuf,
}

fn paths(repository: &Path, command_timeout_secs: u64) -> Result<JournalPaths, AppError> {
    let request =
        CommandSpec::new("git").args(["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    let output = ProcessRunner::in_directory(repository)
        .with_timeout(Duration::from_secs(command_timeout_secs))
        .run(&request)
        .map_err(|error| journal_git_error(&error))?;
    if !output.is_success() {
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "journal_git_metadata_failed",
            "could not resolve common Git metadata directory",
            Some(json!({ "exit_code": output.code })),
        ));
    }
    let common = PathBuf::from(output.stdout.trim());
    let directory = common.join(JOURNAL_DIR);
    Ok(JournalPaths {
        active: directory.join(JOURNAL_FILE),
        lock: directory.join(LOCK_FILE),
        directory,
    })
}

fn journal_git_error(error: &CommandRunError) -> AppError {
    let category = if matches!(error, CommandRunError::Timeout { .. }) {
        ErrorCategory::Timeout
    } else {
        ErrorCategory::ExecutionFailure
    };
    AppError::structured(
        category,
        "journal_git_metadata_failed",
        format!("could not resolve common Git metadata directory: {error:?}"),
        None,
    )
}

fn open_lock(path: &Path) -> Result<File, AppError> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| io_error("journal_lock_open_failed", &error))
}

fn archive_path(active: &Path, index: u32) -> PathBuf {
    PathBuf::from(format!("{}.{index}", active.display()))
}

fn unix_millis() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

fn encode_error(error: &serde_json::Error) -> AppError {
    AppError::structured(
        ErrorCategory::SerializationError,
        "journal_encode_failed",
        format!("could not encode journal record: {error}"),
        None,
    )
}

fn io_error(code: &'static str, error: &std::io::Error) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        code,
        format!("event journal I/O failed: {error}"),
        Some(json!({
            "rollback": "completed GitHub mutations are not rolled back",
            "queue_authority": false,
        })),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::process::Command;
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::config::CaravanConfig;
    use crate::model::RepositoryId;

    fn fixture() -> (TempDir, AppContext) {
        let repository = TempDir::new().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(repository.path())
                .status()
                .unwrap()
                .success()
        );
        let mut config = CaravanConfig::default();
        config.journal.max_bytes = 1024;
        config.journal.max_archives = 2;
        let context = AppContext {
            repository_path: repository.path().to_path_buf(),
            config_path: repository.path().join("config.yaml"),
            config_existed: false,
            config,
        };
        (repository, context)
    }

    fn event(number: u64) -> CaravanEvent {
        CaravanEvent {
            version: 1,
            event_id: EventId(format!("event-{number}")),
            operation_id: OperationId(format!("operation-{number}")),
            kind: if number % 2 == 0 {
                EventKind::HeadAdvanced
            } else {
                EventKind::CiFailed
            },
            repository: RepositoryId {
                owner: "owner".to_owned(),
                name: "repo".to_owned(),
            },
            caravan_id: Some(PrNumber(number)),
            prs: vec![PrNumber(number)],
            fleet: None,
            reason: None,
            metadata: BTreeMap::new(),
            timestamp: format!("{number:013}"),
        }
    }

    #[test]
    fn exact_event_ids_are_deduplicated_and_filters_are_deterministic() {
        let (_repository, context) = fixture();
        append_event(&context, &event(1)).unwrap();
        append_event(&context, &event(1)).unwrap();
        append_event(&context, &event(2)).unwrap();
        let output = snapshot(
            &context,
            &LogInput {
                kind: Some("ci_failed".to_owned()),
                pr: Some(1),
                ..LogInput::default()
            },
        )
        .unwrap();
        assert_eq!(
            output.records,
            vec![JournalRecord::Event {
                version: JOURNAL_VERSION,
                event: event(1),
            }]
        );
    }

    #[test]
    fn concurrent_writers_rotate_without_corrupting_records() {
        let (_repository, context) = fixture();
        let context = Arc::new(context);
        let writers: Vec<_> = (0..8)
            .map(|number| {
                let context = Arc::clone(&context);
                std::thread::spawn(move || append_event(&context, &event(number)).unwrap())
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let output = snapshot(
            &context,
            &LogInput {
                limit: 10_000,
                ..LogInput::default()
            },
        )
        .unwrap();
        assert!(!output.records.is_empty());
        assert!(output.records.len() <= 8);
    }

    #[test]
    fn follow_emits_existing_tail_then_a_concurrent_append() {
        let (_repository, context) = fixture();
        append_event(&context, &event(1)).unwrap();
        let writer_context = context.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            append_event(&writer_context, &event(2)).unwrap();
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut ids = Vec::new();
        follow(&context, &LogInput::default(), &stop, |record| {
            ids.push(record.event_id().0.clone());
            if ids.len() == 2 {
                stop.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        })
        .unwrap();
        writer.join().unwrap();
        assert_eq!(ids, vec!["event-1", "event-2"]);
    }

    #[test]
    fn torn_final_record_is_ignored_and_recovered_before_the_next_append() {
        let (_repository, context) = fixture();
        append_event(&context, &event(1)).unwrap();
        let journal = paths(
            &context.repository_path,
            context.config.command_timeout_secs,
        )
        .unwrap();
        OpenOptions::new()
            .append(true)
            .open(&journal.active)
            .unwrap()
            .write_all(br#"{"record_type":"event"#)
            .unwrap();
        assert_eq!(
            snapshot(&context, &LogInput::default())
                .unwrap()
                .records
                .len(),
            1
        );
        append_event(&context, &event(2)).unwrap();
        assert_eq!(
            snapshot(&context, &LogInput::default())
                .unwrap()
                .records
                .len(),
            2
        );
    }
}
