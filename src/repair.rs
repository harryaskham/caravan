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
const MAX_SEMANTIC_GRANTS: usize = 8;
const MAX_GRANT_ACTOR_BYTES: usize = 128;
const MAX_GRANT_REASON_BYTES: usize = 512;
const MAX_GRANT_EXPIRY_SECS: u64 = 24 * 60 * 60;
const MAX_AGENT_EDIT_PATHS: usize = 64;
const MAX_AGENT_EDIT_PATH_BYTES: usize = 4 * 1024;
const MAX_AGENT_EDIT_DIFF_BYTES: usize = 1024 * 1024;

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
    /// Exact authorized agent identity when session-level edits were used.
    #[arg(long)]
    #[serde(default)]
    pub actor: Option<String>,
    /// Publish the repair but do not immediately resume `sync --all`.
    #[arg(long)]
    #[serde(default)]
    pub no_sync: bool,
}

/// Apply reviewed semantic source changes to exact paths in one repair session.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, clap::Args)]
pub struct RepairGrantInput {
    /// Exact session ID returned by repair start/status.
    #[arg(long)]
    pub session: String,
    /// Repository-relative tracked path; repeat for a bounded reviewed set.
    #[arg(long = "path", required = true)]
    pub paths: Vec<String>,
    /// Reviewed source commit containing the semantic path changes.
    #[arg(long)]
    pub source_revision: String,
    /// Audited operator/agent identity authorizing the semantic restoration.
    #[arg(long)]
    pub actor: String,
    /// Bounded reason/source-contract evidence.
    #[arg(long)]
    pub reason: String,
    /// Grant validity window; continue fails after expiry.
    #[arg(long, default_value_t = 3600)]
    #[serde(default = "default_grant_expiry_secs")]
    pub expires_secs: u64,
}

fn default_grant_expiry_secs() -> u64 {
    3600
}

/// Authorize one audited agent to make bounded arbitrary repository edits.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, clap::Args)]
pub struct RepairAuthorizeAgentEditsInput {
    #[arg(long)]
    pub session: String,
    #[arg(long)]
    pub actor: String,
    #[arg(long)]
    pub reason: String,
    #[arg(long, default_value_t = 3600)]
    #[serde(default = "default_grant_expiry_secs")]
    pub expires_secs: u64,
}

/// Exact session-level authority for agent-owned repository edits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepairAgentEditAuthorization {
    pub session: String,
    pub repository: RepositoryId,
    pub pr: PrNumber,
    pub head_oid: CommitOid,
    pub target_oid: CommitOid,
    pub config_fingerprint: String,
    pub manifest_fingerprint: String,
    pub actor: String,
    pub reason: String,
    pub authorized_unix_ms: u64,
    pub expires_unix_ms: u64,
}

/// Complete bounded file scope and content fingerprint verified before commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepairAgentEditReceipt {
    pub actor: String,
    pub reason: String,
    pub paths: Vec<String>,
    pub path_count: usize,
    pub path_fingerprint: String,
    pub diff_fingerprint: String,
    pub diff_bytes: usize,
    pub staged_index_fingerprint: String,
    pub verified_unix_ms: u64,
    pub fresh_ci_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepairAuthorizeAgentEditsOutput {
    pub repair: RepairStatusOutput,
    pub authorization: RepairAgentEditAuthorization,
    pub already_authorized: bool,
    pub provider_mutated: bool,
    pub next: String,
}

/// Revoke exact semantic grants and restore their pre-grant staged objects.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, clap::Args)]
pub struct RepairRevokeGrantInput {
    #[arg(long)]
    pub session: String,
    #[arg(long = "path", required = true)]
    pub paths: Vec<String>,
    #[arg(long)]
    pub actor: String,
    #[arg(long)]
    pub reason: String,
}

/// Local-only semantic grant revocation receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepairRevokeGrantOutput {
    pub repair: RepairStatusOutput,
    pub revoked_paths: Vec<String>,
    pub provider_mutated: bool,
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

/// Exact reviewed semantic-path authorization and resulting staged object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepairPathGrant {
    pub path: String,
    pub actor: String,
    pub reason: String,
    pub source_revision: CommitOid,
    pub source_parent: CommitOid,
    pub source_blob: CommitOid,
    pub source_parent_blob: CommitOid,
    pub source_patch_fingerprint: String,
    pub baseline_oid: String,
    pub expected_result_oid: String,
    pub granted_unix_ms: u64,
    pub expires_unix_ms: u64,
    pub applied: bool,
}

/// Durable proof that one exact semantic grant was restored and revoked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepairPathGrantRevocation {
    pub path: String,
    pub actor: String,
    pub reason: String,
    pub source_revision: CommitOid,
    pub baseline_oid: String,
    pub expected_result_oid: String,
    pub revoked_unix_ms: u64,
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
    /// Narrow edits use conflicts/grants; broad edits require exact session-level authorization.
    #[serde(default)]
    pub baseline_index: BTreeMap<String, String>,
    #[serde(default)]
    pub semantic_grants: Vec<RepairPathGrant>,
    #[serde(default)]
    pub semantic_grant_revocations: Vec<RepairPathGrantRevocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_edit_authorization: Option<RepairAgentEditAuthorization>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_edit_receipt: Option<RepairAgentEditReceipt>,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_head: Option<CommitOid>,
}

/// Bounded read-only projection of a potentially large repair manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepairStatusOutput {
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
    pub provider_git_url: String,
    pub object_cache_path: String,
    pub object_cache_common_dir: String,
    pub state: RepairState,
    pub phase: RepairPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<RepairPhaseError>,
    #[serde(default)]
    pub conflicting_paths: Vec<String>,
    pub baseline_index_count: usize,
    pub baseline_index_fingerprint: String,
    #[serde(default)]
    pub semantic_grants: Vec<RepairPathGrant>,
    #[serde(default)]
    pub semantic_grant_revocations: Vec<RepairPathGrantRevocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_edit_authorization: Option<RepairAgentEditAuthorization>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_edit_receipt: Option<RepairAgentEditReceipt>,
    pub materialization_timeout_secs: u64,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_head: Option<CommitOid>,
}

/// Result of applying one bounded reviewed semantic-path grant set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepairGrantOutput {
    pub repair: RepairStatusOutput,
    pub grants: Vec<RepairPathGrant>,
    pub already_applied: bool,
    pub next: String,
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
    pub fresh_ci_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_edit_receipt: Option<RepairAgentEditReceipt>,
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
    lock.checkpoint(
        "repair_workspace_ready",
        repair_lock_receipt(&output.repair)?,
        false,
    )?;
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
                    if partial_workspace_ready(
                        &paths.workspace,
                        provider_git_url,
                        &resumed.object_cache_path,
                    ) {
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
        semantic_grants: Vec::new(),
        semantic_grant_revocations: Vec::new(),
        agent_edit_authorization: None,
        agent_edit_receipt: None,
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
    repair.baseline_index = staged_index(&workspace_runner)?;
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

fn repair_manifest_fingerprint(repair: &RepairSession) -> Result<String, AppError> {
    serde_json::to_vec(repair)
        .map(|encoded| stable_fingerprint(&encoded))
        .map_err(|error| {
            AppError::structured(
                ErrorCategory::SerializationError,
                "repair_manifest_fingerprint_failed",
                error.to_string(),
                None,
            )
        })
}

/// Authorize one exact agent identity to stage arbitrary bounded repository edits.
#[allow(clippy::too_many_lines)]
pub fn authorize_agent_edits(
    context: &AppContext,
    input: &RepairAuthorizeAgentEditsInput,
) -> Result<RepairAuthorizeAgentEditsOutput, AppError> {
    validate_session_id(&input.session)?;
    if input.actor.trim().is_empty()
        || input.actor.len() > MAX_GRANT_ACTOR_BYTES
        || input.reason.trim().is_empty()
        || input.reason.len() > MAX_GRANT_REASON_BYTES
        || !(1..=MAX_GRANT_EXPIRY_SECS).contains(&input.expires_secs)
    {
        return Err(AppError::validation(
            "repair_agent_edit_authorization_invalid",
            "authorization requires bounded non-empty actor/reason and an expiry within 24 hours",
        ));
    }
    let paths = repair_paths_for_session(&context.repository_path, &input.session)?;
    let mut repair = read_manifest(&paths.manifest)?;
    require_session_match(&repair, &input.session)?;
    validate_workspace(&paths, &repair)?;
    if repair.state != RepairState::Resolving {
        return Err(AppError::validation(
            "repair_agent_edit_state_invalid",
            "agent edits may be authorized only while the repair is resolving",
        ));
    }
    let current_fingerprint = config_fingerprint(context)?;
    if current_fingerprint != repair.config_fingerprint {
        return Err(AppError::validation(
            "repair_config_changed",
            "Caravan config changed after the repair session was prepared",
        ));
    }
    let mut lock =
        OperationLock::acquire(&context.repository_path, "repair-authorize-agent-edits")?;
    lock.checkpoint(
        "repair_agent_edit_authorization_preflight",
        repair_lock_receipt(&repair)?,
        false,
    )?;
    let runner = ProcessRunner::in_directory(&paths.workspace)
        .with_timeout(Duration::from_secs(context.config.command_timeout_secs));
    verify_remote_head(&runner, &repair.provider_git_url, &repair.head)?;
    verify_remote_head(&runner, &repair.provider_git_url, &repair.target)?;
    let now = unix_ms();
    if let Some(existing) = &repair.agent_edit_authorization
        && existing.actor == input.actor
        && existing.reason == input.reason
        && existing.expires_unix_ms >= now
        && existing.head_oid == repair.head.oid
        && existing.target_oid == repair.target.oid
        && existing.config_fingerprint == repair.config_fingerprint
    {
        let output = RepairAuthorizeAgentEditsOutput {
            repair: repair_status_output(&repair)?,
            authorization: existing.clone(),
            already_authorized: true,
            provider_mutated: false,
            next: "make and stage only reviewed repository-content edits in the managed workspace, then continue with the same --actor".to_owned(),
        };
        lock.release()?;
        return Ok(output);
    }
    if let Some(existing) = &repair.agent_edit_authorization
        && existing.expires_unix_ms >= now
    {
        return Err(AppError::structured(
            ErrorCategory::MissingPermission,
            "repair_agent_edit_authorization_conflict",
            "a different unexpired agent-edit authorization already exists",
            Some(json!({"authorization": existing})),
        ));
    }
    let authorization = RepairAgentEditAuthorization {
        session: repair.session.clone(),
        repository: repair.repository.clone(),
        pr: repair.pr,
        head_oid: repair.head.oid.clone(),
        target_oid: repair.target.oid.clone(),
        config_fingerprint: repair.config_fingerprint.clone(),
        manifest_fingerprint: repair_manifest_fingerprint(&repair)?,
        actor: input.actor.clone(),
        reason: input.reason.clone(),
        authorized_unix_ms: now,
        expires_unix_ms: now.saturating_add(input.expires_secs.saturating_mul(1000)),
    };
    repair.agent_edit_authorization = Some(authorization.clone());
    repair.agent_edit_receipt = None;
    repair.updated_unix_ms = unix_ms();
    write_manifest(&paths.manifest, &repair)?;
    lock.checkpoint(
        "repair_agent_edits_authorized",
        json!({
            "repair": repair_lock_receipt(&repair)?,
            "actor": authorization.actor,
            "expires_unix_ms": authorization.expires_unix_ms,
            "provider_mutated": false,
        }),
        false,
    )?;
    lock.release()?;
    Ok(RepairAuthorizeAgentEditsOutput {
        repair: repair_status_output(&repair)?,
        authorization,
        already_authorized: false,
        provider_mutated: false,
        next: "make and stage reviewed repository-content edits in the managed workspace, then run repair continue with the same --actor".to_owned(),
    })
}

fn validate_grant_text(input: &RepairGrantInput) -> Result<(), AppError> {
    if input.paths.is_empty() || input.paths.len() > MAX_SEMANTIC_GRANTS {
        return Err(AppError::validation(
            "repair_grant_path_count_invalid",
            format!("provide between 1 and {MAX_SEMANTIC_GRANTS} semantic paths"),
        ));
    }
    if input.actor.trim().is_empty()
        || input.actor.len() > MAX_GRANT_ACTOR_BYTES
        || input.reason.trim().is_empty()
        || input.reason.len() > MAX_GRANT_REASON_BYTES
    {
        return Err(AppError::validation(
            "repair_grant_audit_invalid",
            "semantic grant actor/reason must be non-empty and within bounded lengths",
        ));
    }
    if !(1..=MAX_GRANT_EXPIRY_SECS).contains(&input.expires_secs) {
        return Err(AppError::validation(
            "repair_grant_expiry_invalid",
            format!("semantic grant expiry must be between 1 and {MAX_GRANT_EXPIRY_SECS} seconds"),
        ));
    }
    Ok(())
}

fn validate_grant_path(
    workspace: &Path,
    runner: &impl CommandRunner,
    path: &str,
) -> Result<(), AppError> {
    let candidate = Path::new(path);
    let safe = !path.is_empty()
        && path.len() <= 512
        && !path.contains(['\n', '\r', '\0', ':'])
        && !candidate.is_absolute()
        && candidate
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && ![".git", ".github", ".cacophony", ".caravan"]
            .iter()
            .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")));
    if !safe {
        return Err(AppError::validation(
            "repair_grant_path_invalid",
            format!("semantic grant path `{path}` is unsafe or forbidden"),
        ));
    }
    let full = workspace.join(candidate);
    let metadata = fs::symlink_metadata(&full).map_err(|error| {
        repair_io_error(
            "repair_grant_path_unavailable",
            "semantic grant path is unavailable",
            &full,
            &error,
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::validation(
            "repair_grant_path_invalid",
            format!("semantic grant path `{path}` must be a tracked regular file"),
        ));
    }
    require_success(
        runner,
        CommandSpec::new("git").args(["ls-files", "--error-unmatch", "--", path]),
        "repair_grant_path_untracked",
        "semantic grant path must already be tracked",
    )?;
    let unstaged = run(
        runner,
        CommandSpec::new("git").args(["diff", "--quiet", "--", path]),
    )?;
    if unstaged.code != Some(0) {
        return Err(AppError::validation(
            "repair_grant_path_unstaged",
            format!("semantic grant path `{path}` has unstaged edits before authorization"),
        ));
    }
    Ok(())
}

fn index_oid_for_path(runner: &impl CommandRunner, path: &str) -> Result<String, AppError> {
    let output = require_success(
        runner,
        CommandSpec::new("git").args(["ls-files", "--stage", "--", path]),
        "repair_index_inspection_failed",
        "could not inspect semantic grant path index identity",
    )?;
    let mut entries = output.stdout.lines().filter(|line| !line.trim().is_empty());
    let line = entries.next().ok_or_else(|| {
        AppError::validation(
            "repair_grant_path_untracked",
            format!("semantic grant path `{path}` has no index entry"),
        )
    })?;
    if entries.next().is_some() {
        return Err(AppError::validation(
            "repair_grant_path_unmerged",
            format!("semantic grant path `{path}` has multiple index stages"),
        ));
    }
    let metadata = line
        .split_once('\t')
        .map(|(metadata, _)| metadata)
        .ok_or_else(|| {
            AppError::validation(
                "repair_index_output_invalid",
                "git ls-files returned an invalid semantic grant entry",
            )
        })?;
    let mut fields = metadata.split_whitespace();
    let _mode = fields.next();
    let oid = fields.next().ok_or_else(|| {
        AppError::validation(
            "repair_index_output_invalid",
            "semantic grant entry omitted OID",
        )
    })?;
    if fields.next() != Some("0") {
        return Err(AppError::validation(
            "repair_grant_path_unmerged",
            format!("semantic grant path `{path}` is not at stage zero"),
        ));
    }
    Ok(oid.to_owned())
}

/// Apply one reviewed, exact-source semantic path grant to a resolving session.
#[allow(clippy::too_many_lines)]
pub fn grant_paths(
    context: &AppContext,
    input: &RepairGrantInput,
) -> Result<RepairGrantOutput, AppError> {
    validate_session_id(&input.session)?;
    validate_grant_text(input)?;
    let paths = repair_paths_for_session(&context.repository_path, &input.session)?;
    let mut repair = read_manifest(&paths.manifest)?;
    require_session_match(&repair, &input.session)?;
    validate_workspace(&paths, &repair)?;
    if repair.state != RepairState::Resolving {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_grant_state_invalid",
            "semantic paths may be granted only after exact repair materialization reaches resolving",
            Some(json!({"repair": repair_status_output(&repair)?})),
        ));
    }
    let current_fingerprint = config_fingerprint(context)?;
    if current_fingerprint != repair.config_fingerprint {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_config_changed",
            "Caravan config changed after the repair session was prepared",
            Some(json!({"repair": repair_status_output(&repair)?})),
        ));
    }
    let mut lock = OperationLock::acquire(&context.repository_path, "repair-grant")?;
    lock.checkpoint(
        "repair_semantic_grant_preflight",
        repair_lock_receipt(&repair)?,
        false,
    )?;
    let timeout = Duration::from_secs(context.config.command_timeout_secs);
    let runner = ProcessRunner::in_directory(&paths.workspace).with_timeout(timeout);
    verify_remote_head(&runner, &repair.provider_git_url, &repair.head)?;
    verify_remote_head(&runner, &repair.provider_git_url, &repair.target)?;
    let unmerged = require_success(
        &runner,
        CommandSpec::new("git").args(["ls-files", "-u", "-z"]),
        "repair_index_inspection_failed",
        "could not inspect unresolved repair paths before semantic grant",
    )?;
    if !unmerged.stdout.is_empty() {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_grant_conflicts_unresolved",
            "resolve and stage typed mechanical conflicts before granting semantic paths",
            Some(json!({"repair": repair_status_output(&repair)?})),
        ));
    }

    let source_revision = rev_parse(&runner, &input.source_revision)?;
    let parents = commit_parents(&runner, &source_revision)?;
    if parents.len() != 1 {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repair_grant_source_nonlinear",
            "semantic grant source must have exactly one parent",
            Some(json!({"source_revision": source_revision, "parents": parents})),
        ));
    }
    let source_parent = parents[0].clone();
    let now = unix_ms();
    let expires_unix_ms = now.saturating_add(input.expires_secs.saturating_mul(1000));
    let unique_paths = input.paths.iter().cloned().collect::<BTreeSet<_>>();
    if unique_paths.len() != input.paths.len() {
        return Err(AppError::validation(
            "repair_grant_duplicate_path",
            "semantic grant path list contains duplicates",
        ));
    }
    let new_grant_count = unique_paths
        .iter()
        .filter(|path| {
            !repair
                .semantic_grants
                .iter()
                .any(|grant| grant.path == path.as_str())
        })
        .count();
    if repair.semantic_grants.len().saturating_add(new_grant_count) > MAX_SEMANTIC_GRANTS {
        return Err(AppError::validation(
            "repair_grant_limit_exceeded",
            format!("repair sessions allow at most {MAX_SEMANTIC_GRANTS} semantic grants"),
        ));
    }

    let mut proposed = Vec::new();
    let mut already_applied = true;
    for path in unique_paths {
        validate_grant_path(&paths.workspace, &runner, &path)?;
        let baseline_oid = index_oid_for_path(&runner, &path)?;
        if let Some(existing) = repair
            .semantic_grants
            .iter()
            .find(|grant| grant.path == path)
        {
            if existing.source_revision != source_revision
                || existing.actor != input.actor
                || existing.reason != input.reason
                || existing.expires_unix_ms < now
            {
                return Err(AppError::structured(
                    ErrorCategory::Validation,
                    "repair_grant_conflict",
                    format!("path `{path}` already has a different or expired semantic grant"),
                    Some(json!({"existing": existing})),
                ));
            }
            if baseline_oid == existing.expected_result_oid {
                let mut reconciled = existing.clone();
                if !reconciled.applied {
                    already_applied = false;
                    reconciled.applied = true;
                }
                proposed.push((reconciled, None));
                continue;
            }
            if baseline_oid != existing.baseline_oid || existing.applied {
                return Err(AppError::structured(
                    ErrorCategory::Validation,
                    "repair_grant_path_drift",
                    format!("path `{path}` changed outside its exact grant receipt"),
                    Some(json!({"existing": existing, "actual_oid": baseline_oid})),
                ));
            }
            already_applied = false;
            let merged =
                semantic_merge_result(&paths, &runner, &path, &source_parent, &source_revision)?;
            if merged.oid != existing.expected_result_oid {
                return Err(AppError::structured(
                    ErrorCategory::Validation,
                    "repair_grant_result_drift",
                    "recomputed semantic result differs from persisted grant receipt",
                    Some(json!({"existing": existing, "actual_result_oid": merged.oid})),
                ));
            }
            proposed.push((existing.clone(), Some(merged.bytes)));
            continue;
        }
        already_applied = false;
        let source_blob = rev_parse(&runner, &format!("{}:{path}", source_revision.0))?;
        let source_parent_blob = rev_parse(&runner, &format!("{}:{path}", source_parent.0))?;
        let patch = require_success(
            &runner,
            CommandSpec::new("git").args([
                "diff",
                "--binary",
                source_parent.0.as_str(),
                source_revision.0.as_str(),
                "--",
                path.as_str(),
            ]),
            "repair_grant_source_diff_failed",
            "could not derive reviewed semantic source patch",
        )?;
        let merged =
            semantic_merge_result(&paths, &runner, &path, &source_parent, &source_revision)?;
        proposed.push((
            RepairPathGrant {
                path,
                actor: input.actor.clone(),
                reason: input.reason.clone(),
                source_revision: source_revision.clone(),
                source_parent: source_parent.clone(),
                source_blob,
                source_parent_blob,
                source_patch_fingerprint: stable_fingerprint(patch.stdout.as_bytes()),
                baseline_oid,
                expected_result_oid: merged.oid,
                granted_unix_ms: now,
                expires_unix_ms,
                applied: false,
            },
            Some(merged.bytes),
        ));
    }

    for (grant, _) in &proposed {
        if let Some(existing) = repair
            .semantic_grants
            .iter_mut()
            .find(|existing| existing.path == grant.path)
        {
            existing.clone_from(grant);
        } else {
            repair.semantic_grants.push(grant.clone());
        }
    }
    repair.updated_unix_ms = unix_ms();
    write_manifest(&paths.manifest, &repair)?;

    for (grant, content) in proposed {
        if let Some(content) = content {
            fs::write(paths.workspace.join(&grant.path), content).map_err(|error| {
                repair_io_error(
                    "repair_grant_write_failed",
                    "could not write reviewed semantic merge result",
                    &paths.workspace.join(&grant.path),
                    &error,
                )
            })?;
            require_success(
                &runner,
                CommandSpec::new("git").args(["add", "--", grant.path.as_str()]),
                "repair_grant_stage_failed",
                "could not stage reviewed semantic merge result",
            )?;
            let actual = index_oid_for_path(&runner, &grant.path)?;
            if actual != grant.expected_result_oid {
                return Err(AppError::structured(
                    ErrorCategory::Validation,
                    "repair_grant_result_drift",
                    "staged semantic result differs from exact grant receipt",
                    Some(json!({"grant": grant, "actual_result_oid": actual})),
                ));
            }
            let persisted = repair
                .semantic_grants
                .iter_mut()
                .find(|existing| existing.path == grant.path)
                .expect("grant persisted before apply");
            persisted.applied = true;
        }
    }
    repair.updated_unix_ms = unix_ms();
    write_manifest(&paths.manifest, &repair)?;

    lock.checkpoint(
        "repair_semantic_paths_granted",
        repair_lock_receipt(&repair)?,
        false,
    )?;
    lock.release()?;
    let grants = repair
        .semantic_grants
        .iter()
        .filter(|grant| input.paths.contains(&grant.path))
        .cloned()
        .collect::<Vec<_>>();
    Ok(RepairGrantOutput {
        repair: repair_status_output(&repair)?,
        grants,
        already_applied,
        next: "review the exact grant receipts, then run repair continue without editing any other path".to_owned(),
    })
}

enum RevocationPlan {
    Restore {
        grant: RepairPathGrant,
        baseline: String,
    },
    Finalize(RepairPathGrant),
    Already,
}

/// Revoke exact grants and restore the pre-grant staged blob for each path.
#[allow(clippy::too_many_lines)]
pub fn revoke_grants(
    context: &AppContext,
    input: &RepairRevokeGrantInput,
) -> Result<RepairRevokeGrantOutput, AppError> {
    validate_session_id(&input.session)?;
    if input.paths.is_empty()
        || input.paths.len() > MAX_SEMANTIC_GRANTS
        || input.actor.trim().is_empty()
        || input.actor.len() > MAX_GRANT_ACTOR_BYTES
        || input.reason.trim().is_empty()
        || input.reason.len() > MAX_GRANT_REASON_BYTES
    {
        return Err(AppError::validation(
            "repair_grant_revocation_invalid",
            "revocation requires bounded paths, actor, and reason",
        ));
    }
    let paths = repair_paths_for_session(&context.repository_path, &input.session)?;
    let mut repair = read_manifest(&paths.manifest)?;
    require_session_match(&repair, &input.session)?;
    validate_workspace(&paths, &repair)?;
    if repair.state != RepairState::Resolving {
        return Err(AppError::validation(
            "repair_grant_state_invalid",
            "semantic grants may be revoked only while repair is resolving",
        ));
    }
    if config_fingerprint(context)? != repair.config_fingerprint {
        return Err(AppError::validation(
            "repair_config_changed",
            "Caravan config changed after the repair session was prepared",
        ));
    }
    let mut lock = OperationLock::acquire(&context.repository_path, "repair-revoke-grant")?;
    lock.checkpoint(
        "repair_semantic_grant_revocation_preflight",
        repair_lock_receipt(&repair)?,
        false,
    )?;
    let runner = ProcessRunner::in_directory(&paths.workspace)
        .with_timeout(Duration::from_secs(context.config.command_timeout_secs));
    verify_remote_head(&runner, &repair.provider_git_url, &repair.head)?;
    verify_remote_head(&runner, &repair.provider_git_url, &repair.target)?;
    let unique = input.paths.iter().cloned().collect::<BTreeSet<_>>();
    if unique.len() != input.paths.len() {
        return Err(AppError::validation(
            "repair_grant_duplicate_path",
            "revocation path list contains duplicates",
        ));
    }

    // Validate the whole request before touching any path. A later authority,
    // drift, object, or provider failure therefore leaves every grant intact.
    let mut plans = Vec::with_capacity(unique.len());
    for path in &unique {
        let actual_oid = index_oid_for_path(&runner, path)?;
        let Some(grant) = repair
            .semantic_grants
            .iter()
            .find(|grant| grant.path == *path)
            .cloned()
        else {
            let already = repair.semantic_grant_revocations.iter().any(|revocation| {
                revocation.path == *path
                    && revocation.actor == input.actor
                    && revocation.reason == input.reason
                    && revocation.baseline_oid == actual_oid
            });
            if !already {
                return Err(AppError::validation(
                    "repair_grant_not_found",
                    format!("path `{path}` has no exact active or revoked semantic grant"),
                ));
            }
            plans.push((path.clone(), RevocationPlan::Already));
            continue;
        };
        if grant.actor != input.actor {
            return Err(AppError::structured(
                ErrorCategory::MissingPermission,
                "repair_grant_authority_mismatch",
                "grant revocation actor must match the exact granting authority",
                Some(json!({"grant": grant, "actor": input.actor})),
            ));
        }
        if actual_oid == grant.expected_result_oid {
            let baseline = require_success(
                &runner,
                CommandSpec::new("git").args(["cat-file", "blob", grant.baseline_oid.as_str()]),
                "repair_grant_baseline_missing",
                "could not restore semantic grant baseline blob",
            )?;
            plans.push((
                path.clone(),
                RevocationPlan::Restore {
                    grant,
                    baseline: baseline.stdout,
                },
            ));
        } else if actual_oid == grant.baseline_oid {
            // The index restore completed but the final manifest publication did
            // not. Keep the receipt as authority and finish publication below.
            plans.push((path.clone(), RevocationPlan::Finalize(grant)));
        } else {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "repair_grant_result_drift",
                "semantic grant result changed outside its exact revocation states",
                Some(json!({"grant": grant, "actual_oid": actual_oid})),
            ));
        }
    }

    for (path, plan) in &plans {
        if let RevocationPlan::Restore { grant, baseline } = plan {
            fs::write(paths.workspace.join(path), baseline).map_err(|error| {
                repair_io_error(
                    "repair_grant_write_failed",
                    "could not restore semantic grant baseline",
                    &paths.workspace.join(path),
                    &error,
                )
            })?;
            require_success(
                &runner,
                CommandSpec::new("git").args(["add", "--", path.as_str()]),
                "repair_grant_stage_failed",
                "could not stage restored semantic grant baseline",
            )?;
            if index_oid_for_path(&runner, path)? != grant.baseline_oid {
                return Err(AppError::validation(
                    "repair_grant_baseline_drift",
                    "restored semantic grant baseline has an unexpected object ID",
                ));
            }
        }
    }

    let mut manifest_changed = false;
    for (path, plan) in &plans {
        let grant = match plan {
            RevocationPlan::Restore { grant, .. } | RevocationPlan::Finalize(grant) => grant,
            RevocationPlan::Already => continue,
        };
        if index_oid_for_path(&runner, path)? != grant.baseline_oid {
            return Err(AppError::validation(
                "repair_grant_baseline_drift",
                "semantic grant baseline moved before revocation publication",
            ));
        }
        repair.semantic_grants.retain(|active| active.path != *path);
        repair
            .semantic_grant_revocations
            .retain(|revocation| revocation.path != *path);
        repair
            .semantic_grant_revocations
            .push(RepairPathGrantRevocation {
                path: path.clone(),
                actor: input.actor.clone(),
                reason: input.reason.clone(),
                source_revision: grant.source_revision.clone(),
                baseline_oid: grant.baseline_oid.clone(),
                expected_result_oid: grant.expected_result_oid.clone(),
                revoked_unix_ms: unix_ms(),
            });
        manifest_changed = true;
    }
    if repair.semantic_grant_revocations.len() > MAX_SEMANTIC_GRANTS {
        let remove = repair
            .semantic_grant_revocations
            .len()
            .saturating_sub(MAX_SEMANTIC_GRANTS);
        repair.semantic_grant_revocations.drain(..remove);
    }
    let revoked = unique.into_iter().collect::<Vec<_>>();
    if manifest_changed {
        repair.updated_unix_ms = unix_ms();
        write_manifest(&paths.manifest, &repair)?;
    }
    lock.checkpoint(
        "repair_semantic_grants_revoked",
        json!({
            "repair": repair_lock_receipt(&repair)?,
            "revoked_paths": &revoked,
            "actor": input.actor,
            "reason": input.reason,
        }),
        false,
    )?;
    lock.release()?;
    Ok(RepairRevokeGrantOutput {
        repair: repair_status_output(&repair)?,
        revoked_paths: revoked,
        provider_mutated: false,
    })
}

struct SemanticMergeResult {
    oid: String,
    bytes: Vec<u8>,
}

fn semantic_merge_result(
    paths: &RepairPaths,
    runner: &impl CommandRunner,
    path: &str,
    source_parent: &CommitOid,
    source_revision: &CommitOid,
) -> Result<SemanticMergeResult, AppError> {
    let temporary = paths.session_root.join(format!(
        "grant-{}-{}",
        std::process::id(),
        stable_fingerprint(path.as_bytes())
    ));
    fs::create_dir_all(&temporary).map_err(|error| {
        repair_io_error(
            "repair_grant_temp_failed",
            "could not create semantic grant temporary directory",
            &temporary,
            &error,
        )
    })?;
    let current = fs::read(paths.workspace.join(path)).map_err(|error| {
        repair_io_error(
            "repair_grant_read_failed",
            "could not read current semantic path",
            &paths.workspace.join(path),
            &error,
        )
    })?;
    let base = require_success(
        runner,
        CommandSpec::new("git").args(["show", &format!("{}:{path}", source_parent.0)]),
        "repair_grant_source_read_failed",
        "could not read semantic source parent blob",
    )?;
    let source = require_success(
        runner,
        CommandSpec::new("git").args(["show", &format!("{}:{path}", source_revision.0)]),
        "repair_grant_source_read_failed",
        "could not read semantic source blob",
    )?;
    let current_path = temporary.join("current");
    let base_path = temporary.join("base");
    let source_path = temporary.join("source");
    fs::write(&current_path, current).map_err(|error| {
        repair_io_error(
            "repair_grant_temp_failed",
            "could not stage current semantic file",
            &current_path,
            &error,
        )
    })?;
    fs::write(&base_path, base.stdout).map_err(|error| {
        repair_io_error(
            "repair_grant_temp_failed",
            "could not stage semantic base file",
            &base_path,
            &error,
        )
    })?;
    fs::write(&source_path, source.stdout).map_err(|error| {
        repair_io_error(
            "repair_grant_temp_failed",
            "could not stage semantic source file",
            &source_path,
            &error,
        )
    })?;
    let merge = run(
        runner,
        CommandSpec::new("git").args([
            "merge-file",
            "-p",
            current_path.to_string_lossy().as_ref(),
            base_path.to_string_lossy().as_ref(),
            source_path.to_string_lossy().as_ref(),
        ]),
    )?;
    let _ = fs::remove_dir_all(&temporary);
    if merge.code != Some(0) {
        return Err(repair_command_failure(
            "repair_grant_semantic_conflict",
            "reviewed semantic patch does not merge cleanly into the repair workspace",
            &merge,
            json!({"path": path, "provider_mutated": false}),
        ));
    }
    let hash = require_success(
        runner,
        CommandSpec::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .stdin(merge.stdout.clone()),
        "repair_grant_hash_failed",
        "could not hash reviewed semantic merge result",
    )?;
    Ok(SemanticMergeResult {
        oid: hash.stdout.trim().to_owned(),
        bytes: merge.stdout.into_bytes(),
    })
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
                fresh_ci_required: true,
                agent_edit_receipt: repair.agent_edit_receipt.clone(),
            });
        return resume_or_return(context, input, &paths, repair, publication);
    }

    let mut lock = OperationLock::acquire(&context.repository_path, "repair-continue")?;
    lock.checkpoint(
        "repair_verification_in_flight",
        repair_lock_receipt(&repair)?,
        false,
    )?;
    let timeout = Duration::from_secs(context.config.command_timeout_secs);
    let runner = ProcessRunner::in_directory(&paths.workspace).with_timeout(timeout);
    let expected_parents = vec![repair.head.oid.clone(), repair.target.oid.clone()];

    let (new_head, parents) = match repair.state {
        RepairState::Preparing => unreachable!("preparing state returned above"),
        RepairState::Resolving => {
            if try_rev_parse(&runner, "MERGE_HEAD")?.is_some() {
                if let Some(receipt) = verify_resolution(&runner, &repair, input.actor.as_deref())?
                {
                    repair.agent_edit_receipt = Some(receipt);
                    repair.updated_unix_ms = unix_ms();
                    write_manifest(&paths.manifest, &repair)?;
                    lock.checkpoint(
                        "repair_agent_edits_verified",
                        json!({
                            "repair": repair_lock_receipt(&repair)?,
                            "agent_edit_receipt": repair.agent_edit_receipt,
                            "provider_mutated": false,
                        }),
                        false,
                    )?;
                }
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
                json!({
                    "repair": repair_lock_receipt(&repair)?,
                    "new_head": &head,
                    "parents": &found_parents,
                }),
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
                "repair": repair_lock_receipt(&repair)?,
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
        fresh_ci_required: true,
        agent_edit_receipt: repair.agent_edit_receipt.clone(),
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

fn repair_status_output(repair: &RepairSession) -> Result<RepairStatusOutput, AppError> {
    let baseline = serde_json::to_vec(&repair.baseline_index).map_err(|error| {
        AppError::structured(
            ErrorCategory::SerializationError,
            "repair_baseline_fingerprint_failed",
            format!("could not fingerprint repair baseline index: {error}"),
            None,
        )
    })?;
    Ok(RepairStatusOutput {
        version: repair.version,
        session: repair.session.clone(),
        repository: repair.repository.clone(),
        pr: repair.pr,
        head: repair.head.clone(),
        old_base: repair.old_base.clone(),
        target: repair.target.clone(),
        target_pr: repair.target_pr,
        workspace: repair.workspace.clone(),
        provider_git_url: repair.provider_git_url.clone(),
        object_cache_path: repair.object_cache_path.clone(),
        object_cache_common_dir: repair.object_cache_common_dir.clone(),
        state: repair.state,
        phase: repair.phase,
        last_error: repair.last_error.clone(),
        conflicting_paths: repair.conflicting_paths.clone(),
        baseline_index_count: repair.baseline_index.len(),
        baseline_index_fingerprint: stable_fingerprint(&baseline),
        semantic_grants: repair.semantic_grants.clone(),
        semantic_grant_revocations: repair.semantic_grant_revocations.clone(),
        agent_edit_authorization: repair.agent_edit_authorization.clone(),
        agent_edit_receipt: repair.agent_edit_receipt.clone(),
        materialization_timeout_secs: repair.materialization_timeout_secs,
        created_unix_ms: repair.created_unix_ms,
        updated_unix_ms: repair.updated_unix_ms,
        published_head: repair.published_head.clone(),
    })
}

/// Inspect a persisted repair session through a bounded manifest projection.
pub fn status(
    context: &AppContext,
    input: &RepairStatusInput,
) -> Result<RepairStatusOutput, AppError> {
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
    repair_status_output(&repair)
}

fn allowed_repair_paths(repair: &RepairSession) -> Result<BTreeSet<String>, AppError> {
    let now = unix_ms();
    let mut allowed = repair
        .conflicting_paths
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    for grant in &repair.semantic_grants {
        if !grant.applied {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "repair_grant_incomplete",
                format!("semantic grant for `{}` was not fully applied", grant.path),
                Some(json!({"grant": grant})),
            ));
        }
        if grant.expires_unix_ms < now {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "repair_grant_expired",
                format!("semantic grant for `{}` expired", grant.path),
                Some(json!({"grant": grant, "now_unix_ms": now})),
            ));
        }
        allowed.insert(grant.path.clone());
    }
    Ok(allowed)
}

fn staged_paths(runner: &impl CommandRunner) -> Result<BTreeSet<String>, AppError> {
    let output = require_success(
        runner,
        CommandSpec::new("git").args(["diff", "--cached", "--name-only", "--no-renames", "-z"]),
        "repair_index_inspection_failed",
        "could not inspect complete staged repair scope",
    )?;
    Ok(nul_paths(&output.stdout)?.into_iter().collect())
}

fn index_mode_for_path(
    runner: &impl CommandRunner,
    path: &str,
) -> Result<Option<String>, AppError> {
    let output = require_success(
        runner,
        CommandSpec::new("git").args(["ls-files", "--stage", "--", path]),
        "repair_index_inspection_failed",
        "could not inspect staged repair path mode",
    )?;
    Ok(output
        .stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| line.split_whitespace().next())
        .map(str::to_owned))
}

fn validate_agent_edit_path(runner: &impl CommandRunner, path: &str) -> Result<(), AppError> {
    let candidate = Path::new(path);
    let safe = !path.is_empty()
        && path.len() <= 512
        && !path.contains(['\n', '\r', '\0'])
        && !candidate.is_absolute()
        && candidate
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && path != ".git"
        && !path.starts_with(".git/");
    if !safe {
        return Err(AppError::validation(
            "repair_agent_edit_path_forbidden",
            format!("agent edit path `{path}` is outside safe repository content"),
        ));
    }
    let basename = candidate
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let secret_extension = candidate
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["pem", "key", "p12"]
                .iter()
                .any(|blocked| extension.eq_ignore_ascii_case(blocked))
        });
    let secret = basename == ".env"
        || basename.starts_with(".env.")
        || matches!(
            basename.as_str(),
            "id_rsa" | "id_ed25519" | "credentials" | "credentials.json" | ".npmrc" | ".pypirc"
        )
        || secret_extension;
    if secret {
        return Err(AppError::validation(
            "repair_agent_edit_secret_forbidden",
            format!("agent edit path `{path}` resembles an operational secret"),
        ));
    }
    if let Some(mode) = index_mode_for_path(runner, path)?
        && mode != "100644"
        && mode != "100755"
    {
        return Err(AppError::validation(
            "repair_agent_edit_mode_forbidden",
            format!(
                "agent edit path `{path}` must be a regular staged file or deletion, not mode {mode}"
            ),
        ));
    }
    Ok(())
}

fn verify_agent_edit_authorization<'a>(
    repair: &'a RepairSession,
    actor: Option<&str>,
) -> Result<&'a RepairAgentEditAuthorization, AppError> {
    let authorization = repair.agent_edit_authorization.as_ref().ok_or_else(|| {
        AppError::structured(
            ErrorCategory::MissingPermission,
            "repair_agent_edit_authorization_required",
            "staged paths outside conflict/grant scope require an audited agent-edit authorization",
            Some(json!({"repair": repair_status_output(repair).ok()})),
        )
    })?;
    if authorization.expires_unix_ms < unix_ms() {
        return Err(AppError::structured(
            ErrorCategory::MissingPermission,
            "repair_agent_edit_authorization_expired",
            "agent-edit authorization expired before verification",
            Some(json!({"authorization": authorization})),
        ));
    }
    if actor != Some(authorization.actor.as_str()) {
        return Err(AppError::structured(
            ErrorCategory::MissingPermission,
            "repair_agent_edit_authority_mismatch",
            "repair continue actor must exactly match the authorized agent",
            Some(json!({"authorized_actor": authorization.actor, "provided_actor": actor})),
        ));
    }
    if authorization.session != repair.session
        || authorization.repository != repair.repository
        || authorization.pr != repair.pr
        || authorization.head_oid != repair.head.oid
        || authorization.target_oid != repair.target.oid
        || authorization.config_fingerprint != repair.config_fingerprint
    {
        return Err(AppError::validation(
            "repair_agent_edit_authorization_drift",
            "repair identity drifted from the exact agent-edit authorization",
        ));
    }
    Ok(authorization)
}

#[allow(clippy::too_many_lines)]
fn verify_resolution(
    runner: &impl CommandRunner,
    repair: &RepairSession,
    actor: Option<&str>,
) -> Result<Option<RepairAgentEditReceipt>, AppError> {
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
    let current = staged_index(runner)?;
    let allowed = allowed_repair_paths(repair)?;
    for grant in &repair.semantic_grants {
        if current.get(&grant.path) != Some(&grant.expected_result_oid) {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "repair_grant_result_drift",
                format!(
                    "semantic grant result for `{}` changed before continue",
                    grant.path
                ),
                Some(json!({
                    "grant": grant,
                    "actual_oid": current.get(&grant.path),
                })),
            ));
        }
    }
    let changed = staged_paths(runner)?;
    let broad_paths = changed
        .iter()
        .filter(|path| {
            !allowed.contains(*path)
                && repair
                    .baseline_index
                    .get(*path)
                    .is_none_or(|baseline| current.get(*path) != Some(baseline))
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let agent_edit_receipt = if broad_paths.is_empty() {
        None
    } else {
        let authorization = verify_agent_edit_authorization(repair, actor)?;
        if broad_paths.len() > MAX_AGENT_EDIT_PATHS {
            return Err(AppError::validation(
                "repair_agent_edit_path_limit_exceeded",
                format!(
                    "agent edit scope has {} paths; maximum is {MAX_AGENT_EDIT_PATHS}",
                    broad_paths.len()
                ),
            ));
        }
        let path_bytes_total = broad_paths.iter().map(String::len).sum::<usize>();
        if path_bytes_total > MAX_AGENT_EDIT_PATH_BYTES {
            return Err(AppError::validation(
                "repair_agent_edit_path_bytes_exceeded",
                format!(
                    "agent edit path scope is {path_bytes_total} bytes; maximum is {MAX_AGENT_EDIT_PATH_BYTES}"
                ),
            ));
        }
        for path in &broad_paths {
            validate_agent_edit_path(runner, path)?;
        }
        let mut arguments = vec![
            "diff".to_owned(),
            "--cached".to_owned(),
            "--binary".to_owned(),
            "--no-color".to_owned(),
            "--no-ext-diff".to_owned(),
            "--no-renames".to_owned(),
            "--".to_owned(),
        ];
        arguments.extend(broad_paths.iter().cloned());
        let diff = require_success(
            runner,
            CommandSpec::new("git").args(arguments),
            "repair_agent_edit_diff_failed",
            "could not capture authorized staged edit evidence",
        )?;
        if diff.stdout.len() > MAX_AGENT_EDIT_DIFF_BYTES {
            return Err(AppError::validation(
                "repair_agent_edit_diff_too_large",
                format!(
                    "authorized staged diff is {} bytes; maximum is {MAX_AGENT_EDIT_DIFF_BYTES}",
                    diff.stdout.len()
                ),
            ));
        }
        let paths = broad_paths.iter().cloned().collect::<Vec<_>>();
        let path_bytes = serde_json::to_vec(&paths).map_err(|error| {
            AppError::structured(
                ErrorCategory::SerializationError,
                "repair_agent_edit_receipt_failed",
                error.to_string(),
                None,
            )
        })?;
        let staged = paths
            .iter()
            .map(|path| (path.clone(), current.get(path).cloned()))
            .collect::<BTreeMap<_, _>>();
        let staged_bytes = serde_json::to_vec(&staged).map_err(|error| {
            AppError::structured(
                ErrorCategory::SerializationError,
                "repair_agent_edit_receipt_failed",
                error.to_string(),
                None,
            )
        })?;
        Some(RepairAgentEditReceipt {
            actor: authorization.actor.clone(),
            reason: authorization.reason.clone(),
            path_count: paths.len(),
            paths,
            path_fingerprint: stable_fingerprint(&path_bytes),
            diff_fingerprint: stable_fingerprint(diff.stdout.as_bytes()),
            diff_bytes: diff.stdout.len(),
            staged_index_fingerprint: stable_fingerprint(&staged_bytes),
            verified_unix_ms: unix_ms(),
            fresh_ci_required: true,
        })
    };
    for (path, oid) in &repair.baseline_index {
        if allowed.contains(path) || broad_paths.contains(path) {
            continue;
        }
        if current.get(path) != Some(oid) {
            return Err(scope_error(repair, path));
        }
    }
    for path in current.keys() {
        if !repair.baseline_index.contains_key(path)
            && !allowed.contains(path)
            && !broad_paths.contains(path)
        {
            return Err(scope_error(repair, path));
        }
    }
    Ok(agent_edit_receipt)
}

fn scope_error(repair: &RepairSession, path: &str) -> AppError {
    let allowed_paths = repair
        .conflicting_paths
        .iter()
        .cloned()
        .chain(
            repair
                .semantic_grants
                .iter()
                .map(|grant| grant.path.clone()),
        )
        .collect::<BTreeSet<_>>();
    AppError::structured(
        ErrorCategory::Validation,
        "repair_scope_changed",
        format!("path `{path}` changed outside the typed conflict scope"),
        Some(json!({
            "repair": repair,
            "path": path,
            "allowed_paths": allowed_paths,
            "next": "revert unrelated edits, use an exact semantic grant, or obtain session-level agent-edit authorization before continuing",
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

fn staged_index(runner: &impl CommandRunner) -> Result<BTreeMap<String, String>, AppError> {
    let changed = require_success(
        runner,
        CommandSpec::new("git").args(["diff", "--cached", "--name-only", "-z"]),
        "repair_index_inspection_failed",
        "could not inspect mechanically staged repair paths",
    )?;
    let changed = nul_paths(&changed.stdout)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let mut index = stage_zero_index(runner)?;
    index.retain(|path, _| changed.contains(path));
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

fn partial_workspace_ready(
    workspace: &Path,
    provider_git_url: &str,
    object_cache_path: &str,
) -> bool {
    let runner = ProcessRunner::in_directory(workspace).with_timeout(Duration::from_secs(10));
    let repository =
        runner.run(&CommandSpec::new("git").args(["rev-parse", "--is-inside-work-tree"]));
    if !repository.is_ok_and(|output| output.is_success() && output.stdout.trim() == "true") {
        return false;
    }
    let provider_matches = runner
        .run(&CommandSpec::new("git").args(["remote", "get-url", "origin"]))
        .is_ok_and(|output| output.is_success() && output.stdout.trim() == provider_git_url);
    let cache_matches = runner
        .run(&CommandSpec::new("git").args(["remote", "get-url", "cache"]))
        .is_ok_and(|output| output.is_success() && output.stdout.trim() == object_cache_path);
    provider_matches && cache_matches
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

fn repair_lock_receipt(repair: &RepairSession) -> Result<Value, AppError> {
    let encoded = serde_json::to_vec(repair).map_err(|error| {
        AppError::structured(
            ErrorCategory::SerializationError,
            "repair_manifest_fingerprint_failed",
            format!("could not encode repair manifest fingerprint: {error}"),
            None,
        )
    })?;
    let manifest = Path::new(&repair.workspace).parent().map_or_else(
        || PathBuf::from(MANIFEST_NAME),
        |parent| parent.join(MANIFEST_NAME),
    );
    Ok(json!({
        "version": repair.version,
        "session": repair.session,
        "pr": repair.pr,
        "state": repair.state,
        "phase": repair.phase,
        "head_oid": repair.head.oid,
        "target_oid": repair.target.oid,
        "manifest_path": manifest,
        "manifest_bytes": encoded.len(),
        "manifest_fingerprint": stable_fingerprint(&encoded),
        "baseline_index_count": repair.baseline_index.len(),
        "conflicting_path_count": repair.conflicting_paths.len(),
        "semantic_grant_count": repair.semantic_grants.len(),
        "semantic_grant_revocation_count": repair.semantic_grant_revocations.len(),
        "agent_edit_authorized": repair.agent_edit_authorization.is_some(),
        "agent_edit_actor": repair.agent_edit_authorization.as_ref().map(|authorization| &authorization.actor),
        "agent_edit_expires_unix_ms": repair.agent_edit_authorization.as_ref().map(|authorization| authorization.expires_unix_ms),
        "agent_edit_path_count": repair.agent_edit_receipt.as_ref().map_or(0, |receipt| receipt.path_count),
        "agent_edit_path_fingerprint": repair.agent_edit_receipt.as_ref().map(|receipt| &receipt.path_fingerprint),
        "agent_edit_diff_fingerprint": repair.agent_edit_receipt.as_ref().map(|receipt| &receipt.diff_fingerprint),
        "updated_unix_ms": repair.updated_unix_ms,
    }))
}

fn stable_fingerprint(bytes: &[u8]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(bytes);
    format!("{:016x}", hasher.finish())
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
    Ok(stable_fingerprint(&encoded))
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
        fs::write(clone.join("README.md"), "# Base\n").unwrap();
        fs::write(clone.join("SPEC.md"), "# Contract\n").unwrap();
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

    fn semantic_source(fixture: &Fixture) -> CommitOid {
        git(&fixture.clone, &["checkout", "-b", "semantic-source"]);
        fs::write(
            fixture.clone.join("README.md"),
            "# Base\n\nReviewed shell-safe message body.\n",
        )
        .unwrap();
        fs::write(
            fixture.clone.join("SPEC.md"),
            "# Contract\n\nMessage bodies use reviewed files.\n",
        )
        .unwrap();
        git(&fixture.clone, &["add", "README.md", "SPEC.md"]);
        git(
            &fixture.clone,
            &["commit", "-m", "reviewed semantic contracts"],
        );
        let source = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["checkout", "feature"]);
        source
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
        assert_eq!(
            output
                .repair
                .baseline_index
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["target.txt"]
        );
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
        let error = verify_resolution(&runner, &output.repair, None).unwrap_err();
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "repair_agent_edit_authorization_required"
        );
    }

    #[test]
    fn audited_agent_authorization_allows_bounded_repository_edits_and_receipts() {
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
        fs::write(workspace.join("stable.txt"), "agent repair\n").unwrap();
        fs::write(workspace.join("agent-added.txt"), "reviewed addition\n").unwrap();
        git(
            &workspace,
            &["add", "shared.txt", "stable.txt", "agent-added.txt"],
        );
        git(&workspace, &["rm", "SPEC.md"]);
        let authorization = authorize_agent_edits(
            &context,
            &RepairAuthorizeAgentEditsInput {
                session: output.repair.session.clone(),
                actor: "caco-merger".to_owned(),
                reason: "repair exact CI and semantic decision".to_owned(),
                expires_secs: 3600,
            },
        )
        .unwrap();
        assert!(!authorization.already_authorized);
        assert!(!authorization.provider_mutated);
        let replay = authorize_agent_edits(
            &context,
            &RepairAuthorizeAgentEditsInput {
                session: output.repair.session.clone(),
                actor: "caco-merger".to_owned(),
                reason: "repair exact CI and semantic decision".to_owned(),
                expires_secs: 3600,
            },
        )
        .unwrap();
        assert!(replay.already_authorized);

        let mismatch = continue_session(
            &context,
            &RepairContinueInput {
                session: output.repair.session.clone(),
                actor: Some("other-agent".to_owned()),
                no_sync: true,
            },
        )
        .unwrap_err();
        assert_eq!(mismatch.code(), "repair_agent_edit_authority_mismatch");

        let continued = continue_session(
            &context,
            &RepairContinueInput {
                session: output.repair.session,
                actor: Some("caco-merger".to_owned()),
                no_sync: true,
            },
        )
        .unwrap();
        let publication = continued.publication.expect("published");
        let receipt = publication.agent_edit_receipt.expect("agent edit receipt");
        assert_eq!(receipt.actor, "caco-merger");
        assert_eq!(receipt.paths, ["SPEC.md", "agent-added.txt", "stable.txt"]);
        assert_eq!(receipt.path_count, 3);
        assert!(receipt.diff_bytes > 0);
        assert!(receipt.fresh_ci_required);
        assert!(publication.fresh_ci_required);
    }

    #[test]
    fn audited_agent_authorization_rejects_staged_operational_secrets() {
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
        fs::write(workspace.join(".env"), "TOKEN=secret\n").unwrap();
        git(&workspace, &["add", "shared.txt", ".env"]);
        authorize_agent_edits(
            &context,
            &RepairAuthorizeAgentEditsInput {
                session: output.repair.session.clone(),
                actor: "caco-merger".to_owned(),
                reason: "repair source".to_owned(),
                expires_secs: 3600,
            },
        )
        .unwrap();
        let error = continue_session(
            &context,
            &RepairContinueInput {
                session: output.repair.session,
                actor: Some("caco-merger".to_owned()),
                no_sync: true,
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "repair_agent_edit_secret_forbidden");
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
                actor: None,
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
                actor: None,
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
    fn operation_lock_receipt_stays_bounded_for_huge_historical_manifest() {
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
        for index in 0..10_000 {
            repair.baseline_index.insert(
                format!("large/canonical/path/{index:05}.rs"),
                format!("{index:040x}"),
            );
        }
        let receipt = repair_lock_receipt(&repair).unwrap();
        let encoded = serde_json::to_vec(&receipt).unwrap();
        assert!(
            encoded.len() < 4 * 1024,
            "compact receipt was {} bytes",
            encoded.len()
        );
        assert_eq!(receipt["baseline_index_count"], 10_001);
        assert!(receipt.get("baseline_index").is_none());
        let status = repair_status_output(&repair).unwrap();
        let status_json = serde_json::to_vec(&status).unwrap();
        assert!(
            status_json.len() < 8 * 1024,
            "bounded status was {} bytes",
            status_json.len()
        );
        assert_eq!(status.baseline_index_count, 10_001);
        let mut lock = OperationLock::acquire(&fixture.clone, "repair-bounded-test").unwrap();
        lock.checkpoint("repair_workspace_ready", receipt, false)
            .expect("compact repair receipt fits operation lock cap");
        lock.release().unwrap();
    }

    #[test]
    fn audited_semantic_grant_applies_exact_source_paths_idempotently() {
        let fixture = fixture();
        let source = semantic_source(&fixture);
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
        let input = RepairGrantInput {
            session: output.repair.session.clone(),
            paths: vec!["README.md".to_owned(), "SPEC.md".to_owned()],
            source_revision: source.0,
            actor: "operator".to_owned(),
            reason: "restore reviewed message-body contracts".to_owned(),
            expires_secs: 3600,
        };
        let granted = grant_paths(&context, &input).unwrap();
        assert!(!granted.already_applied);
        assert_eq!(granted.grants.len(), 2);
        assert!(granted.grants.iter().all(|grant| grant.applied));
        assert!(
            fs::read_to_string(workspace.join("README.md"))
                .unwrap()
                .contains("Reviewed shell-safe message body")
        );
        assert!(
            fs::read_to_string(workspace.join("SPEC.md"))
                .unwrap()
                .contains("Message bodies use reviewed files")
        );
        let replay = grant_paths(&context, &input).unwrap();
        assert!(replay.already_applied);

        let continued = continue_session(
            &context,
            &RepairContinueInput {
                session: input.session,
                actor: None,
                no_sync: true,
            },
        )
        .unwrap();
        assert!(continued.publication.is_some());
    }

    #[test]
    fn semantic_grant_recovers_staged_result_before_applied_manifest_publication() {
        let fixture = fixture();
        let source = semantic_source(&fixture);
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
        let input = RepairGrantInput {
            session: output.repair.session,
            paths: vec!["README.md".to_owned()],
            source_revision: source.0,
            actor: "operator".to_owned(),
            reason: "recover exact staged result".to_owned(),
            expires_secs: 3600,
        };
        let granted = grant_paths(&context, &input).unwrap();
        let expected = granted.grants[0].expected_result_oid.clone();
        let paths = repair_paths_for_session(&fixture.clone, &input.session).unwrap();
        let mut interrupted = read_manifest(&paths.manifest).unwrap();
        interrupted.semantic_grants[0].applied = false;
        write_manifest(&paths.manifest, &interrupted).unwrap();

        let recovered = grant_paths(&context, &input).unwrap();
        assert!(!recovered.already_applied);
        assert!(recovered.grants[0].applied);
        assert_eq!(
            index_oid_for_path(&ProcessRunner::in_directory(&workspace), "README.md").unwrap(),
            expected
        );
        assert!(read_manifest(&paths.manifest).unwrap().semantic_grants[0].applied);
    }

    #[test]
    fn semantic_grant_revocation_restores_exact_baseline_without_provider_mutation() {
        let fixture = fixture();
        let source = semantic_source(&fixture);
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
        let input = RepairGrantInput {
            session: output.repair.session,
            paths: vec!["README.md".to_owned()],
            source_revision: source.0,
            actor: "operator-a".to_owned(),
            reason: "reviewed contract".to_owned(),
            expires_secs: 3600,
        };
        let granted = grant_paths(&context, &input).unwrap();
        let grant = granted.grants[0].clone();
        let wrong_actor = revoke_grants(
            &context,
            &RepairRevokeGrantInput {
                session: input.session.clone(),
                paths: input.paths.clone(),
                actor: "operator-b".to_owned(),
                reason: "wrong authority".to_owned(),
            },
        )
        .unwrap_err();
        assert_eq!(wrong_actor.code(), "repair_grant_authority_mismatch");
        let revoke_input = RepairRevokeGrantInput {
            session: input.session,
            paths: input.paths,
            actor: "operator-a".to_owned(),
            reason: "review superseded".to_owned(),
        };
        let revoked = revoke_grants(&context, &revoke_input).unwrap();
        assert_eq!(revoked.revoked_paths, ["README.md"]);
        assert!(!revoked.provider_mutated);
        assert!(revoked.repair.semantic_grants.is_empty());
        assert_eq!(
            index_oid_for_path(&ProcessRunner::in_directory(&workspace), "README.md").unwrap(),
            grant.baseline_oid
        );
        let replay = revoke_grants(&context, &revoke_input).unwrap();
        assert_eq!(replay.revoked_paths, ["README.md"]);
        assert_eq!(replay.repair.semantic_grant_revocations.len(), 1);
    }

    #[test]
    fn semantic_grant_revocation_preflights_whole_set_and_recovers_partial_restore() {
        let fixture = fixture();
        let source = semantic_source(&fixture);
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
        let grant_input = RepairGrantInput {
            session: output.repair.session,
            paths: vec!["README.md".to_owned(), "SPEC.md".to_owned()],
            source_revision: source.0,
            actor: "operator".to_owned(),
            reason: "reviewed docs".to_owned(),
            expires_secs: 3600,
        };
        let granted = grant_paths(&context, &grant_input).unwrap();
        let readme = granted
            .grants
            .iter()
            .find(|grant| grant.path == "README.md")
            .unwrap()
            .clone();
        let spec = granted
            .grants
            .iter()
            .find(|grant| grant.path == "SPEC.md")
            .unwrap()
            .clone();
        let paths = repair_paths_for_session(&fixture.clone, &grant_input.session).unwrap();

        let mut mismatched = read_manifest(&paths.manifest).unwrap();
        mismatched
            .semantic_grants
            .iter_mut()
            .find(|grant| grant.path == "SPEC.md")
            .unwrap()
            .actor = "different-actor".to_owned();
        write_manifest(&paths.manifest, &mismatched).unwrap();
        let revoke_input = RepairRevokeGrantInput {
            session: grant_input.session,
            paths: grant_input.paths,
            actor: "operator".to_owned(),
            reason: "replace reviewed source".to_owned(),
        };
        let mismatch = revoke_grants(&context, &revoke_input).unwrap_err();
        assert_eq!(mismatch.code(), "repair_grant_authority_mismatch");
        let runner = ProcessRunner::in_directory(&workspace);
        assert_eq!(
            index_oid_for_path(&runner, "README.md").unwrap(),
            readme.expected_result_oid
        );
        assert_eq!(
            index_oid_for_path(&runner, "SPEC.md").unwrap(),
            spec.expected_result_oid
        );

        mismatched
            .semantic_grants
            .iter_mut()
            .find(|grant| grant.path == "SPEC.md")
            .unwrap()
            .actor = "operator".to_owned();
        write_manifest(&paths.manifest, &mismatched).unwrap();
        let baseline = require_success(
            &runner,
            CommandSpec::new("git").args(["cat-file", "blob", readme.baseline_oid.as_str()]),
            "test_baseline_missing",
            "test baseline unavailable",
        )
        .unwrap();
        fs::write(workspace.join("README.md"), baseline.stdout).unwrap();
        git(&workspace, &["add", "README.md"]);

        let revoked = revoke_grants(&context, &revoke_input).unwrap();
        assert_eq!(revoked.revoked_paths, ["README.md", "SPEC.md"]);
        assert!(revoked.repair.semantic_grants.is_empty());
        assert_eq!(revoked.repair.semantic_grant_revocations.len(), 2);
        assert_eq!(
            index_oid_for_path(&runner, "README.md").unwrap(),
            readme.baseline_oid
        );
        assert_eq!(
            index_oid_for_path(&runner, "SPEC.md").unwrap(),
            spec.baseline_oid
        );
        let replay = revoke_grants(&context, &revoke_input).unwrap();
        assert_eq!(replay.revoked_paths, ["README.md", "SPEC.md"]);
    }

    #[test]
    fn semantic_grant_authority_and_expiry_fail_closed() {
        let fixture = fixture();
        let source = semantic_source(&fixture);
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
        let input = RepairGrantInput {
            session: output.repair.session.clone(),
            paths: vec!["README.md".to_owned()],
            source_revision: source.0,
            actor: "operator-a".to_owned(),
            reason: "reviewed contract".to_owned(),
            expires_secs: 3600,
        };
        grant_paths(&context, &input).unwrap();
        let mut wrong_actor = input.clone();
        wrong_actor.actor = "operator-b".to_owned();
        let mismatch = grant_paths(&context, &wrong_actor).unwrap_err();
        assert_eq!(mismatch.code(), "repair_grant_conflict");

        let paths = repair_paths_for_session(&fixture.clone, &input.session).unwrap();
        let mut repair = read_manifest(&paths.manifest).unwrap();
        repair.semantic_grants[0].expires_unix_ms = unix_ms().saturating_sub(1);
        write_manifest(&paths.manifest, &repair).unwrap();
        let expired = continue_session(
            &context,
            &RepairContinueInput {
                session: input.session,
                actor: None,
                no_sync: true,
            },
        )
        .unwrap_err();
        assert_eq!(expired.code(), "repair_grant_expired");
    }

    #[test]
    fn semantic_grant_does_not_authorize_unlisted_or_unsafe_paths() {
        let fixture = fixture();
        let source = semantic_source(&fixture);
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
        let invalid = grant_paths(
            &context,
            &RepairGrantInput {
                session: output.repair.session.clone(),
                paths: vec!["../README.md".to_owned()],
                source_revision: source.0.clone(),
                actor: "operator".to_owned(),
                reason: "invalid traversal".to_owned(),
                expires_secs: 3600,
            },
        )
        .unwrap_err();
        assert_eq!(invalid.code(), "repair_grant_path_invalid");

        grant_paths(
            &context,
            &RepairGrantInput {
                session: output.repair.session.clone(),
                paths: vec!["README.md".to_owned(), "SPEC.md".to_owned()],
                source_revision: source.0,
                actor: "operator".to_owned(),
                reason: "reviewed docs".to_owned(),
                expires_secs: 3600,
            },
        )
        .unwrap();
        fs::write(workspace.join("stable.txt"), "unauthorized\n").unwrap();
        git(&workspace, &["add", "stable.txt"]);
        let error = continue_session(
            &context,
            &RepairContinueInput {
                session: output.repair.session,
                actor: None,
                no_sync: true,
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "repair_agent_edit_authorization_required");
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
                actor: None,
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
