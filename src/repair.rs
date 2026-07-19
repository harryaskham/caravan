//! Cara-owned isolated repair workspaces for typed sync decisions.
//!
//! A repair session never mutates the caller's checkout. It checks out one
//! exact provider head into a linked worktree below Git's common metadata,
//! starts a non-committing merge against an exact target, and persists enough
//! evidence to verify an agent-owned conflict resolution before a plain
//! fast-forward push. The same clean workspace can then resume `sync --all`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mcp_cli::{ErrorCategory, StructuredError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::command::{CommandOutput, CommandRunError, CommandRunner, CommandSpec, ProcessRunner};
use crate::model::{BranchSnapshot, CommitOid, PrNumber, PullRequestSnapshot, RepositoryId};
use crate::operation_lock::OperationLock;
use crate::{AppContext, AppError, SyncInput};

const REPAIR_VERSION: u32 = 1;
const REPAIR_GIT_NAME_CONFIG: &str = "user.name=Caravan Repair";
const REPAIR_GIT_EMAIL_CONFIG: &str = "user.email=caravan-repair@users.noreply.github.com";
const REPAIR_DIRECTORY: &str = "repair-workspaces";
const MANIFEST_NAME: &str = "session.json";

/// Create an exact isolated repair session for one provider PR.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, clap::Args)]
pub struct RepairStartInput {
    /// Exact provider PR whose head needs semantic repair.
    #[arg(long, value_name = "PR")]
    pub pr: u64,
    /// Exact predecessor PR head to merge. Omit to merge the current default.
    #[arg(long, value_name = "PR")]
    #[serde(default)]
    pub target_pr: Option<u64>,
}

/// Verify, publish, and resume one persisted repair session.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, clap::Args)]
pub struct RepairContinueInput {
    /// Session ID returned by `cara repair start`.
    #[arg(long)]
    pub session: String,
    /// Publish the repair but do not immediately resume `sync --all`.
    #[arg(long)]
    #[serde(default)]
    pub no_sync: bool,
}

/// Inspect one repair session without changing Git or provider state.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, clap::Args)]
pub struct RepairStatusInput {
    /// Session ID returned by `cara repair start`.
    #[arg(long)]
    pub session: String,
}

/// Explicitly remove one preserved local repair session after review.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, clap::Args)]
pub struct RepairAbortInput {
    /// Exact session ID returned by `repair start`/`repair status`.
    #[arg(long)]
    pub session: String,
    /// Required acknowledgement that local repair workspace state is deleted.
    #[arg(long)]
    #[serde(default)]
    pub confirm: bool,
}

/// Auditable local-only repair cleanup receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepairAbortOutput {
    pub session: String,
    pub pr: PrNumber,
    pub previous_state: RepairState,
    pub workspace_removed: bool,
    pub provider_mutated: bool,
}

/// Durable repair lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RepairState {
    Preparing,
    Resolving,
    Committed,
    Published,
}

/// Exact external materialization phase persisted before each subprocess.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RepairPhase {
    #[default]
    Preparing,
    SeedingObjectCache,
    ConfiguringProvider,
    Cloning,
    FetchingHead,
    FetchingTarget,
    CheckingOut,
    Merging,
    Resolving,
    Committed,
    Published,
    SyncPending,
    Completed,
}

/// Bounded durable error evidence for a resumable materialization phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepairPhaseError {
    pub phase: RepairPhase,
    pub code: String,
    pub message: String,
    pub elapsed_ms: u64,
    pub timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_group_id: Option<u32>,
    pub partial_path: String,
    pub next: String,
}

fn default_materialization_timeout_secs() -> u64 {
    180
}

/// Stable, secret-free repair session receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepairSession {
    pub version: u32,
    pub session: String,
    pub repository: RepositoryId,
    pub pr: PrNumber,
    pub head: BranchSnapshot,
    pub old_base: BranchSnapshot,
    pub target: BranchSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_pr: Option<PrNumber>,
    pub workspace: String,
    /// Explicit provider-owned Git URL. Repair never depends on or changes the
    /// caller checkout's possibly internal `origin` configuration.
    pub provider_git_url: String,
    /// Local canonical/object-cache checkout used only as a content-addressed
    /// seed. Provider refs are always re-read from `provider_git_url`.
    #[serde(default)]
    pub object_cache_path: String,
    #[serde(default)]
    pub object_cache_common_dir: String,
    pub config_path: String,
    pub config_fingerprint: String,
    #[serde(default = "default_materialization_timeout_secs")]
    pub materialization_timeout_secs: u64,
    pub state: RepairState,
    #[serde(default)]
    pub phase: RepairPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<RepairPhaseError>,
    #[serde(default)]
    pub conflicting_paths: Vec<String>,
    /// Stage-zero index entries created mechanically by the initial merge.
    /// Agent edits are allowed only for `conflicting_paths`.
    #[serde(default)]
    pub baseline_index: BTreeMap<String, String>,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_head: Option<CommitOid>,
}

/// Result of preparing an isolated workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepairStartOutput {
    pub repair: RepairSession,
    pub already_exists: bool,
    pub next: String,
}

/// Exact non-force publication proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepairPublicationReceipt {
    pub pr: PrNumber,
    pub branch: String,
    pub old_head: CommitOid,
    pub target: CommitOid,
    pub new_head: CommitOid,
    pub parents: Vec<CommitOid>,
    pub force: bool,
    pub remote_verified: bool,
}

/// Result of a verified repair continuation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RepairContinueOutput {
    pub repair: RepairSession,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication: Option<RepairPublicationReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<crate::sync::SyncOutput>,
    pub workspace_preserved: bool,
    pub next: String,
}

/// Prepare one exact provider repair in a persistent linked worktree.
pub fn start(
    context: &AppContext,
    input: &RepairStartInput,
) -> Result<RepairStartOutput, AppError> {
    let mut lock = OperationLock::acquire(&context.repository_path, "repair-start")?;
    lock.checkpoint(
        "repair_discovery_in_flight",
        json!({"pr": input.pr, "target_pr": input.target_pr}),
        false,
    )?;
    let status = crate::read::status(context)?;
    crate::initialization::require_ready(&status.initialization)?;
    let pr = PrNumber(input.pr);
    let candidate = status
        .analysis
        .pull_requests
        .get(&pr)
        .ok_or_else(|| {
            AppError::structured(
                ErrorCategory::TargetNotFound,
                "repair_pr_not_found",
                format!("PR #{pr} is not an open provider PR in the fresh snapshot"),
                Some(json!({"pr": pr, "mutated": false})),
            )
        })?
        .clone();
    let target = match input.target_pr.map(PrNumber) {
        Some(number) => status
            .analysis
            .pull_requests
            .get(&number)
            .map(|pull_request| pull_request.head.clone())
            .ok_or_else(|| {
                AppError::structured(
                    ErrorCategory::TargetNotFound,
                    "repair_target_pr_not_found",
                    format!("target PR #{number} is not present in the fresh provider snapshot"),
                    Some(json!({"pr": pr, "target_pr": number, "mutated": false})),
                )
            })?,
        None => status.analysis.fleet.default_branch.clone(),
    };
    let provider_git_url = provider_git_url(context, &status.repository)?;
    let output = start_exact(
        context,
        &status.repository,
        &candidate,
        &target,
        input.target_pr.map(PrNumber),
        &provider_git_url,
    )?;
    lock.checkpoint("repair_workspace_ready", json!(&output.repair), false)?;
    lock.release()?;
    Ok(output)
}

#[allow(clippy::too_many_lines)]
fn start_exact(
    context: &AppContext,
    repository: &RepositoryId,
    candidate: &PullRequestSnapshot,
    target: &BranchSnapshot,
    target_pr: Option<PrNumber>,
    provider_git_url: &str,
) -> Result<RepairStartOutput, AppError> {
    require_owned_repair(repository, candidate, target)?;
    let paths = repair_paths(&context.repository_path, candidate.number)?;
    if paths.manifest.exists() {
        let existing = read_manifest(&paths.manifest)?;
        if existing.head == candidate.head && existing.target == *target {
            if existing.state == RepairState::Preparing {
                if existing.repository != *repository
                    || existing.pr != candidate.number
                    || existing.old_base != candidate.base
                    || existing.target_pr != target_pr
                    || existing.provider_git_url != provider_git_url
                {
                    return Err(AppError::structured(
                        ErrorCategory::Validation,
                        "repair_preparing_manifest_drift",
                        "preserved preparing session no longer matches fresh repository/PR/provider facts",
                        Some(json!({
                            "existing": existing,
                            "requested_head": candidate.head,
                            "requested_target": target,
                            "next": "inspect and confirm-abort the stale session; never overwrite it",
                        })),
                    ));
                }
                let mut resumed = existing;
                resumed.object_cache_path = canonical_repository_path(context)?;
                resumed.object_cache_common_dir = canonical_git_common_dir(context)?;
                resumed.config_path = absolute_config_path(context);
                resumed.config_fingerprint = config_fingerprint(context)?;
                resumed.materialization_timeout_secs =
                    context.config.repair.materialization_timeout_secs;
                resumed.last_error = None;
                resumed.updated_unix_ms = unix_ms();
                let reuse_workspace = if paths.workspace.exists() {
                    validate_partial_workspace_path(&paths, &resumed)?;
                    if partial_workspace_ready(&paths.workspace, provider_git_url) {
                        true
                    } else {
                        fs::remove_dir_all(&paths.workspace).map_err(|error| {
                            repair_io_error(
                                "repair_partial_cleanup_failed",
                                "could not remove the verified incomplete clone before resume",
                                &paths.workspace,
                                &error,
                            )
                        })?;
                        false
                    }
                } else {
                    false
                };
                write_manifest(&paths.manifest, &resumed)?;
                return materialize_repair(
                    &paths,
                    candidate,
                    target,
                    resumed,
                    true,
                    reuse_workspace,
                );
            }
            validate_workspace(&paths, &existing)?;
            return Ok(RepairStartOutput {
                repair: existing,
                already_exists: true,
                next: "edit only the reported conflicting paths, stage the resolutions, then run `cara repair continue --session <id>`".to_owned(),
            });
        }
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_session_active",
            format!(
                "repair session `{}` already owns PR #{}",
                existing.session, existing.pr
            ),
            Some(json!({
                "existing": existing,
                "requested_head": candidate.head,
                "requested_target": target,
                "next": "finish or explicitly clean the existing repair before starting a different generation",
            })),
        ));
    }

    fs::create_dir_all(&paths.root).map_err(|error| {
        repair_io_error(
            "repair_directory_failed",
            "could not create Cara's repair workspace directory",
            &paths.root,
            &error,
        )
    })?;
    let now = unix_ms();
    let repair = RepairSession {
        version: REPAIR_VERSION,
        session: format!(
            "pr-{}-{}",
            candidate.number.0,
            short_oid(&candidate.head.oid)
        ),
        repository: repository.clone(),
        pr: candidate.number,
        head: candidate.head.clone(),
        old_base: candidate.base.clone(),
        target: target.clone(),
        target_pr,
        workspace: paths.workspace.display().to_string(),
        provider_git_url: provider_git_url.to_owned(),
        object_cache_path: canonical_repository_path(context)?,
        object_cache_common_dir: canonical_git_common_dir(context)?,
        config_path: absolute_config_path(context),
        config_fingerprint: config_fingerprint(context)?,
        materialization_timeout_secs: context.config.repair.materialization_timeout_secs,
        state: RepairState::Preparing,
        phase: RepairPhase::Preparing,
        last_error: None,
        conflicting_paths: Vec::new(),
        baseline_index: BTreeMap::new(),
        created_unix_ms: now,
        updated_unix_ms: now,
        published_head: None,
    };
    write_manifest(&paths.manifest, &repair)?;
    materialize_repair(&paths, candidate, target, repair, false, false)
}

#[allow(clippy::too_many_lines)]
fn materialize_repair(
    paths: &RepairPaths,
    candidate: &PullRequestSnapshot,
    target: &BranchSnapshot,
    mut repair: RepairSession,
    resumed: bool,
    reuse_workspace: bool,
) -> Result<RepairStartOutput, AppError> {
    let timeout = Duration::from_secs(repair.materialization_timeout_secs);
    let root_runner = ProcessRunner::in_directory(&paths.session_root).with_timeout(timeout);
    let workspace = paths.workspace.to_string_lossy().into_owned();
    let provider_git_url = repair.provider_git_url.clone();
    verify_object_cache_identity(&repair)?;
    if !reuse_workspace {
        let object_cache_path = repair.object_cache_path.clone();
        run_materialization_phase(
            &mut repair,
            paths,
            RepairPhase::SeedingObjectCache,
            timeout,
            || {
                require_success(
                    &root_runner,
                    CommandSpec::new("git")
                        .args([
                            "clone",
                            "--quiet",
                            "--shared",
                            "--no-checkout",
                            "--origin",
                            "cache",
                            object_cache_path.as_str(),
                            workspace.as_str(),
                        ])
                        .env("GIT_TERMINAL_PROMPT", "0"),
                    "repair_workspace_seed_failed",
                    "could not seed the isolated repair repository from the canonical object cache",
                )?;
                Ok(())
            },
        )?;
        let workspace_runner = ProcessRunner::in_directory(&paths.workspace).with_timeout(timeout);
        run_materialization_phase(
            &mut repair,
            paths,
            RepairPhase::ConfiguringProvider,
            timeout,
            || {
                require_success(
                    &workspace_runner,
                    CommandSpec::new("git").args([
                        "remote",
                        "add",
                        "origin",
                        provider_git_url.as_str(),
                    ]),
                    "repair_provider_remote_failed",
                    "could not bind the isolated repair repository to the explicit provider remote",
                )?;
                Ok(())
            },
        )?;
    }

    let workspace_runner = ProcessRunner::in_directory(&paths.workspace).with_timeout(timeout);
    run_materialization_phase(
        &mut repair,
        paths,
        RepairPhase::FetchingHead,
        timeout,
        || {
            fetch_exact_materialization(&workspace_runner, &provider_git_url, &candidate.head)?;
            Ok(())
        },
    )?;
    run_materialization_phase(
        &mut repair,
        paths,
        RepairPhase::FetchingTarget,
        timeout,
        || {
            fetch_exact_materialization(&workspace_runner, &provider_git_url, target)?;
            Ok(())
        },
    )?;
    run_materialization_phase(
        &mut repair,
        paths,
        RepairPhase::CheckingOut,
        timeout,
        || {
            require_success(
                &workspace_runner,
                CommandSpec::new("git").args([
                    "checkout",
                    "--quiet",
                    "--detach",
                    candidate.head.oid.0.as_str(),
                ]),
                "repair_workspace_checkout_failed",
                "could not check out the exact provider repair head",
            )?;
            Ok(())
        },
    )?;
    let merge =
        run_materialization_phase(&mut repair, paths, RepairPhase::Merging, timeout, || {
            run(
                &workspace_runner,
                CommandSpec::new("git")
                    .args([
                        "-c",
                        REPAIR_GIT_NAME_CONFIG,
                        "-c",
                        REPAIR_GIT_EMAIL_CONFIG,
                        "-c",
                        "commit.gpgSign=false",
                        "merge",
                        "--no-commit",
                        "--no-ff",
                        target.oid.0.as_str(),
                    ])
                    .env("GIT_TERMINAL_PROMPT", "0"),
            )
        })?;
    if !matches!(merge.code, Some(0 | 1)) {
        let error = repair_command_failure(
            "repair_merge_failed",
            "Git could not start the exact-target repair merge",
            &merge,
            json!({"repair": repair, "workspace_preserved": true}),
        );
        record_phase_error(&mut repair, paths, RepairPhase::Merging, timeout, 0, &error)?;
        return Err(error);
    }
    let merge_head = rev_parse(&workspace_runner, "MERGE_HEAD")?;
    if merge_head != target.oid {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_merge_target_mismatch",
            "the isolated merge recorded a different target than the exact repair receipt",
            Some(json!({"expected": target.oid, "actual": merge_head, "repair": repair})),
        ));
    }
    let conflicting_paths = nul_paths(
        &require_success(
            &workspace_runner,
            CommandSpec::new("git").args(["diff", "--name-only", "--diff-filter=U", "-z"]),
            "repair_conflict_inspection_failed",
            "could not inspect repair conflict paths",
        )?
        .stdout,
    )?;
    if merge.code == Some(1) && conflicting_paths.is_empty() {
        return Err(repair_command_failure(
            "repair_merge_failed",
            "Git reported a merge failure without typed conflict paths",
            &merge,
            json!({"repair": repair, "workspace_preserved": true}),
        ));
    }
    repair.conflicting_paths = conflicting_paths;
    repair.baseline_index = stage_zero_index(&workspace_runner)?;
    repair.state = RepairState::Resolving;
    repair.phase = RepairPhase::Resolving;
    repair.last_error = None;
    repair.updated_unix_ms = unix_ms();
    write_manifest(&paths.manifest, &repair)?;
    Ok(RepairStartOutput {
        repair,
        already_exists: resumed,
        next: "resolve only the reported conflicting paths in the managed workspace, stage them, then run `cara repair continue --session <id>`; do not commit, update refs, or push manually".to_owned(),
    })
}

fn run_materialization_phase<T>(
    repair: &mut RepairSession,
    paths: &RepairPaths,
    phase: RepairPhase,
    timeout: Duration,
    operation: impl FnOnce() -> Result<T, AppError>,
) -> Result<T, AppError> {
    repair.phase = phase;
    repair.last_error = None;
    repair.updated_unix_ms = unix_ms();
    write_manifest(&paths.manifest, repair)?;
    let started = std::time::Instant::now();
    match operation() {
        Ok(value) => Ok(value),
        Err(error) => {
            record_phase_error(
                repair,
                paths,
                phase,
                timeout,
                duration_millis(started.elapsed()),
                &error,
            )?;
            let mut details = error.details().unwrap_or_else(|| json!({}));
            if let Some(object) = details.as_object_mut() {
                object.insert("repair".to_owned(), json!(repair));
                object.insert("phase".to_owned(), json!(phase));
                object.insert(
                    "elapsed_ms".to_owned(),
                    json!(duration_millis(started.elapsed())),
                );
                object.insert("timeout_ms".to_owned(), json!(duration_millis(timeout)));
                let process_group_reaped = error.category() == ErrorCategory::Timeout
                    && error
                        .details()
                        .and_then(|details| details.get("process_group_id").cloned())
                        .is_some_and(|value| !value.is_null());
                object.insert(
                    "process_group_reaped".to_owned(),
                    json!(process_group_reaped),
                );
                object.insert(
                    "next".to_owned(),
                    json!("rerun the exact `cara repair start` to resume this preparing session, or inspect then confirm-abort it"),
                );
            }
            Err(AppError::structured(
                error.category(),
                error.code(),
                error.message(),
                Some(details),
            ))
        }
    }
}

fn record_phase_error(
    repair: &mut RepairSession,
    paths: &RepairPaths,
    phase: RepairPhase,
    timeout: Duration,
    elapsed_ms: u64,
    error: &AppError,
) -> Result<(), AppError> {
    let process_group_id = error
        .details()
        .and_then(|details| details.get("process_group_id").cloned())
        .and_then(|value| serde_json::from_value::<Option<u32>>(value).ok())
        .flatten();
    repair.last_error = Some(RepairPhaseError {
        phase,
        code: error.code(),
        message: error.message(),
        elapsed_ms,
        timeout_ms: duration_millis(timeout),
        process_group_id,
        partial_path: paths.workspace.display().to_string(),
        next: "rerun exact repair start to resume, or repair status then confirmed abort"
            .to_owned(),
    });
    repair.updated_unix_ms = unix_ms();
    write_manifest(&paths.manifest, repair)
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Verify agent-owned conflict resolution, publish non-force, and resume sync.
#[allow(clippy::too_many_lines)]
pub fn continue_session(
    context: &AppContext,
    input: &RepairContinueInput,
) -> Result<RepairContinueOutput, AppError> {
    validate_session_id(&input.session)?;
    let paths = repair_paths_for_session(&context.repository_path, &input.session)?;
    let mut repair = read_manifest(&paths.manifest)?;
    require_session_match(&repair, &input.session)?;
    if repair.state == RepairState::Preparing {
        let target = repair
            .target_pr
            .map_or_else(String::new, |number| format!(" --target-pr {number}"));
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_preparation_incomplete",
            "repair workspace materialization is incomplete and must be resumed from fresh provider facts",
            Some(json!({
                "repair": repair,
                "workspace_preserved": paths.workspace.exists(),
                "next": format!("rerun `cara repair start --pr {}{target}` to resume the exact preparing session, or inspect then confirm-abort it", repair.pr),
            })),
        ));
    }
    validate_workspace(&paths, &repair)?;
    let current_fingerprint = config_fingerprint(context)?;
    if current_fingerprint != repair.config_fingerprint {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_config_changed",
            "Caravan config changed after the exact repair session was prepared",
            Some(json!({
                "repair": repair,
                "current_config_fingerprint": current_fingerprint,
                "workspace_preserved": true,
                "next": "review the new policy and start a new repair generation rather than publishing stale-policy work",
            })),
        ));
    }

    if repair.state == RepairState::Published {
        let publication = repair
            .published_head
            .clone()
            .map(|new_head| RepairPublicationReceipt {
                pr: repair.pr,
                branch: repair.head.name.clone(),
                old_head: repair.head.oid.clone(),
                target: repair.target.oid.clone(),
                new_head,
                parents: vec![repair.head.oid.clone(), repair.target.oid.clone()],
                force: false,
                remote_verified: true,
            });
        return resume_or_return(context, input, &paths, repair, publication);
    }

    let mut lock = OperationLock::acquire(&context.repository_path, "repair-continue")?;
    lock.checkpoint("repair_verification_in_flight", json!(&repair), false)?;
    let timeout = Duration::from_secs(context.config.command_timeout_secs);
    let runner = ProcessRunner::in_directory(&paths.workspace).with_timeout(timeout);
    let expected_parents = vec![repair.head.oid.clone(), repair.target.oid.clone()];

    let (new_head, parents) = match repair.state {
        RepairState::Preparing => unreachable!("preparing state returned above"),
        RepairState::Resolving => {
            if try_rev_parse(&runner, "MERGE_HEAD")?.is_some() {
                verify_resolution(&runner, &repair)?;
                verify_remote_head(&runner, &repair.provider_git_url, &repair.head)?;
                require_success(
                    &runner,
                    CommandSpec::new("git")
                        .args([
                            "-c",
                            REPAIR_GIT_NAME_CONFIG,
                            "-c",
                            REPAIR_GIT_EMAIL_CONFIG,
                            "-c",
                            "commit.gpgSign=false",
                            "commit",
                            "-m",
                            &format!(
                                "Repair PR #{} against {} with Cara",
                                repair.pr, repair.target.name
                            ),
                        ])
                        .env("GIT_TERMINAL_PROMPT", "0"),
                    "repair_commit_failed",
                    "could not commit the verified repair resolution",
                )?;
            }
            // If the process died after commit but before the manifest update,
            // exact parents recover that boundary without another commit.
            let head = rev_parse(&runner, "HEAD")?;
            let found_parents = commit_parents(&runner, &head)?;
            require_exact_parents(&repair, &head, &found_parents, &expected_parents)?;
            repair.state = RepairState::Committed;
            repair.phase = RepairPhase::Committed;
            repair.last_error = None;
            repair.published_head = Some(head.clone());
            repair.updated_unix_ms = unix_ms();
            write_manifest(&paths.manifest, &repair)?;
            lock.checkpoint(
                "repair_committed",
                json!({"repair": &repair, "new_head": &head, "parents": &found_parents}),
                false,
            )?;
            (head, found_parents)
        }
        RepairState::Committed => {
            let head = repair.published_head.clone().ok_or_else(|| {
                AppError::validation(
                    "repair_manifest_invalid",
                    "committed repair manifest omitted its exact prepared head",
                )
            })?;
            if rev_parse(&runner, "HEAD")? != head {
                return Err(AppError::structured(
                    ErrorCategory::Validation,
                    "repair_workspace_head_changed",
                    "repair workspace HEAD changed after the committed checkpoint",
                    Some(json!({"repair": repair, "workspace_preserved": true})),
                ));
            }
            let found_parents = commit_parents(&runner, &head)?;
            require_exact_parents(&repair, &head, &found_parents, &expected_parents)?;
            (head, found_parents)
        }
        RepairState::Published => unreachable!("published state returned above"),
    };

    // The prepared merge is valid only for the exact target generation too.
    // A moved default/predecessor must be rediscovered rather than publishing
    // a repair against stale ancestry.
    verify_remote_head(&runner, &repair.provider_git_url, &repair.target)?;
    let actual_remote = remote_head_oid(&runner, &repair.provider_git_url, &repair.head.name)?;
    if actual_remote == repair.head.oid {
        lock.checkpoint(
            "repair_publication_in_flight",
            json!({
                "repair": &repair,
                "remote_expected_head": &repair.head.oid,
                "new_head": &new_head,
                "force": false,
            }),
            true,
        )?;
        let destination = format!("HEAD:refs/heads/{}", repair.head.name);
        require_success(
            &runner,
            CommandSpec::new("git")
                .args([
                    "push",
                    repair.provider_git_url.as_str(),
                    destination.as_str(),
                ])
                .env("GIT_TERMINAL_PROMPT", "0"),
            "repair_non_fast_forward",
            "non-force repair publication failed; the provider branch may have moved",
        )?;
    } else if actual_remote != new_head {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_stale_head",
            "provider repair branch moved to a different generation before publication",
            Some(json!({
                "repair": repair,
                "expected_old_head": repair.head.oid,
                "prepared_head": new_head,
                "actual_remote_head": actual_remote,
                "workspace_preserved": true,
            })),
        ));
    }
    let published = BranchSnapshot {
        repository: repair.head.repository.clone(),
        name: repair.head.name.clone(),
        oid: new_head.clone(),
    };
    verify_remote_head(&runner, &repair.provider_git_url, &published)?;
    let publication = RepairPublicationReceipt {
        pr: repair.pr,
        branch: repair.head.name.clone(),
        old_head: repair.head.oid.clone(),
        target: repair.target.oid.clone(),
        new_head: new_head.clone(),
        parents,
        force: false,
        remote_verified: true,
    };
    repair.state = RepairState::Published;
    repair.phase = RepairPhase::Published;
    repair.last_error = None;
    repair.published_head = Some(new_head);
    repair.updated_unix_ms = unix_ms();
    write_manifest(&paths.manifest, &repair)?;
    lock.checkpoint("repair_published", json!(&publication), false)?;
    lock.release()?;
    resume_or_return(context, input, &paths, repair, Some(publication))
}

fn resume_or_return(
    context: &AppContext,
    input: &RepairContinueInput,
    paths: &RepairPaths,
    mut repair: RepairSession,
    publication: Option<RepairPublicationReceipt>,
) -> Result<RepairContinueOutput, AppError> {
    if input.no_sync {
        return Ok(RepairContinueOutput {
            repair,
            publication,
            sync: None,
            workspace_preserved: true,
            next: "run `cara repair continue --session <id>` without --no-sync to resume `sync --all` from the clean managed workspace".to_owned(),
        });
    }
    // Keep the caller repository lock held while sync runs in the independent
    // provider clone; this preserves one local mutation owner even though the
    // clone has separate Git metadata.
    repair.phase = RepairPhase::SyncPending;
    repair.updated_unix_ms = unix_ms();
    write_manifest(&paths.manifest, &repair)?;
    let caller_lock = OperationLock::acquire(&context.repository_path, "repair-sync")?;
    let workspace_context = AppContext {
        repository_path: paths.workspace.clone(),
        config_path: PathBuf::from(&repair.config_path),
        config_existed: context.config_existed,
        config: context.config.clone(),
    };
    match crate::sync::sync(
        &workspace_context,
        &SyncInput {
            all: true,
            rerun_failed: false,
        },
    ) {
        Ok(sync) => {
            caller_lock.release()?;
            repair.phase = RepairPhase::Completed;
            repair.updated_unix_ms = unix_ms();
            write_manifest(&paths.manifest, &repair)?;
            cleanup_workspace(paths)?;
            Ok(RepairContinueOutput {
                repair,
                publication,
                sync: Some(sync),
                workspace_preserved: false,
                next:
                    "repair published and `sync --all` converged; the managed workspace was removed"
                        .to_owned(),
            })
        }
        Err(error) => {
            drop(caller_lock);
            Err(attach_repair_resume(&error, &repair, publication.as_ref()))
        }
    }
}

/// Explicitly remove one reviewed local session. This never changes provider state.
pub fn abort(
    context: &AppContext,
    input: &RepairAbortInput,
) -> Result<RepairAbortOutput, AppError> {
    if !input.confirm {
        return Err(AppError::validation(
            "repair_abort_confirmation_required",
            "pass --confirm only after reviewing `cara repair status --session <id>`",
        ));
    }
    validate_session_id(&input.session)?;
    let paths = repair_paths_for_session(&context.repository_path, &input.session)?;
    let repair = read_manifest(&paths.manifest)?;
    require_session_match(&repair, &input.session)?;
    let lock = OperationLock::acquire(&context.repository_path, "repair-abort")?;
    let workspace_removed = paths.session_root.exists();
    if workspace_removed {
        fs::remove_dir_all(&paths.session_root).map_err(|error| {
            repair_io_error(
                "repair_workspace_cleanup_failed",
                "could not remove the confirmed repair session",
                &paths.session_root,
                &error,
            )
        })?;
    }
    lock.release()?;
    Ok(RepairAbortOutput {
        session: repair.session,
        pr: repair.pr,
        previous_state: repair.state,
        workspace_removed,
        provider_mutated: false,
    })
}

/// Inspect a persisted repair session.
pub fn status(context: &AppContext, input: &RepairStatusInput) -> Result<RepairSession, AppError> {
    validate_session_id(&input.session)?;
    let paths = repair_paths_for_session(&context.repository_path, &input.session)?;
    let repair = read_manifest(&paths.manifest)?;
    require_session_match(&repair, &input.session)?;
    validate_manifest_path(&paths, &repair)?;
    if repair.state == RepairState::Preparing {
        if paths.workspace.exists() {
            validate_partial_workspace_path(&paths, &repair)?;
        }
    } else {
        validate_workspace(&paths, &repair)?;
    }
    Ok(repair)
}

fn verify_resolution(runner: &impl CommandRunner, repair: &RepairSession) -> Result<(), AppError> {
    let merge_head = rev_parse(runner, "MERGE_HEAD")?;
    if merge_head != repair.target.oid {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_merge_target_mismatch",
            "repair workspace no longer contains the exact expected merge target",
            Some(json!({"repair": repair, "actual_merge_head": merge_head})),
        ));
    }
    let unmerged = require_success(
        runner,
        CommandSpec::new("git").args(["ls-files", "-u", "-z"]),
        "repair_index_inspection_failed",
        "could not inspect unresolved index entries",
    )?;
    if !unmerged.stdout.is_empty() {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_conflicts_unresolved",
            "repair workspace still has unresolved index entries",
            Some(json!({
                "repair": repair,
                "conflicting_paths": repair.conflicting_paths,
                "next": "resolve and stage every reported conflict without committing",
            })),
        ));
    }
    let unstaged = run(runner, CommandSpec::new("git").args(["diff", "--quiet"]))?;
    if unstaged.code != Some(0) {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_unstaged_changes",
            "repair workspace has unstaged tracked changes",
            Some(json!({"repair": repair, "next": "stage the completed conflict resolution"})),
        ));
    }
    let untracked = require_success(
        runner,
        CommandSpec::new("git").args(["ls-files", "--others", "--exclude-standard", "-z"]),
        "repair_worktree_inspection_failed",
        "could not inspect untracked repair files",
    )?;
    if !untracked.stdout.is_empty() {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_untracked_files",
            "repair workspace contains untracked files outside the exact merge",
            Some(json!({"repair": repair, "paths": nul_paths(&untracked.stdout)?})),
        ));
    }
    require_success(
        runner,
        CommandSpec::new("git").args(["diff", "--cached", "--check"]),
        "repair_conflict_markers_present",
        "staged repair still contains conflict markers or whitespace errors",
    )?;
    let current = stage_zero_index(runner)?;
    let conflicts = repair
        .conflicting_paths
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    for (path, oid) in &repair.baseline_index {
        if conflicts.contains(path) {
            continue;
        }
        if current.get(path) != Some(oid) {
            return Err(scope_error(repair, path));
        }
    }
    for path in current.keys() {
        if !repair.baseline_index.contains_key(path) && !conflicts.contains(path) {
            return Err(scope_error(repair, path));
        }
    }
    Ok(())
}

fn scope_error(repair: &RepairSession, path: &str) -> AppError {
    AppError::structured(
        ErrorCategory::Validation,
        "repair_scope_changed",
        format!("path `{path}` changed outside the typed conflict scope"),
        Some(json!({
            "repair": repair,
            "path": path,
            "allowed_paths": repair.conflicting_paths,
            "next": "revert unrelated edits in the managed workspace; semantic changes are allowed only for typed conflict paths",
        })),
    )
}

fn require_owned_repair(
    repository: &RepositoryId,
    candidate: &PullRequestSnapshot,
    target: &BranchSnapshot,
) -> Result<(), AppError> {
    if candidate.cross_repository
        || candidate.head.repository != *repository
        || candidate.base.repository != *repository
        || target.repository != *repository
    {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_repository_not_owned",
            "repair requires same-repository candidate and target branches owned by the base repository",
            Some(json!({"pr": candidate.number, "mutated": false})),
        ));
    }
    if candidate.state != crate::model::PullRequestState::Open {
        return Err(AppError::validation(
            "repair_pr_not_open",
            format!("PR #{} is not open", candidate.number),
        ));
    }
    Ok(())
}

fn verify_remote_head(
    runner: &impl CommandRunner,
    provider_git_url: &str,
    branch: &BranchSnapshot,
) -> Result<(), AppError> {
    let actual = remote_head_oid(runner, provider_git_url, &branch.name)?;
    if actual != branch.oid {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_stale_head",
            "provider repair branch moved since the exact session receipt",
            Some(json!({
                "branch": branch.name,
                "expected_oid": branch.oid,
                "actual_oid": actual,
                "mutated": false,
                "next": "preserve the workspace, rediscover provider state, and start a new exact repair generation",
            })),
        ));
    }
    Ok(())
}

fn remote_head_oid(
    runner: &impl CommandRunner,
    provider_git_url: &str,
    branch: &str,
) -> Result<CommitOid, AppError> {
    let reference = format!("refs/heads/{branch}");
    let output = require_success(
        runner,
        CommandSpec::new("git").args(["ls-remote", "--refs", provider_git_url, reference.as_str()]),
        "repair_remote_head_unavailable",
        "could not verify the exact provider repair head",
    )?;
    let oid = CommitOid(
        output
            .stdout
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned(),
    );
    if !matches!(oid.0.len(), 40 | 64) || !oid.0.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::structured(
            ErrorCategory::TargetNotFound,
            "repair_remote_head_unavailable",
            "provider did not advertise one valid full repair branch OID",
            Some(json!({"provider_git_url": provider_git_url, "branch": branch})),
        ));
    }
    Ok(oid)
}

fn require_exact_parents(
    repair: &RepairSession,
    new_head: &CommitOid,
    actual: &[CommitOid],
    expected: &[CommitOid],
) -> Result<(), AppError> {
    if actual == expected {
        return Ok(());
    }
    Err(AppError::structured(
        ErrorCategory::Validation,
        "repair_parent_mismatch",
        "repair commit does not have the exact provider head and target parents",
        Some(json!({
            "repair": repair,
            "new_head": new_head,
            "expected_parents": expected,
            "actual_parents": actual,
            "workspace_preserved": true,
        })),
    ))
}

fn commit_parents(
    runner: &impl CommandRunner,
    commit: &CommitOid,
) -> Result<Vec<CommitOid>, AppError> {
    let output = require_success(
        runner,
        CommandSpec::new("git").args(["rev-list", "--parents", "-n", "1", commit.0.as_str()]),
        "repair_parent_inspection_failed",
        "could not inspect repair commit parents",
    )?;
    let mut fields = output.stdout.split_whitespace();
    if fields.next() != Some(commit.0.as_str()) {
        return Err(AppError::validation(
            "repair_parent_inspection_failed",
            "Git returned an unexpected repair commit identity",
        ));
    }
    Ok(fields.map(|value| CommitOid(value.to_owned())).collect())
}

fn stage_zero_index(runner: &impl CommandRunner) -> Result<BTreeMap<String, String>, AppError> {
    let output = require_success(
        runner,
        CommandSpec::new("git").args(["ls-files", "--stage", "-z"]),
        "repair_index_inspection_failed",
        "could not inspect the repair index",
    )?;
    let mut index = BTreeMap::new();
    for entry in output.stdout.split('\0').filter(|entry| !entry.is_empty()) {
        let (metadata, path) = entry.split_once('\t').ok_or_else(|| {
            AppError::validation(
                "repair_index_output_invalid",
                "git ls-files returned an invalid index entry",
            )
        })?;
        let mut fields = metadata.split_whitespace();
        let _mode = fields.next();
        let oid = fields.next().ok_or_else(|| {
            AppError::validation(
                "repair_index_output_invalid",
                "git ls-files omitted an object ID",
            )
        })?;
        let stage = fields.next().ok_or_else(|| {
            AppError::validation(
                "repair_index_output_invalid",
                "git ls-files omitted an index stage",
            )
        })?;
        if stage == "0" {
            index.insert(path.to_owned(), oid.to_owned());
        }
    }
    Ok(index)
}

fn nul_paths(output: &str) -> Result<Vec<String>, AppError> {
    if output.contains('\u{fffd}') {
        return Err(AppError::validation(
            "repair_path_encoding_unsupported",
            "repair paths must be valid UTF-8 for bounded receipts",
        ));
    }
    Ok(output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

fn try_rev_parse(
    runner: &impl CommandRunner,
    revision: &str,
) -> Result<Option<CommitOid>, AppError> {
    let output = run(
        runner,
        CommandSpec::new("git").args(["rev-parse", "--verify", "--quiet", revision]),
    )?;
    match output.code {
        Some(0) => {
            let oid = CommitOid(output.stdout.trim().to_owned());
            if !matches!(oid.0.len(), 40 | 64)
                || !oid.0.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(AppError::validation(
                    "repair_revision_invalid",
                    "Git returned an invalid full repair object ID",
                ));
            }
            Ok(Some(oid))
        }
        Some(1) => Ok(None),
        _ => Err(repair_command_failure(
            "repair_revision_invalid",
            "could not inspect an optional repair revision",
            &output,
            json!({"revision": revision}),
        )),
    }
}

fn rev_parse(runner: &impl CommandRunner, revision: &str) -> Result<CommitOid, AppError> {
    let output = require_success(
        runner,
        CommandSpec::new("git").args(["rev-parse", "--verify", revision]),
        "repair_revision_invalid",
        "could not resolve an exact repair revision",
    )?;
    let oid = CommitOid(output.stdout.trim().to_owned());
    if !matches!(oid.0.len(), 40 | 64) || !oid.0.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::validation(
            "repair_revision_invalid",
            "Git returned an invalid full repair object ID",
        ));
    }
    Ok(oid)
}

#[derive(Debug)]
struct RepairPaths {
    root: PathBuf,
    session_root: PathBuf,
    workspace: PathBuf,
    manifest: PathBuf,
}

fn repair_paths(repository: &Path, pr: PrNumber) -> Result<RepairPaths, AppError> {
    repair_paths_for_session(repository, &format!("pr-{}", pr.0))
}

fn repair_paths_for_session(repository: &Path, session: &str) -> Result<RepairPaths, AppError> {
    let common = git_common_dir(repository)?;
    let root = common.join("caravan").join(REPAIR_DIRECTORY);
    // The canonical on-disk key is PR-scoped; a generation suffix in the
    // public session receipt still resolves to this one active session.
    let key = session.split('-').take(2).collect::<Vec<_>>().join("-");
    let session_root = root.join(&key);
    Ok(RepairPaths {
        workspace: session_root.join("worktree"),
        manifest: session_root.join(MANIFEST_NAME),
        root,
        session_root,
    })
}

fn canonical_repository_path(context: &AppContext) -> Result<String, AppError> {
    fs::canonicalize(&context.repository_path)
        .map(|path| path.display().to_string())
        .map_err(|error| {
            repair_io_error(
                "repair_object_cache_invalid",
                "could not canonicalize the repair object-cache checkout",
                &context.repository_path,
                &error,
            )
        })
}

fn canonical_git_common_dir(context: &AppContext) -> Result<String, AppError> {
    let path = git_common_dir(&context.repository_path)?;
    fs::canonicalize(&path)
        .map(|canonical| canonical.display().to_string())
        .map_err(|error| {
            repair_io_error(
                "repair_object_cache_invalid",
                "could not canonicalize the repair object-cache Git metadata",
                &path,
                &error,
            )
        })
}

fn verify_object_cache_identity(repair: &RepairSession) -> Result<(), AppError> {
    if repair.object_cache_path.is_empty() || repair.object_cache_common_dir.is_empty() {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_object_cache_missing",
            "repair session does not identify a canonical object-cache checkout",
            Some(json!({
                "repair": repair,
                "next": "rerun the exact repair start from the intended provider repository checkout",
            })),
        ));
    }
    let cache = fs::canonicalize(&repair.object_cache_path).map_err(|error| {
        repair_io_error(
            "repair_object_cache_invalid",
            "could not resolve the manifested object-cache checkout",
            Path::new(&repair.object_cache_path),
            &error,
        )
    })?;
    let common = fs::canonicalize(git_common_dir(&cache)?).map_err(|error| {
        repair_io_error(
            "repair_object_cache_invalid",
            "could not resolve the manifested object-cache Git metadata",
            Path::new(&repair.object_cache_common_dir),
            &error,
        )
    })?;
    if common.display().to_string() != repair.object_cache_common_dir {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_object_cache_changed",
            "repair object-cache Git identity changed after session preparation",
            Some(json!({
                "manifested_common_dir": repair.object_cache_common_dir,
                "actual_common_dir": common,
                "provider_mutated": false,
            })),
        ));
    }
    Ok(())
}

fn partial_workspace_ready(workspace: &Path, provider_git_url: &str) -> bool {
    let runner = ProcessRunner::in_directory(workspace).with_timeout(Duration::from_secs(10));
    let repository =
        runner.run(&CommandSpec::new("git").args(["rev-parse", "--is-inside-work-tree"]));
    if !repository.is_ok_and(|output| output.is_success() && output.stdout.trim() == "true") {
        return false;
    }
    runner
        .run(&CommandSpec::new("git").args(["remote", "get-url", "origin"]))
        .is_ok_and(|output| output.is_success() && output.stdout.trim() == provider_git_url)
}

fn fetch_exact_materialization(
    runner: &impl CommandRunner,
    provider_git_url: &str,
    snapshot: &BranchSnapshot,
) -> Result<(), AppError> {
    let expected = remote_head_oid(runner, provider_git_url, &snapshot.name)?;
    if expected != snapshot.oid {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_stale_head",
            "provider branch moved before exact repair materialization",
            Some(json!({
                "branch": snapshot.name,
                "expected_oid": snapshot.oid,
                "actual_oid": expected,
                "provider_mutated": false,
            })),
        ));
    }
    let reference = format!("refs/heads/{}", snapshot.name);
    require_success(
        runner,
        CommandSpec::new("git").args(["check-ref-format", reference.as_str()]),
        "repair_branch_invalid",
        "provider returned an invalid repair branch name",
    )?;
    let fetch = run(
        runner,
        CommandSpec::new("git")
            .args([
                "-c",
                "protocol.version=2",
                "-c",
                "core.sshCommand=ssh -oBatchMode=yes -oConnectTimeout=20 -oServerAliveInterval=15 -oServerAliveCountMax=3",
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-write-fetch-head",
                "--refmap=",
                "--filter=blob:none",
                "origin",
                reference.as_str(),
            ])
            .env("GIT_TERMINAL_PROMPT", "0"),
    )?;
    if !fetch.is_success() {
        let transport_disconnect = is_provider_transport_disconnect(&fetch.stderr);
        return Err(repair_command_failure(
            if transport_disconnect {
                "repair_provider_transport_disconnect"
            } else {
                "repair_exact_fetch_failed"
            },
            if transport_disconnect {
                "provider transport disconnected during resumable exact-object fetch"
            } else {
                "could not fetch the exact repair branch into the resumable object cache"
            },
            &fetch,
            json!({
                "branch": snapshot.name,
                "expected_oid": snapshot.oid,
                "resumable_in_place": true,
                "provider_mutated": false,
            }),
        ));
    }
    let after = remote_head_oid(runner, provider_git_url, &snapshot.name)?;
    if after != snapshot.oid {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_stale_head",
            "provider branch moved during exact repair materialization",
            Some(json!({
                "branch": snapshot.name,
                "expected_oid": snapshot.oid,
                "actual_oid": after,
                "provider_mutated": false,
            })),
        ));
    }
    let commit = format!("{}^{{commit}}", snapshot.oid.0);
    require_success(
        runner,
        CommandSpec::new("git").args(["cat-file", "-e", commit.as_str()]),
        "repair_exact_object_missing",
        "exact repair commit was not materialized after provider fetch",
    )?;
    Ok(())
}

fn is_provider_transport_disconnect(stderr: &str) -> bool {
    stderr.contains("unexpected disconnect")
        || stderr.contains("fetch-pack:")
        || stderr.contains("remote end hung up unexpectedly")
}

fn git_common_dir(repository: &Path) -> Result<PathBuf, AppError> {
    let runner = ProcessRunner::in_directory(repository);
    let output = require_success(
        &runner,
        CommandSpec::new("git").args(["rev-parse", "--path-format=absolute", "--git-common-dir"]),
        "repair_git_common_dir_failed",
        "could not resolve Git's common metadata directory",
    )?;
    let path = PathBuf::from(output.stdout.trim());
    if !path.is_absolute() {
        return Err(AppError::validation(
            "repair_git_common_dir_invalid",
            "Git returned a non-absolute common metadata directory",
        ));
    }
    Ok(path)
}

fn validate_manifest_path(paths: &RepairPaths, repair: &RepairSession) -> Result<(), AppError> {
    if repair.version != REPAIR_VERSION
        || repair.workspace != paths.workspace.display().to_string()
        || !paths.workspace.starts_with(&paths.root)
    {
        return Err(AppError::validation(
            "repair_manifest_invalid",
            "repair manifest does not match the canonical managed workspace",
        ));
    }
    validate_provider_git_url(&repair.repository, &repair.provider_git_url)
}

fn validate_partial_workspace_path(
    paths: &RepairPaths,
    repair: &RepairSession,
) -> Result<(), AppError> {
    validate_manifest_path(paths, repair)?;
    let metadata = fs::symlink_metadata(&paths.workspace).map_err(|error| {
        repair_io_error(
            "repair_workspace_missing",
            "managed repair workspace is unavailable",
            &paths.workspace,
            &error,
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::validation(
            "repair_workspace_invalid",
            "managed repair workspace must be a real directory, not a symlink",
        ));
    }
    Ok(())
}

fn validate_workspace(paths: &RepairPaths, repair: &RepairSession) -> Result<(), AppError> {
    validate_manifest_path(paths, repair)?;
    let metadata = fs::symlink_metadata(&paths.workspace).map_err(|error| {
        repair_io_error(
            "repair_workspace_missing",
            "managed repair workspace is unavailable",
            &paths.workspace,
            &error,
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::validation(
            "repair_workspace_invalid",
            "managed repair workspace must be a real directory, not a symlink",
        ));
    }
    Ok(())
}

fn config_fingerprint(context: &AppContext) -> Result<String, AppError> {
    let encoded = serde_json::to_vec(&context.config).map_err(|error| {
        AppError::structured(
            ErrorCategory::SerializationError,
            "repair_config_fingerprint_failed",
            format!("could not encode Caravan config for repair evidence: {error}"),
            None,
        )
    })?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(&encoded);
    Ok(format!("{:016x}", hasher.finish()))
}

fn provider_git_url(context: &AppContext, repository: &RepositoryId) -> Result<String, AppError> {
    let runner = ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(Duration::from_secs(context.config.command_timeout_secs));
    let slug = format!("{}/{}", repository.owner, repository.name);
    let output = require_success(
        &runner,
        CommandSpec::new("gh").args([
            "repo",
            "view",
            slug.as_str(),
            "--json",
            "sshUrl",
            "--jq",
            ".sshUrl",
        ]),
        "repair_provider_url_failed",
        "could not resolve the explicit provider-owned Git URL",
    )?;
    let url = output.stdout.trim().to_owned();
    validate_provider_git_url(repository, &url)?;
    Ok(url)
}

fn validate_provider_git_url(repository: &RepositoryId, url: &str) -> Result<(), AppError> {
    #[cfg(test)]
    if Path::new(url).is_absolute() {
        return Ok(());
    }
    let path = format!("{}/{}.git", repository.owner, repository.name);
    let allowed = [
        format!("git@github.com:{path}"),
        format!("ssh://git@github.com/{path}"),
        format!("https://github.com/{path}"),
    ];
    if url.starts_with('-')
        || url.contains(['\n', '\r', '\0'])
        || !allowed.iter().any(|candidate| candidate == url)
    {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_provider_url_invalid",
            "repair provider URL does not exactly match the manifested GitHub repository",
            Some(json!({"repository": repository, "url": url})),
        ));
    }
    Ok(())
}

fn absolute_config_path(context: &AppContext) -> String {
    let path = if context.config_path.is_absolute() {
        context.config_path.clone()
    } else {
        context.repository_path.join(&context.config_path)
    };
    path.display().to_string()
}

fn write_manifest(path: &Path, repair: &RepairSession) -> Result<(), AppError> {
    let parent = path.parent().expect("manifest path has parent");
    fs::create_dir_all(parent).map_err(|error| {
        repair_io_error(
            "repair_manifest_write_failed",
            "could not create repair manifest directory",
            parent,
            &error,
        )
    })?;
    let temporary = path.with_extension("json.tmp");
    let encoded = serde_json::to_vec_pretty(repair).map_err(|error| {
        AppError::structured(
            ErrorCategory::SerializationError,
            "repair_manifest_encode_failed",
            format!("could not encode repair manifest: {error}"),
            None,
        )
    })?;
    fs::write(&temporary, encoded).map_err(|error| {
        repair_io_error(
            "repair_manifest_write_failed",
            "could not write repair manifest",
            &temporary,
            &error,
        )
    })?;
    fs::rename(&temporary, path).map_err(|error| {
        repair_io_error(
            "repair_manifest_write_failed",
            "could not atomically publish repair manifest",
            path,
            &error,
        )
    })
}

fn read_manifest(path: &Path) -> Result<RepairSession, AppError> {
    let bytes = fs::read(path).map_err(|error| {
        repair_io_error(
            "repair_session_not_found",
            "could not read the requested repair session",
            path,
            &error,
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        AppError::structured(
            ErrorCategory::SerializationError,
            "repair_manifest_invalid",
            format!("could not decode repair manifest: {error}"),
            Some(json!({"path": path})),
        )
    })
}

fn cleanup_workspace(paths: &RepairPaths) -> Result<(), AppError> {
    fs::remove_dir_all(&paths.session_root).map_err(|error| {
        repair_io_error(
            "repair_workspace_cleanup_failed",
            "repair converged but session metadata could not be removed",
            &paths.session_root,
            &error,
        )
    })
}

fn require_session_match(repair: &RepairSession, requested: &str) -> Result<(), AppError> {
    if repair.session == requested {
        return Ok(());
    }
    Err(AppError::structured(
        ErrorCategory::Validation,
        "repair_session_generation_mismatch",
        "requested repair session does not match the active exact generation",
        Some(json!({
            "requested": requested,
            "active": repair.session,
            "pr": repair.pr,
        })),
    ))
}

fn validate_session_id(session: &str) -> Result<(), AppError> {
    let valid = session.starts_with("pr-")
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if !valid {
        return Err(AppError::validation(
            "repair_session_id_invalid",
            "repair session IDs must use the returned `pr-<number>-<generation>` form",
        ));
    }
    Ok(())
}

fn short_oid(oid: &CommitOid) -> &str {
    oid.0.get(..12).unwrap_or(&oid.0)
}

fn unix_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn attach_repair_resume(
    error: &AppError,
    repair: &RepairSession,
    publication: Option<&RepairPublicationReceipt>,
) -> AppError {
    let mut details = error.details().unwrap_or_else(|| json!({}));
    if let Some(object) = details.as_object_mut() {
        object.insert("repair".to_owned(), json!(repair));
        object.insert("publication".to_owned(), json!(publication));
        object.insert("workspace_preserved".to_owned(), json!(true));
        object.insert(
            "next".to_owned(),
            json!("inspect the typed sync decision in the managed workspace, then rerun `cara repair continue --session <id>`; never use raw update-ref or a nested worktree"),
        );
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

#[allow(clippy::needless_pass_by_value)]
fn run(runner: &impl CommandRunner, command: CommandSpec) -> Result<CommandOutput, AppError> {
    runner.run(&command).map_err(|error| match &error {
        CommandRunError::Timeout {
            process_group_id,
            timeout_ms,
            stdout,
            stderr,
            ..
        } => AppError::structured(
            ErrorCategory::Timeout,
            "repair_command_timeout",
            error.to_string(),
            Some(json!({
                "command": command.display(),
                "process_group_id": process_group_id,
                "timeout_ms": timeout_ms,
                "stdout": bounded(stdout),
                "stderr": bounded(stderr),
                "process_group_reaped": true,
            })),
        ),
        _ => AppError::structured(
            ErrorCategory::ExecutionFailure,
            "repair_command_failed",
            "could not run an isolated repair command",
            Some(json!({"command": command.display(), "source": error.to_string()})),
        ),
    })
}

#[allow(clippy::needless_pass_by_value)]
fn require_success(
    runner: &impl CommandRunner,
    command: CommandSpec,
    code: &'static str,
    message: &'static str,
) -> Result<CommandOutput, AppError> {
    let output = run(runner, command.clone())?;
    if output.is_success() {
        Ok(output)
    } else {
        Err(repair_command_failure(
            code,
            message,
            &output,
            json!({"command": command.display()}),
        ))
    }
}

fn repair_command_failure(
    code: &'static str,
    message: &'static str,
    output: &CommandOutput,
    mut details: Value,
) -> AppError {
    if let Some(object) = details.as_object_mut() {
        object.insert("exit_code".to_owned(), json!(output.code));
        object.insert("stdout".to_owned(), json!(bounded(&output.stdout)));
        object.insert("stderr".to_owned(), json!(bounded(&output.stderr)));
    }
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        code,
        message,
        Some(details),
    )
}

fn bounded(text: &str) -> String {
    const LIMIT: usize = 4096;
    text.chars().take(LIMIT).collect()
}

fn repair_io_error(
    code: &'static str,
    message: &'static str,
    path: &Path,
    error: &std::io::Error,
) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        code,
        format!("{message}: {error}"),
        Some(json!({"path": path})),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::process::Command;

    use super::*;
    use crate::config::CaravanConfig;
    use crate::model::{AutoMergeState, PullRequestState};

    fn git(directory: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(directory)
            .args(arguments)
            .output()
            .expect("run git fixture command");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("UTF-8 git output")
            .trim()
            .to_owned()
    }

    struct Fixture {
        root: tempfile::TempDir,
        clone: PathBuf,
        repository: RepositoryId,
        candidate: PullRequestSnapshot,
        target: BranchSnapshot,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("remote.git");
        git(root.path(), &["init", "--bare", bare.to_str().unwrap()]);
        let clone = root.path().join("clone");
        git(
            root.path(),
            &["clone", bare.to_str().unwrap(), clone.to_str().unwrap()],
        );
        git(&clone, &["config", "user.name", "Cara Repair Test"]);
        git(&clone, &["config", "user.email", "cara@example.invalid"]);
        git(&clone, &["checkout", "-b", "main"]);
        fs::write(clone.join("shared.txt"), "base\n").unwrap();
        fs::write(clone.join("stable.txt"), "stable\n").unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-m", "base"]);
        git(&clone, &["push", "-u", "origin", "main"]);
        let base = CommitOid(git(&clone, &["rev-parse", "HEAD"]));

        git(&clone, &["checkout", "-b", "feature"]);
        fs::write(clone.join("shared.txt"), "feature\n").unwrap();
        git(&clone, &["add", "shared.txt"]);
        git(&clone, &["commit", "-m", "feature"]);
        git(&clone, &["push", "-u", "origin", "feature"]);
        let head = CommitOid(git(&clone, &["rev-parse", "HEAD"]));

        git(&clone, &["checkout", "main"]);
        fs::write(clone.join("shared.txt"), "target\n").unwrap();
        fs::write(clone.join("target.txt"), "target\n").unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-m", "target"]);
        git(&clone, &["push", "origin", "main"]);
        let target_oid = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["checkout", "feature"]);
        fs::write(clone.join("dirty-controller"), "unrelated\n").unwrap();

        let repository = RepositoryId {
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        };
        let branch = |name: &str, oid: CommitOid| BranchSnapshot {
            repository: repository.clone(),
            name: name.to_owned(),
            oid,
        };
        let candidate = PullRequestSnapshot {
            number: PrNumber(7),
            title: "candidate".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch("feature", head),
            base: branch("main", base),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let target = branch("main", target_oid);
        Fixture {
            root,
            clone,
            repository,
            candidate,
            target,
        }
    }

    fn context(path: &Path) -> AppContext {
        AppContext {
            repository_path: path.to_path_buf(),
            config_path: path.join(".caravan/config.yaml"),
            config_existed: false,
            config: CaravanConfig::default(),
        }
    }

    #[test]
    fn dirty_caller_gets_isolated_exact_head_workspace() {
        let fixture = fixture();
        // Prove both caller-local divergence and an unusable daemon/internal
        // origin are irrelevant to the explicit provider-owned workspace.
        git(&fixture.clone, &["add", "dirty-controller"]);
        git(
            &fixture.clone,
            &["commit", "-m", "local-only controller state"],
        );
        git(
            &fixture.clone,
            &["remote", "set-url", "origin", "/invalid/internal/remote"],
        );
        fs::write(fixture.clone.join("still-dirty"), "untracked\n").unwrap();
        let before_head = git(&fixture.clone, &["rev-parse", "HEAD"]);
        let before_status = git(&fixture.clone, &["status", "--porcelain"]);
        let output = start_exact(
            &context(&fixture.clone),
            &fixture.repository,
            &fixture.candidate,
            &fixture.target,
            None,
            fixture.root.path().join("remote.git").to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(output.repair.state, RepairState::Resolving);
        assert_eq!(output.repair.conflicting_paths, ["shared.txt"]);
        assert_eq!(git(&fixture.clone, &["rev-parse", "HEAD"]), before_head);
        assert_eq!(
            git(&fixture.clone, &["status", "--porcelain"]),
            before_status
        );
        assert!(fixture.clone.join("dirty-controller").exists());
        assert!(fixture.clone.join("still-dirty").exists());
        let workspace = Path::new(&output.repair.workspace);
        assert!(workspace.join("shared.txt").exists());
        assert_eq!(
            git(workspace, &["remote", "get-url", "cache"]),
            fs::canonicalize(&fixture.clone)
                .unwrap()
                .display()
                .to_string()
        );
        assert_eq!(
            git(workspace, &["remote", "get-url", "origin"]),
            fixture.root.path().join("remote.git").display().to_string()
        );
    }

    #[test]
    fn object_cache_git_identity_drift_is_rejected() {
        let fixture = fixture();
        let context = context(&fixture.clone);
        let output = start_exact(
            &context,
            &fixture.repository,
            &fixture.candidate,
            &fixture.target,
            None,
            fixture.root.path().join("remote.git").to_str().unwrap(),
        )
        .unwrap();
        let mut repair = output.repair;
        repair.object_cache_common_dir = "/different/git/common-dir".to_owned();
        let error = verify_object_cache_identity(&repair).unwrap_err();
        assert_eq!(error.code(), "repair_object_cache_changed");
    }

    #[test]
    fn exact_preparing_manifest_without_workspace_resumes_materialization() {
        let fixture = fixture();
        let context = context(&fixture.clone);
        let output = start_exact(
            &context,
            &fixture.repository,
            &fixture.candidate,
            &fixture.target,
            None,
            fixture.root.path().join("remote.git").to_str().unwrap(),
        )
        .unwrap();
        let paths = repair_paths(&fixture.clone, fixture.candidate.number).unwrap();
        fs::remove_dir_all(&paths.workspace).unwrap();
        let mut preparing = output.repair;
        preparing.state = RepairState::Preparing;
        preparing.phase = RepairPhase::Cloning;
        preparing.last_error = Some(RepairPhaseError {
            phase: RepairPhase::Cloning,
            code: "repair_command_timeout".to_owned(),
            message: "simulated timeout".to_owned(),
            elapsed_ms: 30_000,
            timeout_ms: 30_000,
            process_group_id: Some(42),
            partial_path: paths.workspace.display().to_string(),
            next: "resume".to_owned(),
        });
        write_manifest(&paths.manifest, &preparing).unwrap();
        let visible = status(
            &context,
            &RepairStatusInput {
                session: preparing.session.clone(),
            },
        )
        .expect("preparing status remains inspectable without a workspace");
        assert_eq!(visible.phase, RepairPhase::Cloning);
        assert_eq!(
            visible.last_error.as_ref().map(|error| error.code.as_str()),
            Some("repair_command_timeout")
        );

        let resumed = start_exact(
            &context,
            &fixture.repository,
            &fixture.candidate,
            &fixture.target,
            None,
            fixture.root.path().join("remote.git").to_str().unwrap(),
        )
        .unwrap();
        assert!(resumed.already_exists);
        assert_eq!(resumed.repair.state, RepairState::Resolving);
        assert_eq!(resumed.repair.phase, RepairPhase::Resolving);
        assert!(resumed.repair.last_error.is_none());
        assert!(paths.workspace.exists());
    }

    #[test]
    fn exact_resume_reuses_valid_partial_repository_objects_in_place() {
        let fixture = fixture();
        let context = context(&fixture.clone);
        let output = start_exact(
            &context,
            &fixture.repository,
            &fixture.candidate,
            &fixture.target,
            None,
            fixture.root.path().join("remote.git").to_str().unwrap(),
        )
        .unwrap();
        let paths = repair_paths(&fixture.clone, fixture.candidate.number).unwrap();
        git(&paths.workspace, &["merge", "--abort"]);
        let marker = paths.workspace.join(".partial-object-resume-proof");
        fs::write(&marker, "preserved\n").unwrap();
        let mut preparing = output.repair;
        preparing.state = RepairState::Preparing;
        preparing.phase = RepairPhase::FetchingHead;
        preparing.last_error = Some(RepairPhaseError {
            phase: RepairPhase::FetchingHead,
            code: "repair_command_timeout".to_owned(),
            message: "simulated sideband disconnect".to_owned(),
            elapsed_ms: 180_000,
            timeout_ms: 180_000,
            process_group_id: Some(42),
            partial_path: paths.workspace.display().to_string(),
            next: "resume".to_owned(),
        });
        write_manifest(&paths.manifest, &preparing).unwrap();

        let resumed = start_exact(
            &context,
            &fixture.repository,
            &fixture.candidate,
            &fixture.target,
            None,
            fixture.root.path().join("remote.git").to_str().unwrap(),
        )
        .unwrap();
        assert!(resumed.already_exists);
        assert_eq!(resumed.repair.state, RepairState::Resolving);
        assert!(
            marker.exists(),
            "valid partial repository was recloned instead of resumed"
        );
    }

    #[test]
    fn materialization_timeout_persists_phase_budget_and_process_group() {
        let fixture = fixture();
        let context = context(&fixture.clone);
        let output = start_exact(
            &context,
            &fixture.repository,
            &fixture.candidate,
            &fixture.target,
            None,
            fixture.root.path().join("remote.git").to_str().unwrap(),
        )
        .unwrap();
        let paths = repair_paths(&fixture.clone, fixture.candidate.number).unwrap();
        let mut repair = output.repair;
        repair.state = RepairState::Preparing;
        let timeout = Duration::from_millis(100);
        let runner = ProcessRunner::new().with_timeout(timeout);
        let error =
            run_materialization_phase(&mut repair, &paths, RepairPhase::Cloning, timeout, || {
                run(&runner, CommandSpec::new("sh").args(["-c", "sleep 30"]))
            })
            .unwrap_err();
        assert_eq!(error.category(), ErrorCategory::Timeout);
        let persisted = read_manifest(&paths.manifest).unwrap();
        let evidence = persisted.last_error.expect("durable phase error");
        assert_eq!(evidence.phase, RepairPhase::Cloning);
        assert_eq!(evidence.code, "repair_command_timeout");
        assert_eq!(evidence.timeout_ms, 100);
        assert!(evidence.elapsed_ms >= 100);
        assert!(evidence.process_group_id.is_some());
        assert_eq!(evidence.partial_path, paths.workspace.display().to_string());
    }

    #[test]
    fn continue_rejects_changes_outside_typed_conflicts() {
        let fixture = fixture();
        let output = start_exact(
            &context(&fixture.clone),
            &fixture.repository,
            &fixture.candidate,
            &fixture.target,
            None,
            fixture.root.path().join("remote.git").to_str().unwrap(),
        )
        .unwrap();
        let workspace = PathBuf::from(&output.repair.workspace);
        fs::write(workspace.join("shared.txt"), "resolved\n").unwrap();
        fs::write(workspace.join("stable.txt"), "unrelated\n").unwrap();
        git(&workspace, &["add", "shared.txt", "stable.txt"]);
        let runner = ProcessRunner::in_directory(&workspace);
        let error = verify_resolution(&runner, &output.repair).unwrap_err();
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "repair_scope_changed"
        );
    }

    #[test]
    fn committed_before_manifest_checkpoint_resumes_without_duplicate_commit() {
        let fixture = fixture();
        let context = context(&fixture.clone);
        let output = start_exact(
            &context,
            &fixture.repository,
            &fixture.candidate,
            &fixture.target,
            None,
            fixture.root.path().join("remote.git").to_str().unwrap(),
        )
        .unwrap();
        let workspace = PathBuf::from(&output.repair.workspace);
        fs::write(workspace.join("shared.txt"), "resolved after restart\n").unwrap();
        git(&workspace, &["add", "shared.txt"]);
        git(
            &workspace,
            &[
                "-c",
                "user.name=Caravan Repair",
                "-c",
                "user.email=caravan-repair@users.noreply.github.com",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "-m",
                "simulated interrupted repair commit",
            ],
        );
        let committed = git(&workspace, &["rev-parse", "HEAD"]);

        let result = continue_session(
            &context,
            &RepairContinueInput {
                session: output.repair.session,
                no_sync: true,
            },
        )
        .unwrap();
        let receipt = result.publication.expect("publication receipt");
        assert_eq!(receipt.new_head.0, committed);
        assert_eq!(
            git(
                &workspace,
                &[
                    "rev-list",
                    "--first-parent",
                    "--count",
                    &format!("{}..HEAD", fixture.candidate.head.oid.0),
                ],
            ),
            "1"
        );
    }

    #[test]
    fn moved_remote_head_cannot_be_overwritten() {
        let fixture = fixture();
        let context = context(&fixture.clone);
        let output = start_exact(
            &context,
            &fixture.repository,
            &fixture.candidate,
            &fixture.target,
            None,
            fixture.root.path().join("remote.git").to_str().unwrap(),
        )
        .unwrap();
        let workspace = PathBuf::from(&output.repair.workspace);
        fs::write(workspace.join("shared.txt"), "resolved\n").unwrap();
        git(&workspace, &["add", "shared.txt"]);

        fs::remove_file(fixture.clone.join("dirty-controller")).unwrap();
        fs::write(fixture.clone.join("remote-race"), "race\n").unwrap();
        git(&fixture.clone, &["add", "remote-race"]);
        git(
            &fixture.clone,
            &["commit", "-m", "concurrent provider move"],
        );
        git(&fixture.clone, &["push", "origin", "feature"]);
        let moved = git(&fixture.clone, &["rev-parse", "HEAD"]);

        let error = continue_session(
            &context,
            &RepairContinueInput {
                session: output.repair.session,
                no_sync: true,
            },
        )
        .unwrap_err();
        assert_eq!(mcp_cli::StructuredError::code(&error), "repair_stale_head");
        let remote = git(
            &fixture.clone,
            &["ls-remote", "origin", "refs/heads/feature"],
        );
        assert_eq!(remote.split_whitespace().next(), Some(moved.as_str()));
    }

    #[test]
    fn provider_sideband_disconnect_has_a_distinct_resumable_class() {
        assert!(is_provider_transport_disconnect(
            "fetch-pack: unexpected disconnect while reading sideband packet"
        ));
        assert!(is_provider_transport_disconnect(
            "fatal: the remote end hung up unexpectedly"
        ));
        assert!(!is_provider_transport_disconnect(
            "fatal: repository not found"
        ));
    }

    #[test]
    fn abort_requires_confirmation_and_never_mutates_provider() {
        let fixture = fixture();
        let context = context(&fixture.clone);
        let output = start_exact(
            &context,
            &fixture.repository,
            &fixture.candidate,
            &fixture.target,
            None,
            fixture.root.path().join("remote.git").to_str().unwrap(),
        )
        .unwrap();
        let session = output.repair.session;
        let workspace = PathBuf::from(&output.repair.workspace);
        let denied = abort(
            &context,
            &RepairAbortInput {
                session: session.clone(),
                confirm: false,
            },
        )
        .unwrap_err();
        assert_eq!(
            mcp_cli::StructuredError::code(&denied),
            "repair_abort_confirmation_required"
        );
        assert!(workspace.exists());

        let receipt = abort(
            &context,
            &RepairAbortInput {
                session,
                confirm: true,
            },
        )
        .unwrap();
        assert!(receipt.workspace_removed);
        assert!(!receipt.provider_mutated);
        assert!(!workspace.exists());
        let remote = git(
            &fixture.clone,
            &["ls-remote", "origin", "refs/heads/feature"],
        );
        assert_eq!(
            remote.split_whitespace().next(),
            Some(fixture.candidate.head.oid.0.as_str())
        );
    }

    #[test]
    fn verified_resolution_has_exact_merge_parents_and_plain_pushes() {
        let fixture = fixture();
        let context = context(&fixture.clone);
        let output = start_exact(
            &context,
            &fixture.repository,
            &fixture.candidate,
            &fixture.target,
            None,
            fixture.root.path().join("remote.git").to_str().unwrap(),
        )
        .unwrap();
        let workspace = PathBuf::from(&output.repair.workspace);
        fs::write(
            workspace.join("shared.txt"),
            "resolved feature and target\n",
        )
        .unwrap();
        git(&workspace, &["add", "shared.txt"]);
        let result = continue_session(
            &context,
            &RepairContinueInput {
                session: output.repair.session,
                no_sync: true,
            },
        )
        .unwrap();
        let receipt = result.publication.expect("publication receipt");
        assert!(!receipt.force);
        assert!(receipt.remote_verified);
        assert_eq!(
            receipt.parents,
            [fixture.candidate.head.oid, fixture.target.oid]
        );
        assert_eq!(
            git(
                &workspace,
                &[
                    "show",
                    "-s",
                    "--format=%an <%ae>|%cn <%ce>",
                    receipt.new_head.0.as_str(),
                ],
            ),
            "Caravan Repair <caravan-repair@users.noreply.github.com>|Caravan Repair <caravan-repair@users.noreply.github.com>"
        );
        let remote_head = git(
            &fixture.clone,
            &["ls-remote", "origin", "refs/heads/feature"],
        );
        assert_eq!(
            remote_head.split_whitespace().next(),
            Some(receipt.new_head.0.as_str())
        );
        assert!(fixture.clone.join("dirty-controller").exists());
    }
}
