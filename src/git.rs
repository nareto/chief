use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const GIT_TRANSIENT_LOCK_RETRY_ATTEMPTS: usize = 3;
#[cfg(not(test))]
pub const GIT_TRANSIENT_LOCK_RETRY_DELAY: Duration = Duration::from_secs(10);
#[cfg(test)]
pub const GIT_TRANSIENT_LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);

fn command_with_safe_directory(cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-c").arg("safe.directory=*").current_dir(cwd);
    cmd
}

fn run_git_command_once(cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
    command_with_safe_directory(cwd)
        .args(args)
        .output()
        .with_context(|| format!("git command failed to start: git {}", args.join(" ")))
}

pub fn has_transient_lock_contention_signature(message: &str) -> bool {
    let text = message.to_ascii_lowercase();
    let has_index_lock_path = text.contains(".git/index.lock") || text.contains(".git\\index.lock");
    (text.contains("unable to create") && has_index_lock_path)
        || text.contains("another git process seems to be running")
        || text.contains("resource busy")
}

pub fn git_output_has_transient_lock_contention_signature(output: &std::process::Output) -> bool {
    has_transient_lock_contention_signature(&String::from_utf8_lossy(&output.stderr))
        || has_transient_lock_contention_signature(&String::from_utf8_lossy(&output.stdout))
}

pub fn run_git_command_with_retry(cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
    run_git_command_with_retry_and_sleep(cwd, args, std::thread::sleep)
}

pub fn run_git_command_with_retry_and_sleep<S>(
    cwd: &Path,
    args: &[&str],
    mut sleep: S,
) -> Result<std::process::Output>
where
    S: FnMut(Duration),
{
    let mut output = run_git_command_once(cwd, args)?;
    let mut retries = 0usize;
    while !output.status.success()
        && retries < GIT_TRANSIENT_LOCK_RETRY_ATTEMPTS
        && git_output_has_transient_lock_contention_signature(&output)
    {
        retries += 1;
        sleep(GIT_TRANSIENT_LOCK_RETRY_DELAY);
        output = run_git_command_once(cwd, args)?;
    }
    Ok(output)
}

pub trait GitOps: Send + Sync {
    fn repo_root(&self) -> &Path;
    fn head_commit(&self, cwd: &Path) -> Result<String>;
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
    pub fn discover(start_dir: impl AsRef<Path>) -> Result<Self> {
        let output =
            run_git_command_with_retry(start_dir.as_ref(), &["rev-parse", "--show-toplevel"])
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
        let output = run_git_command_with_retry(cwd, args)?;
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
        let output = run_git_command_with_retry(cwd, args)?;
        if !output.status.success() {
            return Err(anyhow!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }
}

impl GitOps for ShellGitOps {
    fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    fn head_commit(&self, cwd: &Path) -> Result<String> {
        self.run_capture(cwd, &["rev-parse", "HEAD"])
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

        let mut args = vec!["diff".to_owned(), "--stat".to_owned(), "--".to_owned()];
        args.extend(files.iter().cloned());
        let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let output =
            run_git_command_with_retry(cwd, &arg_refs).context("failed to run git diff --stat")?;
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

        let output = run_git_command_with_retry(cwd, &["commit", "-m", message])
            .context("failed to run git commit")?;

        // If nothing changed, commit can fail with non-zero. In that case keep current HEAD.
        if !output.status.success() {
            let has_changes = !self.changed_files(cwd)?.is_empty();
            if has_changes {
                return Err(anyhow!(
                    "git commit failed and there are still pending changes: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
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

        let mut commit_args = vec!["commit", "-m", message, "--"];
        commit_args.extend(paths);
        let commit_output =
            run_git_command_with_retry(cwd, &commit_args).context("failed to run git commit")?;

        if !commit_output.status.success() {
            // If nothing actually changed, the commit is a no-op — that's fine.
            let staged_output = self.run_capture(cwd, &["diff", "--cached", "--name-only"])?;
            if !staged_output.is_empty() {
                return Err(anyhow!(
                    "git commit for paths {paths:?} failed: {}",
                    String::from_utf8_lossy(&commit_output.stderr).trim()
                ));
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

#[cfg(test)]
mod tests {
    use super::{
        GIT_TRANSIENT_LOCK_RETRY_ATTEMPTS, has_transient_lock_contention_signature,
        run_git_command_with_retry_and_sleep,
    };
    use std::cell::Cell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use uuid::Uuid;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!("chief-git-{label}-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("failed creating temporary directory");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-c")
            .arg("safe.directory=*")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap_or_else(|err| panic!("failed running git {}: {err}", args.join(" ")));
        assert!(
            output.status.success(),
            "git {} failed: stdout={} stderr={}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_git_repo(path: &Path) {
        fs::create_dir_all(path).expect("failed creating git repo directory");
        run_git(path, &["init", "-q"]);
    }

    #[test]
    fn transient_lock_signature_matches_expected_git_error() {
        let err = "fatal: Unable to create '/tmp/repo/.git/index.lock': File exists.
Another git process seems to be running in this repository";
        assert!(has_transient_lock_contention_signature(err));
    }

    #[test]
    fn git_command_retry_recovers_after_lock_is_cleared() {
        let temp = TempDir::new("retry-recovers");
        init_git_repo(&temp.path);
        fs::write(temp.path.join("README.md"), "seed\n").expect("failed to write seed file");
        let lock = temp.path.join(".git").join("index.lock");
        fs::write(&lock, "lock").expect("failed to create lock file");

        let sleep_calls = Cell::new(0usize);
        let output =
            run_git_command_with_retry_and_sleep(&temp.path, &["add", "README.md"], |_| {
                sleep_calls.set(sleep_calls.get() + 1);
                let _ = fs::remove_file(&lock);
            })
            .expect("git add should execute");

        assert!(
            output.status.success(),
            "git add should succeed after retry"
        );
        assert_eq!(
            sleep_calls.get(),
            1,
            "expected exactly one retry sleep call"
        );
    }

    #[test]
    fn git_command_retry_stops_after_retry_budget_exhausted() {
        let temp = TempDir::new("retry-exhausted");
        init_git_repo(&temp.path);
        fs::write(temp.path.join("README.md"), "seed\n").expect("failed to write seed file");
        let lock = temp.path.join(".git").join("index.lock");
        fs::write(&lock, "lock").expect("failed to create lock file");

        let sleep_calls = Cell::new(0usize);
        let output =
            run_git_command_with_retry_and_sleep(&temp.path, &["add", "README.md"], |_| {
                sleep_calls.set(sleep_calls.get() + 1);
            })
            .expect("git add should execute");

        assert!(
            !output.status.success(),
            "git add should remain failed when lock persists"
        );
        assert_eq!(
            sleep_calls.get(),
            GIT_TRANSIENT_LOCK_RETRY_ATTEMPTS,
            "should sleep once per configured retry"
        );
        let _ = fs::remove_file(lock);
    }
}
