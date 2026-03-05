#[allow(dead_code)]
mod api;
#[path = "chief/init_files.rs"]
mod init_files;
#[path = "chief/suite_commands.rs"]
mod suite_commands;
#[cfg(test)]
#[path = "chief/tests.rs"]
mod tests;

use anyhow::{Context, Result, bail};
use chief::domain::{JobStatus, RunExitStatus, Todo, TodoStatus};
use chief::flow::FlowKind;
use chief::git::GitOps;
use chief::orchestrator::OrchestratorError;
use chief::paths;
use chief::scheduler::Scheduler;
use chief::service::{ChiefEngine, ProjectContext, ProjectRegistry};
use chief::storage::{EventQuery, ProjectStore, db_reset_required_from_anyhow};
use clap::{Args, Parser, Subcommand};
use rusqlite::OptionalExtension;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Debug, Parser)]
#[command(name = "chief")]
#[command(about = "Chief orchestration CLI")]
struct Cli {
    #[arg(long, default_value = ".")]
    project_dir: PathBuf,
    #[arg(long)]
    flow: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    max_retries: Option<usize>,
    /// Markdown file used when running flow=loop_file via the default `chief` command.
    #[arg(long)]
    file: Option<PathBuf>,
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
    /// Move legacy root-level chief files into .chief/.
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
    /// Run queued todos using the refactor flow.
    Refactor,
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Path to the Chief repo root that contains *.example.yaml files.
    #[arg(long, default_value = "../chief")]
    chief_root: PathBuf,
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
    file: PathBuf,
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

    let context = ProjectContext::load(&cli.project_dir)?;
    let configured_flow = context.chief_yaml.chief.flow.trim();
    let flow_input = cli.flow.as_deref().unwrap_or(configured_flow);
    let flow_kind: FlowKind = flow_input
        .parse()
        .with_context(|| format!("invalid flow '{flow_input}'"))?;

    let requirements_text = load_requirements_text(&cli.requirements, &cli.requirements_file)?;
    if !requirements_text.trim().is_empty() {
        let engine = ChiefEngine::new(context.clone());
        let diff = engine.process_requirements(&requirements_text, cli.model.clone())?;
        println!("=== git diff HEAD ===");
        if diff.trim().is_empty() {
            println!("(no diff)");
        } else {
            println!("{diff}");
        }
        return Ok(());
    }

    if matches!(flow_kind, FlowKind::LoopFile) {
        let file = cli.file.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "flow 'loop_file' requires --file <path> (or use `chief loop_file --file <path>`)"
            )
        })?;
        return run_loop_file(cli, &LoopFileArgs { file });
    }
    if cli.file.is_some() {
        bail!("--file is only supported when flow resolves to 'loop_file'");
    }

    run_todo_queue_flow(cli, context, flow_kind)
}

fn run_todo_queue_flow(cli: &Cli, context: ProjectContext, flow_kind: FlowKind) -> Result<()> {
    let engine = ChiefEngine::new(context.clone());
    let max_retries = cli
        .max_retries
        .unwrap_or(context.chief_yaml.chief.max_retries.max(1));
    let head_before = context.git.head_commit(&context.project_dir).ok();
    let latest_run_before = latest_run_id(&context.store)?;

    let queue_result = engine.run_todos_until_done_with_retries(
        flow_kind,
        cli.model.clone(),
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
    if let Err(err) = print_cli_flow_summary(&context, run_id, head_before.as_deref()) {
        eprintln!("warning: failed to print run summary: {err:#}");
    }

    match queue_result {
        Ok(()) => {
            println!("all todos are done");
            Ok(())
        }
        Err(OrchestratorError::Retryable(err)) => Err(err).context("maximum retry count reached"),
        Err(OrchestratorError::Unrecoverable(err)) => Err(err).context("unrecoverable failure"),
    }
}

fn run_refactor(cli: &Cli) -> Result<()> {
    if cli.file.is_some() {
        bail!("--file is only supported when flow resolves to 'loop_file'");
    }
    let context = ProjectContext::load(&cli.project_dir)?;
    run_todo_queue_flow(cli, context, FlowKind::Refactor)
}

fn run_clean_done(cli: &Cli) -> Result<()> {
    let context = ProjectContext::load(&cli.project_dir)?;
    let removed = context.store.clean_completed_todos_with_commit()?;
    println!("cleaned completed todos ({removed} removed)");
    Ok(())
}

fn run_tail_events(cli: &Cli, args: &TailEventsArgs) -> Result<()> {
    let context = ProjectContext::load(&cli.project_dir)?;
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

    let context = ProjectContext::load(&project_dir)?;
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
    let mut context = ProjectContext::load(&cli.project_dir)?;
    let file_path = if args.file.is_absolute() {
        args.file.clone()
    } else {
        context.project_dir.join(&args.file)
    };
    let file_contents = fs::read_to_string(&file_path)
        .with_context(|| format!("failed to read loop_file input {}", file_path.display()))?;

    context.chief_yaml.chief.flow = FlowKind::LoopFile.as_str().to_owned();
    println!("loop_file: started {}", file_path.display());
    let head_before = context.git.head_commit(&context.project_dir).ok();

    let synthetic_todo = Todo {
        id: Todo::compute_id(
            &format!("loop_file:{}", file_path.display()),
            file_contents.as_str(),
        ),
        todo: format!("loop_file: {}", file_path.display()),
        expectations: file_contents,
        priority: 1,
        // Keep loop_file detached from todo queue semantics. Suite checks are
        // resolved from the active .chief/chief.yaml during each iteration.
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
        cli.model.clone(),
        Arc::new(AtomicBool::new(false)),
        1,
        |attempt, total, err| {
            println!("loop_file: retry {attempt}/{total} failed: {err:#}");
        },
    );

    if let Err(err) =
        print_cli_flow_summary(&context, Some(run_id.as_str()), head_before.as_deref())
    {
        eprintln!("warning: failed to print run summary: {err:#}");
    }

    match result {
        Ok(outcome) => {
            context.set_job_status(job, JobStatus::Completed, None)?;
            engine.finish_run(&run_id, RunExitStatus::Success)?;
            println!(
                "completed loop_file {}{}",
                outcome.todo_id,
                outcome
                    .commit_hash
                    .as_deref()
                    .map(|hash| format!(" @ {hash}"))
                    .unwrap_or_default()
            );
            Ok(())
        }
        Err(err) => {
            let run_exit_status = if err.is_unrecoverable() {
                RunExitStatus::UnrecoverableFailure
            } else {
                RunExitStatus::Failure
            };
            let _ = context.set_job_status(job, JobStatus::Failed, Some(err.to_string()));
            let _ = engine.finish_run(&run_id, run_exit_status);
            Err(err.into_error()).context("loop_file execution failed")
        }
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

fn convergence_iteration_count(store: &ProjectStore, run_id: &str) -> Result<usize> {
    let conn = rusqlite::Connection::open(&store.db_path)
        .with_context(|| format!("failed to open {}", store.db_path.display()))?;
    let mut stmt = conn
        .prepare(
            "SELECT COUNT(*) FROM events
             WHERE run_id = ?1
               AND event_type = 'phase_change'
               AND msg LIKE 'convergence loop iteration %'",
        )
        .context("failed to prepare iteration count query")?;
    let count: i64 = stmt
        .query_row([run_id], |row| row.get(0))
        .context("failed to query convergence iteration count")?;
    Ok(count.max(0) as usize)
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

fn print_cli_flow_summary(
    context: &ProjectContext,
    run_id: Option<&str>,
    head_before: Option<&str>,
) -> Result<()> {
    let iterations = match run_id {
        Some(run_id) => convergence_iteration_count(&context.store, run_id)?,
        None => 0,
    };
    let commits = git_commits_since(&context.project_dir, head_before)?;

    println!("run summary:");
    println!("iterations: {iterations}");
    println!("commits: {}", commits.len());
    println!("git log --oneline:");
    if commits.is_empty() {
        println!("(no new commits)");
    } else {
        for commit in commits {
            println!("{commit}");
        }
    }
    Ok(())
}
