use super::{Cli, InitArgs};
use anyhow::{Context, Result, bail};
use chief::paths;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};

const BD_AGENTS_TEMPLATE_FILE_NAME: &str = "bd_AGENTS.md";

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
  mcp_servers: {} # chief-managed MCP; remove this key to defer to personal Claude/Codex config
  max_loop_iterations: 20
  required_stable_iterations: 2
  agent_timeout_seconds: 2700
  suite_command_timeout_seconds: 1800
  agent_log_max_output_lines: 10
  agent_log_max_output_chars: 1500
  respect_limits: true
  use_agent_log_truncation_for_stdout_logs: false
"#;

pub(super) fn run_init(cli: &Cli, args: &InitArgs) -> Result<()> {
    run_init_with_bd_command(cli, args, Path::new("bd"))
}

pub(super) fn run_init_with_bd_command(
    cli: &Cli,
    args: &InitArgs,
    bd_command: &Path,
) -> Result<()> {
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
    let chief_example_source = paths::chief_example_path(&chief_root_for_checks);
    if !chief_example_source.is_file() {
        bail!(
            "example file not found: {}\n\
             hint: use `chief init --chief-root <path>` to point to the chief repo directory",
            chief_example_source.display()
        );
    }
    if args.beads {
        let bd_agents_template_source = chief_root_for_checks.join(BD_AGENTS_TEMPLATE_FILE_NAME);
        if !bd_agents_template_source.is_file() {
            bail!(
                "bd agents template not found: {}\n\
                 hint: use `chief init --chief-root <path>` to point to the chief repo directory",
                bd_agents_template_source.display()
            );
        }
    }

    let chief_dir = paths::chief_dir(project_dir);
    fs::create_dir_all(&chief_dir)
        .with_context(|| format!("failed to create {}", chief_dir.display()))?;

    let chief_example_link = paths::chief_example_path(project_dir);
    let chief_yaml_path = paths::chief_yaml_path(project_dir);

    let mut created = 0usize;
    let mut skipped = 0usize;

    if create_file_symlink_if_missing(&chief_example_source, &chief_example_link)? {
        created += 1;
    } else {
        skipped += 1;
    }
    if write_file_if_missing(&chief_yaml_path, INIT_CHIEF_YAML_CONTENT)? {
        created += 1;
    } else {
        skipped += 1;
    }

    let mut gitignore_entries: Vec<&str> = INIT_GITIGNORE_ENTRIES.to_vec();
    if args.beads {
        let bd_agents_template_arg = args.chief_root.join(BD_AGENTS_TEMPLATE_FILE_NAME);
        run_bd_init_if_needed(project_dir, &bd_agents_template_arg, bd_command)?;
        gitignore_entries.push(".beads");
    }
    ensure_gitignore_entries(&project_dir.join(".gitignore"), &gitignore_entries)?;

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

fn run_bd_init_if_needed(
    project_dir: &Path,
    agents_template: &Path,
    bd_command: &Path,
) -> Result<bool> {
    if project_dir.join(".beads").exists() {
        return Ok(false);
    }

    let mut child = Command::new(bd_command)
        .args(["init", "--agents-template"])
        .arg(agents_template)
        .current_dir(project_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "failed to start `{} init --agents-template {}`",
                bd_command.display(),
                agents_template.display()
            )
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        match stdin.write_all(b"n\n") {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => {}
            Err(err) => return Err(err).context("failed to answer `bd init` prompt"),
        }
    }

    let output = child
        .wait_with_output()
        .context("failed to wait for `bd init`")?;
    if output.status.success() {
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let mut detail = format!(
        "`bd init --agents-template {}` failed with status {}",
        agents_template.display(),
        output.status
    );
    if !stderr.is_empty() {
        detail.push_str("; stderr: ");
        detail.push_str(&stderr);
    }
    if !stdout.is_empty() {
        detail.push_str("; stdout: ");
        detail.push_str(&stdout);
    }
    bail!(detail)
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
