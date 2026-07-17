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
const MAX_CAPTURE_BYTES: usize = 64 * 1024;

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
}

impl Default for ProcessRunner {
    fn default() -> Self {
        Self {
            cwd: None,
            timeout: DEFAULT_COMMAND_TIMEOUT,
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
        }
    }

    /// Override the hard deadline for every command run by this instance.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.max(Duration::from_millis(1));
        self
    }
}

impl CommandRunner for ProcessRunner {
    fn run(&self, request: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
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
        let stdout_reader = thread::spawn(move || capture(stdout));
        let stderr_reader = thread::spawn(move || capture(stderr));
        let deadline = Instant::now() + self.timeout;

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
            writer
                .join()
                .map_err(|_| CommandRunError::Spawn {
                    command: request.clone(),
                    message: "stdin writer thread panicked".to_owned(),
                })?
                .map_err(|error| CommandRunError::Spawn {
                    command: request.clone(),
                    message: format!("could not write stdin: {error}"),
                })?;
        }
        let stdout = join_capture(stdout_reader, request, "stdout")?;
        let stderr = join_capture(stderr_reader, request, "stderr")?;
        if timed_out {
            return Err(CommandRunError::Timeout {
                command: request.clone(),
                timeout_ms: duration_millis(self.timeout),
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

fn capture(mut stream: impl Read) -> std::io::Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = MAX_CAPTURE_BYTES.saturating_sub(captured.len());
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
    fn hanging_child_is_terminated_reaped_and_reported_with_evidence() {
        let started = Instant::now();
        let error = ProcessRunner::new()
            .with_timeout(Duration::from_millis(100))
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
        assert_eq!(timeout_ms, 100);
        assert_eq!(stdout, "started");
        assert!(
            stderr.starts_with("diagnostic"),
            "the child's diagnostic must be preserved before any platform shell termination text: {stderr:?}"
        );
    }
}
