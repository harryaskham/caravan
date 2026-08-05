//! `cara` command-line entry point.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use caravan::{
    AGENT_HELP, AppContext, AppError, CheckInput, CreateInput, EvictInput, JoinInput,
    LockRecoverInput, LockStatusInput, LoopInput, PauseInput, ResumeInput, SplitInput, SyncInput,
    TOOL_NAME, active_updater_config, build_router,
    concat::{ConcatExecuteInput, ConcatInput},
    feedback_config, feedback_configuration_error, feedback_panic_config,
    repair::{
        RepairAbortInput, RepairAuthorizeAgentEditsInput, RepairContinueInput, RepairGrantInput,
        RepairRevokeGrantInput, RepairStartInput, RepairStatusInput,
    },
    self_update_check, self_update_run, self_update_status,
    unpark::UnparkInput,
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
    long_about = "Caravan maintains GitHub PRs as labelled, mechanically compatible chains. Automation is non-interactive: sync either converges or returns one structured decision point. Human TTY membership commands can assist with safe branch, commit, push, and PR creation.",
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

    /// Repository to operate on, instead of inferring one from the working
    /// directory.
    ///
    /// Every command except `web` resolved its repository from the invocation
    /// directory, so answering a question about a repository required standing
    /// in it. That is a real constraint rather than a preference: an operator or
    /// agent watching one checkout could not read another without changing
    /// directory, and `web` already took `--repo`, so the capability existed and
    /// was reachable from exactly one subcommand (bd-3c0d9e).
    ///
    /// Spelled `--repository`, not `--repo`: `web` takes a REPEATABLE, required
    /// `--repo` for its multi-repository view, and clap rejects a duplicate long
    /// name at command-tree construction, which made `cara web` unconstructable.
    /// Renaming the single-repository global is the non-breaking half of that
    /// collision (bd-99b842).
    #[arg(long = "repository", global = true, value_name = "PATH")]
    repo: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Explicitly initialize local config and verify/create repository resources.
    Init,
    /// Discover the current PR, all caravans, invalid fragments, and decisions.
    Status,
    /// Validate repository policy without repository or provider access.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Serve the embedded multi-repository Caravan operations dashboard.
    Web(caravan::web::WebInput),
    /// Read the bounded repository event journal, optionally following new records.
    Log(LogCommand),
    /// Report the next PR at a requested queue position, optionally checking it out.
    Queue(caravan::NextInput),
    /// Decide whether existing CI evidence still applies to one exact PR.
    CiGate(caravan::CiGateInput),
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
    /// Set or clear audited automatic-admission priority metadata.
    #[command(subcommand)]
    Priority(PriorityCommand),
    /// Show the current branch's whole caravan and position.
    Show,
    /// Check out the next PR toward the current caravan tail.
    Next,
    /// Check out the previous PR toward the current caravan head.
    Prev,
    /// Arm or revoke audited durable PR-scoped force intent.
    Force(ForceCommand),
    /// Reviewed transition authority for durable PR-scoped force intent.
    #[command(subcommand)]
    ForceIntent(ReviewedForceIntentCommand),
    /// Explicitly freeze one caravan and disable only its head auto-merge.
    Pause(PauseInput),
    /// Explicitly revalidate and resume one paused caravan.
    Resume(ResumeInput),
    /// Recover one exact engine-owned terminal-red parked generation.
    Unpark(UnparkInput),
    /// Exact-owner checkpoint/finalize/rollback for an already-paused caravan.
    #[command(subcommand)]
    PauseRecovery(Box<PauseRecoveryCommand>),
    /// Idempotently synchronize one or all caravans until a decision point.
    ///
    /// Pass `--dry-run` to preview the exact tick without any provider
    /// mutation; it is identical to `cara plan sync`.
    Sync(SyncInput),
    /// Preview exact domain operations without provider mutation.
    #[command(subcommand)]
    Plan(PlanCommand),
    /// Atomically append one complete live caravan after another reviewed plan.
    Concat(ConcatExecuteInput),
    /// Prepare, inspect, or continue a Cara-owned isolated repair workspace.
    #[command(subcommand)]
    Repair(RepairCommand),
    /// Evict a PR and safely reconnect its child when compatible.
    Evict(EvictInput),
    /// Split before a PR, making it a new caravan head.
    Split(SplitInput),
    /// Repeatedly run lightweight sync-all ticks in the foreground.
    Loop(LoopInput),
    /// Inspect or recover the repository operation lock.
    #[command(subcommand)]
    Lock(LockCommand),
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

#[derive(Debug, Args)]
struct LogCommand {
    #[command(flatten)]
    input: caravan::journal::LogInput,
    /// Keep streaming new records until interrupted (CLI-only).
    #[arg(short = 'f', long)]
    follow: bool,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Strictly parse config and verify its declared Cara reader floor.
    Check,
}

#[derive(Debug, Subcommand)]
enum PriorityCommand {
    /// Set one exact configured priority label on an unenrolled PR.
    Set(caravan::priority::PrioritySetInput),
    /// Clear configured priority labels and restore FIFO ordering.
    Clear(caravan::priority::PriorityClearInput),
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
enum LockCommand {
    /// Inspect lock owner, age, stale state, token, and PID liveness.
    Status(LockStatusInput),
    /// Remove only a verified-stale lock after explicit confirmation.
    Recover(LockRecoverInput),
}

#[derive(Debug, Args)]
struct ForceCommand {
    /// Exact active Caravan head PR to arm (omit only with `revoke`).
    #[arg(long, value_name = "PR")]
    pr: Option<u64>,
    /// Audited operator identity (omit only with `revoke`).
    #[arg(long, value_name = "ACTOR")]
    actor: Option<String>,
    /// Bounded operator rationale (omit only with `revoke`).
    #[arg(long, value_name = "TEXT")]
    reason: Option<String>,
    #[command(subcommand)]
    command: Option<ForceSubcommand>,
}

#[derive(Debug, Subcommand)]
enum ForceSubcommand {
    /// Revoke current durable PR-scoped force intent.
    Revoke(caravan::force::ForceIntentInput),
}

#[derive(Debug, Subcommand)]
enum PauseRecoveryCommand {
    /// Bind one exact external owner generation before provider mutation.
    Prepare(caravan::pause::PauseRecoveryInput),
    /// Verify and checkpoint the exact desired base with the old head.
    CheckpointBase(caravan::pause::PauseRecoveryInput),
    /// Verify and checkpoint the exact replacement one-commit head and tree.
    CheckpointHead(caravan::pause::PauseRecoveryInput),
    /// Verify virtual merge/check attribution, advance evidence, and release.
    Finalize(caravan::pause::PauseRecoveryInput),
    /// Verify reverse-lease restoration to the exact old state and release.
    Rollback(caravan::pause::PauseRecoveryInput),
}

impl PauseRecoveryCommand {
    const fn phase(&self) -> caravan::pause::PauseRecoveryPhase {
        match self {
            Self::Prepare(_) => caravan::pause::PauseRecoveryPhase::Prepare,
            Self::CheckpointBase(_) => caravan::pause::PauseRecoveryPhase::CheckpointBase,
            Self::CheckpointHead(_) => caravan::pause::PauseRecoveryPhase::CheckpointHead,
            Self::Finalize(_) => caravan::pause::PauseRecoveryPhase::Finalize,
            Self::Rollback(_) => caravan::pause::PauseRecoveryPhase::Rollback,
        }
    }

    const fn input(&self) -> &caravan::pause::PauseRecoveryInput {
        match self {
            Self::Prepare(input)
            | Self::CheckpointBase(input)
            | Self::CheckpointHead(input)
            | Self::Finalize(input)
            | Self::Rollback(input) => input,
        }
    }
}

#[derive(Debug, Subcommand)]
enum ReviewedForceIntentCommand {
    /// Re-read exact provider/membership/check/decision evidence without mutation.
    Preview(caravan::force_intent::ReviewedForceIntentInput),
    /// Atomically converge durable PR force intent plus squash auto-merge.
    Apply(caravan::force_intent::ReviewedForceIntentInput),
    /// Idempotently revoke durable PR force intent, including after expiry.
    Revoke(caravan::force_intent::ReviewedForceIntentInput),
}

impl ForceCommand {
    fn operation(&self) -> Result<(caravan::force::ForceIntentInput, bool), AppError> {
        if let Some(ForceSubcommand::Revoke(input)) = &self.command {
            if self.pr.is_some() || self.actor.is_some() || self.reason.is_some() {
                return Err(AppError::validation(
                    "force_arguments_ambiguous",
                    "put --pr/--actor/--reason after `force revoke`, not before it",
                ));
            }
            return Ok((input.clone(), true));
        }
        let input = caravan::force::ForceIntentInput {
            pr: self.pr.ok_or_else(|| {
                AppError::validation("force_pr_required", "cara force requires --pr")
            })?,
            actor: self.actor.clone().ok_or_else(|| {
                AppError::validation("force_actor_required", "cara force requires --actor")
            })?,
            reason: self.reason.clone().ok_or_else(|| {
                AppError::validation("force_reason_required", "cara force requires --reason")
            })?,
        };
        Ok((input, false))
    }
}

#[derive(Debug, Subcommand)]
enum PlanCommand {
    /// Plan sync and first auto-admission through the no-write preflight barrier.
    Sync(SyncInput),
    /// Review exact source/target topology and rewrite scope without mutation.
    Concat(ConcatInput),
}

#[derive(Debug, Subcommand)]
enum RepairCommand {
    /// Create or reuse an isolated exact-head repair workspace.
    Start(RepairStartInput),
    /// Authorize one exact agent to edit bounded repository content.
    AuthorizeAgentEdits(RepairAuthorizeAgentEditsInput),
    /// Apply reviewed source changes to explicit semantic paths.
    Grant(RepairGrantInput),
    /// Revoke semantic grants and restore their pre-grant staged blobs.
    RevokeGrant(RepairRevokeGrantInput),
    /// Verify, non-force publish, and resume sync-all.
    Continue(RepairContinueInput),
    /// Inspect persisted repair evidence without mutation.
    Status(RepairStatusInput),
    /// Explicitly remove one reviewed local repair workspace/session.
    Abort(RepairAbortInput),
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
    /// Stable receiver-side dedupe key. Never put credentials in this value.
    #[arg(long)]
    fingerprint: Option<String>,
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
    let cli = Cli::parse();
    let panic_feedback = feedback_panic_config(cli.json);
    feedback_cli::install_panic_hook(&panic_feedback);
    if active_updater_config().is_ok()
        && let Err(error) = updatable_cli::maybe_apply_staged_update(TOOL_NAME)
    {
        eprintln!("warning: staged-update check failed: {error}");
    }

    if let Err(code) = run(&cli) {
        std::process::exit(code);
    }
}

fn run(cli: &Cli) -> Result<(), i32> {
    match &cli.command {
        Command::Init => run_init(cli),
        Command::Status => run_status(cli),
        Command::Config(command) => run_config(cli, command),
        Command::Web(input) => run_web(cli, input),
        Command::Log(command) => run_log(cli, command),
        Command::Queue(input) => run_queue(cli, input),
        Command::CiGate(input) => run_ci_gate(cli, input),
        Command::Check(input) => run_check(cli, input),
        Command::New(input) => run_create_membership(cli, input),
        Command::Renew(input) => {
            run_membership(cli, |context| caravan::membership::renew(context, input))
        }
        Command::Join(input) => run_join_membership(cli, input),
        Command::Rejoin(input) => {
            run_membership(cli, |context| caravan::membership::rejoin(context, input))
        }
        Command::Priority(command) => run_priority(cli, command),
        Command::Show => run_show(cli),
        Command::Next => run_navigation(
            cli,
            caravan::navigation::Scope::Caravan,
            caravan::navigation::Direction::Next,
        ),
        Command::Prev => run_navigation(
            cli,
            caravan::navigation::Scope::Caravan,
            caravan::navigation::Direction::Previous,
        ),
        Command::Force(command) => run_force(cli, command),
        Command::ForceIntent(command) => run_reviewed_force_intent(cli, command),
        Command::Pause(input) => run_pause(cli, input),
        Command::Resume(input) => run_resume(cli, input),
        Command::Unpark(input) => run_unpark(cli, input),
        Command::PauseRecovery(command) => run_pause_recovery(cli, command),
        Command::Sync(input) => run_sync(cli, input),
        Command::Plan(command) => run_plan(cli, command),
        Command::Concat(input) => run_concat(cli, input),
        Command::Repair(command) => run_repair(cli, command),
        Command::Evict(input) => run_evict(cli, input),
        Command::Split(input) => run_split(cli, input),
        Command::Loop(input) => run_loop(cli, input),
        Command::Lock(command) => run_lock(cli, command),
        Command::Van(command) => match command {
            VanCommand::List => run_van_list(cli),
            VanCommand::Next => run_navigation(
                cli,
                caravan::navigation::Scope::Fleet,
                caravan::navigation::Direction::Next,
            ),
            VanCommand::Prev => run_navigation(
                cli,
                caravan::navigation::Scope::Fleet,
                caravan::navigation::Direction::Previous,
            ),
        },
        Command::Help => run_help(cli.json),
        Command::Mcp(command) => run_mcp(command, cli.config.as_deref()),
        Command::SelfUpdate(command) => run_self_update(cli.json, command),
        Command::Feedback(command) => run_feedback(cli.json, command),
    }
}

fn run_web(cli: &Cli, input: &caravan::web::WebInput) -> Result<(), i32> {
    if cli.config.is_some() {
        let error = AppError::validation(
            "web_global_config_unsupported",
            "cara web loads config from each explicit --repo path; do not pass global --config",
        );
        return if cli.json {
            emit_result::<serde_json::Value, _>(true, Err(error))
        } else {
            emit_human_error(error)
        };
    }
    if cli.json {
        return emit_result::<serde_json::Value, _>(
            true,
            Err(AppError::validation(
                "web_json_mode_unsupported",
                "cara web is an HTTP server; omit global --json and use /api/v1/state",
            )),
        );
    }
    caravan::web::serve(input)
        .map_err(|error| emit_human_error(error).expect_err("web server failure is nonzero"))
}

fn load_context(cli: &Cli) -> Result<AppContext, i32> {
    let loaded = match cli.repo.as_deref() {
        Some(repository) => AppContext::load_from_directory(repository, cli.config.as_deref()),
        None => AppContext::load(cli.config.as_deref()),
    };
    loaded.map_err(|error| {
        if cli.json {
            emit_result::<serde_json::Value, _>(true, Err(error))
                .expect_err("an error envelope returns a nonzero exit code")
        } else {
            eprintln!("cara: {error}");
            2
        }
    })
}

fn run_log(cli: &Cli, command: &LogCommand) -> Result<(), i32> {
    let context = load_context(cli)?;
    if !command.follow {
        let result = caravan::journal::snapshot(&context, &command.input);
        if cli.json {
            return emit_result(true, result);
        }
        return match result {
            Ok(output) => {
                for record in &output.records {
                    println!("{}", render_journal_record(record));
                }
                // The journal lives in this checkout's Git common directory.
                // An empty result from a checkout that never wrote one is not
                // evidence that no tick ran anywhere: 248 records once sat in a
                // sibling checkout while this surface reported nothing
                // (bd-768f80). Name the exact file rather than stay silent.
                if output.source.unreadable_records > 0 {
                    eprintln!(
                        "{} skipped {} unreadable record(s) in {} — usually a newer Cara wrote record types this binary does not know",
                        warning("log:"),
                        output.source.unreadable_records,
                        output.source.path
                    );
                }
                if output.matching_records == 0 {
                    if output.source.empty_result_is_uninformative() {
                        eprintln!(
                            "{} no journal at {} — journals are per-checkout, so this is not evidence that no tick ran",
                            warning("log:"),
                            output.source.path
                        );
                    } else {
                        eprintln!(
                            "{} no matching records in {}",
                            dim("log:"),
                            output.source.path
                        );
                    }
                }
                Ok(())
            }
            Err(error) => emit_human_error(error),
        };
    }

    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = Arc::clone(&stop);
    ctrlc::set_handler(move || signal_stop.store(true, Ordering::SeqCst)).map_err(|error| {
        eprintln!("cara: could not install log signal handler: {error}");
        1
    })?;
    caravan::journal::follow(&context, &command.input, &stop, |record| {
        if cli.json {
            if serde_json::to_writer(io::stdout().lock(), record).is_ok() {
                println!();
            }
        } else {
            println!("{}", render_journal_record(record));
        }
    })
    .map_err(|error| {
        let _ = emit_human_error(error);
        1
    })
}

fn render_journal_record(record: &caravan::journal::JournalRecord) -> String {
    match record {
        caravan::journal::JournalRecord::Event { event, .. } => format!(
            "{} {} event={} operation={} prs={}",
            event.timestamp,
            event.kind,
            event.event_id.0,
            event.operation_id.0,
            event
                .prs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
        caravan::journal::JournalRecord::HookDelivery {
            timestamp,
            event_id,
            kind,
            delivery,
            ..
        } => format!(
            "{timestamp} {kind} hook={:?} event={} exit={:?} stdout_bytes={} stderr_bytes={}",
            delivery.state,
            event_id.0,
            delivery.exit_code,
            delivery.stdout_bytes,
            delivery.stderr_bytes
        ),
    }
}

fn run_init(cli: &Cli) -> Result<(), i32> {
    let context = load_context(cli)?;
    emit_result(cli.json, caravan::initialization::init(&context))
}

fn run_config(cli: &Cli, command: &ConfigCommand) -> Result<(), i32> {
    match command {
        ConfigCommand::Check => {
            let path = cli
                .config
                .clone()
                .unwrap_or_else(|| PathBuf::from(caravan::config::DEFAULT_CONFIG_PATH));
            let result = caravan::config::CaravanConfig::load(&path).map(|config| {
                serde_json::json!({
                    "compatible": true,
                    "config_version": config.version,
                    "min_cara_version": config.min_cara_version,
                    "reader_version": caravan::config::CARA_VERSION,
                    "path": path,
                    "provider_mutated": false
                })
            });
            if cli.json {
                emit_result(true, result)
            } else {
                match result {
                    Ok(receipt) => {
                        println!(
                            "config compatible: {} (Cara {}, provider mutation: false)",
                            receipt["path"].as_str().unwrap_or(".caravan/config.yaml"),
                            caravan::config::CARA_VERSION
                        );
                        Ok(())
                    }
                    Err(error) => {
                        eprintln!("cara: {error}");
                        Err(2)
                    }
                }
            }
        }
    }
}

fn emit_status_result(
    json: bool,
    result: Result<caravan::read::StatusReadReceipt, AppError>,
) -> Result<(), i32> {
    if json {
        return emit_result(true, result);
    }
    match result {
        Ok(receipt) => {
            print!("{}", render_status(&receipt.output));
            if let Some(partial) = receipt.status_partial {
                eprintln!(
                    "status partial: phase={} cursor={} elapsed={}ms remaining={}ms; {}",
                    partial.exhausted_phase,
                    partial.cursor,
                    partial.elapsed_ms,
                    partial.remaining_ms,
                    partial.safe_next_action,
                );
            }
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
struct LinuxProcessRow {
    pid: u32,
    ppid: u32,
    session: u32,
    start_time: u64,
}

#[cfg(target_os = "linux")]
fn linux_process_row(pid: u32) -> Option<LinuxProcessRow> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = stat.rsplit_once(')')?.1;
    let fields = after_name.split_whitespace().collect::<Vec<_>>();
    Some(LinuxProcessRow {
        pid,
        ppid: fields.get(1)?.parse().ok()?,
        session: fields.get(3)?.parse().ok()?,
        // Fields after comm start at proc field 3; starttime is field 22.
        start_time: fields.get(19)?.parse().ok()?,
    })
}

#[cfg(target_os = "linux")]
fn linux_process_rows() -> Vec<LinuxProcessRow> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok())
        .filter_map(linux_process_row)
        .collect()
}

#[cfg(target_os = "linux")]
fn process_rows(id_field: &str) -> Vec<(u32, u32)> {
    linux_process_rows()
        .into_iter()
        .filter_map(|row| match id_field {
            "ppid" => Some((row.pid, row.ppid)),
            "sid" => Some((row.pid, row.session)),
            _ => None,
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn process_rows(id_field: &str) -> Vec<(u32, u32)> {
    let columns = format!("pid=,{id_field}=");
    let Ok(output) = ProcessCommand::new("ps")
        .args(["-eo", columns.as_str()])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((
                fields.next()?.parse::<u32>().ok()?,
                fields.next()?.parse::<u32>().ok()?,
            ))
        })
        .collect()
}

fn descendant_pids(root: u32) -> Vec<u32> {
    let rows = process_rows("ppid");
    let mut frontier = vec![root];
    let mut descendants = Vec::new();
    while let Some(parent) = frontier.pop() {
        for (pid, ppid) in &rows {
            if *ppid == parent && !descendants.contains(pid) {
                descendants.push(*pid);
                frontier.push(*pid);
            }
        }
    }
    descendants
}

fn session_pids(session: u32) -> Vec<u32> {
    process_rows("sid")
        .into_iter()
        .filter_map(|(pid, sid)| (sid == session && pid != std::process::id()).then_some(pid))
        .collect()
}

#[cfg(target_os = "linux")]
fn linux_process_start_time(pid: u32) -> Option<u64> {
    linux_process_row(pid).map(|row| row.start_time)
}

#[cfg(unix)]
struct StatusProcessIdentity {
    pid: u32,
    #[cfg(target_os = "linux")]
    pidfd: Option<std::os::fd::OwnedFd>,
    #[cfg(target_os = "linux")]
    start_time: Option<u64>,
}

#[cfg(unix)]
impl StatusProcessIdentity {
    fn capture(pid: u32) -> Self {
        #[cfg(target_os = "linux")]
        let pidfd = i32::try_from(pid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .and_then(|pid| {
                rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).ok()
            });
        Self {
            pid,
            #[cfg(target_os = "linux")]
            pidfd,
            #[cfg(target_os = "linux")]
            start_time: linux_process_start_time(pid),
        }
    }

    fn pidfd_receipt(&self) -> PidfdAcquisition {
        #[cfg(target_os = "linux")]
        {
            if self.pidfd.is_some() {
                PidfdAcquisition::Acquired
            } else {
                PidfdAcquisition::Unavailable
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = self;
            PidfdAcquisition::Unsupported
        }
    }

    fn signal(&self, signal: rustix::process::Signal) -> SignalOutcome {
        #[cfg(target_os = "linux")]
        if let Some(pidfd) = &self.pidfd {
            // pidfd binds the signal to this exact process generation, so a
            // rapidly reused numeric PID cannot receive a stale watchdog KILL.
            match rustix::process::pidfd_send_signal(pidfd, signal) {
                Ok(()) => return SignalOutcome::PidfdSent,
                Err(rustix::io::Errno::SRCH) => return SignalOutcome::AlreadyExited,
                Err(_) => {}
            }
        }
        let Ok(raw_pid) = i32::try_from(self.pid) else {
            return SignalOutcome::InvalidIdentity;
        };
        let Some(pid) = rustix::process::Pid::from_raw(raw_pid) else {
            return SignalOutcome::InvalidIdentity;
        };
        let group = rustix::process::kill_process_group(pid, signal);
        let process = rustix::process::kill_process(pid, signal);
        if group.is_ok() || process.is_ok() {
            SignalOutcome::NumericSent
        } else if matches!(group, Err(rustix::io::Errno::SRCH))
            && matches!(process, Err(rustix::io::Errno::SRCH))
        {
            SignalOutcome::AlreadyExited
        } else {
            SignalOutcome::Failed
        }
    }

    fn reap_if_child(&self) -> ReapOutcome {
        let Ok(raw_pid) = i32::try_from(self.pid) else {
            return ReapOutcome::InvalidIdentity;
        };
        let Some(pid) = rustix::process::Pid::from_raw(raw_pid) else {
            return ReapOutcome::InvalidIdentity;
        };
        match rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG) {
            Ok(Some(_)) => ReapOutcome::Reaped,
            Ok(None) => ReapOutcome::StillRunning,
            Err(rustix::io::Errno::CHILD) => ReapOutcome::NotAChild,
            Err(_) => ReapOutcome::Failed,
        }
    }

    fn same_generation_alive(&self) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(start_time) = self.start_time {
            return linux_process_start_time(self.pid) == Some(start_time);
        }
        let Ok(raw_pid) = i32::try_from(self.pid) else {
            return false;
        };
        rustix::process::Pid::from_raw(raw_pid)
            .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Linux-only variants remain part of the cross-platform cleanup receipt.
enum PidfdAcquisition {
    Acquired,
    Unavailable,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // PidfdSent is constructed only by the Linux cleanup path.
enum SignalOutcome {
    NotAttempted,
    PidfdSent,
    NumericSent,
    AlreadyExited,
    InvalidIdentity,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReapOutcome {
    NotAttempted,
    Reaped,
    StillRunning,
    NotAChild,
    InvalidIdentity,
    Failed,
}

#[derive(Debug)]
struct StatusCleanupTargetReceipt {
    pid: u32,
    pidfd: PidfdAcquisition,
    term: SignalOutcome,
    kill: SignalOutcome,
    reap: ReapOutcome,
    survived: bool,
}

#[derive(Debug)]
struct StatusCleanupReceipt {
    subreaper_enabled: bool,
    root_reaped: bool,
    targets: Vec<StatusCleanupTargetReceipt>,
}

impl StatusCleanupReceipt {
    fn warning(&self) -> Option<String> {
        let survivors = self
            .targets
            .iter()
            .filter(|target| target.survived)
            .map(|target| {
                format!(
                    "pid={} pidfd={:?} term={:?} kill={:?} reap={:?}",
                    target.pid, target.pidfd, target.term, target.kill, target.reap
                )
            })
            .collect::<Vec<_>>();
        (!self.root_reaped || !survivors.is_empty()).then(|| {
            format!(
                "subreaper={} root_reaped={} survivors=[{}]",
                self.subreaper_enabled,
                self.root_reaped,
                survivors.join(", ")
            )
        })
    }
}

fn report_status_cleanup(receipt: &StatusCleanupReceipt) {
    if let Some(warning) = receipt.warning() {
        eprintln!("cara: status watchdog cleanup incomplete: {warning}");
    }
}

#[cfg(unix)]
fn capture_status_identities(root: u32) -> Vec<StatusProcessIdentity> {
    let mut pids = descendant_pids(root);
    for pid in session_pids(root) {
        if pid != root && !pids.contains(&pid) {
            pids.push(pid);
        }
    }
    pids.reverse();
    pids.into_iter()
        .map(StatusProcessIdentity::capture)
        .collect()
}

#[cfg(unix)]
fn merge_status_identities(
    root: u32,
    identities: &mut Vec<StatusProcessIdentity>,
    receipts: &mut Vec<StatusCleanupTargetReceipt>,
) {
    for identity in capture_status_identities(root) {
        if identities
            .iter()
            .any(|existing| existing.pid == identity.pid)
        {
            continue;
        }
        receipts.push(StatusCleanupTargetReceipt {
            pid: identity.pid,
            pidfd: identity.pidfd_receipt(),
            term: SignalOutcome::NotAttempted,
            kill: SignalOutcome::NotAttempted,
            reap: ReapOutcome::NotAttempted,
            survived: false,
        });
        identities.push(identity);
    }
}

#[cfg(unix)]
fn refresh_status_identities(
    root: u32,
    identities: &mut Vec<StatusProcessIdentity>,
    receipts: &mut Vec<StatusCleanupTargetReceipt>,
) {
    #[cfg(target_os = "linux")]
    merge_status_identities(root, identities, receipts);
    #[cfg(not(target_os = "linux"))]
    if identities.is_empty() {
        // Linux has cheap direct /proc scans and needs dynamic subreaper
        // adoption. Other Unix platforms use an external ps snapshot; once the
        // causally-ready provider is registered, repeated scans only add
        // scheduler latency and do not strengthen its durable identity.
        merge_status_identities(root, identities, receipts);
    }
}

fn merge_reap_outcome(existing: ReapOutcome, next: ReapOutcome) -> ReapOutcome {
    if existing == ReapOutcome::Reaped {
        existing
    } else {
        next
    }
}

// Keep the bounded TERM/KILL/adoption/reap phases and their typed receipts in
// one auditable transaction; splitting them would obscure phase ordering.
#[allow(clippy::too_many_lines)]
fn terminate_status_tree(child: &mut Child) -> StatusCleanupReceipt {
    #[cfg(target_os = "linux")]
    let subreaper_enabled = rustix::process::child_subreaper().ok().flatten().is_some();
    #[cfg(not(target_os = "linux"))]
    let subreaper_enabled = false;

    #[cfg(unix)]
    let root = child.id();
    #[cfg(unix)]
    let root_identity = StatusProcessIdentity::capture(root);
    #[cfg(unix)]
    let mut identities = Vec::new();
    #[cfg(unix)]
    let mut receipts = Vec::new();

    // A provider can appear between any two process snapshots. Re-scan during
    // the complete TERM phase and give every observed identity its own receipt.
    let term_deadline = std::time::Instant::now() + Duration::from_millis(250);
    loop {
        #[cfg(unix)]
        {
            refresh_status_identities(root, &mut identities, &mut receipts);
            for (identity, receipt) in identities.iter().zip(&mut receipts) {
                if receipt.term == SignalOutcome::NotAttempted {
                    receipt.term = identity.signal(rustix::process::Signal::TERM);
                }
                receipt.reap = merge_reap_outcome(receipt.reap, identity.reap_if_child());
            }
        }
        if std::time::Instant::now() >= term_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // Keep the worker alive while descendants receive KILL. Its own wait path
    // can reap providers before any orphan adoption is needed.
    let descendant_deadline = std::time::Instant::now() + Duration::from_millis(400);
    loop {
        #[cfg(unix)]
        {
            refresh_status_identities(root, &mut identities, &mut receipts);
            for (identity, receipt) in identities.iter().zip(&mut receipts) {
                if receipt.kill == SignalOutcome::NotAttempted {
                    receipt.kill = identity.signal(rustix::process::Signal::KILL);
                }
                receipt.reap = merge_reap_outcome(receipt.reap, identity.reap_if_child());
            }
        }
        let _ = child.try_wait();
        #[cfg(unix)]
        let any_alive = identities
            .iter()
            .any(StatusProcessIdentity::same_generation_alive);
        #[cfg(not(unix))]
        let any_alive = false;
        if (!identities.is_empty() && !any_alive)
            || std::time::Instant::now() >= descendant_deadline
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // Only after descendants had a bounded opportunity to be reaped do we
    // terminate the worker itself. This avoids creating the orphan in the
    // common case and still has subreaper adoption as the Linux backstop.
    #[cfg(unix)]
    let _ = root_identity.signal(rustix::process::Signal::TERM);
    let root_term_deadline = std::time::Instant::now() + Duration::from_millis(100);
    let mut root_reaped = false;
    while std::time::Instant::now() < root_term_deadline {
        match child.try_wait() {
            Ok(Some(_)) => {
                root_reaped = true;
                break;
            }
            Err(_) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    if !root_reaped {
        #[cfg(unix)]
        let _ = root_identity.signal(rustix::process::Signal::KILL);
        let _ = child.kill();
    }

    // Final scans cover providers adopted only when the worker exited. Every
    // new identity receives a typed KILL/reap receipt before survivor checking.
    let final_deadline = std::time::Instant::now() + Duration::from_millis(250);
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            root_reaped = true;
        }
        #[cfg(unix)]
        {
            refresh_status_identities(root, &mut identities, &mut receipts);
            for (identity, receipt) in identities.iter().zip(&mut receipts) {
                if receipt.kill == SignalOutcome::NotAttempted {
                    receipt.kill = identity.signal(rustix::process::Signal::KILL);
                }
                receipt.reap = merge_reap_outcome(receipt.reap, identity.reap_if_child());
            }
        }
        if std::time::Instant::now() >= final_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    #[cfg(unix)]
    for (identity, receipt) in identities.iter().zip(&mut receipts) {
        receipt.reap = merge_reap_outcome(receipt.reap, identity.reap_if_child());
        receipt.survived = identity.same_generation_alive();
    }

    StatusCleanupReceipt {
        subreaper_enabled,
        root_reaped,
        targets: receipts,
    }
}

fn bounded_status_file(path: &Path) -> String {
    const LIMIT: u64 = 8 * 1024 * 1024;
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() <= LIMIT) {
        std::fs::read_to_string(path).unwrap_or_default()
    } else {
        String::new()
    }
}

// Keep spawn, file-backed capture, process-tree reap, checkpoint fallback, and
// envelope rendering visible at one command-boundary safety transaction.
#[allow(clippy::too_many_lines)]
fn run_status(cli: &Cli) -> Result<(), i32> {
    if std::env::var_os("CARA_STATUS_WATCHDOG_WORKER").is_some() {
        #[cfg(unix)]
        let _ = rustix::process::setsid();
        let context = load_context(cli)?;
        return emit_status_result(cli.json, caravan::read::status_resilient(&context));
    }
    let context = load_context(cli)?;
    #[cfg(target_os = "linux")]
    let _ = rustix::process::set_child_subreaper(Some(rustix::process::Pid::INIT));

    let checkpoint = std::env::temp_dir().join(format!(
        "cara-status-watchdog-{}-{}.json",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis())
    ));
    let stdout_path = checkpoint.with_extension("stdout");
    let stderr_path = checkpoint.with_extension("stderr");
    let executable = std::env::current_exe().map_err(|error| {
        eprintln!("cara: could not resolve status watchdog executable: {error}");
        1
    })?;
    let stdout_file = std::fs::File::create(&stdout_path).map_err(|error| {
        eprintln!("cara: could not create bounded status stdout: {error}");
        1
    })?;
    let stderr_file = std::fs::File::create(&stderr_path).map_err(|error| {
        eprintln!("cara: could not create bounded status stderr: {error}");
        1
    })?;
    let config_argument = context.config_path.display().to_string();
    let mut command = ProcessCommand::new(executable);
    command
        .current_dir(&context.repository_path)
        .args(["--json", "--config", config_argument.as_str(), "status"])
        .env("CARA_STATUS_WATCHDOG_WORKER", "1")
        .env(
            "CARA_STATUS_WATCHDOG_CHECKPOINT",
            checkpoint.display().to_string(),
        )
        // Files, not pipes: an escaped descendant cannot keep the parent
        // blocked waiting for EOF after the worker has been reaped. The worker
        // creates a dedicated session before provider commands are launched.
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));
    let mut child = command.spawn().map_err(|error| {
        eprintln!("cara: could not start bounded status worker: {error}");
        1
    })?;
    let started = std::time::Instant::now();
    let deadline = started + caravan::read::STATUS_COMMAND_WATCHDOG;
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (Some(status), false),
            Ok(None) if std::time::Instant::now() >= deadline => {
                let cleanup = terminate_status_tree(&mut child);
                report_status_cleanup(&cleanup);
                break (None, true);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                let cleanup = terminate_status_tree(&mut child);
                report_status_cleanup(&cleanup);
                let _ = std::fs::remove_file(&checkpoint);
                let _ = std::fs::remove_file(&stdout_path);
                let _ = std::fs::remove_file(&stderr_path);
                return emit_status_result(
                    cli.json,
                    Err(AppError::execution(
                        "status_watchdog_failed",
                        "could not collect the bounded read-only status worker",
                        Some(serde_json::json!({
                            "error": error.to_string(),
                            "mutated": false,
                        })),
                    )),
                );
            }
        }
    };
    let stdout = bounded_status_file(&stdout_path);
    let stderr = bounded_status_file(&stderr_path);
    let _ = std::fs::remove_file(&stdout_path);
    let _ = std::fs::remove_file(&stderr_path);
    if timed_out {
        let fallback =
            caravan::read::status_watchdog_fallback(&context, &checkpoint, started.elapsed());
        let _ = std::fs::remove_file(&checkpoint);
        return emit_status_result(cli.json, fallback);
    }
    let _ = std::fs::remove_file(&checkpoint);
    let status = status.expect("non-timeout worker has an exit status");
    if !status.success() {
        print!("{stdout}");
        eprint!("{stderr}");
        return Err(status.code().unwrap_or(1));
    }
    if cli.json {
        print!("{stdout}");
        eprint!("{stderr}");
        return Ok(());
    }
    let receipt = serde_json::from_str::<serde_json::Value>(&stdout)
        .ok()
        .and_then(|envelope| envelope.get("data").cloned())
        .and_then(|data| serde_json::from_value(data).ok())
        .ok_or_else(|| {
            eprintln!("cara: bounded status worker returned an invalid success envelope");
            1
        })?;
    emit_status_result(false, Ok(receipt))
}

fn run_queue(cli: &Cli, input: &caravan::NextInput) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = caravan::next::next(&context, input);
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            match &output.selected {
                Some(selected) => println!(
                    "{} #{} {} {}",
                    heading("QUEUE"),
                    selected.pr,
                    styled("1;35", selected.branch.clone()),
                    dim(selected.reason.clone()),
                ),
                // Not an error: an empty queue and a provider outage must never
                // look the same to a scheduler.
                None => println!("{}  {}", heading("QUEUE"), dim("no match")),
            }
            if let Some(receipt) = &output.checkout {
                println!(
                    "  checked out {} -> {}",
                    receipt.from_branch, receipt.to_branch
                );
            }
            println!("  {}", output.next);
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_ci_gate(cli: &Cli, input: &caravan::CiGateInput) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = caravan::read::status(&context)
        .and_then(|status| caravan::ci_gate::evaluate(&status, input));
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            println!("{} run_ci={}", output.decision_code, output.run_ci);
            println!("  {}", output.reason);
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_check(cli: &Cli, input: &CheckInput) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = caravan::read::check(&context, input);
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            print!("{}", render_check(&output));
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_show(cli: &Cli) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = caravan::read::show(&context);
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            print!("{}", render_show(&output));
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_create_membership(cli: &Cli, input: &CreateInput) -> Result<(), i32> {
    let context = load_context(cli)?;
    let mut effective = input.clone();
    let mut result = caravan::membership::new(&context, &effective);
    if should_offer_pr_creation(cli, &result) {
        if let Err(error) = prepare_interactive_pr(&context) {
            return emit_human_error(error);
        }
        effective.create_pr = true;
        result = caravan::membership::new(&context, &effective);
    }
    emit_membership_result(cli, result)
}

fn run_join_membership(cli: &Cli, input: &JoinInput) -> Result<(), i32> {
    let context = load_context(cli)?;
    let mut effective = input.clone();
    let mut result = caravan::membership::join(&context, &effective);
    if effective.pr.is_none() && should_offer_pr_creation(cli, &result) {
        if let Err(error) = prepare_interactive_pr(&context) {
            return emit_human_error(error);
        }
        effective.create_pr = true;
        result = caravan::membership::join(&context, &effective);
    }
    emit_membership_result(cli, result)
}

fn should_offer_pr_creation(
    cli: &Cli,
    result: &Result<caravan::membership::MembershipOutput, AppError>,
) -> bool {
    if cli.json || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return false;
    }
    result.as_ref().is_err_and(|error| {
        matches!(
            error.code().as_str(),
            "current_pr_not_found"
                | "create_pr_on_default_branch"
                | "historical_current_pr_missing_caravan_label"
        )
    })
}

fn prepare_interactive_pr(context: &AppContext) -> Result<(), AppError> {
    let branch = git_stdout(context, &["branch", "--show-current"])?;
    if branch.is_empty() {
        return Err(AppError::validation(
            "interactive_pr_detached_head",
            "interactive PR creation requires a named branch",
        ));
    }
    let default_branch = git_stdout(
        context,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .ok()
    .and_then(|value| value.rsplit('/').next().map(ToOwned::to_owned))
    .filter(|value| !value.is_empty())
    .unwrap_or_else(|| "main".to_owned());
    if branch == default_branch {
        prepare_topic_branch(context)?;
    }
    if git_stdout(context, &["rev-parse", "--abbrev-ref", "@{upstream}"]).is_err() {
        if !confirm(
            "Current branch is not published. Push it to origin now?",
            true,
        )? {
            return Err(AppError::validation(
                "interactive_pr_push_declined",
                "branch push was declined; publish it and rerun the same command",
            ));
        }
        git_run(context, &["push", "-u", "origin", "HEAD"])?;
    }
    if !confirm(
        "No open PR exists for this branch. Create one with commit-derived GitHub defaults?",
        true,
    )? {
        return Err(AppError::validation(
            "interactive_pr_creation_declined",
            "PR creation was declined; rerun with --create-pr when ready",
        ));
    }
    Ok(())
}

fn prepare_topic_branch(context: &AppContext) -> Result<(), AppError> {
    let status = git_stdout(context, &["status", "--short"])?;
    if status.trim().is_empty() {
        let default = format!("cara/work-{}", unix_seconds());
        let branch = prompt_value("You are on the default branch. New topic branch", &default)?;
        git_run(context, &["switch", "-c", &branch])?;
        return Err(AppError::validation(
            "interactive_topic_branch_created",
            format!(
                "created topic branch `{branch}`, but there are no changes to commit yet; make or stage the intended changes, then rerun the same Cara command"
            ),
        ));
    }
    eprintln!("Changes on the default branch:\n{status}");
    if !confirm(
        "Create a topic branch, stage all listed changes, and commit them?",
        true,
    )? {
        return Err(AppError::validation(
            "interactive_commit_declined",
            "automatic branch/commit preparation was declined",
        ));
    }
    let message = prompt_value("Commit message", "Prepare pull request")?;
    let default_branch = format!("cara/{}", slugify(&message));
    let branch = prompt_value("Topic branch", &default_branch)?;
    git_run(context, &["switch", "-c", &branch])?;
    git_run(context, &["add", "-A"])?;
    git_run(context, &["commit", "-m", &message])?;
    Ok(())
}

fn prompt_value(label: &str, default: &str) -> Result<String, AppError> {
    eprint!("{label} [{default}]: ");
    io::Write::flush(&mut io::stderr())
        .map_err(|error| AppError::validation("interactive_prompt_failed", error.to_string()))?;
    let mut value = String::new();
    io::stdin()
        .read_line(&mut value)
        .map_err(|error| AppError::validation("interactive_prompt_failed", error.to_string()))?;
    let value = value.trim();
    Ok(if value.is_empty() {
        default.to_owned()
    } else {
        value.to_owned()
    })
}

fn confirm(prompt: &str, default_yes: bool) -> Result<bool, AppError> {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    eprint!("{prompt} {suffix}: ");
    io::Write::flush(&mut io::stderr())
        .map_err(|error| AppError::validation("interactive_prompt_failed", error.to_string()))?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| AppError::validation("interactive_prompt_failed", error.to_string()))?;
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(default_yes);
    }
    Ok(matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn git_stdout(context: &AppContext, args: &[&str]) -> Result<String, AppError> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(&context.repository_path)
        .output()
        .map_err(|error| AppError::validation("interactive_git_failed", error.to_string()))?;
    if !output.status.success() {
        return Err(AppError::validation(
            "interactive_git_failed",
            format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_run(context: &AppContext, args: &[&str]) -> Result<(), AppError> {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(&context.repository_path)
        .status()
        .map_err(|error| AppError::validation("interactive_git_failed", error.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::validation(
            "interactive_git_failed",
            format!("git {} failed", args.join(" ")),
        ))
    }
}

fn slugify(value: &str) -> String {
    let slug = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let slug = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        format!("work-{}", unix_seconds())
    } else {
        slug
    }
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn emit_membership_result(
    cli: &Cli,
    result: Result<caravan::membership::MembershipOutput, AppError>,
) -> Result<(), i32> {
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            print!("{}", render_membership(&output));
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_membership(
    cli: &Cli,
    execute: impl FnOnce(&AppContext) -> Result<caravan::membership::MembershipOutput, AppError>,
) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = execute(&context);
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            print!("{}", render_membership(&output));
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_force(cli: &Cli, command: &ForceCommand) -> Result<(), i32> {
    let (input, revoke) = match command.operation() {
        Ok(operation) => operation,
        Err(error) => return emit_result::<serde_json::Value, _>(cli.json, Err(error)),
    };
    let context = load_context(cli)?;
    let result = if revoke {
        caravan::force::revoke(&context, &input)
    } else {
        caravan::force::arm(&context, &input)
    };
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            println!(
                "force {:?} PR #{}: {} head={} default={} changed={}\n  {}",
                output.operation,
                output.pr,
                if output.intent_present {
                    "armed"
                } else {
                    "absent"
                },
                output.head.oid,
                output.default_branch.oid,
                output.mutated,
                output.next
            );
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_reviewed_force_intent(cli: &Cli, command: &ReviewedForceIntentCommand) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = match command {
        ReviewedForceIntentCommand::Preview(input) => {
            caravan::force_intent::preview(&context, input)
        }
        ReviewedForceIntentCommand::Apply(input) => caravan::force_intent::apply(&context, input),
        ReviewedForceIntentCommand::Revoke(input) => caravan::force_intent::revoke(&context, input),
    };
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            println!(
                "force-intent {} PR #{} head={} membership={} decision={} changed={} atomic={}\n  {}",
                output.action,
                output.pr,
                output.provider_head,
                output.membership_generation,
                output.failure_fingerprint,
                output.mutated,
                output.atomic_provider_transaction,
                output.next,
            );
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_priority(cli: &Cli, command: &PriorityCommand) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = match command {
        PriorityCommand::Set(input) => caravan::priority::set(&context, input),
        PriorityCommand::Clear(input) => caravan::priority::clear(&context, input),
    };
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            let basis = output.selected_label.as_deref().map_or_else(
                || "FIFO".to_owned(),
                |label| {
                    format!(
                        "{label} (rank {})",
                        output.selected_rank.unwrap_or_default()
                    )
                },
            );
            println!(
                "priority PR #{}: {} — {} provider receipts; audit durable",
                output.pr,
                basis,
                output.provider_receipts.len()
            );
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_pause(cli: &Cli, input: &PauseInput) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = caravan::pause::pause(&context, input);
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            println!(
                "pause #{}: {} — {}",
                output.pause.caravan_head,
                if output.receipt.changed {
                    "changed"
                } else {
                    "already paused"
                },
                output.next
            );
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_resume(cli: &Cli, input: &ResumeInput) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = caravan::pause::resume(&context, input);
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            println!(
                "resume #{}: {} — {}",
                output.pause.caravan_head,
                if output.receipt.changed {
                    "changed"
                } else {
                    "already resumed"
                },
                output.next
            );
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_unpark(cli: &Cli, input: &UnparkInput) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = caravan::unpark::unpark(&context, input);
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            println!(
                "unpark #{}: {} — {}",
                output.pr,
                if output.mutated {
                    "changed"
                } else {
                    "already recovered"
                },
                output.next
            );
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_pause_recovery(cli: &Cli, command: &PauseRecoveryCommand) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = caravan::pause::pause_recovery(&context, command.phase(), command.input());
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            println!(
                "pause-recovery {:?}: {:?}, fence {:?} — {}",
                output.phase, output.status, output.fence_state, output.next_action
            );
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_sync(cli: &Cli, input: &SyncInput) -> Result<(), i32> {
    let context = load_context(cli)?;
    // A dry-run must be reachable from the mutating command itself. Routing it
    // here keeps exactly one planner rather than a second, drift-prone preview.
    if input.dry_run {
        let result = caravan::sync::plan_sync(&context, input);
        if cli.json {
            return emit_result(true, result);
        }
        return match result {
            Ok(output) => {
                print!("{}", render_sync_plan(&output));
                Ok(())
            }
            Err(error) => emit_human_error(error),
        };
    }
    // Human sync is a long network operation: stream bounded stage progress so
    // the terminal is never silent. JSON/MCP callers install no observer and
    // keep their exact envelope contract.
    let result = if cli.json {
        caravan::sync::sync(&context, input)
    } else {
        caravan::sync::progress::observing(stream_sync_progress, || {
            caravan::sync::sync(&context, input)
        })
    };
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            print!("{}", render_sync(&output));
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_plan(cli: &Cli, command: &PlanCommand) -> Result<(), i32> {
    let context = load_context(cli)?;
    match command {
        PlanCommand::Sync(input) => {
            let result = caravan::sync::plan_sync(&context, input);
            if cli.json {
                return emit_result(true, result);
            }
            match result {
                Ok(output) => {
                    print!("{}", render_sync_plan(&output));
                    Ok(())
                }
                Err(error) => emit_human_error(error),
            }
        }
        PlanCommand::Concat(input) => {
            let result = caravan::concat::plan(&context, input);
            if cli.json {
                return emit_result(true, result);
            }
            match result {
                Ok(plan) => {
                    println!(
                        "concat plan {}: {:?} + {:?} -> {:?}; rewrites={}",
                        plan.plan_hash,
                        plan.target_members,
                        plan.source_members,
                        plan.new_ordering,
                        plan.members.len()
                    );
                    println!(
                        "execute: cara concat --source-head-pr {} --target-tail-pr {} --actor {} --reason {:?} --expected-plan-hash {}",
                        plan.source_caravan,
                        input.target_tail_pr,
                        plan.actor,
                        plan.reason,
                        plan.plan_hash
                    );
                    Ok(())
                }
                Err(error) => emit_human_error(error),
            }
        }
    }
}

fn run_concat(cli: &Cli, input: &ConcatExecuteInput) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = caravan::concat::execute(&context, input);
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            println!(
                "concatenated {:?}; plan={} idempotent={} changed={}",
                output.resulting_ordering,
                output.plan.plan_hash,
                output.idempotent,
                output.receipt.changed
            );
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

#[allow(clippy::too_many_lines)]
fn run_repair(cli: &Cli, command: &RepairCommand) -> Result<(), i32> {
    let context = load_context(cli)?;
    match command {
        RepairCommand::Start(input) => {
            let result = caravan::repair::start(&context, input);
            if cli.json {
                return emit_result(true, result);
            }
            match result {
                Ok(output) => {
                    println!(
                        "repair {}: PR #{} {} -> {} in {}",
                        output.repair.session,
                        output.repair.pr,
                        output.repair.head.oid,
                        output.repair.target.oid,
                        output.repair.workspace
                    );
                    if output.repair.conflicting_paths.is_empty() {
                        println!("  merge prepared without textual conflicts");
                    } else {
                        println!(
                            "  resolve and stage: {}",
                            output.repair.conflicting_paths.join(", ")
                        );
                    }
                    println!("  {}", output.next);
                    Ok(())
                }
                Err(error) => emit_human_error(error),
            }
        }
        RepairCommand::AuthorizeAgentEdits(input) => {
            let result = caravan::repair::authorize_agent_edits(&context, input);
            if cli.json {
                return emit_result(true, result);
            }
            match result {
                Ok(output) => {
                    println!(
                        "repair {} authorized agent edits: actor={} expires={} provider_mutated={}",
                        output.repair.session,
                        output.authorization.actor,
                        output.authorization.expires_unix_ms,
                        output.provider_mutated
                    );
                    println!("  {}", output.next);
                    Ok(())
                }
                Err(error) => emit_human_error(error),
            }
        }
        RepairCommand::Grant(input) => {
            let result = caravan::repair::grant_paths(&context, input);
            if cli.json {
                return emit_result(true, result);
            }
            match result {
                Ok(output) => {
                    println!(
                        "repair {} semantic grants: {}",
                        output.repair.session,
                        output
                            .grants
                            .iter()
                            .map(|grant| format!("{}@{}", grant.path, grant.source_revision))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    println!("  {}", output.next);
                    Ok(())
                }
                Err(error) => emit_human_error(error),
            }
        }
        RepairCommand::RevokeGrant(input) => {
            let result = caravan::repair::revoke_grants(&context, input);
            if cli.json {
                return emit_result(true, result);
            }
            match result {
                Ok(output) => {
                    println!(
                        "repair {} revoked semantic grants: {} provider_mutated={}",
                        output.repair.session,
                        output.revoked_paths.join(", "),
                        output.provider_mutated
                    );
                    Ok(())
                }
                Err(error) => emit_human_error(error),
            }
        }
        RepairCommand::Continue(input) => {
            let result = caravan::repair::continue_session(&context, input);
            if cli.json {
                return emit_result(true, result);
            }
            match result {
                Ok(output) => {
                    if let Some(receipt) = &output.publication {
                        println!(
                            "repair {} published {} -> {} by non-force fast-forward",
                            output.repair.session, receipt.old_head, receipt.new_head
                        );
                    }
                    if output.sync.is_some() {
                        println!("  sync-all resumed and converged");
                    }
                    println!("  {}", output.next);
                    Ok(())
                }
                Err(error) => emit_human_error(error),
            }
        }
        RepairCommand::Status(input) => {
            let result = caravan::repair::status(&context, input);
            if cli.json {
                return emit_result(true, result);
            }
            match result {
                Ok(repair) => {
                    println!(
                        "repair {}: {:?}/{:?} PR #{} {} -> {} workspace={} materialization_timeout={}s",
                        repair.session,
                        repair.state,
                        repair.phase,
                        repair.pr,
                        repair.head.oid,
                        repair.target.oid,
                        repair.workspace,
                        repair.materialization_timeout_secs,
                    );
                    if !repair.conflicting_paths.is_empty() {
                        println!("  conflicts: {}", repair.conflicting_paths.join(", "));
                    }
                    if !repair.semantic_grants.is_empty() {
                        println!(
                            "  semantic grants: {}",
                            repair
                                .semantic_grants
                                .iter()
                                .map(|grant| format!(
                                    "{}@{} actor={} expires={} applied={}",
                                    grant.path,
                                    grant.source_revision,
                                    grant.actor,
                                    grant.expires_unix_ms,
                                    grant.applied
                                ))
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    if let Some(authorization) = &repair.agent_edit_authorization {
                        println!(
                            "  agent edits: actor={} expires={} reason={}",
                            authorization.actor,
                            authorization.expires_unix_ms,
                            authorization.reason
                        );
                    }
                    if let Some(receipt) = &repair.agent_edit_receipt {
                        println!(
                            "  verified agent diff: paths={} path_fp={} diff_fp={} bytes={} fresh_ci_required={}",
                            receipt.path_count,
                            receipt.path_fingerprint,
                            receipt.diff_fingerprint,
                            receipt.diff_bytes,
                            receipt.fresh_ci_required
                        );
                    }
                    if let Some(error) = &repair.last_error {
                        println!(
                            "  last error: {} phase={:?} elapsed={}ms budget={}ms partial={} — {}",
                            error.code,
                            error.phase,
                            error.elapsed_ms,
                            error.timeout_ms,
                            error.partial_path,
                            error.next,
                        );
                    }
                    Ok(())
                }
                Err(error) => emit_human_error(error),
            }
        }
        RepairCommand::Abort(input) => {
            let result = caravan::repair::abort(&context, input);
            if cli.json {
                return emit_result(true, result);
            }
            match result {
                Ok(output) => {
                    println!(
                        "repair {} aborted: PR #{} local workspace removed={} provider_mutated={}",
                        output.session,
                        output.pr,
                        output.workspace_removed,
                        output.provider_mutated
                    );
                    Ok(())
                }
                Err(error) => emit_human_error(error),
            }
        }
    }
}

fn run_lock(cli: &Cli, command: &LockCommand) -> Result<(), i32> {
    let context = load_context(cli)?;
    match command {
        LockCommand::Status(input) => emit_result(
            cli.json,
            caravan::operation_lock::inspect_lock(
                &context.repository_path,
                std::time::Duration::from_secs(input.stale_after_secs),
            ),
        ),
        LockCommand::Recover(input) => {
            let result = if input.confirm {
                caravan::operation_lock::recover_stale_lock(
                    &context.repository_path,
                    std::time::Duration::from_secs(input.stale_after_secs),
                    &input.token,
                )
            } else {
                Err(AppError::validation(
                    "operation_lock_recovery_confirmation_required",
                    "pass --confirm only after reviewing `cara lock status` evidence",
                ))
            };
            emit_result(cli.json, result)
        }
    }
}

fn run_loop(cli: &Cli, input: &LoopInput) -> Result<(), i32> {
    if input.manual {
        if cli.json {
            return emit_result::<serde_json::Value, _>(
                true,
                Err(AppError::validation(
                    "manual_loop_json_unsupported",
                    "manual decision mode requires an interactive human terminal and is not a JSON/MCP surface",
                )),
            );
        }
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return emit_human_error(AppError::validation(
                "manual_loop_tty_required",
                "manual decision mode requires a controlling TTY",
            ));
        }
        let context = load_context(cli)?;
        return run_manual_loop(&context, input);
    }
    if cli.json && !input.once {
        return emit_result::<serde_json::Value, _>(
            true,
            Err(AppError::validation(
                "unbounded_json_loop_unsupported",
                "use `cara loop --once --json`; the unbounded foreground loop streams human progress only",
            )),
        );
    }
    let context = load_context(cli)?;
    if cli.json {
        return emit_result(
            true,
            caravan::loop_runner::run(&context, input, |_| {}, |_, _| {}),
        );
    }
    let outcome = caravan::sync::progress::observing(stream_sync_progress, || {
        caravan::loop_runner::run(
            &context,
            input,
            |tick| {
                print!("{}", render_loop_tick(tick));
            },
            |error, failure| {
                eprintln!("{}", render_loop_failure(error, failure));
            },
        )
    });
    match outcome {
        Ok(output) => {
            if output.stopped_by_signal {
                println!(
                    "loop stopped after {} tick(s) ({} failed)",
                    output.ticks, output.failed_ticks
                );
            }
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

/// Stream one bounded sync stage line to the terminal.
fn stream_sync_progress(event: &caravan::sync::progress::SyncProgressEvent) {
    use std::io::Write as _;
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "{} {}: {}", dim(".."), event.phase, event.detail);
    let _ = stderr.flush();
}

fn render_loop_failure(
    error: &AppError,
    failure: &caravan::loop_runner::LoopTickFailure,
) -> String {
    let mut rendered = format!(
        "tick {} failed: {} — {}\n  disposition={} wake={} retryable={}\n  next: {}",
        failure.tick,
        failure.code,
        failure.message,
        failure.disposition.as_deref().unwrap_or("unknown"),
        failure.wake_class.as_deref().unwrap_or("unknown"),
        failure.retryable,
        failure.next,
    );
    if !failure.hook_deliveries.is_empty() {
        let _ = write!(
            rendered,
            "\n  hook deliveries: {}",
            failure.hook_deliveries.len()
        );
    }
    if let Some(hook_error) = &failure.hook_error {
        let _ = write!(rendered, "\n  hook delivery failed: {hook_error}");
    }
    if let Some(evidence) = human_error_evidence(error) {
        let _ = write!(rendered, "\n{evidence}");
    }
    rendered
}

fn run_manual_loop(context: &AppContext, input: &LoopInput) -> Result<(), i32> {
    let interval = Duration::from_secs(
        input
            .interval_secs
            .unwrap_or(context.config.loop_config.interval_secs),
    );
    if interval.is_zero() {
        return emit_human_error(AppError::validation(
            "invalid_loop_interval",
            "loop interval must be at least one second",
        ));
    }
    loop {
        match caravan::sync::progress::observing(stream_sync_progress, || {
            caravan::loop_runner::tick(context)
        }) {
            Ok(output) => {
                print!("{}", render_loop_tick(&output));
                if input.once {
                    return Ok(());
                }
                std::thread::sleep(interval);
            }
            Err(error) if manual_external_decision(&error) => {
                if let Err(error) = run_manual_decision_shell(context, input, &error) {
                    return emit_human_error(error);
                }
                // Shell success is not resolution authority. Rediscover and
                // rerun the exact tick immediately.
            }
            Err(error) if input.once => return emit_human_error(error),
            Err(error) => {
                // A failed tick is evidence, not a stop condition: rediscover
                // and retry so provider races and moved default branches
                // converge without restarting the foreground loop.
                eprintln!(
                    "{} {}: {}",
                    failure("cara"),
                    heading(mcp_cli::StructuredError::code(&error)),
                    mcp_cli::StructuredError::message(&error)
                );
                if let Some(evidence) = human_error_evidence(&error) {
                    eprintln!("{evidence}");
                }
                std::thread::sleep(interval);
            }
        }
    }
}

fn manual_external_decision(error: &AppError) -> bool {
    error
        .details()
        .and_then(|details| {
            details
                .get("scheduler_status")?
                .get("wake_class")?
                .as_str()
                .map(str::to_owned)
        })
        .as_deref()
        == Some("external_decision")
}

fn run_manual_decision_shell(
    context: &AppContext,
    input: &LoopInput,
    error: &AppError,
) -> Result<(), AppError> {
    let (decision_file, common_dir) = persist_manual_decision(context, error)?;
    let working_directory = manual_decision_working_directory(context, error, &common_dir);
    let command = input.shell.clone().unwrap_or_else(|| {
        format!(
            "{} -i",
            std::env::var("SHELL").unwrap_or_else(|_| "sh".to_owned())
        )
    });
    eprintln!(
        "{} external decision {}\n  evidence: {}\n  cwd: {}\n  shell: {}",
        heading("MANUAL"),
        error.code(),
        decision_file.display(),
        working_directory.display(),
        command
    );
    let details = error.details().unwrap_or(serde_json::Value::Null);
    let event_id = find_string_field(&details, "event_id").unwrap_or_default();
    let operation_id = find_string_field(&details, "operation_id").unwrap_or_default();
    let repair_session = find_string_field(&details, "session").unwrap_or_default();
    let prs = collect_pr_numbers(&details)
        .into_iter()
        .map(|pr| pr.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let status = std::process::Command::new("sh")
        .args(["-lc", &command])
        .current_dir(&working_directory)
        .env("CARA_DECISION_FILE", &decision_file)
        .env("CARA_DECISION_CODE", error.code())
        .env("CARA_REPOSITORY_PATH", &context.repository_path)
        .env("CARA_EVENT_ID", event_id)
        .env("CARA_OPERATION_ID", operation_id)
        .env("CARA_PRS", prs)
        .env("CARA_REPAIR_SESSION", repair_session)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|spawn| {
            AppError::validation(
                "manual_decision_shell_failed",
                format!("could not launch manual decision shell: {spawn}"),
            )
        })?;
    if !status.success() {
        return Err(AppError::validation(
            "manual_decision_shell_exit",
            format!(
                "manual shell exited with {}; evidence remains at {}",
                status
                    .code()
                    .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                decision_file.display()
            ),
        ));
    }
    Ok(())
}

fn persist_manual_decision(
    context: &AppContext,
    error: &AppError,
) -> Result<(PathBuf, PathBuf), AppError> {
    let common = git_stdout(context, &["rev-parse", "--git-common-dir"])?;
    let common = if Path::new(&common).is_absolute() {
        PathBuf::from(common)
    } else {
        context.repository_path.join(common)
    };
    let common = common
        .canonicalize()
        .map_err(|io| AppError::validation("manual_decision_state_failed", io.to_string()))?;
    let directory = common.join("caravan/manual-decisions");
    fs::create_dir_all(&directory)
        .map_err(|io| AppError::validation("manual_decision_state_failed", io.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .map_err(|io| AppError::validation("manual_decision_state_failed", io.to_string()))?;
    }
    let id = uuid::Uuid::now_v7().to_string();
    let path = directory.join(format!("decision-{id}.json"));
    let payload = serde_json::json!({
        "schema_version": 1,
        "decision_id": id,
        "created_unix_ms": SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX)),
        "repository_path": context.repository_path,
        "error": {
            "category": error.category(),
            "code": error.code(),
            "message": error.message(),
            "details": error.details(),
        }
    });
    let encoded = serde_json::to_vec_pretty(&payload).map_err(|encode| {
        AppError::validation("manual_decision_state_failed", encode.to_string())
    })?;
    if encoded.len() > 1024 * 1024 {
        return Err(AppError::validation(
            "manual_decision_evidence_too_large",
            "manual decision evidence exceeds the one-megabyte bound",
        ));
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .map_err(|io| AppError::validation("manual_decision_state_failed", io.to_string()))?;
        file.write_all(&encoded)
            .map_err(|io| AppError::validation("manual_decision_state_failed", io.to_string()))?;
    }
    #[cfg(not(unix))]
    fs::write(&path, encoded)
        .map_err(|io| AppError::validation("manual_decision_state_failed", io.to_string()))?;
    prune_manual_decisions(&directory, 20);
    Ok((path, common))
}

fn prune_manual_decisions(directory: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut files = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|(modified, _)| *modified);
    let remove = files.len().saturating_sub(keep);
    for (_, path) in files.into_iter().take(remove) {
        let _ = fs::remove_file(path);
    }
}

fn manual_decision_working_directory(
    context: &AppContext,
    error: &AppError,
    common_dir: &Path,
) -> PathBuf {
    let candidate = error
        .details()
        .as_ref()
        .and_then(find_workspace_path)
        .and_then(|path| PathBuf::from(path).canonicalize().ok());
    candidate
        .filter(|path| path.starts_with(&context.repository_path) || path.starts_with(common_dir))
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| context.repository_path.clone())
}

fn find_string_field(value: &serde_json::Value, field: &str) -> Option<String> {
    match value {
        serde_json::Value::Object(object) => object
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                object
                    .values()
                    .find_map(|value| find_string_field(value, field))
            }),
        serde_json::Value::Array(values) => values
            .iter()
            .find_map(|value| find_string_field(value, field)),
        _ => None,
    }
}

fn collect_pr_numbers(value: &serde_json::Value) -> std::collections::BTreeSet<u64> {
    fn visit(value: &serde_json::Value, output: &mut std::collections::BTreeSet<u64>) {
        match value {
            serde_json::Value::Object(object) => {
                if let Some(pr) = object.get("pr").and_then(serde_json::Value::as_u64) {
                    if output.len() < 64 {
                        output.insert(pr);
                    }
                }
                if let Some(prs) = object.get("prs").and_then(serde_json::Value::as_array) {
                    for pr in prs.iter().filter_map(serde_json::Value::as_u64) {
                        if output.len() < 64 {
                            output.insert(pr);
                        }
                    }
                }
                for child in object.values() {
                    visit(child, output);
                }
            }
            serde_json::Value::Array(values) => {
                for child in values {
                    visit(child, output);
                }
            }
            _ => {}
        }
    }
    let mut output = std::collections::BTreeSet::new();
    visit(value, &mut output);
    output
}

fn find_workspace_path(value: &serde_json::Value) -> Option<&str> {
    match value {
        serde_json::Value::Object(object) => {
            if let Some(path) = object.get("workspace").and_then(serde_json::Value::as_str) {
                return Some(path);
            }
            object.values().find_map(find_workspace_path)
        }
        serde_json::Value::Array(values) => values.iter().find_map(find_workspace_path),
        _ => None,
    }
}

fn render_loop_tick(output: &caravan::loop_runner::LoopTickOutput) -> String {
    let mut text = render_sync(&output.sync);
    let ready_deliveries = output
        .hook_deliveries
        .get(output.sync.hook_deliveries.len()..)
        .unwrap_or_default();
    append_hook_deliveries(&mut text, ready_deliveries);
    text
}

#[allow(clippy::too_many_lines)]
fn terminal_output() -> bool {
    !cfg!(test)
        && io::stdout().is_terminal()
        && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
}

fn color_output() -> bool {
    terminal_output() && std::env::var_os("NO_COLOR").is_none()
}

fn styled(code: &str, value: impl AsRef<str>) -> String {
    if color_output() {
        format!("\x1b[{code}m{}\x1b[0m", value.as_ref())
    } else {
        value.as_ref().to_owned()
    }
}

fn heading(value: impl AsRef<str>) -> String {
    styled("1;36", value)
}

fn success(value: impl AsRef<str>) -> String {
    styled("1;32", value)
}

fn warning(value: impl AsRef<str>) -> String {
    styled("1;33", value)
}

fn failure(value: impl AsRef<str>) -> String {
    styled("1;31", value)
}

fn dim(value: impl AsRef<str>) -> String {
    styled("2", value)
}

/// Render one fleet problem row.
///
/// A fleet problem is repository-wide: the same row is emitted for every
/// candidate a `cara check`/`sync` run inspects, regardless of which PR
/// actually carries the defect. Naming the exact PRs keeps a single foreign
/// candidate's problem from reading as a defect in the candidate the operator
/// asked about (Cacophony bd-e4dbcb).
fn render_fleet_problem(problem: &caravan::model::GraphProblem) -> String {
    let prs = problem
        .prs
        .iter()
        .map(|pr| format!("#{pr}"))
        .collect::<Vec<_>>()
        .join(",");
    if prs.is_empty() {
        format!("{:?}: {}", problem.kind, problem.message)
    } else {
        format!("{:?} [{prs}]: {}", problem.kind, problem.message)
    }
}

fn pr_link(number: caravan::model::PrNumber, title: &str, url: &str) -> String {
    let label = format!("#{number} {title}");
    if terminal_output() && !url.is_empty() {
        format!("\x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\")
    } else {
        label
    }
}

#[allow(clippy::too_many_lines)]
fn render_sync_plan(output: &caravan::sync::SyncPlanOutput) -> String {
    let mut text = format!(
        "{}  {}  repository {}  plan {}\n",
        heading("PLAN SYNC"),
        success("NO PROVIDER WRITES"),
        output.repository,
        output.plan_hash
    );
    if let Some(refusal) = &output.tick_refusal {
        // Stated before the steps, because the steps are what it invalidates.
        let _ = writeln!(
            text,
            "  {} a real tick would REFUSE to start: {refusal}",
            failure("!"),
        );
    }
    let _ = writeln!(
        text,
        "  default {}@{} · caravans {} · github reads {}",
        output.default_branch.name,
        output.default_branch.oid,
        output.selected_caravans.len(),
        output.github_requests_used
    );
    for action in &output.actions {
        let subject = action
            .pr
            .map_or_else(|| "fleet".to_owned(), |pr| format!("PR #{pr}"));
        let _ = writeln!(
            text,
            "  {:>2}. {:?}/{:?} {} {} — {}",
            action.order, action.phase, action.state, subject, action.kind, action.reason
        );
    }
    let _ = writeln!(
        text,
        "  auto-admission: enabled={} candidate={} tail={} continuation={}",
        output.auto_admission.enabled,
        output
            .auto_admission
            .candidate_pr
            .map_or_else(|| "none".to_owned(), |pr| format!("#{pr}")),
        output
            .auto_admission
            .target_tail
            .map_or_else(|| "none".to_owned(), |pr| format!("#{pr}")),
        output.auto_admission.continuation,
    );
    for decision in &output.decisions {
        let _ = writeln!(
            text,
            "  {} {} — {}; next: {}",
            warning("DECISION"),
            decision.code,
            decision.reason,
            decision.next
        );
    }
    text
}

#[allow(clippy::too_many_lines)]
fn render_sync(output: &caravan::sync::SyncOutput) -> String {
    let caravans = output
        .synchronized_caravans
        .iter()
        .map(|number| format!("#{number}"))
        .collect::<Vec<_>>()
        .join(", ");
    let state = if output.receipt.changed {
        success("changed")
    } else {
        success("already converged")
    };
    // bd-180cd3: one compact, greppable line per pass, first, so "the loop is
    // running and declining to join" is never mistaken for "the loop is not
    // running". A rich multi-line render is useless if you are asking whether a
    // tick happened at all.
    let mut text = format!(
        "{} verb={} caravans={} unqueued={} synchronized={} joins={} changed={}\n",
        dim("tick:"),
        output.tick.verb,
        output.tick.caravans,
        output.tick.unqueued,
        output.tick.synchronized,
        output.tick.joins,
        output.tick.changed,
    );
    let _ = writeln!(
        text,
        "{}  {state}  {}",
        heading("SYNC"),
        if caravans.is_empty() {
            dim("no caravans")
        } else {
            styled("1;35", format!("caravans {caravans}"))
        }
    );
    let _ = writeln!(
        text,
        "  scheduler: {:?} wake={:?} rebase_on_join={} — {}",
        output.scheduler_status.disposition,
        output.scheduler_status.wake_class,
        output.scheduler_status.rebase_on_join,
        output.scheduler_status.reason,
    );
    for stall in &output.scheduler_status.head_of_line {
        let _ = writeln!(
            text,
            "  {} {:?} at position {} blocks {} PR(s): #{} — {}",
            warning("head-of-line:"),
            stall.kind,
            stall.position,
            stall.blocked_prs.len(),
            stall.blocking_pr,
            stall.evidence,
        );
        for remedy in &stall.remedies {
            let _ = writeln!(text, "    remedy: {remedy}");
        }
    }
    let _ = writeln!(
        text,
        "  auto-admission: enabled={} heuristic={} continuation={:?} considered={} joins={} skips={} mutations={}/{} github={}/{} remaining={}",
        output.auto_admission.enabled,
        output.auto_admission.heuristic_version,
        output.auto_admission.continuation,
        output.auto_admission.candidates_considered,
        output.auto_admission.joins.len(),
        output.auto_admission.skips.len(),
        output.auto_admission.mutations_used,
        output.auto_admission.mutation_limit,
        output.auto_admission.github_requests_used,
        output.auto_admission.github_request_limit,
        output.auto_admission.remaining_candidates.len(),
    );
    if !output.scheduler_status.waiting_prs.is_empty() {
        let waiting = output
            .scheduler_status
            .waiting_prs
            .iter()
            .map(|number| format!("#{number}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(text, "  scheduler waiting CI: {waiting}");
    }
    if let Some(predecessor) = output.historical_predecessor {
        let _ = writeln!(text, "  selected from merged predecessor #{predecessor}");
    }
    if let Some(timing) = &output.timing {
        let _ = writeln!(
            text,
            "  timing: {}ms total (status {}ms + provider {}ms + verify {}ms; deadline {}ms)",
            timing.total_ms,
            timing.initial_status_ms,
            timing.provider_convergence_ms,
            timing.final_status_ms,
            timing.deadline_ms,
        );
    }
    if let Some(recovery) = &output.lock_recovery {
        let _ = writeln!(
            text,
            "  recovered dead {:?} lock owner pid {} (token verified)",
            recovery.removed_owner.operation, recovery.removed_owner.pid
        );
    }
    for pause in &output.paused_caravans {
        let _ = writeln!(
            text,
            "  paused #{}: {:?}; {}",
            pause.record.caravan_head, pause.state, pause.safe_next_action
        );
    }
    for advancement in &output.head_advancements {
        let _ = writeln!(
            text,
            "  head advanced: #{} -> #{}",
            advancement.merged_predecessor, advancement.new_head
        );
    }
    for receipt in &output.rebase_receipts {
        let _ = writeln!(
            text,
            "  rebase #{} {}: {} -> {} onto {} ({})",
            receipt.pr,
            receipt.branch,
            receipt.old_head_oid,
            receipt.new_head_oid,
            receipt.new_base_oid,
            receipt.lease
        );
    }
    for receipt in &output.root_auto_merge {
        let _ = writeln!(
            text,
            "  root auto-merge #{} {}@{}: {} by {} ({})",
            receipt.pr,
            receipt.head.name,
            receipt.head.oid,
            if receipt.provenance.engine_armed {
                "armed"
            } else {
                "already armed"
            },
            receipt.provenance.owner,
            receipt.provenance.reason
        );
    }
    for observation in &output.ci {
        // bd-eff1dc: report current rows against total, so a reader can see that
        // a long check list is mostly superseded lineage rather than a wall of
        // current failures.
        let (current, superseded) = caravan::model::latest_checks_per_identity(&observation.checks);
        let lineage = if superseded.is_empty() {
            String::new()
        } else {
            format!(", {} superseded", superseded.len())
        };
        let _ = writeln!(
            text,
            "  CI #{}: {:?} ({} checks{}, {} failed runs)",
            observation.pr,
            observation.disposition,
            current.len(),
            lineage,
            observation.failed_runs.len()
        );
    }
    for problem in &output.scheduler_status.missing_required_runs {
        let _ = writeln!(
            text,
            "  {} #{} {}@{}: [{}]\n    {}\n    next: {}",
            if problem.operator_action_required {
                failure(problem.kind.code())
            } else {
                dim(problem.kind.code())
            },
            problem.pr,
            problem.head.name,
            problem.head.oid,
            problem.contexts.join(", "),
            problem.message,
            problem.next
        );
    }
    for step in &output.receipt.completed_steps {
        let _ = writeln!(text, "  {:?} {:?}: {}", step.kind, step.state, step.summary);
    }
    if !output.events.is_empty() {
        let _ = writeln!(text, "  audit events: {}", output.events.len());
    }
    append_hook_deliveries(&mut text, &output.hook_deliveries);
    text
}

fn render_membership(output: &caravan::membership::MembershipOutput) -> String {
    let state = if output.receipt.changed {
        success("changed")
    } else {
        dim("already satisfied")
    };
    let mut text = format!(
        "{}  {}  in {}  {state}\n",
        heading(output.receipt.operation.clone().to_uppercase()),
        pr_link(
            output.pull_request.number,
            &output.pull_request.title,
            &output.pull_request.url,
        ),
        styled("1;35", format!("caravan #{}", output.caravan_id)),
    );
    if !output.coexisting_caravans.is_empty() {
        let ids = output
            .coexisting_caravans
            .iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(", ");
        let tail = output
            .coexisting_caravans
            .first()
            .map_or_else(String::new, |id| format!(" --tail-pr {id}"));
        let _ = writeln!(
            text,
            "  {} separate caravan created; {ids} already existed. `cara join --pr {}{tail}` would have stacked instead",
            dim("note:"),
            output.pull_request.number,
        );
    }
    if let Some(join) = &output.join_receipt {
        let _ = writeln!(
            text,
            "  atomic join v{}: #{} -> #{} at {} (force={:?}, ancestry={}, durable={})",
            join.schema_version,
            join.candidate_pr,
            join.predecessor.pr,
            join.predecessor.head_oid,
            join.force_intent,
            join.ancestry_verified,
            join.membership_durable
        );
    }
    for step in &output.receipt.completed_steps {
        let _ = writeln!(text, "  {:?} {:?}: {}", step.kind, step.state, step.summary);
    }
    append_provider_identity(&mut text, &output.provider_api);
    append_hook_deliveries(&mut text, &output.hook_deliveries);
    text
}

fn append_provider_identity(text: &mut String, telemetry: &caravan::model::GitHubApiTelemetry) {
    let Some(source) = telemetry.auth_source.as_deref() else {
        return;
    };
    let app = telemetry.github_app_slug.as_deref().unwrap_or("-");
    let installation = telemetry
        .github_app_installation_id
        .map_or_else(|| "-".to_owned(), |id| id.to_string());
    let transport = telemetry.github_app_git_transport.as_deref().unwrap_or("-");
    let repository = telemetry
        .github_app_git_repository
        .as_deref()
        .unwrap_or("-");
    let _ = writeln!(
        text,
        "  provider: {source} app={app} installation={installation} git={transport} repository={repository}"
    );
}

fn append_hook_deliveries(text: &mut String, deliveries: &[caravan::hooks::HookDelivery]) {
    for delivery in deliveries {
        let _ = writeln!(
            text,
            "  hook {:?} {:?} (blocking={})",
            delivery.kind, delivery.state, delivery.blocking
        );
    }
}

#[allow(clippy::too_many_lines)]
fn render_status(output: &caravan::read::StatusOutput) -> String {
    let mut text = String::new();
    let health = if output.healthy {
        success("healthy")
    } else {
        failure("needs attention")
    };
    let _ = writeln!(
        text,
        "{}  {} @ {}  {health}",
        heading("CARAVAN"),
        output.repository,
        output.default_branch,
    );
    // Only speak up when the effective policy is genuinely one branch's
    // proposal. Announcing provenance on every healthy run would be noise, and
    // noise is how the stale-config failure went unnoticed until it bricked a
    // session.
    if let Some(provenance) = output
        .config_provenance
        .as_ref()
        .filter(|provenance| provenance.is_branch_local_proposal())
    {
        // A stale checkout is louder than a deliberate proposal: the operator's
        // current policy was never read at all (bd-6f234e).
        let label = if provenance.is_stale_policy() {
            "STALE CONFIG:"
        } else {
            "config:"
        };
        let _ = writeln!(text, "  {} {}", warning(label), provenance.reason);
    }
    let current = output
        .current_pr
        .map_or_else(|| "no open PR".to_owned(), |number| format!("PR #{number}"));
    let _ = writeln!(
        text,
        "current: {} ({current})",
        output.current_branch.as_deref().unwrap_or("detached HEAD")
    );
    let _ = writeln!(
        text,
        "initialization: {}{}",
        if output.initialization.ready {
            "ready"
        } else {
            "not ready"
        },
        output
            .initialization
            .next
            .as_ref()
            .map_or_else(String::new, |next| format!(" — {next}"))
    );
    let _ = writeln!(text, "rebase_on_join={}", output.rebase_on_join.state);
    let _ = writeln!(
        text,
        "stack_backend={} capability={} mutations={} native_stacks={} missing_caravans={} truncated={}",
        output.stack_backend.configured.code(),
        output.stack_backend.capability.code(),
        output.stack_backend.mutation_support.code(),
        output.stack_backend.native_stacks.len(),
        output.stack_backend.missing_caravans.len(),
        output.stack_backend.provider_stacks_truncated,
    );
    if let Some(blocker) = &output.initialization.mutation_blocker {
        let _ = writeln!(
            text,
            "  mutation blocker: {} — {} — {}",
            blocker.code, blocker.message, blocker.next
        );
    }
    for native in &output.stack_backend.native_stacks {
        let _ = writeln!(
            text,
            "  native Stack #{} consistency={} caravan={} entries={}",
            native.stack.number,
            native.consistency.code(),
            native
                .caravan_id
                .map_or_else(|| "none".to_owned(), |id| id.to_string()),
            native.stack.pull_requests.len(),
        );
        for problem in &native.problems {
            let _ = writeln!(
                text,
                "    stack problem: {} — {}",
                problem.code, problem.message
            );
        }
    }
    for problem in &output.stack_backend.problems {
        let _ = writeln!(
            text,
            "  stack problem: {} — {}",
            problem.code, problem.message
        );
    }
    let rate = output.provider_api.rate_limit.as_ref().map_or_else(
        || "rate=unavailable".to_owned(),
        |rate| {
            format!(
                "rate={} remaining, cost={}, reset={}",
                rate.remaining, rate.cost, rate.reset_at
            )
        },
    );
    let _ = writeln!(
        text,
        "github: {} calls (graphql={}, rest={}, gh-cli={}) auth={} cache_hits={} cache_age_ms={} {}",
        output.provider_api.calls,
        output.provider_api.graphql_calls,
        output.provider_api.rest_calls,
        output.provider_api.gh_cli_calls,
        output
            .provider_api
            .auth_source
            .as_deref()
            .unwrap_or("unknown"),
        output.provider_api.cache_hits,
        output
            .provider_api
            .cache_age_ms
            .map_or_else(|| "n/a".to_owned(), |age| age.to_string()),
        rate,
    );
    if let Some(action) = &output.rebase_on_join.required_action {
        let _ = writeln!(text, "  action: {action}");
    }
    let _ = writeln!(
        text,
        "auto-admission={} heuristic={} caravans={}/{} active ({} parked, {} excess, at_capacity={}) candidates={} mutations={} github={} duration={}s",
        output.auto_admission.enabled,
        output.auto_admission.heuristic_version,
        output.auto_admission.active_caravans,
        output.auto_admission.max_caravans,
        output.auto_admission.parked_caravans,
        output.auto_admission.excess_active_caravans,
        output.auto_admission.at_caravan_capacity,
        output.auto_admission.max_candidates_per_tick,
        output.auto_admission.max_mutations_per_tick,
        output.auto_admission.max_github_requests_per_tick,
        output.auto_admission.max_duration_secs,
    );
    if output.auto_admission.at_caravan_capacity {
        let active = output
            .auto_admission
            .active_caravan_ids
            .iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(",");
        let blocked = output
            .auto_admission
            .first_blocked_root_candidate
            .map_or_else(|| "none".to_owned(), |pr| format!("#{pr}"));
        let _ = writeln!(
            text,
            "  max_caravans reached: active=[{active}] first_root_candidate={blocked}"
        );
    }
    let history = &output.analysis.fleet.history;
    let parked_count = output
        .analysis
        .fleet
        .caravans
        .iter()
        .filter(|caravan| caravan.parked)
        .count();
    let active_count = output
        .analysis
        .fleet
        .caravans
        .len()
        .saturating_sub(parked_count);
    let _ = writeln!(
        text,
        "\n{}  {}",
        heading("CARAVANS"),
        dim(format!("{active_count} active, {parked_count} parked now"))
    );
    // "Now" and "ever" are different questions. An empty live list was read as
    // "no caravan has ever formed" on a repository that had merged 23 members
    // over eleven days, and re-measuring only ever confirmed the correct value
    // (bd-8c9916). State the historical answer beside the live one.
    if output.analysis.fleet.caravans.is_empty() {
        let _ = writeln!(
            text,
            "  {}",
            if history.has_formed_before() {
                dim(format!(
                    "none in flight now; this is not a lifetime claim — {} merged caravan member(s) observed{}",
                    history.merged_members_observed,
                    history
                        .latest_merged_at
                        .as_deref()
                        .map_or_else(String::new, |latest| format!(", latest {latest}"))
                ))
            } else if history.evidence_may_be_stripped() {
                warning(format!(
                    "none in flight now, and history is UNPROVEN rather than empty: {} merged row(s) came back from the label-filtered query carrying no caravan label, so the evidence was removed",
                    history.unlabelled_merged_rows
                ))
            } else {
                dim(
                    "none in flight now, and no merged caravan member within the bounded discovery window",
                )
            }
        );
    }
    if let Some(previous) = &output.previous_default_oid {
        let _ = writeln!(text, "previous observed default: {previous}");
    }
    if !output.default_branch_movements.is_empty() {
        let _ = writeln!(text, "recent default-branch movements:");
        for movement in &output.default_branch_movements {
            let actor = movement.actor.as_deref().unwrap_or("unknown");
            let source = movement
                .source_pr
                .map_or_else(|| "no PR".to_owned(), |pr| format!("PR #{pr}"));
            let _ = writeln!(
                text,
                "  {} {} actor={} source={} {:?}",
                movement.oid, movement.timestamp, actor, source, movement.ownership
            );
        }
    }
    for pause in &output.pauses {
        let _ = writeln!(
            text,
            "  paused #{}: {:?} — {}",
            pause.record.caravan_head, pause.state, pause.safe_next_action
        );
    }
    for caravan in &output.analysis.fleet.caravans {
        let chain = caravan
            .members
            .iter()
            .map(|number| {
                output.analysis.pull_requests.get(number).map_or_else(
                    || format!("#{number}"),
                    |pull| pr_link(*number, &pull.title, &pull.url),
                )
            })
            .collect::<Vec<_>>()
            .join(&dim("  →  "));
        let state = if caravan.parked { " parked-red" } else { "" };
        let _ = writeln!(
            text,
            "  {}  {chain}",
            if caravan.parked {
                warning(format!("van #{}{state}", caravan.id))
            } else {
                styled("1;35", format!("van #{}", caravan.id))
            }
        );
        if let Some(projection) = output
            .sync_budget
            .caravans
            .iter()
            .find(|projection| projection.caravan_id == caravan.id)
            && output.sync_budget.rebase_on_join
            && (projection.deferred_convergence || projection.at_capacity)
        {
            let _ = writeln!(
                text,
                "    {} apply reserve {}ms vs {}ms deadline; prefix {}/{} member(s), {} deferred, capacity {} — {}",
                if projection.at_capacity {
                    failure("at capacity")
                } else {
                    dim("bounded prefix")
                },
                projection.required_ms,
                output.sync_budget.deadline_ms,
                projection.processable_prefix.len(),
                projection.members.len(),
                projection.deferred.len(),
                output
                    .sync_budget
                    .max_admissible_members
                    .map_or_else(|| "unsound".to_owned(), |bound| bound.to_string()),
                projection.safe_next_action,
            );
        }
    }
    if let Some(defect) = &output.sync_budget.capacity_defect {
        let _ = writeln!(
            text,
            "! {}: the configured deadline implies a {}-member admission bound, below the {}-member floor; draining cannot repair it — {}",
            failure(&defect.code),
            defect.computed_bound,
            defect.minimum_sound_bound,
            defect.safe_next_action,
        );
    }
    if let Some(blocked) = output.sync_budget.blocked_candidate {
        let _ = writeln!(
            text,
            "! caravan_budget_capacity_exhausted: #{blocked} cannot join — {}",
            output.sync_budget.safe_next_action
        );
    }
    if output.merge_candidates_truncated > 0 {
        let _ = writeln!(
            text,
            "! merge-candidate evidence truncated: {} additional active members",
            output.merge_candidates_truncated
        );
    }
    for candidate in &output.merge_candidates {
        let synthetic = candidate.synthetic.as_ref().map_or_else(
            || "unavailable".to_owned(),
            |merge| {
                format!(
                    "{} tree={} parents=[{}]",
                    merge.oid,
                    merge.tree_oid,
                    merge
                        .parents
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                )
            },
        );
        let owner = candidate.auto_merge.actor.as_deref().unwrap_or("unknown");
        let _ = writeln!(
            text,
            "  candidate #{}: {:?} base={}@{} head={}@{} synthetic={} auto-merge={} owner={} updated={}",
            candidate.pr,
            candidate.freshness,
            candidate.base.name,
            candidate.base.oid,
            candidate.head.name,
            candidate.head.oid,
            synthetic,
            candidate.auto_merge.enabled,
            owner,
            candidate.provider_updated_at,
        );
        for reason in &candidate.stale_reasons {
            let _ = writeln!(text, "    ! {reason}");
        }
    }
    if !output.admission.candidates.is_empty() {
        let _ = writeln!(text, "\n{}", heading("WAITING AT THE RAIL"));
        for candidate in &output.admission.candidates {
            let label = output
                .analysis
                .pull_requests
                .get(&candidate.pr)
                .map_or_else(
                    || format!("#{}", candidate.pr),
                    |pull| pr_link(candidate.pr, &pull.title, &pull.url),
                );
            let _ = writeln!(text, "  {label}\n    {}", dim(&candidate.reason));
        }
    }
    for skipped in &output.admission.skipped {
        let _ = writeln!(text, "~ admission #{}: {}", skipped.pr, skipped.reason);
    }
    for rejected in &output.admission.rejected {
        let _ = writeln!(
            text,
            "{} admission #{}: {}",
            warning("!"),
            rejected.pr,
            rejected.reason
        );
    }
    for problem in &output.analysis.fleet.problems {
        let _ = writeln!(text, "{} {}", failure("!"), render_fleet_problem(problem));
    }
    text
}

fn render_show(output: &caravan::read::ShowOutput) -> String {
    let mut text = format!(
        "caravan #{} — {}\n",
        output.caravan.id,
        if output.healthy {
            "healthy"
        } else {
            "needs attention"
        }
    );
    if let Some(predecessor) = output.historical_predecessor {
        let _ = writeln!(
            text,
            "~ merged predecessor #{predecessor} -> active successor #{} (position {})",
            output.current_pr, output.position
        );
    }
    for pull_request in &output.pull_requests {
        let marker = if pull_request.number == output.current_pr {
            ">"
        } else {
            " "
        };
        let _ = writeln!(
            text,
            "{marker} #{} {} [{} -> {}]{}",
            pull_request.number,
            pull_request.title,
            pull_request.head.name,
            pull_request.base.name,
            if pull_request.auto_merge.enabled {
                " auto-merge"
            } else {
                ""
            }
        );
    }
    for candidate in &output.merge_candidates {
        let synthetic = candidate.synthetic.as_ref().map_or_else(
            || "unavailable".to_owned(),
            |merge| {
                format!(
                    "{} parents=[{}]",
                    merge.oid,
                    merge
                        .parents
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                )
            },
        );
        let _ = writeln!(
            text,
            "  lineage #{}: {:?} base={}@{} head={}@{} synthetic={}",
            candidate.pr,
            candidate.freshness,
            candidate.base.name,
            candidate.base.oid,
            candidate.head.name,
            candidate.head.oid,
            synthetic,
        );
    }
    for problem in &output.problems {
        let _ = writeln!(text, "! {}", render_fleet_problem(problem));
    }
    text
}

/// Render the typed intent-aware admission decision bound to a check receipt.
fn render_admission_intent(intent: &caravan::admission::AdmissionIntentDecision) -> String {
    let prs = |numbers: &[caravan::model::PrNumber]| {
        numbers
            .iter()
            .map(|pr| format!("#{pr}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let mut text = String::new();
    let _ = writeln!(
        text,
        "  admission selection={} intent={} order={:?} target_caravan={} bypassed_unjoined=[{}] blocked_by=[{}] provider_mutated={} idempotent={}",
        intent.selection.name(),
        intent.intent.name(),
        intent.outcome,
        intent
            .target_caravan
            .map_or_else(|| "none".to_owned(), |id| format!("#{id}")),
        prs(&intent.bypassed_unjoined_prs),
        prs(&intent.blocking_prs),
        intent.provider_mutated,
        intent.idempotent,
    );
    let _ = writeln!(text, "    {}", intent.reason);
    text
}

fn render_check(output: &caravan::read::CheckOutput) -> String {
    let eligibility = if output.eligible {
        success("eligible")
    } else {
        failure("not eligible")
    };
    let mut text = format!(
        "{}  {}  {eligibility}  {}\n  {}@{}  →  {}@{}\n  next: {:?}",
        heading("CHECK"),
        pr_link(
            output.current_pr,
            &output.candidate.title,
            &output.candidate.url,
        ),
        dim(format!("{:?}", output.mode)),
        output.candidate.head.name,
        output.candidate.head.oid,
        output.candidate.base.name,
        output.candidate.base.oid,
        output.next_action,
    );
    if let Some(target) = output.target_pr {
        let _ = write!(text, " after #{target}");
    }
    text.push('\n');
    let _ = writeln!(text, "rebase_on_join={}", output.rebase_on_join.state);
    if let Some(action) = &output.rebase_on_join.required_action {
        let _ = writeln!(text, "  action: {action}");
    }
    let _ = writeln!(
        text,
        "  head_repository={} head_repository_owner={} same_repository={} draft={} labels=[{}] auto_merge={} enrolled={} canonical={}",
        output.candidate.head.repository,
        output.head_repository_owner,
        !output.candidate.cross_repository,
        output.candidate.draft,
        output
            .candidate
            .labels
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(","),
        output.candidate.auto_merge.enabled,
        output.enrolled,
        output.canonical_candidate,
    );
    if let Some(identity) = &output.merge_candidate {
        let _ = writeln!(
            text,
            "  provider identity: {:?} base={}@{} head={}@{} stale_base={} stale_head={}",
            identity.freshness,
            identity.base.name,
            identity.base.oid,
            identity.head.name,
            identity.head.oid,
            identity.stale_base,
            identity.stale_head,
        );
    }
    if let Some(intent) = &output.admission_intent {
        text.push_str(&render_admission_intent(intent));
    }
    if let Some(note) = &output.admission_note {
        let _ = writeln!(text, "  note: {note}");
    }
    for report in &output.compatibility {
        let _ = writeln!(
            text,
            "  {:?}: {}@{} -> {}@{} conflicts=[{}]",
            report.outcome,
            report.candidate.name,
            report.candidate.oid,
            report.target.name,
            report.target.oid,
            report.conflicting_paths.join(","),
        );
    }
    for reconciliation in &output.squash_reconciliations {
        let _ = writeln!(
            text,
            "  squash-equivalence {}: {}@{} -> {}@{} dropped=[{}] paths=[{}]",
            reconciliation.outcome.name(),
            reconciliation.candidate.name,
            reconciliation.candidate_oid,
            reconciliation.target.name,
            reconciliation.target_oid,
            reconciliation
                .dropped_commits()
                .iter()
                .map(|oid| oid.0.clone())
                .collect::<Vec<_>>()
                .join(","),
            reconciliation.affected_paths().join(","),
        );
        let _ = writeln!(text, "    {}", reconciliation.reason);
    }
    for problem in &output.problems {
        let _ = writeln!(text, "! {}", render_fleet_problem(problem));
    }
    text
}

fn emit_human_error<E>(error: E) -> Result<(), i32>
where
    E: StructuredError + std::fmt::Display,
{
    emit_result::<serde_json::Value, E>(false, Err(error))
}

fn run_evict(cli: &Cli, input: &EvictInput) -> Result<(), i32> {
    if input.reason.trim().is_empty() {
        let error = AppError::validation(
            "eviction_reason_required",
            "eviction requires a non-empty --reason",
        );
        return if cli.json {
            emit_result::<serde_json::Value, _>(true, Err(error))
        } else {
            emit_human_error(error)
        };
    }
    let context = load_context(cli)?;
    let result = caravan::reshape::evict(&context, input);
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            println!(
                "evicted PR #{}; affected {:?}; changed={}",
                output.pr, output.affected_prs, output.receipt.changed
            );
            // bd-cef612: retargeting does not remove the evicted commits from
            // members that were physically rebased onto it.
            if !output.descendants_inheriting_evicted_patch.is_empty() {
                println!(
                    "! {:?} still contain PR #{}'s commits and would reintroduce them when they land",
                    output.descendants_inheriting_evicted_patch, output.pr
                );
                println!(
                    "  unwind them before landing: rebase each onto the surviving predecessor, or evict them too with `cara evict --cascade --pr {}`",
                    output.pr
                );
            }
            let mut hooks = String::new();
            append_provider_identity(&mut hooks, &output.provider_api);
            append_hook_deliveries(&mut hooks, &output.hook_deliveries);
            print!("{hooks}");
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_split(cli: &Cli, input: &SplitInput) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = caravan::reshape::split(&context, input);
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            println!(
                "split at PR #{}; caravans={}; changed={}",
                output.pr,
                output.resulting_fleet.caravans.len(),
                output.receipt.changed
            );
            let mut hooks = String::new();
            append_provider_identity(&mut hooks, &output.provider_api);
            append_hook_deliveries(&mut hooks, &output.hook_deliveries);
            print!("{hooks}");
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_navigation(
    cli: &Cli,
    scope: caravan::navigation::Scope,
    direction: caravan::navigation::Direction,
) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = caravan::navigation::navigate(&context, scope, direction);
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            println!(
                "checked out PR #{} on `{}` ({})",
                output.to_pr, output.branch, output.oid
            );
            if let Some(reconciliation) = output.local_branch_reconciliation {
                println!(
                    "preserved stale local {} at `{}`; advanced {} -> {}",
                    reconciliation.previous_oid,
                    reconciliation.backup_ref,
                    reconciliation.previous_oid,
                    reconciliation.provider_oid,
                );
            }
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_van_list(cli: &Cli) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = caravan::navigation::list(&context);
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            if output.caravans.is_empty() {
                println!("no caravans");
            } else {
                for caravan in output.caravans {
                    let chain = caravan
                        .members
                        .iter()
                        .map(|number| format!("#{number}"))
                        .collect::<Vec<_>>()
                        .join(" -> ");
                    println!("#{}: {chain}", caravan.id);
                }
            }
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_help(json: bool) -> Result<(), i32> {
    if json {
        return emit_result::<_, AppError>(json, Ok(caravan::help()));
    }
    println!("{AGENT_HELP}");
    Ok(())
}

fn run_mcp(command: &McpCommand, config_path: Option<&std::path::Path>) -> Result<(), i32> {
    let server = McpServer::new(
        StdioServerConfig {
            server_name: TOOL_NAME.to_owned(),
            server_version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        build_router(),
    );
    match command {
        McpCommand::Tools => {
            serde_json::to_writer_pretty(io::stdout().lock(), &server.tool_metadata())
                .map_err(|_| 1)?;
            println!();
            Ok(())
        }
        McpCommand::Stdio => {
            let context = AppContext::load(config_path).map_err(|error| {
                eprintln!("cara: {error}");
                2
            })?;
            server.serve_stdio(&context).map_err(|error| {
                eprintln!("mcp error: {error}");
                1
            })
        }
    }
}

fn run_self_update(json: bool, command: &SelfUpdateCommand) -> Result<(), i32> {
    match command {
        SelfUpdateCommand::Status => emit_result(json, self_update_status()),
        SelfUpdateCommand::Check => emit_result(json, self_update_check()),
        SelfUpdateCommand::Run => emit_result(json, self_update_run()),
    }
}

fn feedback_event(args: &FeedbackArgs) -> FeedbackEvent {
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
    if let Some(fingerprint) = &args.fingerprint {
        event = event.with_fingerprint(fingerprint.clone());
    }
    event
}

fn run_feedback(json: bool, command: &FeedbackCommand) -> Result<(), i32> {
    match command {
        FeedbackCommand::Status => emit_result::<_, AppError>(json, Ok(caravan::feedback_status())),
        FeedbackCommand::Report(args) => {
            let event = feedback_event(args);
            let config = feedback_config();
            if let Some(error) = feedback_configuration_error(&config) {
                return emit_result::<feedback_cli::ReportReceipt, _>(json, Err(error));
            }
            let reporter = Reporter::from_config(&config);
            emit_result(
                json,
                reporter
                    .report(&event)
                    .map(|()| feedback_cli::ReportReceipt {
                        reported: reporter.is_enabled(),
                        destination: reporter.destination(),
                    }),
            )
        }
    }
}

/// Dense, human-first summary of the fields Caravan payloads share.
///
/// Rendered before any "see --json" fallback so the terminal always answers
/// *which PR* and *why*, even when the full evidence is far too large to show.
fn digest_lines(details: &serde_json::Value) -> String {
    let mut lines = Vec::new();
    if let Some(prs) = details["prs"].as_array().filter(|prs| !prs.is_empty()) {
        let rendered = prs
            .iter()
            .filter_map(serde_json::Value::as_u64)
            .map(|pr| format!("#{pr}"))
            .collect::<Vec<_>>()
            .join(", ");
        if !rendered.is_empty() {
            lines.push(format!("  prs:    {rendered}"));
        }
    } else if let Some(pr) = details["pr"].as_u64() {
        lines.push(format!("  pr:     #{pr}"));
    }
    for (label, key) in [
        ("kind", "kind"),
        ("status", "status"),
        ("cause", "cause_code"),
        ("reason", "reason"),
        ("detail", "message"),
        ("next", "next"),
    ] {
        if let Some(value) = details[key].as_str().filter(|value| !value.is_empty()) {
            // One dense line each; the full text stays available under --json.
            let trimmed: String = value.chars().take(160).collect();
            lines.push(format!("  {label:<7} {trimmed}"));
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("{}\n", lines.join("\n"))
}

fn short_oid(value: &str) -> &str {
    value.get(..value.len().min(9)).unwrap_or(value)
}

#[allow(clippy::too_many_lines)]
fn human_error_evidence(error: &impl StructuredError) -> Option<String> {
    let details = error.details()?;
    let compact = match error.code().as_str() {
        "join_root_moved_before_apply" => Some(format!(
            "{} #{} changed: {}\n  expected head/base: {} / {}\n  actual head/base:   {} / {}\n  {}",
            warning("Retryable root drift"),
            details["root_pr"],
            details["changed_fields"].as_array().map_or_else(
                || "unknown fields".to_owned(),
                |items| items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            details["expected"]["head"]["oid"]
                .as_str()
                .map_or("unknown", short_oid),
            details["expected"]["base"]["oid"]
                .as_str()
                .map_or("unknown", short_oid),
            details["actual"]["head"]["oid"]
                .as_str()
                .map_or("unknown", short_oid),
            details["actual"]["base"]["oid"]
                .as_str()
                .map_or("unknown", short_oid),
            details["safe_next_action"]
                .as_str()
                .unwrap_or("rediscover and retry the same join"),
        )),
        "rebase_topology_changed" if details.get("source_commit_count").is_some() => Some(format!(
            "{}: source {} → rebuilt {} (dropped {}, added {})\n  source commits: {}\n  rebuilt commits: {}\n  {}",
            warning("Topology replay changed commit count"),
            details["source_commit_count"],
            details["rebuilt_commit_count"],
            details["dropped_commit_count"],
            details["added_commit_count"],
            details["source_commits"].as_array().map_or_else(
                || "unknown".to_owned(),
                |items| items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(short_oid)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            details["rebuilt_commits"].as_array().map_or_else(
                || "none".to_owned(),
                |items| items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(short_oid)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            details["safe_next_action"]
                .as_str()
                .unwrap_or("inspect and rebase the source, then retry"),
        )),
        "physical_sync_budget_insufficient" => Some(format!(
            "{}: {}ms required, {}ms remaining ({} command slots)\n  {}",
            warning("Physical sync stopped before mutation"),
            details["required_ms"],
            details["remaining_ms"],
            details["required_command_slots"],
            details["config_guidance"]
                .as_str()
                .unwrap_or("increase the bounded sync duration and retry"),
        )),
        "rebase_midpoint_head_stale" | "rebase_midpoint_pr_missing" => Some(format!(
            "{}: PR #{} branch {}\n  expected head: {}\n  observed head: {}\n  {}\n  {}",
            warning("Provider has not exposed the exact pushed generation yet"),
            details["receipt"]["pr"],
            details["receipt"]["branch"].as_str().unwrap_or("unknown"),
            details["receipt"]["new_head_oid"]
                .as_str()
                .map_or("unknown", short_oid),
            details["observed_head"]
                .as_str()
                .map_or("absent from discovery", short_oid),
            details["auto_merge_state"].as_str().unwrap_or(
                "head auto-merge stays intentionally disabled until fresh CI is revalidated"
            ),
            details["safe_next_action"]
                .as_str()
                .unwrap_or("rerun the same idempotent sync"),
        )),
        "rebase_stale_lease" => Some(format!(
            "{}: branch {}\n  expected: {}\n  actual:   {}\n  {}",
            warning("Remote generation moved during this tick"),
            details["branch"].as_str().unwrap_or("unknown"),
            details["expected_oid"]
                .as_str()
                .map_or("unknown", short_oid),
            details["actual_oid"].as_str().map_or("unknown", short_oid),
            details["next"]
                .as_str()
                .unwrap_or("rediscover provider state and rerun `cara sync --all`"),
        )),
        "join_empty_source_noop" => Some(format!(
            "{}: {}@{} has no effective patch beyond current main\n  No provider or branch mutation was attempted.",
            success("No-op"),
            details["source"]["branch"].as_str().unwrap_or("source"),
            details["source"]["head_oid"]
                .as_str()
                .map_or("unknown", short_oid),
        )),
        _ => None,
    };
    if compact.is_some() {
        return compact;
    }
    let encoded = serde_json::to_string_pretty(&details).unwrap_or_else(|_| details.to_string());
    if encoded.len() <= 4_096 {
        Some(encoded)
    } else {
        // A payload too large to print is exactly when a reader most needs to
        // know WHICH pull request and WHY. Dropping straight to "too large" and
        // a byte count tells them nothing they can act on, so pull the fields
        // every Caravan payload shares before deferring to --json.
        Some(format!(
            "{}{}\n  Re-run with --json for the complete structured evidence ({} bytes).",
            digest_lines(&details),
            dim("Remaining evidence is too large for the human terminal view."),
            encoded.len()
        ))
    }
}

fn emit_result<T, E>(json: bool, result: Result<T, E>) -> Result<(), i32>
where
    T: Serialize,
    E: StructuredError + std::fmt::Display,
{
    if json {
        let failed = result.is_err();
        write_json_result(io::stdout().lock(), result).map_err(|_| 1)?;
        return if failed { Err(1) } else { Ok(()) };
    }
    match result {
        Ok(output) => {
            serde_json::to_writer_pretty(io::stdout().lock(), &output).map_err(|_| 1)?;
            println!();
            Ok(())
        }
        Err(error) => {
            eprintln!(
                "{} {}: {}",
                failure("cara"),
                heading(error.code()),
                error.message()
            );
            if let Some(evidence) = human_error_evidence(&error) {
                eprintln!("{evidence}");
            }
            Err(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)] // Keep the full PID/PGID/SID handshake and receipt proof together.
    fn status_watchdog_reaps_descendants_without_waiting_for_output_eof() {
        #[cfg(target_os = "linux")]
        let _ = rustix::process::set_child_subreaper(Some(rustix::process::Pid::INIT));
        let directory = tempfile::tempdir().expect("temporary directory");
        let ready = directory.path().join("descendant-ready");
        let stdout = std::fs::File::create(directory.path().join("stdout")).unwrap();
        let stderr = std::fs::File::create(directory.path().join("stderr")).unwrap();
        // Reproduce the real shape: a worker owns a session and waits on a
        // provider in its own process group. The provider ignores TERM, retains
        // inherited descriptors, and handshakes its exact identities.
        let provider_fixture = r#"import os, signal, sys, time
os.setpgid(0, 0)
signal.signal(signal.SIGTERM, signal.SIG_IGN)
with open(sys.argv[1], "w") as ready:
    ready.write(f"{os.getpid()} {os.getpgrp()} {os.getsid(0)} {sys.argv[2]}\n")
    ready.flush()
    os.fsync(ready.fileno())
while True:
    time.sleep(1)
"#;
        let worker_fixture = r#"import os, subprocess, sys
os.setsid()
provider = subprocess.Popen(
    [sys.executable, "-c", os.environ["CARA_WATCHDOG_PROVIDER"], sys.argv[1], str(os.getpid())],
    close_fds=False,
)
provider.wait()
"#;
        let ready_argument = ready.to_string_lossy().into_owned();
        let mut command = ProcessCommand::new("python3");
        command
            .args(["-c", worker_fixture, ready_argument.as_str()])
            .env("CARA_WATCHDOG_PROVIDER", provider_fixture)
            .env("CARA_WATCHDOG_READY", &ready)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        let mut child = command.spawn().expect("spawn watchdog fixture");
        // This is a causal file handshake, not a readiness sleep. Full-suite
        // process pressure can delay Python startup; the bound only prevents a
        // broken fixture from waiting forever and is separate from reap timing.
        let ready_deadline = std::time::Instant::now() + Duration::from_secs(10);
        let (descendant_pid, descendant_group, descendant_session, worker_pid) = loop {
            if let Ok(contents) = std::fs::read_to_string(&ready) {
                let mut fields = contents.split_whitespace();
                if let (Some(pid), Some(group), Some(session), Some(worker)) =
                    (fields.next(), fields.next(), fields.next(), fields.next())
                {
                    if let (Ok(pid), Ok(group), Ok(session), Ok(worker)) = (
                        pid.parse::<u32>(),
                        group.parse::<u32>(),
                        session.parse::<u32>(),
                        worker.parse::<u32>(),
                    ) {
                        break (pid, group, session, worker);
                    }
                }
            }
            assert!(
                !matches!(child.try_wait(), Ok(Some(_))),
                "fixture worker exited before the PID/process-group handshake"
            );
            assert!(
                std::time::Instant::now() < ready_deadline,
                "descendant never completed the PID/process-group handshake within the bounded startup allowance"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(worker_pid, child.id());
        assert_ne!(descendant_pid, child.id());
        assert_eq!(descendant_group, descendant_pid);
        assert_eq!(descendant_session, child.id());
        let process_is_alive = |pid: u32| {
            ProcessCommand::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        };
        assert!(process_is_alive(descendant_pid));
        let registered = capture_status_identities(child.id());
        let provider_registered = registered
            .iter()
            .find(|identity| identity.pid == descendant_pid);
        assert!(
            provider_registered.is_some(),
            "provider {descendant_pid} was absent from pre-teardown registration"
        );
        #[cfg(target_os = "linux")]
        if let Some(provider_registered) = provider_registered {
            assert_eq!(
                provider_registered.pidfd_receipt(),
                PidfdAcquisition::Acquired
            );
        }
        drop(registered);
        let started = std::time::Instant::now();
        let cleanup = terminate_status_tree(&mut child);
        assert!(started.elapsed() < Duration::from_secs(2));
        #[cfg(target_os = "linux")]
        assert!(cleanup.subreaper_enabled);
        let provider_receipt = cleanup
            .targets
            .iter()
            .find(|target| target.pid == descendant_pid);
        assert!(
            provider_receipt.is_some(),
            "missing provider receipt: {cleanup:?}"
        );
        if let Some(provider_receipt) = provider_receipt {
            #[cfg(target_os = "linux")]
            assert_eq!(provider_receipt.pidfd, PidfdAcquisition::Acquired);
            assert_ne!(provider_receipt.term, SignalOutcome::NotAttempted);
            assert_ne!(provider_receipt.kill, SignalOutcome::NotAttempted);
            assert_ne!(provider_receipt.reap, ReapOutcome::NotAttempted);
            assert!(!provider_receipt.survived, "{cleanup:?}");
        }
        assert!(
            !process_is_alive(descendant_pid),
            "descendant {descendant_pid} in independent group {descendant_group} and session {descendant_session} survived the watchdog reap"
        );
        assert!(
            session_pids(descendant_session).is_empty(),
            "watchdog session {descendant_session} retained a process after envelope-safe cleanup"
        );
    }

    #[test]
    fn feedback_report_preserves_the_receiver_dedupe_fingerprint() {
        let event = feedback_event(&FeedbackArgs {
            kind: FeedbackKindArg::Error,
            component: Some("hook-acceptance".to_owned()),
            summary: "environmental hook acceptance failed".to_owned(),
            detail: Some("bounded evidence".to_owned()),
            severity: Some(SeverityArg::Error),
            fingerprint: Some("caravan-hook-acceptance-v1:x86_64:runner".to_owned()),
        });

        assert_eq!(
            event.fingerprint.as_deref(),
            Some("caravan-hook-acceptance-v1:x86_64:runner")
        );
        assert_eq!(event.component, "hook-acceptance");
        assert_eq!(event.detail.as_deref(), Some("bounded evidence"));
    }

    /// A repository-wide problem row names the exact PRs that carry it, so a
    /// single foreign candidate's defect is not read as a defect in every
    /// candidate a run happens to inspect (Cacophony bd-e4dbcb).
    #[test]
    fn fleet_problem_rows_name_their_exact_prs() {
        let row = render_fleet_problem(&caravan::model::GraphProblem {
            kind: caravan::model::GraphProblemKind::InvalidGenerationMetadata,
            prs: vec![caravan::model::PrNumber(2215)],
            message: "missing exact generation metadata: Beads".to_owned(),
        });
        assert_eq!(
            row,
            "InvalidGenerationMetadata [#2215]: missing exact generation metadata: Beads"
        );

        let unattributed = render_fleet_problem(&caravan::model::GraphProblem {
            kind: caravan::model::GraphProblemKind::InvalidGenerationMetadata,
            prs: Vec::new(),
            message: "missing exact generation metadata: Beads".to_owned(),
        });
        assert_eq!(
            unattributed,
            "InvalidGenerationMetadata: missing exact generation metadata: Beads"
        );
    }

    #[derive(Debug)]
    struct HumanTestError {
        code: &'static str,
        details: serde_json::Value,
    }

    impl std::fmt::Display for HumanTestError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(self.code)
        }
    }

    impl mcp_cli::StructuredError for HumanTestError {
        fn category(&self) -> mcp_cli::ErrorCategory {
            mcp_cli::ErrorCategory::Validation
        }
        fn code(&self) -> String {
            self.code.to_owned()
        }
        fn message(&self) -> String {
            self.code.to_owned()
        }
        fn details(&self) -> Option<serde_json::Value> {
            Some(self.details.clone())
        }
    }

    #[test]
    fn human_root_drift_evidence_is_compact_and_actionable() {
        let evidence = human_error_evidence(&HumanTestError {
            code: "join_root_moved_before_apply",
            details: serde_json::json!({
                "root_pr": 2086,
                "changed_fields": ["head"],
                "expected": {"head":{"oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"base":{"oid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"checks":[{"huge":"ignored"}]},
                "actual": {"head":{"oid":"cccccccccccccccccccccccccccccccccccccccc"},"base":{"oid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"checks":[{"huge":"ignored"}]},
                "safe_next_action": "rediscover and retry the same join"
            }),
        })
        .unwrap();
        assert!(evidence.contains("Retryable root drift"));
        assert!(evidence.contains("head"));
        assert!(evidence.contains("rediscover and retry"));
        assert!(!evidence.contains("huge"));
    }

    #[test]
    fn human_topology_evidence_explains_commit_pruning() {
        let evidence = human_error_evidence(&HumanTestError {
            code: "rebase_topology_changed",
            details: serde_json::json!({
                "source_commit_count": 2,
                "rebuilt_commit_count": 1,
                "dropped_commit_count": 1,
                "added_commit_count": 0,
                "source_commits": ["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"],
                "rebuilt_commits": ["cccccccccccccccccccccccccccccccccccccccc"],
                "safe_next_action": "rebase the source and retry"
            }),
        })
        .unwrap();
        assert!(evidence.contains("source 2 → rebuilt 1"));
        assert!(evidence.contains("dropped 1"));
        assert!(evidence.contains("rebase the source"));
        assert!(!evidence.contains("aaaaaaaaaaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn oversized_generic_human_evidence_is_not_dumped() {
        let evidence = human_error_evidence(&HumanTestError {
            code: "large_fixture",
            details: serde_json::json!({"payload": "x".repeat(10_000)}),
        })
        .unwrap();
        assert!(evidence.contains("too large"));
        assert!(evidence.contains("--json"));
        assert!(evidence.len() < 300);
    }

    #[test]
    fn command_tree_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn config_check_is_an_offline_subcommand() {
        let cli = Cli::try_parse_from([
            "cara",
            "--config",
            "tests/fixtures/config-v0.0.7.yaml",
            "config",
            "check",
        ])
        .expect("config check parses");
        assert!(matches!(cli.command, Command::Config(ConfigCommand::Check)));
    }

    #[test]
    fn new_accepts_remote_candidate_without_using_server_checkout_branch() {
        let cli = Cli::try_parse_from(["cara", "new", "--pr", "43"])
            .expect("remote root admission parses");
        let Command::New(input) = cli.command else {
            panic!("expected new");
        };
        assert_eq!(input.pr, Some(43));
        assert!(!input.create_pr);
        assert!(Cli::try_parse_from(["cara", "new", "--pr", "43", "--create-pr"]).is_err());
    }

    #[test]
    fn join_accepts_remote_candidate_and_explicit_tail() {
        let cli = Cli::try_parse_from(["cara", "join", "--pr", "43", "--tail-pr", "42"])
            .expect("remote atomic join parses");
        let Command::Join(input) = cli.command else {
            panic!("expected join");
        };
        assert_eq!(input.pr, Some(43));
        assert_eq!(input.tail_pr, Some(42));
        assert_eq!(input.head_pr, None);
        assert!(!input.create_pr);
        assert!(Cli::try_parse_from(["cara", "join", "--pr", "43", "--create-pr"]).is_err());
    }

    #[test]
    fn force_arm_and_revoke_parse_with_explicit_identity() {
        let arm = Cli::try_parse_from([
            "cara",
            "force",
            "--pr",
            "42",
            "--actor",
            "operator",
            "--reason",
            "accept known failure",
        ])
        .expect("force arm parses");
        let Command::Force(command) = arm.command else {
            panic!("expected force");
        };
        let (input, revoke) = command.operation().unwrap();
        assert_eq!(input.pr, 42);
        assert!(!revoke);

        let revoke = Cli::try_parse_from([
            "cara",
            "force",
            "revoke",
            "--pr",
            "42",
            "--actor",
            "operator",
            "--reason",
            "intent no longer applies",
        ])
        .expect("force revoke parses");
        let Command::Force(command) = revoke.command else {
            panic!("expected force revoke");
        };
        let (input, revoke) = command.operation().unwrap();
        assert_eq!(input.pr, 42);
        assert!(revoke);
    }

    #[test]
    fn reviewed_force_intent_contract_parses_exact_caco_arguments() {
        for action in ["preview", "apply", "revoke"] {
            let cli = Cli::try_parse_from([
                "cara",
                "--json",
                "force-intent",
                action,
                "--pr",
                "2120",
                "--head",
                "4b5ddd6a9f1d61599d68ccd05e5c831dba1fc239",
                "--membership-generation",
                "fnv1a64:1111111111111111",
                "--failure-fingerprint",
                "fnv1a64:2222222222222222",
                "--reason",
                "known provider control-plane failure",
                "--expires-at-ms",
                "9999999999999",
                "--auto-merge",
                "squash",
            ])
            .unwrap_or_else(|error| panic!("{action} must parse: {error}"));
            let Command::ForceIntent(command) = cli.command else {
                panic!("expected reviewed force-intent {action}");
            };
            let input = match command {
                ReviewedForceIntentCommand::Preview(input)
                | ReviewedForceIntentCommand::Apply(input)
                | ReviewedForceIntentCommand::Revoke(input) => input,
            };
            assert_eq!(input.pr, 2120);
            assert_eq!(input.head, "4b5ddd6a9f1d61599d68ccd05e5c831dba1fc239");
            assert_eq!(
                input.auto_merge,
                caravan::force_intent::ReviewedAutoMerge::Squash
            );
        }
    }

    #[test]
    fn priority_set_and_clear_require_exact_audit_inputs() {
        let set = Cli::try_parse_from([
            "cara",
            "priority",
            "set",
            "--pr",
            "43",
            "--label",
            "caravan-priority:high",
            "--actor",
            "operator",
            "--reason",
            "urgent",
        ])
        .expect("priority set parses");
        let Command::Priority(PriorityCommand::Set(input)) = set.command else {
            panic!("expected priority set");
        };
        assert_eq!(input.pr, 43);
        assert_eq!(input.label, "caravan-priority:high");

        let clear = Cli::try_parse_from([
            "cara", "priority", "clear", "--pr", "43", "--actor", "operator", "--reason", "FIFO",
        ])
        .expect("priority clear parses");
        assert!(matches!(
            clear.command,
            Command::Priority(PriorityCommand::Clear(_))
        ));
        assert!(Cli::try_parse_from(["cara", "priority", "clear", "--pr", "43"]).is_err());
    }

    #[test]
    fn plan_sync_parses_as_read_only_nested_command() {
        let cli = Cli::try_parse_from(["cara", "plan", "sync", "--all", "--rerun-failed"])
            .expect("plan sync parses");
        let Command::Plan(PlanCommand::Sync(input)) = cli.command else {
            panic!("expected plan sync");
        };
        assert!(input.all);
        assert!(input.rerun_failed);
    }

    #[test]
    fn web_requires_explicit_repository_paths() {
        let cli = Cli::try_parse_from([
            "cara",
            "web",
            "--repo",
            "/tmp/one",
            "--repo",
            "/tmp/two",
            "--listen",
            "127.0.0.1:4888",
            "--poll-seconds",
            "30",
            "--read-only",
        ])
        .expect("web arguments parse");
        let Command::Web(input) = cli.command else {
            panic!("expected web command");
        };
        assert_eq!(input.repositories.len(), 2);
        assert_eq!(input.listen.port(), 4888);
        assert_eq!(input.poll_seconds, 30);
        assert!(input.read_only);
        assert!(Cli::try_parse_from(["cara", "web"]).is_err());

        let cli = Cli::try_parse_from([
            "cara",
            "web",
            "--repo",
            "/tmp/one",
            "--github-webhook-secret-env",
            "CARA_GITHUB_WEBHOOK_SECRET",
            "--github-installation-id",
            "42",
            "--webhook-sync",
        ])
        .expect("webhook arguments parse");
        let Command::Web(input) = cli.command else {
            panic!("expected web command");
        };
        assert_eq!(input.github_installation_id, Some(42));
        assert!(input.webhook_sync);
    }

    #[test]
    fn check_rejects_both_target_forms() {
        assert!(
            Cli::try_parse_from(["cara", "check", "--tail-pr", "1", "--head-pr", "2"]).is_err()
        );
    }

    #[test]
    fn check_accepts_remote_candidate_and_tail_without_checkout() {
        let cli = Cli::try_parse_from(["cara", "check", "--pr", "41", "--tail-pr", "40"])
            .expect("remote candidate preflight parses");
        let Command::Check(input) = cli.command else {
            panic!("expected check");
        };
        assert_eq!(input.pr, Some(41));
        assert_eq!(input.tail_pr, Some(40));
    }

    #[test]
    fn repair_commands_require_exact_session_and_pr_inputs() {
        let start = Cli::try_parse_from([
            "cara",
            "repair",
            "start",
            "--pr",
            "1962",
            "--target-pr",
            "1972",
        ])
        .expect("repair start parses");
        let Command::Repair(RepairCommand::Start(input)) = start.command else {
            panic!("expected repair start");
        };
        assert_eq!(input.pr, 1962);
        assert_eq!(input.target_pr, Some(1972));

        let authorization = Cli::try_parse_from([
            "cara",
            "repair",
            "authorize-agent-edits",
            "--session",
            "pr-1962-deadbeef",
            "--actor",
            "caco-merger",
            "--reason",
            "repair CI",
        ])
        .expect("repair agent authorization parses");
        let Command::Repair(RepairCommand::AuthorizeAgentEdits(input)) = authorization.command
        else {
            panic!("expected repair agent authorization");
        };
        assert_eq!(input.actor, "caco-merger");

        let grant = Cli::try_parse_from([
            "cara",
            "repair",
            "grant",
            "--session",
            "pr-1962-deadbeef",
            "--path",
            "README.md",
            "--path",
            "SPEC.md",
            "--source-revision",
            "c915e23100000000000000000000000000000000",
            "--actor",
            "operator",
            "--reason",
            "restore reviewed shell-safe contracts",
        ])
        .expect("repair grant parses");
        let Command::Repair(RepairCommand::Grant(input)) = grant.command else {
            panic!("expected repair grant");
        };
        assert_eq!(input.paths, ["README.md", "SPEC.md"]);
        assert_eq!(input.expires_secs, 3600);

        let continuation = Cli::try_parse_from([
            "cara",
            "repair",
            "continue",
            "--session",
            "pr-1962-deadbeef",
            "--actor",
            "caco-merger",
        ])
        .expect("repair continue parses");
        let Command::Repair(RepairCommand::Continue(input)) = continuation.command else {
            panic!("expected repair continue");
        };
        assert_eq!(input.session, "pr-1962-deadbeef");
        assert_eq!(input.actor.as_deref(), Some("caco-merger"));
        assert!(!input.no_sync);
    }

    #[test]
    fn human_status_is_compact_instead_of_dumping_nested_json() {
        let repository = caravan::model::RepositoryId {
            owner: "harryaskham".to_owned(),
            name: "caravan".to_owned(),
        };
        let default_branch = caravan::model::BranchSnapshot {
            repository: repository.clone(),
            name: "main".to_owned(),
            oid: caravan::model::CommitOid("0".repeat(40)),
        };
        let output = caravan::read::StatusOutput {
            config_provenance: None,
            head_merge: caravan::read::HeadMergeStatus::default(),
            runtime: caravan::read::RuntimeProvenance::default(),
            provider_api: caravan::model::GitHubApiTelemetry::default(),
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            timing: None,
            repository: repository.clone(),
            rebase_on_join: caravan::read::RebaseOnJoinStatus::default(),
            stack_backend: caravan::read::StackBackendStatus::default(),
            auto_admission: caravan::read::AutoAdmissionStatus::default(),
            default_branch: "main".to_owned(),
            current_branch: Some("feature".to_owned()),
            current_pr: None,
            healthy: true,
            initialization: caravan::initialization::InitializationStatus::default(),
            admission: caravan::read::AdmissionStatus {
                policy: "priority then FIFO".to_owned(),
                priority_labels: Vec::new(),
                generation_integrity: caravan::generation::GenerationIntegrityStatus::default(),
                candidates: Vec::new(),
                skipped: Vec::new(),
                rejected: Vec::new(),
                next_candidate: None,
            },
            analysis: caravan::graph::GraphAnalysis {
                fleet: caravan::model::CaravanFleet {
                    repository,
                    default_branch,
                    caravans: Vec::new(),
                    unqueued: Vec::new(),
                    problems: Vec::new(),
                    history: caravan::model::CaravanHistory::default(),
                },
                pull_requests: std::collections::BTreeMap::new(),
                compatibility: Vec::new(),
                cumulative_trees: Vec::new(),
                squash_reconciliations: Vec::new(),
            },
            pauses: Vec::new(),
            sync_budget: caravan::sync::SyncBudgetStatus::default(),
        };
        let rendered = render_status(&output);
        assert!(rendered.contains("CARAVAN  harryaskham/caravan @ main  healthy"));
        assert!(rendered.contains("current: feature (no open PR)"));
        assert!(rendered.contains("rebase_on_join=disabled"));
        assert!(rendered.contains("stack_backend=caravan capability=not_probed mutations=caravan"));
        assert!(rendered.contains("set `rebase_on_join: true`"));
        assert!(!rendered.contains("\"analysis\""));
    }

    #[test]
    fn manual_decision_file_is_private_and_bounded_to_git_state() {
        let directory = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(directory.path())
            .status()
            .unwrap();
        assert!(status.success());
        let context = caravan::AppContext {
            repository_path: directory.path().canonicalize().unwrap(),
            config_path: PathBuf::from(".caravan/config.yaml"),
            config_existed: false,
            config: caravan::config::CaravanConfig::default(),
        };
        let error = AppError::validation("manual-test", "decision evidence");
        let (path, common) = persist_manual_decision(&context, &error).unwrap();
        assert!(path.starts_with(common.join("caravan/manual-decisions")));
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["error"]["code"], "manual-test");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn manual_shell_inherits_context_and_zero_exit_requests_rediscovery() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
        let context = caravan::AppContext {
            repository_path: directory.path().canonicalize().unwrap(),
            config_path: PathBuf::from(".caravan/config.yaml"),
            config_existed: false,
            config: caravan::config::CaravanConfig::default(),
        };
        let input = LoopInput {
            interval_secs: None,
            once: true,
            manual: true,
            shell: Some(
                "test -f \"$CARA_DECISION_FILE\" && test \"$CARA_DECISION_CODE\" = manual-test && test \"$PWD\" = \"$CARA_REPOSITORY_PATH\""
                    .to_owned(),
            ),
        };
        run_manual_decision_shell(
            &context,
            &input,
            &AppError::validation("manual-test", "decision"),
        )
        .unwrap();
    }

    #[test]
    fn nested_workspace_evidence_is_discovered() {
        let evidence = serde_json::json!({
            "decision": {"evidence": {"repair": {"workspace": "/tmp/exact-repair"}}}
        });
        assert_eq!(find_workspace_path(&evidence), Some("/tmp/exact-repair"));
    }

    #[test]
    fn interactive_branch_slug_has_sane_bounded_defaults() {
        assert_eq!(
            slugify("Fix: sync the Really Great Queue!"),
            "fix-sync-the-really-great-queue"
        );
        assert!(slugify("---").starts_with("work-"));
    }
}
