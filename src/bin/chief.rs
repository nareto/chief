use anyhow::{Context, Result};
use chief::flow::FlowKind;
use chief::orchestrator::OrchestratorError;
use chief::service::{ChiefEngine, ProjectContext};
use chief::storage::{EventQuery, ProjectStore, db_reset_required_from_anyhow};
use clap::Parser;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "chief")]
#[command(about = "Chief TDD orchestrator CLI")]
struct Cli {
    #[arg(long, default_value = ".")]
    project_dir: PathBuf,
    #[arg(long)]
    flow: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    max_retries: Option<usize>,
    #[arg(long)]
    clean_done: bool,
    #[arg(long, default_value_t = 0)]
    tail_events: usize,
    #[arg(long = "requirements")]
    requirements: Vec<String>,
    #[arg(long = "requirements-file")]
    requirements_file: Vec<PathBuf>,
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
            store.reset_db_from_todos_json()?;
            eprintln!("reset complete. retrying...\n");
            run(&cli)
        }
    }
}

fn run(cli: &Cli) -> Result<()> {
    let context = ProjectContext::load(&cli.project_dir)?;
    let configured_flow = context.chief_toml.chief.flow.trim();
    let flow_input = cli.flow.as_deref().unwrap_or(configured_flow);
    let flow_kind: FlowKind = flow_input
        .parse()
        .with_context(|| format!("invalid flow '{}'", flow_input))?;

    if cli.clean_done {
        let removed = context.store.clean_completed_todos_with_commit()?;
        println!("cleaned completed todos ({removed} removed)");
        return Ok(());
    }

    if cli.tail_events > 0 {
        let events = context.store.query_events(EventQuery {
            limit: cli.tail_events,
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
                let tail = output
                    .lines()
                    .rev()
                    .take(context.chief_toml.chief.agent_log_max_output_lines)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                if !tail.trim().is_empty() {
                    println!("{tail}");
                }
            }
            println!();
        }
        return Ok(());
    }

    let requirements_text = load_requirements_text(&cli.requirements, &cli.requirements_file)?;
    if !requirements_text.trim().is_empty() {
        let engine = ChiefEngine::new(context.clone());
        let diff = engine.process_requirements(
            &requirements_text,
            &context.store.todos_path,
            cli.model.clone(),
        )?;
        println!("=== git diff HEAD ===");
        if diff.trim().is_empty() {
            println!("(no diff)");
        } else {
            println!("{diff}");
        }
        return Ok(());
    }

    let engine = ChiefEngine::new(context.clone());
    let max_retries = cli
        .max_retries
        .unwrap_or(context.chief_toml.chief.max_retries.max(1));

    match engine.run_todos_until_done_with_retries(
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
    ) {
        Ok(()) => {
            println!("all todos are done");
            Ok(())
        }
        Err(OrchestratorError::Retryable(err)) => Err(err).context("maximum retry count reached"),
        Err(OrchestratorError::Unrecoverable(err)) => Err(err).context("unrecoverable failure"),
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

fn confirm_db_reset(db_path: &std::path::Path) -> Result<bool> {
    eprint!(
        "Delete {} and rebuild from todos.json? [y/N]: ",
        db_path.display()
    );
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}
