use super::{Cli, InitArgs};
use anyhow::{Context, Result, bail};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub(super) const INIT_GITIGNORE_ENTRIES: [&str; 3] =
    ["chief.db", "chief.example.yaml", "todos.example.yaml"];
pub(super) const INIT_CHIEF_YAML_CONTENT: &str = r#"chief:
  flow: loop_file
  # flow: single_prompt # uncomment to run queued workflow using todos.yaml
  agent: codex
  agent_extra_args: []
  max_loop_iterations: 20
  required_stable_iterations: 2
  agent_timeout_seconds: 2700
  suite_command_timeout_seconds: 1800
  agent_log_max_output_lines: 10
  agent_log_max_output_chars: 1500
  use_agent_log_truncation_for_stdout_logs: false
"#;

pub(super) fn run_init(cli: &Cli, args: &InitArgs) -> Result<()> {
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

    if write_file_if_missing(&chief_yaml_path, INIT_CHIEF_YAML_CONTENT)? {
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

pub(super) fn ensure_gitignore_entries(path: &Path, entries: &[&str]) -> Result<bool> {
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
