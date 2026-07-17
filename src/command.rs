//! Small subprocess seam used by Git and GitHub adapters.
//!
//! Keeping command execution behind [`CommandRunner`] makes discovery tests
//! hermetic while production still uses the installed, authenticated `git` and
//! `gh` binaries.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A subprocess request with arguments kept separate from shell parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Executable name or path.
    pub program: String,
    /// Exact argument vector.
    pub args: Vec<String>,
}

impl CommandSpec {
    /// Start a command request.
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
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
#[derive(Debug, Clone, Default)]
pub struct ProcessRunner {
    cwd: Option<PathBuf>,
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
        }
    }
}

impl CommandRunner for ProcessRunner {
    fn run(&self, request: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
        let mut command = Command::new(&request.program);
        command.args(&request.args);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }

        let output = command.output().map_err(|error| CommandRunError::Spawn {
            command: request.clone(),
            message: error.to_string(),
        })?;
        let stdout =
            String::from_utf8(output.stdout).map_err(|error| CommandRunError::InvalidUtf8 {
                command: request.clone(),
                stream: "stdout",
                message: error.to_string(),
            })?;
        let stderr =
            String::from_utf8(output.stderr).map_err(|error| CommandRunError::InvalidUtf8 {
                command: request.clone(),
                stream: "stderr",
                message: error.to_string(),
            })?;

        Ok(CommandOutput {
            code: output.status.code(),
            stdout,
            stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_rendering_quotes_shell_metacharacters_without_using_a_shell() {
        let command = CommandSpec::new("gh").args(["pr", "list", "--label", "caravan queue"]);
        assert_eq!(command.display(), "gh pr list --label 'caravan queue'");
    }
}
