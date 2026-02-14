use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::process::Command;

pub trait GitOps: Send + Sync {
    fn repo_root(&self) -> &Path;
    fn changed_files(&self, cwd: &Path) -> Result<Vec<String>>;
    fn diff(&self, cwd: &Path, against_ref: Option<&str>) -> Result<String>;
    fn diff_summary_for_files(&self, cwd: &Path, files: &[String]) -> Result<String>;
    fn commit_committer_timestamp_rfc3339(&self, cwd: &Path, commit_hash: &str) -> Result<String>;
    fn commit_and_tag(&self, cwd: &Path, message: &str) -> Result<String>;
    fn commit_paths(&self, cwd: &Path, paths: &[&str], message: &str) -> Result<()>;
    fn create_worktree(&self, branch: &str, worktree_path: &Path) -> Result<()>;
    fn merge_branch_into_main(&self, branch: &str, main_branch: &str) -> Result<()>;
    fn remove_worktree(&self, worktree_path: &Path, branch: &str) -> Result<()>;
    fn current_branch(&self) -> Result<String>;
}

#[derive(Debug, Clone)]
pub struct ShellGitOps {
    repo_root: PathBuf,
}

impl ShellGitOps {
    fn command_with_safe_directory(cwd: &Path) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-c").arg("safe.directory=*").current_dir(cwd);
        cmd
    }

    pub fn discover(start_dir: impl AsRef<Path>) -> Result<Self> {
        let output = Self::command_with_safe_directory(start_dir.as_ref())
            .arg("rev-parse")
            .arg("--show-toplevel")
            .output()
            .context("failed to execute git rev-parse")?;
        if !output.status.success() {
            return Err(anyhow!(
                "failed to discover git repo root: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok(Self {
            repo_root: PathBuf::from(root),
        })
    }

    fn run_capture(&self, cwd: &Path, args: &[&str]) -> Result<String> {
        let output = Self::command_with_safe_directory(cwd)
            .args(args)
            .output()
            .with_context(|| format!("git command failed to start: git {}", args.join(" ")))?;
        if !output.status.success() {
            return Err(anyhow!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn run_status(&self, cwd: &Path, args: &[&str]) -> Result<()> {
        let status = Self::command_with_safe_directory(cwd)
            .args(args)
            .status()
            .with_context(|| format!("git command failed to start: git {}", args.join(" ")))?;
        if !status.success() {
            return Err(anyhow!("git {} failed", args.join(" ")));
        }
        Ok(())
    }
}

impl GitOps for ShellGitOps {
    fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    fn changed_files(&self, cwd: &Path) -> Result<Vec<String>> {
        let output = self.run_capture(cwd, &["status", "--porcelain", "--untracked-files=all"])?;
        let files = output
            .lines()
            .filter_map(|line| line.split_whitespace().last().map(str::to_owned))
            .collect();
        Ok(files)
    }

    fn diff(&self, cwd: &Path, against_ref: Option<&str>) -> Result<String> {
        let mut args = vec!["diff"];
        if let Some(reference) = against_ref {
            args.push(reference);
        }
        self.run_capture(cwd, &args)
    }

    fn diff_summary_for_files(&self, cwd: &Path, files: &[String]) -> Result<String> {
        if files.is_empty() {
            return Ok(String::new());
        }

        let mut cmd = Self::command_with_safe_directory(cwd);
        cmd.arg("diff").arg("--stat").arg("--");
        for file in files {
            cmd.arg(file);
        }

        let output = cmd.output().context("failed to run git diff --stat")?;
        if !output.status.success() {
            return Err(anyhow!(
                "git diff --stat failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn commit_committer_timestamp_rfc3339(&self, cwd: &Path, commit_hash: &str) -> Result<String> {
        self.run_capture(cwd, &["show", "-s", "--format=%cI", commit_hash])
    }

    fn commit_and_tag(&self, cwd: &Path, message: &str) -> Result<String> {
        self.run_status(cwd, &["add", "-A"])?;

        let status = Self::command_with_safe_directory(cwd)
            .args(["commit", "-m", message])
            .status()
            .context("failed to run git commit")?;

        // If nothing changed, commit can fail with non-zero. In that case keep current HEAD.
        if !status.success() {
            let has_changes = !self.changed_files(cwd)?.is_empty();
            if has_changes {
                return Err(anyhow!(
                    "git commit failed and there are still pending changes"
                ));
            }
        }

        let commit_hash = self.run_capture(cwd, &["rev-parse", "HEAD"])?;
        let tag = format!("chief/{}", &commit_hash.chars().take(8).collect::<String>());
        let _ = self.run_status(cwd, &["tag", &tag]);
        Ok(commit_hash)
    }

    fn commit_paths(&self, cwd: &Path, paths: &[&str], message: &str) -> Result<()> {
        let mut add_args = vec!["add", "--"];
        add_args.extend(paths);
        self.run_status(cwd, &add_args)?;

        let status = Self::command_with_safe_directory(cwd)
            .args(["commit", "-m", message, "--"])
            .args(paths)
            .status()
            .context("failed to run git commit")?;

        if !status.success() {
            // If nothing actually changed, the commit is a no-op — that's fine.
            let output = self.run_capture(cwd, &["diff", "--cached", "--name-only"])?;
            if !output.is_empty() {
                return Err(anyhow!("git commit for paths {paths:?} failed"));
            }
        }
        Ok(())
    }

    fn create_worktree(&self, branch: &str, worktree_path: &Path) -> Result<()> {
        self.run_status(
            &self.repo_root,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                worktree_path
                    .to_str()
                    .ok_or_else(|| anyhow!("invalid worktree path"))?,
                "HEAD",
            ],
        )
    }

    fn merge_branch_into_main(&self, branch: &str, main_branch: &str) -> Result<()> {
        self.run_status(&self.repo_root, &["checkout", main_branch])?;
        self.run_status(
            &self.repo_root,
            &["merge", "--no-ff", branch, "-m", &format!("merge {branch}")],
        )
    }

    fn remove_worktree(&self, worktree_path: &Path, branch: &str) -> Result<()> {
        self.run_status(
            &self.repo_root,
            &[
                "worktree",
                "remove",
                "--force",
                worktree_path
                    .to_str()
                    .ok_or_else(|| anyhow!("invalid worktree path"))?,
            ],
        )?;

        // Branch may already be merged/deleted, ignore failures.
        let _ = self.run_status(&self.repo_root, &["branch", "-D", branch]);
        Ok(())
    }

    fn current_branch(&self) -> Result<String> {
        self.run_capture(&self.repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
    }
}
