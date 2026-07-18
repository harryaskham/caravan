//! Small subprocess seam used by Git and GitHub adapters.
//!
//! Keeping command execution behind [`CommandRunner`] makes discovery tests
//! hermetic while production still uses the installed, authenticated `git` and
//! `gh` binaries.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

/// Failure to start a subprocess or decode its output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandRunError {
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
    /// The child exceeded its hard deadline and was terminated and reaped.
    Timeout {
        /// Requested command.
        command: CommandSpec,
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
}

/// Production subprocess runner.
#[derive(Debug, Clone)]
pub struct ProcessRunner {
    cwd: Option<PathBuf>,
    timeout: Duration,
    operation_deadline: Option<Instant>,
}

impl Default for ProcessRunner {
    fn default() -> Self {
        Self {
            cwd: None,
            timeout: DEFAULT_COMMAND_TIMEOUT,
            operation_deadline: None,
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
        }
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

    fn effective_timeout(&self, request: &CommandSpec) -> Result<Duration, CommandRunError> {
        let timeout = self.operation_deadline.map_or(self.timeout, |deadline| {
            self.timeout
                .min(deadline.saturating_duration_since(Instant::now()))
        });
        if timeout.is_zero() {
            return Err(CommandRunError::Timeout {
                command: request.clone(),
                timeout_ms: 0,
                stdout: String::new(),
                stderr: "operation deadline exhausted before this phase".to_owned(),
            });
        }
        Ok(timeout)
    }
}

impl CommandRunner for ProcessRunner {
    #[allow(clippy::too_many_lines)]
    fn run(&self, request: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
        let timeout = self.effective_timeout(request)?;
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
        let stdout_reader = thread::spawn(move || capture(stdout, MAX_STDOUT_CAPTURE_BYTES));
        let stderr_reader = thread::spawn(move || capture(stderr, MAX_STDERR_CAPTURE_BYTES));
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
                timeout_ms: duration_millis(timeout),
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
            });
        }
        let stdout = String::from_utf8(stdout).map_err(|error| CommandRunError::InvalidUtf8 {
            command: request.clone(),
            stream: "stdout",
            message: error.to_string(),
        })?;
        let stderr = String::from_utf8(stderr).map_err(|error| CommandRunError::InvalidUtf8 {
            command: request.clone(),
            stream: "stderr",
            message: error.to_string(),
        })?;

        Ok(CommandOutput {
            code: status.code(),
            stdout,
            stderr,
        })
    }
}

fn capture(mut stream: impl Read, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(captured.len());
        let retained = remaining.min(count);
        captured.extend_from_slice(&buffer[..retained]);
        truncated |= retained < count;
    }
    if truncated {
        captured.extend_from_slice(b"\n...[truncated]");
    }
    Ok(captured)
}

fn join_capture(
    handle: thread::JoinHandle<std::io::Result<Vec<u8>>>,
    request: &CommandSpec,
    stream: &'static str,
) -> Result<Vec<u8>, CommandRunError> {
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
            timeout_ms,
            stdout,
            stderr,
        } = error
        else {
            panic!("expected timeout error");
        };
        assert_eq!(command.program, "sh");
        assert_eq!(timeout_ms, duration_millis(timeout));
        assert_eq!(stdout, "started");
        assert!(
            stderr.starts_with("diagnostic"),
            "the child's diagnostic must be preserved before any platform shell termination text: {stderr:?}"
        );
    }
}
