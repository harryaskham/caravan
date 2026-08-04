//! Built-in, path-scoped Cara web dashboard.
//!
//! The server is deliberately local-first: repository paths are explicit,
//! assets are embedded in the binary, and no repository discovery or external
//! web dependencies occur. Domain reads still flow through the same typed Cara
//! status implementation used by CLI/JSON/MCP.

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io::{Read, Write as IoWrite};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Args;
use hmac::{Hmac, Mac};
use mcp_cli::{ErrorCategory, StructuredError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::config::{CaravanConfig, DEFAULT_CONFIG_PATH};
use crate::graph::{CompatibilityChecker, GitCompatibilityChecker};
use crate::model::{BranchSnapshot, CompatibilityOutcome, PrNumber, PullRequestPrecondition};
use crate::read::StatusOutput;
use crate::repair::{
    RepairAbortInput, RepairContinueInput, RepairGrantInput, RepairRevokeGrantInput,
    RepairStartInput, RepairStatusInput,
};
use crate::{
    AppContext, AppError, CheckInput, CreateInput, EvictInput, JoinInput, PauseInput, ResumeInput,
    SplitInput, SyncInput,
};

const INDEX_HTML: &str = include_str!("web_assets/index.html");
const APP_CSS: &str = include_str!("web_assets/app.css");
const APP_JS: &str = include_str!("web_assets/app.js");
const WEB_SCHEMA_VERSION: u32 = 7;
const MIN_POLL_SECONDS: u64 = 2;
const MAX_POLL_SECONDS: u64 = 3_600;
const DEFAULT_POLL_SECONDS: u64 = 15;
const WEBHOOK_FALLBACK_POLL_SECONDS: u64 = 300;
const SERVER_TICK: Duration = Duration::from_millis(250);
const MAX_REQUEST_BODY_BYTES: u64 = 1024 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 32;
const MAX_ACTION_HISTORY: usize = 20;
const JOURNAL_SNAPSHOT_LIMIT: usize = 50;
const MAX_WEB_JOURNAL_BYTES: usize = 512 * 1024;
const MAX_WEBHOOK_DELIVERIES: usize = 1_000;
const MAX_WEBHOOK_STATE_BYTES: u64 = 256 * 1024;
const MAX_WEB_COMPATIBILITY_CANDIDATES: usize = 32;
const MAX_WEB_COMPATIBILITY_TARGETS: usize = 16;
const MAX_WEB_COMPATIBILITY_PAIRS: usize = 64;
const MAX_WEB_COMPATIBILITY_SECONDS: u64 = 30;
type WebhookHmac = Hmac<Sha256>;

/// Start a local dashboard over one or more explicit repository paths.
#[derive(Debug, Clone, Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct WebInput {
    /// Repository/worktree path to manage. Repeat for a multi-repository view.
    #[arg(long = "repo", value_name = "PATH", required = true)]
    pub repositories: Vec<PathBuf>,

    /// Loopback HTTP address. Non-loopback binds are refused in this release.
    #[arg(long, default_value = "127.0.0.1:4774", value_name = "ADDRESS")]
    pub listen: SocketAddr,

    /// Seconds between bounded status refresh passes.
    #[arg(long, default_value_t = DEFAULT_POLL_SECONDS, value_name = "SECONDS")]
    pub poll_seconds: u64,

    /// Disable every mutation endpoint while retaining refresh/status views.
    #[arg(long)]
    pub read_only: bool,

    /// Open the dashboard in the platform browser after binding.
    #[arg(long)]
    pub open: bool,

    /// Environment variable containing the GitHub webhook HMAC secret.
    #[arg(long, value_name = "ENV")]
    pub github_webhook_secret_env: Option<String>,

    /// Exact GitHub App installation ID accepted by the webhook endpoint.
    #[arg(long, requires = "github_webhook_secret_env")]
    pub github_installation_id: Option<u64>,

    /// Run one bounded sync-all tick for accepted webhook wakes; otherwise refresh only.
    #[arg(long, requires = "github_webhook_secret_env")]
    pub webhook_sync: bool,

    /// Hosted worker contract over explicit pre-provisioned repositories.
    #[arg(long, requires = "github_webhook_secret_env")]
    pub hosted: bool,
}

/// Secret-free error projection retained beside the most recent snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WebError {
    pub category: ErrorCategory,
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl WebError {
    fn from_app(error: &AppError) -> Self {
        Self {
            category: error.category(),
            code: error.code(),
            message: error.message(),
            details: error.details(),
        }
    }
}

/// Kind of exact destination considered for one unenrolled PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WebCompatibilityTargetKind {
    DefaultBranch,
    CaravanTail,
}

/// One exact candidate/destination mechanical compatibility result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WebCandidateTargetCompatibility {
    pub kind: WebCompatibilityTargetKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caravan_id: Option<PrNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_pr: Option<PrNumber>,
    pub target: BranchSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<CompatibilityOutcome>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicting_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WebError>,
}

/// Bounded exact target projection for one admission candidate generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WebCandidateCompatibility {
    pub pr: PrNumber,
    pub candidate: BranchSnapshot,
    pub generation_fingerprint: String,
    pub complete: bool,
    pub targets_truncated: usize,
    pub targets: Vec<WebCandidateTargetCompatibility>,
}

/// One repository's latest periodic typed Cara status.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebRepositorySnapshot {
    pub id: String,
    pub path: String,
    pub config_path: String,
    pub config_existed: bool,
    /// Effective parsed configuration with hook command bodies redacted.
    pub effective_config: serde_json::Value,
    pub refreshed_unix_ms: u64,
    pub refresh_sequence: u64,
    pub refreshing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusOutput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WebError>,
    /// Exact default/tail compatibility for bounded unenrolled admission candidates.
    #[serde(default)]
    pub candidate_compatibility: Vec<WebCandidateCompatibility>,
    /// Admission candidates omitted by the bounded web compatibility projection.
    #[serde(default)]
    pub candidate_compatibility_truncated: usize,
    /// Most recent bounded typed action result, retained for operational evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_action: Option<WebActionRecord>,
    /// Bounded durable Cara event/hook journal snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal: Option<crate::journal::LogOutput>,
    /// Bounded in-memory action jobs, newest last.
    #[serde(default)]
    pub actions: Vec<WebActionJob>,
}

/// Bounded action evidence retained with the repository snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebActionRecord {
    pub completed_unix_ms: u64,
    pub action: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WebError>,
}

/// Lifecycle state for one asynchronous dashboard action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WebActionJobState {
    Queued,
    Running,
    Succeeded,
    Failed,
}

impl WebActionJobState {
    fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

/// Bounded progress and terminal evidence for one typed action.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebActionJob {
    pub id: String,
    pub action: String,
    pub expected_refresh_sequence: u64,
    /// Exact mutation-authority facts reviewed when the action was accepted.
    pub expected_mutation_fingerprint: String,
    /// Fresh authority fingerprint observed after action/refresh locks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_mutation_fingerprint: Option<String>,
    pub state: WebActionJobState,
    pub started_unix_ms: u64,
    pub updated_unix_ms: u64,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<crate::operation_lock::OperationLockCheckpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WebError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_sequence: Option<u64>,
}

/// Secret-free webhook receiver health and bounded counters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct WebhookStatus {
    pub enabled: bool,
    pub sync_enabled: bool,
    pub accepted: u64,
    pub deduplicated: u64,
    pub rejected: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delivery: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_received_unix_ms: Option<u64>,
}

/// Stable dashboard state returned to the embedded application.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebState {
    pub schema_version: u32,
    pub generated_unix_ms: u64,
    pub started_unix_ms: u64,
    pub listen: String,
    pub poll_seconds: u64,
    pub read_only: bool,
    pub hosted: bool,
    pub webhook: WebhookStatus,
    /// Same-origin token required as `X-Cara-CSRF` on POST requests.
    pub csrf_token: String,
    pub repositories: Vec<WebRepositorySnapshot>,
}

/// Exact-snapshot action request accepted by the same-origin web API.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebActionRequest {
    pub expected_refresh_sequence: u64,
    #[serde(flatten)]
    pub action: WebAction,
}

/// Strict typed Cara operations exposed by the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", content = "input", rename_all = "snake_case")]
pub enum WebAction {
    Check(CheckInput),
    PlanSync(SyncInput),
    PlanConcat(crate::concat::ConcatInput),
    Concat(crate::concat::ConcatExecuteInput),
    Sync(SyncInput),
    Join(JoinInput),
    Rejoin(JoinInput),
    New(CreateInput),
    Renew(CreateInput),
    ForceArm(crate::force::ForceIntentInput),
    ForceRevoke(crate::force::ForceIntentInput),
    PrioritySet(crate::priority::PrioritySetInput),
    PriorityClear(crate::priority::PriorityClearInput),
    Split(SplitInput),
    Evict(EvictInput),
    Pause(PauseInput),
    Resume(ResumeInput),
    PauseRecoveryPrepare(crate::pause::PauseRecoveryInput),
    PauseRecoveryCheckpointBase(crate::pause::PauseRecoveryInput),
    PauseRecoveryCheckpointHead(crate::pause::PauseRecoveryInput),
    PauseRecoveryFinalize(crate::pause::PauseRecoveryInput),
    PauseRecoveryRollback(crate::pause::PauseRecoveryInput),
    RepairStart(RepairStartInput),
    RepairContinue(RepairContinueInput),
    RepairStatus(RepairStatusInput),
    RepairAbort(RepairAbortInput),
    RepairGrant(RepairGrantInput),
    RepairRevokeGrant(RepairRevokeGrantInput),
}

impl WebAction {
    fn name(&self) -> &'static str {
        match self {
            Self::Check(_) => "check",
            Self::PlanSync(_) => "plan_sync",
            Self::PlanConcat(_) => "plan_concat",
            Self::Concat(_) => "concat",
            Self::Sync(_) => "sync",
            Self::Join(_) => "join",
            Self::Rejoin(_) => "rejoin",
            Self::New(_) => "new",
            Self::Renew(_) => "renew",
            Self::ForceArm(_) => "force_arm",
            Self::ForceRevoke(_) => "force_revoke",
            Self::PrioritySet(_) => "priority_set",
            Self::PriorityClear(_) => "priority_clear",
            Self::Split(_) => "split",
            Self::Evict(_) => "evict",
            Self::Pause(_) => "pause",
            Self::Resume(_) => "resume",
            Self::PauseRecoveryPrepare(_) => "pause_recovery_prepare",
            Self::PauseRecoveryCheckpointBase(_) => "pause_recovery_checkpoint_base",
            Self::PauseRecoveryCheckpointHead(_) => "pause_recovery_checkpoint_head",
            Self::PauseRecoveryFinalize(_) => "pause_recovery_finalize",
            Self::PauseRecoveryRollback(_) => "pause_recovery_rollback",
            Self::RepairStart(_) => "repair_start",
            Self::RepairContinue(_) => "repair_continue",
            Self::RepairStatus(_) => "repair_status",
            Self::RepairAbort(_) => "repair_abort",
            Self::RepairGrant(_) => "repair_grant",
            Self::RepairRevokeGrant(_) => "repair_revoke_grant",
        }
    }

    fn mutates(&self) -> bool {
        !matches!(
            self,
            Self::Check(_) | Self::PlanSync(_) | Self::PlanConcat(_) | Self::RepairStatus(_)
        )
    }
}

struct AcceptedWebAction {
    request: WebActionRequest,
    expected_mutation_fingerprint: String,
}

struct RepositoryEntry {
    id: String,
    context: AppContext,
    snapshot: Mutex<WebRepositorySnapshot>,
    refresh_lock: Mutex<()>,
    action_lock: Mutex<()>,
    actions: Mutex<VecDeque<WebActionJob>>,
    webhook_deliveries: Mutex<VecDeque<String>>,
    webhook_delivery_path: PathBuf,
    webhook_sync_pending: AtomicBool,
}

struct Dashboard {
    listen: SocketAddr,
    poll_seconds: u64,
    read_only: bool,
    hosted: bool,
    csrf_token: String,
    started_unix_ms: u64,
    repositories: Vec<Arc<RepositoryEntry>>,
    webhook_secret: Option<Vec<u8>>,
    webhook_installation_id: Option<u64>,
    webhook_sync: bool,
    webhook_status: Mutex<WebhookStatus>,
    stopping: AtomicBool,
    active_requests: AtomicUsize,
}

struct RequestActivity<'a>(&'a AtomicUsize);

impl Drop for RequestActivity<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Dashboard {
    fn state(&self) -> WebState {
        WebState {
            schema_version: WEB_SCHEMA_VERSION,
            generated_unix_ms: unix_ms(),
            started_unix_ms: self.started_unix_ms,
            listen: self.listen.to_string(),
            poll_seconds: self.poll_seconds,
            read_only: self.read_only,
            hosted: self.hosted,
            webhook: self
                .webhook_status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            csrf_token: self.csrf_token.clone(),
            repositories: self
                .repositories
                .iter()
                .map(|repository| {
                    let mut snapshot = repository
                        .snapshot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    snapshot.actions = action_jobs_with_checkpoint(repository);
                    snapshot
                })
                .collect(),
        }
    }

    fn repository(&self, id: &str) -> Option<&Arc<RepositoryEntry>> {
        self.repositories
            .iter()
            .find(|repository| repository.id == id)
    }

    fn refresh_all(&self) {
        for repository in &self.repositories {
            refresh_repository(repository);
            if self.stopping.load(Ordering::Relaxed) {
                break;
            }
        }
    }
}

/// Validate input, load repositories, and build the serving dashboard.
fn build_dashboard(input: &WebInput) -> Result<Arc<Dashboard>, AppError> {
    validate_input(input)?;
    let webhook_secret = load_webhook_secret(input)?;
    let webhook_enabled = webhook_secret.is_some();
    let poll_seconds = if webhook_enabled && input.poll_seconds == DEFAULT_POLL_SECONDS {
        WEBHOOK_FALLBACK_POLL_SECONDS
    } else {
        input.poll_seconds
    };
    let repositories = load_repositories(&input.repositories)?;
    validate_hosted_repositories(input, &repositories)?;
    Ok(Arc::new(Dashboard {
        listen: input.listen,
        poll_seconds,
        read_only: input.read_only,
        hosted: input.hosted,
        csrf_token: uuid::Uuid::now_v7().to_string(),
        started_unix_ms: unix_ms(),
        repositories,
        webhook_secret,
        webhook_installation_id: input.github_installation_id,
        webhook_sync: input.webhook_sync,
        webhook_status: Mutex::new(WebhookStatus {
            enabled: webhook_enabled,
            sync_enabled: input.webhook_sync,
            ..WebhookStatus::default()
        }),
        stopping: AtomicBool::new(false),
        active_requests: AtomicUsize::new(0),
    }))
}

/// Serve until SIGINT/SIGTERM sets the foreground stop flag.
pub fn serve(input: &WebInput) -> Result<(), AppError> {
    let dashboard = build_dashboard(input)?;
    dashboard.refresh_all();

    let server = Server::http(input.listen).map_err(|error| {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "web_bind_failed",
            format!(
                "could not bind Cara web dashboard at {}: {error}",
                input.listen
            ),
            Some(json!({"listen": input.listen, "loopback_required": true})),
        )
    })?;
    let stop = Arc::clone(&dashboard);
    ctrlc::set_handler(move || stop.stopping.store(true, Ordering::SeqCst)).map_err(|error| {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "web_signal_handler_failed",
            error.to_string(),
            None,
        )
    })?;

    let poller = spawn_poller(Arc::clone(&dashboard));
    if input.open {
        open_browser(input.listen);
    }
    eprintln!("Cara web dashboard: http://{}", input.listen);
    eprintln!(
        "Repositories: {} · poll={}s · mutations={}",
        dashboard.repositories.len(),
        dashboard.poll_seconds,
        if dashboard.read_only {
            "disabled"
        } else if dashboard.hosted {
            "webhook-only"
        } else {
            "enabled"
        }
    );

    while !dashboard.stopping.load(Ordering::Relaxed) {
        match server.recv_timeout(SERVER_TICK) {
            Ok(Some(request)) => {
                if dashboard.active_requests.fetch_add(1, Ordering::SeqCst)
                    >= MAX_CONCURRENT_REQUESTS
                {
                    dashboard.active_requests.fetch_sub(1, Ordering::SeqCst);
                    let _ = request.respond(error_response(
                        StatusCode(503),
                        "web_request_limit",
                        "too many concurrent dashboard requests",
                    ));
                    continue;
                }
                let state = Arc::clone(&dashboard);
                thread::spawn(move || {
                    let _activity = RequestActivity(&state.active_requests);
                    handle_request(request, &state);
                });
            }
            Ok(None) => {}
            Err(error) => {
                dashboard.stopping.store(true, Ordering::SeqCst);
                let _ = poller.join();
                return Err(AppError::structured(
                    ErrorCategory::ExecutionFailure,
                    "web_accept_failed",
                    error.to_string(),
                    None,
                ));
            }
        }
    }
    let _ = poller.join();
    Ok(())
}

fn load_webhook_secret(input: &WebInput) -> Result<Option<Vec<u8>>, AppError> {
    input
        .github_webhook_secret_env
        .as_ref()
        .map(|name| {
            std::env::var(name)
                .map_err(|_| {
                    AppError::validation(
                        "webhook_secret_unavailable",
                        format!("webhook secret environment variable `{name}` is unset"),
                    )
                })
                .and_then(|secret| {
                    if secret.len() < 16 {
                        Err(AppError::validation(
                            "webhook_secret_invalid",
                            "webhook secret must contain at least 16 bytes",
                        ))
                    } else {
                        Ok(secret.into_bytes())
                    }
                })
        })
        .transpose()
}

fn validate_input(input: &WebInput) -> Result<(), AppError> {
    if !is_loopback(input.listen.ip()) {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "web_non_loopback_refused",
            "Cara web currently binds only to a loopback address",
            Some(json!({
                "listen": input.listen,
                "safe_next_action": "use --listen 127.0.0.1:PORT or an SSH tunnel"
            })),
        ));
    }
    if !(MIN_POLL_SECONDS..=MAX_POLL_SECONDS).contains(&input.poll_seconds) {
        return Err(AppError::validation(
            "web_poll_interval_invalid",
            format!("--poll-seconds must be between {MIN_POLL_SECONDS} and {MAX_POLL_SECONDS}"),
        ));
    }
    if input.webhook_sync && input.read_only {
        return Err(AppError::validation(
            "webhook_sync_read_only_conflict",
            "--webhook-sync cannot be enabled with --read-only",
        ));
    }
    if input.github_webhook_secret_env.is_some() && input.github_installation_id.is_none() {
        return Err(AppError::validation(
            "webhook_installation_required",
            "webhook receiver requires --github-installation-id",
        ));
    }
    if input
        .github_webhook_secret_env
        .as_deref()
        .is_some_and(|name| name.trim().is_empty())
    {
        return Err(AppError::validation(
            "webhook_secret_env_invalid",
            "webhook secret environment variable name must be non-empty",
        ));
    }
    if input.repositories.is_empty() {
        return Err(AppError::validation(
            "web_repository_required",
            "pass at least one explicit --repo PATH",
        ));
    }
    Ok(())
}

/// Decide whether one interactive same-origin action may mutate.
///
/// A hosted worker is reached through an operator proxy, so the same-origin
/// CSRF token is not authentication. Hosted mutations are therefore accepted
/// only from HMAC-verified webhook deliveries, never from this endpoint.
fn interactive_mutation_refusal(
    dashboard: &Dashboard,
    mutates: bool,
) -> Option<(&'static str, &'static str)> {
    if !mutates {
        return None;
    }
    if dashboard.hosted {
        return Some((
            "web_hosted_interactive_mutation_refused",
            "hosted workers mutate only from verified webhook deliveries",
        ));
    }
    if dashboard.read_only {
        return Some(("web_read_only", "mutation endpoints are disabled"));
    }
    None
}

/// Match one delivery to the repository the deployment declares.
///
/// The configured `repository: owner/name` is authoritative when set, so
/// routing does not depend on a successful provider status read: a repository
/// whose first read failed would otherwise match nothing and have its
/// deliveries rejected until a fallback poll healed it. Repositories with no
/// configured slug still match on observed status exactly as before.
fn repository_matches_slug(repository: &RepositoryEntry, repository_slug: &str) -> bool {
    if repository_slug.is_empty() {
        return false;
    }
    if let Some(configured) = repository.context.config.repository.as_deref() {
        return configured == repository_slug;
    }
    repository
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .status
        .as_ref()
        .is_some_and(|status| status.repository.slug() == repository_slug)
}

/// Secret-free operational health, derived from current snapshots.
///
/// `ok` keeps its existing meaning of "this process is serving", so alerting
/// that already watches it does not silently change meaning. `degraded` is the
/// new signal: it is true when any served repository has never refreshed
/// successfully or is currently carrying a refresh error, both of which leave a
/// hosted worker apparently healthy while doing no useful work.
fn health_payload(dashboard: &Dashboard) -> serde_json::Value {
    let mut never_refreshed = 0_u64;
    let mut erroring = 0_u64;
    let mut oldest_refresh_unix_ms: Option<u64> = None;
    for repository in &dashboard.repositories {
        let snapshot = repository
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if snapshot.status.is_none() {
            never_refreshed += 1;
        } else {
            oldest_refresh_unix_ms = Some(
                oldest_refresh_unix_ms.map_or(snapshot.refreshed_unix_ms, |oldest| {
                    oldest.min(snapshot.refreshed_unix_ms)
                }),
            );
        }
        if snapshot.error.is_some() {
            erroring += 1;
        }
    }
    let webhook = dashboard
        .webhook_status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    json!({
        "ok": true,
        "degraded": never_refreshed > 0 || erroring > 0,
        "schema_version": WEB_SCHEMA_VERSION,
        "hosted": dashboard.hosted,
        "read_only": dashboard.read_only,
        "repositories": dashboard.repositories.len(),
        "repositories_never_refreshed": never_refreshed,
        "repositories_erroring": erroring,
        "oldest_refresh_unix_ms": oldest_refresh_unix_ms,
        "started_unix_ms": dashboard.started_unix_ms,
        "webhook": {
            "enabled": webhook.enabled,
            "sync_enabled": webhook.sync_enabled,
            "accepted": webhook.accepted,
            "deduplicated": webhook.deduplicated,
            "rejected": webhook.rejected,
            "last_received_unix_ms": webhook.last_received_unix_ms,
        },
    })
}

fn is_loopback(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_loopback(),
        IpAddr::V6(address) => address.is_loopback(),
    }
}

/// Enforce the hosted worker contract over already-loaded repositories.
/// Ordinary local `cara web` keeps `ambient`/`local_only` behavior untouched.
fn validate_hosted_repositories(
    input: &WebInput,
    repositories: &[Arc<RepositoryEntry>],
) -> Result<(), AppError> {
    if !input.hosted {
        return Ok(());
    }
    if !input.webhook_sync {
        return Err(AppError::validation(
            "web_hosted_requires_webhook_sync",
            "--hosted requires --webhook-sync so accepted deliveries perform bounded work",
        ));
    }
    if input.read_only {
        return Err(AppError::validation(
            "web_hosted_read_only_conflict",
            "--hosted cannot be combined with --read-only",
        ));
    }
    let Some(expected_installation) = input.github_installation_id else {
        return Err(AppError::validation(
            "web_hosted_installation_required",
            "--hosted requires the exact --github-installation-id it serves",
        ));
    };
    let mut configured_slugs: std::collections::BTreeMap<&str, &Path> =
        std::collections::BTreeMap::new();
    for repository in repositories {
        let config = &repository.context.config;
        let path = &repository.context.repository_path;
        if config.github_auth.mode != crate::config::GithubAuthMode::AppInstallation {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "web_hosted_repository_auth_not_app",
                "--hosted requires github_auth.mode: app_installation for every served repository",
                Some(json!({"path": path, "github_auth_mode": "ambient"})),
            ));
        }
        if config.github_auth.installation_id != Some(expected_installation) {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "web_hosted_installation_mismatch",
                "every hosted repository must pin the exact installation this worker serves",
                Some(json!({
                    "path": path,
                    "expected_installation_id": expected_installation,
                    "repository_installation_id": config.github_auth.installation_id,
                })),
            ));
        }
        if config.writer.mode != crate::config::WriterMode::RemoteFenced {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "web_hosted_writer_not_fenced",
                "--hosted requires writer.mode: remote_fenced for every served repository",
                Some(json!({"path": path})),
            ));
        }
        if config.repository.is_none() {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "web_hosted_repository_slug_required",
                "--hosted requires exact repository: owner/name for every served repository",
                Some(json!({"path": path})),
            ));
        }
        // Deliveries route by configured slug, so two worktrees declaring the
        // same owner/name would make routing non-deterministic. Canonical path
        // deduplication cannot catch this because the paths differ.
        if let Some(slug) = config.repository.as_deref()
            && let Some(existing) = configured_slugs.insert(slug, path)
        {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "web_hosted_repository_slug_duplicate",
                "--hosted requires one served worktree per repository slug",
                Some(json!({"repository": slug, "paths": [existing, path]})),
            ));
        }
    }
    Ok(())
}

fn load_repositories(paths: &[PathBuf]) -> Result<Vec<Arc<RepositoryEntry>>, AppError> {
    let mut seen = BTreeSet::new();
    paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let canonical = path.canonicalize().map_err(|error| {
                AppError::structured(
                    ErrorCategory::Validation,
                    "web_repository_unavailable",
                    format!(
                        "could not resolve repository path {}: {error}",
                        path.display()
                    ),
                    Some(json!({"path": path})),
                )
            })?;
            if !canonical.is_dir() || !canonical.join(".git").exists() {
                return Err(AppError::structured(
                    ErrorCategory::Validation,
                    "web_repository_not_git",
                    "--repo must name an existing Git worktree root",
                    Some(json!({"path": canonical})),
                ));
            }
            if !seen.insert(canonical.clone()) {
                return Err(AppError::structured(
                    ErrorCategory::Validation,
                    "web_repository_duplicate",
                    "the same canonical repository path was supplied more than once",
                    Some(json!({"path": canonical})),
                ));
            }
            let config_path = canonical.join(DEFAULT_CONFIG_PATH);
            let config_existed = config_path.exists();
            let config = if config_existed {
                CaravanConfig::load(&config_path).map_err(|error| {
                    AppError::structured(
                        ErrorCategory::Validation,
                        "web_repository_config_invalid",
                        error.to_string(),
                        Some(json!({"config_path": &config_path})),
                    )
                })?
            } else {
                CaravanConfig::default()
            };
            config.validate_runtime_environment().map_err(|error| {
                AppError::structured(
                    ErrorCategory::Validation,
                    "web_repository_auth_policy_mismatch",
                    error.to_string(),
                    Some(json!({"config_path": &config_path})),
                )
            })?;
            let context = AppContext {
                repository_path: canonical.clone(),
                config_path: config_path.clone(),
                config_existed,
                config,
            };
            let effective_config = redacted_config(&context.config)?;
            let id = format!("repo-{}", index + 1);
            let webhook_delivery_path = webhook_delivery_path(&canonical)?;
            let webhook_deliveries = load_webhook_deliveries(&webhook_delivery_path);
            Ok(Arc::new(RepositoryEntry {
                id: id.clone(),
                context,
                snapshot: Mutex::new(WebRepositorySnapshot {
                    id,
                    path: canonical.display().to_string(),
                    config_path: config_path.display().to_string(),
                    config_existed,
                    effective_config,
                    refreshed_unix_ms: 0,
                    refresh_sequence: 0,
                    refreshing: false,
                    status: None,
                    error: None,
                    candidate_compatibility: Vec::new(),
                    candidate_compatibility_truncated: 0,
                    last_action: None,
                    journal: None,
                    actions: Vec::new(),
                }),
                refresh_lock: Mutex::new(()),
                action_lock: Mutex::new(()),
                actions: Mutex::new(VecDeque::new()),
                webhook_deliveries: Mutex::new(webhook_deliveries),
                webhook_delivery_path,
                webhook_sync_pending: AtomicBool::new(false),
            }))
        })
        .collect()
}

fn webhook_delivery_path(repository: &Path) -> Result<PathBuf, AppError> {
    let dot_git = repository.join(".git");
    let common = if dot_git.is_dir() {
        dot_git
    } else {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--git-common-dir"])
            .current_dir(repository)
            .output()
            .map_err(|error| {
                AppError::validation("webhook_state_unavailable", error.to_string())
            })?;
        if !output.status.success() {
            return Err(AppError::validation(
                "webhook_state_unavailable",
                String::from_utf8_lossy(&output.stderr).trim(),
            ));
        }
        let common = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if PathBuf::from(&common).is_absolute() {
            PathBuf::from(common)
        } else {
            repository.join(common)
        }
    };
    let directory = common.join("caravan/webhooks");
    fs::create_dir_all(&directory)
        .map_err(|error| AppError::validation("webhook_state_unavailable", error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).map_err(|error| {
            AppError::validation("webhook_state_unavailable", error.to_string())
        })?;
    }
    Ok(directory.join("deliveries.log"))
}

fn load_webhook_deliveries(path: &Path) -> VecDeque<String> {
    let Ok(file) = fs::File::open(path) else {
        return VecDeque::new();
    };
    let mut content = String::new();
    if file
        .take(MAX_WEBHOOK_STATE_BYTES)
        .read_to_string(&mut content)
        .is_err()
    {
        return VecDeque::new();
    }
    content
        .lines()
        .filter(|line| !line.is_empty() && line.len() <= 128)
        .rev()
        .take(MAX_WEBHOOK_DELIVERIES)
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn record_webhook_delivery(repository: &RepositoryEntry, delivery: &str) -> Result<bool, AppError> {
    persist_webhook_delivery(
        &repository.webhook_delivery_path,
        &repository.webhook_deliveries,
        delivery,
    )
}

fn persist_webhook_delivery(
    path: &Path,
    delivery_log: &Mutex<VecDeque<String>>,
    delivery: &str,
) -> Result<bool, AppError> {
    let mut deliveries = delivery_log
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if deliveries.iter().any(|existing| existing == delivery) {
        return Ok(true);
    }
    deliveries.push_back(delivery.to_owned());
    let rewrite = deliveries.len() > MAX_WEBHOOK_DELIVERIES;
    if rewrite {
        deliveries.pop_front();
    }
    let write = if rewrite {
        let body = deliveries.iter().cloned().collect::<Vec<_>>().join("\n") + "\n";
        fs::write(path, body)
    } else {
        let mut options = fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(path)
            .and_then(|mut file| writeln!(file, "{delivery}"))
    };
    write.map_err(|error| AppError::validation("webhook_state_write_failed", error.to_string()))?;
    Ok(false)
}

fn action_jobs_with_checkpoint(repository: &RepositoryEntry) -> Vec<WebActionJob> {
    let mut jobs = repository
        .actions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    if let Some(job) = jobs.iter_mut().rev().find(|job| !job.state.terminal())
        && let Ok(lock) = crate::operation_lock::inspect_lock(
            &repository.context.repository_path,
            crate::operation_lock::DEFAULT_STALE_AFTER,
        )
        && let Some(checkpoint) = lock.owner.and_then(|owner| owner.checkpoint)
    {
        job.phase.clone_from(&checkpoint.phase);
        job.updated_unix_ms = checkpoint.updated_unix_ms;
        job.checkpoint = Some(checkpoint);
    }
    jobs
}

fn enqueue_action_job(
    repository: &RepositoryEntry,
    job: WebActionJob,
) -> Result<(), Box<WebActionJob>> {
    let mut jobs = repository
        .actions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(active) = jobs.iter().rev().find(|job| !job.state.terminal()) {
        return Err(Box::new(active.clone()));
    }
    while jobs.len() >= MAX_ACTION_HISTORY {
        jobs.pop_front();
    }
    jobs.push_back(job);
    Ok(())
}

fn update_action_job(
    repository: &RepositoryEntry,
    id: &str,
    update: impl FnOnce(&mut WebActionJob),
) {
    let mut jobs = repository
        .actions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(job) = jobs.iter_mut().find(|job| job.id == id) {
        update(job);
        job.updated_unix_ms = unix_ms();
    }
}

fn redacted_config(config: &CaravanConfig) -> Result<serde_json::Value, AppError> {
    let mut value = serde_json::to_value(config).map_err(|error| {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "web_config_encode_failed",
            error.to_string(),
            None,
        )
    })?;
    if let Some(hooks) = value
        .get_mut("hooks")
        .and_then(serde_json::Value::as_object_mut)
    {
        for hook in hooks.values_mut() {
            if let Some(command) = hook
                .as_object_mut()
                .and_then(|object| object.get_mut("command"))
            {
                *command = serde_json::Value::String("<configured; redacted>".to_owned());
            }
        }
    }
    Ok(value)
}

fn mutation_authority_fingerprint(
    context: &AppContext,
    snapshot: &WebRepositorySnapshot,
) -> Option<String> {
    let status = snapshot.status.as_ref()?;
    if snapshot.error.is_some() {
        return None;
    }
    let pull_requests = status
        .analysis
        .pull_requests
        .iter()
        .map(|(number, pull_request)| {
            json!({
                "number": number,
                "precondition": PullRequestPrecondition::from(pull_request),
            })
        })
        .collect::<Vec<_>>();
    let merge_candidates = status
        .merge_candidates
        .iter()
        .map(|candidate| {
            json!({
                "pr": candidate.pr,
                "base": candidate.base,
                "head": candidate.head,
                "synthetic": candidate.synthetic,
                "freshness": candidate.freshness,
                "auto_merge": candidate.auto_merge,
            })
        })
        .collect::<Vec<_>>();
    let material = serde_json::to_vec(&json!({
        "schema_version": 1,
        "repository_path": context.repository_path,
        "config_path": context.config_path,
        "config_existed": context.config_existed,
        "config": context.config,
        "repository": status.repository,
        "default_branch": status.analysis.fleet.default_branch,
        "current_branch": status.current_branch,
        "current_pr": status.current_pr,
        "pull_requests": pull_requests,
        "caravans": status.analysis.fleet.caravans,
        "unqueued": status.analysis.fleet.unqueued,
        "problems": status.analysis.fleet.problems,
        "compatibility": status.analysis.compatibility,
        "initialization": status.initialization,
        "pauses": status.pauses,
        "merge_candidates": merge_candidates,
        "candidate_compatibility": snapshot.candidate_compatibility,
        "candidate_compatibility_truncated": snapshot.candidate_compatibility_truncated,
    }))
    .ok()?;
    Some(format!("sha256:{:x}", Sha256::digest(material)))
}

fn action_authority(repository: &RepositoryEntry) -> (u64, Option<String>) {
    let snapshot = repository
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (
        snapshot.refresh_sequence,
        mutation_authority_fingerprint(&repository.context, &snapshot),
    )
}

fn fresh_action_authority(
    repository: &RepositoryEntry,
    expected_refresh_sequence: u64,
) -> (u64, Option<String>) {
    let observed_sequence = repository
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .refresh_sequence;
    if observed_sequence != expected_refresh_sequence {
        // A queued poll/webhook refresh may have advanced only the dashboard
        // sequence. Force one fresh provider read under the retained refresh
        // lock before deciding whether mutation authority actually changed.
        crate::read::invalidate_status_cache(&repository.context);
        refresh_repository_locked(repository);
    }
    action_authority(repository)
}

fn validate_action_authority(
    expected_refresh_sequence: u64,
    expected_fingerprint: &str,
    actual_refresh_sequence: u64,
    actual_fingerprint: Option<String>,
) -> Result<String, AppError> {
    let Some(actual_fingerprint) = actual_fingerprint else {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "web_snapshot_unavailable",
            "fresh repository mutation-authority facts are unavailable",
            Some(json!({
                "expected_refresh_sequence": expected_refresh_sequence,
                "actual_refresh_sequence": actual_refresh_sequence,
                "expected_mutation_fingerprint": expected_fingerprint,
                "actual_mutation_fingerprint": null,
                "mutated": false,
                "safe_next_action": "wait for a successful status refresh, review it, then submit a new typed action"
            })),
        ));
    };
    if actual_refresh_sequence == expected_refresh_sequence
        || actual_fingerprint == expected_fingerprint
    {
        return Ok(actual_fingerprint);
    }
    Err(AppError::structured(
        ErrorCategory::Validation,
        "web_snapshot_stale",
        "mutation-sensitive repository facts changed before the queued action acquired its lock",
        Some(json!({
            "expected_refresh_sequence": expected_refresh_sequence,
            "actual_refresh_sequence": actual_refresh_sequence,
            "expected_mutation_fingerprint": expected_fingerprint,
            "actual_mutation_fingerprint": actual_fingerprint,
            "mutated": false,
            "safe_next_action": "review the fresh snapshot and changed authority fingerprint, then submit a new typed action"
        })),
    ))
}

fn bound_web_journal(mut output: crate::journal::LogOutput) -> crate::journal::LogOutput {
    while output.records.len() > 1
        && serde_json::to_vec(&output).map_or(usize::MAX, |bytes| bytes.len())
            > MAX_WEB_JOURNAL_BYTES
    {
        output.records.remove(0);
        output.truncated = true;
    }
    if serde_json::to_vec(&output).map_or(usize::MAX, |bytes| bytes.len()) > MAX_WEB_JOURNAL_BYTES {
        output.records.clear();
        output.truncated = true;
    }
    output
}

#[derive(Debug, Clone)]
struct WebCompatibilityTargetSpec {
    kind: WebCompatibilityTargetKind,
    caravan_id: Option<PrNumber>,
    tail_pr: Option<PrNumber>,
    branch: BranchSnapshot,
}

fn web_candidate_compatibility(
    context: &AppContext,
    status: &StatusOutput,
    previous: &[WebCandidateCompatibility],
) -> (Vec<WebCandidateCompatibility>, usize) {
    let timeout = Duration::from_secs(
        context
            .config
            .command_timeout_secs
            .clamp(1, MAX_WEB_COMPATIBILITY_SECONDS),
    );
    let checker = GitCompatibilityChecker::new(&context.repository_path, "origin")
        .with_timeout(timeout)
        .with_operation_deadline(std::time::Instant::now() + timeout);
    project_candidate_compatibility(
        status,
        &checker,
        MAX_WEB_COMPATIBILITY_CANDIDATES,
        MAX_WEB_COMPATIBILITY_TARGETS,
        MAX_WEB_COMPATIBILITY_PAIRS,
        previous,
    )
}

#[allow(clippy::too_many_lines)]
fn project_candidate_compatibility(
    status: &StatusOutput,
    checker: &impl CompatibilityChecker,
    max_candidates: usize,
    max_targets: usize,
    max_pairs: usize,
    previous: &[WebCandidateCompatibility],
) -> (Vec<WebCandidateCompatibility>, usize) {
    let mut target_specs = vec![WebCompatibilityTargetSpec {
        kind: WebCompatibilityTargetKind::DefaultBranch,
        caravan_id: None,
        tail_pr: None,
        branch: status.analysis.fleet.default_branch.clone(),
    }];
    target_specs.extend(status.analysis.fleet.caravans.iter().filter_map(|caravan| {
        let tail_pr = caravan.tail()?;
        let tail = status.analysis.pull_requests.get(&tail_pr)?;
        Some(WebCompatibilityTargetSpec {
            kind: WebCompatibilityTargetKind::CaravanTail,
            caravan_id: Some(caravan.id),
            tail_pr: Some(tail_pr),
            branch: tail.head.clone(),
        })
    }));
    target_specs.sort_by_key(|target| {
        (
            target.kind != WebCompatibilityTargetKind::DefaultBranch,
            target.tail_pr,
        )
    });
    target_specs.dedup_by(|left, right| {
        left.kind == right.kind && left.tail_pr == right.tail_pr && left.branch == right.branch
    });
    let targets_truncated = target_specs.len().saturating_sub(max_targets);
    target_specs.truncate(max_targets);

    let total_candidates = status.admission.candidates.len();
    let selected_candidates = status
        .admission
        .candidates
        .iter()
        .take(max_candidates)
        .filter_map(|candidate| {
            status
                .analysis
                .pull_requests
                .get(&candidate.pr)
                .map(|pull_request| (candidate.pr, pull_request.head.clone()))
        })
        .collect::<Vec<_>>();
    let candidates_truncated = total_candidates.saturating_sub(selected_candidates.len());
    let budget_error = || WebError {
        category: ErrorCategory::Validation,
        code: "web_compatibility_budget_exhausted".to_owned(),
        message: "bounded dashboard compatibility pair budget was exhausted".to_owned(),
        details: Some(json!({
            "max_candidates": max_candidates,
            "max_targets": max_targets,
            "max_pairs": max_pairs,
            "mutated": false,
        })),
    };

    let mut rows = selected_candidates
        .into_iter()
        .map(|(pr, candidate)| {
            let targets = target_specs
                .iter()
                .map(|target| WebCandidateTargetCompatibility {
                    kind: target.kind,
                    caravan_id: target.caravan_id,
                    tail_pr: target.tail_pr,
                    target: target.branch.clone(),
                    outcome: None,
                    conflicting_paths: Vec::new(),
                    diagnostic: None,
                    error: None,
                })
                .collect::<Vec<_>>();
            let material = serde_json::to_vec(&json!({
                "schema_version": 1,
                "pr": pr,
                "candidate": candidate,
                "targets": targets.iter().map(|target| json!({
                    "kind": target.kind,
                    "caravan_id": target.caravan_id,
                    "tail_pr": target.tail_pr,
                    "target": target.target,
                })).collect::<Vec<_>>(),
            }))
            .unwrap_or_default();
            WebCandidateCompatibility {
                pr,
                candidate,
                generation_fingerprint: format!("sha256:{:x}", Sha256::digest(material)),
                complete: false,
                targets_truncated,
                targets,
            }
        })
        .collect::<Vec<_>>();

    for row in &mut rows {
        if let Some(previous) = previous.iter().find(|previous| {
            previous.pr == row.pr
                && previous.complete
                && previous.generation_fingerprint == row.generation_fingerprint
        }) {
            row.clone_from(previous);
        }
    }

    let mut scheduled = Vec::new();
    let mut branches = Vec::new();
    for (candidate_index, row) in rows.iter_mut().enumerate() {
        if row.complete {
            continue;
        }
        for (target_index, target) in row.targets.iter_mut().enumerate() {
            if scheduled.len() >= max_pairs {
                target.error = Some(budget_error());
                continue;
            }
            scheduled.push((candidate_index, target_index));
            branches.push(row.candidate.clone());
            branches.push(target.target.clone());
        }
    }
    branches.sort_by(|left, right| (&left.name, &left.oid.0).cmp(&(&right.name, &right.oid.0)));
    branches.dedup();

    match checker.prepare(&branches) {
        Ok(()) => {
            for (candidate_index, target_index) in scheduled {
                let row = &mut rows[candidate_index];
                let target = &mut row.targets[target_index];
                match checker.check(&row.candidate, &target.target) {
                    Ok(report) => {
                        target.outcome = Some(report.outcome);
                        target.conflicting_paths = report.conflicting_paths;
                        target.diagnostic = report.diagnostic;
                    }
                    Err(error) => target.error = Some(WebError::from_app(&error)),
                }
            }
        }
        Err(error) => {
            let error = WebError::from_app(&error);
            for (candidate_index, target_index) in scheduled {
                rows[candidate_index].targets[target_index].error = Some(error.clone());
            }
        }
    }
    for row in &mut rows {
        row.complete = row.targets_truncated == 0
            && !row.targets.is_empty()
            && row.targets.iter().all(|target| target.outcome.is_some());
    }
    (rows, candidates_truncated)
}

fn has_active_action(repository: &RepositoryEntry) -> bool {
    repository
        .actions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|job| !job.state.terminal())
}

/// Refresh only when no accepted action owns the repository mutation window.
/// A worker performs one authoritative post-action refresh, so polling and
/// webhook refreshes safely coalesce behind queued/running work.
fn refresh_repository(repository: &RepositoryEntry) -> bool {
    if has_active_action(repository) {
        return false;
    }
    let _refresh = repository
        .refresh_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if has_active_action(repository) {
        return false;
    }
    refresh_repository_locked(repository);
    true
}

fn refresh_repository_locked(repository: &RepositoryEntry) {
    let previous_candidate_compatibility = {
        let mut snapshot = repository
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.refreshing = true;
        snapshot.candidate_compatibility.clone()
    };
    // Coalesce duplicate poll/manual refreshes inside this long-lived process.
    // Mutating action paths invalidate this cache and retain their own exact
    // provider preflight, so cached status is never mutation authority.
    let result = crate::read::status_cached(&repository.context, Duration::from_secs(5));
    let candidate_compatibility = result.as_ref().ok().map(|status| {
        web_candidate_compatibility(
            &repository.context,
            status,
            &previous_candidate_compatibility,
        )
    });
    let journal = crate::journal::snapshot(
        &repository.context,
        &crate::journal::LogInput {
            limit: JOURNAL_SNAPSHOT_LIMIT,
            kind: None,
            pr: None,
            since: None,
            until: None,
        },
    )
    .map(bound_web_journal);
    let mut snapshot = repository
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    snapshot.refresh_sequence = snapshot.refresh_sequence.saturating_add(1);
    snapshot.refreshed_unix_ms = unix_ms();
    snapshot.refreshing = false;
    if let Ok(journal) = journal {
        snapshot.journal = Some(journal);
    }
    match result {
        Ok(status) => {
            let (candidate_compatibility, truncated) =
                candidate_compatibility.unwrap_or_else(|| (Vec::new(), 0));
            snapshot.status = Some(status);
            snapshot.error = None;
            snapshot.candidate_compatibility = candidate_compatibility;
            snapshot.candidate_compatibility_truncated = truncated;
        }
        Err(error) => {
            snapshot.error = Some(WebError::from_app(&error));
            snapshot.candidate_compatibility.clear();
            snapshot.candidate_compatibility_truncated = 0;
        }
    }
}

fn spawn_poller(dashboard: Arc<Dashboard>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let interval = Duration::from_secs(dashboard.poll_seconds);
        while !sleep_until_stopped(&dashboard.stopping, interval) {
            dashboard.refresh_all();
        }
    })
}

fn sleep_until_stopped(stopping: &AtomicBool, duration: Duration) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < duration {
        if stopping.load(Ordering::Relaxed) {
            return true;
        }
        thread::sleep(SERVER_TICK.min(duration.saturating_sub(started.elapsed())));
    }
    stopping.load(Ordering::Relaxed)
}

fn handle_request(mut request: Request, dashboard: &Dashboard) {
    let method = request.method().clone();
    let path = request.url().split('?').next().unwrap_or("/").to_owned();
    let response = match (method, path.as_str()) {
        (Method::Get, "/") => static_response(INDEX_HTML, "text/html; charset=utf-8"),
        (Method::Get, "/assets/app.css") => static_response(APP_CSS, "text/css; charset=utf-8"),
        (Method::Get, "/assets/app.js") => {
            static_response(APP_JS, "text/javascript; charset=utf-8")
        }
        (Method::Get, "/api/v1/state") => json_response(StatusCode(200), &dashboard.state()),
        (Method::Get, "/api/v1/health") => {
            json_response(StatusCode(200), &health_payload(dashboard))
        }
        (Method::Post, "/api/v1/webhooks/github") => handle_github_webhook(&mut request, dashboard),
        (Method::Post, path) if path.ends_with("/refresh") => {
            handle_refresh(&request, dashboard, path)
        }
        (Method::Post, path) if path.ends_with("/action") => {
            handle_action(&mut request, dashboard, path)
        }
        _ => error_response(
            StatusCode(404),
            "web_route_not_found",
            "unknown Cara web route",
        ),
    };
    let _ = request.respond(response);
}

#[allow(clippy::too_many_lines)]
fn handle_github_webhook(
    request: &mut Request,
    dashboard: &Dashboard,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let Some(secret) = dashboard.webhook_secret.as_deref() else {
        return error_response(
            StatusCode(404),
            "webhook_disabled",
            "GitHub webhook receiver is not enabled",
        );
    };
    let signature = request_header(request, "X-Hub-Signature-256").unwrap_or_default();
    let delivery = request_header(request, "X-GitHub-Delivery").unwrap_or_default();
    let event = request_header(request, "X-GitHub-Event").unwrap_or_default();
    let valid_delivery = !delivery.is_empty()
        && delivery.len() <= 128
        && delivery
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if !valid_delivery || event.is_empty() || event.len() > 64 {
        record_webhook_rejection(dashboard);
        return error_response(
            StatusCode(400),
            "webhook_headers_invalid",
            "GitHub webhook delivery/event headers are missing or invalid",
        );
    }
    let mut body = Vec::new();
    let read = request
        .as_reader()
        .take(MAX_REQUEST_BODY_BYTES.saturating_add(1))
        .read_to_end(&mut body);
    if let Err(error) = read {
        record_webhook_rejection(dashboard);
        return error_response(
            StatusCode(400),
            "webhook_body_read_failed",
            &error.to_string(),
        );
    }
    if u64::try_from(body.len()).unwrap_or(u64::MAX) > MAX_REQUEST_BODY_BYTES {
        record_webhook_rejection(dashboard);
        return error_response(
            StatusCode(413),
            "webhook_body_too_large",
            "webhook payload exceeds the one MiB body limit",
        );
    }
    if !valid_webhook_signature(secret, &body, &signature) {
        record_webhook_rejection(dashboard);
        return error_response(
            StatusCode(401),
            "webhook_signature_invalid",
            "GitHub webhook HMAC verification failed",
        );
    }
    let payload = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(payload) => payload,
        Err(error) => {
            record_webhook_rejection(dashboard);
            return error_response(StatusCode(400), "webhook_json_invalid", &error.to_string());
        }
    };
    let installation = payload
        .get("installation")
        .and_then(|value| value.get("id"))
        .and_then(serde_json::Value::as_u64);
    if installation != dashboard.webhook_installation_id {
        record_webhook_rejection(dashboard);
        return error_response(
            StatusCode(403),
            "webhook_installation_mismatch",
            "webhook installation does not match the configured GitHub App installation",
        );
    }
    let repository_slug = payload
        .get("repository")
        .and_then(|value| value.get("full_name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let Some(repository) = dashboard
        .repositories
        .iter()
        .find(|repository| repository_matches_slug(repository, repository_slug))
    else {
        record_webhook_rejection(dashboard);
        return error_response(
            StatusCode(404),
            "webhook_repository_unknown",
            "webhook repository is not one of the explicit dashboard repositories",
        );
    };
    let duplicate = match record_webhook_delivery(repository, &delivery) {
        Ok(duplicate) => duplicate,
        Err(error) => {
            return error_response(StatusCode(500), error.code().as_str(), &error.message());
        }
    };
    if duplicate {
        let mut status = dashboard
            .webhook_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.deduplicated = status.deduplicated.saturating_add(1);
        return json_response(
            StatusCode(200),
            &json!({"ok": true, "accepted": false, "deduplicated": true, "delivery": delivery}),
        );
    }
    let default_branch = repository
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .status
        .as_ref()
        .map(|status| status.default_branch.clone())
        .unwrap_or_default();
    let wake = webhook_event_is_wake(&event, &payload, &default_branch);
    let mut coalesced = false;
    if wake {
        if dashboard.webhook_sync {
            coalesced = !enqueue_webhook_sync(repository);
        } else {
            crate::read::invalidate_status_cache(&repository.context);
            coalesced = !refresh_repository(repository);
        }
    }
    {
        let mut status = dashboard
            .webhook_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.accepted = status.accepted.saturating_add(1);
        status.last_event = Some(event.clone());
        status.last_delivery = Some(delivery.clone());
        status.last_received_unix_ms = Some(unix_ms());
    }
    json_response(
        StatusCode(202),
        &json!({
            "ok": true,
            "accepted": true,
            "wake": wake,
            "sync": dashboard.webhook_sync && wake,
            "coalesced": coalesced,
            "event": event,
            "delivery": delivery,
            "repository": repository_slug,
        }),
    )
}

fn request_header(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str().to_owned())
}

fn valid_webhook_signature(secret: &[u8], body: &[u8], signature: &str) -> bool {
    let Some(hex) = signature.strip_prefix("sha256=") else {
        return false;
    };
    if hex.len() != 64 {
        return false;
    }
    let mut bytes = [0_u8; 32];
    for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        let Some(high) = (pair[0] as char).to_digit(16) else {
            return false;
        };
        let Some(low) = (pair[1] as char).to_digit(16) else {
            return false;
        };
        bytes[index] = u8::try_from((high << 4) | low).unwrap_or_default();
    }
    WebhookHmac::new_from_slice(secret).is_ok_and(|mut mac| {
        mac.update(body);
        mac.verify_slice(&bytes).is_ok()
    })
}

fn webhook_event_is_wake(event: &str, payload: &serde_json::Value, default_branch: &str) -> bool {
    match event {
        "push" => payload
            .get("ref")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|reference| reference == format!("refs/heads/{default_branch}")),
        "pull_request" => payload
            .get("action")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|action| {
                matches!(
                    action,
                    "opened"
                        | "synchronize"
                        | "edited"
                        | "labeled"
                        | "unlabeled"
                        | "closed"
                        | "reopened"
                        | "ready_for_review"
                        | "converted_to_draft"
                )
            }),
        "pull_request_review" => payload
            .get("action")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|action| matches!(action, "submitted" | "edited" | "dismissed")),
        "check_run" | "check_suite" | "workflow_run" | "status" => true,
        _ => false,
    }
}

fn record_webhook_rejection(dashboard: &Dashboard) {
    let mut status = dashboard
        .webhook_status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    status.rejected = status.rejected.saturating_add(1);
}

fn enqueue_webhook_sync(repository: &Arc<RepositoryEntry>) -> bool {
    let (expected_refresh_sequence, expected_mutation_fingerprint) = action_authority(repository);
    let Some(expected_mutation_fingerprint) = expected_mutation_fingerprint else {
        repository
            .webhook_sync_pending
            .store(true, Ordering::SeqCst);
        return false;
    };
    let id = uuid::Uuid::now_v7().to_string();
    let job = WebActionJob {
        id: id.clone(),
        action: "sync".to_owned(),
        expected_refresh_sequence,
        expected_mutation_fingerprint: expected_mutation_fingerprint.clone(),
        actual_mutation_fingerprint: None,
        state: WebActionJobState::Queued,
        started_unix_ms: unix_ms(),
        updated_unix_ms: unix_ms(),
        phase: "webhook_queued".to_owned(),
        checkpoint: None,
        error: None,
        refresh_sequence: None,
    };
    if enqueue_action_job(repository, job).is_err() {
        repository
            .webhook_sync_pending
            .store(true, Ordering::SeqCst);
        return false;
    }
    let repository = Arc::clone(repository);
    thread::spawn(move || {
        execute_action_job(
            &repository,
            &id,
            AcceptedWebAction {
                request: WebActionRequest {
                    expected_refresh_sequence,
                    action: WebAction::Sync(SyncInput {
                        all: true,
                        rerun_failed: false,
                        dry_run: false,
                    }),
                },
                expected_mutation_fingerprint,
            },
        );
    });
    true
}

fn handle_refresh(
    request: &Request,
    dashboard: &Dashboard,
    path: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    if !csrf_valid(request, &dashboard.csrf_token) {
        return error_response(
            StatusCode(403),
            "web_csrf_invalid",
            "missing or invalid X-Cara-CSRF",
        );
    }
    let Some(id) = repository_id_from_path(path, "refresh") else {
        return error_response(
            StatusCode(404),
            "web_repository_not_found",
            "unknown repository",
        );
    };
    let Some(repository) = dashboard.repository(id) else {
        return error_response(
            StatusCode(404),
            "web_repository_not_found",
            "unknown repository",
        );
    };
    crate::read::invalidate_status_cache(&repository.context);
    refresh_repository(repository);
    let snapshot = repository
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    json_response(StatusCode(200), &snapshot)
}

#[allow(clippy::too_many_lines)]
fn handle_action(
    request: &mut Request,
    dashboard: &Dashboard,
    path: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    if !csrf_valid(request, &dashboard.csrf_token) {
        return error_response(
            StatusCode(403),
            "web_csrf_invalid",
            "missing or invalid X-Cara-CSRF",
        );
    }
    let Some(id) = repository_id_from_path(path, "action") else {
        return error_response(
            StatusCode(404),
            "web_repository_not_found",
            "unknown repository",
        );
    };
    let Some(repository) = dashboard.repository(id) else {
        return error_response(
            StatusCode(404),
            "web_repository_not_found",
            "unknown repository",
        );
    };
    let mut body = Vec::new();
    let read = request
        .as_reader()
        .take(MAX_REQUEST_BODY_BYTES.saturating_add(1))
        .read_to_end(&mut body);
    if let Err(error) = read {
        return error_response(StatusCode(400), "web_body_read_failed", &error.to_string());
    }
    if u64::try_from(body.len()).unwrap_or(u64::MAX) > MAX_REQUEST_BODY_BYTES {
        return error_response(
            StatusCode(413),
            "web_body_too_large",
            "action request exceeds the one MiB body limit",
        );
    }
    let action_request = match serde_json::from_slice::<WebActionRequest>(&body) {
        Ok(request) => request,
        Err(error) => {
            return error_response(StatusCode(400), "web_action_invalid", &error.to_string());
        }
    };
    if let Some((code, message)) =
        interactive_mutation_refusal(dashboard, action_request.action.mutates())
    {
        return error_response(StatusCode(403), code, message);
    }
    let (actual_sequence, expected_mutation_fingerprint) = action_authority(repository);
    if actual_sequence != action_request.expected_refresh_sequence {
        return error_response(
            StatusCode(409),
            "web_snapshot_stale",
            "repository snapshot changed; refresh and review exact facts before retrying",
        );
    }
    let Some(expected_mutation_fingerprint) = expected_mutation_fingerprint else {
        return error_response(
            StatusCode(409),
            "web_snapshot_unavailable",
            "repository mutation-authority facts are unavailable; wait for a successful refresh",
        );
    };
    let action_name = action_request.action.name().to_owned();
    let action_id = uuid::Uuid::now_v7().to_string();
    let job = WebActionJob {
        id: action_id.clone(),
        action: action_name,
        expected_refresh_sequence: action_request.expected_refresh_sequence,
        expected_mutation_fingerprint: expected_mutation_fingerprint.clone(),
        actual_mutation_fingerprint: None,
        state: WebActionJobState::Queued,
        started_unix_ms: unix_ms(),
        updated_unix_ms: unix_ms(),
        phase: "queued".to_owned(),
        checkpoint: None,
        error: None,
        refresh_sequence: None,
    };
    if let Err(active) = enqueue_action_job(repository, job.clone()) {
        return json_response(
            StatusCode(409),
            &json!({
                "ok": false,
                "error": {
                    "code": "web_action_in_progress",
                    "message": "one typed action is already active for this repository"
                },
                "job": active,
            }),
        );
    }
    let repository = Arc::clone(repository);
    let repository_id = repository.id.clone();
    let worker_id = action_id.clone();
    thread::spawn(move || {
        execute_action_job(
            &repository,
            &worker_id,
            AcceptedWebAction {
                request: action_request,
                expected_mutation_fingerprint,
            },
        );
    });
    json_response(
        StatusCode(202),
        &json!({
            "ok": true,
            "accepted": true,
            "repository_id": repository_id,
            "action_id": action_id,
            "job": job,
        }),
    )
}

fn execute_action_job(repository: &Arc<RepositoryEntry>, id: &str, accepted: AcceptedWebAction) {
    let AcceptedWebAction {
        request,
        expected_mutation_fingerprint,
    } = accepted;
    update_action_job(repository, id, |job| {
        job.state = WebActionJobState::Running;
        "waiting_for_repository_lock".clone_into(&mut job.phase);
    });
    let _action = repository
        .action_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _refresh = repository
        .refresh_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    update_action_job(repository, id, |job| {
        "validating_exact_snapshot".clone_into(&mut job.phase);
    });
    let (actual_sequence, actual_mutation_fingerprint) =
        fresh_action_authority(repository, request.expected_refresh_sequence);
    update_action_job(repository, id, |job| {
        job.actual_mutation_fingerprint
            .clone_from(&actual_mutation_fingerprint);
    });
    let result = match validate_action_authority(
        request.expected_refresh_sequence,
        &expected_mutation_fingerprint,
        actual_sequence,
        actual_mutation_fingerprint,
    ) {
        Ok(_actual_fingerprint) => {
            update_action_job(repository, id, |job| {
                "domain_operation_in_flight".clone_into(&mut job.phase);
            });
            run_action(&repository.context, request.action)
        }
        Err(error) => Err(error),
    };
    crate::read::invalidate_status_cache(&repository.context);
    update_action_job(repository, id, |job| {
        "post_action_refresh".clone_into(&mut job.phase);
    });
    refresh_repository_locked(repository);
    let action_name = {
        let jobs = repository
            .actions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        jobs.iter()
            .find(|job| job.id == id)
            .map_or_else(|| "unknown".to_owned(), |job| job.action.clone())
    };
    let action_record = match &result {
        Ok(result) => WebActionRecord {
            completed_unix_ms: unix_ms(),
            action: action_name,
            ok: true,
            result: Some(result.clone()),
            error: None,
        },
        Err(error) => WebActionRecord {
            completed_unix_ms: unix_ms(),
            action: action_name,
            ok: false,
            result: None,
            error: Some(WebError::from_app(error)),
        },
    };
    let refresh_sequence = {
        let mut snapshot = repository
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.last_action = Some(action_record);
        snapshot.refresh_sequence
    };
    update_action_job(repository, id, |job| {
        job.refresh_sequence = Some(refresh_sequence);
        match result {
            Ok(_result) => {
                job.state = WebActionJobState::Succeeded;
                "completed".clone_into(&mut job.phase);
            }
            Err(error) => {
                job.state = WebActionJobState::Failed;
                "failed".clone_into(&mut job.phase);
                job.error = Some(WebError::from_app(&error));
            }
        }
    });
    if repository
        .webhook_sync_pending
        .swap(false, Ordering::SeqCst)
    {
        let _ = enqueue_webhook_sync(repository);
    }
}

fn run_action(context: &AppContext, action: WebAction) -> Result<serde_json::Value, AppError> {
    match action {
        WebAction::Check(input) => serialize_action(crate::read::check(context, &input)),
        WebAction::PlanSync(input) => serialize_action(crate::sync::plan_sync(context, &input)),
        WebAction::PlanConcat(input) => serialize_action(crate::concat::plan(context, &input)),
        WebAction::Concat(input) => serialize_action(crate::concat::execute(context, &input)),
        WebAction::Sync(input) => serialize_action(crate::sync::sync(context, &input)),
        WebAction::Join(input) => serialize_action(crate::membership::join(context, &input)),
        WebAction::Rejoin(input) => serialize_action(crate::membership::rejoin(context, &input)),
        WebAction::New(input) => serialize_action(crate::membership::new(context, &input)),
        WebAction::Renew(input) => serialize_action(crate::membership::renew(context, &input)),
        WebAction::ForceArm(input) => serialize_action(crate::force::arm(context, &input)),
        WebAction::ForceRevoke(input) => serialize_action(crate::force::revoke(context, &input)),
        WebAction::PrioritySet(input) => serialize_action(crate::priority::set(context, &input)),
        WebAction::PriorityClear(input) => {
            serialize_action(crate::priority::clear(context, &input))
        }
        WebAction::Split(input) => serialize_action(crate::reshape::split(context, &input)),
        WebAction::Evict(input) => serialize_action(crate::reshape::evict(context, &input)),
        WebAction::Pause(input) => serialize_action(crate::pause::pause(context, &input)),
        WebAction::Resume(input) => serialize_action(crate::pause::resume(context, &input)),
        WebAction::PauseRecoveryPrepare(input) => serialize_action(crate::pause::pause_recovery(
            context,
            crate::pause::PauseRecoveryPhase::Prepare,
            &input,
        )),
        WebAction::PauseRecoveryCheckpointBase(input) => {
            serialize_action(crate::pause::pause_recovery(
                context,
                crate::pause::PauseRecoveryPhase::CheckpointBase,
                &input,
            ))
        }
        WebAction::PauseRecoveryCheckpointHead(input) => {
            serialize_action(crate::pause::pause_recovery(
                context,
                crate::pause::PauseRecoveryPhase::CheckpointHead,
                &input,
            ))
        }
        WebAction::PauseRecoveryFinalize(input) => serialize_action(crate::pause::pause_recovery(
            context,
            crate::pause::PauseRecoveryPhase::Finalize,
            &input,
        )),
        WebAction::PauseRecoveryRollback(input) => serialize_action(crate::pause::pause_recovery(
            context,
            crate::pause::PauseRecoveryPhase::Rollback,
            &input,
        )),
        WebAction::RepairStart(input) => serialize_action(crate::repair::start(context, &input)),
        WebAction::RepairContinue(input) => {
            serialize_action(crate::repair::continue_session(context, &input))
        }
        WebAction::RepairStatus(input) => serialize_action(crate::repair::status(context, &input)),
        WebAction::RepairAbort(input) => serialize_action(crate::repair::abort(context, &input)),
        WebAction::RepairGrant(input) => {
            serialize_action(crate::repair::grant_paths(context, &input))
        }
        WebAction::RepairRevokeGrant(input) => {
            serialize_action(crate::repair::revoke_grants(context, &input))
        }
    }
}

fn serialize_action<T: Serialize>(
    result: Result<T, AppError>,
) -> Result<serde_json::Value, AppError> {
    let output = result?;
    serde_json::to_value(output).map_err(|error| {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "web_action_encode_failed",
            error.to_string(),
            None,
        )
    })
}

fn repository_id_from_path<'a>(path: &'a str, operation: &str) -> Option<&'a str> {
    let prefix = "/api/v1/repos/";
    path.strip_prefix(prefix)?
        .strip_suffix(&format!("/{operation}"))
        .filter(|id| !id.is_empty() && !id.contains('/'))
}

fn csrf_valid(request: &Request, expected: &str) -> bool {
    request
        .headers()
        .iter()
        .any(|header| header.field.equiv("X-Cara-CSRF") && header.value.as_str() == expected)
}

fn static_response(
    body: &'static str,
    content_type: &'static str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    response_from_bytes(StatusCode(200), body.as_bytes().to_vec(), content_type)
}

fn json_response(status: StatusCode, value: &impl Serialize) -> Response<std::io::Cursor<Vec<u8>>> {
    match serde_json::to_vec(value) {
        Ok(body) => response_from_bytes(status, body, "application/json; charset=utf-8"),
        Err(error) => error_response(
            StatusCode(500),
            "web_json_encode_failed",
            &error.to_string(),
        ),
    }
}

fn error_response(
    status: StatusCode,
    code: &str,
    message: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_vec(&json!({
        "ok": false,
        "error": {"code": code, "message": message}
    }))
    .unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
    response_from_bytes(status, body, "application/json; charset=utf-8")
}

fn response_from_bytes(
    status: StatusCode,
    body: Vec<u8>,
    content_type: &'static str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut response = Response::from_data(body).with_status_code(status);
    for (name, value) in [
        ("Content-Type", content_type),
        ("X-Content-Type-Options", "nosniff"),
        ("X-Frame-Options", "DENY"),
        ("Referrer-Policy", "no-referrer"),
        ("Cache-Control", "no-store"),
        (
            "Content-Security-Policy",
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
        ),
    ] {
        if let Ok(header) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            response.add_header(header);
        }
    }
    response
}

fn open_browser(listen: SocketAddr) {
    let url = format!("http://{listen}");
    #[cfg(target_os = "macos")]
    let command = ("open", vec![url.as_str()]);
    #[cfg(target_os = "linux")]
    let command = ("xdg-open", vec![url.as_str()]);
    #[cfg(target_os = "windows")]
    let command = ("cmd", vec!["/C", "start", url.as_str()]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let command = ("", Vec::<&str>::new());
    if !command.0.is_empty() {
        let _ = std::process::Command::new(command.0)
            .args(command.1)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_loopback_and_unbounded_polling() {
        let mut input = WebInput {
            repositories: vec![PathBuf::from(".")],
            listen: "0.0.0.0:4774".parse().unwrap(),
            poll_seconds: 15,
            read_only: true,
            open: false,
            github_webhook_secret_env: None,
            github_installation_id: None,
            webhook_sync: false,
            hosted: false,
        };
        assert_eq!(
            validate_input(&input).unwrap_err().code(),
            "web_non_loopback_refused"
        );
        input.listen = "127.0.0.1:4774".parse().unwrap();
        input.poll_seconds = 1;
        assert_eq!(
            validate_input(&input).unwrap_err().code(),
            "web_poll_interval_invalid"
        );
    }

    #[test]
    fn webhook_configuration_requires_installation_and_mutation_mode() {
        let mut input = WebInput {
            repositories: vec![PathBuf::from(".")],
            listen: "127.0.0.1:4774".parse().unwrap(),
            poll_seconds: 15,
            read_only: false,
            open: false,
            github_webhook_secret_env: Some("CARA_TEST_SECRET".to_owned()),
            github_installation_id: None,
            webhook_sync: false,
            hosted: false,
        };
        assert_eq!(
            validate_input(&input).unwrap_err().code(),
            "webhook_installation_required"
        );
        input.github_installation_id = Some(42);
        input.read_only = true;
        input.webhook_sync = true;
        assert_eq!(
            validate_input(&input).unwrap_err().code(),
            "webhook_sync_read_only_conflict"
        );
    }

    #[test]
    fn webhook_hmac_and_event_selection_are_exact() {
        let secret = b"reviewed-webhook-secret";
        let body = br#"{"repository":{"full_name":"owner/repo"}}"#;
        let mut mac = WebhookHmac::new_from_slice(secret).unwrap();
        mac.update(body);
        let signature_hex =
            mac.finalize()
                .into_bytes()
                .iter()
                .fold(String::new(), |mut output, byte| {
                    use std::fmt::Write as _;
                    write!(output, "{byte:02x}").unwrap();
                    output
                });
        let signature = format!("sha256={signature_hex}");
        assert!(valid_webhook_signature(secret, body, &signature));
        assert!(!valid_webhook_signature(secret, b"changed", &signature));
        assert!(webhook_event_is_wake(
            "push",
            &json!({"ref": "refs/heads/main"}),
            "main"
        ));
        assert!(!webhook_event_is_wake(
            "push",
            &json!({"ref": "refs/heads/topic"}),
            "main"
        ));
        assert!(webhook_event_is_wake(
            "pull_request",
            &json!({"action": "synchronize"}),
            "main"
        ));
        assert!(!webhook_event_is_wake("ping", &json!({}), "main"));
    }

    #[test]
    fn webhook_delivery_ids_are_durable_private_and_deduplicated() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("deliveries.log");
        let deliveries = Mutex::new(VecDeque::new());
        assert!(!persist_webhook_delivery(&path, &deliveries, "delivery-1").unwrap());
        assert!(persist_webhook_delivery(&path, &deliveries, "delivery-1").unwrap());
        assert_eq!(load_webhook_deliveries(&path), ["delivery-1".to_owned()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn repository_route_ids_are_strict() {
        assert_eq!(
            repository_id_from_path("/api/v1/repos/repo-1/refresh", "refresh"),
            Some("repo-1")
        );
        assert_eq!(
            repository_id_from_path("/api/v1/repos/a/b/refresh", "refresh"),
            None
        );
    }

    #[test]
    fn repository_paths_are_canonical_and_deduplicated() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        let repositories = load_repositories(&[directory.path().to_path_buf()]).unwrap();
        assert_eq!(repositories.len(), 1);
        assert_eq!(repositories[0].id, "repo-1");
        assert_eq!(
            repositories[0].context.repository_path,
            directory.path().canonicalize().unwrap()
        );

        let error =
            load_repositories(&[directory.path().to_path_buf(), directory.path().join(".")])
                .err()
                .expect("duplicate path fails");
        assert_eq!(error.code(), "web_repository_duplicate");
    }

    fn hosted_entry(config: CaravanConfig) -> Arc<RepositoryEntry> {
        // Derive the path from the declared slug so distinct repositories are
        // distinct worktrees, matching real deployments.
        let path = PathBuf::from(format!(
            "/tmp/hosted-{}",
            config
                .repository
                .as_deref()
                .unwrap_or("unslugged")
                .replace('/', "-")
        ));
        Arc::new(RepositoryEntry {
            id: "repo-1".to_owned(),
            context: AppContext {
                repository_path: path.clone(),
                config_path: path.join(DEFAULT_CONFIG_PATH),
                config_existed: true,
                config,
            },
            snapshot: Mutex::new(WebRepositorySnapshot {
                id: "repo-1".to_owned(),
                path: path.display().to_string(),
                config_path: path.join(DEFAULT_CONFIG_PATH).display().to_string(),
                config_existed: true,
                effective_config: serde_json::Value::Null,
                refreshed_unix_ms: 0,
                refresh_sequence: 0,
                refreshing: false,
                status: None,
                error: None,
                candidate_compatibility: Vec::new(),
                candidate_compatibility_truncated: 0,
                last_action: None,
                journal: None,
                actions: Vec::new(),
            }),
            refresh_lock: Mutex::new(()),
            action_lock: Mutex::new(()),
            actions: Mutex::new(VecDeque::new()),
            webhook_deliveries: Mutex::new(VecDeque::new()),
            webhook_delivery_path: path.join("deliveries.json"),
            webhook_sync_pending: AtomicBool::new(false),
        })
    }

    fn hosted_config(
        mode: crate::config::GithubAuthMode,
        installation_id: Option<u64>,
        writer: crate::config::WriterMode,
        repository: Option<&str>,
    ) -> CaravanConfig {
        let mut config = CaravanConfig::default();
        config.github_auth.mode = mode;
        config.github_auth.installation_id = installation_id;
        if mode == crate::config::GithubAuthMode::AppInstallation {
            config.github_auth.app_slug = Some("caravan-hosted".to_owned());
        }
        config.writer.mode = writer;
        config.repository = repository.map(str::to_owned);
        config
    }

    fn hosted_input(hosted: bool, installation_id: Option<u64>) -> WebInput {
        WebInput {
            repositories: vec![PathBuf::from(".")],
            listen: "127.0.0.1:4774".parse().unwrap(),
            poll_seconds: 15,
            read_only: false,
            open: false,
            github_webhook_secret_env: Some("CARA_TEST_SECRET".to_owned()),
            github_installation_id: installation_id,
            webhook_sync: true,
            hosted,
        }
    }

    #[test]
    fn hosted_mode_requires_app_identity_fenced_writer_and_one_installation() {
        let compliant = || {
            hosted_config(
                crate::config::GithubAuthMode::AppInstallation,
                Some(42),
                crate::config::WriterMode::RemoteFenced,
                Some("owner/repo"),
            )
        };
        let second = hosted_config(
            crate::config::GithubAuthMode::AppInstallation,
            Some(42),
            crate::config::WriterMode::RemoteFenced,
            Some("owner/second"),
        );
        validate_hosted_repositories(
            &hosted_input(true, Some(42)),
            &[hosted_entry(compliant()), hosted_entry(second)],
        )
        .expect("two distinct repositories on one installation are accepted");

        for (config, expected) in [
            (
                hosted_config(
                    crate::config::GithubAuthMode::Ambient,
                    None,
                    crate::config::WriterMode::RemoteFenced,
                    Some("owner/ambient"),
                ),
                "web_hosted_repository_auth_not_app",
            ),
            (
                hosted_config(
                    crate::config::GithubAuthMode::AppInstallation,
                    Some(43),
                    crate::config::WriterMode::RemoteFenced,
                    Some("owner/mismatch"),
                ),
                "web_hosted_installation_mismatch",
            ),
            (
                hosted_config(
                    crate::config::GithubAuthMode::AppInstallation,
                    Some(42),
                    crate::config::WriterMode::LocalOnly,
                    Some("owner/localonly"),
                ),
                "web_hosted_writer_not_fenced",
            ),
            (
                hosted_config(
                    crate::config::GithubAuthMode::AppInstallation,
                    Some(42),
                    crate::config::WriterMode::RemoteFenced,
                    None,
                ),
                "web_hosted_repository_slug_required",
            ),
        ] {
            let error = validate_hosted_repositories(
                &hosted_input(true, Some(42)),
                &[hosted_entry(compliant()), hosted_entry(config)],
            )
            .expect_err("non-compliant hosted repository is refused");
            assert_eq!(error.code(), expected);
        }

        let mut without_sync = hosted_input(true, Some(42));
        without_sync.webhook_sync = false;
        assert_eq!(
            validate_hosted_repositories(&without_sync, &[hosted_entry(compliant())])
                .unwrap_err()
                .code(),
            "web_hosted_requires_webhook_sync"
        );

        let mut read_only = hosted_input(true, Some(42));
        read_only.read_only = true;
        assert_eq!(
            validate_hosted_repositories(&read_only, &[hosted_entry(compliant())])
                .unwrap_err()
                .code(),
            "web_hosted_read_only_conflict"
        );

        assert_eq!(
            validate_hosted_repositories(&hosted_input(true, None), &[hosted_entry(compliant())])
                .unwrap_err()
                .code(),
            "web_hosted_installation_required"
        );

        let policy = redacted_config(&compliant()).unwrap();
        assert_eq!(policy["github_auth"]["mode"], "app_installation");
        assert_eq!(policy["github_auth"]["installation_id"], 42);
        assert_eq!(policy["writer"]["mode"], "remote_fenced");
        assert_eq!(policy["repository"], "owner/repo");
    }

    #[test]
    fn local_dashboard_mode_accepts_ambient_local_only_repositories() {
        let ambient = hosted_config(
            crate::config::GithubAuthMode::Ambient,
            None,
            crate::config::WriterMode::LocalOnly,
            None,
        );
        let mut local = hosted_input(false, None);
        local.webhook_sync = false;
        local.github_webhook_secret_env = None;
        validate_hosted_repositories(&local, &[hosted_entry(ambient)])
            .expect("default local dashboard keeps ambient/local_only behavior");
    }

    fn test_dashboard(hosted: bool, read_only: bool) -> Dashboard {
        Dashboard {
            listen: "127.0.0.1:4774".parse().unwrap(),
            poll_seconds: 15,
            read_only,
            hosted,
            csrf_token: "token".to_owned(),
            started_unix_ms: 0,
            repositories: Vec::new(),
            webhook_secret: None,
            webhook_installation_id: None,
            webhook_sync: hosted,
            webhook_status: Mutex::new(WebhookStatus::default()),
            stopping: AtomicBool::new(false),
            active_requests: AtomicUsize::new(0),
        }
    }

    #[test]
    fn hosted_workers_refuse_interactive_mutations_but_keep_webhook_work() {
        let hosted = test_dashboard(true, false);
        for action in [
            WebAction::Sync(SyncInput::default()),
            WebAction::Join(JoinInput::default()),
            WebAction::ForceArm(crate::force::ForceIntentInput {
                pr: 1,
                actor: "operator".to_owned(),
                reason: "test".to_owned(),
            }),
        ] {
            assert!(action.mutates(), "expected a mutating action");
            assert_eq!(
                interactive_mutation_refusal(&hosted, action.mutates()),
                Some((
                    "web_hosted_interactive_mutation_refused",
                    "hosted workers mutate only from verified webhook deliveries",
                ))
            );
        }
        // Observability actions stay available to humans behind the proxy.
        for action in [
            WebAction::Check(CheckInput::default()),
            WebAction::PlanSync(SyncInput::default()),
        ] {
            assert!(!action.mutates());
            assert_eq!(
                interactive_mutation_refusal(&hosted, action.mutates()),
                None
            );
        }

        // The refusal is interactive-only. The webhook entry point takes no
        // Dashboard and never consults hosted/read_only, so verified deliveries
        // still drive work: here it durably defers pending sync rather than
        // being refused (a bare fixture has no status snapshot yet).
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        let repositories = load_repositories(&[directory.path().to_path_buf()]).unwrap();
        assert!(!enqueue_webhook_sync(&repositories[0]));
        assert!(repositories[0].webhook_sync_pending.load(Ordering::SeqCst));

        // Local and read-only modes keep their exact prior behavior.
        assert_eq!(
            interactive_mutation_refusal(&test_dashboard(false, false), true),
            None
        );
        assert_eq!(
            interactive_mutation_refusal(&test_dashboard(false, true), true),
            Some(("web_read_only", "mutation endpoints are disabled"))
        );
        assert_eq!(
            interactive_mutation_refusal(&test_dashboard(false, true), false),
            None
        );
    }

    #[test]
    fn deliveries_route_by_configured_identity_before_observed_status() {
        let configured = hosted_entry(hosted_config(
            crate::config::GithubAuthMode::Ambient,
            None,
            crate::config::WriterMode::LocalOnly,
            Some("owner/repo"),
        ));
        // No successful status read yet: the pre-fix observed-status match
        // rejected this repository's deliveries with 404 until a poll healed it.
        assert!(
            configured
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .status
                .is_none()
        );
        assert!(repository_matches_slug(&configured, "owner/repo"));
        assert!(!repository_matches_slug(&configured, "owner/other"));
        assert!(!repository_matches_slug(&configured, ""));

        // Configured identity is authoritative over a stale observation.
        {
            let mut snapshot = configured
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            snapshot.status = Some(authority_status("aaaaaaaa"));
        }
        assert!(repository_matches_slug(&configured, "owner/repo"));
        assert!(!repository_matches_slug(&configured, "owner/renamed"));

        // With no configured slug, observed status still routes as before.
        let observed = hosted_entry(hosted_config(
            crate::config::GithubAuthMode::Ambient,
            None,
            crate::config::WriterMode::LocalOnly,
            None,
        ));
        assert!(!repository_matches_slug(&observed, "owner/repo"));
        {
            let mut snapshot = observed
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            snapshot.status = Some(authority_status("bbbbbbbb"));
        }
        assert!(repository_matches_slug(&observed, "owner/repo"));
    }

    #[test]
    fn hosted_startup_refuses_two_worktrees_declaring_one_slug() {
        let entry = || {
            hosted_entry(hosted_config(
                crate::config::GithubAuthMode::AppInstallation,
                Some(42),
                crate::config::WriterMode::RemoteFenced,
                Some("owner/repo"),
            ))
        };
        assert_eq!(
            validate_hosted_repositories(&hosted_input(true, Some(42)), &[entry(), entry()])
                .unwrap_err()
                .code(),
            "web_hosted_repository_slug_duplicate"
        );
    }

    #[test]
    fn health_reports_degradation_without_redefining_ok() {
        let healthy = hosted_entry(hosted_config(
            crate::config::GithubAuthMode::Ambient,
            None,
            crate::config::WriterMode::LocalOnly,
            Some("owner/healthy"),
        ));
        healthy
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status = Some(authority_status("cccccccc"));
        healthy
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .refreshed_unix_ms = 5_000;

        let mut dashboard = test_dashboard(true, false);
        dashboard.repositories = vec![Arc::clone(&healthy)];
        let payload = health_payload(&dashboard);
        assert_eq!(payload["ok"], true);
        assert_eq!(payload["degraded"], false);
        assert_eq!(payload["hosted"], true);
        assert_eq!(payload["repositories"], 1);
        assert_eq!(payload["repositories_never_refreshed"], 0);
        assert_eq!(payload["repositories_erroring"], 0);
        assert_eq!(payload["oldest_refresh_unix_ms"], 5_000);

        // A repository that has never refreshed does no useful work while the
        // process still answers, so it must surface as degraded.
        let never = hosted_entry(hosted_config(
            crate::config::GithubAuthMode::Ambient,
            None,
            crate::config::WriterMode::LocalOnly,
            Some("owner/never"),
        ));
        dashboard.repositories = vec![Arc::clone(&healthy), Arc::clone(&never)];
        let payload = health_payload(&dashboard);
        assert_eq!(payload["ok"], true, "ok keeps meaning 'serving'");
        assert_eq!(payload["degraded"], true);
        assert_eq!(payload["repositories_never_refreshed"], 1);

        // A refreshed repository carrying an error is also degraded.
        let erroring = hosted_entry(hosted_config(
            crate::config::GithubAuthMode::Ambient,
            None,
            crate::config::WriterMode::LocalOnly,
            Some("owner/erroring"),
        ));
        {
            let mut snapshot = erroring
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            snapshot.status = Some(authority_status("dddddddd"));
            snapshot.refreshed_unix_ms = 9_000;
            snapshot.error = Some(WebError {
                category: ErrorCategory::ExecutionFailure,
                code: "provider_unavailable".to_owned(),
                message: "provider read failed".to_owned(),
                details: None,
            });
        }
        dashboard.repositories = vec![Arc::clone(&healthy), erroring];
        let payload = health_payload(&dashboard);
        assert_eq!(payload["degraded"], true);
        assert_eq!(payload["repositories_erroring"], 1);
        assert_eq!(payload["repositories_never_refreshed"], 0);

        // Webhook liveness is reported so silence is detectable.
        {
            let mut status = dashboard
                .webhook_status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            status.enabled = true;
            status.sync_enabled = true;
            status.accepted = 7;
            status.rejected = 2;
            status.deduplicated = 3;
            status.last_received_unix_ms = Some(12_345);
        }
        let payload = health_payload(&dashboard);
        assert_eq!(payload["webhook"]["enabled"], true);
        assert_eq!(payload["webhook"]["accepted"], 7);
        assert_eq!(payload["webhook"]["rejected"], 2);
        assert_eq!(payload["webhook"]["deduplicated"], 3);
        assert_eq!(payload["webhook"]["last_received_unix_ms"], 12_345);
        let encoded = serde_json::to_string(&payload).unwrap();
        assert!(!encoded.contains("secret"));
        assert!(!encoded.contains("token"));
    }

    fn authority_status(head_oid: &str) -> StatusOutput {
        let repository = crate::model::RepositoryId {
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        };
        let branch = |name: &str, oid: &str| crate::model::BranchSnapshot {
            repository: repository.clone(),
            name: name.to_owned(),
            oid: crate::model::CommitOid(oid.to_owned()),
        };
        let pull_request = crate::model::PullRequestSnapshot {
            merge_state_status: None,
            number: crate::model::PrNumber(1),
            title: "candidate".to_owned(),
            url: "https://example.invalid/1".to_owned(),
            state: crate::model::PullRequestState::Open,
            draft: false,
            head: branch("feature", head_oid),
            base: branch("main", "main-oid"),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: crate::model::AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: Some("2026-01-01T00:00:00Z".to_owned()),
            merged_at: None,
            updated_at: None,
        };
        let analysis = crate::graph::GraphAnalysis {
            fleet: crate::model::CaravanFleet {
                repository: repository.clone(),
                default_branch: branch("main", "main-oid"),
                caravans: Vec::new(),
                unqueued: vec![pull_request.number],
                problems: Vec::new(),
                history: crate::model::CaravanHistory::default(),
            },
            pull_requests: std::collections::BTreeMap::from([(pull_request.number, pull_request)]),
            compatibility: Vec::new(),
            cumulative_trees: Vec::new(),
            squash_reconciliations: Vec::new(),
        };
        let admission = crate::read::resolve_admission(&analysis, &[]);
        StatusOutput {
            config_provenance: None,
            head_merge: crate::read::HeadMergeStatus::default(),
            runtime: crate::read::RuntimeProvenance::default(),
            provider_api: crate::model::GitHubApiTelemetry::default(),
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            timing: None,
            repository,
            rebase_on_join: crate::read::RebaseOnJoinStatus::default(),
            stack_backend: crate::read::StackBackendStatus::default(),
            auto_admission: crate::read::AutoAdmissionStatus::default(),
            default_branch: "main".to_owned(),
            current_branch: Some("feature".to_owned()),
            current_pr: Some(crate::model::PrNumber(1)),
            healthy: true,
            initialization: crate::initialization::InitializationStatus::default(),
            analysis,
            pauses: Vec::new(),
            admission,
            sync_budget: crate::sync::SyncBudgetStatus::default(),
        }
    }

    #[test]
    fn saloon_projection_distinguishes_main_conflict_and_clean_tail() {
        let mut status = authority_status("candidate-head");
        let repository = status.repository.clone();
        let tail = crate::model::PullRequestSnapshot {
            merge_state_status: None,
            number: crate::model::PrNumber(2),
            title: "tail".to_owned(),
            url: "https://example.invalid/2".to_owned(),
            state: crate::model::PullRequestState::Open,
            draft: false,
            head: crate::model::BranchSnapshot {
                repository: repository.clone(),
                name: "tail-2".to_owned(),
                oid: crate::model::CommitOid("tail-head".to_owned()),
            },
            base: status.analysis.fleet.default_branch.clone(),
            cross_repository: false,
            labels: BTreeSet::from(["caravan".to_owned()]),
            auto_merge: crate::model::AutoMergeState::squash(),
            checks: Vec::new(),
            created_at: Some("2026-01-01T00:00:02Z".to_owned()),
            merged_at: None,
            updated_at: None,
        };
        status
            .analysis
            .pull_requests
            .insert(tail.number, tail.clone());
        status.analysis.fleet.caravans =
            vec![crate::model::Caravan::new(vec![tail.number]).expect("one-member caravan")];
        status.admission = crate::read::resolve_admission(&status.analysis, &[]);
        let checker = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
            Ok(crate::model::CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: if target.name == "main" {
                    CompatibilityOutcome::Conflict
                } else {
                    CompatibilityOutcome::Clean
                },
                conflicting_paths: if target.name == "main" {
                    vec!["scripts/test-release-helpers.sh".to_owned()]
                } else {
                    Vec::new()
                },
                diagnostic: Some("fixture".to_owned()),
            })
        };

        let (rows, truncated) = project_candidate_compatibility(&status, &checker, 8, 8, 64, &[]);
        assert_eq!(truncated, 0);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].complete);
        assert_eq!(rows[0].targets.len(), 2);
        assert_eq!(
            rows[0].targets[0].kind,
            WebCompatibilityTargetKind::DefaultBranch
        );
        assert_eq!(
            rows[0].targets[0].outcome,
            Some(CompatibilityOutcome::Conflict)
        );
        assert_eq!(
            rows[0].targets[0].conflicting_paths,
            ["scripts/test-release-helpers.sh"]
        );
        assert_eq!(rows[0].targets[1].tail_pr, Some(crate::model::PrNumber(2)));
        assert_eq!(
            rows[0].targets[1].outcome,
            Some(CompatibilityOutcome::Clean)
        );

        let must_not_recheck = |_candidate: &BranchSnapshot, _target: &BranchSnapshot| {
            panic!("an unchanged complete generation must reuse exact compatibility evidence")
        };
        let (cached, cached_truncated) =
            project_candidate_compatibility(&status, &must_not_recheck, 8, 8, 64, &rows);
        assert_eq!(cached_truncated, 0);
        assert_eq!(cached, rows);
    }

    #[test]
    fn saloon_projection_never_marks_unevaluated_pairs_complete() {
        let status = authority_status("candidate-head");
        let checker = |_candidate: &BranchSnapshot, _target: &BranchSnapshot| {
            panic!("zero pair budget must not run compatibility")
        };
        let (rows, truncated) = project_candidate_compatibility(&status, &checker, 8, 8, 0, &[]);
        assert_eq!(truncated, 0);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].complete);
        assert_eq!(rows[0].targets.len(), 1);
        assert!(rows[0].targets[0].outcome.is_none());
        assert_eq!(
            rows[0].targets[0].error.as_ref().unwrap().code,
            "web_compatibility_budget_exhausted"
        );
    }

    fn set_authority_snapshot(repository: &RepositoryEntry, sequence: u64, status: StatusOutput) {
        let mut snapshot = repository
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.refresh_sequence = sequence;
        snapshot.status = Some(status);
        snapshot.error = None;
    }

    fn test_job(id: usize, state: WebActionJobState) -> WebActionJob {
        WebActionJob {
            id: format!("job-{id}"),
            action: "sync".to_owned(),
            expected_refresh_sequence: 1,
            expected_mutation_fingerprint: "sha256:test".to_owned(),
            actual_mutation_fingerprint: None,
            state,
            started_unix_ms: 1,
            updated_unix_ms: 1,
            phase: "test".to_owned(),
            checkpoint: None,
            error: None,
            refresh_sequence: None,
        }
    }

    #[test]
    fn action_jobs_are_serial_and_history_is_bounded() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        let repositories = load_repositories(&[directory.path().to_path_buf()]).unwrap();
        let repository = &repositories[0];
        for id in 0..(MAX_ACTION_HISTORY + 5) {
            enqueue_action_job(repository, test_job(id, WebActionJobState::Succeeded)).unwrap();
        }
        let jobs = action_jobs_with_checkpoint(repository);
        assert_eq!(jobs.len(), MAX_ACTION_HISTORY);
        assert_eq!(jobs.first().unwrap().id, "job-5");

        enqueue_action_job(repository, test_job(100, WebActionJobState::Running)).unwrap();
        let conflict = enqueue_action_job(repository, test_job(101, WebActionJobState::Queued))
            .expect_err("one repository action at a time");
        assert_eq!(conflict.id, "job-100");
        assert!(!conflict.state.terminal());
    }

    #[test]
    fn harmless_refresh_sequence_drift_keeps_accepted_action_authority() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        let repositories = load_repositories(&[directory.path().to_path_buf()]).unwrap();
        let repository = &repositories[0];
        set_authority_snapshot(repository, 7, authority_status("head-a"));
        let (_, expected) = action_authority(repository);
        let expected = expected.unwrap();

        {
            let mut snapshot = repository
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            snapshot.refresh_sequence = 8;
            snapshot.refreshed_unix_ms = 999;
            snapshot.status.as_mut().unwrap().provider_api.calls = 42;
        }
        let (actual_sequence, actual) = action_authority(repository);
        let actual = actual.unwrap();

        assert_eq!(expected, actual);
        assert_eq!(
            validate_action_authority(7, &expected, actual_sequence, Some(actual)).unwrap(),
            expected
        );
    }

    #[test]
    fn provider_fact_drift_still_fails_before_dashboard_mutation() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        let repositories = load_repositories(&[directory.path().to_path_buf()]).unwrap();
        let repository = &repositories[0];
        set_authority_snapshot(repository, 7, authority_status("head-a"));
        let (_, expected) = action_authority(repository);
        let expected = expected.unwrap();

        set_authority_snapshot(repository, 8, authority_status("head-b"));
        let (actual_sequence, actual) = action_authority(repository);
        let actual = actual.unwrap();
        let error = validate_action_authority(7, &expected, actual_sequence, Some(actual.clone()))
            .expect_err("changed provider head invalidates accepted authority");

        assert_eq!(error.code(), "web_snapshot_stale");
        let details = error.details().unwrap();
        assert_eq!(details["expected_mutation_fingerprint"], expected);
        assert_eq!(details["actual_mutation_fingerprint"], actual);
        assert_eq!(details["mutated"], false);
    }

    #[test]
    fn polling_refresh_coalesces_behind_an_accepted_action() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        let repositories = load_repositories(&[directory.path().to_path_buf()]).unwrap();
        let repository = &repositories[0];
        set_authority_snapshot(repository, 7, authority_status("head-a"));
        enqueue_action_job(repository, test_job(1, WebActionJobState::Queued)).unwrap();

        assert!(!refresh_repository(repository));
        assert_eq!(
            repository
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .refresh_sequence,
            7
        );
    }

    #[test]
    fn action_authority_evidence_survives_state_reconnect() {
        let mut job = test_job(1, WebActionJobState::Failed);
        job.expected_mutation_fingerprint = "sha256:expected".to_owned();
        job.actual_mutation_fingerprint = Some("sha256:actual".to_owned());
        let encoded = serde_json::to_vec(&job).unwrap();
        let decoded: WebActionJob = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(decoded.expected_mutation_fingerprint, "sha256:expected");
        assert_eq!(
            decoded.actual_mutation_fingerprint.as_deref(),
            Some("sha256:actual")
        );
        assert_eq!(decoded.state, WebActionJobState::Failed);
    }

    #[test]
    fn webhook_bursts_coalesce_behind_an_active_action() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        let repositories = load_repositories(&[directory.path().to_path_buf()]).unwrap();
        let repository = &repositories[0];
        enqueue_action_job(repository, test_job(1, WebActionJobState::Running)).unwrap();
        assert!(!enqueue_webhook_sync(repository));
        assert!(repository.webhook_sync_pending.load(Ordering::SeqCst));
        assert_eq!(action_jobs_with_checkpoint(repository).len(), 1);
    }

    #[test]
    fn web_journal_projection_is_byte_bounded() {
        let event = crate::hooks::event(
            crate::model::EventKind::SyncFailed,
            crate::model::OperationId::new(),
            crate::model::RepositoryId {
                owner: "owner".to_owned(),
                name: "repo".to_owned(),
            },
            None,
            Vec::new(),
            None,
            Some("x".repeat(MAX_WEB_JOURNAL_BYTES + 1)),
            std::collections::BTreeMap::new(),
        );
        let bounded = bound_web_journal(crate::journal::LogOutput {
            records: vec![crate::journal::JournalRecord::Event { version: 1, event }],
            limit: 1,
            matching_records: 1,
            truncated: false,
            source: crate::journal::JournalSource {
                path: "/tmp/test/events-v1.jsonl".to_owned(),
                present: true,
                archives: 0,
                unreadable_records: 0,
            },
        });
        assert!(bounded.records.is_empty());
        assert!(bounded.truncated);
        assert!(serde_json::to_vec(&bounded).unwrap().len() < MAX_WEB_JOURNAL_BYTES);
    }

    #[test]
    fn active_action_surfaces_durable_operation_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .current_dir(directory.path())
            .args(["init", "--quiet"])
            .status()
            .unwrap();
        assert!(status.success());
        let repositories = load_repositories(&[directory.path().to_path_buf()]).unwrap();
        let repository = &repositories[0];
        enqueue_action_job(repository, test_job(1, WebActionJobState::Running)).unwrap();
        let mut lock = crate::operation_lock::OperationLock::acquire(directory.path(), "sync")
            .expect("test lock");
        lock.checkpoint(
            "provider_convergence_in_flight",
            json!({"pr": 42, "completed_steps": 3}),
            true,
        )
        .unwrap();

        let jobs = action_jobs_with_checkpoint(repository);
        assert_eq!(jobs[0].phase, "provider_convergence_in_flight");
        let checkpoint = jobs[0].checkpoint.as_ref().expect("checkpoint");
        assert_eq!(checkpoint.evidence["pr"], 42);
        assert!(checkpoint.provider_state_indeterminate);
        lock.release().unwrap();
    }

    #[test]
    fn web_actions_are_strictly_tagged_and_classified() {
        let check: WebActionRequest = serde_json::from_value(json!({
            "expected_refresh_sequence": 7,
            "action": "check",
            "input": {"pr": 42, "tail_pr": 41}
        }))
        .unwrap();
        assert_eq!(check.expected_refresh_sequence, 7);
        assert_eq!(check.action.name(), "check");
        assert!(!check.action.mutates());

        let sync: WebActionRequest = serde_json::from_value(json!({
            "expected_refresh_sequence": 8,
            "action": "sync",
            "input": {"all": true, "rerun_failed": false}
        }))
        .unwrap();
        assert_eq!(sync.action.name(), "sync");
        assert!(sync.action.mutates());
        let plan: WebActionRequest = serde_json::from_value(json!({
            "expected_refresh_sequence": 8,
            "action": "plan_sync",
            "input": {"all": true, "rerun_failed": false}
        }))
        .unwrap();
        assert_eq!(plan.action.name(), "plan_sync");
        assert!(!plan.action.mutates());
        let new: WebActionRequest = serde_json::from_value(json!({
            "expected_refresh_sequence": 9,
            "action": "new",
            "input": {"pr": 42, "create_pr": false, "reason": "Saloon admission"}
        }))
        .unwrap();
        let WebAction::New(input) = new.action else {
            panic!("expected new action");
        };
        assert_eq!(input.pr, Some(42));

        for (action, input) in [
            (
                "force_arm",
                json!({"pr": 42, "actor": "cara-web", "reason": "known failure"}),
            ),
            (
                "force_revoke",
                json!({"pr": 42, "actor": "cara-web", "reason": "withdraw intent"}),
            ),
            (
                "priority_set",
                json!({"pr": 42, "label": "caravan-priority:high", "actor": "cara-web", "reason": "urgent"}),
            ),
            (
                "priority_clear",
                json!({"pr": 42, "actor": "cara-web", "reason": "FIFO"}),
            ),
        ] {
            let request: WebActionRequest = serde_json::from_value(json!({
                "expected_refresh_sequence": 10,
                "action": action,
                "input": input,
            }))
            .unwrap();
            assert_eq!(request.action.name(), action);
            assert!(request.action.mutates());
        }
        assert!(
            serde_json::from_value::<WebActionRequest>(json!({
                "expected_refresh_sequence": 1,
                "action": "shell",
                "input": {"command": "rm -rf"}
            }))
            .is_err()
        );
    }

    #[test]
    fn effective_web_config_redacts_hook_commands() {
        let mut config = CaravanConfig::default();
        config.hooks.insert(
            crate::model::EventKind::HeadAdvanced,
            crate::config::HookConfig {
                command: "notify --token secret-value".to_owned(),
                timeout_secs: 12,
                blocking: true,
            },
        );
        let value = redacted_config(&config).unwrap();
        let encoded = serde_json::to_string(&value).unwrap();
        assert!(!encoded.contains("secret-value"));
        assert_eq!(
            value["hooks"]["head_advanced"]["command"],
            "<configured; redacted>"
        );
        assert_eq!(value["hooks"]["head_advanced"]["blocking"], true);
    }

    #[test]
    fn embedded_assets_are_self_contained() {
        assert!(INDEX_HTML.contains("/assets/app.css"));
        assert!(INDEX_HTML.contains("/assets/app.js"));
        assert!(!INDEX_HTML.contains("https://"));
        assert!(!APP_CSS.contains("url(http"));
        assert!(!APP_JS.contains("cdn."));
        assert!(!APP_JS.contains("import("));
        assert!(APP_JS.contains("https://github.com/"));
        assert!(INDEX_HTML.contains("id=\"plan-sync\""));
        assert!(INDEX_HTML.contains("id=\"plan-concat\""));
        assert!(INDEX_HTML.contains("id=\"execute-concat\""));
        assert!(APP_JS.contains("plan_concat"));
        assert!(APP_JS.contains("expected_plan_hash"));
        assert!(APP_JS.contains("Parked red"));
        assert!(APP_JS.contains("parkedCaravans"));
        assert!(!APP_JS.contains("evict+rejoin"));
        assert!(INDEX_HTML.contains("id=\"show-config\""));
        assert!(INDEX_HTML.contains("id=\"show-evidence\""));
        assert!(!INDEX_HTML.contains("Surveying the trail"));
        assert!(!INDEX_HTML.contains("ambient repository discovery"));
        assert!(APP_JS.contains("target=\"_blank\" rel=\"noopener noreferrer\""));
        assert!(APP_JS.contains("last_action"));
        assert!(APP_JS.contains("plan_sync"));
        assert!(APP_JS.contains("Action progress"));
        assert!(APP_JS.contains("Journal receipt"));
        assert!(INDEX_HTML.contains("<h1>Caravan</h1>"));
        assert!(!INDEX_HTML.contains("Caravan Control"));
        assert!(!INDEX_HTML.contains("Waiting at the rail"));
        assert!(INDEX_HTML.contains("id=\"repository-sidebar\""));
        assert!(INDEX_HTML.contains("id=\"evidence-sidebar\""));
        assert!(INDEX_HTML.contains("id=\"evidence-content\""));
        assert!(INDEX_HTML.contains("id=\"attention-sidebar\""));
        assert!(INDEX_HTML.contains("class=\"dashboard-scroll\""));
        assert!(INDEX_HTML.contains("id=\"saloon\""));
        for label in [
            "Ready",
            "Conflicting",
            "Saddling Up",
            "Other",
            "Bounty List",
        ] {
            assert!(
                APP_JS.contains(label),
                "missing Saloon classification {label}"
            );
        }
        assert!(APP_JS.contains(
            "const SALOON_ORDER = [\"ready\", \"conflicting\", \"saddling\", \"other\", \"bounty\"]"
        ));
        assert!(APP_JS.contains("admissionFact(status, \"candidates\", pr)"));
        assert!(APP_JS.contains("Ready (${ready.map(targetLabel).join(\", \")})"));
        assert!(APP_JS.contains("Conflicting (${conflicting.map(targetLabel).join(\", \")})"));
        assert!(APP_JS.contains("Exact target compatibility"));
        assert!(APP_JS.contains("force_arm"));
        assert!(APP_JS.contains("force_revoke"));
        assert!(APP_JS.contains("priority_set"));
        assert!(APP_JS.contains("priority_clear"));
        assert!(APP_JS.contains("data-audit-required"));
        assert!(APP_JS.contains("Actor and reason are required"));
        assert!(APP_JS.contains("caravan.saloon.${repositoryId}.${name}"));
        assert!(APP_JS.contains("ui.saloon.addEventListener(\"toggle\""));
        assert!(APP_JS.contains("group.open ? \"open\" : \"closed\""));
        assert!(APP_CSS.contains(".dashboard.no-caravans .caravan-list .empty-state"));
        assert!(APP_CSS.contains(".repo-rail { grid-column: 1;"));
        assert!(APP_CSS.contains(".content { grid-column: 2;"));
        assert!(APP_CSS.contains(".evidence-rail { grid-column: 3;"));
        assert!(APP_CSS.contains(".attention-rail { grid-column: 4;"));
        assert!(APP_CSS.contains(".dashboard-scroll {"));
        assert!(APP_CSS.contains("overflow-y: auto"));
        assert!(APP_CSS.contains(".caravan { min-height: 220px;"));
        assert!(APP_CSS.contains(".saloon-groups { display: grid; align-content: start; gap: 7px; min-height: 0; overflow: visible;"));
        assert!(APP_JS.contains("toggleSidebar(\"evidence\")"));
        assert!(!APP_JS.contains("openInspector(\"evidence\")"));
        assert!(APP_JS.contains("problem?.kind === \"dissolved_member\""));
        assert!(APP_JS.contains("data-dismiss-decision"));
        assert!(APP_JS.contains("data-restore-decisions"));
        assert!(APP_JS.contains("Only historical dissolved-member notices can be dismissed"));
    }
}
