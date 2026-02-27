use super::strategies::SinglePromptPhaseStrategy;
use super::{
    build_flow, AgentRunWithGitChanges, FlowExecution, FlowKind, PhaseStrategy, TestSuiteConfig,
    SINGLE_PROMPT_CHANGED_FILES_RETRY_MESSAGE,
    SINGLE_PROMPT_RETRY_HAS_ASSOCIATED_TEST_SUITES_PAYLOAD_KEY,
    SINGLE_PROMPT_RETRY_REASON_CONVERGENCE_CHANGED_FILES, SINGLE_PROMPT_RETRY_REASON_PAYLOAD_KEY,
};
use crate::agent::{AgentRequest, CodingAgent};
use crate::config::ChiefConfig;
use crate::domain::{AgentOutput, EventType, LoopDecision, Phase, Todo, TodoStatus};
use crate::git::GitOps;
use crate::prompt::PromptStore;
use crate::storage::ProjectStore;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[test]
fn parses_known_flow_kinds() {
    assert_eq!(FlowKind::from_str("tdd").unwrap(), FlowKind::Tdd);
    assert_eq!(
        FlowKind::from_str("single_prompt").unwrap(),
        FlowKind::SinglePrompt
    );
    assert_eq!(FlowKind::from_str("loop_file").unwrap(), FlowKind::LoopFile);
    assert_eq!(FlowKind::from_str(" TDD ").unwrap(), FlowKind::Tdd);
}

#[test]
fn rejects_unknown_flow_kind() {
    let err = FlowKind::from_str("unknown").unwrap_err();
    assert!(
        err.to_string().contains("unknown flow"),
        "unexpected parse error: {}",
        err
    );
}

#[test]
fn build_flow_matches_kind() {
    let tdd = build_flow(FlowKind::Tdd, 6, 2);
    let single_prompt = build_flow(FlowKind::SinglePrompt, 6, 2);
    let loop_file = build_flow(FlowKind::LoopFile, 20, 2);

    assert_eq!(tdd.name(), "tdd");
    assert_eq!(single_prompt.name(), "single_prompt");
    assert_eq!(loop_file.name(), "loop_file");
}

fn temp_project_dir() -> PathBuf {
    std::env::temp_dir().join(format!("chief-flow-test-{}", Uuid::new_v4()))
}

fn record_single_prompt_event(
    store: &ProjectStore,
    run_id: &str,
    todo_id: &str,
    level: &str,
    event_type: EventType,
    msg: &str,
    mut payload: BTreeMap<String, Value>,
    todo_context_hash: &str,
    execution_context_hash: &str,
) {
    payload.insert(
        super::TODO_CONTEXT_HASH_PAYLOAD_KEY.to_owned(),
        Value::String(todo_context_hash.to_owned()),
    );
    payload.insert(
        super::EXECUTION_CONTEXT_HASH_PAYLOAD_KEY.to_owned(),
        Value::String(execution_context_hash.to_owned()),
    );
    store
        .record_event(&crate::domain::EventRecord {
            id: None,
            run_id: run_id.to_owned(),
            job_id: Some(format!("job-{run_id}")),
            todo_id: Some(todo_id.to_owned()),
            timestamp: chrono::Utc::now(),
            level: level.to_owned(),
            phase: Some(Phase::SinglePrompt),
            msg: msg.to_owned(),
            event_type,
            payload,
        })
        .expect("event should log");
}

#[derive(Debug)]
struct NoopPromptStore;

impl PromptStore for NoopPromptStore {
    fn render_json(&self, _template_name: &str, _data: &Value) -> Result<String> {
        Err(anyhow!("not used in this test"))
    }

    fn exists(&self, _template_name: &str) -> bool {
        false
    }
}

#[derive(Debug)]
struct NoopAgent;

impl CodingAgent for NoopAgent {
    fn name(&self) -> &str {
        "noop"
    }

    fn run(&self, _request: AgentRequest) -> Result<AgentOutput> {
        Err(anyhow!("not used in this test"))
    }
}

#[derive(Debug)]
struct SuccessfulAgent;

impl CodingAgent for SuccessfulAgent {
    fn name(&self) -> &str {
        "success"
    }

    fn run(&self, _request: AgentRequest) -> Result<AgentOutput> {
        Ok(AgentOutput {
            exit_code: 0,
            command: "success-agent".to_owned(),
            stdout: String::new(),
            stderr: String::new(),
            merged_output: String::new(),
        })
    }
}

#[derive(Debug)]
struct OneShotDirtyAgent {
    dirty_file: PathBuf,
    dirty_flag: Arc<AtomicBool>,
    runs: Mutex<usize>,
}

impl OneShotDirtyAgent {
    fn new(dirty_file: PathBuf, dirty_flag: Arc<AtomicBool>) -> Self {
        Self {
            dirty_file,
            dirty_flag,
            runs: Mutex::new(0),
        }
    }
}

impl CodingAgent for OneShotDirtyAgent {
    fn name(&self) -> &str {
        "one-shot-dirty"
    }

    fn run(&self, _request: AgentRequest) -> Result<AgentOutput> {
        let mut runs = self.runs.lock().expect("runs mutex poisoned");
        if *runs == 0 {
            fs::write(&self.dirty_file, "dirty").expect("dirty file write should succeed");
            self.dirty_flag.store(true, Ordering::SeqCst);
        }
        *runs += 1;

        Ok(AgentOutput {
            exit_code: 0,
            command: "one-shot-dirty-agent".to_owned(),
            stdout: String::new(),
            stderr: String::new(),
            merged_output: String::new(),
        })
    }
}

#[derive(Debug, Default)]
struct RecordingPromptStore {
    rendered_suite_names: Mutex<Vec<Vec<String>>>,
}

impl RecordingPromptStore {
    fn rendered_suite_names(&self) -> Vec<Vec<String>> {
        self.rendered_suite_names
            .lock()
            .expect("rendered suites mutex poisoned")
            .clone()
    }
}

impl PromptStore for RecordingPromptStore {
    fn render_json(&self, _template_name: &str, data: &Value) -> Result<String> {
        let suite_names = data
            .get("suites")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.get("name").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.rendered_suite_names
            .lock()
            .expect("rendered suites mutex poisoned")
            .push(suite_names);
        Ok("single prompt request".to_owned())
    }

    fn exists(&self, _template_name: &str) -> bool {
        true
    }
}

fn suite_named(name: &str) -> TestSuiteConfig {
    TestSuiteConfig {
        name: name.to_owned(),
        language: "Rust".to_owned(),
        framework: "cargo test".to_owned(),
        test_root: ".".to_owned(),
        test_command: "cargo test".to_owned(),
        target_type: crate::domain::TargetType::Project,
        default_target: None,
        file_patterns: Vec::new(),
        disallow_write_globs: Vec::new(),
        test_init: None,
        test_setup: None,
        cache_paths: Vec::new(),
        cache_key_files: Vec::new(),
        cache_mode: crate::config::SuiteCacheMode::Copy,
        post_green_command: None,
        cleanup_command: None,
        command_timeout_seconds: None,
        lint_command: None,
        lint_fix_command: None,
        env: BTreeMap::new(),
        strip_root_from_target: true,
    }
}

fn suite_named_with_test_command(name: &str, test_command: &str) -> TestSuiteConfig {
    let mut suite = suite_named(name);
    suite.test_command = test_command.to_owned();
    suite
}

#[derive(Debug)]
struct NoopGitOps {
    root: PathBuf,
}

impl GitOps for NoopGitOps {
    fn repo_root(&self) -> &Path {
        &self.root
    }

    fn changed_files(&self, _cwd: &Path) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn diff(&self, _cwd: &Path, _against_ref: Option<&str>) -> Result<String> {
        Ok(String::new())
    }

    fn diff_summary_for_files(&self, _cwd: &Path, _files: &[String]) -> Result<String> {
        Ok(String::new())
    }

    fn commit_committer_timestamp_rfc3339(
        &self,
        _cwd: &Path,
        _commit_hash: &str,
    ) -> Result<String> {
        Ok("1970-01-01T00:00:00+00:00".to_owned())
    }

    fn commit_and_tag(&self, _cwd: &Path, _message: &str) -> Result<String> {
        Ok("noop-commit".to_owned())
    }

    fn commit_paths(&self, _cwd: &Path, _paths: &[&str], _message: &str) -> Result<()> {
        Ok(())
    }

    fn create_worktree(&self, _branch: &str, _worktree_path: &Path) -> Result<()> {
        Ok(())
    }

    fn merge_branch_into_main(&self, _branch: &str, _main_branch: &str) -> Result<()> {
        Ok(())
    }

    fn remove_worktree(&self, _worktree_path: &Path, _branch: &str) -> Result<()> {
        Ok(())
    }

    fn current_branch(&self) -> Result<String> {
        Ok("main".to_owned())
    }
}

#[derive(Debug)]
struct DirtyTrackingGitOps {
    root: PathBuf,
    dirty_file: String,
    dirty_flag: Arc<AtomicBool>,
    commit_messages: Mutex<Vec<String>>,
    head_hash: Mutex<String>,
    commit_count: Mutex<usize>,
}

impl DirtyTrackingGitOps {
    fn new(root: PathBuf, dirty_file: impl Into<String>, dirty_flag: Arc<AtomicBool>) -> Self {
        Self {
            root,
            dirty_file: dirty_file.into(),
            dirty_flag,
            commit_messages: Mutex::new(Vec::new()),
            head_hash: Mutex::new("mock-head-0".to_owned()),
            commit_count: Mutex::new(0),
        }
    }

    fn commit_messages(&self) -> Vec<String> {
        self.commit_messages
            .lock()
            .expect("commit messages mutex poisoned")
            .clone()
    }
}

impl GitOps for DirtyTrackingGitOps {
    fn repo_root(&self) -> &Path {
        &self.root
    }

    fn changed_files(&self, _cwd: &Path) -> Result<Vec<String>> {
        if self.dirty_flag.load(Ordering::SeqCst) {
            Ok(vec![self.dirty_file.clone()])
        } else {
            Ok(Vec::new())
        }
    }

    fn diff(&self, _cwd: &Path, _against_ref: Option<&str>) -> Result<String> {
        Ok(String::new())
    }

    fn diff_summary_for_files(&self, _cwd: &Path, _files: &[String]) -> Result<String> {
        Ok(String::new())
    }

    fn commit_committer_timestamp_rfc3339(
        &self,
        _cwd: &Path,
        _commit_hash: &str,
    ) -> Result<String> {
        Ok("1970-01-01T00:00:00+00:00".to_owned())
    }

    fn commit_and_tag(&self, _cwd: &Path, message: &str) -> Result<String> {
        self.commit_messages
            .lock()
            .expect("commit messages mutex poisoned")
            .push(message.to_owned());

        if self.dirty_flag.swap(false, Ordering::SeqCst) {
            let mut commit_count = self
                .commit_count
                .lock()
                .expect("commit count mutex poisoned");
            *commit_count += 1;
            let commit_hash = format!("mock-commit-{}", *commit_count);
            *self.head_hash.lock().expect("head hash mutex poisoned") = commit_hash;
        }

        Ok(self
            .head_hash
            .lock()
            .expect("head hash mutex poisoned")
            .clone())
    }

    fn commit_paths(&self, _cwd: &Path, _paths: &[&str], _message: &str) -> Result<()> {
        Ok(())
    }

    fn create_worktree(&self, _branch: &str, _worktree_path: &Path) -> Result<()> {
        Ok(())
    }

    fn merge_branch_into_main(&self, _branch: &str, _main_branch: &str) -> Result<()> {
        Ok(())
    }

    fn remove_worktree(&self, _worktree_path: &Path, _branch: &str) -> Result<()> {
        Ok(())
    }

    fn current_branch(&self) -> Result<String> {
        Ok("main".to_owned())
    }
}

#[test]
fn execute_suite_command_returns_timeout_exit_code() {
    let project_dir = temp_project_dir();
    fs::create_dir_all(&project_dir).expect("project dir should be created");
    let cancel_signal = Arc::new(AtomicBool::new(false));

    let out = super::execute_suite_command(
        "sleep 2",
        &project_dir,
        &BTreeMap::new(),
        &cancel_signal,
        Some(1),
    )
    .expect("suite command should return timeout output");

    assert_eq!(out.exit_code, 124);
    assert!(
        out.merged_output
            .contains("suite command timed out after 1 second(s) and was terminated"),
        "expected timeout message in merged output, got: {}",
        out.merged_output
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn run_test_and_lint_runs_all_suites_and_returns_false_when_any_test_fails() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "run all suites even when one fails".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();
    let marker_file = project_dir.join("second-suite-ran.txt");

    let execution = FlowExecution {
        run_id: "run-1".to_owned(),
        job_id: "job-1".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    let suites = vec![
        suite_named_with_test_command("first", "exit 1"),
        suite_named_with_test_command("second", "printf second > second-suite-ran.txt"),
    ];

    let all_ok = super::run_test_and_lint(&execution, &suites, Phase::SinglePrompt)
        .expect("test+lint run should complete");

    assert!(!all_ok, "any suite failure should return retry outcome");
    assert!(
        marker_file.exists(),
        "all suites should run even when an earlier suite fails"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn run_test_suite_executes_cleanup_command_even_on_test_failure() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "cleanup should run after test command".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();
    let cleanup_marker = project_dir.join("cleanup-ran.txt");

    let execution = FlowExecution {
        run_id: "run-1".to_owned(),
        job_id: "job-1".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    let mut suite = suite_named_with_test_command("frontend", "exit 1");
    suite.cleanup_command = Some("printf cleanup > cleanup-ran.txt".to_owned());

    let out = execution
        .run_test_suite(&suite, Phase::SinglePrompt)
        .expect("test command should complete with nonzero exit");
    assert_eq!(out.exit_code, 1, "test result should be preserved");
    assert!(
        cleanup_marker.exists(),
        "cleanup command should run even when test command fails"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn suite_preparation_commands_run_once_per_suite_per_execution() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "suite setup should run once".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();

    let execution = FlowExecution {
        run_id: "run-1".to_owned(),
        job_id: "job-1".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    let marker_file = project_dir.join("suite-setup.log");
    let mut suite = suite_named_with_test_command("backend", "cat suite-setup.log >/dev/null");
    suite.test_init = Some("printf init >> suite-setup.log".to_owned());
    suite.test_setup = Some("printf setup >> suite-setup.log".to_owned());

    let first_run = super::run_test_and_lint(&execution, &[suite.clone()], Phase::SinglePrompt)
        .expect("first test+lint run should complete");
    let second_run = super::run_test_and_lint(&execution, &[suite], Phase::SinglePrompt)
        .expect("second test+lint run should complete");

    assert!(first_run, "first suite run should pass");
    assert!(second_run, "second suite run should pass");

    let marker = fs::read_to_string(&marker_file)
        .expect("suite preparation marker should be created exactly once");
    assert_eq!(
        marker, "initsetup",
        "test_init and test_setup should execute only once for a suite in a worker execution"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn previous_steps_log_orders_entries_oldest_first_within_limit() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "order logs".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();

    let execution = FlowExecution {
        run_id: "run-1".to_owned(),
        job_id: "job-1".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo: todo.clone(),
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    execution
        .log_event(
            "info",
            Some(Phase::Red),
            EventType::TestRun,
            "oldest event",
            BTreeMap::new(),
        )
        .expect("oldest event should log");
    execution
        .log_event(
            "info",
            Some(Phase::Red),
            EventType::TestRun,
            "middle event",
            BTreeMap::new(),
        )
        .expect("middle event should log");
    execution
        .log_event(
            "info",
            Some(Phase::Red),
            EventType::TestRun,
            "newest event",
            BTreeMap::new(),
        )
        .expect("newest event should log");

    store
        .record_event(&crate::domain::EventRecord {
            id: None,
            run_id: "run-1".to_owned(),
            job_id: Some("job-1".to_owned()),
            todo_id: Some("other-todo".to_owned()),
            timestamp: chrono::Utc::now(),
            level: "info".to_owned(),
            phase: Some(Phase::Red),
            msg: "other todo event".to_owned(),
            event_type: EventType::TestRun,
            payload: BTreeMap::new(),
        })
        .expect("other todo event should log");

    let log = execution
        .previous_steps_log(Phase::Red, &[EventType::TestRun], 2)
        .expect("previous_steps_log should succeed");
    let entries = log.split("\n\n").collect::<Vec<_>>();

    assert_eq!(
        entries.len(),
        2,
        "should keep only the last 2 matching events"
    );
    assert!(
        entries[0].starts_with("[1] test_run: middle event"),
        "first entry should be the oldest among returned events, got: {}",
        entries[0]
    );
    assert!(
        entries[1].starts_with("[2] test_run: newest event"),
        "second entry should be the newest among returned events, got: {}",
        entries[1]
    );
    assert!(
        !log.contains("oldest event"),
        "entries older than limit should be dropped"
    );
    assert!(
        !log.contains("other todo event"),
        "entries from other todos must be excluded"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn previous_steps_log_includes_command_and_truncated_output() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "include command".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig {
        agent_log_max_output_lines: 2,
        ..ChiefConfig::default()
    };

    let execution = FlowExecution {
        run_id: "run-1".to_owned(),
        job_id: "job-1".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    let mut payload = BTreeMap::new();
    payload.insert(
        "command".to_owned(),
        Value::String("cargo test --lib".to_owned()),
    );
    payload.insert(
        "output".to_owned(),
        Value::String("line-a\nline-b\nline-c".to_owned()),
    );

    execution
        .log_event(
            "warning",
            Some(Phase::Green),
            EventType::TestRun,
            "test failed",
            payload,
        )
        .expect("event should log");

    let log = execution
        .previous_steps_log(Phase::Green, &[EventType::TestRun], 1)
        .expect("previous_steps_log should succeed");
    assert!(
        log.contains("command: cargo test --lib"),
        "expected command to be included in log entry, got: {log}"
    );
    assert!(
        log.contains("line-b\nline-c"),
        "expected only the configured output tail to be present, got: {log}"
    );
    assert!(
        !log.contains("line-a"),
        "old output lines beyond tail limit should be omitted, got: {log}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn previous_steps_log_excludes_history_when_execution_context_hash_changes() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "include command".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: vec!["backend".to_owned()],
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();
    let old_suite = suite_named_with_test_command("backend", "cargo test");
    let new_suite = suite_named_with_test_command("backend", "cargo test --all");
    let old_suites = [old_suite];
    let new_suites = [new_suite];

    let old_execution = FlowExecution {
        run_id: "run-old".to_owned(),
        job_id: "job-old".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &old_suites,
        todo: todo.clone(),
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    let mut payload = BTreeMap::new();
    payload.insert("command".to_owned(), Value::String("cargo test".to_owned()));
    payload.insert(
        "output".to_owned(),
        Value::String("failing output".to_owned()),
    );
    payload.insert("exit_code".to_owned(), Value::from(1));
    old_execution
        .log_event(
            "warning",
            Some(Phase::Red),
            EventType::TestRun,
            "test failed",
            payload,
        )
        .expect("old-context event should log");

    let new_execution = FlowExecution {
        run_id: "run-new".to_owned(),
        job_id: "job-new".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &new_suites,
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    let log = new_execution
        .previous_steps_log(Phase::Red, &[EventType::TestRun], 8)
        .expect("previous_steps_log should succeed");
    assert_eq!(
        log, "No previous attempts recorded.",
        "execution-context drift should suppress prior history injection"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn single_prompt_failure_context_includes_failed_commands_and_output_tails() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "capture failed command context".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig {
        agent_log_max_output_lines: 2,
        ..ChiefConfig::default()
    };

    let execution = FlowExecution {
        run_id: "run-1".to_owned(),
        job_id: "job-1".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    let mut lint_payload = BTreeMap::new();
    lint_payload.insert(
        "command".to_owned(),
        Value::String(".venv/bin/python -m ruff check .".to_owned()),
    );
    lint_payload.insert(
        "output".to_owned(),
        Value::String("lint-line-1\nlint-line-2\nlint-line-3".to_owned()),
    );
    lint_payload.insert("exit_code".to_owned(), Value::from(1));

    execution
        .log_event(
            "warning",
            Some(Phase::SinglePrompt),
            EventType::Lint,
            "lint failed (backend)",
            lint_payload,
        )
        .expect("lint event should log");

    let mut test_payload = BTreeMap::new();
    test_payload.insert(
        "command".to_owned(),
        Value::String("source .venv/bin/activate && pytest tests/".to_owned()),
    );
    test_payload.insert(
        "output".to_owned(),
        Value::String("test-line-1\ntest-line-2\ntest-line-3".to_owned()),
    );
    test_payload.insert("exit_code".to_owned(), Value::from(1));

    execution
        .log_event(
            "warning",
            Some(Phase::SinglePrompt),
            EventType::TestRun,
            "test failed (backend)",
            test_payload,
        )
        .expect("test event should log");

    let mut other_payload = BTreeMap::new();
    other_payload.insert(
        "command".to_owned(),
        Value::String("codex exec --json --dangerously-bypass-approvals-and-sandbox -".to_owned()),
    );
    other_payload.insert(
        "output".to_owned(),
        Value::String("agent-line-1\nagent-line-2\nagent-line-3".to_owned()),
    );
    other_payload.insert("exit_code".to_owned(), Value::from(1));
    execution
        .log_event(
            "warning",
            Some(Phase::SinglePrompt),
            EventType::AgentResponse,
            "agent response failed",
            other_payload,
        )
        .expect("other failure event should log");

    let context = execution
        .latest_single_prompt_failure_context()
        .expect("single prompt failure context should resolve");

    assert!(context.failed_lint, "lint failure should be detected");
    assert!(context.failed_test, "test failure should be detected");
    assert!(context.failed_other, "other failure should be detected");
    assert_eq!(context.lint_failures.len(), 1);
    assert_eq!(context.test_failures.len(), 1);
    assert_eq!(context.other_failures.len(), 1);
    assert_eq!(
        context.lint_failures[0].command,
        ".venv/bin/python -m ruff check ."
    );
    assert_eq!(
        context.test_failures[0].command,
        "source .venv/bin/activate && pytest tests/"
    );
    assert_eq!(
        context.lint_failures[0].output_tail,
        "lint-line-2\nlint-line-3"
    );
    assert_eq!(
        context.test_failures[0].output_tail,
        "test-line-2\ntest-line-3"
    );
    assert_eq!(context.other_failures[0].event_type, "agent_response");
    assert_eq!(context.other_failures[0].message, "agent response failed");
    assert_eq!(
        context.other_failures[0].output_tail,
        "agent-line-2\nagent-line-3"
    );
    assert!(
        context.lint_failures[0].event_id > 0,
        "lint failure should include persisted event id"
    );
    assert!(
        context.lint_failures[0]
            .sqlite_query
            .contains("FROM events WHERE run_id='run-1' AND todo_id='todo-1' AND id="),
        "lint failure should include an event-specific sqlite query"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn single_prompt_failure_context_excludes_history_when_todo_context_hash_changes() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo_old = Todo {
        id: "todo-1".to_owned(),
        todo: "old todo text".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };
    let todo_new = Todo {
        id: "todo-1".to_owned(),
        todo: "new todo text".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();

    let old_execution = FlowExecution {
        run_id: "run-old".to_owned(),
        job_id: "job-old".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo: todo_old,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    old_execution
        .log_event(
            "info",
            Some(Phase::SinglePrompt),
            EventType::AgentPrompt,
            "Agent prompt (single_prompt)",
            BTreeMap::new(),
        )
        .expect("old-context prompt event should log");
    let mut failed_payload = BTreeMap::new();
    failed_payload.insert("exit_code".to_owned(), Value::from(1));
    failed_payload.insert(
        "command".to_owned(),
        Value::String("codex exec --json -".to_owned()),
    );
    failed_payload.insert(
        "output".to_owned(),
        Value::String("old-context failure".to_owned()),
    );
    old_execution
        .log_event(
            "warning",
            Some(Phase::SinglePrompt),
            EventType::AgentResponse,
            "agent response failed",
            failed_payload,
        )
        .expect("old-context failure event should log");

    let new_execution = FlowExecution {
        run_id: "run-new".to_owned(),
        job_id: "job-new".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo: todo_new,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    let context = new_execution
        .latest_single_prompt_failure_context()
        .expect("single prompt failure context should resolve");
    assert!(
        !context.failed_other,
        "todo-context drift should suppress prior single_prompt failure history"
    );
    assert!(
        context.other_failures.is_empty(),
        "todo-context drift should suppress prior single_prompt failure details"
    );
    assert!(
        !new_execution
            .has_previous_single_prompt_attempt_since_last_retry_reset()
            .expect("single prompt attempt check should succeed"),
        "todo-context drift should reset first-attempt detection"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn single_prompt_failure_context_includes_all_latest_iteration_failures_in_order() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "capture all failed commands".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig {
        agent_log_max_output_lines: 2,
        ..ChiefConfig::default()
    };

    let execution = FlowExecution {
        run_id: "run-1".to_owned(),
        job_id: "job-1".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    // Older iteration failures should be ignored once we hit the latest iteration boundary.
    execution
        .log_event(
            "info",
            Some(Phase::SinglePrompt),
            EventType::AgentPrompt,
            "Agent prompt (single_prompt)",
            BTreeMap::new(),
        )
        .expect("older iteration prompt should log");
    let mut old_lint_payload = BTreeMap::new();
    old_lint_payload.insert(
        "command".to_owned(),
        Value::String("old-lint-command".to_owned()),
    );
    old_lint_payload.insert(
        "output".to_owned(),
        Value::String("old-lint-line-1\nold-lint-line-2".to_owned()),
    );
    old_lint_payload.insert("exit_code".to_owned(), Value::from(1));
    execution
        .log_event(
            "warning",
            Some(Phase::SinglePrompt),
            EventType::Lint,
            "old lint failed",
            old_lint_payload,
        )
        .expect("older iteration lint event should log");

    // Latest iteration prompt boundary.
    execution
        .log_event(
            "info",
            Some(Phase::SinglePrompt),
            EventType::AgentPrompt,
            "Agent prompt (single_prompt)",
            BTreeMap::new(),
        )
        .expect("latest iteration prompt should log");

    let mut lint_a_payload = BTreeMap::new();
    lint_a_payload.insert(
        "command".to_owned(),
        Value::String("lint-command-a".to_owned()),
    );
    lint_a_payload.insert(
        "output".to_owned(),
        Value::String("lint-a-line-1\nlint-a-line-2\nlint-a-line-3".to_owned()),
    );
    lint_a_payload.insert("exit_code".to_owned(), Value::from(1));
    execution
        .log_event(
            "warning",
            Some(Phase::SinglePrompt),
            EventType::Lint,
            "lint A failed",
            lint_a_payload,
        )
        .expect("lint A should log");

    let mut lint_b_payload = BTreeMap::new();
    lint_b_payload.insert(
        "command".to_owned(),
        Value::String("lint-command-b".to_owned()),
    );
    lint_b_payload.insert(
        "output".to_owned(),
        Value::String("lint-b-line-1\nlint-b-line-2\nlint-b-line-3".to_owned()),
    );
    lint_b_payload.insert("exit_code".to_owned(), Value::from(1));
    execution
        .log_event(
            "warning",
            Some(Phase::SinglePrompt),
            EventType::Lint,
            "lint B failed",
            lint_b_payload,
        )
        .expect("lint B should log");

    let mut test_a_payload = BTreeMap::new();
    test_a_payload.insert(
        "command".to_owned(),
        Value::String("test-command-a".to_owned()),
    );
    test_a_payload.insert(
        "output".to_owned(),
        Value::String("test-a-line-1\ntest-a-line-2\ntest-a-line-3".to_owned()),
    );
    test_a_payload.insert("exit_code".to_owned(), Value::from(1));
    execution
        .log_event(
            "warning",
            Some(Phase::SinglePrompt),
            EventType::TestRun,
            "test A failed",
            test_a_payload,
        )
        .expect("test A should log");

    let mut test_b_payload = BTreeMap::new();
    test_b_payload.insert(
        "command".to_owned(),
        Value::String("test-command-b".to_owned()),
    );
    test_b_payload.insert(
        "output".to_owned(),
        Value::String("test-b-line-1\ntest-b-line-2\ntest-b-line-3".to_owned()),
    );
    test_b_payload.insert("exit_code".to_owned(), Value::from(1));
    execution
        .log_event(
            "warning",
            Some(Phase::SinglePrompt),
            EventType::TestRun,
            "test B failed",
            test_b_payload,
        )
        .expect("test B should log");

    let context = execution
        .latest_single_prompt_failure_context()
        .expect("single prompt failure context should resolve");

    assert!(context.failed_lint);
    assert!(context.failed_test);
    assert!(!context.failed_other);
    assert_eq!(
        context
            .lint_failures
            .iter()
            .map(|item| item.command.as_str())
            .collect::<Vec<_>>(),
        vec!["lint-command-a", "lint-command-b"]
    );
    assert_eq!(
        context
            .test_failures
            .iter()
            .map(|item| item.command.as_str())
            .collect::<Vec<_>>(),
        vec!["test-command-a", "test-command-b"]
    );
    assert_eq!(
        context.lint_failures[0].output_tail,
        "lint-a-line-2\nlint-a-line-3"
    );
    assert_eq!(
        context.test_failures[1].output_tail,
        "test-b-line-2\ntest-b-line-3"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn single_prompt_failure_context_uses_latest_suite_failures_across_runs() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "reuse latest failed suite output across runs".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: vec!["backend".to_owned(), "frontend".to_owned()],
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();

    let execution = FlowExecution {
        run_id: "run-current".to_owned(),
        job_id: "job-current".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };
    let todo_context_hash = execution.todo_context_hash();
    let execution_context_hash = execution.execution_context_hash();
    let suite_payload = |suite: &str, command: &str, exit_code: i64, output: &str| {
        let mut payload = BTreeMap::new();
        payload.insert("suite".to_owned(), Value::String(suite.to_owned()));
        payload.insert("command".to_owned(), Value::String(command.to_owned()));
        payload.insert("exit_code".to_owned(), Value::from(exit_code));
        payload.insert("output".to_owned(), Value::String(output.to_owned()));
        payload
    };
    // Older run: both suites fail.
    record_single_prompt_event(
        &store,
        "run-older",
        "todo-1",
        "warning",
        EventType::Lint,
        "Lint failed (backend)",
        suite_payload("backend", "lint-backend-old", 1, "lint backend old failed"),
        todo_context_hash.as_str(),
        execution_context_hash.as_str(),
    );
    record_single_prompt_event(
        &store,
        "run-older",
        "todo-1",
        "warning",
        EventType::Lint,
        "Lint failed (frontend)",
        suite_payload(
            "frontend",
            "lint-frontend-old",
            1,
            "lint frontend old failed",
        ),
        todo_context_hash.as_str(),
        execution_context_hash.as_str(),
    );
    record_single_prompt_event(
        &store,
        "run-older",
        "todo-1",
        "warning",
        EventType::TestRun,
        "Test run failed (backend)",
        suite_payload("backend", "test-backend-old", 1, "test backend old failed"),
        todo_context_hash.as_str(),
        execution_context_hash.as_str(),
    );
    record_single_prompt_event(
        &store,
        "run-older",
        "todo-1",
        "warning",
        EventType::TestRun,
        "Test run failed (frontend)",
        suite_payload(
            "frontend",
            "test-frontend-old",
            1,
            "test frontend old failed",
        ),
        todo_context_hash.as_str(),
        execution_context_hash.as_str(),
    );

    // Later run: backend now passes, frontend still fails.
    record_single_prompt_event(
        &store,
        "run-resumed",
        "todo-1",
        "info",
        EventType::Lint,
        "Lint passed (backend)",
        suite_payload(
            "backend",
            "lint-backend-resumed",
            0,
            "lint backend resumed passed",
        ),
        todo_context_hash.as_str(),
        execution_context_hash.as_str(),
    );
    record_single_prompt_event(
        &store,
        "run-resumed",
        "todo-1",
        "warning",
        EventType::Lint,
        "Lint failed (frontend)",
        suite_payload(
            "frontend",
            "lint-frontend-resumed",
            1,
            "lint frontend resumed failed",
        ),
        todo_context_hash.as_str(),
        execution_context_hash.as_str(),
    );
    record_single_prompt_event(
        &store,
        "run-resumed",
        "todo-1",
        "info",
        EventType::TestRun,
        "Test run passed (backend)",
        suite_payload(
            "backend",
            "test-backend-resumed",
            0,
            "test backend resumed passed",
        ),
        todo_context_hash.as_str(),
        execution_context_hash.as_str(),
    );
    record_single_prompt_event(
        &store,
        "run-resumed",
        "todo-1",
        "warning",
        EventType::TestRun,
        "Test run failed (frontend)",
        suite_payload(
            "frontend",
            "test-frontend-resumed",
            1,
            "test frontend resumed failed",
        ),
        todo_context_hash.as_str(),
        execution_context_hash.as_str(),
    );

    // Most recent run timed out before suite checks; timeout should not become failed_other.
    record_single_prompt_event(
        &store,
        "run-current",
        "todo-1",
        "info",
        EventType::AgentPrompt,
        "Agent prompt (single_prompt)",
        BTreeMap::new(),
        todo_context_hash.as_str(),
        execution_context_hash.as_str(),
    );
    let mut timeout_payload = BTreeMap::new();
    timeout_payload.insert("exit_code".to_owned(), Value::from(124));
    timeout_payload.insert(
        "command".to_owned(),
        Value::String("claude -p - --dangerously-skip-permissions --verbose".to_owned()),
    );
    timeout_payload.insert(
        "output".to_owned(),
        Value::String(
            "agent timed out after 2700 second(s) and was terminated.\npartial output".to_owned(),
        ),
    );
    record_single_prompt_event(
        &store,
        "run-current",
        "todo-1",
        "warning",
        EventType::AgentResponse,
        "Agent response (single_prompt)",
        timeout_payload,
        todo_context_hash.as_str(),
        execution_context_hash.as_str(),
    );
    let mut timeout_phase_failure_payload = BTreeMap::new();
    timeout_phase_failure_payload.insert("exit_code".to_owned(), Value::from(124));
    timeout_phase_failure_payload.insert(
        "command".to_owned(),
        Value::String("claude -p - --dangerously-skip-permissions --verbose".to_owned()),
    );
    record_single_prompt_event(
        &store,
        "run-current",
        "todo-1",
        "warning",
        EventType::PhaseFailure,
        "single_prompt agent step failed",
        timeout_phase_failure_payload,
        todo_context_hash.as_str(),
        execution_context_hash.as_str(),
    );

    let context = execution
        .latest_single_prompt_failure_context()
        .expect("single prompt failure context should resolve");

    assert!(
        context.failed_lint,
        "frontend latest lint failure should be present"
    );
    assert!(
        context.failed_test,
        "frontend latest test failure should be present"
    );
    assert!(
        !context.failed_other,
        "agent timeout response should not be included as failed_other"
    );
    assert_eq!(
        context
            .lint_failures
            .iter()
            .map(|item| item.command.as_str())
            .collect::<Vec<_>>(),
        vec!["lint-frontend-resumed"],
        "only suites whose latest lint run failed should be included"
    );
    assert_eq!(
        context
            .test_failures
            .iter()
            .map(|item| item.command.as_str())
            .collect::<Vec<_>>(),
        vec!["test-frontend-resumed"],
        "only suites whose latest test run failed should be included"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn single_prompt_failure_context_stops_at_retry_cleanup_reset_for_suite_history() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "ignore suite failures before reset markers".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: vec!["frontend".to_owned()],
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();

    let execution = FlowExecution {
        run_id: "run-current".to_owned(),
        job_id: "job-current".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };
    let todo_context_hash = execution.todo_context_hash();
    let execution_context_hash = execution.execution_context_hash();

    let mut old_test_payload = BTreeMap::new();
    old_test_payload.insert("suite".to_owned(), Value::String("frontend".to_owned()));
    old_test_payload.insert(
        "command".to_owned(),
        Value::String("test-frontend-old".to_owned()),
    );
    old_test_payload.insert("exit_code".to_owned(), Value::from(1));
    old_test_payload.insert(
        "output".to_owned(),
        Value::String("frontend test failed before reset".to_owned()),
    );
    record_single_prompt_event(
        &store,
        "run-old",
        "todo-1",
        "warning",
        EventType::TestRun,
        "Test run failed (frontend)",
        old_test_payload,
        todo_context_hash.as_str(),
        execution_context_hash.as_str(),
    );

    store
        .record_event(&crate::domain::EventRecord {
            id: None,
            run_id: "run-reset".to_owned(),
            job_id: Some("job-reset".to_owned()),
            todo_id: Some("todo-1".to_owned()),
            timestamp: chrono::Utc::now(),
            level: "warning".to_owned(),
            phase: Some(Phase::Red),
            msg: "Retry cleanup: discarded local git changes before loop manual/1".to_owned(),
            event_type: EventType::GitOp,
            payload: BTreeMap::new(),
        })
        .expect("reset marker event should log");

    let context = execution
        .latest_single_prompt_failure_context()
        .expect("single prompt failure context should resolve");

    assert!(
        !context.failed_test,
        "suite failures before latest retry cleanup marker should be ignored"
    );
    assert!(
        context.test_failures.is_empty(),
        "suite failure details before reset marker should not be included"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn single_prompt_changed_files_retry_without_associated_suites_is_not_failure_context() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "changed files should not be failure context without associated suites".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();

    let mut execution = FlowExecution {
        run_id: "run-1".to_owned(),
        job_id: "job-1".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    let mut strategy = SinglePromptPhaseStrategy::new(Vec::new());
    strategy.last_agent_run = Some(AgentRunWithGitChanges {
        output: AgentOutput::success("agent-step", ""),
        touched_files: vec!["src/flow.rs".to_owned()],
        had_git_changes: true,
    });

    let decision = strategy
        .check_goal(&mut execution, 0, &AgentOutput::success("unused", ""))
        .expect("single_prompt check_goal should succeed");
    assert!(
        matches!(decision, LoopDecision::Retry),
        "changed files must continue to trigger retry"
    );

    let context = execution
        .latest_single_prompt_failure_context()
        .expect("single prompt failure context should resolve");
    assert!(
        !context.failed_other,
        "changed-files convergence retry should not count as failed_other when todo has no associated suites"
    );
    assert!(
        context.other_failures.is_empty(),
        "changed-files convergence retry should be excluded from other_failures when todo has no associated suites"
    );

    let events = execution
        .todo_events_since_last_retry_reset(100)
        .expect("event query should succeed");
    let changed_files_event = events
        .into_iter()
        .find(|event| event.msg == SINGLE_PROMPT_CHANGED_FILES_RETRY_MESSAGE)
        .expect("changed-files phase failure event should be logged");
    assert_eq!(changed_files_event.event_type, EventType::PhaseFailure);
    assert_eq!(
        changed_files_event
            .payload
            .get(SINGLE_PROMPT_RETRY_REASON_PAYLOAD_KEY)
            .and_then(Value::as_str),
        Some(SINGLE_PROMPT_RETRY_REASON_CONVERGENCE_CHANGED_FILES),
        "changed-files retry event should include a structured retry reason"
    );
    assert_eq!(
        changed_files_event
            .payload
            .get(SINGLE_PROMPT_RETRY_HAS_ASSOCIATED_TEST_SUITES_PAYLOAD_KEY)
            .and_then(Value::as_bool),
        Some(false),
        "changed-files retry event should include associated-suite presence"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn single_prompt_changed_files_retry_with_associated_suites_remains_failure_context() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "changed files should remain failure context with associated suites".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: vec!["backend".to_owned()],
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();

    let mut execution = FlowExecution {
        run_id: "run-1".to_owned(),
        job_id: "job-1".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    let mut strategy = SinglePromptPhaseStrategy::new(Vec::new());
    strategy.last_agent_run = Some(AgentRunWithGitChanges {
        output: AgentOutput::success("agent-step", ""),
        touched_files: vec!["src/flow.rs".to_owned()],
        had_git_changes: true,
    });

    let decision = strategy
        .check_goal(&mut execution, 0, &AgentOutput::success("unused", ""))
        .expect("single_prompt check_goal should succeed");
    assert!(
        matches!(decision, LoopDecision::Retry),
        "changed files must continue to trigger retry"
    );

    let context = execution
        .latest_single_prompt_failure_context()
        .expect("single prompt failure context should resolve");
    assert!(
        context.failed_other,
        "changed-files convergence retry should remain failed_other when todo has associated suites"
    );
    assert_eq!(
        context.other_failures.len(),
        1,
        "changed-files convergence retry should be included in other_failures when todo has associated suites"
    );
    assert_eq!(
        context.other_failures[0].event_type,
        EventType::PhaseFailure.as_str()
    );
    assert_eq!(
        context.other_failures[0].message,
        SINGLE_PROMPT_CHANGED_FILES_RETRY_MESSAGE
    );

    let events = execution
        .todo_events_since_last_retry_reset(100)
        .expect("event query should succeed");
    let changed_files_event = events
        .into_iter()
        .find(|event| event.msg == SINGLE_PROMPT_CHANGED_FILES_RETRY_MESSAGE)
        .expect("changed-files phase failure event should be logged");
    assert_eq!(
        changed_files_event
            .payload
            .get(SINGLE_PROMPT_RETRY_REASON_PAYLOAD_KEY)
            .and_then(Value::as_str),
        Some(SINGLE_PROMPT_RETRY_REASON_CONVERGENCE_CHANGED_FILES),
        "changed-files retry event should include a structured retry reason"
    );
    assert_eq!(
        changed_files_event
            .payload
            .get(SINGLE_PROMPT_RETRY_HAS_ASSOCIATED_TEST_SUITES_PAYLOAD_KEY)
            .and_then(Value::as_bool),
        Some(true),
        "changed-files retry event should include associated-suite presence"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn touched_files_since_last_retry_reset_uses_all_runs_and_stops_at_latest_reset() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "collect touched files".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();

    let execution = FlowExecution {
        run_id: "run-current".to_owned(),
        job_id: "job-current".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };
    let todo_context_hash = execution.todo_context_hash();
    let execution_context_hash = execution.execution_context_hash();

    let diff_payload = |paths: &[&str]| {
        let mut payload = BTreeMap::new();
        payload.insert(
            "touched_files".to_owned(),
            Value::Array(
                paths
                    .iter()
                    .map(|path| Value::String((*path).to_owned()))
                    .collect(),
            ),
        );
        payload.insert("had_git_changes".to_owned(), Value::Bool(true));
        payload
    };

    let old_payload = diff_payload(&["backend/tests/old_should_be_ignored.py"]);
    store
        .record_event(&crate::domain::EventRecord {
            id: None,
            run_id: "run-older".to_owned(),
            job_id: Some("job-older".to_owned()),
            todo_id: Some("todo-1".to_owned()),
            timestamp: chrono::Utc::now(),
            level: "info".to_owned(),
            phase: Some(Phase::SinglePrompt),
            msg: "Iteration git change detection".to_owned(),
            event_type: EventType::Diff,
            payload: old_payload,
        })
        .expect("old diff event should log");

    store
        .record_event(&crate::domain::EventRecord {
            id: None,
            run_id: "run-older".to_owned(),
            job_id: Some("job-older".to_owned()),
            todo_id: Some("todo-1".to_owned()),
            timestamp: chrono::Utc::now(),
            level: "warning".to_owned(),
            phase: Some(Phase::Red),
            msg: "Retry cleanup: discarded local git changes before loop 2/10".to_owned(),
            event_type: EventType::GitOp,
            payload: {
                let mut payload = BTreeMap::new();
                payload.insert(
                    "files".to_owned(),
                    Value::Array(vec![Value::String(
                        "backend/tests/old_should_be_ignored.py".to_owned(),
                    )]),
                );
                payload
            },
        })
        .expect("reset marker event should log");

    let newer_payload = diff_payload(&["backend/app/main.py", "frontend/src/app/page.tsx"]);
    record_single_prompt_event(
        &store,
        "run-resumed",
        "todo-1",
        "info",
        EventType::Diff,
        "Iteration git change detection",
        newer_payload,
        todo_context_hash.as_str(),
        execution_context_hash.as_str(),
    );

    let latest_payload = diff_payload(&["backend/app/main.py", "backend/tests/test_api.py"]);
    record_single_prompt_event(
        &store,
        "run-current",
        "todo-1",
        "info",
        EventType::Diff,
        "Iteration git change detection",
        latest_payload,
        todo_context_hash.as_str(),
        execution_context_hash.as_str(),
    );

    let other_todo_payload = diff_payload(&["backend/ignored_from_other_todo.py"]);
    store
        .record_event(&crate::domain::EventRecord {
            id: None,
            run_id: "run-current".to_owned(),
            job_id: Some("job-current".to_owned()),
            todo_id: Some("todo-other".to_owned()),
            timestamp: chrono::Utc::now(),
            level: "info".to_owned(),
            phase: Some(Phase::SinglePrompt),
            msg: "Iteration git change detection".to_owned(),
            event_type: EventType::Diff,
            payload: other_todo_payload,
        })
        .expect("other-todo event should log");

    let files = execution
        .touched_files_since_last_retry_reset()
        .expect("touched file collection should succeed");

    assert_eq!(
        files,
        vec![
            "backend/app/main.py".to_owned(),
            "backend/tests/test_api.py".to_owned(),
            "frontend/src/app/page.tsx".to_owned(),
        ]
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn previous_attempt_detection_spans_runs_and_resets_on_retry_cleanup() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "detect previous attempt".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();

    let execution = FlowExecution {
        run_id: "run-current".to_owned(),
        job_id: "job-current".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };
    let todo_context_hash = execution.todo_context_hash();
    let execution_context_hash = execution.execution_context_hash();

    assert!(
        !execution
            .has_previous_single_prompt_attempt_since_last_retry_reset()
            .expect("query should succeed"),
        "without history this should be treated as first attempt"
    );

    record_single_prompt_event(
        &store,
        "run-old",
        "todo-1",
        "info",
        EventType::AgentPrompt,
        "Agent prompt (single_prompt)",
        BTreeMap::new(),
        todo_context_hash.as_str(),
        execution_context_hash.as_str(),
    );

    assert!(
        execution
            .has_previous_single_prompt_attempt_since_last_retry_reset()
            .expect("query should succeed"),
        "a previous run attempt should be detected"
    );

    store
        .record_event(&crate::domain::EventRecord {
            id: None,
            run_id: "run-current".to_owned(),
            job_id: Some("job-current".to_owned()),
            todo_id: Some("todo-1".to_owned()),
            timestamp: chrono::Utc::now(),
            level: "warning".to_owned(),
            phase: Some(Phase::Red),
            msg: "Retry cleanup: discarded local git changes before loop 2/10".to_owned(),
            event_type: EventType::GitOp,
            payload: BTreeMap::new(),
        })
        .expect("reset marker event should log");

    assert!(
        !execution
            .has_previous_single_prompt_attempt_since_last_retry_reset()
            .expect("query should succeed"),
        "after retry cleanup reset, prior attempts should be ignored"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn single_prompt_uses_todo_suites_in_prompt_when_todo_sets_subset() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "run single prompt with todo suites".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: vec!["backend".to_owned()],
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = RecordingPromptStore::default();
    let agent = SuccessfulAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();
    let suites = vec![
        suite_named_with_test_command("backend", "exit 0"),
        suite_named_with_test_command("frontend", "exit 0"),
    ];

    let mut execution = FlowExecution {
        run_id: "run-1".to_owned(),
        job_id: "job-1".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &suites,
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    let flow = build_flow(FlowKind::SinglePrompt, 6, 2);
    let outcome = flow
        .run_todo(&mut execution)
        .expect("single_prompt flow should complete");
    assert_eq!(outcome.todo_id, "todo-1");

    let rendered = prompts.rendered_suite_names();
    assert!(
        !rendered.is_empty(),
        "single_prompt should render at least one prompt"
    );
    assert_eq!(
        rendered[0],
        vec!["backend".to_owned()],
        "single_prompt prompt should include todo-configured suites only"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn loop_file_runs_lint_and_tests_for_all_configured_suites() {
    let project_dir = temp_project_dir();
    fs::create_dir_all(&project_dir).expect("project dir should be created");
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let marker_file = project_dir.join("loop-file-all-suites.log");
    let backend_lint = format!("printf BL >> {}", marker_file.display());
    let backend_test = format!("printf BT >> {}", marker_file.display());
    let frontend_lint = format!("printf FL >> {}", marker_file.display());
    let frontend_test = format!("printf FT >> {}", marker_file.display());

    fs::write(
        project_dir.join("chief.yaml"),
        format!(
            "chief:\n  suite_command_timeout_seconds: 1800\nsuites:\n  - name: backend\n    language: shell\n    framework: shell\n    test_root: .\n    test_command: \"{backend_test}\"\n    lint_command: \"{backend_lint}\"\n  - name: frontend\n    language: shell\n    framework: shell\n    test_root: .\n    test_command: \"{frontend_test}\"\n    lint_command: \"{frontend_lint}\"\n"
        ),
    )
    .expect("chief.yaml should be written");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "loop_file should run all configured suites".to_owned(),
        expectations: "task body".to_owned(),
        priority: 1,
        test_suites: vec!["backend".to_owned()],
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = RecordingPromptStore::default();
    let agent = SuccessfulAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();

    let mut execution = FlowExecution {
        run_id: "run-1".to_owned(),
        job_id: "job-1".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    let flow = build_flow(FlowKind::LoopFile, 4, 1);
    let outcome = flow
        .run_todo(&mut execution)
        .expect("loop_file flow should complete");
    assert_eq!(outcome.todo_id, "todo-1");

    let marker_contents =
        fs::read_to_string(&marker_file).expect("suite command marker should exist");
    assert_eq!(
        marker_contents, "BLFLBTFT",
        "loop_file should run lint and test commands for every configured suite"
    );

    let rendered = prompts.rendered_suite_names();
    assert!(
        !rendered.is_empty(),
        "loop_file should render at least one prompt"
    );
    assert_eq!(
        rendered[0],
        vec!["backend".to_owned(), "frontend".to_owned()],
        "loop_file prompt should include all configured suites"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn loop_file_salvages_uncommitted_changes_with_harness_commit() {
    let project_dir = temp_project_dir();
    fs::create_dir_all(&project_dir).expect("project dir should be created");
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo_title = "loop_file harness salvage commit".to_owned();
    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: todo_title.clone(),
        expectations: "task body".to_owned(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = RecordingPromptStore::default();
    let dirty_flag = Arc::new(AtomicBool::new(false));
    let agent = OneShotDirtyAgent::new(project_dir.join("dirty.txt"), dirty_flag.clone());
    let git = DirtyTrackingGitOps::new(project_dir.clone(), "dirty.txt", dirty_flag.clone());
    let chief_config = ChiefConfig::default();

    let mut execution = FlowExecution {
        run_id: "run-1".to_owned(),
        job_id: "job-1".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    let flow = build_flow(FlowKind::LoopFile, 4, 1);
    let outcome = flow
        .run_todo(&mut execution)
        .expect("loop_file flow should salvage and complete");

    assert_eq!(outcome.commit_hash.as_deref(), Some("mock-commit-1"));

    let commit_messages = git.commit_messages();
    assert_eq!(
        commit_messages.len(),
        2,
        "flow should create one salvage commit attempt and one final harness commit attempt"
    );
    assert_eq!(
        commit_messages[0],
        format!("chief(loop_file salvage): {todo_title} (iteration 1)")
    );
    assert_eq!(
        commit_messages[1],
        format!("chief(loop_file): {todo_title}")
    );

    assert!(
        !dirty_flag.load(Ordering::SeqCst),
        "harness salvage commit should leave no pending changes"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn run_test_and_lint_runs_tests_even_when_lint_fails() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "run tests for all suites even when lint fails".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();

    let execution = FlowExecution {
        run_id: "run-1".to_owned(),
        job_id: "job-1".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    let first_marker = project_dir.join("first-suite-test-ran.txt");
    let second_marker = project_dir.join("second-suite-test-ran.txt");
    let mut first =
        suite_named_with_test_command("first", "printf first > first-suite-test-ran.txt");
    first.lint_command = Some("exit 1".to_owned());
    let mut second =
        suite_named_with_test_command("second", "printf second > second-suite-test-ran.txt");
    second.lint_command = Some("exit 0".to_owned());

    let all_ok = super::run_test_and_lint(&execution, &[first, second], Phase::SinglePrompt)
        .expect("test+lint run should complete");

    assert!(!all_ok, "lint failure should keep retry outcome");
    assert!(
        first_marker.exists(),
        "tests should still run for suites whose lint step failed"
    );
    assert!(
        second_marker.exists(),
        "tests should run for all selected suites after lint checks"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn lint_fix_command_recovers_lint_failure() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "lint fix should recover".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();

    let execution = FlowExecution {
        run_id: "run-1".to_owned(),
        job_id: "job-1".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    // The lint command fails on first run but the fix command creates a marker
    // that makes the lint command succeed on re-run.
    let marker = project_dir.join("lint-fixed.txt");
    let lint_cmd = format!("test -f '{}' && exit 0 || exit 1", marker.display());
    let fix_cmd = format!("printf fixed > '{}'", marker.display());

    let mut suite = suite_named("fixable");
    suite.lint_command = Some(lint_cmd);
    suite.lint_fix_command = Some(fix_cmd);

    let all_ok = super::run_lint_checks(&execution, &[suite], Phase::SinglePrompt)
        .expect("lint checks should complete");

    assert!(all_ok, "lint should pass after successful fix + re-check");

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn lint_fix_command_still_fails_when_recheck_fails() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "lint fix cannot recover".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();

    let execution = FlowExecution {
        run_id: "run-1".to_owned(),
        job_id: "job-1".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    // Fix command succeeds but lint still fails on re-check.
    let mut suite = suite_named("unfixable");
    suite.lint_command = Some("exit 1".to_owned());
    suite.lint_fix_command = Some("exit 0".to_owned());

    let all_ok = super::run_lint_checks(&execution, &[suite], Phase::SinglePrompt)
        .expect("lint checks should complete");

    assert!(
        !all_ok,
        "lint should still fail when re-check fails after fix"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn run_test_and_lint_reloads_chief_yaml_suite_commands_between_iterations() {
    let project_dir = temp_project_dir();
    fs::create_dir_all(&project_dir).expect("project dir should be created");
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let marker_file = project_dir.join("reload-suite-command.log");
    let first_command = format!("printf first >> {}", marker_file.display());
    let second_command = format!("printf second >> {}", marker_file.display());

    fs::write(
        project_dir.join("chief.yaml"),
        format!(
            "chief:\n  suite_command_timeout_seconds: 1800\nsuites:\n  - name: backend\n    language: shell\n    framework: shell\n    test_root: .\n    test_command: \"{first_command}\"\n"
        ),
    )
    .expect("initial chief.yaml should be written");

    let todo = Todo {
        id: "todo-1".to_owned(),
        todo: "reload suite command".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: vec!["backend".to_owned()],
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let prompts = NoopPromptStore;
    let agent = NoopAgent;
    let git = NoopGitOps {
        root: project_dir.clone(),
    };
    let chief_config = ChiefConfig::default();
    let initial_suite = suite_named_with_test_command("backend", "printf stale >> does-not-matter");

    let execution = FlowExecution {
        run_id: "run-1".to_owned(),
        job_id: "job-1".to_owned(),
        worker_index: 1,
        project_dir: project_dir.clone(),
        store: &store,
        prompts: &prompts,
        agent: &agent,
        git: &git,
        chief_config: &chief_config,
        all_suites: &[],
        todo,
        cancel_signal: Arc::new(AtomicBool::new(false)),
        prepared_suites: RefCell::new(BTreeSet::new()),
    };

    let first_ok = super::run_test_and_lint(
        &execution,
        std::slice::from_ref(&initial_suite),
        Phase::SinglePrompt,
    )
    .expect("first lint+test run should complete");
    assert!(first_ok, "first lint+test run should pass");

    fs::write(
        project_dir.join("chief.yaml"),
        format!(
            "chief:\n  suite_command_timeout_seconds: 1800\nsuites:\n  - name: backend\n    language: shell\n    framework: shell\n    test_root: .\n    test_command: \"{second_command}\"\n"
        ),
    )
    .expect("updated chief.yaml should be written");

    let second_ok = super::run_test_and_lint(&execution, &[initial_suite], Phase::SinglePrompt)
        .expect("second lint+test run should complete");
    assert!(second_ok, "second lint+test run should pass");

    let marker_contents =
        fs::read_to_string(&marker_file).expect("suite command marker should exist");
    assert_eq!(
        marker_contents, "firstsecond",
        "suite commands should be reloaded from chief.yaml between check iterations"
    );

    let _ = fs::remove_dir_all(&project_dir);
}
