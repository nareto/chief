use super::run_worker_with_executor;
use crate::domain::{JobStatus, Todo, TodoStatus};
use crate::flow::FlowKind;
use crate::orchestrator::{OrchestratorError, retry_with_policy_and_hook_and_delay};
use crate::service::ProjectContext;
use anyhow::anyhow;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use tokio::sync::Mutex;
use uuid::Uuid;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("chief-worker-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("failed to create temporary directory");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn init_git_repo(path: &Path) {
    fs::create_dir_all(path).expect("failed to create project directory");
    let init = Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(path)
        .output()
        .expect("failed to run git init");
    assert!(
        init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let email = Command::new("git")
        .args(["config", "user.email", "worker-test@example.com"])
        .current_dir(path)
        .output()
        .expect("failed to configure git user email");
    assert!(
        email.status.success(),
        "git config user.email failed: {}",
        String::from_utf8_lossy(&email.stderr)
    );

    let name = Command::new("git")
        .args(["config", "user.name", "Worker Test"])
        .current_dir(path)
        .output()
        .expect("failed to configure git user name");
    assert!(
        name.status.success(),
        "git config user.name failed: {}",
        String::from_utf8_lossy(&name.stderr)
    );

    fs::write(path.join("README.md"), "seed\n").expect("failed to write seed file");
    let add = Command::new("git")
        .args(["add", "README.md"])
        .current_dir(path)
        .output()
        .expect("failed to git add seed file");
    assert!(
        add.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let commit = Command::new("git")
        .args(["commit", "-m", "init", "-q"])
        .current_dir(path)
        .output()
        .expect("failed to commit seed file");
    assert!(
        commit.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
}

fn pending_todo(text: &str) -> Todo {
    Todo {
        id: String::new(),
        todo: text.to_owned(),
        expectations: String::new(),
        priority: 0,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    }
    .normalize()
}

#[test]
fn worker_worktree_dir_name_uses_chief_prefix() {
    assert_eq!(
        super::worker_worktree_dir_name("abc-123"),
        "chief_abc-123".to_owned()
    );
}

#[test]
fn unrecoverable_worker_error_marks_job_failed_with_lock_details() {
    let temp = TempDir::new("unrecoverable");
    let project_dir = temp.path.join("project");
    init_git_repo(&project_dir);
    let context = ProjectContext::load(&project_dir).expect("failed to load project context");

    let todo = context
        .store
        .append_todo(pending_todo("simulate unrecoverable lock failure"))
        .expect("failed to append todo");
    let todo_id = todo.id.clone();
    let run_id = "run-unrecoverable-worker";
    context
        .store
        .start_run(run_id)
        .expect("failed to start run record");
    let job = context
        .create_job(
            run_id,
            1,
            FlowKind::SinglePrompt,
            Some(todo_id.clone()),
            None,
        )
        .expect("failed to create job");

    let terminal_error = "git commit failed: Unable to create '/tmp/repo/.git/index.lock': File exists.\nAnother git process seems to be running in this repository".to_owned();
    let result = run_worker_with_executor(
        context.clone(),
        run_id.to_owned(),
        job,
        todo,
        FlowKind::SinglePrompt,
        None,
        false,
        Arc::new(Mutex::new(())),
        Arc::new(AtomicBool::new(false)),
        move |_context,
              _run_id,
              _job,
              _todo,
              _flow_kind,
              _work_dir,
              _model_override,
              _cancel_signal| {
            Err(OrchestratorError::unrecoverable(anyhow!(
                terminal_error.clone()
            )))
        },
    );

    assert_eq!(result.status, "failed");
    assert!(
        result.unrecoverable,
        "failure should be marked unrecoverable"
    );
    let rendered_result_error = result.error.unwrap_or_default();
    assert!(
        rendered_result_error.contains(".git/index.lock"),
        "worker result should preserve lock path details: {rendered_result_error}"
    );
    assert!(
        rendered_result_error
            .to_ascii_lowercase()
            .contains("another git process seems to be running"),
        "worker result should preserve lock contention hint: {rendered_result_error}"
    );

    let jobs = context.store.list_jobs(10).expect("failed to list jobs");
    assert_eq!(jobs.len(), 1, "expected one job record");
    let job = &jobs[0];
    assert_eq!(job.status, JobStatus::Failed);
    let persisted_error = job.error.clone().unwrap_or_default();
    assert!(
        persisted_error.contains(".git/index.lock"),
        "persisted job error should include lock path details: {persisted_error}"
    );

    let todos = context.store.list_todos().expect("failed to list todos");
    let persisted_todo = todos
        .into_iter()
        .find(|item| item.id == todo_id)
        .expect("todo should still exist");
    assert_eq!(
        persisted_todo.status,
        TodoStatus::Pending,
        "todo should be reset to pending after unrecoverable worker failure"
    );
}

#[test]
fn lock_retry_exhaustion_in_executor_keeps_worker_terminal_state_and_error_details() {
    let temp = TempDir::new("retry-exhaustion");
    let project_dir = temp.path.join("project");
    init_git_repo(&project_dir);
    let context = ProjectContext::load(&project_dir).expect("failed to load project context");

    let todo = context
        .store
        .append_todo(pending_todo("simulate lock retry exhaustion"))
        .expect("failed to append todo");
    let todo_id = todo.id.clone();
    let run_id = "run-retry-exhaustion-worker";
    context
        .store
        .start_run(run_id)
        .expect("failed to start run record");
    let job = context
        .create_job(
            run_id,
            1,
            FlowKind::SinglePrompt,
            Some(todo_id.clone()),
            None,
        )
        .expect("failed to create job");

    let terminal_lock_detail = "git commit failed: Unable to create '/tmp/repo/.git/index.lock': File exists.\nAnother git process seems to be running in this repository".to_owned();
    let mut operation_calls = 0usize;
    let mut retry_callbacks = Vec::new();
    let mut sleep_calls = Vec::new();

    let result = run_worker_with_executor(
        context.clone(),
        run_id.to_owned(),
        job,
        todo,
        FlowKind::SinglePrompt,
        None,
        false,
        Arc::new(Mutex::new(())),
        Arc::new(AtomicBool::new(false)),
        |_context, _run_id, _job, _todo, _flow_kind, _work_dir, _model_override, _cancel_signal| {
            let mut first_error = Some(anyhow!(terminal_lock_detail.clone()));
            let retry_outcome = retry_with_policy_and_hook_and_delay::<(), _, _, _, _>(
                4,
                |_attempt, _max| {
                    if let Some(err) = first_error.take() {
                        return Err(OrchestratorError::retryable(err));
                    }

                    operation_calls += 1;
                    Err(OrchestratorError::retryable(anyhow!(
                        terminal_lock_detail.clone()
                    )))
                },
                |_attempt, _max, _err| Some(Duration::from_secs(10)),
                |attempt, max, _err, delay| {
                    retry_callbacks.push((attempt, max, delay.as_secs()));
                },
                |delay| sleep_calls.push(delay.as_secs()),
            );

            match retry_outcome {
                Err(OrchestratorError::Retryable(err)) => {
                    Err(OrchestratorError::unrecoverable(anyhow!(
                        "transient lock/contention retry budget exhausted after 3 retries: {err}"
                    )))
                }
                Err(other) => Err(other),
                Ok(_) => unreachable!("fake lock contention retry flow should fail"),
            }
        },
    );

    assert_eq!(operation_calls, 3, "expected three retry executions");
    assert_eq!(
        retry_callbacks,
        vec![(1, 4, 10), (2, 4, 10), (3, 4, 10)],
        "retry callbacks should be invoked for each retry with 10s delays"
    );
    assert_eq!(
        sleep_calls,
        vec![10, 10, 10],
        "sleep callback should be invoked between retries"
    );
    assert_eq!(result.status, "failed");
    assert!(result.unrecoverable, "failure should remain unrecoverable");
    let rendered_result_error = result.error.unwrap_or_default();
    assert!(
        rendered_result_error.contains("retry budget exhausted"),
        "worker result should preserve retry exhaustion context: {rendered_result_error}"
    );
    assert!(
        rendered_result_error.contains(".git/index.lock"),
        "worker result should preserve lock path details: {rendered_result_error}"
    );

    let jobs = context.store.list_jobs(10).expect("failed to list jobs");
    assert_eq!(jobs.len(), 1, "expected one job record");
    let job = &jobs[0];
    assert_eq!(job.status, JobStatus::Failed);
    let persisted_error = job.error.clone().unwrap_or_default();
    assert!(
        persisted_error.contains("retry budget exhausted"),
        "persisted job error should include retry exhaustion details: {persisted_error}"
    );

    let todos = context.store.list_todos().expect("failed to list todos");
    let persisted_todo = todos
        .into_iter()
        .find(|item| item.id == todo_id)
        .expect("todo should still exist");
    assert_eq!(
        persisted_todo.status,
        TodoStatus::Pending,
        "todo should be reset to pending after retry exhaustion"
    );
}
