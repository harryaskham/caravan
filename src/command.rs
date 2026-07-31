//! Small subprocess seam used by Git and GitHub adapters.
//!
//! Keeping command execution behind [`CommandRunner`] makes discovery tests
//! hermetic while production still uses the installed, authenticated `git` and
//! `gh` binaries.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Default hard deadline for one Git/GitHub subprocess.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const TERMINATION_GRACE: Duration = Duration::from_millis(250);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
// Provider JSON for repositories with hundreds of PRs routinely exceeds the
// diagnostic cap. Keep stdout large enough for bounded GitHub list queries,
// while stderr remains a small evidence stream.
const MAX_STDOUT_CAPTURE_BYTES: usize = 32 * 1024 * 1024;
const MAX_STDERR_CAPTURE_BYTES: usize = 64 * 1024;
const OUTPUT_LIMIT_EVIDENCE_BYTES: usize = 4 * 1024;
const GITHUB_AUTH_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const GITHUB_APP_GIT_TOKEN_ENV: &str = "CARA_GITHUB_APP_GIT_TOKEN";
const GITHUB_APP_GIT_CREDENTIAL_HELPER: &str = "!f() { if test \"$1\" = get; then printf '%s\\n' 'username=x-access-token' \"password=$CARA_GITHUB_APP_GIT_TOKEN\"; fi; }; f";

/// Whether one checkout's `origin` names a GitHub repository at all.
///
/// Separates "Cara could not determine the repository" from "Cara could not
/// authenticate" so a diagnostic never sends a reader to rotate credentials for
/// a remote-configuration problem (bd-ce545f).
static GITHUB_REMOTE_IS_GITHUB: OnceLock<Mutex<HashMap<std::path::PathBuf, bool>>> =
    OnceLock::new();

static GITHUB_AUTH_CACHE: OnceLock<Mutex<HashMap<String, Option<GithubAuthSelection>>>> =
    OnceLock::new();
static GITHUB_APP_PRINCIPALS: OnceLock<Mutex<HashMap<String, (String, u64)>>> = OnceLock::new();

#[derive(Clone)]
enum GithubAuthSelection {
    Ambient,
    Token(String),
    AppInstallation(GithubAppCredential),
    Refused(String),
}

#[derive(Clone)]
struct GithubAppCredential {
    token: String,
    app_slug: String,
    installation_id: u64,
    expires_unix_secs: u64,
}

impl GithubAppCredential {
    fn usable(&self) -> bool {
        current_unix_secs().saturating_add(60) < self.expires_unix_secs
    }
}

#[derive(Clone)]
struct GithubAppGitAuth {
    credential: GithubAppCredential,
    repository: GithubRepository,
}

#[derive(Clone)]
struct GithubAppMode {
    credential_command: String,
    expected_slug: String,
    expected_installation_id: u64,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GithubAppCredentialResponse {
    token: String,
    app_slug: String,
    installation_id: u64,
    repository: String,
    expires_unix_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GithubGitTransport {
    Https,
    Http,
    Ssh,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GithubRepository {
    host: String,
    owner: String,
    name: String,
    git_transport: GithubGitTransport,
}

impl GithubRepository {
    fn cache_key(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.name)
    }

    fn api_path(&self) -> String {
        format!("repos/{}/{}", self.owner, self.name)
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
struct CommandIo {
    env: BTreeMap<String, String>,
    stdin: Option<String>,
}

impl std::fmt::Debug for CommandIo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandIo")
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .field("stdin", &self.stdin.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// A subprocess request with arguments kept separate from shell parsing.
#[derive(Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Executable name or path.
    pub program: String,
    /// Exact argument vector.
    pub args: Vec<String>,
    /// Optional I/O additions stay boxed so ordinary command/error values remain small.
    io: Option<Box<CommandIo>>,
}

impl std::fmt::Debug for CommandSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandSpec")
            .field("program", &self.program)
            .field(
                "args",
                &self
                    .args
                    .iter()
                    .map(|argument| redact_url_userinfo(argument))
                    .collect::<Vec<_>>(),
            )
            .field("io", &self.io)
            .finish()
    }
}

impl CommandSpec {
    /// Start a command request.
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            io: None,
        }
    }

    /// Append one argument.
    #[must_use]
    pub fn arg(mut self, argument: impl Into<String>) -> Self {
        self.args.push(argument.into());
        self
    }

    /// Append an argument iterator.
    #[must_use]
    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(arguments.into_iter().map(Into::into));
        self
    }

    /// Add one explicit environment value without exposing it in diagnostics.
    #[must_use]
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.io
            .get_or_insert_with(|| Box::new(CommandIo::default()))
            .env
            .insert(name.into(), value.into());
        self
    }

    /// Provide a UTF-8 stdin payload.
    #[must_use]
    pub fn stdin(mut self, input: impl Into<String>) -> Self {
        self.io
            .get_or_insert_with(|| Box::new(CommandIo::default()))
            .stdin = Some(input.into());
        self
    }

    fn has_env(&self, name: &str) -> bool {
        self.io.as_ref().is_some_and(|io| io.env.contains_key(name))
    }

    /// Render a diagnostic-only command line without executing a shell.
    #[must_use]
    pub fn display(&self) -> String {
        std::iter::once(self.program.clone())
            .chain(
                self.args
                    .iter()
                    .map(|argument| redact_url_userinfo(argument)),
            )
            .map(|value| quote_for_diagnostic(&value))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn redact_url_userinfo(value: &str) -> String {
    for scheme in ["https://", "http://"] {
        if let Some(rest) = value.strip_prefix(scheme)
            && let Some((userinfo, host_and_path)) = rest.split_once('@')
            && !userinfo.is_empty()
            && host_and_path.contains('/')
        {
            return format!("{scheme}<redacted>@{host_and_path}");
        }
    }
    value.to_owned()
}

fn quote_for_diagnostic(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-._/:,=".contains(character))
    {
        value.to_owned()
    } else {
        format!("'{escaped}'", escaped = value.replace('\'', "'\\''"))
    }
}

/// Captured subprocess result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Process exit code, or `None` when the process ended through a signal.
    pub code: Option<i32>,
    /// UTF-8 standard output.
    pub stdout: String,
    /// UTF-8 standard error.
    pub stderr: String,
}

impl CommandOutput {
    /// Construct a successful output, primarily for fake runners.
    #[must_use]
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// Construct a failed output, primarily for fake runners.
    #[must_use]
    pub fn failure(code: i32, stderr: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    /// Whether the child returned exit code zero.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Bounded prefix/suffix evidence for one completely drained child stream.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StreamCaptureEvidence {
    pub limit_bytes: usize,
    pub total_bytes: u64,
    pub truncated: bool,
    pub prefix: String,
    pub suffix: String,
}

/// Failure to start a subprocess or decode its output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandRunError {
    /// The shared authenticated GitHub subprocess budget was exhausted.
    GithubRequestBudgetExceeded {
        /// Command which would have exceeded the bound.
        command: CommandSpec,
        /// Configured maximum authenticated `gh` subprocesses.
        limit: u32,
        /// Requests already reserved by this operation.
        used: u32,
    },
    /// Explicit GitHub App mode could not produce a valid exact-repository
    /// installation credential. It never falls back to ambient user auth.
    GithubAppAuthRefused {
        command: CommandSpec,
        message: String,
    },
    /// App mode could not bind remote Git transport to the same exact HTTPS
    /// repository and installation credential as provider API operations.
    GithubAppGitTransportRefused {
        command: CommandSpec,
        message: String,
    },
    /// The executable could not be started or waited for.
    Spawn {
        /// Requested command.
        command: CommandSpec,
        /// Operating-system error text.
        message: String,
    },
    /// A command emitted bytes that were not UTF-8.
    InvalidUtf8 {
        /// Requested command.
        command: CommandSpec,
        /// Stream containing invalid bytes.
        stream: &'static str,
        /// Decoder error text.
        message: String,
    },
    /// One or both completely drained output streams exceeded their independent bound.
    OutputLimit {
        /// Requested command.
        command: CommandSpec,
        /// Child exit status, retained even though output cannot be consumed safely.
        code: Option<i32>,
        /// Independently bounded stdout evidence.
        stdout: Box<StreamCaptureEvidence>,
        /// Independently bounded stderr evidence.
        stderr: Box<StreamCaptureEvidence>,
    },
    /// The child exceeded its hard deadline and was terminated and reaped.
    Timeout {
        /// Requested command.
        command: CommandSpec,
        /// Owned Unix process group (equal to child PID) when a child started.
        process_group_id: Option<u32>,
        /// Configured hard deadline in milliseconds.
        timeout_ms: u64,
        /// Bounded output captured before termination.
        stdout: String,
        /// Bounded diagnostic output captured before termination.
        stderr: String,
    },
}

impl std::fmt::Display for CommandRunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GithubRequestBudgetExceeded {
                command,
                limit,
                used,
            } => write!(
                formatter,
                "`{}` would exceed the GitHub request budget ({used}/{limit} already used)",
                command.display()
            ),
            Self::GithubAppAuthRefused { command, message } => write!(
                formatter,
                "GitHub App authentication refused `{}`: {message}",
                command.display()
            ),
            Self::GithubAppGitTransportRefused { command, message } => write!(
                formatter,
                "GitHub App git transport refused `{}`: {message}",
                command.display()
            ),
            Self::Spawn { command, message } => {
                write!(
                    formatter,
                    "could not run `{}`: {message}",
                    command.display()
                )
            }
            Self::InvalidUtf8 {
                command,
                stream,
                message,
            } => write!(
                formatter,
                "`{}` emitted invalid UTF-8 on {stream}: {message}",
                command.display()
            ),
            Self::OutputLimit {
                command,
                stdout,
                stderr,
                ..
            } => write!(
                formatter,
                "`{}` exceeded its output capture bound (stdout {}/{}, stderr {}/{})",
                command.display(),
                stdout.total_bytes,
                stdout.limit_bytes,
                stderr.total_bytes,
                stderr.limit_bytes,
            ),
            Self::Timeout {
                command,
                timeout_ms,
                ..
            } => write!(
                formatter,
                "`{}` exceeded its {timeout_ms}ms deadline and was terminated",
                command.display()
            ),
        }
    }
}

impl std::error::Error for CommandRunError {}

/// Injectable command execution interface.
pub trait CommandRunner {
    /// Execute a subprocess request and capture all output.
    fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError>;

    /// Return secret-free GitHub API telemetry accumulated by this runner.
    fn github_api_telemetry(&self) -> crate::model::GitHubApiTelemetry {
        crate::model::GitHubApiTelemetry::default()
    }
}

/// Shared exact bound for authenticated `gh` subprocesses in one operation.
#[derive(Debug, Clone)]
pub struct GithubRequestBudget {
    limit: u32,
    used: Arc<AtomicU32>,
}

impl GithubRequestBudget {
    #[must_use]
    pub fn new(limit: u32) -> Self {
        Self {
            limit,
            used: Arc::new(AtomicU32::new(0)),
        }
    }

    fn reserve(&self, command: &CommandSpec) -> Result<(), CommandRunError> {
        self.used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < self.limit).then_some(used + 1)
            })
            .map(|_| ())
            .map_err(|used| CommandRunError::GithubRequestBudgetExceeded {
                command: command.clone(),
                limit: self.limit,
                used,
            })
    }

    #[must_use]
    pub fn used(&self) -> u32 {
        self.used.load(Ordering::SeqCst)
    }

    #[must_use]
    pub const fn limit(&self) -> u32 {
        self.limit
    }
}

/// Production subprocess runner.
#[derive(Debug, Clone)]
pub struct ProcessRunner {
    cwd: Option<PathBuf>,
    timeout: Duration,
    operation_deadline: Option<Instant>,
    github_request_budget: Option<GithubRequestBudget>,
    infer_github_auth: bool,
    github_app_auth_retry: bool,
    stdout_capture_limit: usize,
    stderr_capture_limit: usize,
    github_api_telemetry: Arc<Mutex<crate::model::GitHubApiTelemetry>>,
}

impl Default for ProcessRunner {
    fn default() -> Self {
        Self {
            cwd: None,
            timeout: DEFAULT_COMMAND_TIMEOUT,
            operation_deadline: None,
            github_request_budget: None,
            infer_github_auth: true,
            github_app_auth_retry: true,
            stdout_capture_limit: MAX_STDOUT_CAPTURE_BYTES,
            stderr_capture_limit: MAX_STDERR_CAPTURE_BYTES,
            github_api_telemetry: Arc::new(Mutex::new(crate::model::GitHubApiTelemetry::default())),
        }
    }
}

impl ProcessRunner {
    /// Use the process's current directory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Run every command from the specified repository directory.
    #[must_use]
    pub fn in_directory(path: impl AsRef<Path>) -> Self {
        Self {
            cwd: Some(path.as_ref().to_path_buf()),
            timeout: DEFAULT_COMMAND_TIMEOUT,
            operation_deadline: None,
            github_request_budget: None,
            infer_github_auth: true,
            github_app_auth_retry: true,
            stdout_capture_limit: MAX_STDOUT_CAPTURE_BYTES,
            stderr_capture_limit: MAX_STDERR_CAPTURE_BYTES,
            github_api_telemetry: Arc::new(Mutex::new(crate::model::GitHubApiTelemetry::default())),
        }
    }

    /// Read the secret-free GitHub API telemetry shared by runner clones.
    #[must_use]
    pub fn github_api_telemetry(&self) -> crate::model::GitHubApiTelemetry {
        self.github_api_telemetry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Override the hard deadline for every command run by this instance.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.max(Duration::from_millis(1));
        self
    }

    /// Share one absolute deadline across a multi-command operation. Each child
    /// receives only the smaller of its normal timeout and the operation's
    /// remaining budget.
    #[must_use]
    pub fn with_operation_deadline(mut self, deadline: Instant) -> Self {
        self.operation_deadline = Some(deadline);
        self
    }

    #[cfg(test)]
    #[must_use]
    fn with_capture_limits(mut self, stdout: usize, stderr: usize) -> Self {
        self.stdout_capture_limit = stdout.max(1);
        self.stderr_capture_limit = stderr.max(1);
        self
    }

    /// Share one exact authenticated `gh` request budget across runners.
    #[must_use]
    pub fn with_github_request_budget(mut self, budget: GithubRequestBudget) -> Self {
        self.github_request_budget = Some(budget);
        self
    }

    fn without_github_auth_inference(mut self) -> Self {
        self.infer_github_auth = false;
        self
    }

    fn inferred_github_auth(&self, request: &CommandSpec) -> Option<GithubAuthSelection> {
        let is_gh = Path::new(&request.program)
            .file_name()
            .is_some_and(|name| name == "gh");
        if !self.infer_github_auth
            || !is_gh
            || request.has_env("GH_TOKEN")
            || request.has_env("GITHUB_TOKEN")
        {
            return None;
        }
        match github_app_mode() {
            Err(message) => Some(GithubAuthSelection::Refused(message)),
            Ok(Some(_)) => resolve_github_auth(self.cwd.as_deref(), self.operation_deadline)
                .or_else(|| {
                    Some(GithubAuthSelection::Refused(
                        "App mode could not resolve the exact repository".to_owned(),
                    ))
                }),
            Ok(None) => resolve_github_auth(self.cwd.as_deref(), self.operation_deadline),
        }
    }

    fn inferred_github_app_git_auth(
        &self,
        request: &CommandSpec,
    ) -> Result<Option<GithubAppGitAuth>, String> {
        if !self.infer_github_auth || !is_remote_git_request(request) {
            return Ok(None);
        }
        if github_app_mode()?.is_none() {
            return Ok(None);
        }
        let cwd = self.cwd.as_deref().unwrap_or_else(|| Path::new("."));
        let runner = auth_probe_runner(cwd, self.operation_deadline);
        let origin = discover_github_repository(&runner, cwd);
        let explicit = explicit_github_repository(request)?;
        let repository = origin
            .or(explicit)
            .ok_or_else(|| "no exact GitHub repository remote is available".to_owned())?;
        validate_github_app_git_repository(&repository, request)?;
        validate_github_app_git_configuration(&runner)?;
        match resolve_github_auth_for_repository(&runner, &repository) {
            Some(GithubAuthSelection::AppInstallation(credential)) => Ok(Some(GithubAppGitAuth {
                credential,
                repository,
            })),
            Some(GithubAuthSelection::Refused(message)) => Err(message),
            _ => Err("App mode did not resolve an installation credential".to_owned()),
        }
    }

    /// True when this checkout's origin was probed and is not a GitHub remote.
    fn origin_is_not_github(&self) -> bool {
        let Some(cwd) = self.cwd.as_deref() else {
            return false;
        };
        GITHUB_REMOTE_IS_GITHUB.get().is_some_and(|cache| {
            cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(cwd)
                .is_some_and(|is_github| !is_github)
        })
    }

    fn record_github_request(&self, request: &CommandSpec, auth: Option<&GithubAuthSelection>) {
        if !is_gh_request(request) {
            return;
        }
        let mut telemetry = self
            .github_api_telemetry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        telemetry.calls = telemetry.calls.saturating_add(1);
        let is_api = request
            .args
            .first()
            .is_some_and(|argument| argument == "api");
        let is_graphql = is_api && request.args.iter().any(|argument| argument == "graphql");
        if is_graphql {
            telemetry.graphql_calls = telemetry.graphql_calls.saturating_add(1);
        } else if is_api {
            telemetry.rest_calls = telemetry.rest_calls.saturating_add(1);
        } else {
            telemetry.gh_cli_calls = telemetry.gh_cli_calls.saturating_add(1);
        }
        let explicit = request.has_env("GH_TOKEN") || request.has_env("GITHUB_TOKEN");
        telemetry.authenticated = explicit
            || auth.is_some_and(|selection| !matches!(selection, GithubAuthSelection::Refused(_)));
        telemetry.auth_source = Some(if explicit {
            "explicit_command_token".to_owned()
        } else {
            match auth {
                Some(GithubAuthSelection::Ambient) => std::env::var("CARA_GITHUB_AUTH_KIND")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "ambient_token".to_owned()),
                Some(GithubAuthSelection::Token(_)) => "gh_auth_account".to_owned(),
                Some(GithubAuthSelection::AppInstallation(credential)) => {
                    telemetry.github_app_slug = Some(credential.app_slug.clone());
                    telemetry.github_app_installation_id = Some(credential.installation_id);
                    telemetry.github_app_token_expires_unix_secs =
                        Some(credential.expires_unix_secs);
                    "github_app_installation".to_owned()
                }
                Some(GithubAuthSelection::Refused(_)) => "github_app_refused".to_owned(),
                // Never claim an auth verdict Cara did not reach. A checkout
                // whose origin is not GitHub cannot name a repository to probe
                // against, and reporting that as `unauthenticated` misdirects
                // the reader toward credentials (bd-ce545f).
                None if self.origin_is_not_github() => {
                    "repository_unresolved_from_git_remotes".to_owned()
                }
                None => "gh_default_or_unauthenticated".to_owned(),
            }
        });
    }

    fn record_github_app_git_transport(&self, auth: Option<&GithubAppGitAuth>) {
        let Some(auth) = auth else {
            return;
        };
        let mut telemetry = self
            .github_api_telemetry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        telemetry.authenticated = true;
        telemetry.auth_source = Some("github_app_installation".to_owned());
        telemetry.github_app_slug = Some(auth.credential.app_slug.clone());
        telemetry.github_app_installation_id = Some(auth.credential.installation_id);
        telemetry.github_app_token_expires_unix_secs = Some(auth.credential.expires_unix_secs);
        telemetry.github_app_git_transport = Some("https_credential_helper".to_owned());
        telemetry.github_app_git_repository = Some(format!(
            "{}/{}",
            auth.repository.owner, auth.repository.name
        ));
    }

    fn record_github_response(&self, request: &CommandSpec, output: &CommandOutput) {
        if !is_gh_request(request) || !output.is_success() {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&output.stdout) else {
            return;
        };
        let rate = value
            .get("data")
            .and_then(|data| data.get("rateLimit"))
            .or_else(|| value.get("rateLimit"));
        let Some(rate) = rate else {
            return;
        };
        let Some(cost) = rate.get("cost").and_then(serde_json::Value::as_u64) else {
            return;
        };
        let Some(remaining) = rate.get("remaining").and_then(serde_json::Value::as_u64) else {
            return;
        };
        let Some(reset_at) = rate.get("resetAt").and_then(serde_json::Value::as_str) else {
            return;
        };
        self.github_api_telemetry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rate_limit = Some(crate::model::GitHubRateLimit {
            cost,
            remaining,
            reset_at: reset_at.to_owned(),
        });
    }

    fn effective_timeout(&self, request: &CommandSpec) -> Result<Duration, CommandRunError> {
        let remaining = self
            .operation_deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let timeout = remaining.map_or(self.timeout, |remaining| self.timeout.min(remaining));
        // A sliver of budget is not a budget. Launching a command with a
        // deadline no real command can meet reports the *command* as the
        // failure, which sends a reader chasing git or the provider when the
        // true cause is an exhausted operation deadline. Refuse just above zero
        // and name the real cause instead (bd-89e59f).
        let exhausted = remaining.is_some_and(|remaining| remaining < MINIMUM_COMMAND_BUDGET);
        if timeout.is_zero() || exhausted {
            return Err(CommandRunError::Timeout {
                command: request.clone(),
                process_group_id: None,
                timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                stdout: String::new(),
                stderr: format!(
                    "operation deadline exhausted before this phase: {}ms remained, below the {}ms minimum any command can use; raise sync.max_duration_secs or lower sync.max_candidates_per_tick so a tick completes within its budget",
                    timeout.as_millis(),
                    MINIMUM_COMMAND_BUDGET.as_millis(),
                ),
            });
        }
        Ok(timeout)
    }
}

/// Smallest budget any real command can use.
///
/// Below this a launch is guaranteed waste: the child is reaped before it can
/// finish, and the resulting error names the command rather than the exhausted
/// deadline that actually caused it.
const MINIMUM_COMMAND_BUDGET: Duration = Duration::from_millis(750);

fn invalidate_github_app_auth_cache() {
    if let Some(cache) = GITHUB_AUTH_CACHE.get() {
        cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, selection| {
                !matches!(selection, Some(GithubAuthSelection::AppInstallation(_)))
            });
    }
}

fn auth_probe_runner(cwd: &Path, operation_deadline: Option<Instant>) -> ProcessRunner {
    let runner = ProcessRunner::in_directory(cwd)
        .with_timeout(GITHUB_AUTH_PROBE_TIMEOUT)
        .without_github_auth_inference();
    operation_deadline.map_or(runner.clone(), |deadline| {
        runner.with_operation_deadline(deadline)
    })
}

fn discover_github_repository(runner: &ProcessRunner, cwd: &Path) -> Option<GithubRepository> {
    let remote = runner
        .run(&CommandSpec::new("git").args(["config", "--get-all", "remote.origin.url"]))
        .ok()
        .filter(CommandOutput::is_success)?;
    let mut urls = remote.stdout.lines().filter(|url| !url.trim().is_empty());
    let parsed = urls.next().and_then(parse_github_remote);
    let parsed = if urls.next().is_none() { parsed } else { None };
    // Record why a probe could not run. Collapsing "this checkout's origin is
    // not GitHub" into an auth verdict sent readers to re-run `gh auth login`,
    // mutating real credential state for a problem that was never about
    // credentials (bd-ce545f).
    GITHUB_REMOTE_IS_GITHUB
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(cwd.to_path_buf(), parsed.is_some());
    parsed
}

fn resolve_github_auth(
    cwd: Option<&Path>,
    operation_deadline: Option<Instant>,
) -> Option<GithubAuthSelection> {
    let cwd = cwd?;
    let runner = auth_probe_runner(cwd, operation_deadline);
    let repository = discover_github_repository(&runner, cwd)?;
    resolve_github_auth_for_repository(&runner, &repository)
}

fn resolve_github_auth_for_repository(
    runner: &ProcessRunner,
    repository: &GithubRepository,
) -> Option<GithubAuthSelection> {
    let app_mode = !matches!(github_app_mode(), Ok(None));
    let key = repository.cache_key();
    let cache = GITHUB_AUTH_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    // Retain this guard through refresh. Broker/token exchange is infrequent and
    // this gives all concurrent runners one process-wide single-flight instead
    // of minting several installation tokens for the same expiry boundary.
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(selection) = cache.get(&key).cloned() {
        let mode_matches =
            matches!(selection, Some(GithubAuthSelection::AppInstallation(_))) == app_mode;
        match &selection {
            Some(GithubAuthSelection::AppInstallation(credential))
                if mode_matches && !credential.usable() => {}
            _ if mode_matches => return selection,
            _ => {}
        }
    }
    let selection = resolve_github_auth_uncached(runner, repository);
    if !matches!(selection, Some(GithubAuthSelection::Refused(_))) {
        cache.insert(key, selection.clone());
    }
    selection
}

fn resolve_github_auth_uncached(
    runner: &ProcessRunner,
    repository: &GithubRepository,
) -> Option<GithubAuthSelection> {
    match github_app_mode() {
        Err(message) => return Some(GithubAuthSelection::Refused(message)),
        Ok(Some(mode)) => {
            return Some(
                match resolve_github_app_credential(runner, repository, &mode) {
                    Ok(credential) => GithubAuthSelection::AppInstallation(credential),
                    Err(message) => GithubAuthSelection::Refused(message),
                },
            );
        }
        Ok(None) => {}
    }

    let ambient = std::env::var("GH_TOKEN")
        .ok()
        .filter(|token| !token.trim().is_empty())
        .or_else(|| {
            std::env::var("GITHUB_TOKEN")
                .ok()
                .filter(|token| !token.trim().is_empty())
        });
    // An explicitly supplied ambient token is already an operator/runner
    // choice. Let the first real provider request validate it rather than
    // spending one REST request per short-lived Cara process on a redundant
    // access probe. Account fallback still probes because it must choose among
    // potentially unrelated gh logins.
    if ambient.is_some() {
        return Some(GithubAuthSelection::Ambient);
    }

    if let Some(token) = github_token_for_login(runner, repository, &repository.owner)
        && github_token_can_access(runner, repository, &token)
    {
        return Some(GithubAuthSelection::Token(token));
    }

    let status_command = CommandSpec::new("gh")
        .args([
            "auth",
            "status",
            "--hostname",
            repository.host.as_str(),
            "--json",
            "hosts",
        ])
        .env("GH_TOKEN", "")
        .env("GITHUB_TOKEN", "");
    let status = runner.run(&status_command).ok();
    let status_json = status
        .as_ref()
        .filter(|output| output.is_success())
        .map(|output| output.stdout.as_str());
    for login in github_auth_candidates(repository, status_json)
        .into_iter()
        .filter(|login| login != &repository.owner)
    {
        if let Some(token) = github_token_for_login(runner, repository, &login)
            && github_token_can_access(runner, repository, &token)
        {
            return Some(GithubAuthSelection::Token(token));
        }
    }
    None
}

fn current_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn github_app_mode_from_values(
    mode: &str,
    credential_command: Option<String>,
    expected_slug: Option<String>,
    expected_installation_id: Option<String>,
) -> Result<Option<GithubAppMode>, String> {
    match mode.trim() {
        "" | "ambient" => Ok(None),
        "app_installation" => {
            let credential_command = credential_command
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "App mode requires a credential broker command".to_owned())?;
            let expected_slug = expected_slug
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "App mode requires an expected App slug".to_owned())?;
            let expected_installation_id = expected_installation_id
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|installation_id| *installation_id > 0)
                .ok_or_else(|| "App mode requires an expected installation ID".to_owned())?;
            Ok(Some(GithubAppMode {
                credential_command,
                expected_slug,
                expected_installation_id,
            }))
        }
        _ => Err("unknown CARA_GITHUB_AUTH_MODE; expected ambient or app_installation".to_owned()),
    }
}

fn github_app_mode() -> Result<Option<GithubAppMode>, String> {
    github_app_mode_from_values(
        &std::env::var("CARA_GITHUB_AUTH_MODE").unwrap_or_default(),
        std::env::var("CARA_GITHUB_APP_CREDENTIAL_COMMAND").ok(),
        std::env::var("CARA_GITHUB_APP_SLUG").ok(),
        std::env::var("CARA_GITHUB_APP_INSTALLATION_ID").ok(),
    )
}

fn resolve_github_app_credential(
    runner: &ProcessRunner,
    repository: &GithubRepository,
    mode: &GithubAppMode,
) -> Result<GithubAppCredential, String> {
    let expected_repository = format!("{}/{}", repository.owner, repository.name);
    let output = runner
        .run(
            &CommandSpec::new(&mode.credential_command)
                .env("CARA_GITHUB_APP_REPOSITORY", &expected_repository)
                .env("CARA_GITHUB_APP_HOST", &repository.host),
        )
        .map_err(|_| "credential broker could not be executed".to_owned())?;
    if !output.is_success() {
        return Err("credential broker returned a nonzero exit status".to_owned());
    }
    let response: GithubAppCredentialResponse = serde_json::from_str(&output.stdout)
        .map_err(|_| "credential broker returned invalid JSON".to_owned())?;
    let credential = validate_github_app_credential(response, repository)?;
    if credential.app_slug != mode.expected_slug {
        return Err("credential broker response names a different App slug".to_owned());
    }
    if credential.installation_id != mode.expected_installation_id {
        return Err("credential broker response names a different installation".to_owned());
    }
    bind_github_app_principal(repository, &credential)?;
    if !github_token_can_access(runner, repository, &credential.token) {
        return Err("App installation token cannot access the exact repository".to_owned());
    }
    Ok(credential)
}

fn bind_github_app_principal(
    repository: &GithubRepository,
    credential: &GithubAppCredential,
) -> Result<(), String> {
    let mut principals = GITHUB_APP_PRINCIPALS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let expected = (credential.app_slug.clone(), credential.installation_id);
    match principals.get(&repository.cache_key()) {
        Some(bound) if bound != &expected => {
            Err("credential refresh changed the bound App installation principal".to_owned())
        }
        Some(_) => Ok(()),
        None => {
            principals.insert(repository.cache_key(), expected);
            Ok(())
        }
    }
}

fn validate_github_app_credential(
    response: GithubAppCredentialResponse,
    repository: &GithubRepository,
) -> Result<GithubAppCredential, String> {
    if response.token.trim().is_empty() {
        return Err("credential broker returned an empty token".to_owned());
    }
    if response.repository != format!("{}/{}", repository.owner, repository.name) {
        return Err("credential broker response names a different repository".to_owned());
    }
    if response.app_slug.trim().is_empty() || response.installation_id == 0 {
        return Err("credential broker returned invalid App installation identity".to_owned());
    }
    let credential = GithubAppCredential {
        token: response.token,
        app_slug: response.app_slug,
        installation_id: response.installation_id,
        expires_unix_secs: response.expires_unix_secs,
    };
    if !credential.usable() {
        return Err("credential broker returned an expired or near-expiry token".to_owned());
    }
    Ok(credential)
}

fn github_token_for_login(
    runner: &ProcessRunner,
    repository: &GithubRepository,
    login: &str,
) -> Option<String> {
    runner
        .run(
            &CommandSpec::new("gh")
                .args([
                    "auth",
                    "token",
                    "--hostname",
                    repository.host.as_str(),
                    "--user",
                    login,
                ])
                .env("GH_TOKEN", "")
                .env("GITHUB_TOKEN", ""),
        )
        .ok()
        .filter(CommandOutput::is_success)
        .map(|output| output.stdout.trim().to_owned())
        .filter(|token| !token.is_empty())
}

fn github_token_can_access(
    runner: &ProcessRunner,
    repository: &GithubRepository,
    token: &str,
) -> bool {
    runner
        .run(
            &CommandSpec::new("gh")
                .args([
                    "api",
                    "--hostname",
                    repository.host.as_str(),
                    "--silent",
                    repository.api_path().as_str(),
                ])
                .env("GH_TOKEN", token),
        )
        .is_ok_and(|output| output.is_success())
}

fn github_auth_candidates(repository: &GithubRepository, status_json: Option<&str>) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();
    if seen.insert(repository.owner.clone()) {
        candidates.push(repository.owner.clone());
    }
    let Some(accounts) = status_json
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .and_then(|value| {
            value
                .get("hosts")?
                .get(&repository.host)?
                .as_array()
                .cloned()
        })
    else {
        return candidates;
    };
    for active in [true, false] {
        for account in &accounts {
            let successful =
                account.get("state").and_then(serde_json::Value::as_str) == Some("success");
            let matches_active =
                account.get("active").and_then(serde_json::Value::as_bool) == Some(active);
            let login = account
                .get("login")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if successful && matches_active && !login.is_empty() && seen.insert(login.to_owned()) {
                candidates.push(login.to_owned());
            }
        }
    }
    candidates
}

fn parse_github_remote(remote: &str) -> Option<GithubRepository> {
    let (host, path, git_transport) = if let Some(rest) = remote.strip_prefix("ssh://") {
        let (authority, path) = rest.split_once('/')?;
        (authority.rsplit('@').next()?, path, GithubGitTransport::Ssh)
    } else if let Some(rest) = remote.strip_prefix("https://") {
        let (host, path) = rest.split_once('/')?;
        if host.contains('@') {
            return None;
        }
        (host, path, GithubGitTransport::Https)
    } else if let Some(rest) = remote.strip_prefix("http://") {
        let (host, path) = rest.split_once('/')?;
        if host.contains('@') {
            return None;
        }
        (host, path, GithubGitTransport::Http)
    } else {
        let (authority, path) = remote.split_once(':')?;
        (authority.rsplit('@').next()?, path, GithubGitTransport::Ssh)
    };
    let mut parts = path
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(GithubRepository {
        host: host.to_owned(),
        owner: owner.to_owned(),
        name: name.to_owned(),
        git_transport,
    })
}

fn is_gh_request(request: &CommandSpec) -> bool {
    Path::new(&request.program)
        .file_name()
        .is_some_and(|name| name == "gh")
}

fn should_refresh_github_app_auth(
    selection: Option<&GithubAuthSelection>,
    output: &CommandOutput,
    retry_allowed: bool,
) -> bool {
    retry_allowed
        && matches!(selection, Some(GithubAuthSelection::AppInstallation(_)))
        && output.code == Some(1)
        && (output.stderr.contains("HTTP 401")
            || output
                .stderr
                .to_ascii_lowercase()
                .contains("bad credentials"))
}

fn validate_github_app_git_configuration(runner: &ProcessRunner) -> Result<(), String> {
    let forbidden = runner
        .run(&CommandSpec::new("git").args([
            "config",
            "--get-regexp",
            r"^(url\..*\.insteadof|remote\..*\.pushurl|http\..*\.sslverify)$",
        ]))
        .map_err(|_| "could not inspect Git transport configuration".to_owned())?;
    match forbidden.code {
        Some(0 | 1) if forbidden.stdout.trim().is_empty() => Ok(()),
        Some(0) => Err(
            "Git URL rewrites, pushurl, and URL-specific TLS overrides are forbidden in App mode"
                .to_owned(),
        ),
        _ => Err("could not inspect Git transport configuration".to_owned()),
    }
}

fn explicit_github_repository(request: &CommandSpec) -> Result<Option<GithubRepository>, String> {
    let mut selected: Option<GithubRepository> = None;
    for argument in &request.args {
        if redact_url_userinfo(argument) != *argument {
            return Err("credential-bearing remote URLs are forbidden in App mode".to_owned());
        }
        let Some(repository) = parse_github_remote(argument) else {
            continue;
        };
        if selected.as_ref().is_some_and(|existing| {
            existing.cache_key() != repository.cache_key()
                || existing.git_transport != repository.git_transport
        }) {
            return Err("command names more than one repository or transport".to_owned());
        }
        selected = Some(repository);
    }
    Ok(selected)
}

fn validate_github_app_git_repository(
    repository: &GithubRepository,
    request: &CommandSpec,
) -> Result<(), String> {
    if repository.git_transport != GithubGitTransport::Https {
        return Err(
            "App transport requires an HTTPS origin; SSH and plaintext HTTP refuse".to_owned(),
        );
    }
    for argument in &request.args {
        if let Some(explicit) = parse_github_remote(argument)
            && (explicit.cache_key() != repository.cache_key()
                || explicit.git_transport != GithubGitTransport::Https)
        {
            return Err("command names a different repository or non-HTTPS remote".to_owned());
        }
    }
    Ok(())
}

fn is_remote_git_request(request: &CommandSpec) -> bool {
    Path::new(&request.program)
        .file_name()
        .is_some_and(|name| name == "git")
        && request
            .args
            .iter()
            .any(|argument| matches!(argument.as_str(), "fetch" | "push" | "ls-remote" | "clone"))
}

fn github_app_git_environment(auth: &GithubAppGitAuth) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            GITHUB_APP_GIT_TOKEN_ENV.to_owned(),
            auth.credential.token.clone(),
        ),
        ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
        ("GCM_INTERACTIVE".to_owned(), "Never".to_owned()),
        ("GIT_CONFIG_COUNT".to_owned(), "5".to_owned()),
        (
            "GIT_CONFIG_KEY_0".to_owned(),
            "credential.helper".to_owned(),
        ),
        ("GIT_CONFIG_VALUE_0".to_owned(), String::new()),
        (
            "GIT_CONFIG_KEY_1".to_owned(),
            "credential.helper".to_owned(),
        ),
        (
            "GIT_CONFIG_VALUE_1".to_owned(),
            GITHUB_APP_GIT_CREDENTIAL_HELPER.to_owned(),
        ),
        (
            "GIT_CONFIG_KEY_2".to_owned(),
            "credential.useHttpPath".to_owned(),
        ),
        ("GIT_CONFIG_VALUE_2".to_owned(), "true".to_owned()),
        ("GIT_CONFIG_KEY_3".to_owned(), "core.hooksPath".to_owned()),
        ("GIT_CONFIG_VALUE_3".to_owned(), "/dev/null".to_owned()),
        ("GIT_CONFIG_KEY_4".to_owned(), "http.sslVerify".to_owned()),
        ("GIT_CONFIG_VALUE_4".to_owned(), "true".to_owned()),
    ])
}

fn should_refresh_github_app_git_auth(
    auth: Option<&GithubAppGitAuth>,
    output: &CommandOutput,
    retry_allowed: bool,
) -> bool {
    if !retry_allowed || auth.is_none() || output.is_success() {
        return false;
    }
    let stderr = output.stderr.to_ascii_lowercase();
    stderr.contains("authentication failed")
        || stderr.contains("bad credentials")
        || stderr.contains("http 401")
        || stderr.contains("could not read username")
}

impl CommandRunner for ProcessRunner {
    #[allow(clippy::too_many_lines)]
    fn run(&self, request: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
        let is_gh = Path::new(&request.program)
            .file_name()
            .is_some_and(|name| name == "gh");
        if is_gh {
            if let Some(budget) = &self.github_request_budget {
                budget.reserve(request)?;
            }
        }
        let timeout = self.effective_timeout(request)?;
        let github_auth = self.inferred_github_auth(request);
        let github_app_git_auth =
            self.inferred_github_app_git_auth(request)
                .map_err(|message| CommandRunError::GithubAppGitTransportRefused {
                    command: request.clone(),
                    message,
                })?;
        self.record_github_request(request, github_auth.as_ref());
        self.record_github_app_git_transport(github_app_git_auth.as_ref());
        if let Some(GithubAuthSelection::Refused(message)) = &github_auth {
            return Err(CommandRunError::GithubAppAuthRefused {
                command: request.clone(),
                message: message.clone(),
            });
        }
        let mut command = Command::new(&request.program);
        command
            .args(&request.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(io) = &request.io {
            command.envs(&io.env);
            if io.stdin.is_some() {
                command.stdin(Stdio::piped());
            }
        }
        match &github_auth {
            Some(GithubAuthSelection::Token(token)) => command.env("GH_TOKEN", token),
            Some(GithubAuthSelection::AppInstallation(credential)) => {
                command.env("GH_TOKEN", &credential.token)
            }
            _ => &mut command,
        };
        if let Some(auth) = &github_app_git_auth {
            for name in [
                "GIT_SSL_NO_VERIFY",
                "GIT_TRACE",
                "GIT_TRACE_CURL",
                "GIT_CURL_VERBOSE",
                "GIT_CONFIG_PARAMETERS",
            ] {
                command.env_remove(name);
            }
            command.envs(github_app_git_environment(auth));
        }
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command.spawn().map_err(|error| CommandRunError::Spawn {
            command: request.clone(),
            message: error.to_string(),
        })?;
        let child_process_group = child.id();
        let stdin_writer = request
            .io
            .as_ref()
            .and_then(|io| io.stdin.as_ref())
            .map(|input| {
                let mut stdin = child.stdin.take().expect("piped stdin");
                let input = input.clone();
                thread::spawn(move || stdin.write_all(input.as_bytes()))
            });
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdout_limit = self.stdout_capture_limit;
        let stderr_limit = self.stderr_capture_limit;
        let stdout_reader = thread::spawn(move || capture(stdout, stdout_limit));
        let stderr_reader = thread::spawn(move || capture(stderr, stderr_limit));
        let deadline = Instant::now() + timeout;

        let (status, timed_out) = loop {
            match child.try_wait() {
                Ok(Some(status)) => break (status, false),
                Ok(None) if Instant::now() >= deadline => {
                    let status =
                        terminate_and_reap(&mut child).map_err(|error| CommandRunError::Spawn {
                            command: request.clone(),
                            message: format!("could not reap timed-out child: {error}"),
                        })?;
                    break (status, true);
                }
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Err(error) => {
                    let _ = terminate_and_reap(&mut child);
                    if let Some(writer) = stdin_writer {
                        let _ = writer.join();
                    }
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(CommandRunError::Spawn {
                        command: request.clone(),
                        message: error.to_string(),
                    });
                }
            }
        };
        if let Some(writer) = stdin_writer {
            let write_result = writer.join().map_err(|_| CommandRunError::Spawn {
                command: request.clone(),
                message: "stdin writer thread panicked".to_owned(),
            })?;
            // A child is allowed to close stdin without consuming the whole
            // request. Once it has exited and been reaped, its status and
            // captured output are more authoritative than the writer's EPIPE.
            // Other I/O failures still indicate that delivery itself broke.
            if let Err(error) = write_result {
                if error.kind() != std::io::ErrorKind::BrokenPipe {
                    return Err(CommandRunError::Spawn {
                        command: request.clone(),
                        message: format!("could not write stdin: {error}"),
                    });
                }
            }
        }
        let stdout = join_capture(stdout_reader, request, "stdout")?;
        let stderr = join_capture(stderr_reader, request, "stderr")?;
        if timed_out {
            return Err(CommandRunError::Timeout {
                command: request.clone(),
                process_group_id: Some(child_process_group),
                timeout_ms: duration_millis(timeout),
                stdout: stdout.diagnostic_text(),
                stderr: stderr.diagnostic_text(),
            });
        }
        if stdout.truncated || stderr.truncated {
            return Err(CommandRunError::OutputLimit {
                command: request.clone(),
                code: status.code(),
                stdout: Box::new(stdout.evidence(self.stdout_capture_limit)),
                stderr: Box::new(stderr.evidence(self.stderr_capture_limit)),
            });
        }
        let stdout =
            String::from_utf8(stdout.bytes).map_err(|error| CommandRunError::InvalidUtf8 {
                command: request.clone(),
                stream: "stdout",
                message: error.to_string(),
            })?;
        let stderr =
            String::from_utf8(stderr.bytes).map_err(|error| CommandRunError::InvalidUtf8 {
                command: request.clone(),
                stream: "stderr",
                message: error.to_string(),
            })?;

        let output = CommandOutput {
            code: status.code(),
            stdout,
            stderr,
        };
        if should_refresh_github_app_auth(github_auth.as_ref(), &output, self.github_app_auth_retry)
            || should_refresh_github_app_git_auth(
                github_app_git_auth.as_ref(),
                &output,
                self.github_app_auth_retry,
            )
        {
            invalidate_github_app_auth_cache();
            let mut retry = self.clone();
            retry.github_app_auth_retry = false;
            return retry.run(request);
        }
        self.record_github_response(request, &output);
        Ok(output)
    }

    fn github_api_telemetry(&self) -> crate::model::GitHubApiTelemetry {
        self.github_api_telemetry()
    }
}

struct CapturedStream {
    bytes: Vec<u8>,
    suffix: Vec<u8>,
    total_bytes: u64,
    truncated: bool,
}

impl CapturedStream {
    fn evidence(&self, limit_bytes: usize) -> StreamCaptureEvidence {
        StreamCaptureEvidence {
            limit_bytes,
            total_bytes: self.total_bytes,
            truncated: self.truncated,
            prefix: String::from_utf8_lossy(
                &self.bytes[..self.bytes.len().min(OUTPUT_LIMIT_EVIDENCE_BYTES)],
            )
            .into_owned(),
            suffix: String::from_utf8_lossy(&self.suffix).into_owned(),
        }
    }

    fn diagnostic_text(&self) -> String {
        let evidence = self.evidence(self.bytes.len());
        if self.total_bytes <= u64::try_from(self.bytes.len()).unwrap_or(u64::MAX) {
            return String::from_utf8_lossy(&self.bytes).into_owned();
        }
        format!(
            "{}\n...[{} bytes omitted]...\n{}",
            evidence.prefix,
            self.total_bytes.saturating_sub(
                u64::try_from(evidence.prefix.len() + evidence.suffix.len()).unwrap_or(u64::MAX)
            ),
            evidence.suffix
        )
    }
}

fn capture(mut stream: impl Read, limit: usize) -> std::io::Result<CapturedStream> {
    let mut captured = Vec::new();
    let mut suffix = Vec::new();
    let mut total_bytes = 0_u64;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total_bytes = total_bytes.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        let remaining = limit.saturating_sub(captured.len());
        let retained = remaining.min(count);
        captured.extend_from_slice(&buffer[..retained]);
        if count >= OUTPUT_LIMIT_EVIDENCE_BYTES {
            suffix.clear();
            suffix.extend_from_slice(&buffer[count - OUTPUT_LIMIT_EVIDENCE_BYTES..count]);
        } else {
            suffix.extend_from_slice(&buffer[..count]);
            if suffix.len() > OUTPUT_LIMIT_EVIDENCE_BYTES {
                suffix.drain(..suffix.len() - OUTPUT_LIMIT_EVIDENCE_BYTES);
            }
        }
    }
    Ok(CapturedStream {
        truncated: total_bytes > u64::try_from(limit).unwrap_or(u64::MAX),
        bytes: captured,
        suffix,
        total_bytes,
    })
}

fn join_capture(
    handle: thread::JoinHandle<std::io::Result<CapturedStream>>,
    request: &CommandSpec,
    stream: &'static str,
) -> Result<CapturedStream, CommandRunError> {
    handle
        .join()
        .map_err(|_| CommandRunError::Spawn {
            command: request.clone(),
            message: format!("{stream} capture thread panicked"),
        })?
        .map_err(|error| CommandRunError::Spawn {
            command: request.clone(),
            message: format!("could not capture {stream}: {error}"),
        })
}

fn terminate_and_reap(child: &mut Child) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    signal_process_group(child.id(), "-TERM");
    #[cfg(not(unix))]
    let _ = child.kill();

    let grace_deadline = Instant::now() + TERMINATION_GRACE;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= grace_deadline {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }

    #[cfg(unix)]
    signal_process_group(child.id(), "-KILL");
    let _ = child.kill();
    child.wait()
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .args([signal, "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_rendering_quotes_shell_metacharacters_without_using_a_shell() {
        let command = CommandSpec::new("gh").args(["pr", "list", "--label", "caravan queue"]);
        assert_eq!(command.display(), "gh pr list --label 'caravan queue'");
    }

    #[test]
    fn parses_common_github_remote_forms_without_repository_config() {
        let ssh = parse_github_remote("ssh://git@github.com/harryaskham/caravan.git").unwrap();
        let scp = parse_github_remote("git@github.com:harryaskham/caravan.git").unwrap();
        let https = parse_github_remote("https://github.com/harryaskham/caravan.git").unwrap();
        assert_eq!(ssh.cache_key(), https.cache_key());
        assert_eq!(scp.cache_key(), https.cache_key());
        assert_eq!(ssh.git_transport, GithubGitTransport::Ssh);
        assert_eq!(scp.git_transport, GithubGitTransport::Ssh);
        assert_eq!(https.git_transport, GithubGitTransport::Https);
        assert_eq!(parse_github_remote("/tmp/local.git"), None);
    }

    #[test]
    fn app_broker_requires_an_independent_explicit_mode() {
        let broker = Some("/secure/broker".to_owned());
        assert!(
            github_app_mode_from_values("", broker.clone(), None, None)
                .unwrap()
                .is_none()
        );
        assert!(
            github_app_mode_from_values("ambient", broker.clone(), None, None)
                .unwrap()
                .is_none()
        );
        let missing_identity =
            github_app_mode_from_values("app_installation", broker.clone(), None, None)
                .err()
                .expect("identity is mandatory");
        assert!(missing_identity.contains("expected App slug"));
        let valid = github_app_mode_from_values(
            "app_installation",
            broker,
            Some("caravan".to_owned()),
            Some("42".to_owned()),
        )
        .unwrap();
        assert!(valid.is_some());
        let unknown = github_app_mode_from_values("surprise", None, None, None)
            .err()
            .expect("unknown mode refuses");
        assert!(unknown.contains("unknown CARA_GITHUB_AUTH_MODE"));
    }

    #[test]
    fn app_credential_is_exact_repository_scoped_and_expiry_bounded() {
        let repository = GithubRepository {
            host: "github.com".to_owned(),
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
            git_transport: GithubGitTransport::Https,
        };
        let response = GithubAppCredentialResponse {
            token: "installation-secret".to_owned(),
            app_slug: "caravan".to_owned(),
            installation_id: 42,
            repository: "owner/repo".to_owned(),
            expires_unix_secs: current_unix_secs() + 3_600,
        };
        let credential = validate_github_app_credential(response, &repository).unwrap();
        assert!(credential.usable());
        assert_eq!(credential.app_slug, "caravan");
        assert_eq!(credential.installation_id, 42);

        let wrong = GithubAppCredentialResponse {
            token: "never-render-me".to_owned(),
            app_slug: "caravan".to_owned(),
            installation_id: 42,
            repository: "owner/other".to_owned(),
            expires_unix_secs: current_unix_secs() + 3_600,
        };
        let error = validate_github_app_credential(wrong, &repository)
            .err()
            .expect("repository mismatch refuses");
        assert!(error.contains("different repository"));
        assert!(!error.contains("never-render-me"));
    }

    #[test]
    fn app_principal_cannot_change_across_token_refresh() {
        let repository =
            parse_github_remote("https://github.com/caravan-test/app-principal-refresh-test.git")
                .unwrap();
        let first = GithubAppCredential {
            token: "first-secret".to_owned(),
            app_slug: "caravan".to_owned(),
            installation_id: 42,
            expires_unix_secs: current_unix_secs() + 3_600,
        };
        bind_github_app_principal(&repository, &first).unwrap();
        bind_github_app_principal(&repository, &first).unwrap();
        let changed = GithubAppCredential {
            token: "second-secret".to_owned(),
            app_slug: "other-app".to_owned(),
            installation_id: 99,
            expires_unix_secs: current_unix_secs() + 3_600,
        };
        let error = bind_github_app_principal(&repository, &changed).unwrap_err();
        assert!(error.contains("changed the bound App installation principal"));
        assert!(!error.contains("first-secret"));
        assert!(!error.contains("second-secret"));
    }

    #[test]
    fn app_credential_near_expiry_fails_without_exposing_token() {
        let repository = GithubRepository {
            host: "github.com".to_owned(),
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
            git_transport: GithubGitTransport::Https,
        };
        let response = GithubAppCredentialResponse {
            token: "near-expiry-secret".to_owned(),
            app_slug: "caravan".to_owned(),
            installation_id: 42,
            repository: "owner/repo".to_owned(),
            expires_unix_secs: current_unix_secs() + 30,
        };
        let error = validate_github_app_credential(response, &repository)
            .err()
            .expect("near-expiry credential refuses");
        assert!(error.contains("expired or near-expiry"));
        assert!(!error.contains("near-expiry-secret"));
    }

    #[test]
    fn app_git_transport_requires_exact_https_repository() {
        let request = CommandSpec::new("git").args(["push", "origin", "HEAD:refs/heads/x"]);
        let https = parse_github_remote("https://github.com/owner/repo.git").unwrap();
        validate_github_app_git_repository(&https, &request).unwrap();

        let ssh = parse_github_remote("git@github.com:owner/repo.git").unwrap();
        let error = validate_github_app_git_repository(&ssh, &request).unwrap_err();
        assert!(error.contains("HTTPS origin"));

        let other = CommandSpec::new("git").args([
            "ls-remote",
            "https://github.com/other/repo.git",
            "refs/heads/main",
        ]);
        let error = validate_github_app_git_repository(&https, &other).unwrap_err();
        assert!(error.contains("different repository"));
        assert!(is_remote_git_request(&request));
        assert!(!is_remote_git_request(
            &CommandSpec::new("git").args(["status"])
        ));
    }

    #[test]
    fn app_git_transport_rejects_url_rewrites_before_credentials() {
        let root = std::env::temp_dir().join(format!("cara-app-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let runner = ProcessRunner::in_directory(&root).without_github_auth_inference();
        assert!(
            runner
                .run(&CommandSpec::new("git").args(["init", "--quiet"]))
                .unwrap()
                .is_success()
        );
        assert!(
            runner
                .run(&CommandSpec::new("git").args([
                    "config",
                    "url.https://attacker.invalid/.insteadOf",
                    "https://github.com/"
                ]))
                .unwrap()
                .is_success()
        );
        let error = validate_github_app_git_configuration(&runner).unwrap_err();
        let _ = std::fs::remove_dir_all(&root);
        assert!(error.contains("URL rewrites"));
    }

    #[test]
    fn command_debug_redacts_environment_and_stdin_secrets() {
        let request = CommandSpec::new("git")
            .args(["credential", "fill"])
            .env(GITHUB_APP_GIT_TOKEN_ENV, "debug-secret")
            .stdin("password=stdin-secret");
        let debug = format!("{request:?}");
        assert!(debug.contains(GITHUB_APP_GIT_TOKEN_ENV));
        assert!(!debug.contains("debug-secret"));
        assert!(!debug.contains("stdin-secret"));
    }

    #[cfg(unix)]
    fn clear_app_git_child_auth_environment(command: &mut std::process::Command) {
        for name in [
            "CARA_APP_GIT_CHILD_FIXTURE",
            "CARA_GITHUB_AUTH_MODE",
            "CARA_GITHUB_APP_CREDENTIAL_COMMAND",
            "CARA_GITHUB_APP_SLUG",
            "CARA_GITHUB_APP_INSTALLATION_ID",
            "CARA_GITHUB_APP_GIT_TOKEN",
            "CARA_GITHUB_AUTH_KIND",
            "BROKER_TOKEN",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_HOST",
            "GH_REPO",
            "GIT_ASKPASS",
            "SSH_ASKPASS",
            "GIT_TERMINAL_PROMPT",
            "GCM_INTERACTIVE",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_SSL_NO_VERIFY",
            "GIT_TRACE",
            "GIT_TRACE_CURL",
            "GIT_CURL_VERBOSE",
            "GIT_DIR",
            "GIT_WORK_TREE",
        ] {
            command.env_remove(name);
        }
        for index in 0..16 {
            command.env_remove(format!("GIT_CONFIG_KEY_{index}"));
            command.env_remove(format!("GIT_CONFIG_VALUE_{index}"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn app_git_child_scrubs_parallel_auth_environment() {
        const CHILD: &str = "CARA_APP_GIT_CHILD_FIXTURE";
        const TOKEN: &str = "broker-git-sentinel-secret";
        if std::env::var(CHILD).is_ok() {
            assert_eq!(
                std::env::var("CARA_GITHUB_AUTH_MODE").as_deref(),
                Ok("app_installation")
            );
            assert_eq!(
                std::env::var("CARA_GITHUB_APP_SLUG").as_deref(),
                Ok("caravan")
            );
            assert_eq!(
                std::env::var("CARA_GITHUB_APP_INSTALLATION_ID").as_deref(),
                Ok("4242")
            );
            assert!(std::env::var("BROKER_TOKEN").is_ok_and(|value| !value.is_empty()));
            assert!(std::env::var("GH_TOKEN").is_err());
            assert!(std::env::var("GIT_CONFIG_COUNT").is_err());
            return;
        }

        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "command::tests::app_git_child_scrubs_parallel_auth_environment",
            ])
            // Deliberately poison values before the scrub. Success proves the
            // child does not inherit either command-local or parallel-suite auth.
            .env("GH_TOKEN", "parallel-poison-secret")
            .env("GIT_CONFIG_COUNT", "999")
            .env("CARA_GITHUB_AUTH_MODE", "invalid-parallel-mode");
        clear_app_git_child_auth_environment(&mut child);
        let output = child
            .env(CHILD, "1")
            .env("BROKER_TOKEN", TOKEN)
            .env("CARA_GITHUB_AUTH_MODE", "app_installation")
            .env(
                "CARA_GITHUB_APP_CREDENTIAL_COMMAND",
                "/reviewed/test-broker",
            )
            .env("CARA_GITHUB_APP_SLUG", "caravan")
            .env("CARA_GITHUB_APP_INSTALLATION_ID", "4242")
            .output()
            .unwrap();
        let evidence = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let sanitized = evidence
            .replace(TOKEN, "<redacted-token>")
            .replace("parallel-poison-secret", "<redacted-poison>");
        assert!(
            output.status.success(),
            "child failed with sanitized evidence: {sanitized}"
        );
        assert!(!evidence.contains(TOKEN));
        assert!(!evidence.contains("parallel-poison-secret"));
    }

    #[test]
    fn app_git_helper_reads_token_only_from_environment() {
        let auth = GithubAppGitAuth {
            credential: GithubAppCredential {
                token: "helper-sentinel-secret".to_owned(),
                app_slug: "caravan".to_owned(),
                installation_id: 42,
                expires_unix_secs: current_unix_secs() + 3_600,
            },
            repository: parse_github_remote("https://github.com/owner/repo.git").unwrap(),
        };
        let mut request = CommandSpec::new("git")
            .args(["credential", "fill"])
            .stdin("protocol=https\nhost=github.com\npath=owner/repo.git\n\n");
        for (name, value) in github_app_git_environment(&auth) {
            request = request.env(name, value);
        }
        assert!(!request.display().contains("helper-sentinel-secret"));
        assert!(!format!("{request:?}").contains("helper-sentinel-secret"));
        let runner = ProcessRunner::new().without_github_auth_inference();
        let output = runner.run(&request).expect("credential helper executes");
        assert!(output.is_success());
        assert!(output.stdout.contains("username=x-access-token"));
        assert!(output.stdout.contains("password=helper-sentinel-secret"));
        runner.record_github_app_git_transport(Some(&auth));
        let telemetry = runner.github_api_telemetry();
        assert_eq!(
            telemetry.github_app_git_transport.as_deref(),
            Some("https_credential_helper")
        );
        assert_eq!(
            telemetry.github_app_git_repository.as_deref(),
            Some("owner/repo")
        );
        assert!(
            !serde_json::to_string(&telemetry)
                .unwrap()
                .contains("helper-sentinel-secret")
        );
    }

    #[test]
    fn credential_bearing_remote_urls_are_rejected_and_redacted() {
        let request = CommandSpec::new("git").args([
            "clone",
            "https://user:url-sentinel-secret@github.com/owner/repo.git",
        ]);
        let error = explicit_github_repository(&request).unwrap_err();
        assert!(error.contains("credential-bearing remote URLs"));
        assert!(!request.display().contains("url-sentinel-secret"));
        assert!(!format!("{request:?}").contains("url-sentinel-secret"));
        assert!(request.display().contains("<redacted>@github.com"));
    }

    #[test]
    fn app_telemetry_names_installation_but_never_token() {
        let runner = ProcessRunner::new();
        let request = CommandSpec::new("gh").args(["api", "repos/owner/repo"]);
        let selection = GithubAuthSelection::AppInstallation(GithubAppCredential {
            token: "telemetry-secret".to_owned(),
            app_slug: "caravan".to_owned(),
            installation_id: 42,
            expires_unix_secs: current_unix_secs() + 3_600,
        });
        runner.record_github_request(&request, Some(&selection));
        let telemetry = runner.github_api_telemetry();
        assert_eq!(
            telemetry.auth_source.as_deref(),
            Some("github_app_installation")
        );
        assert_eq!(telemetry.github_app_slug.as_deref(), Some("caravan"));
        assert_eq!(telemetry.github_app_installation_id, Some(42));
        let encoded = serde_json::to_string(&telemetry).unwrap();
        assert!(!encoded.contains("telemetry-secret"));
    }

    #[test]
    fn app_401_refreshes_once_and_never_for_ambient_tokens() {
        let app = GithubAuthSelection::AppInstallation(GithubAppCredential {
            token: "refresh-secret".to_owned(),
            app_slug: "caravan".to_owned(),
            installation_id: 42,
            expires_unix_secs: current_unix_secs() + 3_600,
        });
        let unauthorized = CommandOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: "HTTP 401: Bad credentials".to_owned(),
        };
        assert!(should_refresh_github_app_auth(
            Some(&app),
            &unauthorized,
            true
        ));
        assert!(!should_refresh_github_app_auth(
            Some(&app),
            &unauthorized,
            false
        ));
        assert!(!should_refresh_github_app_auth(
            Some(&GithubAuthSelection::Token("ambient-secret".to_owned())),
            &unauthorized,
            true
        ));
    }

    #[test]
    fn app_git_auth_failure_refreshes_once() {
        let auth = GithubAppGitAuth {
            credential: GithubAppCredential {
                token: "refresh-git-secret".to_owned(),
                app_slug: "caravan".to_owned(),
                installation_id: 42,
                expires_unix_secs: current_unix_secs() + 3_600,
            },
            repository: parse_github_remote("https://github.com/owner/repo.git").unwrap(),
        };
        let failed = CommandOutput::failure(
            128,
            "fatal: Authentication failed for 'https://github.com/owner/repo.git'",
        );
        assert!(should_refresh_github_app_git_auth(
            Some(&auth),
            &failed,
            true
        ));
        assert!(!should_refresh_github_app_git_auth(
            Some(&auth),
            &failed,
            false
        ));
        assert!(!should_refresh_github_app_git_auth(None, &failed, true));
    }

    #[test]
    fn refused_app_auth_is_never_reported_as_authenticated() {
        let runner = ProcessRunner::new();
        let request = CommandSpec::new("gh").args(["api", "repos/owner/repo"]);
        runner.record_github_request(
            &request,
            Some(&GithubAuthSelection::Refused("broker failed".to_owned())),
        );
        let telemetry = runner.github_api_telemetry();
        assert!(!telemetry.authenticated);
        assert_eq!(telemetry.auth_source.as_deref(), Some("github_app_refused"));
    }

    #[test]
    fn repository_owner_is_fast_auth_candidate_before_active_fallbacks() {
        let repository = GithubRepository {
            host: "github.com".to_owned(),
            owner: "harryaskham".to_owned(),
            name: "private-repo".to_owned(),
            git_transport: GithubGitTransport::Https,
        };
        let status = serde_json::json!({
            "hosts": {"github.com": [
                {"state":"success", "active":true, "login":"other-account"},
                {"state":"success", "active":false, "login":"harryaskham"},
                {"state":"success", "active":false, "login":"third-account"}
            ]}
        });

        assert_eq!(
            github_auth_candidates(&repository, Some(&status.to_string())),
            ["harryaskham", "other-account", "third-account"]
        );
    }

    #[test]
    fn command_specific_token_disables_inference_and_is_hidden_from_display() {
        let request = CommandSpec::new("gh")
            .args(["api", "repos/owner/private"])
            .env("GH_TOKEN", "secret-value");
        assert!(
            ProcessRunner::in_directory(".")
                .inferred_github_auth(&request)
                .is_none()
        );
        assert!(!request.display().contains("secret-value"));
    }

    #[test]
    fn github_telemetry_counts_calls_and_extracts_graphql_budget() {
        let runner = ProcessRunner::new();
        let request = CommandSpec::new("gh").args(["api", "graphql"]);
        runner.record_github_request(&request, Some(&GithubAuthSelection::Ambient));
        runner.record_github_response(
            &request,
            &CommandOutput {
                code: Some(0),
                stdout: r#"{"data":{"rateLimit":{"cost":17,"remaining":4983,"resetAt":"2026-07-20T20:00:00Z"}}}"#.to_owned(),
                stderr: String::new(),
            },
        );
        let telemetry = runner.github_api_telemetry();
        assert!(telemetry.authenticated);
        assert_eq!(telemetry.auth_source.as_deref(), Some("ambient_token"));
        assert_eq!(telemetry.calls, 1);
        assert_eq!(telemetry.graphql_calls, 1);
        assert_eq!(telemetry.rest_calls, 0);
        assert_eq!(telemetry.gh_cli_calls, 0);
        let rate = telemetry.rate_limit.expect("rate telemetry");
        assert_eq!(rate.cost, 17);
        assert_eq!(rate.remaining, 4_983);
    }

    #[test]
    fn explicit_environment_and_stdin_reach_the_child() {
        let output = ProcessRunner::new()
            .run(
                &CommandSpec::new("sh")
                    .args(["-c", "printf '%s:' \"$CARA_EVENT\"; cat"])
                    .env("CARA_EVENT", "sync_failed")
                    .stdin("payload"),
            )
            .expect("child succeeds");

        assert_eq!(output.stdout, "sync_failed:payload");
    }

    #[test]
    fn large_stdout_remains_complete_and_separate_from_control_stderr() {
        let output = ProcessRunner::new()
            .run(&CommandSpec::new("sh").args([
                "-c",
                "printf '\"'; i=0; while [ $i -lt 20000 ]; do printf abcdefgh; i=$((i+1)); done; printf '\"'; printf 'wrapper diagnostic\\001' >&2",
            ]))
            .expect("large child output succeeds");

        assert!(output.is_success());
        assert!(output.stdout.len() > 64 * 1024);
        let decoded: String = serde_json::from_str(&output.stdout).expect("complete JSON stdout");
        assert_eq!(decoded.len(), 160_000);
        assert!(output.stderr.contains("wrapper diagnostic"));
        assert!(output.stderr.contains('\u{1}'));
        assert!(!output.stdout.contains("wrapper diagnostic"));
    }

    #[test]
    fn output_limit_is_typed_with_independent_prefix_suffix_evidence() {
        let error = ProcessRunner::new()
            .with_capture_limits(32, 24)
            .run(&CommandSpec::new("sh").args([
                "-c",
                "printf 'stdout-BEGIN-'; printf '%080d' 0; printf -- '-stdout-END'; printf 'stderr-BEGIN-' >&2; printf '%060d' 0 >&2; printf -- '-stderr-END' >&2",
            ]))
            .expect_err("both streams exceed independent bounds");
        let CommandRunError::OutputLimit {
            code,
            stdout,
            stderr,
            ..
        } = error
        else {
            panic!("expected output limit");
        };
        assert_eq!(code, Some(0));
        assert!(stdout.truncated);
        assert!(stderr.truncated);
        assert_eq!(stdout.limit_bytes, 32);
        assert_eq!(stderr.limit_bytes, 24);
        assert!(stdout.total_bytes > 32);
        assert!(stderr.total_bytes > 24);
        assert!(stdout.prefix.starts_with("stdout-BEGIN-"));
        assert!(stdout.suffix.ends_with("-stdout-END"));
        assert!(stderr.prefix.starts_with("stderr-BEGIN-"));
        assert!(stderr.suffix.ends_with("-stderr-END"));
        assert!(!stdout.prefix.contains("stderr"));
    }

    #[test]
    fn stderr_limit_does_not_truncate_or_reclassify_stdout() {
        let error = ProcessRunner::new()
            .with_capture_limits(64, 8)
            .run(&CommandSpec::new("sh").args([
                "-c",
                "printf valid-json; printf 'diagnostic-that-is-too-long' >&2",
            ]))
            .expect_err("stderr alone exceeds its independent bound");
        let CommandRunError::OutputLimit { stdout, stderr, .. } = error else {
            panic!("expected output limit");
        };
        assert!(!stdout.truncated);
        assert_eq!(stdout.prefix, "valid-json");
        assert!(stderr.truncated);
        assert_eq!(stderr.limit_bytes, 8);
    }

    #[cfg(unix)]
    #[test]
    fn shared_github_request_budget_is_exact_across_cloned_runners() {
        let directory = tempfile::tempdir().unwrap();
        let gh = directory.path().join("gh");
        let temp_directory = std::env::temp_dir();
        let true_binary = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join("true"))
            .find(|candidate| !candidate.starts_with(&temp_directory) && candidate.is_file())
            .expect("true is available outside the shared test temporary directory");
        std::os::unix::fs::symlink(true_binary, &gh).unwrap();
        let budget = GithubRequestBudget::new(1);
        let first = ProcessRunner::new().with_github_request_budget(budget.clone());
        let second = ProcessRunner::new().with_github_request_budget(budget.clone());

        first
            .run(&CommandSpec::new(gh.display().to_string()))
            .expect("first gh command is inside the bound");
        let error = second
            .run(&CommandSpec::new(gh.display().to_string()))
            .expect_err("second gh command must fail before spawning");

        assert!(matches!(
            error,
            CommandRunError::GithubRequestBudgetExceeded {
                limit: 1,
                used: 1,
                ..
            }
        ));
        assert_eq!(budget.used(), 1);
    }

    #[test]
    fn long_operation_budget_still_caps_each_child_at_normal_timeout() {
        let request = CommandSpec::new("never-executed");
        let runner = ProcessRunner::new()
            .with_timeout(Duration::from_secs(30))
            .with_operation_deadline(Instant::now() + Duration::from_secs(150));

        let effective = runner
            .effective_timeout(&request)
            .expect("long operation has remaining budget");
        assert!(effective <= Duration::from_secs(30));
        assert!(effective >= Duration::from_secs(29));
    }

    /// Live shape (cacophony, v0.0.21): an hour-long tick left 285ms, and a
    /// `git fetch` was launched against it. The refusal must name the exhausted
    /// deadline, not the command, or a reader debugs git instead of the budget.
    #[test]
    fn a_sliver_of_budget_refuses_naming_the_deadline_not_the_command() {
        let runner = ProcessRunner::in_directory(Path::new("."))
            .with_timeout(Duration::from_secs(60))
            .with_operation_deadline(Instant::now() + Duration::from_millis(285));

        let error = runner
            .run(&CommandSpec::new("git").args(["fetch", "--quiet", "origin"]))
            .expect_err("a 285ms budget cannot run a fetch");

        let rendered = format!("{error:?}");
        assert!(
            rendered.contains("operation deadline exhausted"),
            "must name the deadline: {rendered}"
        );
        assert!(
            rendered.contains("max_duration_secs"),
            "must name the actionable knob: {rendered}"
        );
    }

    #[test]
    fn one_absolute_deadline_bounds_multiple_phases_and_reaps_the_hung_phase() {
        let operation_started = Instant::now();
        // The assertion is about sharing one absolute budget and keeping it
        // authoritative over the per-child timeout. Keep the operation budget
        // large enough that ordinary process scheduling under a fully parallel
        // Nix build cannot masquerade as a deadline violation.
        let operation_budget = Duration::from_secs(10);
        let child_timeout = Duration::from_secs(120);
        let runner = ProcessRunner::new()
            .with_timeout(child_timeout)
            .with_operation_deadline(operation_started + operation_budget);
        runner
            .run(&CommandSpec::new("sh").args(["-c", "exit 0"]))
            .expect("first phase fits the budget");
        let first_phase_elapsed = operation_started.elapsed();
        let error = runner
            .run(&CommandSpec::new("sh").args(["-c", "printf phase-two; sleep 300"]))
            .expect_err("hung second phase must consume only the remaining budget");

        assert!(operation_started.elapsed() < operation_budget + Duration::from_secs(15));
        let CommandRunError::Timeout {
            timeout_ms, stdout, ..
        } = error
        else {
            panic!("expected deadline timeout");
        };
        // The hung phase inherits only what the shared operation deadline has
        // left, never the much larger per-child timeout.
        assert!(
            timeout_ms <= duration_millis(operation_budget),
            "remaining budget was {timeout_ms}ms"
        );
        assert!(
            timeout_ms < duration_millis(child_timeout),
            "operation deadline must stay authoritative, got {timeout_ms}ms"
        );
        assert!(
            timeout_ms
                <= duration_millis(operation_budget.saturating_sub(first_phase_elapsed)) + 50,
            "second phase must not regain the first phase's spent budget: {timeout_ms}ms"
        );
        assert!(stdout.is_empty() || stdout == "phase-two");
    }

    #[cfg(unix)]
    #[test]
    fn child_that_closes_stdin_preserves_its_exit_status_and_stderr() {
        let output = ProcessRunner::new()
            .run(
                &CommandSpec::new("sh")
                    .args(["-c", "exec 0<&-; printf diagnostic >&2; exit 17"])
                    .stdin("x".repeat(1024 * 1024)),
            )
            .unwrap_or_else(|error| {
                panic!("the child's own result wins over a closed stdin pipe: {error}")
            });

        assert_eq!(output.code, Some(17));
        assert_eq!(output.stderr, "diagnostic");
    }

    #[cfg(unix)]
    #[test]
    fn parallel_timeouts_cannot_erase_fast_child_status_or_output() {
        const WORKERS: usize = 8;
        const ITERATIONS: usize = 32;
        let payload = "x".repeat(1024 * 1024);

        thread::scope(|scope| {
            for worker in 0..WORKERS {
                let payload = &payload;
                scope.spawn(move || {
                    for iteration in 0..ITERATIONS {
                        if iteration % 8 == 0 {
                            let error = ProcessRunner::new()
                                .with_timeout(Duration::from_millis(200))
                                .run(&CommandSpec::new("sh").args(["-c", "sleep 300"]))
                                .expect_err("the hanging sibling must time out");
                            assert!(
                                matches!(error, CommandRunError::Timeout { .. }),
                                "worker {worker} iteration {iteration} returned {error}"
                            );
                            continue;
                        }

                        let output = ProcessRunner::new()
                            .run(
                                &CommandSpec::new("sh")
                                    .args([
                                        "-c",
                                        "exec 0<&-; printf parallel-diagnostic >&2; exit 17",
                                    ])
                                    .stdin(payload.clone()),
                            )
                            .unwrap_or_else(|error| {
                                panic!(
                                    "worker {worker} iteration {iteration} lost the child result: {error}"
                                )
                            });
                        assert_eq!(output.code, Some(17));
                        assert_eq!(output.stderr, "parallel-diagnostic");
                    }
                });
            }
        });
    }

    #[test]
    fn hanging_child_is_terminated_reaped_and_reported_with_evidence() {
        let started = Instant::now();
        // Leave enough startup budget for the child to be scheduled even when
        // the full Nix suite is running in parallel. The assertion below is
        // about preserving bytes emitted before termination, not about proving
        // that a fresh shell can always start within 100 ms on a loaded host.
        let timeout = Duration::from_secs(5);
        let error = ProcessRunner::new()
            .with_timeout(timeout)
            .run(
                &CommandSpec::new("sh")
                    .args(["-c", "printf started; printf diagnostic >&2; sleep 300"]),
            )
            .expect_err("hanging child must time out");

        assert!(started.elapsed() < Duration::from_secs(30));
        let CommandRunError::Timeout {
            command,
            process_group_id,
            timeout_ms,
            stdout,
            stderr,
        } = error
        else {
            panic!("expected timeout error");
        };
        assert_eq!(command.program, "sh");
        assert!(process_group_id.is_some());
        assert_eq!(timeout_ms, duration_millis(timeout));
        assert_eq!(stdout, "started");
        assert!(
            stderr.starts_with("diagnostic"),
            "the child's diagnostic must be preserved before any platform shell termination text: {stderr:?}"
        );
    }
}

#[cfg(test)]
mod repository_resolution_diagnostics_tests {
    use super::*;

    fn git(directory: &Path, args: &[&str]) {
        let runner = ProcessRunner::in_directory(directory).without_github_auth_inference();
        runner
            .run(&CommandSpec::new("git").args(args.iter().copied()))
            .expect("git command runs");
    }

    /// bd-ce545f: a managed checkout points at a local daemon mirror, so Cara
    /// never reaches an auth verdict at all. Reporting that as
    /// `gh_default_or_unauthenticated` sent readers to re-run `gh auth login`,
    /// which mutates credential state for a problem that is not about
    /// credentials.
    #[test]
    fn a_non_github_origin_is_reported_as_repository_resolution_not_auth() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        git(path, &["init", "-q", "."]);
        git(
            path,
            &["remote", "add", "origin", "/tmp/local-daemon-mirror.git"],
        );

        let selection = resolve_github_auth(Some(path), None);
        assert!(
            selection.is_none(),
            "a non-GitHub origin cannot name a repository to probe"
        );

        let runner = ProcessRunner::in_directory(path);
        assert!(
            runner.origin_is_not_github(),
            "the probe must record why it could not run"
        );
    }

    #[test]
    fn a_github_origin_is_never_blamed_on_repository_resolution() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        git(path, &["init", "-q", "."]);
        git(
            path,
            &[
                "remote",
                "add",
                "origin",
                "ssh://git@github.com/harryaskham/caravan.git",
            ],
        );

        let _ = resolve_github_auth(Some(path), None);

        let runner = ProcessRunner::in_directory(path);
        assert!(
            !runner.origin_is_not_github(),
            "a real GitHub remote keeps ordinary auth reporting"
        );
    }
}
