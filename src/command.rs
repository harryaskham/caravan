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

static GITHUB_AUTH_CACHE: OnceLock<Mutex<HashMap<String, Option<GithubAuthSelection>>>> =
    OnceLock::new();

#[derive(Clone)]
enum GithubAuthSelection {
    Ambient,
    Token(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GithubRepository {
    host: String,
    owner: String,
    name: String,
}

impl GithubRepository {
    fn cache_key(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.name)
    }

    fn api_path(&self) -> String {
        format!("repos/{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CommandIo {
    env: BTreeMap<String, String>,
    stdin: Option<String>,
}

/// A subprocess request with arguments kept separate from shell parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Executable name or path.
    pub program: String,
    /// Exact argument vector.
    pub args: Vec<String>,
    /// Optional I/O additions stay boxed so ordinary command/error values remain small.
    io: Option<Box<CommandIo>>,
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
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(quote_for_diagnostic)
            .collect::<Vec<_>>()
            .join(" ")
    }
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
        resolve_github_auth(self.cwd.as_deref(), self.operation_deadline)
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
        telemetry.authenticated = explicit || auth.is_some();
        telemetry.auth_source = Some(if explicit {
            "explicit_command_token".to_owned()
        } else {
            match auth {
                Some(GithubAuthSelection::Ambient) => std::env::var("CARA_GITHUB_AUTH_KIND")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "ambient_token".to_owned()),
                Some(GithubAuthSelection::Token(_)) => "gh_auth_account".to_owned(),
                None => "gh_default_or_unauthenticated".to_owned(),
            }
        });
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
        let timeout = self.operation_deadline.map_or(self.timeout, |deadline| {
            self.timeout
                .min(deadline.saturating_duration_since(Instant::now()))
        });
        if timeout.is_zero() {
            return Err(CommandRunError::Timeout {
                command: request.clone(),
                process_group_id: None,
                timeout_ms: 0,
                stdout: String::new(),
                stderr: "operation deadline exhausted before this phase".to_owned(),
            });
        }
        Ok(timeout)
    }
}

fn resolve_github_auth(
    cwd: Option<&Path>,
    operation_deadline: Option<Instant>,
) -> Option<GithubAuthSelection> {
    let cwd = cwd?;
    let runner = ProcessRunner::in_directory(cwd)
        .with_timeout(GITHUB_AUTH_PROBE_TIMEOUT)
        .without_github_auth_inference();
    let runner = operation_deadline.map_or(runner.clone(), |deadline| {
        runner.with_operation_deadline(deadline)
    });
    let remote = runner
        .run(&CommandSpec::new("git").args(["config", "--get", "remote.origin.url"]))
        .ok()
        .filter(CommandOutput::is_success)?;
    let repository = parse_github_remote(remote.stdout.trim())?;
    let key = repository.cache_key();
    let cache = GITHUB_AUTH_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(selection) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .cloned()
    {
        return selection;
    }
    let selection = resolve_github_auth_uncached(&runner, &repository);
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, selection.clone());
    selection
}

fn resolve_github_auth_uncached(
    runner: &ProcessRunner,
    repository: &GithubRepository,
) -> Option<GithubAuthSelection> {
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
    let (host, path) = if let Some(rest) = remote.strip_prefix("ssh://") {
        let (authority, path) = rest.split_once('/')?;
        (authority.rsplit('@').next()?, path)
    } else if let Some(rest) = remote
        .strip_prefix("https://")
        .or_else(|| remote.strip_prefix("http://"))
    {
        rest.split_once('/')?
    } else {
        let (authority, path) = remote.split_once(':')?;
        (authority.rsplit('@').next()?, path)
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
    })
}

fn is_gh_request(request: &CommandSpec) -> bool {
    Path::new(&request.program)
        .file_name()
        .is_some_and(|name| name == "gh")
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
        self.record_github_request(request, github_auth.as_ref());
        let mut command = Command::new(&request.program);
        command
            .args(&request.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(GithubAuthSelection::Token(token)) = &github_auth {
            command.env("GH_TOKEN", token);
        }
        if let Some(io) = &request.io {
            command.envs(&io.env);
            if io.stdin.is_some() {
                command.stdin(Stdio::piped());
            }
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
        let expected = GithubRepository {
            host: "github.com".to_owned(),
            owner: "harryaskham".to_owned(),
            name: "caravan".to_owned(),
        };
        assert_eq!(
            parse_github_remote("ssh://git@github.com/harryaskham/caravan.git"),
            Some(expected.clone())
        );
        assert_eq!(
            parse_github_remote("git@github.com:harryaskham/caravan.git"),
            Some(expected.clone())
        );
        assert_eq!(
            parse_github_remote("https://github.com/harryaskham/caravan.git"),
            Some(expected)
        );
        assert_eq!(parse_github_remote("/tmp/local.git"), None);
    }

    #[test]
    fn repository_owner_is_fast_auth_candidate_before_active_fallbacks() {
        let repository = GithubRepository {
            host: "github.com".to_owned(),
            owner: "harryaskham".to_owned(),
            name: "private-repo".to_owned(),
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
        let true_binary = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join("true"))
            .find(|candidate| candidate.is_file())
            .expect("true is available on the hermetic test PATH");
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

    #[test]
    fn one_absolute_deadline_bounds_multiple_phases_and_reaps_the_hung_phase() {
        let operation_started = Instant::now();
        // The assertion is about sharing one absolute budget, not requiring a
        // loaded host to schedule the first shell within one second. Keep the
        // child timeout larger so the operation deadline remains authoritative.
        let operation_budget = Duration::from_secs(3);
        let runner = ProcessRunner::new()
            .with_timeout(Duration::from_secs(5))
            .with_operation_deadline(operation_started + operation_budget);
        runner
            .run(&CommandSpec::new("sh").args(["-c", "sleep 0.05"]))
            .expect("first phase fits the budget");
        let error = runner
            .run(&CommandSpec::new("sh").args(["-c", "printf phase-two; sleep 30"]))
            .expect_err("hung second phase must consume only the remaining budget");

        assert!(operation_started.elapsed() < Duration::from_secs(4));
        let CommandRunError::Timeout {
            timeout_ms, stdout, ..
        } = error
        else {
            panic!("expected deadline timeout");
        };
        assert!(
            timeout_ms <= duration_millis(operation_budget).saturating_sub(50),
            "remaining budget was {timeout_ms}ms"
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
                                .with_timeout(Duration::from_millis(20))
                                .run(&CommandSpec::new("sh").args(["-c", "sleep 30"]))
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
        let timeout = Duration::from_secs(1);
        let error = ProcessRunner::new()
            .with_timeout(timeout)
            .run(
                &CommandSpec::new("sh")
                    .args(["-c", "printf started; printf diagnostic >&2; sleep 30"]),
            )
            .expect_err("hanging child must time out");

        assert!(started.elapsed() < Duration::from_secs(5));
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
