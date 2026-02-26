use super::*;
use crate::git::{
    GIT_TRANSIENT_LOCK_RETRY_ATTEMPTS, git_output_has_transient_lock_contention_signature,
    run_git_command_with_retry,
};
use anyhow::{Context, Result, anyhow};
use std::path::Path;

impl ProjectStore {
    pub(super) fn auto_commit_todos_yaml(&self) -> Result<()> {
        // Unit tests use temp directories that are not git repos.
        if !self.project_dir.join(".git").exists() {
            return Ok(());
        }

        let relative_todos_path = self
            .todos_path
            .strip_prefix(&self.project_dir)
            .unwrap_or_else(|_| Path::new("todos.yaml"))
            .to_string_lossy()
            .to_string();
        let relative_todos = relative_todos_path.as_str();

        let status_args = ["status", "--porcelain", "--", relative_todos];
        let status_output = self.run_git_command(&status_args)?;
        if !status_output.status.success() {
            return Err(anyhow!(
                "git status failed for {}: {}",
                relative_todos,
                String::from_utf8_lossy(&status_output.stderr).trim()
            ));
        }
        if String::from_utf8_lossy(&status_output.stdout)
            .trim()
            .is_empty()
        {
            return Ok(());
        }

        let add_args = ["add", "--", relative_todos];
        let add_output = self.run_git_command(&add_args)?;
        if !add_output.status.success() {
            return Err(anyhow!(
                "git add failed for {}: {}",
                relative_todos,
                String::from_utf8_lossy(&add_output.stderr).trim()
            ));
        }

        let staged_args = ["diff", "--cached", "--name-only", "--", relative_todos];
        let staged_output = self.run_git_command(&staged_args)?;
        if !staged_output.status.success() {
            return Err(anyhow!(
                "git diff --cached failed for {}: {}",
                relative_todos,
                String::from_utf8_lossy(&staged_output.stderr).trim()
            ));
        }
        if String::from_utf8_lossy(&staged_output.stdout)
            .trim()
            .is_empty()
        {
            return Ok(());
        }

        let commit_message = format!("chore(todos): sync {relative_todos}");
        let commit_args = [
            "commit",
            "-m",
            commit_message.as_str(),
            "--",
            relative_todos,
        ];
        let commit_output = self.run_git_command(&commit_args)?;
        if commit_output.status.success() {
            return Ok(());
        }

        let remaining_output = self.run_git_command(&status_args)?;
        if remaining_output.status.success()
            && String::from_utf8_lossy(&remaining_output.stdout)
                .trim()
                .is_empty()
        {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&commit_output.stderr)
            .trim()
            .to_owned();
        let stdout = String::from_utf8_lossy(&commit_output.stdout)
            .trim()
            .to_owned();
        let commit_failure = format!(
            "git commit failed for {}: {}{}{}",
            relative_todos,
            stderr,
            if !stderr.is_empty() && !stdout.is_empty() {
                " | "
            } else {
                ""
            },
            stdout,
        );
        if git_output_has_transient_lock_contention_signature(&commit_output) {
            return Err(anyhow!(
                "transient lock/contention retry budget exhausted after {GIT_TRANSIENT_LOCK_RETRY_ATTEMPTS} retries: {commit_failure}"
            ));
        }
        Err(anyhow!(commit_failure))
    }

    fn run_git_command(&self, args: &[&str]) -> Result<std::process::Output> {
        run_git_command_with_retry(&self.project_dir, args)
            .with_context(|| format!("failed to run git {}", args.join(" ")))
    }
}
