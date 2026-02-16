use crate::agent::{AgentCancelledError, AgentRequest, CodingAgent};
use crate::agent_stream;
use crate::config::{ChiefConfig, TestSuiteConfig};
use crate::domain::{
    AgentOutput, EventRecord, EventType, LoopDecision, Phase, Todo, WaitState, payload_from_json,
};
use crate::git::GitOps;
use crate::prompt::PromptStore;
use crate::storage::{EventQuery, ProjectStore};
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FlowKind {
    #[default]
    Tdd,
    SinglePrompt,
}

impl FlowKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tdd => "tdd",
            Self::SinglePrompt => "single_prompt",
        }
    }

    /// Resolve a configured flow string to its canonical name.
    /// Known flow names are normalized, empty input defaults to `SinglePrompt`,
    /// and unrecognized values are returned as-is (custom flow names).
    pub fn resolve_name(input: &str) -> String {
        let trimmed = input.trim();
        trimmed
            .parse::<FlowKind>()
            .map(|kind| kind.as_str().to_owned())
            .unwrap_or_else(|_| {
                if trimmed.is_empty() {
                    FlowKind::SinglePrompt.as_str().to_owned()
                } else {
                    trimmed.to_owned()
                }
            })
    }
}

#[derive(Debug, Clone)]
pub struct FlowParseError {
    input: String,
}

impl fmt::Display for FlowParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown flow '{}'; expected one of: tdd, single_prompt",
            self.input
        )
    }
}

impl std::error::Error for FlowParseError {}

impl FromStr for FlowKind {
    type Err = FlowParseError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "tdd" => Ok(Self::Tdd),
            "single_prompt" => Ok(Self::SinglePrompt),
            other => Err(FlowParseError {
                input: other.to_owned(),
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TodoOutcome {
    pub todo_id: String,
    pub commit_hash: Option<String>,
}

#[derive(Debug, Clone)]
struct AgentRunWithGitChanges {
    output: AgentOutput,
    touched_files: Vec<String>,
    had_git_changes: bool,
}

#[derive(Debug, Clone, Default)]
struct SinglePromptFailureContext {
    failed_lint: bool,
    failed_test: bool,
    failed_other: bool,
    touched_files_since_last_retry_reset: Vec<String>,
    lint_failures: Vec<SinglePromptFailureItem>,
    test_failures: Vec<SinglePromptFailureItem>,
    other_failures: Vec<SinglePromptFailureItem>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct SinglePromptFailureItem {
    event_id: i64,
    event_type: String,
    message: String,
    command: String,
    output_tail: String,
    sqlite_query: String,
}

const SINGLE_PROMPT_CHANGED_FILES_RETRY_MESSAGE: &str =
    "single_prompt iteration changed files; waiting for two consecutive no-change iterations";
const SINGLE_PROMPT_RETRY_REASON_PAYLOAD_KEY: &str = "single_prompt_retry_reason";
const SINGLE_PROMPT_RETRY_REASON_CONVERGENCE_CHANGED_FILES: &str = "convergence_changed_files";
const SINGLE_PROMPT_RETRY_HAS_ASSOCIATED_TEST_SUITES_PAYLOAD_KEY: &str =
    "single_prompt_retry_has_associated_test_suites";
const TODO_CONTEXT_HASH_PAYLOAD_KEY: &str = "todo_context_hash";
const EXECUTION_CONTEXT_HASH_PAYLOAD_KEY: &str = "execution_context_hash";

#[derive(Debug, Serialize)]
struct TodoContextFingerprint {
    id: String,
    todo: String,
    expectations: String,
    test_suites: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SuiteExecutionFingerprint {
    name: String,
    test_root: String,
    target_type: crate::domain::TargetType,
    default_target: Option<String>,
    strip_root_from_target: bool,
    test_command: String,
    lint_command: Option<String>,
    lint_fix_command: Option<String>,
    post_green_command: Option<String>,
    cleanup_command: Option<String>,
    test_init: Option<String>,
    test_setup: Option<String>,
    cache_paths: Vec<String>,
    cache_key_files: Vec<String>,
    cache_mode: crate::config::SuiteCacheMode,
    command_timeout_seconds: Option<u64>,
    env: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct ExecutionContextFingerprint {
    flow: String,
    required_stable_iterations: usize,
    max_loop_iterations: usize,
    agent_timeout_seconds: u64,
    suite_command_timeout_seconds: u64,
    todo_test_suites: Vec<String>,
    suites: Vec<SuiteExecutionFingerprint>,
}

fn normalized_suite_names(names: &[String]) -> Vec<String> {
    let mut normalized = names
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn md5_hex_of_serializable<T: Serialize>(value: &T) -> String {
    let encoded = serde_json::to_vec(value).unwrap_or_default();
    format!("{:x}", md5::compute(encoded))
}

fn escape_sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SuiteCommandKind {
    Test,
    Lint,
    PostGreen,
}

impl SuiteCommandKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Lint => "lint",
            Self::PostGreen => "post_green",
        }
    }
}

pub fn suite_command_cwd(project_dir: &Path, suite: &TestSuiteConfig) -> PathBuf {
    project_dir.join(&suite.test_root)
}

pub fn suite_command_for_kind(
    suite: &TestSuiteConfig,
    kind: SuiteCommandKind,
    target_override: Option<&str>,
) -> Option<String> {
    match kind {
        SuiteCommandKind::Test => Some(replace_target_placeholder(
            &suite.test_command,
            suite,
            target_override,
        )),
        SuiteCommandKind::Lint => suite
            .lint_command
            .as_ref()
            .map(|cmd| replace_target_placeholder(cmd, suite, target_override)),
        SuiteCommandKind::PostGreen => suite.post_green_command.clone(),
    }
}

pub fn execute_suite_command(
    command: &str,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    cancel_signal: &Arc<AtomicBool>,
    timeout_seconds: Option<u64>,
) -> Result<AgentOutput> {
    let mut process = Command::new("sh");
    process.arg("-lc").arg(command);
    process.current_dir(cwd);
    process.envs(env.iter());
    process.stdout(Stdio::piped());
    process.stderr(Stdio::piped());
    configure_process_group(&mut process);
    let mut child = process
        .spawn()
        .with_context(|| format!("failed to run command: {command}"))?;
    let (output, wait_state) =
        wait_for_command_with_cancel(&mut child, cancel_signal, timeout_seconds)?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let mut merged_output = format!("{stdout}\n{stderr}").trim().to_owned();
    if wait_state == WaitState::TimedOut {
        let timeout_seconds = timeout_seconds.unwrap_or_default();
        if merged_output.is_empty() {
            merged_output = format!(
                "suite command timed out after {timeout_seconds} second(s) and was terminated."
            );
        } else {
            merged_output = format!(
                "suite command timed out after {timeout_seconds} second(s) and was terminated.\n{merged_output}"
            );
        }
    }
    Ok(AgentOutput {
        exit_code: if wait_state == WaitState::TimedOut {
            124
        } else {
            output.status.code().unwrap_or(1)
        },
        command: command.to_owned(),
        merged_output,
        stdout,
        stderr,
    })
}

pub fn execute_suite_cleanup_command(
    cleanup_command: Option<&str>,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    timeout_seconds: Option<u64>,
) -> Result<Option<AgentOutput>> {
    let Some(command) = cleanup_command
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let cancel_signal = Arc::new(AtomicBool::new(false));
    let out = execute_suite_command(command, cwd, env, &cancel_signal, timeout_seconds)?;
    Ok(Some(out))
}

pub fn configure_process_group(process: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.process_group(0);
    }
}

pub fn terminate_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        if let Ok(pid) = i32::try_from(child.id()) {
            let pgid = nix::unistd::Pid::from_raw(pid);
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGTERM);
            std::thread::sleep(Duration::from_millis(200));
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
        }
    }
    let _ = child.kill();
}

fn replace_target_placeholder(
    command: &str,
    suite: &TestSuiteConfig,
    target_override: Option<&str>,
) -> String {
    let target = target_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| suite.default_target.clone())
        .unwrap_or_else(|| ".".to_owned());
    command.replace("{target}", &target)
}

pub struct FlowExecution<'a> {
    pub run_id: String,
    pub job_id: String,
    pub worker_index: usize,
    pub project_dir: PathBuf,
    pub store: &'a ProjectStore,
    pub prompts: &'a dyn PromptStore,
    pub agent: &'a dyn CodingAgent,
    pub git: &'a dyn GitOps,
    pub chief_config: &'a ChiefConfig,
    pub all_suites: &'a [TestSuiteConfig],
    pub todo: Todo,
    pub cancel_signal: Arc<AtomicBool>,
    pub(crate) prepared_suites: RefCell<BTreeSet<String>>,
}

impl<'a> FlowExecution<'a> {
    const RETRY_CLEANUP_DISCARDED_MSG_PREFIX: &'static str =
        "Retry cleanup: discarded local git changes before loop";
    const ITERATION_GIT_CHANGE_DETECTION_MSG: &'static str = "Iteration git change detection";

    pub fn selected_suites(&self) -> Vec<TestSuiteConfig> {
        if self.todo.test_suites.is_empty() {
            return Vec::new();
        }
        let names = self.todo.test_suites.iter().collect::<HashSet<_>>();
        self.all_suites
            .iter()
            .filter(|suite| names.contains(&suite.name))
            .cloned()
            .collect()
    }

    fn todo_context_hash(&self) -> String {
        let fingerprint = TodoContextFingerprint {
            id: self.todo.id.trim().to_owned(),
            todo: self.todo.todo.trim().to_owned(),
            expectations: self.todo.expectations.trim().to_owned(),
            test_suites: normalized_suite_names(&self.todo.test_suites),
        };
        md5_hex_of_serializable(&fingerprint)
    }

    fn execution_context_hash(&self) -> String {
        let todo_suite_names = normalized_suite_names(&self.todo.test_suites);
        let configured_todo_suites = if todo_suite_names.is_empty() {
            Vec::new()
        } else {
            let expected = todo_suite_names.iter().collect::<HashSet<_>>();
            let mut suites = self
                .all_suites
                .iter()
                .filter(|suite| expected.contains(&suite.name))
                .cloned()
                .collect::<Vec<_>>();
            suites.sort_by(|left, right| left.name.cmp(&right.name));
            suites
        };
        let suites = configured_todo_suites
            .into_iter()
            .map(|suite| SuiteExecutionFingerprint {
                name: suite.name,
                test_root: suite.test_root,
                target_type: suite.target_type,
                default_target: suite.default_target,
                strip_root_from_target: suite.strip_root_from_target,
                test_command: suite.test_command,
                lint_command: suite.lint_command,
                lint_fix_command: suite.lint_fix_command,
                post_green_command: suite.post_green_command,
                cleanup_command: suite.cleanup_command,
                test_init: suite.test_init,
                test_setup: suite.test_setup,
                cache_paths: suite.cache_paths,
                cache_key_files: suite.cache_key_files,
                cache_mode: suite.cache_mode,
                command_timeout_seconds: suite.command_timeout_seconds,
                env: suite.env,
            })
            .collect::<Vec<_>>();
        let fingerprint = ExecutionContextFingerprint {
            flow: FlowKind::resolve_name(&self.chief_config.flow),
            required_stable_iterations: self.chief_config.required_stable_iterations,
            max_loop_iterations: self.chief_config.max_loop_iterations,
            agent_timeout_seconds: self.chief_config.agent_timeout_seconds,
            suite_command_timeout_seconds: self.chief_config.suite_command_timeout_seconds,
            todo_test_suites: todo_suite_names,
            suites,
        };
        md5_hex_of_serializable(&fingerprint)
    }

    fn event_matches_current_context(
        event: &EventRecord,
        expected_todo_hash: &str,
        expected_exec_hash: &str,
    ) -> bool {
        let todo_hash = event
            .payload
            .get(TODO_CONTEXT_HASH_PAYLOAD_KEY)
            .and_then(Value::as_str)
            .unwrap_or_default();
        if todo_hash != expected_todo_hash {
            return false;
        }

        let exec_hash = event
            .payload
            .get(EXECUTION_CONTEXT_HASH_PAYLOAD_KEY)
            .and_then(Value::as_str)
            .unwrap_or_default();
        exec_hash == expected_exec_hash
    }

    pub fn log_event(
        &self,
        level: &str,
        phase: Option<Phase>,
        event_type: EventType,
        msg: impl Into<String>,
        mut payload: BTreeMap<String, Value>,
    ) -> Result<()> {
        payload.insert(
            TODO_CONTEXT_HASH_PAYLOAD_KEY.to_owned(),
            Value::String(self.todo_context_hash()),
        );
        payload.insert(
            EXECUTION_CONTEXT_HASH_PAYLOAD_KEY.to_owned(),
            Value::String(self.execution_context_hash()),
        );
        let event = EventRecord {
            id: None,
            run_id: self.run_id.clone(),
            job_id: Some(self.job_id.clone()),
            todo_id: Some(self.todo.id.clone()),
            timestamp: Utc::now(),
            level: level.to_owned(),
            phase,
            msg: msg.into(),
            event_type,
            payload,
        };
        self.store.record_event(&event)
    }

    pub fn previous_steps_log(
        &self,
        phase: Phase,
        event_types: &[EventType],
        limit: usize,
    ) -> Result<String> {
        let events = self.store.query_events(EventQuery {
            limit: limit.max(1) * 6,
            event_type: None,
            phase: Some(phase),
            level: None,
            contains_text: None,
        })?;

        let allowed = event_types
            .iter()
            .map(|event_type| event_type.as_str())
            .collect::<HashSet<_>>();
        let todo_hash = self.todo_context_hash();
        let exec_hash = self.execution_context_hash();

        let mut filtered = events
            .into_iter()
            .filter(|event| event.todo_id.as_deref() == Some(&self.todo.id))
            .filter(|event| allowed.contains(event.event_type.as_str()))
            .filter(|event| {
                Self::event_matches_current_context(event, todo_hash.as_str(), exec_hash.as_str())
            })
            .collect::<Vec<_>>();

        filtered.sort_by_key(|event| event.id);
        if filtered.len() > limit {
            let keep_from = filtered.len() - limit;
            filtered = filtered.split_off(keep_from);
        }

        if filtered.is_empty() {
            return Ok("No previous attempts recorded.".to_owned());
        }

        let mut lines = Vec::with_capacity(filtered.len());
        for (idx, event) in filtered.iter().enumerate() {
            let mut line = format!("[{}] {}: {}", idx + 1, event.event_type.as_str(), event.msg);
            if let Some(command) = event.payload.get("command").and_then(Value::as_str) {
                let command = command.trim();
                if !command.is_empty() {
                    line.push_str("\ncommand: ");
                    line.push_str(command);
                }
            }
            if let Some(output) = event.payload.get("output").and_then(Value::as_str) {
                let tail = output
                    .lines()
                    .rev()
                    .take(self.chief_config.agent_log_max_output_lines)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                if !tail.trim().is_empty() {
                    line.push('\n');
                    line.push_str(&tail);
                }
            }
            lines.push(line);
        }

        Ok(lines.join("\n\n"))
    }

    fn latest_single_prompt_failure_context(&self) -> Result<SinglePromptFailureContext> {
        let events = self.todo_events_since_last_retry_reset(1_000)?;

        let mut lint_failures = Vec::new();
        let mut test_failures = Vec::new();
        let mut other_failures = Vec::new();
        let max_output_lines = self.chief_config.agent_log_max_output_lines;
        let todo_has_associated_test_suites = !self.todo.test_suites.is_empty();
        let configured_suite_names = self
            .todo
            .test_suites
            .iter()
            .map(|suite| suite.trim())
            .filter(|suite| !suite.is_empty())
            .collect::<HashSet<_>>();
        let mut seen_latest_lint_suites = HashSet::new();
        let mut seen_latest_test_suites = HashSet::new();
        let mut include_other_failures = true;

        for event in events {
            if event.phase != Some(Phase::SinglePrompt) {
                continue;
            }

            if event.event_type == EventType::AgentPrompt {
                // Keep "other failures" focused on the latest completed iteration.
                // Lint/test suite status is still collected across runs below.
                include_other_failures = false;
                continue;
            }

            let has_nonzero_exit = event_exit_code(&event).unwrap_or(0) != 0;
            let is_warning_or_error = event.level == "warning" || event.level == "error";

            if matches!(event.event_type, EventType::Lint | EventType::TestRun) {
                let structured_suite_name = suite_name_from_event(&event);
                let suite_name = if let Some(name) = structured_suite_name {
                    if !configured_suite_names.is_empty()
                        && !configured_suite_names.contains(name.as_str())
                    {
                        continue;
                    }
                    name
                } else {
                    // Legacy fallback without suite metadata: keep only latest-iteration context.
                    if !configured_suite_names.is_empty() || !include_other_failures {
                        continue;
                    }
                    let Some(name) = suite_fallback_key_from_event(&event) else {
                        continue;
                    };
                    name
                };

                let seen = if event.event_type == EventType::Lint {
                    &mut seen_latest_lint_suites
                } else {
                    &mut seen_latest_test_suites
                };
                if !seen.insert(suite_name) {
                    continue;
                }

                if has_nonzero_exit {
                    let item = single_prompt_failure_item_from_event(&event, max_output_lines);
                    if event.event_type == EventType::Lint {
                        lint_failures.push(item);
                    } else {
                        test_failures.push(item);
                    }
                }
                continue;
            }

            if !include_other_failures {
                continue;
            }

            if is_single_prompt_convergence_changed_files_retry_event(&event) {
                let has_associated_test_suites = event
                    .payload
                    .get(SINGLE_PROMPT_RETRY_HAS_ASSOCIATED_TEST_SUITES_PAYLOAD_KEY)
                    .and_then(Value::as_bool)
                    .unwrap_or(todo_has_associated_test_suites);
                if !has_associated_test_suites {
                    continue;
                }
            }

            if is_agent_timeout_response_event(&event) {
                continue;
            }

            if has_nonzero_exit
                || matches!(
                    event.event_type,
                    EventType::PhaseFailure | EventType::Error | EventType::AgentResponse
                ) && is_warning_or_error
            {
                other_failures.push(single_prompt_failure_item_from_event(
                    &event,
                    max_output_lines,
                ));
            }
        }

        // query_events returns newest first; prompt context is easier to read oldest->newest.
        lint_failures.reverse();
        test_failures.reverse();
        other_failures.reverse();
        let touched_files_since_last_retry_reset = self.touched_files_since_last_retry_reset()?;

        Ok(SinglePromptFailureContext {
            failed_lint: !lint_failures.is_empty(),
            failed_test: !test_failures.is_empty(),
            failed_other: !other_failures.is_empty(),
            touched_files_since_last_retry_reset,
            lint_failures,
            test_failures,
            other_failures,
        })
    }

    fn has_previous_single_prompt_attempt_since_last_retry_reset(&self) -> Result<bool> {
        let events = self.todo_events_since_last_retry_reset(1_000)?;
        Ok(events.into_iter().any(|event| {
            event.phase == Some(Phase::SinglePrompt) && event.event_type == EventType::AgentPrompt
        }))
    }

    fn todo_events_since_last_retry_reset(&self, limit: usize) -> Result<Vec<EventRecord>> {
        let events = self.store.query_events(EventQuery {
            limit,
            event_type: None,
            phase: None,
            level: None,
            contains_text: None,
        })?;
        let todo_hash = self.todo_context_hash();
        let exec_hash = self.execution_context_hash();

        let mut filtered = Vec::new();
        for event in events {
            if event.todo_id.as_deref() != Some(&self.todo.id) {
                continue;
            }

            if event.event_type == EventType::GitOp
                && event
                    .msg
                    .starts_with(Self::RETRY_CLEANUP_DISCARDED_MSG_PREFIX)
            {
                break;
            }

            if !Self::event_matches_current_context(&event, todo_hash.as_str(), exec_hash.as_str())
            {
                continue;
            }

            filtered.push(event);
        }

        Ok(filtered)
    }

    fn touched_files_since_last_retry_reset(&self) -> Result<Vec<String>> {
        let events = self.todo_events_since_last_retry_reset(1_000)?;

        let mut files = BTreeSet::new();
        for event in events {
            if event.event_type != EventType::Diff
                || event.msg != Self::ITERATION_GIT_CHANGE_DETECTION_MSG
            {
                continue;
            }

            let Some(entries) = event.payload.get("touched_files").and_then(Value::as_array) else {
                continue;
            };

            for entry in entries {
                let Some(path) = entry.as_str().map(str::trim) else {
                    continue;
                };
                if !path.is_empty() {
                    files.insert(path.to_owned());
                }
            }
        }

        Ok(files.into_iter().collect())
    }

    pub fn run_suite_command(
        &self,
        command: &str,
        cwd: &Path,
        env: &BTreeMap<String, String>,
        timeout_seconds: u64,
    ) -> Result<AgentOutput> {
        self.ensure_not_cancelled()?;
        execute_suite_command(
            command,
            cwd,
            env,
            &self.cancel_signal,
            Some(timeout_seconds.max(1)),
        )
    }

    pub fn run_test_suite(&self, suite: &TestSuiteConfig, phase: Phase) -> Result<AgentOutput> {
        self.ensure_suite_prepared(suite, phase)?;
        let cmd = suite_command_for_kind(suite, SuiteCommandKind::Test, None)
            .unwrap_or_else(|| suite.test_command.clone());
        let cwd = suite_command_cwd(&self.project_dir, suite);
        let timeout_seconds = self.suite_command_timeout_seconds(suite);
        self.log_suite_command_started(
            phase,
            suite,
            SuiteCommandKind::Test,
            &cmd,
            &cwd,
            timeout_seconds,
        )?;
        let test_output = self.run_suite_command(&cmd, &cwd, &suite.env, timeout_seconds);
        let cleanup_output = execute_suite_cleanup_command(
            suite.cleanup_command.as_deref(),
            &cwd,
            &suite.env,
            Some(timeout_seconds),
        );

        match cleanup_output {
            Ok(Some(out)) => {
                self.log_event(
                    if out.exit_code == 0 {
                        "info"
                    } else {
                        "warning"
                    },
                    Some(phase),
                    EventType::Msg,
                    format!(
                        "Cleanup command {} ({})",
                        if out.exit_code == 0 {
                            "passed"
                        } else {
                            "failed"
                        },
                        suite.name
                    ),
                    payload_from_json(json!({
                        "suite": suite.name,
                        "kind": "cleanup",
                        "command": out.command,
                        "exit_code": out.exit_code,
                        "output": out.merged_output,
                    })),
                )?;
            }
            Ok(None) => {}
            Err(err) => {
                self.log_event(
                    "warning",
                    Some(phase),
                    EventType::Msg,
                    format!("Cleanup command failed to execute ({})", suite.name),
                    payload_from_json(json!({
                        "suite": suite.name,
                        "kind": "cleanup",
                        "error": err.to_string(),
                    })),
                )?;
            }
        }

        test_output
    }

    pub fn run_lint_suite(
        &self,
        suite: &TestSuiteConfig,
        phase: Phase,
    ) -> Result<Option<AgentOutput>> {
        self.ensure_suite_prepared(suite, phase)?;
        let Some(cmd) = suite_command_for_kind(suite, SuiteCommandKind::Lint, None) else {
            return Ok(None);
        };
        let cwd = suite_command_cwd(&self.project_dir, suite);
        let timeout_seconds = self.suite_command_timeout_seconds(suite);
        self.log_suite_command_started(
            phase,
            suite,
            SuiteCommandKind::Lint,
            &cmd,
            &cwd,
            timeout_seconds,
        )?;
        let out = self.run_suite_command(&cmd, &cwd, &suite.env, timeout_seconds)?;
        Ok(Some(out))
    }

    pub fn run_post_green_suite(
        &self,
        suite: &TestSuiteConfig,
        phase: Phase,
    ) -> Result<Option<AgentOutput>> {
        self.ensure_suite_prepared(suite, phase)?;
        let Some(command) = suite_command_for_kind(suite, SuiteCommandKind::PostGreen, None) else {
            return Ok(None);
        };
        let cwd = suite_command_cwd(&self.project_dir, suite);
        let timeout_seconds = self.suite_command_timeout_seconds(suite);
        self.log_suite_command_started(
            phase,
            suite,
            SuiteCommandKind::PostGreen,
            &command,
            &cwd,
            timeout_seconds,
        )?;
        let out = self.run_suite_command(&command, &cwd, &suite.env, timeout_seconds)?;
        Ok(Some(out))
    }

    pub fn run_agent(
        &self,
        phase: Phase,
        prompt: String,
        disallowed_paths: Vec<String>,
    ) -> Result<AgentOutput> {
        self.ensure_not_cancelled()?;

        let query_id = Uuid::new_v4().to_string();
        let project_name = self
            .store
            .project_dir
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("project")
            .to_owned();
        let todo_id = self.todo.id.clone();

        agent_stream::start_query(
            &project_name,
            &query_id,
            &self.run_id,
            &self.job_id,
            &todo_id,
            phase.as_str(),
        );

        if let Err(err) = self.log_event(
            "info",
            Some(phase),
            EventType::AgentPrompt,
            format!("Agent prompt ({})", phase.as_str()),
            payload_from_json(json!({
                "prompt": prompt,
                "agent_query_id": query_id,
            })),
        ) {
            agent_stream::complete_query(&project_name, &query_id, None, Some(err.to_string()));
            return Err(err);
        }

        let before_files = self
            .git
            .changed_files(&self.project_dir)
            .unwrap_or_default();

        let stream_project = project_name.clone();
        let stream_query_id = query_id.clone();
        let out = match self.agent.run(AgentRequest {
            prompt,
            cwd: self.project_dir.clone(),
            timeout_seconds: Some(self.chief_config.agent_timeout_seconds),
            disallowed_paths,
            cancel_signal: Some(self.cancel_signal.clone()),
            on_chunk: Some(Arc::new(move |stream, text| {
                agent_stream::push_chunk(&stream_project, &stream_query_id, stream, text);
            })),
        }) {
            Ok(out) => out,
            Err(err) => {
                agent_stream::complete_query(&project_name, &query_id, None, Some(err.to_string()));
                return Err(err);
            }
        };

        agent_stream::complete_query(&project_name, &query_id, Some(out.exit_code), None);

        self.log_event(
            if out.exit_code == 0 {
                "info"
            } else {
                "warning"
            },
            Some(phase),
            EventType::AgentResponse,
            format!("Agent response ({})", phase.as_str()),
            payload_from_json(json!({
                "exit_code": out.exit_code,
                "command": out.command,
                "output": out.merged_output,
                "stdout": out.stdout,
                "stderr": out.stderr,
                "agent_query_id": query_id,
            })),
        )?;

        let after_files = self
            .git
            .changed_files(&self.project_dir)
            .unwrap_or_default();
        let new_files = after_files
            .iter()
            .filter(|file| !before_files.contains(file))
            .cloned()
            .collect::<Vec<_>>();

        let diff_summary = self
            .git
            .diff_summary_for_files(&self.project_dir, &new_files)
            .unwrap_or_default();

        self.log_event(
            "info",
            Some(phase),
            EventType::Diff,
            "Diff after agent run",
            payload_from_json(json!({
                "files": new_files,
                "summary": diff_summary,
            })),
        )?;

        Ok(out)
    }

    fn run_agent_with_git_changes(
        &self,
        phase: Phase,
        prompt: String,
        disallowed_paths: Vec<String>,
    ) -> Result<AgentRunWithGitChanges> {
        let before = self.working_tree_snapshot()?;
        let output = self.run_agent(phase, prompt, disallowed_paths)?;
        let after = self.working_tree_snapshot()?;
        let touched_files = changed_paths_between_snapshots(&before, &after);
        let had_git_changes = !touched_files.is_empty();

        self.log_event(
            "info",
            Some(phase),
            EventType::Diff,
            "Iteration git change detection",
            payload_from_json(json!({
                "touched_files": &touched_files,
                "had_git_changes": had_git_changes,
            })),
        )?;

        Ok(AgentRunWithGitChanges {
            output,
            touched_files,
            had_git_changes,
        })
    }

    fn working_tree_snapshot(&self) -> Result<BTreeMap<String, String>> {
        let files = self.git.changed_files(&self.project_dir)?;
        let mut snapshot = BTreeMap::new();
        for file in files {
            let path = self.project_dir.join(&file);
            let signature = if path.is_file() {
                let content = fs::read(&path)
                    .with_context(|| format!("failed reading changed file {}", path.display()))?;
                format!("file:{:x}", md5::compute(content))
            } else if path.is_dir() {
                "dir".to_owned()
            } else {
                "missing".to_owned()
            };
            snapshot.insert(file, signature);
        }
        Ok(snapshot)
    }

    fn ensure_not_cancelled(&self) -> Result<()> {
        if self.cancel_signal.load(Ordering::SeqCst) {
            return Err(anyhow!(AgentCancelledError));
        }
        Ok(())
    }

    fn suite_command_timeout_seconds(&self, suite: &TestSuiteConfig) -> u64 {
        suite
            .command_timeout_seconds
            .unwrap_or(self.chief_config.suite_command_timeout_seconds)
            .max(1)
    }

    fn ensure_suite_prepared(&self, suite: &TestSuiteConfig, phase: Phase) -> Result<()> {
        if self.prepared_suites.borrow().contains(&suite.name) {
            return Ok(());
        }

        self.run_suite_prepare_command(suite, phase, "test_init", suite.test_init.as_deref())?;
        self.run_suite_prepare_command(suite, phase, "test_setup", suite.test_setup.as_deref())?;
        self.prepared_suites.borrow_mut().insert(suite.name.clone());
        Ok(())
    }

    fn run_suite_prepare_command(
        &self,
        suite: &TestSuiteConfig,
        phase: Phase,
        kind: &str,
        command: Option<&str>,
    ) -> Result<()> {
        let Some(command) = command.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(());
        };
        let cwd = suite_command_cwd(&self.project_dir, suite);
        let timeout_seconds = self.suite_command_timeout_seconds(suite);
        self.log_event(
            "info",
            Some(phase),
            EventType::PhaseChange,
            format!("Running {kind} command ({})", suite.name),
            payload_from_json(json!({
                "suite": suite.name,
                "kind": kind,
                "command": command,
                "cwd": cwd.display().to_string(),
                "timeout_seconds": timeout_seconds,
            })),
        )?;
        let out = self.run_suite_command(command, &cwd, &suite.env, timeout_seconds)?;
        self.log_event(
            if out.exit_code == 0 {
                "info"
            } else {
                "warning"
            },
            Some(phase),
            EventType::Msg,
            format!(
                "{kind} command {} ({})",
                if out.exit_code == 0 {
                    "passed"
                } else {
                    "failed"
                },
                suite.name
            ),
            payload_from_json(json!({
                "suite": suite.name,
                "kind": kind,
                "command": out.command,
                "exit_code": out.exit_code,
                "output": out.merged_output,
            })),
        )?;
        if out.exit_code != 0 {
            return Err(anyhow!(
                "{} command failed for suite {} (exit code {})",
                kind,
                suite.name,
                out.exit_code
            ));
        }
        Ok(())
    }

    fn log_suite_command_started(
        &self,
        phase: Phase,
        suite: &TestSuiteConfig,
        kind: SuiteCommandKind,
        command: &str,
        cwd: &Path,
        timeout_seconds: u64,
    ) -> Result<()> {
        self.log_event(
            "info",
            Some(phase),
            EventType::PhaseChange,
            format!("Running {} command ({})", kind.as_str(), suite.name),
            payload_from_json(json!({
                "suite": suite.name,
                "kind": kind.as_str(),
                "command": command,
                "cwd": cwd.display().to_string(),
                "timeout_seconds": timeout_seconds,
            })),
        )
    }
}

fn event_exit_code(event: &EventRecord) -> Option<i64> {
    let value = event.payload.get("exit_code")?;
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn suite_name_from_event(event: &EventRecord) -> Option<String> {
    if let Some(suite) = event
        .payload
        .get("suite")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
    {
        return Some(suite);
    }

    if let Some(open) = event.msg.rfind('(')
        && event.msg.ends_with(')')
        && open + 1 < event.msg.len() - 1
    {
        let suite = event.msg[open + 1..event.msg.len() - 1].trim();
        if !suite.is_empty() {
            return Some(suite.to_owned());
        }
    }

    None
}

fn suite_fallback_key_from_event(event: &EventRecord) -> Option<String> {
    event
        .payload
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            let msg = event.msg.trim();
            if msg.is_empty() {
                None
            } else {
                Some(msg.to_owned())
            }
        })
}

fn is_agent_timeout_response_event(event: &EventRecord) -> bool {
    if event.event_type == EventType::AgentResponse {
        return event
            .payload
            .get("output")
            .and_then(Value::as_str)
            .map(|output| {
                output.contains("agent timed out after ") && output.contains(" second(s)")
            })
            .unwrap_or(false);
    }

    event.event_type == EventType::PhaseFailure
        && event.msg == "single_prompt agent step failed"
        && event_exit_code(event) == Some(124)
}

fn is_single_prompt_convergence_changed_files_retry_event(event: &EventRecord) -> bool {
    if event.event_type != EventType::PhaseFailure {
        return false;
    }

    if matches!(
        event
            .payload
            .get(SINGLE_PROMPT_RETRY_REASON_PAYLOAD_KEY)
            .and_then(Value::as_str),
        Some(reason) if reason == SINGLE_PROMPT_RETRY_REASON_CONVERGENCE_CHANGED_FILES
    ) {
        return true;
    }

    // Backward-compatible fallback for old events logged before retry metadata existed.
    event.msg == SINGLE_PROMPT_CHANGED_FILES_RETRY_MESSAGE
}

fn tail_output_lines(output: &str, max_lines: usize) -> String {
    output
        .lines()
        .rev()
        .take(max_lines.max(1))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

fn single_prompt_failure_item_from_event(
    event: &EventRecord,
    max_output_lines: usize,
) -> SinglePromptFailureItem {
    let command = event
        .payload
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_default();
    let raw_output = event
        .payload
        .get("output")
        .and_then(Value::as_str)
        .or_else(|| event.payload.get("stderr").and_then(Value::as_str))
        .or_else(|| event.payload.get("stdout").and_then(Value::as_str))
        .unwrap_or("");
    let output_tail = tail_output_lines(raw_output, max_output_lines);
    let event_id = event.id.unwrap_or_default();
    let run_id = escape_sql_literal(&event.run_id);
    let todo_id = escape_sql_literal(event.todo_id.as_deref().unwrap_or_default());
    let sqlite_query = format!(
        "SELECT id,timestamp,phase,msg,payload FROM events WHERE run_id='{run_id}' AND todo_id='{todo_id}' AND id={event_id} LIMIT 1;"
    );
    SinglePromptFailureItem {
        event_id,
        event_type: event.event_type.as_str().to_owned(),
        message: event.msg.clone(),
        command,
        output_tail,
        sqlite_query,
    }
}

pub trait PhaseStrategy {
    fn phase(&self) -> Phase;
    fn check_goal_before_loop(&self) -> bool {
        false
    }
    fn attempt_fix(&mut self, execution: &mut FlowExecution<'_>) -> Result<AgentOutput>;
    fn check_goal(
        &mut self,
        execution: &mut FlowExecution<'_>,
        iteration_idx: isize,
        output: &AgentOutput,
    ) -> Result<LoopDecision>;
}

pub trait LoopPolicy: Send + Sync {
    fn name(&self) -> &'static str;
    fn run(
        &self,
        strategy: &mut dyn PhaseStrategy,
        execution: &mut FlowExecution<'_>,
    ) -> Result<()>;
}

fn phase_label_for_log(phase: Phase) -> String {
    phase.as_str().to_ascii_uppercase()
}

#[derive(Debug, Clone)]
pub struct ConvergenceLoopPolicy {
    pub required_stable_iterations: usize,
    pub max_loops: usize,
}

impl Default for ConvergenceLoopPolicy {
    fn default() -> Self {
        Self {
            required_stable_iterations: 2,
            max_loops: 6,
        }
    }
}

impl LoopPolicy for ConvergenceLoopPolicy {
    fn name(&self) -> &'static str {
        "convergence"
    }

    fn run(
        &self,
        strategy: &mut dyn PhaseStrategy,
        execution: &mut FlowExecution<'_>,
    ) -> Result<()> {
        let phase = strategy.phase();
        let phase_label = phase_label_for_log(phase);
        let mut stable = 0usize;
        if strategy.check_goal_before_loop() {
            let pre = strategy.check_goal(execution, -1, &AgentOutput::success("precheck", ""))?;
            if matches!(pre, LoopDecision::Success) {
                execution.log_event(
                    "info",
                    Some(phase),
                    EventType::PhaseChange,
                    format!("{phase_label} phase done during pre-check"),
                    BTreeMap::new(),
                )?;
                return Ok(());
            }
        }

        for iteration in 0..self.max_loops {
            execution.log_event(
                "info",
                Some(phase),
                EventType::PhaseChange,
                format!(
                    "{} loop iteration {}/{}",
                    self.name(),
                    iteration + 1,
                    self.max_loops
                ),
                BTreeMap::new(),
            )?;

            let output = strategy.attempt_fix(execution)?;
            let decision = strategy.check_goal(execution, iteration as isize, &output)?;
            match decision {
                LoopDecision::Success => {
                    execution.log_event(
                        "info",
                        Some(phase),
                        EventType::PhaseChange,
                        format!(
                            "{phase_label} phase done on iteration {}/{}",
                            iteration + 1,
                            self.max_loops
                        ),
                        BTreeMap::new(),
                    )?;
                    return Ok(());
                }
                LoopDecision::Stable => {
                    stable += 1;
                    if stable >= self.required_stable_iterations {
                        execution.log_event(
                            "info",
                            Some(phase),
                            EventType::PhaseChange,
                            format!(
                                "{phase_label} phase done after stable result {}/{}",
                                stable, self.required_stable_iterations
                            ),
                            BTreeMap::new(),
                        )?;
                        return Ok(());
                    }
                    execution.log_event(
                        "info",
                        Some(phase),
                        EventType::PhaseChange,
                        format!(
                            "{phase_label} phase stable {stable}/{}; retrying to confirm",
                            self.required_stable_iterations
                        ),
                        BTreeMap::new(),
                    )?;
                }
                LoopDecision::Retry => {
                    stable = 0;
                    execution.log_event(
                        "warning",
                        Some(phase),
                        EventType::PhaseChange,
                        format!(
                            "{phase_label} phase retrying after iteration {}/{}",
                            iteration + 1,
                            self.max_loops
                        ),
                        BTreeMap::new(),
                    )?;
                }
            }
        }

        execution.log_event(
            "warning",
            Some(phase),
            EventType::PhaseFailure,
            format!(
                "{phase_label} phase retry loop exhausted after {} iterations",
                self.max_loops
            ),
            BTreeMap::new(),
        )?;
        Err(anyhow!("convergence loop failed to converge"))
    }
}

#[derive(Debug, Clone)]
pub struct UntilPassLoopPolicy {
    pub max_loops: usize,
}

impl Default for UntilPassLoopPolicy {
    fn default() -> Self {
        Self { max_loops: 6 }
    }
}

impl LoopPolicy for UntilPassLoopPolicy {
    fn name(&self) -> &'static str {
        "until_pass"
    }

    fn run(
        &self,
        strategy: &mut dyn PhaseStrategy,
        execution: &mut FlowExecution<'_>,
    ) -> Result<()> {
        let phase = strategy.phase();
        let phase_label = phase_label_for_log(phase);
        if strategy.check_goal_before_loop() {
            let pre = strategy.check_goal(execution, -1, &AgentOutput::success("precheck", ""))?;
            if matches!(pre, LoopDecision::Success) {
                execution.log_event(
                    "info",
                    Some(phase),
                    EventType::PhaseChange,
                    format!("{phase_label} phase done during pre-check"),
                    BTreeMap::new(),
                )?;
                return Ok(());
            }
        }

        for iteration in 0..self.max_loops {
            execution.log_event(
                "info",
                Some(phase),
                EventType::PhaseChange,
                format!(
                    "{} loop iteration {}/{}",
                    self.name(),
                    iteration + 1,
                    self.max_loops
                ),
                BTreeMap::new(),
            )?;
            let output = strategy.attempt_fix(execution)?;
            let decision = strategy.check_goal(execution, iteration as isize, &output)?;
            if matches!(decision, LoopDecision::Success) {
                execution.log_event(
                    "info",
                    Some(phase),
                    EventType::PhaseChange,
                    format!(
                        "{phase_label} phase done on iteration {}/{}",
                        iteration + 1,
                        self.max_loops
                    ),
                    BTreeMap::new(),
                )?;
                return Ok(());
            }
            execution.log_event(
                "warning",
                Some(phase),
                EventType::PhaseChange,
                format!(
                    "{phase_label} phase retrying after iteration {}/{}",
                    iteration + 1,
                    self.max_loops
                ),
                BTreeMap::new(),
            )?;
        }

        execution.log_event(
            "warning",
            Some(phase),
            EventType::PhaseFailure,
            format!(
                "{phase_label} phase retry loop exhausted after {} iterations",
                self.max_loops
            ),
            BTreeMap::new(),
        )?;
        Err(anyhow!("until-pass loop failed to reach success"))
    }
}

pub trait TodoFlow: Send + Sync {
    fn name(&self) -> &'static str;
    fn run_todo(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome>;
}

#[derive(Debug, Clone)]
pub struct TddFlow {
    red_loop: ConvergenceLoopPolicy,
    green_loop: UntilPassLoopPolicy,
    post_green_loop: UntilPassLoopPolicy,
}

impl Default for TddFlow {
    fn default() -> Self {
        Self::with_loop_policy(6, 2)
    }
}

impl TddFlow {
    pub fn with_loop_policy(max_loop: usize, required_stable_iterations: usize) -> Self {
        let red_loop = ConvergenceLoopPolicy {
            max_loops: max_loop.max(1),
            required_stable_iterations: required_stable_iterations.max(1),
        };
        Self {
            red_loop,
            green_loop: UntilPassLoopPolicy {
                max_loops: max_loop.max(1),
            },
            post_green_loop: UntilPassLoopPolicy {
                max_loops: max_loop.max(1),
            },
        }
    }
}

impl TodoFlow for TddFlow {
    fn name(&self) -> &'static str {
        "tdd"
    }

    fn run_todo(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome> {
        let suites = execution.selected_suites();
        for suite in &suites {
            execution.ensure_suite_prepared(suite, Phase::Red)?;
        }

        if !suites.is_empty() {
            let mut red = RedPhaseStrategy::new(suites.clone());
            self.red_loop.run(&mut red, execution)?;
            execution.log_event(
                "info",
                Some(Phase::Red),
                EventType::PhaseChange,
                "RED phase done; starting GREEN phase",
                BTreeMap::new(),
            )?;
        }

        let mut green = GreenPhaseStrategy::new(suites.clone());
        self.green_loop.run(&mut green, execution)?;
        execution.log_event(
            "info",
            Some(Phase::Green),
            EventType::PhaseChange,
            "GREEN phase done; starting POST_GREEN phase",
            BTreeMap::new(),
        )?;

        let mut post_green = PostGreenPhaseStrategy::new(suites);
        self.post_green_loop.run(&mut post_green, execution)?;
        execution.log_event(
            "info",
            Some(Phase::PostGreen),
            EventType::PhaseChange,
            "POST_GREEN phase done; preparing commit",
            BTreeMap::new(),
        )?;

        let commit_hash = execution
            .git
            .commit_and_tag(
                &execution.project_dir,
                &format!("chief: {}", execution.todo.todo),
            )
            .context("failed to commit completed todo")?;

        execution.log_event(
            "info",
            Some(Phase::Exit),
            EventType::GitOp,
            format!("Committed todo {}", execution.todo.id),
            payload_from_json(json!({ "commit_hash": commit_hash })),
        )?;

        Ok(TodoOutcome {
            todo_id: execution.todo.id.clone(),
            commit_hash: Some(commit_hash),
        })
    }
}

#[derive(Debug, Clone)]
pub struct SinglePromptFlow {
    loop_policy: ConvergenceLoopPolicy,
}

impl Default for SinglePromptFlow {
    fn default() -> Self {
        Self::with_loop_policy(6, 2)
    }
}

impl SinglePromptFlow {
    pub fn with_loop_policy(max_loop: usize, required_stable_iterations: usize) -> Self {
        let loop_policy = ConvergenceLoopPolicy {
            max_loops: max_loop.max(1),
            required_stable_iterations: required_stable_iterations.max(1),
        };
        Self { loop_policy }
    }
}

impl TodoFlow for SinglePromptFlow {
    fn name(&self) -> &'static str {
        "single_prompt"
    }

    fn run_todo(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome> {
        let candidate_suites = execution.selected_suites();
        for suite in &candidate_suites {
            execution.ensure_suite_prepared(suite, Phase::SinglePrompt)?;
        }

        let mut strategy = SinglePromptPhaseStrategy::new(candidate_suites);
        self.loop_policy.run(&mut strategy, execution)?;
        strategy.run_post_green_for_involved_suites(execution)?;

        execution.log_event(
            "info",
            Some(Phase::SinglePrompt),
            EventType::PhaseChange,
            "SINGLE_PROMPT loop done; preparing commit",
            BTreeMap::new(),
        )?;

        let commit_hash = execution
            .git
            .commit_and_tag(
                &execution.project_dir,
                &format!("chief(single_prompt): {}", execution.todo.todo),
            )
            .context("failed to commit todo")?;

        execution.log_event(
            "info",
            Some(Phase::Exit),
            EventType::GitOp,
            format!("Committed todo {}", execution.todo.id),
            payload_from_json(json!({ "commit_hash": commit_hash })),
        )?;

        Ok(TodoOutcome {
            todo_id: execution.todo.id.clone(),
            commit_hash: Some(commit_hash),
        })
    }
}

#[derive(Debug, Clone)]
struct SinglePromptPhaseStrategy {
    candidate_suites: Vec<TestSuiteConfig>,
    involved_suite_names: BTreeSet<String>,
    last_agent_run: Option<AgentRunWithGitChanges>,
    attempts: usize,
}

impl SinglePromptPhaseStrategy {
    fn new(candidate_suites: Vec<TestSuiteConfig>) -> Self {
        Self {
            candidate_suites,
            involved_suite_names: BTreeSet::new(),
            last_agent_run: None,
            attempts: 0,
        }
    }

    fn involved_suites(&self) -> Vec<TestSuiteConfig> {
        if self.involved_suite_names.is_empty() {
            return Vec::new();
        }
        self.candidate_suites
            .iter()
            .filter(|suite| self.involved_suite_names.contains(&suite.name))
            .cloned()
            .collect()
    }

    fn run_post_green_for_involved_suites(&self, execution: &FlowExecution<'_>) -> Result<()> {
        let involved_suites = self.involved_suites();
        if involved_suites.is_empty() {
            execution.log_event(
                "info",
                Some(Phase::PostGreen),
                EventType::PhaseChange,
                "single_prompt: no involved suites; skipping post-green commands",
                BTreeMap::new(),
            )?;
            return Ok(());
        }

        let mut post_green_ok = true;
        for suite in &involved_suites {
            if let Some(out) = execution.run_post_green_suite(suite, Phase::PostGreen)? {
                execution.log_event(
                    if out.exit_code == 0 {
                        "info"
                    } else {
                        "warning"
                    },
                    Some(Phase::PostGreen),
                    EventType::PostGreenOutput,
                    format!("Post-green command result ({})", suite.name),
                    payload_from_json(json!({
                        "suite": suite.name,
                        "command": out.command,
                        "exit_code": out.exit_code,
                        "output": out.merged_output,
                    })),
                )?;
                if out.exit_code != 0 {
                    post_green_ok = false;
                }
            }
        }

        if post_green_ok {
            Ok(())
        } else {
            Err(anyhow!(
                "single_prompt post-green checks failed for involved suites"
            ))
        }
    }
}

impl PhaseStrategy for SinglePromptPhaseStrategy {
    fn phase(&self) -> Phase {
        Phase::SinglePrompt
    }

    fn attempt_fix(&mut self, execution: &mut FlowExecution<'_>) -> Result<AgentOutput> {
        let failure_context = execution.latest_single_prompt_failure_context()?;
        let has_previous_attempts = self.attempts > 0
            || execution.has_previous_single_prompt_attempt_since_last_retry_reset()?;

        let prompt = execution.prompts.render_json(
            "singleprompt.md",
            &json!({
                "todo": execution.todo,
                "suites": self.candidate_suites,
                "iteration": self.attempts + 1,
                "run_id": execution.run_id,
                "first_attempt": !has_previous_attempts,
                "failed_lint": failure_context.failed_lint,
                "failed_test": failure_context.failed_test,
                "failed_other": failure_context.failed_other,
                "touched_files_since_last_retry_reset": failure_context.touched_files_since_last_retry_reset,
                "lint_failures": failure_context.lint_failures,
                "test_failures": failure_context.test_failures,
                "other_failures": failure_context.other_failures,
            }),
        )?;

        let run = execution.run_agent_with_git_changes(Phase::SinglePrompt, prompt, Vec::new())?;
        let output = run.output.clone();
        self.last_agent_run = Some(run);
        self.attempts += 1;
        Ok(output)
    }

    fn check_goal(
        &mut self,
        execution: &mut FlowExecution<'_>,
        _iteration_idx: isize,
        output: &AgentOutput,
    ) -> Result<LoopDecision> {
        let run = self
            .last_agent_run
            .take()
            .unwrap_or_else(|| AgentRunWithGitChanges {
                output: output.clone(),
                touched_files: Vec::new(),
                had_git_changes: true,
            });

        let suites_for_checks = self.candidate_suites.clone();
        for suite in &suites_for_checks {
            self.involved_suite_names.insert(suite.name.clone());
        }

        if suites_for_checks.is_empty() {
            execution.log_event(
                "info",
                Some(Phase::SinglePrompt),
                EventType::PhaseChange,
                "single_prompt: no todo-associated suites; skipping lint+test commands",
                BTreeMap::new(),
            )?;
        } else {
            let all_pass = run_test_and_lint(execution, &suites_for_checks, Phase::SinglePrompt)?;
            if !all_pass {
                return Ok(LoopDecision::Retry);
            }
        }

        if run.output.exit_code != 0 {
            execution.log_event(
                "warning",
                Some(Phase::SinglePrompt),
                EventType::PhaseFailure,
                "single_prompt agent step failed",
                payload_from_json(json!({
                    "exit_code": run.output.exit_code,
                    "command": run.output.command,
                })),
            )?;
            return Ok(LoopDecision::Retry);
        }

        if run.had_git_changes {
            let has_associated_test_suites = !execution.todo.test_suites.is_empty();
            execution.log_event(
                "warning",
                Some(Phase::SinglePrompt),
                EventType::PhaseFailure,
                SINGLE_PROMPT_CHANGED_FILES_RETRY_MESSAGE,
                payload_from_json(json!({
                    "touched_files": run.touched_files,
                    (SINGLE_PROMPT_RETRY_REASON_PAYLOAD_KEY): SINGLE_PROMPT_RETRY_REASON_CONVERGENCE_CHANGED_FILES,
                    (SINGLE_PROMPT_RETRY_HAS_ASSOCIATED_TEST_SUITES_PAYLOAD_KEY): has_associated_test_suites,
                })),
            )?;
            return Ok(LoopDecision::Retry);
        }

        execution.log_event(
            "info",
            Some(Phase::SinglePrompt),
            EventType::PhaseChange,
            "single_prompt iteration had no git file changes",
            BTreeMap::new(),
        )?;
        Ok(LoopDecision::Stable)
    }
}

pub fn build_flow(
    flow_kind: FlowKind,
    max_loop: usize,
    required_stable_iterations: usize,
) -> Box<dyn TodoFlow> {
    let max_loop = max_loop.max(1);
    let required_stable_iterations = required_stable_iterations.max(1);
    match flow_kind {
        FlowKind::SinglePrompt => Box::new(SinglePromptFlow::with_loop_policy(
            max_loop,
            required_stable_iterations,
        )),
        FlowKind::Tdd => Box::new(TddFlow::with_loop_policy(
            max_loop,
            required_stable_iterations,
        )),
    }
}

#[derive(Debug, Clone)]
struct RedPhaseStrategy {
    suites: Vec<TestSuiteConfig>,
}

impl RedPhaseStrategy {
    fn new(suites: Vec<TestSuiteConfig>) -> Self {
        Self { suites }
    }
}

impl PhaseStrategy for RedPhaseStrategy {
    fn phase(&self) -> Phase {
        Phase::Red
    }

    fn attempt_fix(&mut self, execution: &mut FlowExecution<'_>) -> Result<AgentOutput> {
        let previous_steps_log = execution.previous_steps_log(
            Phase::Red,
            &[
                EventType::TestRun,
                EventType::PhaseFailure,
                EventType::AgentResponse,
                EventType::Diff,
                EventType::Lint,
            ],
            8,
        )?;

        let prompt = execution.prompts.render_json(
            "red.md",
            &json!({
                "todo": execution.todo,
                "suites": self.suites,
                "previous_steps_log": previous_steps_log,
            }),
        )?;

        execution.run_agent(Phase::Red, prompt, Vec::new())
    }

    fn check_goal(
        &mut self,
        execution: &mut FlowExecution<'_>,
        _iteration_idx: isize,
        output: &AgentOutput,
    ) -> Result<LoopDecision> {
        if output.exit_code != 0 {
            execution.log_event(
                "warning",
                Some(Phase::Red),
                EventType::PhaseFailure,
                "red attempt failed",
                payload_from_json(json!({
                    "exit_code": output.exit_code,
                    "command": output.command,
                })),
            )?;
            return Ok(LoopDecision::Retry);
        }

        let lint_ok = run_lint_checks(execution, &self.suites, Phase::Red)?;
        if lint_ok {
            Ok(LoopDecision::Stable)
        } else {
            Ok(LoopDecision::Retry)
        }
    }
}

#[derive(Debug, Clone)]
struct GreenPhaseStrategy {
    suites: Vec<TestSuiteConfig>,
}

impl GreenPhaseStrategy {
    fn new(suites: Vec<TestSuiteConfig>) -> Self {
        Self { suites }
    }
}

impl PhaseStrategy for GreenPhaseStrategy {
    fn phase(&self) -> Phase {
        Phase::Green
    }

    fn attempt_fix(&mut self, execution: &mut FlowExecution<'_>) -> Result<AgentOutput> {
        let previous_steps_log = execution.previous_steps_log(
            Phase::Green,
            &[
                EventType::TestRun,
                EventType::PhaseFailure,
                EventType::PostGreenOutput,
                EventType::AgentResponse,
                EventType::Diff,
                EventType::Lint,
            ],
            8,
        )?;

        let prompt = execution.prompts.render_json(
            "green.md",
            &json!({
                "todo": execution.todo,
                "previous_steps_log": previous_steps_log,
            }),
        )?;

        execution.run_agent(Phase::Green, prompt, Vec::new())
    }

    fn check_goal(
        &mut self,
        execution: &mut FlowExecution<'_>,
        _iteration_idx: isize,
        output: &AgentOutput,
    ) -> Result<LoopDecision> {
        if output.exit_code != 0 {
            return Ok(LoopDecision::Retry);
        }

        if self.suites.is_empty() {
            // No explicit tests configured: success on successful agent run.
            return Ok(LoopDecision::Success);
        }

        let all_pass = run_test_and_lint(execution, &self.suites, Phase::Green)?;
        if all_pass {
            Ok(LoopDecision::Success)
        } else {
            Ok(LoopDecision::Retry)
        }
    }
}

#[derive(Debug, Clone)]
struct PostGreenPhaseStrategy {
    suites: Vec<TestSuiteConfig>,
}

impl PostGreenPhaseStrategy {
    fn new(suites: Vec<TestSuiteConfig>) -> Self {
        Self { suites }
    }
}

impl PhaseStrategy for PostGreenPhaseStrategy {
    fn phase(&self) -> Phase {
        Phase::PostGreen
    }

    fn check_goal_before_loop(&self) -> bool {
        true
    }

    fn attempt_fix(&mut self, execution: &mut FlowExecution<'_>) -> Result<AgentOutput> {
        let previous_steps_log = execution.previous_steps_log(
            Phase::PostGreen,
            &[
                EventType::PostGreenOutput,
                EventType::PhaseFailure,
                EventType::TestRun,
                EventType::AgentResponse,
                EventType::Diff,
                EventType::Lint,
            ],
            8,
        )?;

        let post_green_commands = self
            .suites
            .iter()
            .filter_map(|suite| suite.post_green_command.clone())
            .collect::<Vec<_>>();

        let prompt = execution.prompts.render_json(
            "post_green.md",
            &json!({
                "todo": execution.todo,
                "post_green_commands": post_green_commands,
                "previous_steps_log": previous_steps_log,
            }),
        )?;

        execution.run_agent(Phase::PostGreen, prompt, Vec::new())
    }

    fn check_goal(
        &mut self,
        execution: &mut FlowExecution<'_>,
        _iteration_idx: isize,
        _output: &AgentOutput,
    ) -> Result<LoopDecision> {
        if self.suites.is_empty() {
            return Ok(LoopDecision::Success);
        }

        let lint_ok = run_lint_checks(execution, &self.suites, Phase::PostGreen)?;
        let mut post_green_ok = true;

        for suite in &self.suites {
            if let Some(out) = execution.run_post_green_suite(suite, Phase::PostGreen)? {
                execution.log_event(
                    if out.exit_code == 0 {
                        "info"
                    } else {
                        "warning"
                    },
                    Some(Phase::PostGreen),
                    EventType::PostGreenOutput,
                    format!("Post-green command result ({})", suite.name),
                    payload_from_json(json!({
                        "suite": suite.name,
                        "command": out.command,
                        "exit_code": out.exit_code,
                        "output": out.merged_output,
                    })),
                )?;
                if out.exit_code != 0 {
                    post_green_ok = false;
                }
            }
        }

        if lint_ok && post_green_ok {
            Ok(LoopDecision::Success)
        } else {
            Ok(LoopDecision::Retry)
        }
    }
}

fn run_lint_checks(
    execution: &FlowExecution<'_>,
    suites: &[TestSuiteConfig],
    phase: Phase,
) -> Result<bool> {
    let mut all_ok = true;

    for suite in suites {
        let Some(out) = execution.run_lint_suite(suite, phase)? else {
            continue;
        };

        execution.log_event(
            if out.exit_code == 0 {
                "info"
            } else {
                "warning"
            },
            Some(phase),
            EventType::Lint,
            format!(
                "Lint {} ({})",
                if out.exit_code == 0 {
                    "passed"
                } else {
                    "failed"
                },
                suite.name
            ),
            payload_from_json(json!({
                "suite": suite.name,
                "command": out.command,
                "exit_code": out.exit_code,
                "output": out.merged_output,
            })),
        )?;

        if out.exit_code != 0 {
            if let Some(fix_cmd) = &suite.lint_fix_command {
                let cwd = suite_command_cwd(&execution.project_dir, suite);
                let timeout_seconds = execution.suite_command_timeout_seconds(suite);
                let fix_out =
                    execution.run_suite_command(fix_cmd, &cwd, &suite.env, timeout_seconds)?;

                execution.log_event(
                    if fix_out.exit_code == 0 {
                        "info"
                    } else {
                        "warning"
                    },
                    Some(phase),
                    EventType::LintFix,
                    format!(
                        "Lint fix {} ({})",
                        if fix_out.exit_code == 0 {
                            "succeeded"
                        } else {
                            "failed"
                        },
                        suite.name
                    ),
                    payload_from_json(json!({
                        "suite": suite.name,
                        "command": fix_out.command,
                        "exit_code": fix_out.exit_code,
                        "output": fix_out.merged_output,
                    })),
                )?;

                if fix_out.exit_code == 0 {
                    // Re-run lint after successful fix.
                    if let Some(recheck) = execution.run_lint_suite(suite, phase)? {
                        execution.log_event(
                            if recheck.exit_code == 0 {
                                "info"
                            } else {
                                "warning"
                            },
                            Some(phase),
                            EventType::Lint,
                            format!(
                                "Lint re-check {} ({})",
                                if recheck.exit_code == 0 {
                                    "passed"
                                } else {
                                    "still failing"
                                },
                                suite.name
                            ),
                            payload_from_json(json!({
                                "suite": suite.name,
                                "command": recheck.command,
                                "exit_code": recheck.exit_code,
                                "output": recheck.merged_output,
                            })),
                        )?;

                        if recheck.exit_code == 0 {
                            continue; // Fixed — count as pass.
                        }
                    }
                }
            }

            all_ok = false;
        }
    }

    Ok(all_ok)
}

fn changed_paths_between_snapshots(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut touched = BTreeSet::new();

    for (path, before_signature) in before {
        let changed = match after.get(path) {
            Some(after_signature) => after_signature != before_signature,
            None => true,
        };
        if changed {
            touched.insert(path.clone());
        }
    }

    for path in after.keys() {
        if !before.contains_key(path) {
            touched.insert(path.clone());
        }
    }

    touched.into_iter().collect()
}

fn wait_for_command_with_cancel(
    child: &mut std::process::Child,
    cancel_signal: &Arc<AtomicBool>,
    timeout_seconds: Option<u64>,
) -> Result<(std::process::Output, WaitState)> {
    let stdout_reader = spawn_pipe_reader(child.stdout.take());
    let stderr_reader = spawn_pipe_reader(child.stderr.take());
    let timeout = timeout_seconds
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs);
    let started = Instant::now();

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break (status, WaitState::Completed);
        }

        if cancel_signal.load(Ordering::SeqCst) {
            terminate_process_tree(child);
            break (child.wait()?, WaitState::Cancelled);
        }

        if timeout
            .map(|limit| started.elapsed() >= limit)
            .unwrap_or(false)
        {
            terminate_process_tree(child);
            break (child.wait()?, WaitState::TimedOut);
        }

        std::thread::sleep(Duration::from_millis(50));
    };

    let stdout = join_pipe_reader(stdout_reader, "stdout")?;
    let stderr = join_pipe_reader(stderr_reader, "stderr")?;

    if status.1 == WaitState::Cancelled {
        return Err(anyhow!(AgentCancelledError));
    }

    Ok((
        std::process::Output {
            status: status.0,
            stdout,
            stderr,
        },
        status.1,
    ))
}

fn spawn_pipe_reader<T>(pipe: Option<T>) -> JoinHandle<Result<Vec<u8>>>
where
    T: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut data = Vec::new();
        if let Some(mut stream) = pipe {
            stream.read_to_end(&mut data)?;
        }
        Ok(data)
    })
}

fn join_pipe_reader(handle: JoinHandle<Result<Vec<u8>>>, stream_name: &str) -> Result<Vec<u8>> {
    match handle.join() {
        Ok(result) => result.with_context(|| format!("failed reading {stream_name} stream")),
        Err(_) => Err(anyhow!("{stream_name} reader thread panicked")),
    }
}

fn run_test_and_lint(
    execution: &FlowExecution<'_>,
    suites: &[TestSuiteConfig],
    phase: Phase,
) -> Result<bool> {
    let lint_ok = run_lint_checks(execution, suites, phase)?;

    let mut tests_ok = true;
    for suite in suites {
        let out = execution.run_test_suite(suite, phase)?;
        execution.log_event(
            if out.exit_code == 0 {
                "info"
            } else {
                "warning"
            },
            Some(phase),
            EventType::TestRun,
            format!(
                "Test run {} ({})",
                if out.exit_code == 0 {
                    "passed"
                } else {
                    "failed"
                },
                suite.name
            ),
            payload_from_json(json!({
                "suite": suite.name,
                "command": out.command,
                "exit_code": out.exit_code,
                "output": out.merged_output,
            })),
        )?;

        if out.exit_code != 0 {
            tests_ok = false;
        }
    }

    Ok(lint_ok && tests_ok)
}

#[cfg(test)]
mod tests;
