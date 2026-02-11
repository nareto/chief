use anyhow::{Context, Result, bail};
use chief::flow::FlowKind;
use chief::orchestrator::OrchestratorError;
use chief::service::{ChiefEngine, ProjectContext};
use chief::storage::{EventQuery, ProjectStore, db_reset_required_from_anyhow};
use clap::{Args, Parser, Subcommand};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

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
    /// Remove completed todos that have a commit hash.
    CleanDone,
    /// Print recent project events.
    TailEvents(TailEventsArgs),
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

const INIT_GITIGNORE_ENTRIES: [&str; 3] = ["chief.db", "chief.example.yaml", "todos.example.yaml"];

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
            store.reset_db_from_todos_file()?;
            eprintln!("reset complete. retrying...\n");
            run(&cli)
        }
    }
}

fn run_command(cli: &Cli, command: &Commands) -> Result<()> {
    match command {
        Commands::Init(args) => run_init(cli, args),
        Commands::CleanDone => run_clean_done(cli),
        Commands::TailEvents(args) => run_tail_events(cli, args),
    }
}

fn run_init(cli: &Cli, args: &InitArgs) -> Result<()> {
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

    let chief_root_for_checks = if args.chief_root.is_absolute() {
        args.chief_root.clone()
    } else {
        project_dir.join(&args.chief_root)
    };
    let chief_example_source = chief_root_for_checks.join("chief.example.yaml");
    let todos_example_source = chief_root_for_checks.join("todos.example.yaml");
    if !chief_example_source.is_file() {
        bail!("example file not found: {}", chief_example_source.display());
    }
    if !todos_example_source.is_file() {
        bail!("example file not found: {}", todos_example_source.display());
    }

    let chief_example_link = project_dir.join("chief.example.yaml");
    let todos_example_link = project_dir.join("todos.example.yaml");
    let chief_yaml_path = project_dir.join("chief.yaml");
    let todos_yaml_path = project_dir.join("todos.yaml");

    let mut created = 0usize;
    let mut skipped = 0usize;

    if create_file_symlink_if_missing(
        &args.chief_root.join("chief.example.yaml"),
        &chief_example_link,
    )? {
        created += 1;
    } else {
        skipped += 1;
    }
    if create_file_symlink_if_missing(
        &args.chief_root.join("todos.example.yaml"),
        &todos_example_link,
    )? {
        created += 1;
    } else {
        skipped += 1;
    }

    if write_file_if_missing(&chief_yaml_path, "chief: {}\n")? {
        created += 1;
    } else {
        skipped += 1;
    }
    if write_file_if_missing(&todos_yaml_path, "todos: []\n")? {
        created += 1;
    } else {
        skipped += 1;
    }

    ensure_gitignore_entries(&project_dir.join(".gitignore"), &INIT_GITIGNORE_ENTRIES)?;

    println!(
        "initialized chief files in {} (created {created}, skipped {skipped})",
        project_dir.display()
    );
    Ok(())
}

fn ensure_chief_yaml_exists(project_dir: &Path) -> Result<()> {
    let config_path = project_dir.join("chief.yaml");
    if config_path.is_file() {
        return Ok(());
    }
    bail!(
        "missing required chief config at {}. create chief.yaml (run `chief init` or copy chief.example.yaml)",
        config_path.display()
    )
}

fn run(cli: &Cli) -> Result<()> {
    if !matches!(cli.command, Some(Commands::Init(_))) {
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
        .with_context(|| format!("invalid flow '{}'", flow_input))?;

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
        .unwrap_or(context.chief_yaml.chief.max_retries.max(1));

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
            let tail = output
                .lines()
                .rev()
                .take(context.chief_yaml.chief.agent_log_max_output_lines)
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

fn write_file_if_missing(path: &Path, content: &str) -> Result<bool> {
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to create {}", path.display()));
        }
    };
    file.write_all(content.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

fn ensure_gitignore_entries(path: &Path, entries: &[&str]) -> Result<bool> {
    let mut content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", path.display()));
        }
    };

    let mut missing = Vec::new();
    for entry in entries {
        if !gitignore_contains_entry(&content, entry) {
            missing.push(*entry);
        }
    }
    if missing.is_empty() {
        return Ok(false);
    }

    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    for entry in missing {
        content.push_str(entry);
        content.push('\n');
    }

    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

fn gitignore_contains_entry(content: &str, entry: &str) -> bool {
    content.lines().map(str::trim).any(|line| {
        line == entry
            || line.strip_prefix('/').is_some_and(|value| value == entry)
            || line.strip_prefix("./").is_some_and(|value| value == entry)
    })
}

#[cfg(unix)]
fn create_file_symlink_if_missing(target: &Path, link: &Path) -> Result<bool> {
    match std::os::unix::fs::symlink(target, link) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed to create symlink {} -> {}",
                link.display(),
                target.display()
            )
        }),
    }
}

#[cfg(windows)]
fn create_file_symlink_if_missing(target: &Path, link: &Path) -> Result<bool> {
    match std::os::windows::fs::symlink_file(target, link) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed to create symlink {} -> {}",
                link.display(),
                target.display()
            )
        }),
    }
}

fn confirm_db_reset(db_path: &Path) -> Result<bool> {
    eprint!(
        "Delete {} and rebuild from todos.yaml? [y/N]: ",
        db_path.display()
    );
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "chief-{prefix}-{}-{}",
                std::process::id(),
                Uuid::new_v4()
            ));
            fs::create_dir_all(&path).expect("temp dir should be created");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn run_non_init_fails_fast_when_chief_yaml_is_missing() {
        let temp = TempDir::new("run-missing-chief-yaml");
        let cli = Cli {
            project_dir: temp.path.clone(),
            flow: None,
            model: None,
            max_retries: None,
            requirements: Vec::new(),
            requirements_file: Vec::new(),
            command: None,
        };

        let err = run(&cli).expect_err("run should reject projects missing chief.yaml");
        let rendered = err.to_string();
        assert!(
            rendered.contains("missing required chief config"),
            "error should clearly explain missing chief.yaml: {rendered}"
        );
        assert!(
            rendered.contains("chief.yaml"),
            "error should include chief.yaml path: {rendered}"
        );
        assert!(
            !temp.path.join("chief.db").exists(),
            "rejected run should not execute todo processing or create chief.db"
        );
    }

    #[test]
    fn ensure_gitignore_entries_creates_file_when_missing() {
        let temp = TempDir::new("gitignore-create");
        let gitignore_path = temp.path.join(".gitignore");

        let changed =
            ensure_gitignore_entries(&gitignore_path, &INIT_GITIGNORE_ENTRIES).expect("ok");

        assert!(changed);
        assert_eq!(
            fs::read_to_string(gitignore_path).expect("gitignore should exist"),
            "chief.db\nchief.example.yaml\ntodos.example.yaml\n"
        );
    }

    #[test]
    fn ensure_gitignore_entries_appends_only_missing_entries() {
        let temp = TempDir::new("gitignore-append");
        let gitignore_path = temp.path.join(".gitignore");
        fs::write(&gitignore_path, "target/\nchief.db").expect("seed gitignore should be written");

        let changed =
            ensure_gitignore_entries(&gitignore_path, &INIT_GITIGNORE_ENTRIES).expect("ok");

        assert!(changed);
        assert_eq!(
            fs::read_to_string(&gitignore_path).expect("gitignore should be readable"),
            "target/\nchief.db\nchief.example.yaml\ntodos.example.yaml\n"
        );
    }

    #[test]
    fn ensure_gitignore_entries_is_idempotent() {
        let temp = TempDir::new("gitignore-idempotent");
        let gitignore_path = temp.path.join(".gitignore");
        fs::write(
            &gitignore_path,
            "/chief.db\n./chief.example.yaml\ntodos.example.yaml\n",
        )
        .expect("seed gitignore should be written");

        let changed =
            ensure_gitignore_entries(&gitignore_path, &INIT_GITIGNORE_ENTRIES).expect("ok");

        assert!(!changed);
        assert_eq!(
            fs::read_to_string(&gitignore_path).expect("gitignore should be readable"),
            "/chief.db\n./chief.example.yaml\ntodos.example.yaml\n"
        );
    }
}
