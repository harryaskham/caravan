//! `cara` command-line entry point.

use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use caravan::{
    build_router, feedback_config, feedback_configuration_error, feedback_panic_config,
    updater_config, AppContext, AppError, CheckInput, CreateInput, EvictInput, JoinInput,
    LockRecoverInput, LockStatusInput, LoopInput, PauseInput, ResumeInput, SplitInput, SyncInput,
    AGENT_HELP, TOOL_NAME,
};
use clap::{Args, Parser, Subcommand, ValueEnum};
use feedback_cli::{FeedbackEvent, FeedbackKind, Reporter, Severity};
use mcp_cli::{write_json_result, McpServer, StdioServerConfig, StructuredError};
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
    /// Explicitly initialize local config and verify/create repository resources.
    Init,
    /// Discover the current PR, all caravans, invalid fragments, and decisions.
    Status,
    /// Read the bounded repository event journal, optionally following new records.
    Log(LogCommand),
    /// Return the canonical next priority-then-FIFO admission without mutation.
    NextCandidate,
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
    /// Explicitly freeze one caravan and disable only its head auto-merge.
    Pause(PauseInput),
    /// Explicitly revalidate and resume one paused caravan.
    Resume(ResumeInput),
    /// Idempotently synchronize one or all caravans until a decision point.
    Sync(SyncInput),
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
    let cli = Cli::parse();
    let panic_feedback = feedback_panic_config(cli.json);
    feedback_cli::install_panic_hook(&panic_feedback);
    if let Err(error) = updatable_cli::maybe_apply_staged_update(TOOL_NAME) {
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
        Command::Log(command) => run_log(cli, command),
        Command::NextCandidate => run_next_candidate(cli),
        Command::Check(input) => run_check(cli, input),
        Command::New(input) => {
            run_membership(cli, |context| caravan::membership::new(context, input))
        }
        Command::Renew(input) => {
            run_membership(cli, |context| caravan::membership::renew(context, input))
        }
        Command::Join(input) => {
            run_membership(cli, |context| caravan::membership::join(context, input))
        }
        Command::Rejoin(input) => {
            run_membership(cli, |context| caravan::membership::rejoin(context, input))
        }
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
        Command::Pause(input) => run_pause(cli, input),
        Command::Resume(input) => run_resume(cli, input),
        Command::Sync(input) => run_sync(cli, input),
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

fn load_context(cli: &Cli) -> Result<AppContext, i32> {
    AppContext::load(cli.config.as_deref()).map_err(|error| {
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

fn run_status(cli: &Cli) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = caravan::read::status(&context);
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            print!("{}", render_status(&output));
            Ok(())
        }
        Err(error) => emit_human_error(error),
    }
}

fn run_next_candidate(cli: &Cli) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = caravan::read::next_candidate(&context);
    if cli.json {
        return emit_result(true, result);
    }
    match result {
        Ok(output) => {
            if let Some(number) = output.admission.next_candidate {
                let reason = output
                    .admission
                    .candidates
                    .iter()
                    .find(|candidate| candidate.pr == number)
                    .map(|candidate| candidate.reason.as_str())
                    .or_else(|| {
                        output
                            .admission
                            .rejected
                            .iter()
                            .find(|candidate| candidate.pr == number)
                            .map(|candidate| candidate.reason.as_str())
                    })
                    .unwrap_or("fail closed: provider attempt metadata unavailable");
                println!("next admission attempt: #{number} — {reason}");
                println!("  {}", output.attempt_contract);
            } else {
                println!("no automatic-admission attempt");
            }
            for rejected in output.admission.rejected {
                println!("! #{}: {}", rejected.pr, rejected.reason);
            }
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

fn run_sync(cli: &Cli, input: &SyncInput) -> Result<(), i32> {
    let context = load_context(cli)?;
    let result = caravan::sync::sync(&context, input);
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
        return emit_result(true, caravan::loop_runner::run(&context, input, |_| {}));
    }
    match caravan::loop_runner::run(&context, input, |tick| {
        print!("{}", render_loop_tick(tick));
    }) {
        Ok(output) => {
            if output.stopped_by_signal {
                println!("loop stopped after {} tick(s)", output.ticks);
            }
            Ok(())
        }
        Err(error) => emit_human_error(error),
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

fn render_sync(output: &caravan::sync::SyncOutput) -> String {
    let caravans = output
        .synchronized_caravans
        .iter()
        .map(|number| format!("#{number}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut text = format!(
        "sync: {} — {}\n",
        if output.receipt.changed {
            "changed"
        } else {
            "already converged"
        },
        if caravans.is_empty() {
            "no caravans".to_owned()
        } else {
            format!("caravans {caravans}")
        }
    );
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
    for observation in &output.ci {
        let _ = writeln!(
            text,
            "  CI #{}: {:?} ({} checks, {} failed runs)",
            observation.pr,
            observation.disposition,
            observation.checks.len(),
            observation.failed_runs.len()
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
    let mut text = format!(
        "{}: PR #{} in caravan #{} ({})\n",
        output.receipt.operation,
        output.pull_request.number,
        output.caravan_id,
        if output.receipt.changed {
            "changed"
        } else {
            "already satisfied"
        }
    );
    for step in &output.receipt.completed_steps {
        let _ = writeln!(text, "  {:?} {:?}: {}", step.kind, step.state, step.summary);
    }
    append_hook_deliveries(&mut text, &output.hook_deliveries);
    text
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
    let _ = writeln!(
        text,
        "{} @ {} — {}",
        output.repository,
        output.default_branch,
        if output.healthy {
            "healthy"
        } else {
            "needs attention"
        }
    );
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
    if let Some(action) = &output.rebase_on_join.required_action {
        let _ = writeln!(text, "  action: {action}");
    }
    let _ = writeln!(text, "caravans: {}", output.analysis.fleet.caravans.len());
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
            .map(|number| format!("#{number}"))
            .collect::<Vec<_>>()
            .join(" -> ");
        let _ = writeln!(text, "  #{}: {chain}", caravan.id);
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
        let _ = writeln!(text, "automatic admission (priority then FIFO):");
        for candidate in &output.admission.candidates {
            let _ = writeln!(text, "  #{}: {}", candidate.pr, candidate.reason);
        }
    }
    for rejected in &output.admission.rejected {
        let _ = writeln!(text, "! admission #{}: {}", rejected.pr, rejected.reason);
    }
    for problem in &output.analysis.fleet.problems {
        let _ = writeln!(text, "! {:?}: {}", problem.kind, problem.message);
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
        let _ = writeln!(text, "! {:?}: {}", problem.kind, problem.message);
    }
    text
}

fn render_check(output: &caravan::read::CheckOutput) -> String {
    let mut text = format!(
        "check: {} ({:?}) — PR #{} [{}@{} -> {}@{}]; next: {:?}",
        if output.eligible {
            "eligible"
        } else {
            "not eligible"
        },
        output.mode,
        output.current_pr,
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
        output.candidate.labels.iter().cloned().collect::<Vec<_>>().join(","),
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
    for problem in &output.problems {
        let _ = writeln!(text, "! {:?}: {}", problem.kind, problem.message);
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
            let mut hooks = String::new();
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
    let context = AppContext::load(config_path).map_err(|error| {
        eprintln!("cara: {error}");
        2
    })?;
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
        FeedbackCommand::Status => emit_result::<_, AppError>(json, Ok(caravan::feedback_status())),
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
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            timing: None,
            repository: repository.clone(),
            rebase_on_join: caravan::read::RebaseOnJoinStatus::default(),
            default_branch: "main".to_owned(),
            current_branch: Some("feature".to_owned()),
            current_pr: None,
            healthy: true,
            initialization: caravan::initialization::InitializationStatus::default(),
            admission: caravan::read::AdmissionStatus {
                policy: "priority then FIFO".to_owned(),
                priority_labels: Vec::new(),
                candidates: Vec::new(),
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
                },
                pull_requests: std::collections::BTreeMap::new(),
                compatibility: Vec::new(),
            },
            pauses: Vec::new(),
        };
        let rendered = render_status(&output);
        assert!(rendered.contains("harryaskham/caravan @ main — healthy"));
        assert!(rendered.contains("current: feature (no open PR)"));
        assert!(rendered.contains("rebase_on_join=disabled"));
        assert!(rendered.contains("set `rebase_on_join: true`"));
        assert!(!rendered.contains("\"analysis\""));
    }
}
