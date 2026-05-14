use super::{Cli, InitArgs};
use anyhow::{Context, Result, bail};
use chief::paths;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub(super) const INIT_GITIGNORE_ENTRIES: [&str; 3] = [
    ".chief/chief.db",
    ".chief/chief.example.yaml",
    ".chief/codex-home",
];
pub(super) const INIT_CHIEF_YAML_CONTENT: &str = r#"chief:
  flow: loop_file
  # flow: refactor # uncomment to run queued workflow
  agent: codex
  agent_extra_args: []
  mcp_servers: {} # chief-managed MCP; remove this key to defer to personal Claude/Codex/Cursor config
  max_loop_iterations: 20
  required_stable_iterations: 2
  change_exclude: []
  agent_timeout_seconds: 2700
  # agent_wait_seconds: 60 # fixed wait between agent calls; overrides respect_limits when set
  suite_command_timeout_seconds: 1800
  agent_log_max_output_lines: 10
  agent_log_max_output_chars: 1500
  respect_limits: true
  use_agent_log_truncation_for_stdout_logs: false
"#;

pub(super) const CHIEF_EXAMPLE_YAML_CONTENT: &str =
    include_str!("../../../.chief/chief.example.yaml");

pub(super) fn run_init(cli: &Cli, args: &InitArgs) -> Result<()> {
    // Keep parsing --chief-root for compatibility; init no longer needs a checkout.
    let _ = &args.chief_root;

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

    let chief_example_path = paths::chief_example_path(project_dir);
    let chief_yaml_path = paths::chief_yaml_path(project_dir);

    let mut created = 0usize;
    let mut skipped = 0usize;

    if write_file_if_missing(&chief_example_path, CHIEF_EXAMPLE_YAML_CONTENT)? {
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
        chief_dir.display()
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
