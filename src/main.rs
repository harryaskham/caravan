//! `cara` command-line entry point.

use std::io;
use std::path::PathBuf;

use caravan::{
    AGENT_HELP, AppContext, AppError, CheckInput, CreateInput, EmptyInput, EvictInput, JoinInput,
    LoopInput, OperationOutput, SplitInput, SyncInput, TOOL_NAME, build_router, feedback_config,
    feedback_destination, scaffold_operation, updater_config,
};
use clap::{Args, Parser, Subcommand, ValueEnum};
use feedback_cli::{FeedbackEvent, FeedbackKind, Reporter, Severity};
use mcp_cli::{McpServer, StdioServerConfig, StructuredError, write_json_result};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    name = "cara",
    version,
    about = "Agent-in-the-loop GitHub merge queue",
    long_about = "Caravan maintains GitHub PRs as labelled, mechanically compatible chains. It is non-interactive: sync either converges or returns one structured decision point for a user or MCP-connected agent.",
    arg_required_else_help = true,
    disable_help_subcommand = true
)]
struct Cli {
    /// Emit a stable machine-readable mcp-cli envelope.
    #[arg(long, global = true)]
    json: bool,

    /// Override the repository-local .caravan/config.yaml path.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Discover the current PR, all caravans, invalid fragments, and decisions.
    Status,
    /// Validate current or proposed caravan state without mutation.
    Check(CheckInput),
    /// Create a one-PR caravan from the current branch.
    New(CreateInput),
    /// Reevaluate an evicted PR as a new caravan.
    Renew(CreateInput),
    /// Append the current PR after a caravan tail.
    Join(JoinInput),
    /// Reevaluate and append an evicted PR after a caravan tail.
    Rejoin(JoinInput),
    /// Show the current branch's whole caravan and position.
    Show,
    /// Check out the next PR toward the current caravan tail.
    Next,
    /// Check out the previous PR toward the current caravan head.
    Prev,
    /// Idempotently synchronize one or all caravans until a decision point.
    Sync(SyncInput),
    /// Evict a PR and safely reconnect its child when compatible.
    Evict(EvictInput),
    /// Split before a PR, making it a new caravan head.
    Split(SplitInput),
    /// Repeatedly run lightweight sync-all ticks in the foreground.
    Loop(LoopInput),
    /// Fleet-level caravan browsing operations.
    #[command(subcommand)]
    Van(VanCommand),
    /// Print agent-oriented operating and recovery instructions.
    Help,
    /// Model Context Protocol surfaces.
    #[command(subcommand)]
    Mcp(McpCommand),
    /// Inspect, check, or apply a GitHub release update.
    #[command(subcommand)]
    SelfUpdate(SelfUpdateCommand),
    /// Inspect or emit structured feedback through feedback-cli.
    #[command(subcommand)]
    Feedback(FeedbackCommand),
}

#[derive(Debug, Subcommand)]
enum VanCommand {
    /// List caravan heads in deterministic fleet navigation order.
    List,
    /// Check out the next caravan head.
    Next,
    /// Check out the previous caravan head.
    Prev,
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Serve typed Caravan tools over MCP stdio.
    Stdio,
    /// Print MCP tool names, descriptions, and schemas as JSON.
    Tools,
}

#[derive(Debug, Subcommand)]
enum SelfUpdateCommand {
    /// Report installed and staged binary paths without network access.
    Status,
    /// Check GitHub releases for a newer version.
    Check,
    /// Download, verify, stage, and promote the latest release.
    Run,
}

#[derive(Debug, Subcommand)]
enum FeedbackCommand {
    /// Show the configured feedback sink without sending an event.
    Status,
    /// Send one structured feedback event.
    Report(FeedbackArgs),
}

#[derive(Debug, Args)]
struct FeedbackArgs {
    /// Event kind.
    #[arg(long, value_enum, default_value_t = FeedbackKindArg::Info)]
    kind: FeedbackKindArg,
    /// Component; defaults to cara.
    #[arg(long)]
    component: Option<String>,
    /// Short event summary.
    #[arg(long)]
    summary: String,
    /// Optional detail or evidence.
    #[arg(long)]
    detail: Option<String>,
    /// Optional severity.
    #[arg(long, value_enum)]
    severity: Option<SeverityArg>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum FeedbackKindArg {
    Error,
    Exception,
    Perf,
    Info,
}

impl From<FeedbackKindArg> for FeedbackKind {
    fn from(value: FeedbackKindArg) -> Self {
        match value {
            FeedbackKindArg::Error => Self::Error,
            FeedbackKindArg::Exception => Self::Exception,
            FeedbackKindArg::Perf => Self::Perf,
            FeedbackKindArg::Info => Self::Info,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SeverityArg {
    Info,
    Warning,
    Error,
    Critical,
}

impl From<SeverityArg> for Severity {
    fn from(value: SeverityArg) -> Self {
        match value {
            SeverityArg::Info => Self::Info,
            SeverityArg::Warning => Self::Warning,
            SeverityArg::Error => Self::Error,
            SeverityArg::Critical => Self::Critical,
        }
    }
}

fn main() {
    let feedback = feedback_config();
    feedback_cli::install_panic_hook(&feedback);
    if let Err(error) = updatable_cli::maybe_apply_staged_update(TOOL_NAME) {
        eprintln!("warning: staged-update check failed: {error}");
    }

    let cli = Cli::parse();
    if let Err(code) = run(&cli) {
        std::process::exit(code);
    }
}

fn run(cli: &Cli) -> Result<(), i32> {
    match &cli.command {
        Command::Status => run_domain(cli.json, "status", &EmptyInput::default()),
        Command::Check(input) => run_domain(cli.json, "check", input),
        Command::New(input) => run_domain(cli.json, "new", input),
        Command::Renew(input) => run_domain(cli.json, "renew", input),
        Command::Join(input) => run_domain(cli.json, "join", input),
        Command::Rejoin(input) => run_domain(cli.json, "rejoin", input),
        Command::Show => run_domain(cli.json, "show", &EmptyInput::default()),
        Command::Next => run_domain(cli.json, "next", &EmptyInput::default()),
        Command::Prev => run_domain(cli.json, "prev", &EmptyInput::default()),
        Command::Sync(input) => run_domain(cli.json, "sync", input),
        Command::Evict(input) => run_domain(cli.json, "evict", input),
        Command::Split(input) => run_domain(cli.json, "split", input),
        Command::Loop(input) => run_domain(cli.json, "loop", input),
        Command::Van(command) => match command {
            VanCommand::List => run_domain(cli.json, "van list", &EmptyInput::default()),
            VanCommand::Next => run_domain(cli.json, "van next", &EmptyInput::default()),
            VanCommand::Prev => run_domain(cli.json, "van prev", &EmptyInput::default()),
        },
        Command::Help => run_help(cli.json),
        Command::Mcp(command) => run_mcp(command, cli.config.clone()),
        Command::SelfUpdate(command) => run_self_update(cli.json, command),
        Command::Feedback(command) => run_feedback(cli.json, command),
    }
}

fn run_domain<T>(json: bool, operation: &str, input: &T) -> Result<(), i32> {
    emit_result(json, scaffold_operation::<T>(operation, input))
}

fn run_help(json: bool) -> Result<(), i32> {
    if json {
        return emit_result::<_, AppError>(json, Ok(caravan::help()));
    }
    println!("{AGENT_HELP}");
    Ok(())
}

fn run_mcp(command: &McpCommand, config_path: Option<PathBuf>) -> Result<(), i32> {
    let server = McpServer::new(
        StdioServerConfig {
            server_name: TOOL_NAME.to_owned(),
            server_version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        build_router(),
    );
    let context = AppContext { config_path };
    match command {
        McpCommand::Tools => {
            serde_json::to_writer_pretty(io::stdout().lock(), &server.tool_metadata())
                .map_err(|_| 1)?;
            println!();
            Ok(())
        }
        McpCommand::Stdio => server.serve_stdio(&context).map_err(|error| {
            eprintln!("mcp error: {error}");
            1
        }),
    }
}

fn run_self_update(json: bool, command: &SelfUpdateCommand) -> Result<(), i32> {
    let updater = updatable_cli::Updater::new(updater_config());
    match command {
        SelfUpdateCommand::Status => emit_result(
            json,
            updater
                .current_status()
                .map_err(updatable_cli::UpdateError::from),
        ),
        SelfUpdateCommand::Check => emit_result(
            json,
            updater
                .check_latest()
                .map_err(updatable_cli::UpdateError::from),
        ),
        SelfUpdateCommand::Run => emit_result(
            json,
            updater
                .run_update()
                .map_err(updatable_cli::UpdateError::from),
        ),
    }
}

fn run_feedback(json: bool, command: &FeedbackCommand) -> Result<(), i32> {
    match command {
        FeedbackCommand::Status => {
            let output = serde_json::json!({
                "enabled": true,
                "destination": feedback_destination(),
            });
            emit_result::<_, AppError>(json, Ok(output))
        }
        FeedbackCommand::Report(args) => {
            let mut event = FeedbackEvent::new(
                args.kind.into(),
                args.component
                    .clone()
                    .unwrap_or_else(|| TOOL_NAME.to_owned()),
                args.summary.clone(),
            );
            if let Some(detail) = &args.detail {
                event = event.with_detail(detail.clone());
            }
            if let Some(severity) = args.severity {
                event = event.with_severity(severity.into());
            }
            let reporter = Reporter::from_config(&feedback_config());
            emit_result(
                json,
                reporter.report(&event).map(|()| {
                    serde_json::json!({
                        "reported": true,
                        "destination": reporter.destination(),
                    })
                }),
            )
        }
    }
}

fn emit_result<T, E>(json: bool, result: Result<T, E>) -> Result<(), i32>
where
    T: Serialize,
    E: StructuredError + std::fmt::Display,
{
    if json {
        return write_json_result(io::stdout().lock(), result).map_err(|_| 1);
    }
    match result {
        Ok(output) => {
            serde_json::to_writer_pretty(io::stdout().lock(), &output).map_err(|_| 1)?;
            println!();
            Ok(())
        }
        Err(error) => {
            eprintln!("cara: {error}");
            if let Some(details) = error.details() {
                eprintln!(
                    "{}",
                    serde_json::to_string_pretty(&details).unwrap_or_else(|_| details.to_string())
                );
            }
            Err(1)
        }
    }
}

#[allow(dead_code)]
fn _operation_output_type_anchor(_output: OperationOutput) {}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_tree_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn join_accepts_explicit_tail_target() {
        let cli =
            Cli::try_parse_from(["cara", "join", "--tail-pr", "42"]).expect("join target parses");
        let Command::Join(input) = cli.command else {
            panic!("expected join");
        };
        assert_eq!(input.tail_pr, Some(42));
        assert_eq!(input.head_pr, None);
    }

    #[test]
    fn check_rejects_both_target_forms() {
        assert!(
            Cli::try_parse_from(["cara", "check", "--tail-pr", "1", "--head-pr", "2"]).is_err()
        );
    }
}
