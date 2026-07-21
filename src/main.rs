//! `cara` command-line entry point.

use std::fmt::Write as _;
use std::io;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use caravan::{
    AGENT_HELP, AppContext, AppError, CheckInput, CreateInput, EvictInput, JoinInput,
    LockRecoverInput, LockStatusInput, LoopInput, PauseInput, ResumeInput, SplitInput, SyncInput,
    TOOL_NAME, build_router, feedback_config, feedback_configuration_error, feedback_panic_config,
    repair::{
        RepairAbortInput, RepairAuthorizeAgentEditsInput, RepairContinueInput, RepairGrantInput,
        RepairRevokeGrantInput, RepairStartInput, RepairStatusInput,
    },
    updater_config,
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

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Explicitly initialize local config and verify/create repository resources.
    Init,
    /// Discover the current PR, all caravans, invalid fragments, and decisions.
    Status,
    /// Serve the embedded multi-repository Caravan operations dashboard.
    Web(caravan::web::WebInput),
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
    /// Preview exact domain operations without provider mutation.
    #[command(subcommand)]
    Plan(PlanCommand),
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
enum PlanCommand {
    /// Plan sync and first auto-admission through the no-write preflight barrier.
    Sync(SyncInput),
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
        Command::Web(input) => run_web(cli, input),
        Command::Log(command) => run_log(cli, command),
        Command::NextCandidate => run_next_candidate(cli),
        Command::Check(input) => run_check(cli, input),
        Command::New(input) => run_create_membership(cli, input),
        Command::Renew(input) => {
            run_membership(cli, |context| caravan::membership::renew(context, input))
        }
        Command::Join(input) => run_join_membership(cli, input),
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
        Command::Plan(command) => run_plan(cli, command),
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
    let mut text = format!(
        "{}  {state}  {}\n",
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
        "auto-admission={} heuristic={} candidates={} mutations={} github={} duration={}s",
        output.auto_admission.enabled,
        output.auto_admission.heuristic_version,
        output.auto_admission.max_candidates_per_tick,
        output.auto_admission.max_mutations_per_tick,
        output.auto_admission.max_github_requests_per_tick,
        output.auto_admission.max_duration_secs,
    );
    let _ = writeln!(
        text,
        "\n{}  {}",
        heading("CARAVANS"),
        dim(format!("{} active", output.analysis.fleet.caravans.len()))
    );
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
        let _ = writeln!(
            text,
            "  {}  {chain}",
            styled("1;35", format!("van #{}", caravan.id))
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
        let _ = writeln!(
            text,
            "{} {:?}: {}",
            failure("!"),
            problem.kind,
            problem.message
        );
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
            provider_api: caravan::model::GitHubApiTelemetry::default(),
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            timing: None,
            repository: repository.clone(),
            rebase_on_join: caravan::read::RebaseOnJoinStatus::default(),
            auto_admission: caravan::read::AutoAdmissionStatus::default(),
            default_branch: "main".to_owned(),
            current_branch: Some("feature".to_owned()),
            current_pr: None,
            healthy: true,
            initialization: caravan::initialization::InitializationStatus::default(),
            admission: caravan::read::AdmissionStatus {
                policy: "priority then FIFO".to_owned(),
                priority_labels: Vec::new(),
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
                },
                pull_requests: std::collections::BTreeMap::new(),
                compatibility: Vec::new(),
            },
            pauses: Vec::new(),
        };
        let rendered = render_status(&output);
        assert!(rendered.contains("CARAVAN  harryaskham/caravan @ main  healthy"));
        assert!(rendered.contains("current: feature (no open PR)"));
        assert!(rendered.contains("rebase_on_join=disabled"));
        assert!(rendered.contains("set `rebase_on_join: true`"));
        assert!(!rendered.contains("\"analysis\""));
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
