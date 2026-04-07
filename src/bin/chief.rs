#[allow(dead_code)]
mod api;
#[path = "chief/init_files.rs"]
mod init_files;
#[path = "chief/suite_commands.rs"]
mod suite_commands;
#[cfg(test)]
#[path = "chief/tests.rs"]
mod tests;

use agentusage::{ApprovalPolicy, UsageConfig, run_claude, run_codex};
use anyhow::{Context, Result, bail};
use chief::config::{ChiefConfigOverrides, McpServerConfig};
use chief::domain::{JobStatus, RunExitStatus, Todo, TodoStatus};
use chief::flow::FlowKind;
use chief::git::GitOps;
use chief::orchestrator::OrchestratorError;
use chief::paths;
use chief::scheduler::Scheduler;
use chief::service::{ChiefEngine, ProjectContext, ProjectRegistry};
use chief::storage::{EventQuery, ProjectStore, db_reset_required_from_anyhow};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

mod chief_option_help {
    #[derive(Debug, Clone, Copy)]
    pub(super) struct ChiefOptionHelpSpec {
        pub key: &'static str,
        pub help: &'static str,
    }

    pub(super) const FLOW: &str = "Flow to run (`loop_file`, `bd`, or `refactor`).";
    pub(super) const AGENT: &str = "Agent binary to use (`codex`, `claude`, or `opencode`).";
    pub(super) const MODEL: &str = "Model override passed to the selected agent.";
    pub(super) const MODEL_REASONING_EFFORT: &str =
        "Reasoning effort for model adapters that support it.";
    pub(super) const AGENT_EXTRA_ARGS: &str =
        "Extra CLI args forwarded to the selected agent (YAML/JSON string list).";
    pub(super) const MCP_SERVERS: &str =
        "MCP server config (`personal` or YAML/JSON object; `{}` means no servers).";
    pub(super) const MAX_RETRIES: &str =
        "Retry budget for queued todo flows (minimum effective value is 1).";
    pub(super) const MAX_LOOP_ITERATIONS: &str =
        "Maximum convergence iterations for loop-style flows.";
    pub(super) const REQUIRED_STABLE_ITERATIONS: &str =
        "Consecutive stable iterations required before convergence is done.";
    pub(super) const AGENT_TIMEOUT_SECONDS: &str =
        "Per-agent invocation timeout in seconds (0 disables timeout).";
    pub(super) const SUITE_COMMAND_TIMEOUT_SECONDS: &str =
        "Default timeout in seconds for suite/readiness commands.";
    pub(super) const AGENT_LOG_MAX_OUTPUT_LINES: &str =
        "Tail line count kept when truncating agent output in logs.";
    pub(super) const AGENT_LOG_MAX_OUTPUT_CHARS: &str =
        "Tail character count kept when truncating agent output in logs.";
    pub(super) const RESPECT_LIMITS: &str =
        "When true, wait for agentusage limits before launching agent calls.";
    pub(super) const USE_AGENT_LOG_TRUNCATION_FOR_STDOUT_LOGS: &str =
        "When true, apply agent log truncation to stdout log output too.";

    pub(super) const SPECS: [ChiefOptionHelpSpec; 15] = [
        ChiefOptionHelpSpec {
            key: "flow",
            help: FLOW,
        },
        ChiefOptionHelpSpec {
            key: "agent",
            help: AGENT,
        },
        ChiefOptionHelpSpec {
            key: "model",
            help: MODEL,
        },
        ChiefOptionHelpSpec {
            key: "model_reasoning_effort",
            help: MODEL_REASONING_EFFORT,
        },
        ChiefOptionHelpSpec {
            key: "agent_extra_args",
            help: AGENT_EXTRA_ARGS,
        },
        ChiefOptionHelpSpec {
            key: "mcp_servers",
            help: MCP_SERVERS,
        },
        ChiefOptionHelpSpec {
            key: "max_retries",
            help: MAX_RETRIES,
        },
        ChiefOptionHelpSpec {
            key: "max_loop_iterations",
            help: MAX_LOOP_ITERATIONS,
        },
        ChiefOptionHelpSpec {
            key: "required_stable_iterations",
            help: REQUIRED_STABLE_ITERATIONS,
        },
        ChiefOptionHelpSpec {
            key: "agent_timeout_seconds",
            help: AGENT_TIMEOUT_SECONDS,
        },
        ChiefOptionHelpSpec {
            key: "suite_command_timeout_seconds",
            help: SUITE_COMMAND_TIMEOUT_SECONDS,
        },
        ChiefOptionHelpSpec {
            key: "agent_log_max_output_lines",
            help: AGENT_LOG_MAX_OUTPUT_LINES,
        },
        ChiefOptionHelpSpec {
            key: "agent_log_max_output_chars",
            help: AGENT_LOG_MAX_OUTPUT_CHARS,
        },
        ChiefOptionHelpSpec {
            key: "respect_limits",
            help: RESPECT_LIMITS,
        },
        ChiefOptionHelpSpec {
            key: "use_agent_log_truncation_for_stdout_logs",
            help: USE_AGENT_LOG_TRUNCATION_FOR_STDOUT_LOGS,
        },
    ];
}

#[derive(Debug, Clone, Args, Default)]
struct CliChiefOverrides {
    #[arg(long, global = true, help = chief_option_help::FLOW)]
    flow: Option<String>,
    #[arg(long, global = true, help = chief_option_help::AGENT)]
    agent: Option<String>,
    #[arg(long, global = true, help = chief_option_help::MODEL)]
    model: Option<String>,
    #[arg(long, global = true, help = chief_option_help::MODEL_REASONING_EFFORT)]
    model_reasoning_effort: Option<String>,
    #[arg(long, global = true, help = chief_option_help::AGENT_EXTRA_ARGS)]
    agent_extra_args: Option<String>,
    #[arg(long, global = true, help = chief_option_help::MCP_SERVERS)]
    mcp_servers: Option<String>,
    #[arg(long, global = true, help = chief_option_help::MAX_RETRIES)]
    max_retries: Option<usize>,
    #[arg(long, global = true, help = chief_option_help::MAX_LOOP_ITERATIONS)]
    max_loop_iterations: Option<usize>,
    #[arg(long, global = true, help = chief_option_help::REQUIRED_STABLE_ITERATIONS)]
    required_stable_iterations: Option<usize>,
    #[arg(long, global = true, help = chief_option_help::AGENT_TIMEOUT_SECONDS)]
    agent_timeout_seconds: Option<u64>,
    #[arg(long, global = true, help = chief_option_help::SUITE_COMMAND_TIMEOUT_SECONDS)]
    suite_command_timeout_seconds: Option<u64>,
    #[arg(long, global = true, help = chief_option_help::AGENT_LOG_MAX_OUTPUT_LINES)]
    agent_log_max_output_lines: Option<usize>,
    #[arg(long, global = true, help = chief_option_help::AGENT_LOG_MAX_OUTPUT_CHARS)]
    agent_log_max_output_chars: Option<usize>,
    #[arg(long, global = true, help = chief_option_help::RESPECT_LIMITS)]
    respect_limits: Option<bool>,
    #[arg(
        long,
        global = true,
        help = chief_option_help::USE_AGENT_LOG_TRUNCATION_FOR_STDOUT_LOGS
    )]
    use_agent_log_truncation_for_stdout_logs: Option<bool>,
}

#[derive(Debug, Parser)]
#[command(name = "chief")]
#[command(about = "Chief orchestration CLI")]
struct Cli {
    #[arg(long, default_value = ".")]
    project_dir: PathBuf,
    #[command(flatten)]
    chief: CliChiefOverrides,
    /// Markdown file used when running flow=loop_file via the default `chief` command.
    #[arg(long)]
    file: Option<PathBuf>,
    /// Prompt text used when running flow=loop_file via the default `chief` command.
    /// Mutually exclusive with --file.
    #[arg(long)]
    prompt: Option<String>,
    /// Scope convergence to these paths only (repeatable). Ignored for non-loop_file flows.
    #[arg(long = "watch-only")]
    watch_only: Vec<String>,
    #[arg(long = "requirements")]
    requirements: Vec<String>,
    #[arg(long = "requirements-file")]
    requirements_file: Vec<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Initialize Chief config files in a new project directory.
    Init(InitArgs),
    /// Move legacy root-level `chief.yaml`, `chief.example.yaml`, and `chief.db` into `.chief/`.
    Migrate,
    /// Remove completed todos that have a commit hash.
    CleanDone,
    /// Run project pre-run checks (same readiness checks used by backend/frontend start).
    Check(CheckArgs),
    /// Print recent project events.
    TailEvents(TailEventsArgs),
    /// Run suite-level commands from chief.yaml for a specific suite.
    Suite(SuiteArgs),
    /// Execute one loop_file flow run from a markdown file.
    #[command(name = "loop_file", alias = "loop-file")]
    LoopFile(LoopFileArgs),
    /// Run a bd-driven convergence loop using prompts/bd.md.
    Bd,
    /// Run queued todos using the refactor flow.
    Refactor,
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Path to the Chief repo root that contains *.example.yaml files.
    #[arg(long, default_value = "../chief")]
    chief_root: PathBuf,
    /// Initialize beads integration (runs `bd init`).
    #[arg(long, default_value_t = false)]
    beads: bool,
}

#[derive(Debug, Args)]
struct TailEventsArgs {
    /// Maximum number of most-recent events to print.
    #[arg(long, short = 'n', default_value_t = 50)]
    limit: usize,
}

#[derive(Debug, Args)]
struct CheckArgs {
    /// Force executing checks even when cached readiness is still valid.
    #[arg(long, default_value_t = false)]
    force: bool,
}

#[derive(Debug, Args)]
struct SuiteArgs {
    #[command(subcommand)]
    command: SuiteCommand,
}

#[derive(Debug, Subcommand)]
enum SuiteCommand {
    /// Run the suite test command.
    Test(SuiteRunArgs),
    /// Run the suite test_init command.
    #[command(name = "test_init", alias = "test-init")]
    TestInit(SuiteRunNoTargetArgs),
    /// Run the suite test_setup command.
    #[command(name = "test_setup", alias = "test-setup")]
    TestSetup(SuiteRunNoTargetArgs),
    /// Run the suite lint command.
    #[command(alias = "linting")]
    Lint(SuiteRunArgs),
    /// Run the suite lint_fix command.
    #[command(
        name = "lint_fix",
        alias = "lint-fix",
        alias = "linting_fix",
        alias = "linting-fix"
    )]
    LintFix(SuiteRunArgs),
}

#[derive(Debug, Args)]
struct SuiteRunArgs {
    /// Suite name as configured in chief.yaml.
    #[arg(long)]
    suite: String,
    /// Optional target value used for {target} placeholder replacement.
    #[arg(long)]
    target: Option<String>,
}

#[derive(Debug, Args)]
struct SuiteRunNoTargetArgs {
    /// Suite name as configured in chief.yaml.
    #[arg(long)]
    suite: String,
}

#[derive(Debug, Args)]
struct LoopFileArgs {
    /// Markdown file path to load as the loop_file task body.
    #[arg(long)]
    file: Option<PathBuf>,
    /// Prompt text for the loop_file task body.
    /// Mutually exclusive with --file.
    #[arg(long)]
    prompt: Option<String>,
    /// Scope convergence to these paths only (repeatable). When set, an iteration
    /// is considered stable only if none of the specified paths were modified.
    #[arg(long = "watch-only")]
    watch_only: Vec<String>,
}

impl CliChiefOverrides {
    fn to_config_overrides(&self) -> Result<ChiefConfigOverrides> {
        let Self {
            flow,
            agent,
            model,
            model_reasoning_effort,
            agent_extra_args,
            mcp_servers,
            max_retries,
            max_loop_iterations,
            required_stable_iterations,
            agent_timeout_seconds,
            suite_command_timeout_seconds,
            agent_log_max_output_lines,
            agent_log_max_output_chars,
            respect_limits,
            use_agent_log_truncation_for_stdout_logs,
        } = self.clone();

        let agent_extra_args = agent_extra_args
            .as_deref()
            .map(parse_agent_extra_args_override)
            .transpose()?;
        let mcp_servers = mcp_servers
            .as_deref()
            .map(parse_mcp_servers_override)
            .transpose()?;

        Ok(ChiefConfigOverrides {
            flow,
            agent,
            model,
            model_reasoning_effort,
            agent_extra_args,
            mcp_servers,
            max_retries,
            max_loop_iterations,
            required_stable_iterations,
            agent_timeout_seconds,
            suite_command_timeout_seconds,
            agent_log_max_output_lines,
            agent_log_max_output_chars,
            respect_limits,
            use_agent_log_truncation_for_stdout_logs,
        })
    }
}

fn parse_agent_extra_args_override(raw: &str) -> Result<Vec<String>> {
    serde_yaml::from_str::<Vec<String>>(raw).with_context(
        || "--agent-extra-args must be a YAML/JSON string list (for example: [] or [\"--foo\"])",
    )
}

fn parse_mcp_servers_override(raw: &str) -> Result<Option<BTreeMap<String, McpServerConfig>>> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("personal") {
        return Ok(None);
    }

    serde_yaml::from_str::<BTreeMap<String, McpServerConfig>>(trimmed)
        .map(Some)
        .with_context(|| {
            "--mcp-servers must be `personal` or a YAML/JSON object (for example: {} or {docs: {transport: stdio, command: npx}})"
        })
}

fn apply_cli_overrides_to_context(context: &mut ProjectContext, cli: &Cli) -> Result<()> {
    let overrides = cli.chief.to_config_overrides()?;
    let current = std::mem::take(&mut context.chief_yaml.chief);
    context.chief_yaml.chief = current.apply_overrides(overrides);
    Ok(())
}

fn load_context_with_cli_overrides(project_dir: &Path, cli: &Cli) -> Result<ProjectContext> {
    let mut context = ProjectContext::load(project_dir)?;
    apply_cli_overrides_to_context(&mut context, cli)?;
    Ok(context)
}

fn main() {
    if let Err(err) = run_with_db_reset_prompt() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run_with_db_reset_prompt() -> Result<()> {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => Ok(()),
        Err(err) => {
            let Some(reset) = db_reset_required_from_anyhow(&err) else {
                return Err(err);
            };
            eprintln!(
                "warning: chief.db is inconsistent at {}\nreason: {}\n",
                reset.db_path.display(),
                reset.reason
            );
            if !confirm_db_reset(&reset.db_path)? {
                eprintln!("cancelled: chief.db reset declined by user");
                return Ok(());
            }
            let store = ProjectStore::new(&cli.project_dir);
            store.reset_db()?;
            eprintln!("reset complete. retrying...\n");
            run(&cli)
        }
    }
}

fn run_command(cli: &Cli, command: &Commands) -> Result<()> {
    match command {
        Commands::Init(args) => init_files::run_init(cli, args),
        Commands::Migrate => run_migrate(cli),
        Commands::CleanDone => run_clean_done(cli),
        Commands::Check(args) => run_check(cli, args),
        Commands::TailEvents(args) => run_tail_events(cli, args),
        Commands::Suite(args) => suite_commands::run_suite_command(cli, args),
        Commands::LoopFile(args) => run_loop_file(cli, args),
        Commands::Bd => run_bd(cli),
        Commands::Refactor => run_refactor(cli),
    }
}

fn ensure_chief_yaml_exists(project_dir: &Path) -> Result<()> {
    let config_path = paths::chief_yaml_path(project_dir);
    if config_path.is_file() {
        return Ok(());
    }
    bail!(
        "missing required chief config at {}. create .chief/chief.yaml (run `chief init` or copy .chief/chief.example.yaml)",
        config_path.display()
    )
}

fn run(cli: &Cli) -> Result<()> {
    if !matches!(cli.command, Some(Commands::Init(_) | Commands::Migrate)) {
        ensure_chief_yaml_exists(&cli.project_dir)?;
    }

    if let Some(command) = &cli.command {
        return run_command(cli, command);
    }

    let context = load_context_with_cli_overrides(&cli.project_dir, cli)?;
    let flow_input = context.chief_yaml.chief.flow.trim();
    let flow_kind: FlowKind = flow_input
        .parse()
        .with_context(|| format!("invalid flow '{flow_input}'"))?;

    let requirements_text = load_requirements_text(&cli.requirements, &cli.requirements_file)?;
    if !requirements_text.trim().is_empty() {
        let engine = ChiefEngine::new(context.clone());
        let diff = engine.process_requirements(&requirements_text, cli.chief.model.clone())?;
        println!("=== git diff HEAD ===");
        if diff.trim().is_empty() {
            println!("(no diff)");
        } else {
            println!("{diff}");
        }
        return Ok(());
    }

    if matches!(flow_kind, FlowKind::LoopFile) {
        let file = cli.file.clone();
        let prompt = cli.prompt.clone();
        if file.is_none() && prompt.is_none() {
            bail!(
                "flow 'loop_file' requires either --file <path> or --prompt <text> (or use `chief loop_file --file <path>` or `chief loop_file --prompt <text>`)"
            );
        }
        if file.is_some() && prompt.is_some() {
            bail!("--file and --prompt are mutually exclusive for loop_file flow");
        }
        return run_loop_file(
            cli,
            &LoopFileArgs {
                file,
                prompt,
                watch_only: cli.watch_only.clone(),
            },
        );
    }
    if cli.file.is_some() || cli.prompt.is_some() {
        bail!("--file and --prompt are only supported when flow resolves to 'loop_file'");
    }
    if matches!(flow_kind, FlowKind::Bd) {
        return run_bd(cli);
    }

    run_todo_queue_flow(cli, context, flow_kind)
}

fn run_todo_queue_flow(cli: &Cli, mut context: ProjectContext, flow_kind: FlowKind) -> Result<()> {
    let report_started_at = Utc::now();
    context.chief_yaml.chief.flow = flow_kind.as_str().to_owned();
    let engine = ChiefEngine::new(context.clone());
    let max_retries = context.chief_yaml.chief.max_retries.max(1);
    let head_before = context.git.head_commit(&context.project_dir).ok();
    let latest_run_before = latest_run_id(&context.store)?;

    let queue_result = engine.run_todos_until_done_with_retries(
        flow_kind,
        cli.chief.model.clone(),
        max_retries,
        |outcome| {
            println!(
                "completed todo {}{}",
                outcome.todo_id,
                outcome
                    .commit_hash
                    .as_deref()
                    .map(|hash| format!(" @ {hash}"))
                    .unwrap_or_default()
            );
        },
        |attempt, total, err| {
            eprintln!("run failed ({attempt}/{total}): {err:#}");
        },
    );

    let latest_run_after = latest_run_id(&context.store)?;
    let run_id = latest_run_after
        .as_deref()
        .filter(|candidate| latest_run_before.as_deref() != Some(*candidate));

    let exit_status = match &queue_result {
        Ok(()) => RunExitStatus::Success,
        Err(OrchestratorError::Retryable(_)) => RunExitStatus::Failure,
        Err(OrchestratorError::Unrecoverable(_)) => RunExitStatus::UnrecoverableFailure,
    };
    let exit_reason = match &queue_result {
        Ok(()) => Some("todo queue drained".to_owned()),
        Err(OrchestratorError::Retryable(err)) => Some(err.to_string()),
        Err(OrchestratorError::Unrecoverable(err)) => Some(err.to_string()),
    };

    match &queue_result {
        Ok(()) => {
            println!("all todos are done");
        }
        Err(_) => {}
    }
    if let Err(err) = print_cli_run_report(
        &context,
        run_id,
        head_before.as_deref(),
        report_started_at,
        exit_status,
        exit_reason.as_deref(),
    ) {
        eprintln!("warning: failed to print run report: {err:#}");
    }

    match queue_result {
        Ok(()) => Ok(()),
        Err(OrchestratorError::Retryable(err)) => Err(err).context("maximum retry count reached"),
        Err(OrchestratorError::Unrecoverable(err)) => Err(err).context("unrecoverable failure"),
    }
}

fn run_refactor(cli: &Cli) -> Result<()> {
    let context = load_context_with_cli_overrides(&cli.project_dir, cli)?;
    if cli.file.is_some() || cli.prompt.is_some() {
        bail!("--file and --prompt are only supported when flow resolves to 'loop_file'");
    }
    run_todo_queue_flow(cli, context, FlowKind::Refactor)
}

fn run_bd(cli: &Cli) -> Result<()> {
    let report_started_at = Utc::now();
    let mut context = load_context_with_cli_overrides(&cli.project_dir, cli)?;
    context.chief_yaml.chief.flow = FlowKind::Bd.as_str().to_owned();
    println!("bd: started {}", context.project_dir.display());
    let head_before = context.git.head_commit(&context.project_dir).ok();

    let synthetic_todo = Todo {
        id: Todo::compute_id("bd:ready", "prompts/bd.md"),
        todo: "bd ready convergence".to_owned(),
        expectations:
            "Resolve the current ready bd tickets using prompts/bd.md until `bd ready --json` is empty."
                .to_owned(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let engine = ChiefEngine::new(context.clone());
    let run_id = engine.start_run()?;
    let mut job = context.create_job(
        &run_id,
        1,
        FlowKind::Bd,
        Some(synthetic_todo.id.clone()),
        None,
    )?;
    job = context.set_job_status(job, JobStatus::Running, None)?;

    let result = engine.run_single_todo_with_retries(
        &run_id,
        &job.id,
        1,
        synthetic_todo,
        FlowKind::Bd,
        context.project_dir.clone(),
        cli.chief.model.clone(),
        Vec::new(),
        Arc::new(AtomicBool::new(false)),
        1,
        |attempt, total, err| {
            println!("bd: retry {attempt}/{total} failed: {err:#}");
        },
    );

    let exit_status = if let Err(err) = &result {
        if err.is_unrecoverable() {
            RunExitStatus::UnrecoverableFailure
        } else {
            RunExitStatus::Failure
        }
    } else {
        RunExitStatus::Success
    };
    let exit_reason = result
        .as_ref()
        .err()
        .map(|err| err.to_string())
        .or_else(|| Some("bd flow completed".to_owned()));
    let finalize_result = match &result {
        Ok(_) => context
            .set_job_status(job, JobStatus::Completed, None)
            .and_then(|_| engine.finish_run(&run_id, RunExitStatus::Success)),
        Err(err) => {
            let _ = context.set_job_status(job, JobStatus::Failed, Some(err.to_string()));
            engine.finish_run(&run_id, exit_status)
        }
    };

    if let Ok(outcome) = &result {
        println!(
            "completed bd {}{}",
            outcome.todo_id,
            outcome
                .commit_hash
                .as_deref()
                .map(|hash| format!(" @ {hash}"))
                .unwrap_or_default()
        );
    }
    if let Err(err) = &finalize_result {
        eprintln!("warning: failed to finalize bd run state: {err:#}");
    }
    if let Err(err) = print_cli_run_report(
        &context,
        Some(run_id.as_str()),
        head_before.as_deref(),
        report_started_at,
        exit_status,
        exit_reason.as_deref(),
    ) {
        eprintln!("warning: failed to print run report: {err:#}");
    }
    finalize_result?;

    match result {
        Ok(_) => Ok(()),
        Err(err) => Err(err.into_error()).context("bd execution failed"),
    }
}

fn run_clean_done(cli: &Cli) -> Result<()> {
    let context = load_context_with_cli_overrides(&cli.project_dir, cli)?;
    let removed = context.store.clean_completed_todos_with_commit()?;
    println!("cleaned completed todos ({removed} removed)");
    Ok(())
}

fn run_tail_events(cli: &Cli, args: &TailEventsArgs) -> Result<()> {
    let context = load_context_with_cli_overrides(&cli.project_dir, cli)?;
    let events = context.store.query_events(EventQuery {
        limit: args.limit,
        ..EventQuery::default()
    })?;
    if events.is_empty() {
        println!("No events recorded.");
        return Ok(());
    }
    for event in events.into_iter().rev() {
        println!(
            "[{}] {} {} {} - {}",
            event.timestamp.to_rfc3339(),
            event.level,
            event.phase.map(|phase| phase.as_str()).unwrap_or("-"),
            event.event_type.as_str(),
            event.msg
        );
        if let Some(output) = event.payload.get("output").and_then(|value| value.as_str()) {
            let mut lines: Vec<_> = output
                .lines()
                .rev()
                .take(context.chief_yaml.chief.agent_log_max_output_lines)
                .collect();
            lines.reverse();
            let tail = lines.join("\n");
            if !tail.trim().is_empty() {
                println!("{tail}");
            }
        }
        println!();
    }
    Ok(())
}

fn run_check(cli: &Cli, args: &CheckArgs) -> Result<()> {
    let project_dir = if cli.project_dir.is_absolute() {
        cli.project_dir.clone()
    } else {
        std::env::current_dir()
            .context("failed resolving current directory for --project-dir")?
            .join(&cli.project_dir)
    };

    if !project_dir.exists() {
        bail!(
            "project directory does not exist: {}",
            project_dir.display()
        );
    }
    if !project_dir.is_dir() {
        bail!("project path is not a directory: {}", project_dir.display());
    }

    let context = load_context_with_cli_overrides(&project_dir, cli)?;
    let project_name = context.name.clone();
    let projects_dir = project_dir.clone();
    let registry = ProjectRegistry::discover(&projects_dir, std::slice::from_ref(&project_dir))
        .with_context(|| {
            format!(
                "failed discovering project registry for {}",
                project_dir.display()
            )
        })?;
    let scheduler = Scheduler::new(registry, 1);
    let service = api::service::ApiService::new(scheduler, 1);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to initialize async runtime for `chief check`")?;

    let result = runtime.block_on(async {
        use tokio::sync::broadcast::error::{RecvError, TryRecvError};

        let mut readiness_receiver = service.subscribe_readiness_stream();
        let readiness_check = service.run_readiness_check(&project_name, args.force);
        tokio::pin!(readiness_check);

        loop {
            tokio::select! {
                readiness_result = &mut readiness_check => {
                    // Drain any trailing chunks queued right before completion.
                    loop {
                        match readiness_receiver.try_recv() {
                            Ok(api::service::ReadinessStreamMessage::Chunk { project: stream_project, text })
                                if stream_project == project_name =>
                            {
                                print!("{text}");
                                let _ = io::stdout().flush();
                            }
                            Ok(_) => {}
                            Err(TryRecvError::Lagged(_)) => continue,
                            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                        }
                    }
                    break readiness_result.map_err(anyhow::Error::new);
                }
                readiness_message = readiness_receiver.recv() => {
                    match readiness_message {
                        Ok(api::service::ReadinessStreamMessage::Chunk { project: stream_project, text })
                            if stream_project == project_name =>
                        {
                            print!("{text}");
                            let _ = io::stdout().flush();
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => {}
                    }
                }
            }
        }
    })?;

    println!("project: {project_name}");
    println!("status: {}", result.readiness.status);
    println!("summary: {}", result.readiness.summary);
    println!(
        "executed: {}",
        if result.ran { "yes" } else { "no (cached)" }
    );
    if let Some(checked_at) = result.readiness.checked_at {
        println!("checked_at: {checked_at}");
    }
    if let Some(checking_started_at) = result.readiness.checking_started_at {
        println!("checking_started_at: {checking_started_at}");
    }
    println!("updated_at: {}", result.readiness.updated_at);

    if result.readiness.status != "ready" {
        bail!("{}", result.readiness.summary);
    }

    Ok(())
}

fn run_loop_file(cli: &Cli, args: &LoopFileArgs) -> Result<()> {
    let report_started_at = Utc::now();
    let mut context = load_context_with_cli_overrides(&cli.project_dir, cli)?;

    let (source_desc, expectations): (String, String) = if let Some(ref file) = args.file {
        let file_path = if file.is_absolute() {
            file.clone()
        } else {
            context.project_dir.join(file)
        };
        let contents = fs::read_to_string(&file_path)
            .with_context(|| format!("failed to read loop_file input {}", file_path.display()))?;
        (file_path.display().to_string(), contents)
    } else if let Some(ref prompt) = args.prompt {
        ("cli-prompt".to_string(), prompt.clone())
    } else {
        bail!("loop_file requires either --file or --prompt");
    };

    context.chief_yaml.chief.flow = FlowKind::LoopFile.as_str().to_owned();
    println!("loop_file: started {}", source_desc);
    let head_before = context.git.head_commit(&context.project_dir).ok();

    let synthetic_todo = Todo {
        id: Todo::compute_id(&format!("loop_file:{}", source_desc), expectations.as_str()),
        todo: format!("loop_file: {}", source_desc),
        expectations,
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let engine = ChiefEngine::new(context.clone());
    let run_id = engine.start_run()?;
    let mut job = context.create_job(
        &run_id,
        1,
        FlowKind::LoopFile,
        Some(synthetic_todo.id.clone()),
        None,
    )?;
    job = context.set_job_status(job, JobStatus::Running, None)?;

    let result = engine.run_single_todo_with_retries(
        &run_id,
        &job.id,
        1,
        synthetic_todo,
        FlowKind::LoopFile,
        context.project_dir.clone(),
        cli.chief.model.clone(),
        args.watch_only.clone(),
        Arc::new(AtomicBool::new(false)),
        1,
        |attempt, total, err| {
            println!("loop_file: retry {attempt}/{total} failed: {err:#}");
        },
    );

    let exit_status = if let Err(err) = &result {
        if err.is_unrecoverable() {
            RunExitStatus::UnrecoverableFailure
        } else {
            RunExitStatus::Failure
        }
    } else {
        RunExitStatus::Success
    };
    let exit_reason = result
        .as_ref()
        .err()
        .map(|err| err.to_string())
        .or_else(|| Some("loop_file flow completed".to_owned()));
    let finalize_result = match &result {
        Ok(_) => context
            .set_job_status(job, JobStatus::Completed, None)
            .and_then(|_| engine.finish_run(&run_id, RunExitStatus::Success)),
        Err(err) => {
            let _ = context.set_job_status(job, JobStatus::Failed, Some(err.to_string()));
            engine.finish_run(&run_id, exit_status)
        }
    };

    if let Ok(outcome) = &result {
        println!(
            "completed loop_file {}{}",
            outcome.todo_id,
            outcome
                .commit_hash
                .as_deref()
                .map(|hash| format!(" @ {hash}"))
                .unwrap_or_default()
        );
    }
    if let Err(err) = &finalize_result {
        eprintln!("warning: failed to finalize loop_file run state: {err:#}");
    }
    if let Err(err) = print_cli_run_report(
        &context,
        Some(run_id.as_str()),
        head_before.as_deref(),
        report_started_at,
        exit_status,
        exit_reason.as_deref(),
    ) {
        eprintln!("warning: failed to print run report: {err:#}");
    }
    finalize_result?;

    match result {
        Ok(_) => Ok(()),
        Err(err) => Err(err.into_error()).context("loop_file execution failed"),
    }
}

fn load_requirements_text(inline: &[String], files: &[PathBuf]) -> Result<String> {
    let mut chunks = Vec::new();
    for item in inline {
        let trimmed = item.trim();
        if !trimmed.is_empty() {
            chunks.push(trimmed.to_owned());
        }
    }

    for file in files {
        let content = fs::read_to_string(file)
            .with_context(|| format!("failed to read requirements file {}", file.display()))?;
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            chunks.push(trimmed.to_owned());
        }
    }

    Ok(chunks.join("\n\n"))
}

fn confirm_db_reset(db_path: &Path) -> Result<bool> {
    eprint!(
        "Delete {} and rebuild empty schema? [y/N]: ",
        db_path.display()
    );
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn run_migrate(cli: &Cli) -> Result<()> {
    let project_dir = &cli.project_dir;
    if !project_dir.exists() {
        bail!(
            "project directory does not exist: {}",
            project_dir.display()
        );
    }
    if !project_dir.is_dir() {
        bail!("project path is not a directory: {}", project_dir.display());
    }

    let chief_dir = paths::chief_dir(project_dir);
    fs::create_dir_all(&chief_dir)
        .with_context(|| format!("failed to create {}", chief_dir.display()))?;

    let legacy_file_names = [
        paths::CHIEF_DB_FILE_NAME,
        paths::CHIEF_YAML_FILE_NAME,
        paths::CHIEF_EXAMPLE_FILE_NAME,
    ];

    let mut moved = 0usize;
    let mut skipped = 0usize;
    for file_name in legacy_file_names {
        let source = paths::legacy_root_file_path(project_dir, file_name);
        if !source.exists() {
            skipped += 1;
            continue;
        }

        let destination = chief_dir.join(file_name);
        if destination.exists() {
            bail!(
                "cannot migrate {} because destination already exists: {}",
                source.display(),
                destination.display()
            );
        }

        fs::rename(&source, &destination).with_context(|| {
            format!(
                "failed to migrate {} -> {}",
                source.display(),
                destination.display()
            )
        })?;
        moved += 1;
    }

    init_files::ensure_gitignore_entries(
        &project_dir.join(".gitignore"),
        &init_files::INIT_GITIGNORE_ENTRIES,
    )?;
    println!(
        "migrated chief files to {} (moved {moved}, skipped {skipped})",
        chief_dir.display()
    );
    Ok(())
}

fn latest_run_id(store: &ProjectStore) -> Result<Option<String>> {
    if !store.db_path.exists() {
        return Ok(None);
    }
    let conn = rusqlite::Connection::open(&store.db_path)
        .with_context(|| format!("failed to open {}", store.db_path.display()))?;
    let mut stmt = match conn.prepare("SELECT run_id FROM runs ORDER BY started_at DESC LIMIT 1") {
        Ok(stmt) => stmt,
        Err(err) if err.to_string().contains("no such table: runs") => return Ok(None),
        Err(err) => return Err(err).context("failed to prepare latest run query"),
    };
    stmt.query_row([], |row| row.get::<_, String>(0))
        .optional()
        .context("failed to read latest run id")
}

#[derive(Debug, Clone)]
struct ReportRunRecord {
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    exit_status: Option<String>,
}

#[derive(Debug, Clone)]
struct ReportEventRecord {
    level: String,
    msg: String,
    event_type: String,
    payload: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StableIterationProgress {
    current: usize,
    required: usize,
    converged: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct ReportAgentUsageLimit {
    label: String,
    percent_used: u32,
    percent_remaining: u32,
    reset_info: String,
    #[serde(default)]
    reset_minutes: Option<i64>,
    #[serde(default)]
    spent: Option<String>,
    #[serde(default)]
    requests: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct ReportAgentUsageSnapshot {
    provider: String,
    limits: Vec<ReportAgentUsageLimit>,
}

#[derive(Debug, Clone)]
struct CliRunReport {
    flow_name: String,
    project_dir: PathBuf,
    run_id: Option<String>,
    status: String,
    exit_reason: String,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    iterations: usize,
    stable_progress: Option<StableIterationProgress>,
    agent_calls: usize,
    warning_count: usize,
    lint_passed: usize,
    lint_failed: usize,
    test_passed: usize,
    test_failed: usize,
    wait_seconds_applied: f64,
    commits: Vec<String>,
    agent_name: String,
    usage_snapshot: Option<ReportAgentUsageSnapshot>,
    usage_source: Option<&'static str>,
}

fn parse_rfc3339_utc(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .with_context(|| format!("invalid RFC3339 timestamp '{value}'"))
}

fn load_run_record(store: &ProjectStore, run_id: &str) -> Result<Option<ReportRunRecord>> {
    let conn = rusqlite::Connection::open(&store.db_path)
        .with_context(|| format!("failed to open {}", store.db_path.display()))?;
    let mut stmt = conn
        .prepare(
            "SELECT started_at, ended_at, exit_status
             FROM runs
             WHERE run_id = ?1
             LIMIT 1",
        )
        .context("failed to prepare run record query")?;
    stmt.query_row([run_id], |row| {
        let started_at: String = row.get(0)?;
        let ended_at: Option<String> = row.get(1)?;
        let exit_status: Option<String> = row.get(2)?;
        Ok((started_at, ended_at, exit_status))
    })
    .optional()
    .context("failed to query run record")?
    .map(|(started_at, ended_at, exit_status)| {
        Ok(ReportRunRecord {
            started_at: parse_rfc3339_utc(&started_at)?,
            ended_at: ended_at.as_deref().map(parse_rfc3339_utc).transpose()?,
            exit_status,
        })
    })
    .transpose()
}

fn load_run_events(store: &ProjectStore, run_id: &str) -> Result<Vec<ReportEventRecord>> {
    let conn = rusqlite::Connection::open(&store.db_path)
        .with_context(|| format!("failed to open {}", store.db_path.display()))?;
    let mut stmt = conn
        .prepare(
            "SELECT level, msg, event_type, payload
             FROM events
             WHERE run_id = ?1
             ORDER BY id ASC",
        )
        .context("failed to prepare run event query")?;
    let rows = stmt
        .query_map([run_id], |row| {
            let payload_text: Option<String> = row.get(3)?;
            Ok((
                row.get::<_, Option<String>>(0)?
                    .unwrap_or_else(|| "info".to_owned()),
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                payload_text,
            ))
        })
        .context("failed to query run events")?;

    rows.map(|row| {
        let (level, msg, event_type, payload_text) = row?;
        let payload = payload_text
            .as_deref()
            .and_then(|text| serde_json::from_str::<BTreeMap<String, Value>>(text).ok())
            .unwrap_or_default();
        Ok(ReportEventRecord {
            level,
            msg,
            event_type,
            payload,
        })
    })
    .collect::<Result<Vec<_>>>()
}

fn git_commits_since(project_dir: &Path, head_before: Option<&str>) -> Result<Vec<String>> {
    let mut command = std::process::Command::new("git");
    command.arg("log").arg("--oneline");
    if let Some(head_before) = head_before {
        let range = format!("{head_before}..HEAD");
        command.arg(range);
    }

    let output = command
        .current_dir(project_dir)
        .output()
        .context("failed to run git log --oneline")?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

fn parse_stable_iteration_progress(msg: &str) -> Option<StableIterationProgress> {
    let (converged, tail) = if let Some((_, tail)) = msg.rsplit_once("stable result ") {
        (true, tail)
    } else if let Some((_, tail)) = msg.rsplit_once("phase stable ") {
        (false, tail)
    } else {
        return None;
    };

    let ratio = tail
        .split([';', ' '])
        .find(|segment| segment.contains('/'))?
        .trim();
    let (current, required) = ratio.split_once('/')?;
    Some(StableIterationProgress {
        current: current.parse().ok()?,
        required: required.parse().ok()?,
        converged,
    })
}

fn duration_from_datetimes(started_at: DateTime<Utc>, ended_at: DateTime<Utc>) -> Duration {
    ended_at
        .signed_duration_since(started_at)
        .to_std()
        .unwrap_or(Duration::ZERO)
}

fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn should_use_color_stdout() -> bool {
    if let Ok(force) = std::env::var("CLICOLOR_FORCE")
        && force.trim() != "0"
        && !force.trim().is_empty()
    {
        return true;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    io::stdout().is_terminal()
}

fn style(text: &str, code: &str, enabled: bool) -> String {
    if !enabled {
        return text.to_owned();
    }
    format!("{code}{text}\x1b[0m")
}

fn format_status_label(status: &str, enabled: bool) -> String {
    match status {
        "success" => style("SUCCESS", "\x1b[32m", enabled),
        "failure" => style("FAILURE", "\x1b[31m", enabled),
        "unrecoverable_failure" => style("UNRECOVERABLE", "\x1b[31;1m", enabled),
        other => style(&other.to_ascii_uppercase(), "\x1b[33m", enabled),
    }
}

fn format_report_key(key: &str, enabled: bool) -> String {
    style(key, "\x1b[36m", enabled)
}

fn format_report_header(enabled: bool) -> String {
    style("CHIEF RUN REPORT", "\x1b[1;37m", enabled)
}

fn format_usage_snapshot_lines(snapshot: &ReportAgentUsageSnapshot) -> Vec<String> {
    snapshot
        .limits
        .iter()
        .map(|limit| {
            let mut line = format!(
                "{}: {}% remaining, {}% used, {}",
                limit.label, limit.percent_remaining, limit.percent_used, limit.reset_info
            );
            if let Some(requests) = &limit.requests {
                line.push_str(&format!(", requests {requests}"));
            }
            if let Some(spent) = &limit.spent {
                line.push_str(&format!(", spent {spent}"));
            }
            line
        })
        .collect()
}

fn derive_exit_reason(
    status: &str,
    events: &[ReportEventRecord],
    fallback: Option<&str>,
) -> String {
    if status != "success" {
        if let Some(event) = events
            .iter()
            .rev()
            .find(|event| event.event_type == "phase_failure" || event.level == "error")
        {
            return event.msg.clone();
        }
        return fallback
            .map(str::to_owned)
            .unwrap_or_else(|| "run failed".to_owned());
    }

    if let Some(progress) = events
        .iter()
        .rev()
        .find_map(|event| parse_stable_iteration_progress(&event.msg))
        && progress.converged
    {
        return format!(
            "converged after {} stable iteration(s) out of {} required",
            progress.current, progress.required
        );
    }

    if let Some(event) = events.iter().rev().find(|event| {
        event.msg.contains("phase done on iteration")
            || event
                .msg
                .contains("found no ready tickets during pre-check")
            || event.msg.contains("skipping commit")
            || event.msg.contains("loop done; preparing commit")
    }) {
        return event.msg.clone();
    }

    fallback
        .map(str::to_owned)
        .unwrap_or_else(|| "run completed successfully".to_owned())
}

fn probe_current_agent_usage(
    agent_name: &str,
    project_dir: &Path,
) -> Result<ReportAgentUsageSnapshot> {
    let config = UsageConfig {
        timeout: 45,
        verbose: false,
        approval_policy: ApprovalPolicy::Fail,
        directory: Some(project_dir.display().to_string()),
    };

    let usage = if agent_name.eq_ignore_ascii_case("claude") {
        run_claude(&config)?
    } else if agent_name.eq_ignore_ascii_case("codex") {
        run_codex(&config)?
    } else {
        bail!("unsupported agent '{}' for usage reporting", agent_name);
    };

    Ok(ReportAgentUsageSnapshot {
        provider: usage.provider,
        limits: usage
            .entries
            .into_iter()
            .map(|entry| ReportAgentUsageLimit {
                label: entry.label,
                percent_used: entry.percent_used,
                percent_remaining: entry.percent_remaining,
                reset_info: entry.reset_info,
                reset_minutes: entry.reset_minutes,
                spent: entry.spent,
                requests: entry.requests,
            })
            .collect(),
    })
}

fn build_cli_run_report(
    context: &ProjectContext,
    run_id: Option<&str>,
    head_before: Option<&str>,
    started_at_fallback: DateTime<Utc>,
    exit_status_fallback: RunExitStatus,
    exit_reason_fallback: Option<&str>,
) -> Result<CliRunReport> {
    let run_record = match run_id {
        Some(run_id) => load_run_record(&context.store, run_id)?,
        None => None,
    };
    let events = match run_id {
        Some(run_id) => load_run_events(&context.store, run_id)?,
        None => Vec::new(),
    };
    let ended_at_fallback = Utc::now();
    let status = run_record
        .as_ref()
        .and_then(|record| record.exit_status.clone())
        .unwrap_or_else(|| exit_status_fallback.as_str().to_owned());
    let commits = git_commits_since(&context.project_dir, head_before)?;
    let started_at = run_record
        .as_ref()
        .map(|record| record.started_at)
        .unwrap_or(started_at_fallback);
    let ended_at = run_record
        .as_ref()
        .and_then(|record| record.ended_at)
        .unwrap_or(ended_at_fallback);
    let stable_progress = events
        .iter()
        .rev()
        .find_map(|event| parse_stable_iteration_progress(&event.msg));
    let usage_snapshot_from_events = events.iter().rev().find_map(|event| {
        event
            .payload
            .get("usage")
            .cloned()
            .and_then(|value| serde_json::from_value::<ReportAgentUsageSnapshot>(value).ok())
    });
    let (usage_snapshot, usage_source) =
        match probe_current_agent_usage(&context.chief_yaml.chief.agent, &context.project_dir) {
            Ok(snapshot) => (Some(snapshot), Some("live")),
            Err(_) => (usage_snapshot_from_events, Some("cached")),
        };
    let usage_source = if usage_snapshot.is_some() {
        usage_source
    } else {
        None
    };

    let mut lint_passed = 0usize;
    let mut lint_failed = 0usize;
    let mut test_passed = 0usize;
    let mut test_failed = 0usize;
    let mut wait_seconds_applied = 0.0f64;

    for event in &events {
        match event.event_type.as_str() {
            "lint" => {
                if event
                    .payload
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .unwrap_or(1)
                    == 0
                {
                    lint_passed += 1;
                } else {
                    lint_failed += 1;
                }
            }
            "test_run" => {
                if event
                    .payload
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .unwrap_or(1)
                    == 0
                {
                    test_passed += 1;
                } else {
                    test_failed += 1;
                }
            }
            "agent_cmd" => {
                wait_seconds_applied += event
                    .payload
                    .get("wait_seconds_applied")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
            }
            _ => {}
        }
    }

    Ok(CliRunReport {
        flow_name: context.chief_yaml.chief.flow.clone(),
        project_dir: context.project_dir.clone(),
        run_id: run_id.map(str::to_owned),
        status: status.clone(),
        exit_reason: derive_exit_reason(&status, &events, exit_reason_fallback),
        started_at,
        ended_at,
        iterations: events
            .iter()
            .filter(|event| {
                event.event_type == "phase_change" && event.msg.contains(" loop iteration ")
            })
            .count(),
        stable_progress,
        agent_calls: events
            .iter()
            .filter(|event| event.event_type == "agent_prompt")
            .count(),
        warning_count: events
            .iter()
            .filter(|event| {
                matches!(
                    event.level.to_ascii_lowercase().as_str(),
                    "warning" | "warn" | "error"
                )
            })
            .count(),
        lint_passed,
        lint_failed,
        test_passed,
        test_failed,
        wait_seconds_applied,
        commits,
        agent_name: context.chief_yaml.chief.agent.clone(),
        usage_snapshot,
        usage_source,
    })
}

fn render_cli_run_report(report: &CliRunReport) -> String {
    let color = should_use_color_stdout();
    let mut lines = Vec::new();
    let divider = style(
        "================================================================",
        "\x1b[90m",
        color,
    );
    lines.push(divider.clone());
    lines.push(format_report_header(color));
    lines.push(divider.clone());
    lines.push(format!(
        "{} {}",
        format_report_key("status", color),
        format_status_label(&report.status, color)
    ));
    lines.push(format!(
        "{} {}",
        format_report_key("reason", color),
        report.exit_reason
    ));
    lines.push(format!(
        "{} {}",
        format_report_key("flow", color),
        report.flow_name
    ));
    if let Some(run_id) = &report.run_id {
        lines.push(format!("{} {}", format_report_key("run_id", color), run_id));
    }
    lines.push(format!(
        "{} {}",
        format_report_key("project", color),
        report.project_dir.display()
    ));
    lines.push(String::new());
    lines.push(format!(
        "{} {}",
        format_report_key("started_at", color),
        report.started_at.to_rfc3339()
    ));
    lines.push(format!(
        "{} {}",
        format_report_key("ended_at", color),
        report.ended_at.to_rfc3339()
    ));
    lines.push(format!(
        "{} {}",
        format_report_key("duration", color),
        format_duration(duration_from_datetimes(report.started_at, report.ended_at))
    ));
    lines.push(String::new());
    lines.push(format!(
        "{} {}",
        format_report_key("iterations", color),
        report.iterations
    ));
    if let Some(progress) = &report.stable_progress {
        lines.push(format!(
            "{} {}/{}{}",
            format_report_key("stable_iterations", color),
            progress.current,
            progress.required,
            if progress.converged {
                " (converged)"
            } else {
                ""
            }
        ));
    }
    lines.push(format!(
        "{} {}",
        format_report_key("agent_calls", color),
        report.agent_calls
    ));
    lines.push(format!(
        "{} {}",
        format_report_key("warnings", color),
        report.warning_count
    ));
    lines.push(format!(
        "{} {} passed, {} failed",
        format_report_key("lint", color),
        report.lint_passed,
        report.lint_failed
    ));
    lines.push(format!(
        "{} {} passed, {} failed",
        format_report_key("tests", color),
        report.test_passed,
        report.test_failed
    ));
    lines.push(format!(
        "{} {}",
        format_report_key("limit_wait", color),
        format_duration(Duration::from_secs_f64(
            report.wait_seconds_applied.max(0.0)
        ))
    ));
    lines.push(String::new());
    lines.push(format!(
        "{} {}",
        format_report_key("commits", color),
        report.commits.len()
    ));
    if report.commits.is_empty() {
        lines.push("  (no new commits)".to_owned());
    } else {
        for commit in &report.commits {
            lines.push(format!("  {commit}"));
        }
    }
    lines.push(String::new());
    lines.push(format!(
        "{} {}",
        format_report_key("agent", color),
        report.agent_name
    ));
    match (&report.usage_snapshot, report.usage_source) {
        (Some(snapshot), Some(source)) => {
            lines.push(format!(
                "{} {} ({source})",
                format_report_key("usage_provider", color),
                snapshot.provider
            ));
            for line in format_usage_snapshot_lines(snapshot) {
                lines.push(format!("  {line}"));
            }
        }
        _ => lines.push(format!("{} unavailable", format_report_key("usage", color))),
    }
    lines.push(divider);
    lines.join("\n")
}

fn print_cli_run_report(
    context: &ProjectContext,
    run_id: Option<&str>,
    head_before: Option<&str>,
    started_at_fallback: DateTime<Utc>,
    exit_status_fallback: RunExitStatus,
    exit_reason_fallback: Option<&str>,
) -> Result<()> {
    let report = build_cli_run_report(
        context,
        run_id,
        head_before,
        started_at_fallback,
        exit_status_fallback,
        exit_reason_fallback,
    )?;
    println!("{}", render_cli_run_report(&report));
    Ok(())
}
