use super::{
    ChiefEngine, ProjectContext, ProjectRegistry, is_transient_lock_contention_error,
    retry_transient_lock_contention_with_delay,
};
use crate::domain::{RunExitStatus, Todo, TodoStatus};
use crate::flow::{FlowKind, TodoOutcome};
use crate::orchestrator::OrchestratorError;
use anyhow::anyhow;
use rusqlite::Connection;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("chief-project-registry-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("failed creating temporary directory");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn init_git_repo(path: &Path) {
    fs::create_dir_all(path).expect("failed creating git repo directory");
    let output = Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(path)
        .output()
        .expect("failed to run git init");
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn pending_todo(text: &str) -> Todo {
    Todo {
        id: String::new(),
        todo: text.to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    }
    .normalize()
}

#[test]
fn chief_engine_start_run_requires_chief_yaml() {
    let root = TempDir::new("missing-chief-yaml");
    let project_dir = root.path.join("project");
    init_git_repo(&project_dir);
    let context = ProjectContext::load(&project_dir).expect("project context should load");
    assert!(
        !context.config_path.exists(),
        "fixture should intentionally omit chief.yaml"
    );
    assert!(
        !context.store.db_path.exists(),
        "chief.db should not exist before start_run"
    );

    let err = ChiefEngine::new(context.clone())
        .start_run()
        .expect_err("start_run should fail without chief.yaml");
    let rendered = err.to_string();
    assert!(
        rendered.contains("missing required chief config"),
        "error should explain missing config: {rendered}"
    );
    assert!(
        rendered.contains("chief.yaml"),
        "error should reference chief.yaml path: {rendered}"
    );
    assert!(
        !context.store.db_path.exists(),
        "rejected start_run should not create chief.db"
    );
}

#[test]
fn worker_worktree_dir_name_uses_chief_prefix() {
    assert_eq!(
        super::worker_worktree_dir_name("abc-123"),
        "chief_abc-123".to_owned()
    );
}

#[test]
fn discover_merges_projects_dir_and_manual_projects() {
    let projects_root = TempDir::new("projects-root");
    let manual_root = TempDir::new("manual-root");

    let in_tree = projects_root.path.join("in-tree");
    let ignored = projects_root.path.join("not-a-repo");
    let manual = manual_root.path.join("manual-repo");
    init_git_repo(&in_tree);
    fs::create_dir_all(&ignored).expect("failed creating non-repo directory");
    init_git_repo(&manual);

    let registry = ProjectRegistry::discover(&projects_root.path, std::slice::from_ref(&manual))
        .expect("project discovery should succeed");
    let names = registry
        .list_projects()
        .into_iter()
        .map(|project| project.name)
        .collect::<Vec<_>>();

    assert_eq!(names, vec!["in-tree".to_owned(), "manual-repo".to_owned()]);
}

#[test]
fn discover_dedupes_manual_project_already_in_projects_dir() {
    let projects_root = TempDir::new("dedupe-root");
    let shared = projects_root.path.join("shared");
    init_git_repo(&shared);

    let registry = ProjectRegistry::discover(&projects_root.path, std::slice::from_ref(&shared))
        .expect("project discovery should succeed");
    let names = registry
        .list_projects()
        .into_iter()
        .map(|project| project.name)
        .collect::<Vec<_>>();

    assert_eq!(names, vec!["shared".to_owned()]);
}

#[test]
fn discover_errors_on_duplicate_project_names() {
    let projects_root = TempDir::new("dupe-names-root");
    let manual_root = TempDir::new("dupe-names-manual");

    let in_tree = projects_root.path.join("same-name");
    let manual = manual_root.path.join("same-name");
    init_git_repo(&in_tree);
    init_git_repo(&manual);

    let err = ProjectRegistry::discover(&projects_root.path, std::slice::from_ref(&manual))
        .expect_err("discovery should fail for duplicate project names");
    assert!(
        err.to_string().contains("duplicate project name"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn run_todos_until_done_with_retries_stops_after_first_terminal_todo_failure() {
    let root = TempDir::new("cli-fail-fast");
    let project_dir = root.path.join("project");
    init_git_repo(&project_dir);
    fs::create_dir_all(crate::paths::chief_dir(&project_dir))
        .expect(".chief directory should be created");
    fs::write(crate::paths::chief_yaml_path(&project_dir), "chief: {}\n")
        .expect("failed to write chief.yaml fixture");

    let context = ProjectContext::load(&project_dir).expect("failed to load project context");
    let first = context
        .store
        .append_todo(pending_todo("first todo"))
        .expect("failed to append first todo");
    let second = context
        .store
        .append_todo(pending_todo("second todo"))
        .expect("failed to append second todo");

    let engine = ChiefEngine::new(context.clone());
    let mut runner_calls = 0usize;
    let mut completed_ids = Vec::new();

    let result = engine.run_todos_until_done_with_retries_with_runner(
        FlowKind::Refactor,
        None,
        3,
        |outcome: &TodoOutcome| completed_ids.push(outcome.todo_id.clone()),
        |_attempt, _total, _err| {},
        |_run_id, _flow_kind, _model_override, _max_retries, _retry_hook| {
            runner_calls += 1;
            Err(OrchestratorError::retryable(anyhow!(
                "simulated terminal todo failure"
            )))
        },
    );

    assert!(
        matches!(result, Err(OrchestratorError::Retryable(_))),
        "todo queue should fail on the first terminal todo failure"
    );
    assert_eq!(
        runner_calls, 1,
        "CLI todo queue should stop immediately instead of trying another todo"
    );
    assert!(
        completed_ids.is_empty(),
        "no todo completion callback should fire on immediate terminal failure"
    );

    let todos = context.store.list_todos().expect("failed to list todos");
    let pending_ids = todos
        .iter()
        .filter(|todo| todo.status == TodoStatus::Pending)
        .map(|todo| todo.id.clone())
        .collect::<Vec<_>>();
    assert!(
        pending_ids.contains(&first.id),
        "first todo should remain pending after terminal failure"
    );
    assert!(
        pending_ids.contains(&second.id),
        "second todo should never be picked once first todo fails terminally"
    );

    let conn = Connection::open(&context.store.db_path).expect("failed to open chief.db");
    let (run_status, run_exit_status): (String, Option<String>) = conn
        .query_row(
            "SELECT status, exit_status FROM runs ORDER BY started_at DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("failed to query latest run");
    assert_eq!(run_status, "finished");
    assert_eq!(
        run_exit_status.as_deref(),
        Some(RunExitStatus::Failure.as_str()),
        "CLI run should finish with failure after terminal todo failure"
    );
}

#[test]
fn loop_file_flow_forces_single_outer_retry_attempt() {
    let root = TempDir::new("loop-file-max-retries");
    let project_dir = root.path.join("project");
    init_git_repo(&project_dir);
    fs::create_dir_all(crate::paths::chief_dir(&project_dir))
        .expect(".chief directory should be created");
    fs::write(crate::paths::chief_yaml_path(&project_dir), "chief: {}\n")
        .expect("failed to write chief.yaml fixture");

    let context = ProjectContext::load(&project_dir).expect("failed to load project context");
    let engine = ChiefEngine::new(context);
    let mut observed_max_retries = Vec::new();

    let result = engine.run_todos_until_done_with_retries_with_runner(
        FlowKind::LoopFile,
        None,
        9,
        |_outcome: &TodoOutcome| {},
        |_attempt, _total, _err| {},
        |_run_id, flow_kind, _model_override, max_retries, _retry_hook| {
            observed_max_retries.push((flow_kind, max_retries));
            Ok(None)
        },
    );

    assert!(
        result.is_ok(),
        "loop_file queue run should complete cleanly"
    );
    assert_eq!(
        observed_max_retries.len(),
        1,
        "runner should be called once"
    );
    assert_eq!(
        observed_max_retries[0],
        (FlowKind::LoopFile, 1),
        "loop_file flow must force max_retries to 1 regardless of caller value"
    );
}

#[test]
fn transient_lock_contention_signature_is_detected() {
    let err = anyhow!(
        "git commit failed: Unable to create '/tmp/repo/.git/index.lock': File exists.\nAnother git process seems to be running in this repository"
    );
    assert!(is_transient_lock_contention_error(&err));
}

#[test]
fn transient_lock_contention_io_error_kinds_are_detected() {
    let would_block = anyhow!(io::Error::new(io::ErrorKind::WouldBlock, "would block"));
    let timed_out = anyhow!(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
    let interrupted = anyhow!(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));

    assert!(is_transient_lock_contention_error(&would_block));
    assert!(is_transient_lock_contention_error(&timed_out));
    assert!(is_transient_lock_contention_error(&interrupted));
}

#[test]
fn transient_lock_retry_path_retries_three_times_with_ten_second_delays() {
    let mut operation_calls = 0usize;
    let mut retry_callbacks = Vec::new();
    let mut sleeps = Vec::new();
    let err = retry_transient_lock_contention_with_delay::<(), _, _, _>(
            anyhow!(
                "git command failed: Unable to create '/tmp/repo/.git/index.lock': File exists.\nAnother git process seems to be running in this repository"
            ),
            || {
                operation_calls += 1;
                Err(OrchestratorError::retryable(anyhow!(
                    "git command failed: Unable to create '/tmp/repo/.git/index.lock': File exists.\nAnother git process seems to be running in this repository"
                )))
            },
            |attempt, total, _err, delay| {
                retry_callbacks.push((attempt, total, delay.as_secs()));
            },
            |delay| sleeps.push(delay.as_secs()),
        )
        .expect_err("transient lock retries should eventually fail");

    assert!(matches!(err, OrchestratorError::Unrecoverable(_)));
    let rendered = err.as_error().to_string();
    assert!(
        rendered.contains("retry budget exhausted"),
        "terminal lock retry failure should mention exhausted retry budget: {rendered}"
    );
    assert!(
        rendered.contains(".git/index.lock"),
        "terminal lock retry failure should preserve index.lock details: {rendered}"
    );
    assert!(
        rendered
            .to_ascii_lowercase()
            .contains("another git process seems to be running"),
        "terminal lock retry failure should preserve conflict hint: {rendered}"
    );
    assert_eq!(
        operation_calls, 3,
        "exactly three retry executions expected"
    );
    assert_eq!(
        retry_callbacks,
        vec![(1, 3, 10), (2, 3, 10), (3, 3, 10)],
        "retry callbacks should report attempt counters and 10-second delays"
    );
    assert_eq!(
        sleeps,
        vec![10, 10, 10],
        "sleep should be invoked between retries"
    );
}

#[test]
fn transient_io_retry_path_retries_three_times_with_ten_second_delays() {
    let mut operation_calls = 0usize;
    let mut retry_callbacks = Vec::new();
    let mut sleeps = Vec::new();
    let err = retry_transient_lock_contention_with_delay::<(), _, _, _>(
        anyhow!(io::Error::new(io::ErrorKind::WouldBlock, "index.lock busy")),
        || {
            operation_calls += 1;
            Err(OrchestratorError::retryable(anyhow!(io::Error::new(
                io::ErrorKind::TimedOut,
                "git index lock timed out",
            ))))
        },
        |attempt, total, _err, delay| {
            retry_callbacks.push((attempt, total, delay.as_secs()));
        },
        |delay| sleeps.push(delay.as_secs()),
    )
    .expect_err("transient io retries should eventually fail");

    assert!(matches!(err, OrchestratorError::Unrecoverable(_)));
    let rendered = err.as_error().to_string();
    assert!(
        rendered.contains("retry budget exhausted"),
        "terminal io retry failure should mention exhausted retry budget: {rendered}"
    );
    assert!(
        rendered.to_ascii_lowercase().contains("timed out"),
        "terminal io retry failure should preserve final io details: {rendered}"
    );
    assert_eq!(
        operation_calls, 3,
        "exactly three retry executions expected"
    );
    assert_eq!(
        retry_callbacks,
        vec![(1, 3, 10), (2, 3, 10), (3, 3, 10)],
        "retry callbacks should report attempt counters and 10-second delays"
    );
    assert_eq!(
        sleeps,
        vec![10, 10, 10],
        "sleep should be invoked between retries"
    );
}

#[test]
fn transient_lock_retry_path_can_succeed_after_retries() {
    let mut operation_calls = 0usize;
    let mut sleeps = Vec::new();
    let outcome = retry_transient_lock_contention_with_delay(
            anyhow!(
                "git command failed: Unable to create '/tmp/repo/.git/index.lock': File exists.\nAnother git process seems to be running in this repository"
            ),
            || {
                operation_calls += 1;
                if operation_calls < 2 {
                    Err(OrchestratorError::retryable(anyhow!(
                        "git command failed: Unable to create '/tmp/repo/.git/index.lock': File exists.\nAnother git process seems to be running in this repository"
                    )))
                } else {
                    Ok("ok")
                }
            },
            |_attempt, _total, _err, _delay| {},
            |delay| sleeps.push(delay.as_secs()),
        )
        .expect("transient lock retry should recover");

    assert_eq!(outcome, "ok");
    assert_eq!(operation_calls, 2);
    assert_eq!(sleeps, vec![10, 10]);
}

#[test]
fn non_matching_runtime_failure_is_not_classified_as_transient_lock_contention() {
    let err = anyhow!("git merge failed: conflict in working tree");
    assert!(!is_transient_lock_contention_error(&err));

    let mut operation_calls = 0usize;
    let mut retry_callbacks = 0usize;
    let mut sleeps = Vec::new();
    let result = retry_transient_lock_contention_with_delay::<(), _, _, _>(
        err,
        || {
            operation_calls += 1;
            Ok(())
        },
        |_attempt, _total, _err, _delay| retry_callbacks += 1,
        |delay| sleeps.push(delay.as_secs()),
    );

    assert!(matches!(result, Err(OrchestratorError::Retryable(_))));
    assert_eq!(
        operation_calls, 0,
        "non-transient errors should not enter lock retry path"
    );
    assert_eq!(retry_callbacks, 0);
    assert!(sleeps.is_empty());
}
