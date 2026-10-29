//! Write-ahead sync identity and read-only recovery receipts (bd-6a588d).
//!
//! The scope is thread-local, like sync progress: all production subprocesses,
//! including repair runners, participate without creating a second writer.
//! No environment, stdin, command output, or credential-bearing arguments are
//! stored. Exact native merge intent lives in the sealed landing checkpoint.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use clap::Args;
use mcp_cli::StructuredError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::command::{CommandIntent, CommandRunError, CommandSpec};
use crate::github::{GitHubStackLandCheckpoint, GitHubStackLandPhase, GitHubStackMergeStatus};
use crate::model::OperationId;
use crate::{AppError, stack_checkpoint};

#[derive(Debug, Clone, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReceiptInput {
    /// Omit to list durable sync identities. Supply one to read its exact receipt.
    #[arg(long)]
    pub operation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WriteAttempt {
    pub program: String,
    pub intent: String,
    /// Digest only: arguments can contain comment bodies or credentials.
    pub arguments_hash: String,
    pub state: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SyncReceipt {
    pub schema_version: u32,
    pub operation_id: OperationId,
    /// `unfinished` is not success: after interruption consult exact evidence.
    pub state: String,
    pub writes: Vec<WriteAttempt>,
    pub native_operations: Vec<String>,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NativeReceipt {
    pub operation_id: String,
    /// Merge outcome only, not whether preceding lock/audit writes happened.
    pub outcome: String,
    pub request: crate::github::GitHubStackMergeRequestIdentity,
    pub checkpoint: GitHubStackLandCheckpoint,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReceiptOutput {
    pub operations: Vec<SyncReceipt>,
    pub native_merges: Vec<NativeReceipt>,
}

struct Active {
    repository: PathBuf,
    receipt: SyncReceipt,
}

thread_local! {
    static ACTIVE: RefCell<Option<Rc<RefCell<Active>>>> = const { RefCell::new(None) };
}

pub(crate) struct Scope {
    active: Rc<RefCell<Active>>,
    previous: Option<Rc<RefCell<Active>>>,
}

impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|slot| *slot.borrow_mut() = self.previous.take());
    }
}

fn save(active: &Active) -> Result<(), AppError> {
    stack_checkpoint::write(
        &active.repository,
        &format!("sync-{}", active.receipt.operation_id),
        &active.receipt,
    )
}

impl Scope {
    pub(crate) fn begin(repository: &Path) -> Result<Self, AppError> {
        let active = Rc::new(RefCell::new(Active {
            repository: repository.to_owned(),
            receipt: SyncReceipt {
                schema_version: 1,
                operation_id: OperationId::new(),
                state: "unfinished".to_owned(),
                writes: Vec::new(),
                native_operations: Vec::new(),
                error_code: None,
            },
        }));
        save(&active.borrow())?;
        // stderr is separate from the CLI/MCP result envelope and is flushed
        // before any provider write, so even SIGKILL leaves a caller-visible ID.
        eprintln!(
            "{}",
            json!({
                "phase": "sync_started",
                "operation_id": active.borrow().receipt.operation_id,
                "receipt_command": format!("cara operation --operation-id {} --json", active.borrow().receipt.operation_id),
            })
        );
        let previous = ACTIVE.with(|slot| slot.replace(Some(Rc::clone(&active))));
        Ok(Self { active, previous })
    }

    pub(crate) fn finish<T>(&self, result: Result<T, AppError>) -> Result<T, AppError> {
        let mut active = self.active.borrow_mut();
        let state = match &result {
            Ok(_) => "completed",
            Err(_) if !may_have_written(&active.receipt) => "failed_before_provider_write",
            Err(_) => "failed",
        };
        state.clone_into(&mut active.receipt.state);
        active.receipt.error_code = result.as_ref().err().map(StructuredError::code);
        let saved = save(&active);
        match (result, saved) {
            (Ok(output), Ok(())) => Ok(output),
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(attach(&active, &error)),
            (Err(error), Err(storage)) => {
                let mut error = attach(&active, &error);
                error.details.as_mut().expect("attached details")["receipt_write_error"] =
                    json!(storage.code());
                Err(error)
            }
        }
    }
}

fn may_have_written(receipt: &SyncReceipt) -> bool {
    receipt
        .writes
        .iter()
        .any(|attempt| attempt.state != "not_started")
}

fn attach(active: &Active, error: &AppError) -> AppError {
    let mut details = error.details().unwrap_or_else(|| json!({}));
    if !details.is_object() {
        details = json!({"source": details});
    }
    details["operation_id"] = json!(active.receipt.operation_id);
    details["receipt_command"] = json!(format!(
        "cara operation --operation-id {} --json",
        active.receipt.operation_id
    ));
    details["provider_write_may_have_occurred"] = json!(may_have_written(&active.receipt));
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

pub(crate) fn operation_id() -> OperationId {
    ACTIVE.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or_else(OperationId::new, |active| {
                active.borrow().receipt.operation_id.clone()
            })
    })
}

/// Must succeed before spawning a mutating subprocess. An unfinished attempt
/// means "may have run", never permission to replay it.
pub(crate) fn before_spawn(command: &CommandSpec) -> Result<Option<Attempt>, CommandRunError> {
    let intent = command
        .inferred_write_intent()
        .unwrap_or_else(|| command.intent());
    if !intent.is_write() {
        return Ok(None);
    }
    ACTIVE.with(|slot| {
        let Some(active) = slot.borrow().clone() else {
            return Ok(None);
        };
        let mut state = active.borrow_mut();
        let index = state.receipt.writes.len();
        state.receipt.writes.push(WriteAttempt {
            program: Path::new(&command.program)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            intent: match intent {
                CommandIntent::ProviderWrite => "provider_write",
                _ => "git_write",
            }
            .to_owned(),
            arguments_hash: crate::membership::fnv1a64(
                &serde_json::to_vec(&command.args).expect("arguments serialize"),
            ),
            state: "may_have_run".to_owned(),
            exit_code: None,
        });
        if let Err(error) = save(&state) {
            state.receipt.writes.pop();
            return Err(storage_refusal(command, &error));
        }
        drop(state);
        Ok(Some(Attempt { active, index }))
    })
}

pub(crate) struct Attempt {
    active: Rc<RefCell<Active>>,
    index: usize,
}

impl Attempt {
    pub(crate) fn finish(
        &self,
        command: &CommandSpec,
        state: &str,
        code: Option<i32>,
    ) -> Result<(), CommandRunError> {
        let mut active = self.active.borrow_mut();
        state.clone_into(&mut active.receipt.writes[self.index].state);
        active.receipt.writes[self.index].exit_code = code;
        save(&active).map_err(|error| storage_refusal(command, &error))
    }
}

fn storage_refusal(command: &CommandSpec, error: &AppError) -> CommandRunError {
    CommandRunError::MutationFenceRefused {
        command: command.clone(),
        intent: command.intent(),
        message: format!("durable sync receipt unavailable: {error}"),
    }
}

fn native_key(operation_id: &str) -> String {
    format!(
        "receipt-land-{}",
        crate::membership::fnv1a64(operation_id.as_bytes()).replace(':', "-")
    )
}

/// Archive before advancing the active cursor. Retirement can delete the
/// cursor but must never erase the operation's exact, inspectable evidence.
pub(crate) fn save_native(
    repository: &Path,
    key: &str,
    checkpoint: &GitHubStackLandCheckpoint,
) -> Result<(), AppError> {
    let outcome = match checkpoint.terminal_status {
        Some(GitHubStackMergeStatus::Merged) => "applied",
        Some(GitHubStackMergeStatus::Failed) => "not_applied",
        _ if checkpoint.phase < GitHubStackLandPhase::Submitting => "not_attempted",
        _ => "unknown",
    };
    let receipt = NativeReceipt {
        operation_id: checkpoint.plan.operation_id.clone(),
        outcome: outcome.to_owned(),
        request: checkpoint
            .terminal_receipt
            .as_ref()
            .map(|receipt| receipt.merge.request.clone())
            .or_else(|| {
                checkpoint
                    .merge
                    .as_ref()
                    .map(|merge| merge.merge.request.clone())
            })
            .unwrap_or_else(|| {
                crate::github::merge_request_identity(&checkpoint.repository, &checkpoint.plan)
            }),
        checkpoint: checkpoint.clone(),
    };
    stack_checkpoint::write(repository, &native_key(&receipt.operation_id), &receipt)?;
    ACTIVE.with(|slot| -> Result<(), AppError> {
        if let Some(active) = slot.borrow().as_ref() {
            let mut active = active.borrow_mut();
            if !active
                .receipt
                .native_operations
                .contains(&receipt.operation_id)
            {
                active
                    .receipt
                    .native_operations
                    .push(receipt.operation_id.clone());
                save(&active)?;
            }
        }
        Ok(())
    })?;
    stack_checkpoint::write(repository, key, checkpoint)
}

/// Local-only: neither writer acquisition nor provider access is needed, even
/// when the killed writer left a lock or current repository policy is broken.
pub fn inspect(repository: &Path, input: &ReceiptInput) -> Result<ReceiptOutput, AppError> {
    let mut operations = Vec::new();
    if let Some(id) = &input.operation_id {
        if uuid::Uuid::try_parse(id).is_ok() {
            if let Some(receipt) =
                stack_checkpoint::load::<SyncReceipt>(repository, &format!("sync-{id}"))?
            {
                operations.push(receipt);
            }
        }
    } else {
        for key in stack_checkpoint::list_keys(repository, "sync-")? {
            if let Some(receipt) = stack_checkpoint::load(repository, &key)? {
                operations.push(receipt);
            }
        }
    }
    let mut native_merges = Vec::new();
    for key in stack_checkpoint::list_keys(repository, "receipt-land-")? {
        if let Some(receipt) = stack_checkpoint::load::<NativeReceipt>(repository, &key)? {
            if !receipt.checkpoint.verify() {
                return Err(AppError::validation(
                    "sync_receipt_invalid",
                    "native receipt checkpoint failed verification",
                ));
            }
            if input.operation_id.as_ref().is_none_or(|id| {
                receipt.operation_id == *id
                    || receipt.operation_id.starts_with(&format!("{id}:stack:"))
                    || operations.iter().any(|operation| {
                        operation.native_operations.contains(&receipt.operation_id)
                    })
            }) {
                native_merges.push(receipt);
            }
        }
    }
    if input.operation_id.is_some() && operations.is_empty() && native_merges.is_empty() {
        return Err(AppError::validation(
            "sync_receipt_not_found",
            "no durable receipt for this operation in this Git common directory",
        ));
    }
    Ok(ReceiptOutput {
        operations,
        native_merges,
    })
}
