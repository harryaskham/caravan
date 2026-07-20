//! Built-in, path-scoped Cara web dashboard.
//!
//! The server is deliberately local-first: repository paths are explicit,
//! assets are embedded in the binary, and no repository discovery or external
//! web dependencies occur. Domain reads still flow through the same typed Cara
//! status implementation used by CLI/JSON/MCP.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Args;
use mcp_cli::{ErrorCategory, StructuredError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::config::{CaravanConfig, DEFAULT_CONFIG_PATH};
use crate::read::StatusOutput;
use crate::{AppContext, AppError};

const INDEX_HTML: &str = include_str!("web_assets/index.html");
const APP_CSS: &str = include_str!("web_assets/app.css");
const APP_JS: &str = include_str!("web_assets/app.js");
const WEB_SCHEMA_VERSION: u32 = 1;
const MIN_POLL_SECONDS: u64 = 2;
const MAX_POLL_SECONDS: u64 = 3_600;
const SERVER_TICK: Duration = Duration::from_millis(250);

/// Start a local dashboard over one or more explicit repository paths.
#[derive(Debug, Clone, Args)]
pub struct WebInput {
    /// Repository/worktree path to manage. Repeat for a multi-repository view.
    #[arg(long = "repo", value_name = "PATH", required = true)]
    pub repositories: Vec<PathBuf>,

    /// Loopback HTTP address. Non-loopback binds are refused in this release.
    #[arg(long, default_value = "127.0.0.1:4774", value_name = "ADDRESS")]
    pub listen: SocketAddr,

    /// Seconds between bounded status refresh passes.
    #[arg(long, default_value_t = 15, value_name = "SECONDS")]
    pub poll_seconds: u64,

    /// Disable every mutation endpoint while retaining refresh/status views.
    #[arg(long)]
    pub read_only: bool,

    /// Open the dashboard in the platform browser after binding.
    #[arg(long)]
    pub open: bool,
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

/// One repository's latest periodic typed Cara status.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebRepositorySnapshot {
    pub id: String,
    pub path: String,
    pub config_path: String,
    pub config_existed: bool,
    pub refreshed_unix_ms: u64,
    pub refresh_sequence: u64,
    pub refreshing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusOutput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WebError>,
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
    /// Same-origin token required as `X-Cara-CSRF` on POST requests.
    pub csrf_token: String,
    pub repositories: Vec<WebRepositorySnapshot>,
}

struct RepositoryEntry {
    id: String,
    context: AppContext,
    snapshot: Mutex<WebRepositorySnapshot>,
    refresh_lock: Mutex<()>,
}

struct Dashboard {
    listen: SocketAddr,
    poll_seconds: u64,
    read_only: bool,
    csrf_token: String,
    started_unix_ms: u64,
    repositories: Vec<Arc<RepositoryEntry>>,
    stopping: AtomicBool,
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
            csrf_token: self.csrf_token.clone(),
            repositories: self
                .repositories
                .iter()
                .map(|repository| {
                    repository
                        .snapshot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
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

/// Serve until SIGINT/SIGTERM sets the foreground stop flag.
pub fn serve(input: &WebInput) -> Result<(), AppError> {
    validate_input(input)?;
    let dashboard = Arc::new(Dashboard {
        listen: input.listen,
        poll_seconds: input.poll_seconds,
        read_only: input.read_only,
        csrf_token: uuid::Uuid::now_v7().to_string(),
        started_unix_ms: unix_ms(),
        repositories: load_repositories(&input.repositories)?,
        stopping: AtomicBool::new(false),
    });
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
        } else {
            "enabled"
        }
    );

    while !dashboard.stopping.load(Ordering::Relaxed) {
        match server.recv_timeout(SERVER_TICK) {
            Ok(Some(request)) => handle_request(request, &dashboard),
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
    if input.repositories.is_empty() {
        return Err(AppError::validation(
            "web_repository_required",
            "pass at least one explicit --repo PATH",
        ));
    }
    Ok(())
}

fn is_loopback(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_loopback(),
        IpAddr::V6(address) => address.is_loopback(),
    }
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
            let context = AppContext {
                repository_path: canonical.clone(),
                config_path: config_path.clone(),
                config_existed,
                config,
            };
            let id = format!("repo-{}", index + 1);
            Ok(Arc::new(RepositoryEntry {
                id: id.clone(),
                context,
                snapshot: Mutex::new(WebRepositorySnapshot {
                    id,
                    path: canonical.display().to_string(),
                    config_path: config_path.display().to_string(),
                    config_existed,
                    refreshed_unix_ms: 0,
                    refresh_sequence: 0,
                    refreshing: false,
                    status: None,
                    error: None,
                }),
                refresh_lock: Mutex::new(()),
            }))
        })
        .collect()
}

fn refresh_repository(repository: &RepositoryEntry) {
    let _refresh = repository
        .refresh_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    {
        let mut snapshot = repository
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.refreshing = true;
    }
    let result = crate::read::status(&repository.context);
    let mut snapshot = repository
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    snapshot.refresh_sequence = snapshot.refresh_sequence.saturating_add(1);
    snapshot.refreshed_unix_ms = unix_ms();
    snapshot.refreshing = false;
    match result {
        Ok(status) => {
            snapshot.status = Some(status);
            snapshot.error = None;
        }
        Err(error) => {
            snapshot.error = Some(WebError::from_app(&error));
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

fn handle_request(request: Request, dashboard: &Dashboard) {
    let method = request.method().clone();
    let path = request.url().split('?').next().unwrap_or("/").to_owned();
    let response = match (method, path.as_str()) {
        (Method::Get, "/") => static_response(INDEX_HTML, "text/html; charset=utf-8"),
        (Method::Get, "/assets/app.css") => static_response(APP_CSS, "text/css; charset=utf-8"),
        (Method::Get, "/assets/app.js") => {
            static_response(APP_JS, "text/javascript; charset=utf-8")
        }
        (Method::Get, "/api/v1/state") => json_response(StatusCode(200), &dashboard.state()),
        (Method::Get, "/api/v1/health") => json_response(
            StatusCode(200),
            &json!({
                "ok": true,
                "schema_version": WEB_SCHEMA_VERSION,
                "repositories": dashboard.repositories.len(),
                "started_unix_ms": dashboard.started_unix_ms,
            }),
        ),
        (Method::Post, path) if path.ends_with("/refresh") => {
            handle_refresh(&request, dashboard, path)
        }
        (Method::Post, path) if path.ends_with("/action") => {
            if !csrf_valid(&request, &dashboard.csrf_token) {
                error_response(
                    StatusCode(403),
                    "web_csrf_invalid",
                    "missing or invalid X-Cara-CSRF",
                )
            } else if dashboard.read_only {
                error_response(
                    StatusCode(403),
                    "web_read_only",
                    "mutation endpoints are disabled",
                )
            } else {
                error_response(
                    StatusCode(501),
                    "web_action_not_implemented",
                    "the typed mutation API lands in the next dashboard slice",
                )
            }
        }
        _ => error_response(
            StatusCode(404),
            "web_route_not_found",
            "unknown Cara web route",
        ),
    };
    let _ = request.respond(response);
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
    refresh_repository(repository);
    let snapshot = repository
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    json_response(StatusCode(200), &snapshot)
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

    #[test]
    fn embedded_assets_are_self_contained() {
        assert!(INDEX_HTML.contains("/assets/app.css"));
        assert!(INDEX_HTML.contains("/assets/app.js"));
        assert!(!INDEX_HTML.contains("https://"));
        assert!(!APP_CSS.contains("url(http"));
        assert!(!APP_JS.contains("https://"));
    }
}
