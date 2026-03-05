use super::readiness::{chief_yaml_content_hash, readiness_chief_yaml_hash};
use super::{
    ApiService, RETRY_CLEANUP_DISCARDED_MSG_PREFIX, is_internal_workspace_state_file,
    parse_todo_status_input, resolve_last_done_todo_committed_at,
};
use crate::api::error::ApiError;
use crate::api::types::{RunSuiteCheckRequest, StartProjectRequest, UpdateChiefYamlRequest};
use axum::body::to_bytes;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chief::domain::{EventType, Todo, TodoStatus};
use chief::flow::SuiteCommandKind;
use chief::git::GitOps;
use chief::scheduler::Scheduler;
use chief::service::ProjectRegistry;
use chief::storage::{EventQuery, ProjectStore, ReadinessStatus};
use rusqlite::Connection;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use uuid::Uuid;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("chief-api-service-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("failed to create temporary directory");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn run_git_with_env(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-c")
        .arg("safe.directory=*")
        .args(args)
        .current_dir(cwd);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {}: {err}", args.join(" ")));
    assert!(
        output.status.success(),
        "git {} failed: stdout={} stderr={}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn run_git(cwd: &Path, args: &[&str]) -> String {
    run_git_with_env(cwd, args, &[])
}

fn create_commit_with_date(
    project_dir: &Path,
    file_name: &str,
    content: &str,
    message: &str,
    timestamp: &str,
) -> String {
    fs::write(project_dir.join(file_name), format!("{content}\n"))
        .expect("failed to write commit fixture file");
    run_git(project_dir, &["add", file_name]);
    run_git_with_env(
        project_dir,
        &["commit", "-m", message],
        &[
            ("GIT_AUTHOR_DATE", timestamp),
            ("GIT_COMMITTER_DATE", timestamp),
        ],
    );
    run_git(project_dir, &["rev-parse", "HEAD"])
}

fn test_todo(id: &str, status: TodoStatus, done_at_commit: Option<&str>) -> Todo {
    Todo {
        id: id.to_owned(),
        todo: format!("todo {id}"),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status,
        done_at_commit: done_at_commit.map(str::to_owned),
    }
}

#[test]
fn parse_todo_status_input_rejects_attempted() {
    assert_eq!(
        parse_todo_status_input("pending"),
        Some(TodoStatus::Pending)
    );
    assert_eq!(
        parse_todo_status_input("in_progress"),
        Some(TodoStatus::InProgress)
    );
    assert_eq!(parse_todo_status_input("done"), Some(TodoStatus::Done));
    assert_eq!(parse_todo_status_input("attempted"), None);
}

#[derive(Debug)]
struct RecordingGitOps {
    root: PathBuf,
    responses: HashMap<String, Option<String>>,
    calls: Mutex<Vec<String>>,
}

impl RecordingGitOps {
    fn new(responses: &[(&str, Option<&str>)]) -> Self {
        Self {
            root: PathBuf::from("."),
            responses: responses
                .iter()
                .map(|(hash, value)| {
                    (
                        (*hash).to_owned(),
                        value.as_ref().map(|timestamp| (*timestamp).to_owned()),
                    )
                })
                .collect(),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls mutex poisoned").clone()
    }
}

impl GitOps for RecordingGitOps {
    fn repo_root(&self) -> &Path {
        &self.root
    }

    fn head_commit(&self, _cwd: &Path) -> anyhow::Result<String> {
        Ok("mock-head-0".to_owned())
    }

    fn changed_files(&self, _cwd: &Path) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn diff(&self, _cwd: &Path, _against_ref: Option<&str>) -> anyhow::Result<String> {
        Ok(String::new())
    }

    fn diff_summary_for_files(&self, _cwd: &Path, _files: &[String]) -> anyhow::Result<String> {
        Ok(String::new())
    }

    fn commit_committer_timestamp_rfc3339(
        &self,
        _cwd: &Path,
        commit_hash: &str,
    ) -> anyhow::Result<String> {
        self.calls
            .lock()
            .expect("calls mutex poisoned")
            .push(commit_hash.to_owned());
        match self.responses.get(commit_hash) {
            Some(Some(value)) => Ok(value.clone()),
            _ => Err(anyhow::anyhow!("commit not found")),
        }
    }

    fn commit_and_tag(&self, _cwd: &Path, _message: &str) -> anyhow::Result<String> {
        Ok("noop".to_owned())
    }

    fn commit_paths(&self, _cwd: &Path, _paths: &[&str], _message: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn create_worktree(&self, _branch: &str, _worktree_path: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn merge_branch_into_main(&self, _branch: &str, _main_branch: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn remove_worktree(&self, _worktree_path: &Path, _branch: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn current_branch(&self) -> anyhow::Result<String> {
        Ok("main".to_owned())
    }
}

fn init_git_repo(project_dir: &Path) {
    run_git(project_dir, &["init"]);
    run_git(
        project_dir,
        &[
            "config",
            "user.email",
            "chief-api-service-tests@example.com",
        ],
    );
    run_git(
        project_dir,
        &["config", "user.name", "Chief API Service Tests"],
    );
}

fn write_todos(project_dir: &Path, todos_yaml: &str) {
    fs::write(
        chief::paths::todos_path(&project_dir),
        format!("{todos_yaml}\n"),
    )
    .expect("failed to write todos.yaml");
}

fn write_chief_yaml(project_dir: &Path, chief_yaml: &str) {
    fs::write(
        chief::paths::chief_yaml_path(&project_dir),
        format!("{chief_yaml}\n"),
    )
    .expect("failed to write chief.yaml");
}

fn setup_service(initial_todos_yaml: &str) -> (TempDir, ApiService, String, PathBuf) {
    let workspace = TempDir::new("workspace");
    let project_name = format!("project-{}", Uuid::new_v4());
    let project_dir = workspace.path.join(&project_name);
    fs::create_dir_all(&project_dir).expect("failed to create project directory");

    init_git_repo(&project_dir);
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");
    write_todos(&project_dir, initial_todos_yaml);

    run_git(&project_dir, &["add", "--all"]);
    run_git(&project_dir, &["commit", "-m", "chore: baseline"]);

    store
        .reset_db_from_todos_file()
        .expect("reset_db_from_todos_file should seed sqlite from todos.yaml");

    let registry =
        ProjectRegistry::discover(&workspace.path, &[]).expect("project discovery should succeed");
    let scheduler = Scheduler::new(registry, 4);
    let service = ApiService::new(scheduler, 1);
    (workspace, service, project_name, project_dir)
}

async fn assert_invalid_yaml_api_error(err: ApiError) {
    let response = err.into_response();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("error response body should be readable");
    let payload: serde_json::Value =
        serde_json::from_slice(&body).expect("error response should be JSON");
    let message = payload
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        message.contains("invalid YAML in"),
        "expected invalid YAML error, got: {message}"
    );
}

#[tokio::test]
async fn start_project_rejects_missing_chief_yaml_without_creating_run_or_job_records() {
    let (_workspace, service, project, project_dir) = setup_service(
        r#"todos:
  - id: pending-1
    todo: Example pending todo
    expectations: Example expectations
    priority: 1
    test_suites: []
    status: pending"#,
    );
    let expected_config_path = chief::paths::chief_yaml_path(&project_dir)
        .display()
        .to_string();
    assert!(
        !chief::paths::chief_yaml_path(&project_dir).exists(),
        "fixture should intentionally omit chief.yaml"
    );

    let err = service
        .start_project(
            project.clone(),
            StartProjectRequest {
                agents: Some(1),
                flow: None,
                model: None,
                start_anyway: None,
            },
        )
        .await
        .expect_err("start_project should reject when chief.yaml is missing");

    let response = err.into_response();
    assert_eq!(
        response.status(),
        StatusCode::CONFLICT,
        "missing chief.yaml should return HTTP 409"
    );
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("error response body should be readable");
    let payload: serde_json::Value =
        serde_json::from_slice(&body).expect("error response should be JSON");
    assert_eq!(
        payload.get("code").and_then(serde_json::Value::as_str),
        Some("chief_yaml_missing"),
        "error code should identify missing chief.yaml"
    );
    let details = payload
        .get("details")
        .expect("error payload should include details");
    assert_eq!(
        details
            .get("config_path")
            .and_then(serde_json::Value::as_str),
        Some(expected_config_path.as_str()),
        "error details should include the missing config path"
    );
    let hint = details
        .get("hint")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        hint.contains("chief init"),
        "error details should include a remediation hint: {hint}"
    );

    let store = ProjectStore::new(&project_dir);
    let jobs = store.list_jobs(50).expect("jobs should be readable");
    assert!(
        jobs.is_empty(),
        "rejected start_project should not create job records"
    );
    let conn = Connection::open(&store.db_path).expect("chief.db should be readable");
    let run_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
        .expect("runs table should be queryable");
    assert_eq!(
        run_count, 0,
        "rejected start_project should not create run records"
    );
}

#[tokio::test]
async fn start_project_rejects_loop_file_flow_as_cli_only() {
    let (_workspace, service, project, project_dir) = setup_service(
        r#"todos:
  - id: pending-1
    todo: Example pending todo
    expectations: Example expectations
    priority: 1
    test_suites: []
    status: pending"#,
    );
    write_chief_yaml(
        &project_dir,
        r#"chief:
  flow: loop_file"#,
    );

    let err = service
        .start_project(
            project,
            StartProjectRequest {
                agents: Some(1),
                flow: Some("loop_file".to_owned()),
                model: None,
                start_anyway: Some(true),
            },
        )
        .await
        .expect_err("start_project should reject loop_file flow");
    let response = err.into_response();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("error response body should be readable");
    let payload: serde_json::Value =
        serde_json::from_slice(&body).expect("error response should be JSON");
    let message = payload
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        message.contains("CLI-only"),
        "loop_file rejection should clearly direct to CLI: {message}"
    );
}

#[tokio::test]
async fn start_project_blocks_when_pre_run_checks_detect_broken_suite_command() {
    let workspace = TempDir::new("workspace");
    let project_name = format!("project-{}", Uuid::new_v4());
    let project_dir = workspace.path.join(&project_name);
    fs::create_dir_all(&project_dir).expect("failed to create project directory");

    init_git_repo(&project_dir);
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");
    write_todos(
        &project_dir,
        r#"todos:
  - id: pending-1
    todo: Example pending todo
    expectations: Example expectations
    priority: 1
    test_suites: []
    status: pending"#,
    );
    write_chief_yaml(
        &project_dir,
        r#"chief:
  flow: refactor
suites:
  - name: smoke
    language: rust
    framework: cargo
    test_root: .
    test_command: "missing-ready-check-cmd {target}"
    default_target: "."
    lint_command: "echo lint-ok""#,
    );

    run_git(&project_dir, &["add", "--all"]);
    run_git(&project_dir, &["commit", "-m", "chore: baseline"]);

    store
        .reset_db_from_todos_file()
        .expect("reset_db_from_todos_file should seed sqlite from todos.yaml");

    let registry =
        ProjectRegistry::discover(&workspace.path, &[]).expect("project discovery should succeed");
    let scheduler = Scheduler::new(registry, 4);
    let service = ApiService::new(scheduler, 1);

    let err = service
        .start_project(
            project_name.clone(),
            StartProjectRequest {
                agents: Some(1),
                flow: None,
                model: None,
                start_anyway: None,
            },
        )
        .await
        .expect_err("start_project should fail when pre-run checks are not ready");

    let response = err.into_response();
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "pre-run checks failures should block start with HTTP 422"
    );
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("error response body should be readable");
    let payload: serde_json::Value =
        serde_json::from_slice(&body).expect("error response should be JSON");
    let message = payload
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        message.contains("Not ready"),
        "expected readiness failure message, got: {message}"
    );

    let readiness = store
        .get_readiness_state()
        .expect("readiness state should be persisted");
    assert_eq!(readiness.status, ReadinessStatus::NotReady);
    assert!(
        readiness.checked_at.is_some(),
        "pre-run checks should persist completion time"
    );
    assert!(
        readiness.summary.contains("Not ready"),
        "readiness summary should explain command failures"
    );
}

#[tokio::test]
async fn start_project_pre_run_checks_use_clean_worktree_without_untracked_files() {
    let workspace = TempDir::new("workspace");
    let project_name = format!("project-{}", Uuid::new_v4());
    let project_dir = workspace.path.join(&project_name);
    fs::create_dir_all(&project_dir).expect("failed to create project directory");

    init_git_repo(&project_dir);
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");
    write_todos(
        &project_dir,
        r#"todos:
  - id: pending-1
    todo: Example pending todo
    expectations: Example expectations
    priority: 1
    test_suites: []
    status: pending"#,
    );
    write_chief_yaml(
        &project_dir,
        r#"chief:
  flow: refactor
suites:
  - name: smoke
    language: rust
    framework: cargo
    test_root: .
    test_init: "test -f .runtime-only.env"
    test_command: "echo ok""#,
    );

    run_git(&project_dir, &["add", "--all"]);
    run_git(&project_dir, &["commit", "-m", "chore: baseline"]);
    fs::write(project_dir.join(".runtime-only.env"), "TOKEN=dev\n")
        .expect("failed to write runtime-only env file");

    store
        .reset_db_from_todos_file()
        .expect("reset_db_from_todos_file should seed sqlite from todos.yaml");

    let registry =
        ProjectRegistry::discover(&workspace.path, &[]).expect("project discovery should succeed");
    let scheduler = Scheduler::new(registry, 4);
    let service = ApiService::new(scheduler, 1);

    let err = service
        .start_project(
            project_name.clone(),
            StartProjectRequest {
                agents: Some(1),
                flow: None,
                model: None,
                start_anyway: None,
            },
        )
        .await
        .expect_err("start_project should fail because untracked file is absent in worktree");

    let response = err.into_response();
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "pre-run checks should fail when test_init depends on untracked files"
    );
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("error response body should be readable");
    let payload: serde_json::Value =
        serde_json::from_slice(&body).expect("error response should be JSON");
    let message = payload
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        message.contains("test_init"),
        "error message should identify the failing test_init check: {message}"
    );

    let readiness = store
        .get_readiness_state()
        .expect("readiness state should be persisted");
    assert_eq!(
        readiness.status,
        ReadinessStatus::NotReady,
        "readiness should persist the worktree validation failure"
    );
    let commands = readiness
        .details
        .get("commands")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        commands.iter().any(|command| {
            command.get("kind").and_then(serde_json::Value::as_str) == Some("test_init")
                && command.get("failed").and_then(serde_json::Value::as_bool) == Some(true)
        }),
        "readiness details should include the failed test_init command"
    );
}

#[tokio::test]
async fn run_suite_check_uses_clean_worktree_without_untracked_files() {
    let workspace = TempDir::new("workspace");
    let project_name = format!("project-{}", Uuid::new_v4());
    let project_dir = workspace.path.join(&project_name);
    fs::create_dir_all(&project_dir).expect("failed to create project directory");

    init_git_repo(&project_dir);
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");
    write_todos(
        &project_dir,
        r#"todos:
  - id: pending-1
    todo: Example pending todo
    expectations: Example expectations
    priority: 1
    test_suites: []
    status: pending"#,
    );
    write_chief_yaml(
        &project_dir,
        r#"chief:
  flow: refactor
suites:
  - name: smoke
    language: rust
    framework: cargo
    test_root: .
    test_command: "test -f .runtime-only.env""#,
    );

    run_git(&project_dir, &["add", "--all"]);
    run_git(&project_dir, &["commit", "-m", "chore: baseline"]);
    fs::write(project_dir.join(".runtime-only.env"), "TOKEN=dev\n")
        .expect("failed to write runtime-only env file");

    store
        .reset_db_from_todos_file()
        .expect("reset_db_from_todos_file should seed sqlite from todos.yaml");

    let registry =
        ProjectRegistry::discover(&workspace.path, &[]).expect("project discovery should succeed");
    let scheduler = Scheduler::new(registry, 4);
    let service = ApiService::new(scheduler, 1);

    let response = service
        .run_suite_check(
            &project_name,
            RunSuiteCheckRequest {
                suite: "smoke".to_owned(),
                kind: SuiteCommandKind::Test,
                target: None,
            },
        )
        .await
        .expect("suite check should execute");

    assert_ne!(
        response.exit_code, 0,
        "suite check should fail because untracked file is absent in clean worktree"
    );
    assert!(
        response.cwd.contains("chief_suite_check_"),
        "suite check should execute in chief_-prefixed temp worktree, got: {}",
        response.cwd
    );

    let worktree_root = project_dir
        .parent()
        .unwrap_or(project_dir.as_path())
        .join(format!("{project_name}__worktrees"));
    if worktree_root.exists() {
        let has_suite_check_dirs = fs::read_dir(&worktree_root)
            .expect("worktree root should be readable")
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .map(|name| name.starts_with("chief_suite_check_"))
                    .unwrap_or(false)
            });
        assert!(
            !has_suite_check_dirs,
            "suite check temp worktree should be cleaned up"
        );
    }
}

#[tokio::test]
async fn start_project_persists_pre_run_check_result_if_request_future_is_dropped() {
    let workspace = TempDir::new("workspace");
    let project_name = format!("project-{}", Uuid::new_v4());
    let project_dir = workspace.path.join(&project_name);
    fs::create_dir_all(&project_dir).expect("failed to create project directory");

    init_git_repo(&project_dir);
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");
    write_todos(
        &project_dir,
        r#"todos:
  - id: pending-1
    todo: Example pending todo
    expectations: Example expectations
    priority: 1
    test_suites: []
    status: pending"#,
    );
    write_chief_yaml(
        &project_dir,
        r#"chief:
  flow: refactor
suites:
  - name: smoke
    language: rust
    framework: cargo
    test_root: .
    test_command: "sleep 2""#,
    );

    run_git(&project_dir, &["add", "--all"]);
    run_git(&project_dir, &["commit", "-m", "chore: baseline"]);

    store
        .reset_db_from_todos_file()
        .expect("reset_db_from_todos_file should seed sqlite from todos.yaml");

    let registry =
        ProjectRegistry::discover(&workspace.path, &[]).expect("project discovery should succeed");
    let scheduler = Scheduler::new(registry, 4);
    let service = ApiService::new(scheduler, 1);

    let service_for_start = service.clone();
    let project_for_start = project_name.clone();
    let start_handle = tokio::spawn(async move {
        service_for_start
            .start_project(
                project_for_start,
                StartProjectRequest {
                    agents: Some(1),
                    flow: None,
                    model: None,
                    start_anyway: None,
                },
            )
            .await
    });

    let mut observed_checking_state = false;
    for _ in 0..50 {
        let readiness = store
            .get_readiness_state()
            .expect("readiness state should be readable while checks run");
        if readiness.status == ReadinessStatus::Checking {
            observed_checking_state = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        observed_checking_state,
        "expected pre-run checks to enter checking state before aborting request future"
    );

    start_handle.abort();
    let _ = start_handle.await;

    tokio::time::sleep(std::time::Duration::from_millis(2600)).await;

    let readiness = store
        .get_readiness_state()
        .expect("readiness state should be persisted even after request future drops");
    assert_eq!(
        readiness.status,
        ReadinessStatus::Ready,
        "pre-run checks should persist terminal status when start request future is dropped"
    );
    assert!(
        readiness.checked_at.is_some(),
        "pre-run checks should persist completion timestamp when request future is dropped"
    );
}

#[tokio::test]
async fn start_project_uses_latest_flow_from_chief_yaml_on_disk() {
    let workspace = TempDir::new("workspace");
    let project_name = format!("project-{}", Uuid::new_v4());
    let project_dir = workspace.path.join(&project_name);
    fs::create_dir_all(&project_dir).expect("failed to create project directory");

    init_git_repo(&project_dir);
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");
    write_todos(
        &project_dir,
        r#"todos:
  - id: done-1
    todo: Completed todo
    expectations: Already done
    priority: 1
    test_suites: []
    status: done"#,
    );
    write_chief_yaml(
        &project_dir,
        r#"chief:
  flow: refactor
  model: gpt-5"#,
    );

    run_git(&project_dir, &["add", "--all"]);
    run_git(&project_dir, &["commit", "-m", "chore: baseline"]);

    store
        .reset_db_from_todos_file()
        .expect("reset_db_from_todos_file should seed sqlite from todos.yaml");

    let registry =
        ProjectRegistry::discover(&workspace.path, &[]).expect("project discovery should succeed");
    let scheduler = Scheduler::new(registry, 4);
    let service = ApiService::new(scheduler, 1);

    write_chief_yaml(
        &project_dir,
        r#"chief:
  flow: refactor
  model: gpt-5"#,
    );

    let response = service
        .start_project(
            project_name.clone(),
            StartProjectRequest {
                agents: Some(1),
                flow: None,
                model: None,
                start_anyway: None,
            },
        )
        .await
        .expect("start_project should succeed");

    assert!(
        response.message.contains("flow=refactor"),
        "start_project should use refreshed chief.yaml flow, got message: {}",
        response.message
    );
}

#[tokio::test]
async fn start_project_skips_pre_run_checks_when_last_success_matches_chief_yaml() {
    let (_workspace, service, project, project_dir) = setup_service_with_chief_yaml(
        r#"todos:
  - id: done-1
    todo: Completed todo
    expectations: Already done
    priority: 1
    test_suites: []
    status: done"#,
        r#"chief:
  flow: refactor"#,
    );

    let store = ProjectStore::new(&project_dir);
    let chief_yaml_hash = chief_yaml_content_hash(&chief::paths::chief_yaml_path(&project_dir))
        .expect("chief.yaml hash should be computed");
    store
        .set_readiness_result(
            ReadinessStatus::Ready,
            "Ready: cached pre-run checks.",
            &serde_json::json!({
                "chief_yaml_hash": chief_yaml_hash,
                "commands_total": 0,
                "commands_failed": 0,
            }),
        )
        .expect("seeded readiness state should persist");
    let readiness_before = store
        .get_readiness_state()
        .expect("readiness state should be readable before start");

    service
        .start_project(
            project.clone(),
            StartProjectRequest {
                agents: Some(1),
                flow: None,
                model: None,
                start_anyway: None,
            },
        )
        .await
        .expect("start_project should succeed when readiness can be reused");

    let readiness_after = store
        .get_readiness_state()
        .expect("readiness state should be readable after start");
    assert_eq!(readiness_after.status, ReadinessStatus::Ready);
    assert_eq!(
        readiness_after.summary, readiness_before.summary,
        "pre-run checks should be skipped when chief.yaml is unchanged and previous check succeeded"
    );
    assert_eq!(
        readiness_after.checked_at, readiness_before.checked_at,
        "checked_at should remain unchanged when pre-run checks are skipped"
    );
}

#[tokio::test]
async fn start_project_reruns_pre_run_checks_when_chief_yaml_changes_after_success() {
    let (_workspace, service, project, project_dir) = setup_service_with_chief_yaml(
        r#"todos:
  - id: done-1
    todo: Completed todo
    expectations: Already done
    priority: 1
    test_suites: []
    status: done"#,
        r#"chief:
  flow: refactor"#,
    );

    let store = ProjectStore::new(&project_dir);
    let original_hash = chief_yaml_content_hash(&chief::paths::chief_yaml_path(&project_dir))
        .expect("chief.yaml hash should be computed");
    store
        .set_readiness_result(
            ReadinessStatus::Ready,
            "Ready: cached pre-run checks.",
            &serde_json::json!({
                "chief_yaml_hash": original_hash,
                "commands_total": 0,
                "commands_failed": 0,
            }),
        )
        .expect("seeded readiness state should persist");
    let readiness_before = store
        .get_readiness_state()
        .expect("readiness state should be readable before start");

    write_chief_yaml(
        &project_dir,
        r#"chief:
  flow: refactor
  model: gpt-5"#,
    );
    let updated_hash = chief_yaml_content_hash(&chief::paths::chief_yaml_path(&project_dir))
        .expect("updated chief.yaml hash should be computed");
    assert_ne!(
        updated_hash,
        readiness_chief_yaml_hash(&readiness_before.details).unwrap_or_default(),
        "fixture should use a modified chief.yaml hash"
    );

    let response = service
        .start_project(
            project.clone(),
            StartProjectRequest {
                agents: Some(1),
                flow: None,
                model: None,
                start_anyway: None,
            },
        )
        .await
        .expect("start_project should succeed after rerunning pre-run checks");
    assert!(
        response.message.contains("flow=refactor"),
        "start_project should use updated chief.yaml flow, got: {}",
        response.message
    );

    let readiness_after = store
        .get_readiness_state()
        .expect("readiness state should be readable after start");
    assert_eq!(readiness_after.status, ReadinessStatus::Ready);
    assert_ne!(
        readiness_after.summary, readiness_before.summary,
        "rerun pre-run checks should replace cached readiness summary"
    );
    assert_ne!(
        readiness_after.checked_at, readiness_before.checked_at,
        "checked_at should be refreshed when chief.yaml changes"
    );
    assert_eq!(
        readiness_chief_yaml_hash(&readiness_after.details),
        Some(updated_hash.as_str()),
        "rerun readiness details should persist the latest chief.yaml hash"
    );
}

#[tokio::test]
async fn start_project_reruns_pre_run_checks_when_previous_result_was_not_successful() {
    let (_workspace, service, project, project_dir) = setup_service_with_chief_yaml(
        r#"todos:
  - id: done-1
    todo: Completed todo
    expectations: Already done
    priority: 1
    test_suites: []
    status: done"#,
        r#"chief:
  flow: refactor"#,
    );

    let store = ProjectStore::new(&project_dir);
    let chief_yaml_hash = chief_yaml_content_hash(&chief::paths::chief_yaml_path(&project_dir))
        .expect("chief.yaml hash should be computed");
    store
        .set_readiness_result(
            ReadinessStatus::NotReady,
            "Not ready: cached pre-run checks failure.",
            &serde_json::json!({
                "chief_yaml_hash": chief_yaml_hash,
                "commands_total": 1,
                "commands_failed": 1,
            }),
        )
        .expect("seeded readiness failure should persist");
    let readiness_before = store
        .get_readiness_state()
        .expect("readiness state should be readable before start");
    assert_eq!(
        readiness_before.status,
        ReadinessStatus::NotReady,
        "fixture should seed a failed readiness state"
    );

    service
        .start_project(
            project.clone(),
            StartProjectRequest {
                agents: Some(1),
                flow: None,
                model: None,
                start_anyway: None,
            },
        )
        .await
        .expect("start_project should rerun pre-run checks after failure");

    let readiness_after = store
        .get_readiness_state()
        .expect("readiness state should be readable after start");
    assert_eq!(
        readiness_after.status,
        ReadinessStatus::Ready,
        "rerun pre-run checks should recover readiness status"
    );
    assert!(
        readiness_after.summary.starts_with("Ready:"),
        "rerun pre-run checks should replace failure summary with success details"
    );
    assert_ne!(
        readiness_after.checked_at, readiness_before.checked_at,
        "checked_at should be refreshed when rerunning after failed readiness"
    );
    assert_eq!(
        readiness_chief_yaml_hash(&readiness_after.details),
        Some(chief_yaml_hash.as_str()),
        "rerun readiness details should persist the current chief.yaml hash"
    );
}

#[tokio::test]
async fn get_todos_refreshes_from_todos_yaml_without_db_reset() {
    let (_workspace, service, project, project_dir) = setup_service(
        r#"todos:
  - id: todo-in-db
    todo: Existing todo
    expectations: Existing expectations
    priority: 1
    test_suites: []
    status: pending"#,
    );

    write_todos(
        &project_dir,
        r#"todos:
  - id: todo-in-db
    todo: Existing todo
    expectations: Existing expectations
    priority: 1
    test_suites: []
    status: pending
  - id: manual-new
    todo: Manually added todo
    expectations: Appears without reset_db
    priority: 8
    test_suites: []
    status: pending"#,
    );

    let response = service
        .get_todos(&project)
        .await
        .expect("get_todos should succeed after manual todos.yaml edit");

    assert!(
        response.todos.iter().any(|todo| todo.id == "manual-new"),
        "new todo from todos.yaml should be visible via get_todos"
    );
}

#[tokio::test]
async fn get_todos_removes_items_deleted_from_todos_yaml() {
    let (_workspace, service, project, project_dir) = setup_service(
        r#"todos:
  - id: todo-keep
    todo: Keep this todo
    expectations: Keep this expectations
    priority: 5
    test_suites: []
    status: pending
  - id: todo-remove
    todo: Remove this todo
    expectations: Remove this expectations
    priority: 2
    test_suites: []
    status: pending"#,
    );

    write_todos(
        &project_dir,
        r#"todos:
  - id: todo-keep
    todo: Keep this todo
    expectations: Keep this expectations
    priority: 5
    test_suites: []
    status: pending"#,
    );

    let response = service
        .get_todos(&project)
        .await
        .expect("get_todos should sync file removals");
    assert_eq!(
        response.todos.len(),
        1,
        "response should exactly match todos.yaml after manual removal"
    );
    assert_eq!(
        response.todos[0].id, "todo-keep",
        "remaining todo should still be visible"
    );
    assert!(
        response.todos.iter().all(|todo| todo.id != "todo-remove"),
        "removed todo should not be returned by get_todos"
    );

    let store = ProjectStore::new(&project_dir);
    let sqlite_todos = store
        .list_todos()
        .expect("sqlite todos should be readable after sync");
    assert!(
        sqlite_todos.iter().all(|todo| todo.id != "todo-remove"),
        "removed todo should also be deleted from sqlite after refresh sync"
    );
}

#[tokio::test]
async fn get_todos_and_get_state_refresh_between_calls_after_manual_todo_edits() {
    let (_workspace, service, project, project_dir) = setup_service(
        r#"todos:
  - id: todo-alpha
    todo: Original todo text
    expectations: Original expectations
    priority: 1
    test_suites: []
    status: pending
  - id: todo-beta
    todo: Already done
    expectations: Keep done
    priority: 2
    test_suites: []
    status: done
  - id: todo-remove
    todo: To be removed by manual edit
    expectations: Should disappear on next read
    priority: 3
    test_suites: []
    status: pending"#,
    );

    let initial_todos = service
        .get_todos(&project)
        .await
        .expect("first get_todos should succeed before manual edit");
    assert!(
        initial_todos
            .todos
            .iter()
            .any(|todo| todo.id == "todo-remove"),
        "first get_todos should reflect baseline todo set"
    );

    let initial_state = service
        .get_state(&project)
        .await
        .expect("first get_state should succeed before manual edit");
    assert_eq!(initial_state.todos.total, 3, "baseline total should match");
    assert_eq!(
        initial_state.todos.completed, 1,
        "baseline completed count should match"
    );
    assert_eq!(
        initial_state.todos.available, 2,
        "baseline available count should match"
    );

    write_todos(
        &project_dir,
        r#"todos:
  - id: todo-alpha
    todo: Edited todo text from file
    expectations: Edited expectations from file
    priority: 9
    test_suites: []
    status: done
    done_at_commit: manual-edit-commit
  - id: todo-gamma
    todo: Newly added todo from file
    expectations: Added between API reads
    priority: 6
    test_suites: []
    status: pending
  - id: todo-beta
    todo: Already done
    expectations: Keep done
    priority: 2
    test_suites: []
    status: done"#,
    );

    let todos = service
        .get_todos(&project)
        .await
        .expect("get_todos should reflect manual todo edits");
    let edited = todos
        .todos
        .iter()
        .find(|todo| todo.id == "todo-alpha")
        .expect("edited todo should exist");
    assert_eq!(edited.todo, "Edited todo text from file");
    assert_eq!(edited.expectations, "Edited expectations from file");
    assert_eq!(edited.priority, 9);
    assert_eq!(edited.done_at_commit.as_deref(), Some("manual-edit-commit"));
    assert!(
        todos.todos.iter().any(|todo| todo.id == "todo-gamma"),
        "second get_todos should include manually added todos"
    );
    assert!(
        todos.todos.iter().all(|todo| todo.id != "todo-remove"),
        "second get_todos should remove todos deleted from todos.yaml"
    );

    let state = service
        .get_state(&project)
        .await
        .expect("get_state should refresh todos before progress calculation");
    assert_eq!(
        state.todos.total, 3,
        "total todos should reflect file edits"
    );
    assert_eq!(
        state.todos.completed, 2,
        "completed count should reflect edited todo status"
    );
    assert_eq!(
        state.todos.available, 1,
        "available count should reflect add/update/remove reconciliation"
    );
}

#[test]
fn resolve_last_done_todo_committed_at_deduplicates_hashes() {
    let git = RecordingGitOps::new(&[
        ("commit-a", Some("2024-05-01T10:00:00+00:00")),
        ("commit-b", Some("2024-06-01T10:00:00+00:00")),
    ]);
    let todos = vec![
        test_todo("done-a", TodoStatus::Done, Some("commit-a")),
        test_todo("done-a-duplicate", TodoStatus::Done, Some("commit-a")),
        test_todo("pending", TodoStatus::Pending, Some("commit-b")),
        test_todo("done-b", TodoStatus::Done, Some("commit-b")),
    ];

    let resolved = resolve_last_done_todo_committed_at(&git, Path::new("."), &todos);

    assert_eq!(resolved.as_deref(), Some("2024-06-01T10:00:00+00:00"));
    assert_eq!(
        git.calls(),
        vec!["commit-a".to_owned(), "commit-b".to_owned()],
        "duplicate done_at_commit hashes should only be resolved once"
    );
}

#[tokio::test]
async fn get_state_returns_latest_resolved_done_todo_commit_timestamp() {
    let (_workspace, service, project, project_dir) = setup_service(
        r#"todos:
  - id: baseline
    todo: Baseline todo
    expectations: Baseline expectations
    priority: 1
    test_suites: []
    status: pending"#,
    );

    let older_commit = create_commit_with_date(
        &project_dir,
        "older.txt",
        "older",
        "chore: older commit",
        "2024-01-02T03:04:05+00:00",
    );
    let newer_commit = create_commit_with_date(
        &project_dir,
        "newer.txt",
        "newer",
        "chore: newer commit",
        "2024-03-04T05:06:07+00:00",
    );
    let newer_timestamp = run_git(
        &project_dir,
        &["show", "-s", "--format=%cI", newer_commit.as_str()],
    );
    let expected_latest_timestamp = chrono::DateTime::parse_from_rfc3339(&newer_timestamp)
        .expect("newer commit timestamp should parse")
        .with_timezone(&chrono::Utc)
        .to_rfc3339();

    write_todos(
        &project_dir,
        &format!(
            r#"todos:
  - id: done-old
    todo: done old
    expectations: done old expectations
    priority: 10
    test_suites: []
    status: done
    done_at_commit: {older_commit}
  - id: done-new
    todo: done new
    expectations: done new expectations
    priority: 9
    test_suites: []
    status: done
    done_at_commit: {newer_commit}
  - id: done-new-duplicate
    todo: done new duplicate
    expectations: duplicate commit hash to verify dedupe path
    priority: 8
    test_suites: []
    status: done
    done_at_commit: {newer_commit}
  - id: pending
    todo: pending todo
    expectations: pending expectations
    priority: 7
    test_suites: []
    status: pending"#
        ),
    );

    let state = service
        .get_state(&project)
        .await
        .expect("get_state should resolve done todo commit timestamps");

    assert_eq!(
        state.last_done_todo_committed_at.as_deref(),
        Some(expected_latest_timestamp.as_str()),
        "state should report the most recent committer timestamp among done todos"
    );
    assert_eq!(state.todos.total, 4, "total should remain unchanged");
    assert_eq!(
        state.todos.completed, 3,
        "completed should remain unchanged"
    );
    assert_eq!(
        state.todos.available, 1,
        "available should remain unchanged"
    );
}

#[tokio::test]
async fn get_state_ignores_unresolved_done_commit_hashes_and_returns_null_timestamp() {
    let (_workspace, service, project, project_dir) = setup_service(
        r#"todos:
  - id: baseline
    todo: Baseline todo
    expectations: Baseline expectations
    priority: 1
    test_suites: []
    status: pending"#,
    );

    write_todos(
        &project_dir,
        r#"todos:
  - id: done-missing-1
    todo: done with missing hash
    expectations: unresolved hash should be ignored
    priority: 5
    test_suites: []
    status: done
    done_at_commit: deadbeefdeadbeefdeadbeefdeadbeefdeadbeef
  - id: done-missing-2
    todo: done with another missing hash
    expectations: unresolved hash should be ignored
    priority: 4
    test_suites: []
    status: done
    done_at_commit: not-a-real-commit
  - id: pending
    todo: pending todo
    expectations: keep counters unchanged
    priority: 3
    test_suites: []
    status: pending"#,
    );

    let state = service
        .get_state(&project)
        .await
        .expect("get_state should succeed even when done_at_commit hashes cannot be resolved");

    assert!(
        state.last_done_todo_committed_at.is_none(),
        "unresolved done_at_commit hashes should result in null timestamp"
    );
    assert_eq!(state.todos.total, 3, "total should remain unchanged");
    assert_eq!(
        state.todos.completed, 2,
        "completed should remain unchanged"
    );
    assert_eq!(
        state.todos.available, 1,
        "available should remain unchanged"
    );
}

#[tokio::test]
async fn reset_project_workspace_discards_changes_and_logs_reset_markers_for_non_done_todos() {
    let (_workspace, service, project, project_dir) = setup_service(
        r#"todos:
  - id: pending-1
    todo: Pending todo
    expectations: reset marker should be recorded
    priority: 3
    test_suites: []
    status: pending
  - id: pending-2
    todo: Second pending todo
    expectations: reset marker should be recorded
    priority: 2
    test_suites: []
    status: pending
  - id: done-1
    todo: Done todo
    expectations: done todos should be ignored
    priority: 1
    test_suites: []
    status: done"#,
    );

    fs::write(project_dir.join("tracked.txt"), "baseline\n")
        .expect("failed to create tracked file fixture");
    run_git(&project_dir, &["add", "tracked.txt"]);
    run_git(
        &project_dir,
        &["commit", "-m", "chore: add tracked fixture file"],
    );

    fs::write(project_dir.join("tracked.txt"), "dirty change\n")
        .expect("failed to dirty tracked file");
    fs::write(project_dir.join("scratch.tmp"), "dirty untracked change\n")
        .expect("failed to create untracked dirty file");

    let response = service
        .reset_project_workspace(&project)
        .await
        .expect("reset_project_workspace should succeed");

    assert!(
        response.message.contains("discarded 2 local git change(s)"),
        "response message should report discarded changes, got: {}",
        response.message
    );

    let status_after = run_git(&project_dir, &["status", "--porcelain"]);
    let remaining_user_changes = status_after
        .lines()
        .filter_map(|line| line.split_whitespace().last())
        .filter(|path| !is_internal_workspace_state_file(path))
        .collect::<Vec<_>>();
    assert!(
        remaining_user_changes.is_empty(),
        "workspace should be clean for user files after reset, got: {status_after}"
    );

    let store = ProjectStore::new(&project_dir);
    let markers = store
        .query_events(EventQuery {
            limit: 200,
            ..EventQuery::default()
        })
        .expect("events should be queryable")
        .into_iter()
        .filter(|event| {
            event.event_type == EventType::GitOp
                && event.msg.starts_with(RETRY_CLEANUP_DISCARDED_MSG_PREFIX)
        })
        .collect::<Vec<_>>();

    assert_eq!(
        markers.len(),
        2,
        "one marker should be recorded per non-done todo"
    );

    let mut marker_todo_ids = markers
        .iter()
        .filter_map(|event| event.todo_id.clone())
        .collect::<Vec<_>>();
    marker_todo_ids.sort();
    assert_eq!(
        marker_todo_ids,
        vec!["pending-1".to_owned(), "pending-2".to_owned()],
        "marker events should target non-done todos only"
    );

    for marker in &markers {
        let files = marker
            .payload
            .get("files")
            .and_then(serde_json::Value::as_array)
            .expect("marker should include files payload")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>();
        assert!(
            files.contains(&"tracked.txt"),
            "marker payload should include tracked file path"
        );
        assert!(
            files.contains(&"scratch.tmp"),
            "marker payload should include untracked file path"
        );
    }
}

#[tokio::test]
async fn reset_project_workspace_logs_marker_even_when_worktree_is_already_clean() {
    let (_workspace, service, project, project_dir) = setup_service(
        r#"todos:
  - id: pending-1
    todo: Pending todo
    expectations: clean workspace should still create marker
    priority: 1
    test_suites: []
    status: pending"#,
    );

    let response = service
        .reset_project_workspace(&project)
        .await
        .expect("reset_project_workspace should succeed for clean worktree");
    assert!(
        response.message.contains("workspace already clean"),
        "response should acknowledge already-clean workspace"
    );

    let store = ProjectStore::new(&project_dir);
    let markers = store
        .query_events(EventQuery {
            limit: 100,
            ..EventQuery::default()
        })
        .expect("events should be queryable")
        .into_iter()
        .filter(|event| {
            event.event_type == EventType::GitOp
                && event.msg.starts_with(RETRY_CLEANUP_DISCARDED_MSG_PREFIX)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        markers.len(),
        1,
        "clean workspace reset should still create one marker for pending todo"
    );
    assert_eq!(
        markers[0].todo_id.as_deref(),
        Some("pending-1"),
        "marker should be tied to the pending todo"
    );
    let files = markers[0]
        .payload
        .get("files")
        .and_then(serde_json::Value::as_array)
        .expect("marker payload should include files")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>();
    assert!(
        files.is_empty(),
        "clean workspace marker should record an empty files list"
    );
}

fn setup_service_with_chief_yaml(
    initial_todos_yaml: &str,
    initial_chief_yaml: &str,
) -> (TempDir, ApiService, String, PathBuf) {
    let workspace = TempDir::new("workspace");
    let project_name = format!("project-{}", Uuid::new_v4());
    let project_dir = workspace.path.join(&project_name);
    fs::create_dir_all(&project_dir).expect("failed to create project directory");

    init_git_repo(&project_dir);
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");
    write_todos(&project_dir, initial_todos_yaml);
    write_chief_yaml(&project_dir, initial_chief_yaml);

    run_git(&project_dir, &["add", "--all"]);
    run_git(&project_dir, &["commit", "-m", "chore: baseline"]);

    store
        .reset_db_from_todos_file()
        .expect("reset_db_from_todos_file should seed sqlite from todos.yaml");

    let registry =
        ProjectRegistry::discover(&workspace.path, &[]).expect("project discovery should succeed");
    let scheduler = Scheduler::new(registry, 4);
    let service = ApiService::new(scheduler, 1);
    (workspace, service, project_name, project_dir)
}

#[tokio::test]
async fn update_chief_yaml_commits_changes_to_git() {
    let (_workspace, service, project, project_dir) = setup_service_with_chief_yaml(
        r#"todos:
  - id: todo-1
    todo: Example
    expectations: Example
    priority: 1
    test_suites: []
    status: pending"#,
        r#"chief:
  flow: refactor"#,
    );

    let updated_yaml = "chief:\n  flow: refactor\n  model: gpt-5\n";
    service
        .update_chief_yaml(
            &project,
            UpdateChiefYamlRequest {
                content: updated_yaml.to_owned(),
            },
        )
        .await
        .expect("update_chief_yaml should succeed");

    let saved = fs::read_to_string(chief::paths::chief_yaml_path(&project_dir))
        .expect("chief.yaml should exist");
    assert_eq!(saved, updated_yaml, "file should contain updated content");

    let log = run_git(&project_dir, &["log", "--oneline", "-1"]);
    assert!(
        log.contains("update .chief/chief.yaml via settings"),
        "latest commit should be the chief.yaml settings update, got: {log}"
    );

    let status = run_git(&project_dir, &["status", "--porcelain"]);
    let user_changes = status
        .lines()
        .filter_map(|line| line.split_whitespace().last())
        .filter(|path| !is_internal_workspace_state_file(path))
        .collect::<Vec<_>>();
    assert!(
        user_changes.is_empty(),
        ".chief/chief.yaml should be committed (no dirty state), got: {status}"
    );
}

#[tokio::test]
async fn update_chief_yaml_noop_commit_when_content_unchanged() {
    let (_workspace, service, project, project_dir) = setup_service_with_chief_yaml(
        r#"todos:
  - id: todo-1
    todo: Example
    expectations: Example
    priority: 1
    test_suites: []
    status: pending"#,
        r#"chief:
  flow: refactor"#,
    );

    let commit_before = run_git(&project_dir, &["rev-parse", "HEAD"]);

    let existing_content = fs::read_to_string(chief::paths::chief_yaml_path(&project_dir))
        .expect("chief.yaml should exist");
    service
        .update_chief_yaml(
            &project,
            UpdateChiefYamlRequest {
                content: existing_content,
            },
        )
        .await
        .expect("update_chief_yaml should succeed even without changes");

    let commit_after = run_git(&project_dir, &["rev-parse", "HEAD"]);
    assert_eq!(
        commit_before, commit_after,
        "no new commit should be created when chief.yaml content is unchanged"
    );
}

#[tokio::test]
async fn update_chief_yaml_does_not_commit_other_dirty_files() {
    let (_workspace, service, project, project_dir) = setup_service_with_chief_yaml(
        r#"todos:
  - id: todo-1
    todo: Example
    expectations: Example
    priority: 1
    test_suites: []
    status: pending"#,
        r#"chief:
  flow: refactor"#,
    );

    fs::write(project_dir.join("unrelated.txt"), "dirty\n")
        .expect("failed to create unrelated dirty file");

    service
        .update_chief_yaml(
            &project,
            UpdateChiefYamlRequest {
                content: "chief:\n  flow: refactor\n  model: gpt-5\n".to_owned(),
            },
        )
        .await
        .expect("update_chief_yaml should succeed");

    let committed_files = run_git(
        &project_dir,
        &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
    );
    assert_eq!(
        committed_files.trim(),
        ".chief/chief.yaml",
        "only .chief/chief.yaml should be in the commit, got: {committed_files}"
    );

    let status = run_git(&project_dir, &["status", "--porcelain"]);
    assert!(
        status.contains("unrelated.txt"),
        "unrelated dirty file should remain uncommitted, got: {status}"
    );
}

#[tokio::test]
async fn read_endpoints_return_api_errors_for_invalid_todos_yaml() {
    let (_workspace, service, project, project_dir) = setup_service(
        r#"todos:
  - id: valid-todo
    todo: Valid baseline
    expectations: Baseline expectations
    priority: 3
    test_suites: []
    status: pending"#,
    );

    let initial_todos = service
        .get_todos(&project)
        .await
        .expect("first get_todos should succeed for valid yaml");
    assert_eq!(
        initial_todos.todos.len(),
        1,
        "baseline get_todos should return the seeded todo"
    );

    let initial_state = service
        .get_state(&project)
        .await
        .expect("first get_state should succeed for valid yaml");
    assert_eq!(initial_state.todos.total, 1, "baseline total should match");
    assert_eq!(
        initial_state.todos.available, 1,
        "baseline available should match"
    );
    assert_eq!(
        initial_state.todos.completed, 0,
        "baseline completed should match"
    );

    fs::write(
        chief::paths::todos_path(&project_dir),
        "todos:\n  - id: broken\n    todo: [missing quote\n",
    )
    .expect("failed to write invalid todos.yaml");

    let todos_error = service
        .get_todos(&project)
        .await
        .expect_err("get_todos should fail for invalid todos.yaml");
    assert_invalid_yaml_api_error(todos_error).await;

    let state_error = service
        .get_state(&project)
        .await
        .expect_err("get_state should fail for invalid todos.yaml");
    assert_invalid_yaml_api_error(state_error).await;

    write_todos(
        &project_dir,
        r#"todos:
  - id: recovered-todo
    todo: Recovered after fixing yaml
    expectations: Reads should recover after parse error
    priority: 7
    test_suites: []
    status: pending"#,
    );

    let recovered_todos = service
        .get_todos(&project)
        .await
        .expect("get_todos should recover after restoring valid yaml");
    assert_eq!(
        recovered_todos
            .todos
            .iter()
            .map(|todo| todo.id.as_str())
            .collect::<Vec<_>>(),
        vec!["recovered-todo"],
        "post-recovery read should reflect latest synchronized file content"
    );
}
