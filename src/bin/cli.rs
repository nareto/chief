use anyhow::{Context, Result};
use chief::service::{ChiefEngine, ProjectContext};
use chief::storage::EventQuery;
use clap::Parser;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "chief-cli")]
#[command(about = "Chief TDD orchestrator CLI")]
struct Cli {
    #[arg(long, default_value = ".")]
    project_dir: PathBuf,
    #[arg(long, default_value = "tdd")]
    flow: String,
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
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let context = ProjectContext::load(&cli.project_dir)?;

    if cli.clean_done {
        let mut todo_file = context.store.load_todo_file()?;
        todo_file.todos.retain(|todo| {
            !(todo.status == chief::domain::TodoStatus::Done && todo.done_at_commit.is_some())
        });
        context.store.save_todo_file(&todo_file)?;
        context.store.sync_todos_from_file()?;
        println!("cleaned completed todos");
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

    let mut failure_count = 0usize;
    loop {
        match engine.run_next_todo(&cli.flow, cli.model.clone()) {
            Ok(Some(outcome)) => {
                failure_count = 0;
                println!(
                    "completed todo {}{}",
                    outcome.todo_id,
                    outcome
                        .commit_hash
                        .as_deref()
                        .map(|hash| format!(" @ {hash}"))
                        .unwrap_or_default()
                );
            }
            Ok(None) => {
                println!("all todos are done");
                break;
            }
            Err(err) => {
                failure_count += 1;
                eprintln!("run failed ({failure_count}/{max_retries}): {err:#}");
                if failure_count >= max_retries {
                    return Err(err).context("maximum retry count reached");
                }
            }
        }
    }

    Ok(())
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
