//! Bounded, repository-scoped clone reuse for hosted workers.
//!
//! Cache entries are immutable object hints. Every hosted job receives a
//! unique, dissociated clone and proves the exact remote default generation
//! before the existing web repository loader can grant it any domain surface.
//! The module never acquires mutation authority: provider and branch writes
//! remain fenced by [`crate::remote_lease`].

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::AppError;
use crate::command::{CommandOutput, CommandRunner, CommandSpec, ProcessRunner};

pub const DEFAULT_HOSTED_CLONE_CACHE_MAX_BYTES: u64 = 20 * 1024 * 1024 * 1024;
pub const DEFAULT_HOSTED_CLONE_CACHE_MAX_AGE_SECS: u64 = 24 * 60 * 60;
pub const DEFAULT_HOSTED_CLONE_CACHE_MAX_ENTRIES: usize = 64;
pub const DEFAULT_HOSTED_CLONE_CACHE_MAX_JOBS: usize = 32;
pub const DEFAULT_HOSTED_CLONE_MAX_DURATION_SECS: u64 = 10 * 60;
const HOSTED_CLONE_TIMEOUT: Duration = Duration::from_secs(120);
const CACHE_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_RETRY: Duration = Duration::from_millis(50);
const CACHE_RECEIPT: &str = ".cara-hosted-cache.json";
const JOB_RECEIPT: &str = ".cara-hosted-job.json";
const JOB_LOCK: &str = ".cara-hosted-job.lock";

#[derive(Debug, Clone)]
pub struct HostedCloneCacheConfig {
    pub root: PathBuf,
    pub installation_id: u64,
    pub max_bytes: u64,
    pub max_age: Duration,
    pub max_entries: usize,
    pub max_jobs: usize,
    pub max_duration: Duration,
}

impl HostedCloneCacheConfig {
    pub fn validate(&self) -> Result<(), AppError> {
        if self.installation_id == 0 {
            return Err(AppError::validation(
                "hosted_clone_installation_invalid",
                "hosted clone materialization requires a nonzero installation ID",
            ));
        }
        if self.max_bytes == 0 || self.max_entries == 0 || self.max_jobs == 0 {
            return Err(AppError::validation(
                "hosted_clone_bounds_invalid",
                "hosted clone byte, entry, and job bounds must all be nonzero",
            ));
        }
        if self.max_age.is_zero() || self.max_duration.is_zero() {
            return Err(AppError::validation(
                "hosted_clone_duration_invalid",
                "hosted clone maximum age and operation duration must be nonzero",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HostedRepositorySpec {
    host: String,
    owner: String,
    name: String,
    remote_url: String,
}

impl HostedRepositorySpec {
    fn github(slug: &str) -> Result<Self, AppError> {
        let Some((owner, name)) = slug.split_once('/') else {
            return Err(invalid_repository(slug));
        };
        if name.contains('/') || !valid_repository_part(owner) || !valid_repository_part(name) {
            return Err(invalid_repository(slug));
        }
        Ok(Self {
            host: "github.com".to_owned(),
            owner: owner.to_owned(),
            name: name.to_owned(),
            remote_url: format!("https://github.com/{owner}/{name}.git"),
        })
    }

    fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

fn valid_repository_part(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn invalid_repository(slug: &str) -> AppError {
    AppError::structured(
        ErrorCategory::Validation,
        "hosted_clone_repository_invalid",
        "--hosted-repository must be one exact OWNER/NAME GitHub repository",
        Some(json!({"repository": slug})),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RemoteDefaultProof {
    default_branch: String,
    head_sha: String,
    object_format: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CacheReceipt {
    schema_version: u32,
    host: String,
    repository: String,
    installation_id: u64,
    object_format: String,
    default_branch: String,
    remote_url: String,
    cache_key: String,
    created_unix_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct JobReceipt {
    schema_version: u32,
    job_id: String,
    repository: String,
    installation_id: u64,
    cache_key: String,
    expected_head: String,
    default_branch: String,
    created_unix_secs: u64,
}

/// Secret-free materialization evidence exposed by the hosted dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HostedCloneStatus {
    pub repository: String,
    pub cache_key: String,
    pub cache_hit: bool,
    pub cache_bytes: u64,
    pub job_id: String,
    pub expected_head: String,
    pub default_branch: String,
    pub object_format: String,
    pub elapsed_ms: u64,
    pub cleanup_count: u64,
    pub exact_ref_verified: bool,
    pub credential_transport_verified: bool,
}

/// One job-owned clone. Dropping it removes only its locked job directory.
#[derive(Debug)]
pub struct HostedCloneWorktree {
    path: PathBuf,
    job_root: PathBuf,
    lease: Option<File>,
    status: HostedCloneStatus,
}

impl HostedCloneWorktree {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn status(&self) -> &HostedCloneStatus {
        &self.status
    }
}

impl Drop for HostedCloneWorktree {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take() {
            let _ = FileExt::unlock(&lease);
        }
        let _ = fs::remove_dir_all(&self.job_root);
    }
}

/// Materialize all explicitly allowlisted hosted repositories.
///
/// App credential resolution remains inside [`ProcessRunner`]. The generated
/// remote is exact HTTPS and contains no credential material.
pub fn materialize_hosted_repositories(
    config: &HostedCloneCacheConfig,
    repositories: &[String],
) -> Result<Vec<HostedCloneWorktree>, AppError> {
    config.validate()?;
    validate_bootstrap_app_environment(config.installation_id)?;
    let runner = ProcessRunner::new()
        .with_timeout(HOSTED_CLONE_TIMEOUT)
        .with_operation_deadline(Instant::now() + config.max_duration);
    materialize_with_runner(config, repositories, &runner)
}

fn validate_bootstrap_app_environment(expected_installation_id: u64) -> Result<(), AppError> {
    let mode = std::env::var("CARA_GITHUB_AUTH_MODE").ok();
    let slug = std::env::var("CARA_GITHUB_APP_SLUG").ok();
    let installation = std::env::var("CARA_GITHUB_INSTALLATION_ID").ok();
    validate_bootstrap_app_values(
        mode.as_deref(),
        slug.as_deref(),
        installation.as_deref(),
        expected_installation_id,
    )
}

fn validate_bootstrap_app_values(
    mode: Option<&str>,
    app_slug: Option<&str>,
    installation_id: Option<&str>,
    expected_installation_id: u64,
) -> Result<(), AppError> {
    if mode != Some("app_installation") || app_slug.is_none_or(str::is_empty) {
        return Err(AppError::validation(
            "hosted_clone_app_auth_required",
            "hosted clone bootstrap requires CARA_GITHUB_AUTH_MODE=app_installation and CARA_GITHUB_APP_SLUG",
        ));
    }
    let observed_installation_id = installation_id.and_then(|value| value.parse::<u64>().ok());
    if observed_installation_id != Some(expected_installation_id) {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "hosted_clone_installation_mismatch",
            "hosted clone bootstrap installation environment does not match --github-installation-id",
            Some(json!({
                "expected_installation_id": expected_installation_id,
                "observed_installation_id": observed_installation_id,
            })),
        ));
    }
    Ok(())
}

fn materialize_with_runner(
    config: &HostedCloneCacheConfig,
    repositories: &[String],
    runner: &dyn CommandRunner,
) -> Result<Vec<HostedCloneWorktree>, AppError> {
    config.validate()?;
    let root = prepare_root(&config.root)?;

    let mut seen = BTreeSet::new();
    let mut materialized = Vec::with_capacity(repositories.len());
    for repository in repositories {
        let spec = HostedRepositorySpec::github(repository)?;
        if !seen.insert(spec.slug()) {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "hosted_clone_repository_duplicate",
                "the same hosted repository was requested more than once",
                Some(json!({"repository": spec.slug()})),
            ));
        }
        materialized.push(materialize_one(config, &root, &spec, runner)?);
    }
    let _quota_lock = acquire_lock(&root.join("locks/quota.lock"), CACHE_LOCK_TIMEOUT)?;
    let _ = cleanup_partial_cache_builds(&root)?;
    let _ = cleanup_quarantine(config, &root)?;
    enforce_cache_bounds(config, &root, &BTreeSet::new())?;
    Ok(materialized)
}

fn materialize_one(
    config: &HostedCloneCacheConfig,
    root: &Path,
    spec: &HostedRepositorySpec,
    runner: &dyn CommandRunner,
) -> Result<HostedCloneWorktree, AppError> {
    let started = Instant::now();
    let remote = read_remote_default(spec, runner)?;
    let cache_key = cache_key(config, spec, &remote);
    // Serialize cache publication and quota accounting only. Returned,
    // dissociated job worktrees remain fully concurrent.
    let _quota_lock = acquire_lock(&root.join("locks/quota.lock"), CACHE_LOCK_TIMEOUT)?;
    let (cache_path, cache_hit) = prepare_cache(config, root, spec, &remote, &cache_key, runner)?;
    create_job_clone(
        JobCloneInput {
            config,
            root,
            spec,
            remote: &remote,
            cache_key: &cache_key,
            cache_path: &cache_path,
            cache_hit,
            started,
        },
        runner,
    )
}

fn prepare_cache(
    config: &HostedCloneCacheConfig,
    root: &Path,
    spec: &HostedRepositorySpec,
    remote: &RemoteDefaultProof,
    cache_key: &str,
    runner: &dyn CommandRunner,
) -> Result<(PathBuf, bool), AppError> {
    let cache_path = root.join("cache").join(format!("{cache_key}.git"));
    let cache_lock_path = root.join("locks").join(format!("{cache_key}.lock"));
    let cache_lock = acquire_lock(&cache_lock_path, CACHE_LOCK_TIMEOUT)?;
    let cache_hit = ensure_cache(config, root, spec, remote, cache_key, &cache_path, runner)?;
    FileExt::unlock(&cache_lock).map_err(|error| {
        io_error(
            "hosted_clone_cache_unlock_failed",
            "could not release the hosted clone cache lock",
            &cache_lock_path,
            error,
        )
    })?;
    Ok((cache_path, cache_hit))
}

#[derive(Clone, Copy)]
struct JobCloneInput<'a> {
    config: &'a HostedCloneCacheConfig,
    root: &'a Path,
    spec: &'a HostedRepositorySpec,
    remote: &'a RemoteDefaultProof,
    cache_key: &'a str,
    cache_path: &'a Path,
    cache_hit: bool,
    started: Instant,
}

fn create_job_clone(
    input: JobCloneInput<'_>,
    runner: &dyn CommandRunner,
) -> Result<HostedCloneWorktree, AppError> {
    let JobCloneInput {
        config,
        root,
        spec,
        remote,
        cache_key,
        cache_path,
        cache_hit,
        started,
    } = input;
    let cleanup_count = prepare_job_quota(config, root, spec, cache_path)?;

    let job_id = uuid::Uuid::now_v7().to_string();
    let job_root = root.join("jobs").join(&job_id);
    create_contained_dir(root, &job_root)?;
    let lease = create_job_lease(&job_root)?;
    write_job_receipt(config, spec, remote, cache_key, &job_id, &job_root)?;

    let worktree = job_root.join("worktree");
    let clone = CommandSpec::new("git").args([
        "clone",
        "--quiet",
        "--single-branch",
        "--branch",
        remote.default_branch.as_str(),
        "--reference-if-able",
        cache_path.to_string_lossy().as_ref(),
        "--dissociate",
        spec.remote_url.as_str(),
        worktree.to_string_lossy().as_ref(),
    ]);
    if let Err(error) = run_success(
        runner,
        &clone,
        "hosted_clone_job_failed",
        "could not create the isolated hosted repository clone",
        Some(json!({"repository": spec.slug()})),
    ) {
        cleanup_failed_job(&lease, &job_root);
        return Err(error);
    }
    if let Err(error) = verify_job(root, spec, remote, &worktree, runner) {
        let _ = FileExt::unlock(&lease);
        quarantine(root, &job_root, "job")?;
        let _ = cleanup_quarantine(config, root)?;
        return Err(error);
    }
    if directory_size(root)? > config.max_bytes {
        cleanup_failed_job(&lease, &job_root);
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "hosted_clone_byte_limit",
            "hosted clone cache and active jobs exceed the configured byte bound",
            Some(json!({"max_bytes": config.max_bytes, "repository": spec.slug()})),
        ));
    }
    let status = HostedCloneStatus {
        repository: spec.slug(),
        cache_key: cache_key.to_owned(),
        cache_hit,
        cache_bytes: directory_size(cache_path)?,
        job_id,
        expected_head: remote.head_sha.clone(),
        default_branch: remote.default_branch.clone(),
        object_format: remote.object_format.clone(),
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        cleanup_count,
        exact_ref_verified: true,
        credential_transport_verified: true,
    };
    Ok(HostedCloneWorktree {
        path: worktree,
        job_root,
        lease: Some(lease),
        status,
    })
}

fn prepare_job_quota(
    config: &HostedCloneCacheConfig,
    root: &Path,
    spec: &HostedRepositorySpec,
    cache_path: &Path,
) -> Result<u64, AppError> {
    let cleanup_count = cleanup_holderless_jobs(root)?
        .saturating_add(cleanup_partial_cache_builds(root)?)
        .saturating_add(cleanup_quarantine(config, root)?);
    enforce_cache_bounds(config, root, &BTreeSet::from([cache_path.to_path_buf()]))?;
    if directory_size(root)? > config.max_bytes {
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "hosted_clone_byte_limit",
            "hosted clone cache and active jobs already exceed the configured byte bound",
            Some(json!({"max_bytes": config.max_bytes, "repository": spec.slug()})),
        ));
    }
    let active_jobs = count_active_jobs(root)?;
    if active_jobs >= config.max_jobs {
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "hosted_clone_job_limit",
            "hosted clone job limit would be exceeded",
            Some(json!({
                "active_jobs": active_jobs,
                "max_jobs": config.max_jobs,
            })),
        ));
    }
    Ok(cleanup_count)
}

fn create_job_lease(job_root: &Path) -> Result<File, AppError> {
    let lease_path = job_root.join(JOB_LOCK);
    let lease = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&lease_path)
        .map_err(|error| {
            io_error(
                "hosted_clone_job_lock_failed",
                "could not create the hosted job lease",
                &lease_path,
                error,
            )
        })?;
    lease.lock_exclusive().map_err(|error| {
        io_error(
            "hosted_clone_job_lock_failed",
            "could not lock the hosted job lease",
            &lease_path,
            error,
        )
    })?;
    Ok(lease)
}

fn write_job_receipt(
    config: &HostedCloneCacheConfig,
    spec: &HostedRepositorySpec,
    remote: &RemoteDefaultProof,
    cache_key: &str,
    job_id: &str,
    job_root: &Path,
) -> Result<(), AppError> {
    write_private_json(
        &job_root.join(JOB_RECEIPT),
        &JobReceipt {
            schema_version: 1,
            job_id: job_id.to_owned(),
            repository: spec.slug(),
            installation_id: config.installation_id,
            cache_key: cache_key.to_owned(),
            expected_head: remote.head_sha.clone(),
            default_branch: remote.default_branch.clone(),
            created_unix_secs: unix_secs(),
        },
    )
}

fn cleanup_failed_job(lease: &File, job_root: &Path) {
    let _ = FileExt::unlock(lease);
    let _ = fs::remove_dir_all(job_root);
}

fn read_remote_default(
    spec: &HostedRepositorySpec,
    runner: &dyn CommandRunner,
) -> Result<RemoteDefaultProof, AppError> {
    let command =
        CommandSpec::new("git").args(["ls-remote", "--symref", spec.remote_url.as_str(), "HEAD"]);
    let output = run_success(
        runner,
        &command,
        "hosted_clone_remote_probe_failed",
        "could not read the exact hosted repository default ref",
        Some(json!({"repository": spec.slug()})),
    )?;
    parse_remote_default(&output.stdout).ok_or_else(|| {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "hosted_clone_remote_default_ambiguous",
            "hosted repository did not expose one symbolic default branch and exact HEAD",
            Some(json!({"repository": spec.slug()})),
        )
    })
}

fn parse_remote_default(output: &str) -> Option<RemoteDefaultProof> {
    let mut branch = None;
    let mut head = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("ref: refs/heads/")
            && let Some((value, name)) = value.split_once('\t')
            && name == "HEAD"
            && branch.replace(value.to_owned()).is_some()
        {
            return None;
        } else if let Some((value, name)) = line.split_once('\t')
            && name == "HEAD"
            && (value.len() == 40 || value.len() == 64)
            && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            && head.replace(value.to_owned()).is_some()
        {
            return None;
        }
    }
    let head_sha = head?;
    Some(RemoteDefaultProof {
        default_branch: branch?,
        object_format: if head_sha.len() == 40 {
            "sha1"
        } else {
            "sha256"
        }
        .to_owned(),
        head_sha,
    })
}

fn cache_key(
    config: &HostedCloneCacheConfig,
    spec: &HostedRepositorySpec,
    remote: &RemoteDefaultProof,
) -> String {
    let mut hasher = Sha256::new();
    for value in [
        spec.host.as_str(),
        spec.owner.as_str(),
        spec.name.as_str(),
        &config.installation_id.to_string(),
        remote.object_format.as_str(),
        remote.default_branch.as_str(),
    ] {
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn ensure_cache(
    config: &HostedCloneCacheConfig,
    root: &Path,
    spec: &HostedRepositorySpec,
    remote: &RemoteDefaultProof,
    cache_key: &str,
    cache_path: &Path,
    runner: &dyn CommandRunner,
) -> Result<bool, AppError> {
    if fs::symlink_metadata(cache_path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        quarantine(root, cache_path, "cache-symlink")?;
    }
    if cache_path.exists() {
        let age = cache_path
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .unwrap_or(Duration::MAX);
        if age <= config.max_age
            && verify_cache(
                spec,
                remote,
                config.installation_id,
                cache_key,
                cache_path,
                runner,
            )
            .is_ok()
        {
            return Ok(true);
        }
        quarantine(root, cache_path, "cache")?;
    }

    let temporary = root
        .join("cache")
        .join(format!(".building-{cache_key}-{}", uuid::Uuid::now_v7()));
    let clone = CommandSpec::new("git").args([
        "clone",
        "--quiet",
        "--mirror",
        spec.remote_url.as_str(),
        temporary.to_string_lossy().as_ref(),
    ]);
    if let Err(error) = run_success(
        runner,
        &clone,
        "hosted_clone_cache_build_failed",
        "could not build the hosted repository object cache",
        Some(json!({"repository": spec.slug()})),
    ) {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    let receipt = CacheReceipt {
        schema_version: 1,
        host: spec.host.clone(),
        repository: spec.slug(),
        installation_id: config.installation_id,
        object_format: remote.object_format.clone(),
        default_branch: remote.default_branch.clone(),
        remote_url: spec.remote_url.clone(),
        cache_key: cache_key.to_owned(),
        created_unix_secs: unix_secs(),
    };
    write_private_json(&temporary.join(CACHE_RECEIPT), &receipt)?;
    if let Err(error) = verify_cache(
        spec,
        remote,
        config.installation_id,
        cache_key,
        &temporary,
        runner,
    ) {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, cache_path).map_err(|error| {
        io_error(
            "hosted_clone_cache_publish_failed",
            "could not atomically publish the hosted clone cache",
            cache_path,
            error,
        )
    })?;
    Ok(false)
}

fn verify_cache(
    spec: &HostedRepositorySpec,
    remote: &RemoteDefaultProof,
    installation_id: u64,
    cache_key: &str,
    cache_path: &Path,
    runner: &dyn CommandRunner,
) -> Result<(), AppError> {
    let receipt: CacheReceipt = read_json(&cache_path.join(CACHE_RECEIPT), "hosted clone cache")?;
    let expected = CacheReceipt {
        schema_version: 1,
        host: spec.host.clone(),
        repository: spec.slug(),
        installation_id,
        object_format: remote.object_format.clone(),
        default_branch: remote.default_branch.clone(),
        remote_url: spec.remote_url.clone(),
        cache_key: cache_key.to_owned(),
        created_unix_secs: receipt.created_unix_secs,
    };
    if receipt != expected {
        return Err(AppError::validation(
            "hosted_clone_cache_identity_mismatch",
            "hosted clone cache identity does not match the requested repository generation",
        ));
    }
    let git_dir = format!("--git-dir={}", cache_path.display());
    let bare = git_stdout(
        runner,
        [git_dir.as_str(), "rev-parse", "--is-bare-repository"],
    )?;
    let object = git_stdout(
        runner,
        [git_dir.as_str(), "rev-parse", "--show-object-format"],
    )?;
    let symbolic = git_stdout(runner, [git_dir.as_str(), "symbolic-ref", "HEAD"])?;
    let origin = git_stdout(
        runner,
        [git_dir.as_str(), "config", "--get", "remote.origin.url"],
    )?;
    if bare.trim() != "true"
        || object.trim() != remote.object_format
        || symbolic.trim() != format!("refs/heads/{}", remote.default_branch)
        || origin.trim() != spec.remote_url
    {
        return Err(AppError::validation(
            "hosted_clone_cache_corrupt",
            "hosted clone cache failed repository, object-format, or default-ref verification",
        ));
    }
    let forbidden = runner
        .run(&CommandSpec::new("git").args([
            git_dir.as_str(),
            "config",
            "--local",
            "--get-regexp",
            r"^(credential\.|url\..*\.insteadof|remote\..*\.pushurl|http\.)",
        ]))
        .map_err(|error| command_error("hosted_clone_cache_config_probe_failed", error))?;
    if !matches!(forbidden.code, Some(1)) || !forbidden.stdout.trim().is_empty() {
        return Err(AppError::validation(
            "hosted_clone_cache_credential_persisted",
            "hosted clone cache persisted a credential helper, URL rewrite, push URL, or HTTP override",
        ));
    }
    Ok(())
}

fn verify_job(
    root: &Path,
    spec: &HostedRepositorySpec,
    remote: &RemoteDefaultProof,
    worktree: &Path,
    runner: &dyn CommandRunner,
) -> Result<(), AppError> {
    let canonical = worktree.canonicalize().map_err(|error| {
        io_error(
            "hosted_clone_job_unavailable",
            "could not canonicalize the hosted job worktree",
            worktree,
            error,
        )
    })?;
    if !canonical.starts_with(root) || !canonical.join(".git").is_dir() {
        return Err(AppError::validation(
            "hosted_clone_job_containment",
            "hosted clone escaped its cache root or did not produce an isolated Git directory",
        ));
    }
    let cwd = canonical.to_string_lossy();
    let head = git_stdout(runner, ["-C", cwd.as_ref(), "rev-parse", "HEAD"])?;
    let object = git_stdout(
        runner,
        ["-C", cwd.as_ref(), "rev-parse", "--show-object-format"],
    )?;
    let branch = git_stdout(
        runner,
        ["-C", cwd.as_ref(), "symbolic-ref", "--short", "HEAD"],
    )?;
    let origin = git_stdout(
        runner,
        ["-C", cwd.as_ref(), "config", "--get", "remote.origin.url"],
    )?;
    if head.trim() != remote.head_sha {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "hosted_clone_default_moved",
            "hosted repository default branch moved during isolated materialization",
            Some(json!({
                "repository": spec.slug(),
                "expected_head": remote.head_sha,
                "observed_head": head.trim(),
            })),
        ));
    }
    if object.trim() != remote.object_format
        || branch.trim() != remote.default_branch
        || origin.trim() != spec.remote_url
    {
        return Err(AppError::validation(
            "hosted_clone_job_identity_mismatch",
            "hosted job clone failed repository, object-format, or default-branch verification",
        ));
    }
    if canonical.join(".git/objects/info/alternates").exists() {
        return Err(AppError::validation(
            "hosted_clone_job_shared_objects",
            "hosted job clone retained mutable shared-object alternates",
        ));
    }
    let forbidden = runner
        .run(&CommandSpec::new("git").args([
            "-C",
            cwd.as_ref(),
            "config",
            "--local",
            "--get-regexp",
            r"^(credential\.|url\..*\.insteadof|remote\..*\.pushurl|http\.)",
        ]))
        .map_err(|error| command_error("hosted_clone_job_config_probe_failed", error))?;
    if !matches!(forbidden.code, Some(1)) || !forbidden.stdout.trim().is_empty() {
        return Err(AppError::validation(
            "hosted_clone_job_credential_persisted",
            "hosted job clone persisted a credential helper, URL rewrite, push URL, or HTTP override",
        ));
    }
    Ok(())
}

fn prepare_root(root: &Path) -> Result<PathBuf, AppError> {
    if root.exists()
        && fs::symlink_metadata(root).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(AppError::validation(
            "hosted_clone_root_symlink",
            "hosted clone cache root must not be a symbolic link",
        ));
    }
    fs::create_dir_all(root).map_err(|error| {
        io_error(
            "hosted_clone_root_unavailable",
            "could not create the hosted clone cache root",
            root,
            error,
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).map_err(|error| {
            io_error(
                "hosted_clone_root_permissions_failed",
                "could not make the hosted clone cache root private",
                root,
                error,
            )
        })?;
    }
    let canonical = root.canonicalize().map_err(|error| {
        io_error(
            "hosted_clone_root_unavailable",
            "could not canonicalize the hosted clone cache root",
            root,
            error,
        )
    })?;
    for child in ["cache", "jobs", "locks", "quarantine"] {
        create_contained_dir(&canonical, &canonical.join(child))?;
    }
    Ok(canonical)
}

fn create_contained_dir(root: &Path, path: &Path) -> Result<(), AppError> {
    if path.exists()
        && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(AppError::validation(
            "hosted_clone_path_symlink",
            "hosted clone cache paths must not be symbolic links",
        ));
    }
    fs::create_dir_all(path).map_err(|error| {
        io_error(
            "hosted_clone_path_unavailable",
            "could not create a hosted clone cache path",
            path,
            error,
        )
    })?;
    let canonical = path.canonicalize().map_err(|error| {
        io_error(
            "hosted_clone_path_unavailable",
            "could not canonicalize a hosted clone cache path",
            path,
            error,
        )
    })?;
    if !canonical.starts_with(root) {
        return Err(AppError::validation(
            "hosted_clone_path_escape",
            "hosted clone cache path escaped its canonical root",
        ));
    }
    Ok(())
}

fn acquire_lock(path: &Path, timeout: Duration) -> Result<File, AppError> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(AppError::validation(
            "hosted_clone_lock_symlink",
            "hosted clone lock paths must not be symbolic links",
        ));
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| {
            io_error(
                "hosted_clone_cache_lock_failed",
                "could not open the hosted clone cache lock",
                path,
                error,
            )
        })?;
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(LOCK_RETRY);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(AppError::structured(
                    ErrorCategory::Timeout,
                    "hosted_clone_cache_lock_timeout",
                    "timed out waiting for the hosted clone cache lock",
                    Some(json!({"path": path, "timeout_ms": timeout.as_millis()})),
                ));
            }
            Err(error) => {
                return Err(io_error(
                    "hosted_clone_cache_lock_failed",
                    "could not lock the hosted clone cache entry",
                    path,
                    error,
                ));
            }
        }
    }
}

fn cleanup_holderless_jobs(root: &Path) -> Result<u64, AppError> {
    let jobs = root.join("jobs");
    let mut cleaned = 0_u64;
    for entry in fs::read_dir(&jobs).map_err(|error| {
        io_error(
            "hosted_clone_job_scan_failed",
            "could not scan hosted clone jobs",
            &jobs,
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            io_error(
                "hosted_clone_job_scan_failed",
                "could not read a hosted clone job entry",
                &jobs,
                error,
            )
        })?;
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(lock) = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.join(JOB_LOCK))
        else {
            quarantine(root, &path, "job")?;
            cleaned = cleaned.saturating_add(1);
            continue;
        };
        match lock.try_lock_exclusive() {
            Ok(()) => {
                let _ = FileExt::unlock(&lock);
                fs::remove_dir_all(&path).map_err(|error| {
                    io_error(
                        "hosted_clone_job_cleanup_failed",
                        "could not remove a holderless hosted clone job",
                        &path,
                        error,
                    )
                })?;
                cleaned = cleaned.saturating_add(1);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                return Err(io_error(
                    "hosted_clone_job_lock_probe_failed",
                    "could not inspect a hosted clone job lease",
                    &path,
                    error,
                ));
            }
        }
    }
    Ok(cleaned)
}

fn count_active_jobs(root: &Path) -> Result<usize, AppError> {
    let jobs = root.join("jobs");
    fs::read_dir(&jobs)
        .map_err(|error| {
            io_error(
                "hosted_clone_job_scan_failed",
                "could not scan hosted clone jobs",
                &jobs,
                error,
            )
        })?
        .try_fold(0_usize, |count, entry| {
            let entry = entry.map_err(|error| {
                io_error(
                    "hosted_clone_job_scan_failed",
                    "could not read a hosted clone job entry",
                    &jobs,
                    error,
                )
            })?;
            Ok(count + usize::from(entry.file_type().is_ok_and(|kind| kind.is_dir())))
        })
}

fn enforce_cache_bounds(
    config: &HostedCloneCacheConfig,
    root: &Path,
    preserve: &BTreeSet<PathBuf>,
) -> Result<(), AppError> {
    let cache_root = root.join("cache");
    let now = SystemTime::now();
    let mut entries = Vec::new();
    for entry in fs::read_dir(&cache_root).map_err(|error| {
        io_error(
            "hosted_clone_cache_scan_failed",
            "could not scan hosted clone cache entries",
            &cache_root,
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            io_error(
                "hosted_clone_cache_scan_failed",
                "could not read a hosted clone cache entry",
                &cache_root,
                error,
            )
        })?;
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path();
        let metadata = entry.metadata().map_err(|error| {
            io_error(
                "hosted_clone_cache_metadata_failed",
                "could not inspect a hosted clone cache entry",
                &path,
                error,
            )
        })?;
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        let age = now.duration_since(modified).unwrap_or_default();
        let size = directory_size(&path)?;
        entries.push((path, modified, age, size));
    }
    entries.sort_by_key(|(_, modified, _, _)| *modified);
    let mut total = entries.iter().map(|(_, _, _, size)| *size).sum::<u64>();
    let mut count = entries.len();
    for (path, _, age, size) in entries {
        let must_remove =
            age > config.max_age || count > config.max_entries || total > config.max_bytes;
        if !must_remove || preserve.contains(&path) {
            continue;
        }
        let key = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("unknown");
        let lock_path = root.join("locks").join(format!("{key}.lock"));
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path);
        let Ok(lock) = lock else { continue };
        if lock.try_lock_exclusive().is_err() {
            continue;
        }
        if fs::remove_dir_all(&path).is_ok() {
            total = total.saturating_sub(size);
            count = count.saturating_sub(1);
        }
        let _ = FileExt::unlock(&lock);
        let _ = fs::remove_file(&lock_path);
    }
    if total > config.max_bytes || count > config.max_entries {
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "hosted_clone_cache_quota_exceeded",
            "active or locked hosted clone cache entries exceed configured bounds",
            Some(json!({
                "bytes": total,
                "max_bytes": config.max_bytes,
                "entries": count,
                "max_entries": config.max_entries,
            })),
        ));
    }
    Ok(())
}

fn cleanup_partial_cache_builds(root: &Path) -> Result<u64, AppError> {
    let cache = root.join("cache");
    let mut cleaned = 0_u64;
    for entry in fs::read_dir(&cache).map_err(|error| {
        io_error(
            "hosted_clone_cache_scan_failed",
            "could not scan hosted clone cache entries",
            &cache,
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            io_error(
                "hosted_clone_cache_scan_failed",
                "could not read a hosted clone cache entry",
                &cache,
                error,
            )
        })?;
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(key) = name
            .strip_prefix(".building-")
            .and_then(|rest| rest.split_once('-').map(|(key, _)| key))
        else {
            continue;
        };
        let lock_path = root.join("locks").join(format!("{key}.lock"));
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| {
                io_error(
                    "hosted_clone_cache_lock_failed",
                    "could not inspect a partial cache build lock",
                    &lock_path,
                    error,
                )
            })?;
        match lock.try_lock_exclusive() {
            Ok(()) => {
                quarantine(root, &entry.path(), "cache-build")?;
                cleaned = cleaned.saturating_add(1);
                let _ = FileExt::unlock(&lock);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                return Err(io_error(
                    "hosted_clone_cache_lock_failed",
                    "could not inspect a partial cache build lock",
                    &lock_path,
                    error,
                ));
            }
        }
    }
    Ok(cleaned)
}

fn cleanup_quarantine(config: &HostedCloneCacheConfig, root: &Path) -> Result<u64, AppError> {
    let quarantine_root = root.join("quarantine");
    let mut cleaned = 0_u64;
    let now = SystemTime::now();
    let mut entries = Vec::new();
    for entry in fs::read_dir(&quarantine_root).map_err(|error| {
        io_error(
            "hosted_clone_quarantine_scan_failed",
            "could not scan hosted clone quarantine",
            &quarantine_root,
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            io_error(
                "hosted_clone_quarantine_scan_failed",
                "could not read a hosted clone quarantine entry",
                &quarantine_root,
                error,
            )
        })?;
        let path = entry.path();
        let metadata = entry.metadata().map_err(|error| {
            io_error(
                "hosted_clone_quarantine_scan_failed",
                "could not inspect a hosted clone quarantine entry",
                &path,
                error,
            )
        })?;
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        let age = now.duration_since(modified).unwrap_or_default();
        entries.push((path, modified, age, directory_size(&entry.path())?));
    }
    entries.sort_by_key(|(_, modified, _, _)| *modified);
    let mut root_bytes = directory_size(root)?;
    let mut count = entries.len();
    for (path, _, age, size) in entries {
        if age > config.max_age || count > config.max_entries || root_bytes > config.max_bytes {
            if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            }
            .map_err(|error| {
                io_error(
                    "hosted_clone_quarantine_cleanup_failed",
                    "could not remove bounded hosted clone quarantine evidence",
                    &path,
                    error,
                )
            })?;
            root_bytes = root_bytes.saturating_sub(size);
            count = count.saturating_sub(1);
            cleaned = cleaned.saturating_add(1);
        }
    }
    Ok(cleaned)
}

fn quarantine(root: &Path, path: &Path, kind: &str) -> Result<(), AppError> {
    if !path.exists() {
        return Ok(());
    }
    let target = root.join("quarantine").join(format!(
        "{kind}-{}-{}",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("unknown"),
        uuid::Uuid::now_v7()
    ));
    fs::rename(path, &target).map_err(|error| {
        io_error(
            "hosted_clone_quarantine_failed",
            "could not quarantine a partial or corrupt hosted clone path",
            path,
            error,
        )
    })
}

fn directory_size(path: &Path) -> Result<u64, AppError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        io_error(
            "hosted_clone_size_failed",
            "could not inspect hosted clone storage",
            path,
            error,
        )
    })?;
    if metadata.file_type().is_symlink() {
        return Err(AppError::validation(
            "hosted_clone_storage_symlink",
            "hosted clone storage must not contain symbolic links",
        ));
    }
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    fs::read_dir(path)
        .map_err(|error| {
            io_error(
                "hosted_clone_size_failed",
                "could not scan hosted clone storage",
                path,
                error,
            )
        })?
        .try_fold(0_u64, |size, entry| {
            let entry = entry.map_err(|error| {
                io_error(
                    "hosted_clone_size_failed",
                    "could not read hosted clone storage",
                    path,
                    error,
                )
            })?;
            Ok(size.saturating_add(directory_size(&entry.path())?))
        })
}

fn git_stdout<const N: usize>(
    runner: &dyn CommandRunner,
    args: [&str; N],
) -> Result<String, AppError> {
    run_success(
        runner,
        &CommandSpec::new("git").args(args),
        "hosted_clone_git_probe_failed",
        "could not verify hosted clone Git state",
        None,
    )
    .map(|output| output.stdout)
}

fn run_success(
    runner: &dyn CommandRunner,
    command: &CommandSpec,
    code: &'static str,
    message: &'static str,
    details: Option<serde_json::Value>,
) -> Result<CommandOutput, AppError> {
    let output = runner
        .run(command)
        .map_err(|error| command_error(code, error))?;
    if !output.is_success() {
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            code,
            message,
            Some(json!({
                "command": command.display(),
                "exit_code": output.code,
                "context": details.unwrap_or(serde_json::Value::Null),
            })),
        ));
    }
    Ok(output)
}

fn command_error(code: &'static str, error: impl std::fmt::Display) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        code,
        "hosted clone command could not complete",
        Some(json!({"diagnostic": error.to_string()})),
    )
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path, label: &str) -> Result<T, AppError> {
    let bytes = fs::read(path).map_err(|error| {
        io_error(
            "hosted_clone_receipt_unavailable",
            &format!("could not read {label} receipt"),
            path,
            error,
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        AppError::validation(
            "hosted_clone_receipt_invalid",
            format!("{label} receipt is invalid JSON"),
        )
    })
}

fn write_private_json(path: &Path, value: &impl Serialize) -> Result<(), AppError> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| {
        io_error(
            "hosted_clone_receipt_write_failed",
            "could not create a private hosted clone receipt",
            path,
            error,
        )
    })?;
    let bytes = serde_json::to_vec(value).map_err(|_| {
        AppError::validation(
            "hosted_clone_receipt_encode_failed",
            "could not encode hosted clone receipt",
        )
    })?;
    file.write_all(&bytes).map_err(|error| {
        io_error(
            "hosted_clone_receipt_write_failed",
            "could not write a private hosted clone receipt",
            path,
            error,
        )
    })?;
    file.sync_all().map_err(|error| {
        io_error(
            "hosted_clone_receipt_write_failed",
            "could not durably write a private hosted clone receipt",
            path,
            error,
        )
    })
}

fn io_error(code: &'static str, message: &str, path: &Path, error: std::io::Error) -> AppError {
    let diagnostic = error.to_string();
    drop(error);
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        code,
        format!("{message}: {diagnostic}"),
        Some(json!({"path": path})),
    )
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use mcp_cli::StructuredError;

    use super::*;
    use crate::command::{CommandRunError, CommandRunner};

    #[derive(Clone, Default)]
    struct LocalRunner {
        calls: Arc<Mutex<Vec<CommandSpec>>>,
    }

    impl CommandRunner for LocalRunner {
        fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
            self.calls.lock().unwrap().push(command.clone());
            let output = Command::new(&command.program)
                .args(&command.args)
                .output()
                .map_err(|error| CommandRunError::Spawn {
                    command: command.clone(),
                    message: error.to_string(),
                })?;
            Ok(CommandOutput {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
    }

    #[derive(Clone)]
    struct MovingRunner {
        inner: LocalRunner,
        seed: PathBuf,
        moved: Arc<AtomicBool>,
    }

    impl CommandRunner for MovingRunner {
        fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
            if command
                .args
                .iter()
                .any(|argument| argument == "--dissociate")
                && !self.moved.swap(true, Ordering::SeqCst)
            {
                fs::write(self.seed.join("moved.txt"), b"moved\n").unwrap();
                git(Some(&self.seed), &["add", "moved.txt"]);
                git(Some(&self.seed), &["commit", "-m", "move default"]);
                git(Some(&self.seed), &["push", "origin", "main"]);
            }
            self.inner.run(command)
        }
    }

    fn git(cwd: Option<&Path>, args: &[&str]) -> String {
        let mut command = Command::new("git");
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn local_repository(temp: &Path, name: &str) -> (HostedRepositorySpec, String, PathBuf) {
        let seed = temp.join(format!("{name}-seed"));
        fs::create_dir(&seed).unwrap();
        git(Some(&seed), &["init", "--initial-branch", "main"]);
        git(Some(&seed), &["config", "user.name", "Cara Test"]);
        git(
            Some(&seed),
            &["config", "user.email", "cara@example.invalid"],
        );
        fs::write(seed.join("README.md"), format!("# {name}\n")).unwrap();
        git(Some(&seed), &["add", "README.md"]);
        git(Some(&seed), &["commit", "-m", "initial"]);
        let head = git(Some(&seed), &["rev-parse", "HEAD"]);

        let origin = temp.join(format!("{name}.git"));
        git(None, &["init", "--bare", origin.to_string_lossy().as_ref()]);
        git(
            Some(&seed),
            &["remote", "add", "origin", origin.to_string_lossy().as_ref()],
        );
        git(Some(&seed), &["push", "-u", "origin", "main"]);
        git(
            None,
            &[
                "--git-dir",
                origin.to_string_lossy().as_ref(),
                "symbolic-ref",
                "HEAD",
                "refs/heads/main",
            ],
        );
        (
            HostedRepositorySpec {
                host: "github.test".to_owned(),
                owner: "owner".to_owned(),
                name: name.to_owned(),
                remote_url: origin.to_string_lossy().into_owned(),
            },
            head,
            seed,
        )
    }

    fn test_config(root: &Path) -> HostedCloneCacheConfig {
        HostedCloneCacheConfig {
            root: root.to_path_buf(),
            installation_id: 42,
            max_bytes: 128 * 1024 * 1024,
            max_age: Duration::from_secs(3_600),
            max_entries: 8,
            max_jobs: 8,
            max_duration: Duration::from_secs(120),
        }
    }

    #[test]
    fn bootstrap_requires_exact_app_identity_before_network_access() {
        assert!(
            validate_bootstrap_app_values(
                Some("app_installation"),
                Some("cara-app"),
                Some("42"),
                42,
            )
            .is_ok()
        );
        assert_eq!(
            validate_bootstrap_app_values(None, Some("cara-app"), Some("42"), 42)
                .unwrap_err()
                .code(),
            "hosted_clone_app_auth_required"
        );
        assert_eq!(
            validate_bootstrap_app_values(
                Some("app_installation"),
                Some("cara-app"),
                Some("43"),
                42,
            )
            .unwrap_err()
            .code(),
            "hosted_clone_installation_mismatch"
        );
    }

    #[test]
    fn repository_slug_and_remote_default_are_strict() {
        assert!(HostedRepositorySpec::github("owner/repo").is_ok());
        for invalid in [
            "",
            "owner",
            "owner/repo/more",
            "../repo",
            "owner/..",
            "owner/re po",
        ] {
            assert!(HostedRepositorySpec::github(invalid).is_err(), "{invalid}");
        }
        assert_eq!(
            parse_remote_default(
                "ref: refs/heads/main\tHEAD\n0123456789012345678901234567890123456789\tHEAD\n"
            ),
            Some(RemoteDefaultProof {
                default_branch: "main".to_owned(),
                head_sha: "0123456789012345678901234567890123456789".to_owned(),
                object_format: "sha1".to_owned(),
            })
        );
        assert!(parse_remote_default("0123456789012345678901234567890123456789\tHEAD\n").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cache_root_and_lock_refuse_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        let linked = temp.path().join("linked");
        symlink(&real, &linked).unwrap();
        assert_eq!(
            prepare_root(&linked).unwrap_err().code(),
            "hosted_clone_root_symlink"
        );

        let root = prepare_root(&temp.path().join("hosted")).unwrap();
        let lock_target = temp.path().join("lock-target");
        File::create(&lock_target).unwrap();
        let lock_link = root.join("locks/linked.lock");
        symlink(&lock_target, &lock_link).unwrap();
        assert_eq!(
            acquire_lock(&lock_link, Duration::from_millis(1))
                .unwrap_err()
                .code(),
            "hosted_clone_lock_symlink"
        );
    }

    #[test]
    fn holderless_job_cleanup_preserves_a_locked_active_job() {
        let temp = tempfile::tempdir().unwrap();
        let root = prepare_root(temp.path()).unwrap();
        let stale = root.join("jobs/stale");
        create_contained_dir(&root, &stale).unwrap();
        File::create(stale.join(JOB_LOCK)).unwrap();
        let active = root.join("jobs/active");
        create_contained_dir(&root, &active).unwrap();
        let active_lock = File::create(active.join(JOB_LOCK)).unwrap();
        active_lock.lock_exclusive().unwrap();

        cleanup_holderless_jobs(&root).unwrap();
        assert!(!stale.exists());
        assert!(active.exists());
        FileExt::unlock(&active_lock).unwrap();
    }

    #[test]
    fn isolated_jobs_reuse_only_dissociated_objects_and_cleanup_by_lease() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, expected_head, _) = local_repository(temp.path(), "repo");
        let config = test_config(&temp.path().join("hosted"));
        let root = prepare_root(&config.root).unwrap();
        let runner = LocalRunner::default();

        let first = materialize_one(&config, &root, &spec, &runner).unwrap();
        let second = materialize_one(&config, &root, &spec, &runner).unwrap();
        assert_ne!(first.path(), second.path());
        assert!(!first.status().cache_hit);
        assert!(second.status().cache_hit);
        assert_eq!(first.status().repository, "owner/repo");
        assert_eq!(first.status().expected_head, expected_head);
        assert!(first.status().cache_bytes > 0);
        assert!(first.status().exact_ref_verified);
        assert!(first.status().credential_transport_verified);
        for job in [&first, &second] {
            assert_eq!(git(Some(job.path()), &["rev-parse", "HEAD"]), expected_head);
            assert!(job.path().join(".git").is_dir());
            assert!(!job.path().join(".git/objects/info/alternates").exists());
            assert_eq!(
                git(Some(job.path()), &["config", "--get", "remote.origin.url"]),
                spec.remote_url
            );
            let local_config = fs::read_to_string(job.path().join(".git/config")).unwrap();
            assert!(!local_config.contains("credential"));
            assert!(!local_config.contains("@github"));
        }
        assert_eq!(count_active_jobs(&root).unwrap(), 2);
        let calls = runner.calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .all(|call| call.inferred_write_intent().is_none())
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.args.iter().any(|arg| arg == "--mirror"))
                .count(),
            1,
            "the second job must reuse the one validated cache entry"
        );
        drop(calls);
        drop(first);
        assert_eq!(count_active_jobs(&root).unwrap(), 1);
        drop(second);
        assert_eq!(count_active_jobs(&root).unwrap(), 0);
    }

    #[test]
    fn concurrent_jobs_and_repositories_never_share_writable_state() {
        let temp = tempfile::tempdir().unwrap();
        let (first_spec, _, _) = local_repository(temp.path(), "first");
        let (second_spec, _, _) = local_repository(temp.path(), "second");
        let config = test_config(&temp.path().join("hosted"));
        let root = prepare_root(&config.root).unwrap();
        let runner = LocalRunner::default();

        let (first, second) = thread::scope(|scope| {
            let first_config = config.clone();
            let first_root = root.clone();
            let first_spec = first_spec.clone();
            let first_runner = runner.clone();
            let first = scope.spawn(move || {
                materialize_one(&first_config, &first_root, &first_spec, &first_runner).unwrap()
            });
            let second_config = config.clone();
            let second_root = root.clone();
            let second_spec = second_spec.clone();
            let second_runner = runner.clone();
            let second = scope.spawn(move || {
                materialize_one(&second_config, &second_root, &second_spec, &second_runner).unwrap()
            });
            (first.join().unwrap(), second.join().unwrap())
        });
        assert_ne!(first.path(), second.path());
        assert_ne!(
            git(
                Some(first.path()),
                &["config", "--get", "remote.origin.url"]
            ),
            git(
                Some(second.path()),
                &["config", "--get", "remote.origin.url"]
            )
        );
        assert_eq!(count_active_jobs(&root).unwrap(), 2);
        assert_eq!(
            fs::read_dir(root.join("cache"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .count(),
            2
        );
    }

    #[test]
    fn cache_entry_bound_evicts_old_identity_without_touching_active_jobs() {
        let temp = tempfile::tempdir().unwrap();
        let (first_spec, _, _) = local_repository(temp.path(), "first-bound");
        let (second_spec, _, _) = local_repository(temp.path(), "second-bound");
        let mut config = test_config(&temp.path().join("hosted"));
        config.max_entries = 1;
        let root = prepare_root(&config.root).unwrap();
        let runner = LocalRunner::default();
        let first = materialize_one(&config, &root, &first_spec, &runner).unwrap();
        let second = materialize_one(&config, &root, &second_spec, &runner).unwrap();
        assert!(first.path().exists());
        assert!(second.path().exists());
        assert_eq!(
            fs::read_dir(root.join("cache"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .count(),
            1
        );
    }

    #[test]
    fn stale_cache_is_rebuilt_and_active_job_limit_refuses_new_work() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _, _) = local_repository(temp.path(), "stale");
        let mut config = test_config(&temp.path().join("hosted"));
        config.max_age = Duration::from_nanos(1);
        config.max_jobs = 1;
        let root = prepare_root(&config.root).unwrap();
        let runner = LocalRunner::default();
        let first = materialize_one(&config, &root, &spec, &runner).unwrap();
        thread::sleep(Duration::from_millis(2));
        let error = materialize_one(&config, &root, &spec, &runner).unwrap_err();
        assert_eq!(error.code(), "hosted_clone_job_limit");
        assert!(first.path().exists());
        assert_eq!(
            runner
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.args.iter().any(|argument| argument == "--mirror"))
                .count(),
            2,
            "the expired object hint is rebuilt before the job limit is checked"
        );
    }

    #[test]
    fn default_movement_during_clone_quarantines_the_job() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _, seed) = local_repository(temp.path(), "moving");
        let config = test_config(&temp.path().join("hosted"));
        let root = prepare_root(&config.root).unwrap();
        let runner = MovingRunner {
            inner: LocalRunner::default(),
            seed,
            moved: Arc::new(AtomicBool::new(false)),
        };
        let error = materialize_one(&config, &root, &spec, &runner).unwrap_err();
        assert_eq!(error.code(), "hosted_clone_default_moved");
        assert_eq!(count_active_jobs(&root).unwrap(), 0);
        assert!(
            fs::read_dir(root.join("quarantine"))
                .unwrap()
                .next()
                .is_some()
        );
    }

    #[test]
    fn corrupt_cache_is_quarantined_and_rebuilt_without_provider_write() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _, _) = local_repository(temp.path(), "corrupt");
        let config = test_config(&temp.path().join("hosted"));
        let root = prepare_root(&config.root).unwrap();
        let runner = LocalRunner::default();
        drop(materialize_one(&config, &root, &spec, &runner).unwrap());
        let cache = fs::read_dir(root.join("cache"))
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| entry.file_type().unwrap().is_dir())
            .unwrap()
            .path();
        fs::write(cache.join(CACHE_RECEIPT), b"not-json").unwrap();

        let job = materialize_one(&config, &root, &spec, &runner).unwrap();
        assert!(job.path().join(".git").is_dir());
        assert!(
            fs::read_dir(root.join("quarantine"))
                .unwrap()
                .next()
                .is_some()
        );
        assert!(
            runner
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|call| call.inferred_write_intent().is_none())
        );
    }

    #[test]
    fn persisted_cache_or_job_transport_is_rejected_without_echoing_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _, _) = local_repository(temp.path(), "transport");
        let config = test_config(&temp.path().join("hosted"));
        let root = prepare_root(&config.root).unwrap();
        let runner = LocalRunner::default();
        let job = materialize_one(&config, &root, &spec, &runner).unwrap();
        git(
            Some(job.path()),
            &["config", "credential.helper", "/private/credential-helper"],
        );
        let remote = read_remote_default(&spec, &runner).unwrap();
        assert_eq!(
            verify_job(&root, &spec, &remote, job.path(), &runner)
                .unwrap_err()
                .code(),
            "hosted_clone_job_credential_persisted"
        );
        drop(job);

        let cache = fs::read_dir(root.join("cache"))
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| entry.file_type().unwrap().is_dir())
            .unwrap()
            .path();
        git(
            None,
            &[
                "--git-dir",
                cache.to_string_lossy().as_ref(),
                "config",
                "credential.helper",
                "/private/credential-helper",
            ],
        );
        let rebuilt = materialize_one(&config, &root, &spec, &runner).unwrap();
        assert!(rebuilt.path().exists());
        assert!(
            fs::read_dir(root.join("quarantine"))
                .unwrap()
                .next()
                .is_some()
        );
        let calls = runner.calls.lock().unwrap();
        assert!(calls.iter().all(|call| {
            !call.display().contains("/private/credential-helper")
                && call.inferred_write_intent().is_none()
        }));
    }

    #[test]
    fn byte_and_job_bounds_fail_before_returning_an_unfenced_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _, _) = local_repository(temp.path(), "bounded");
        let mut config = test_config(&temp.path().join("hosted"));
        config.max_bytes = 1;
        let root = prepare_root(&config.root).unwrap();
        let error = materialize_one(&config, &root, &spec, &LocalRunner::default()).unwrap_err();
        assert!(matches!(
            error.code().as_str(),
            "hosted_clone_cache_quota_exceeded" | "hosted_clone_byte_limit"
        ));
        assert_eq!(count_active_jobs(&root).unwrap(), 0);
    }

    #[test]
    fn cache_key_binds_installation_format_and_default_branch() {
        let temp = tempfile::tempdir().unwrap();
        let spec = HostedRepositorySpec::github("owner/repo").unwrap();
        let remote = RemoteDefaultProof {
            default_branch: "main".to_owned(),
            head_sha: "a".repeat(40),
            object_format: "sha1".to_owned(),
        };
        let config = HostedCloneCacheConfig {
            root: temp.path().to_path_buf(),
            installation_id: 42,
            max_bytes: 1,
            max_age: Duration::from_secs(1),
            max_entries: 1,
            max_jobs: 1,
            max_duration: Duration::from_secs(120),
        };
        let key = cache_key(&config, &spec, &remote);
        let mut changed = config.clone();
        changed.installation_id = 43;
        assert_ne!(key, cache_key(&changed, &spec, &remote));
        let mut moved = remote.clone();
        moved.default_branch = "trunk".to_owned();
        assert_ne!(key, cache_key(&config, &spec, &moved));
        moved.default_branch = "main".to_owned();
        moved.object_format = "sha256".to_owned();
        assert_ne!(key, cache_key(&config, &spec, &moved));
    }
}
