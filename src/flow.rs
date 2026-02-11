use crate::agent::{AgentCancelledError, AgentRequest, CodingAgent};
use crate::config::{ChiefConfig, TestSuiteConfig};
use crate::domain::{AgentOutput, EventRecord, EventType, LoopDecision, Phase, Todo};
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
                "suite command timed out after {} second(s) and was terminated.",
                timeout_seconds
            );
        } else {
            merged_output = format!(
                "suite command timed out after {} second(s) and was terminated.\n{}",
                timeout_seconds, merged_output
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

fn configure_process_group(process: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.process_group(0);
    }
}

fn terminate_process_tree(child: &mut std::process::Child) {
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

    pub fn log_event(
        &self,
        level: &str,
        phase: Option<Phase>,
        event_type: EventType,
        msg: impl Into<String>,
        payload: BTreeMap<String, Value>,
    ) -> Result<()> {
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

        let mut filtered = events
            .into_iter()
            .filter(|event| event.todo_id.as_deref() == Some(&self.todo.id))
            .filter(|event| allowed.contains(event.event_type.as_str()))
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
                    line.push_str("\n");
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

        for event in events {
            if event.phase != Some(Phase::SinglePrompt) {
                continue;
            }

            // Keep failure context focused on the latest completed iteration only.
            if event.event_type == EventType::AgentPrompt {
                break;
            }

            let has_nonzero_exit = event_exit_code(&event).unwrap_or(0) != 0;
            let is_warning_or_error = event.level == "warning" || event.level == "error";

            match event.event_type {
                EventType::Lint if has_nonzero_exit => lint_failures.push(
                    single_prompt_failure_item_from_event(&event, max_output_lines),
                ),
                EventType::TestRun if has_nonzero_exit => test_failures.push(
                    single_prompt_failure_item_from_event(&event, max_output_lines),
                ),
                _ => {}
            }

            if matches!(event.event_type, EventType::Lint | EventType::TestRun) {
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
        self.run_suite_command(&cmd, &cwd, &suite.env, timeout_seconds)
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

        self.log_event(
            "info",
            Some(phase),
            EventType::AgentPrompt,
            format!("Agent prompt ({})", phase.as_str()),
            payload_from_json(json!({ "prompt": prompt })),
        )?;

        let before_files = self
            .git
            .changed_files(&self.project_dir)
            .unwrap_or_default();

        let out = self.agent.run(AgentRequest {
            prompt,
            cwd: self.project_dir.clone(),
            timeout_seconds: Some(self.chief_config.agent_timeout_seconds),
            disallowed_paths,
            cancel_signal: Some(self.cancel_signal.clone()),
        })?;

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
                "touched_files": touched_files.clone(),
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
        Self {
            red_loop: ConvergenceLoopPolicy::default(),
            green_loop: UntilPassLoopPolicy::default(),
            post_green_loop: UntilPassLoopPolicy::default(),
        }
    }
}

impl TodoFlow for TddFlow {
    fn name(&self) -> &'static str {
        "tdd"
    }

    fn run_todo(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome> {
        let suites = execution.selected_suites();

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
        Self {
            loop_policy: ConvergenceLoopPolicy::default(),
        }
    }
}

impl TodoFlow for SinglePromptFlow {
    fn name(&self) -> &'static str {
        "single_prompt"
    }

    fn run_todo(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome> {
        let candidate_suites = execution.selected_suites();

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

pub fn build_flow(flow_kind: FlowKind) -> Box<dyn TodoFlow> {
    match flow_kind {
        FlowKind::SinglePrompt => Box::new(SinglePromptFlow::default()),
        FlowKind::Tdd => Box::new(TddFlow::default()),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitState {
    Completed,
    TimedOut,
    Cancelled,
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

fn payload_from_json(value: Value) -> BTreeMap<String, Value> {
    match value {
        Value::Object(map) => map.into_iter().collect(),
        _ => BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentRunWithGitChanges, FlowExecution, FlowKind, PhaseStrategy,
        SINGLE_PROMPT_CHANGED_FILES_RETRY_MESSAGE,
        SINGLE_PROMPT_RETRY_HAS_ASSOCIATED_TEST_SUITES_PAYLOAD_KEY,
        SINGLE_PROMPT_RETRY_REASON_CONVERGENCE_CHANGED_FILES,
        SINGLE_PROMPT_RETRY_REASON_PAYLOAD_KEY, SinglePromptPhaseStrategy, TestSuiteConfig,
        build_flow,
    };
    use crate::agent::{AgentRequest, CodingAgent};
    use crate::config::ChiefConfig;
    use crate::domain::{AgentOutput, EventType, LoopDecision, Phase, Todo, TodoStatus};
    use crate::git::GitOps;
    use crate::prompt::PromptStore;
    use crate::storage::ProjectStore;
    use anyhow::{Result, anyhow};
    use serde_json::Value;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    #[test]
    fn parses_known_flow_kinds() {
        assert_eq!(FlowKind::from_str("tdd").unwrap(), FlowKind::Tdd);
        assert_eq!(
            FlowKind::from_str("single_prompt").unwrap(),
            FlowKind::SinglePrompt
        );
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
        let tdd = build_flow(FlowKind::Tdd);
        let single_prompt = build_flow(FlowKind::SinglePrompt);

        assert_eq!(tdd.name(), "tdd");
        assert_eq!(single_prompt.name(), "single_prompt");
    }

    fn temp_project_dir() -> PathBuf {
        std::env::temp_dir().join(format!("chief-flow-test-{}", Uuid::new_v4()))
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
            post_green_command: None,
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
        let mut chief_config = ChiefConfig::default();
        chief_config.agent_log_max_output_lines = 2;

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
        let mut chief_config = ChiefConfig::default();
        chief_config.agent_log_max_output_lines = 2;

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
            Value::String(
                "codex exec --json --dangerously-bypass-approvals-and-sandbox -".to_owned(),
            ),
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
        let mut chief_config = ChiefConfig::default();
        chief_config.agent_log_max_output_lines = 2;

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
    fn single_prompt_changed_files_retry_without_associated_suites_is_not_failure_context() {
        let project_dir = temp_project_dir();
        let store = ProjectStore::new(&project_dir);
        store.init().expect("store init should succeed");

        let todo = Todo {
            id: "todo-1".to_owned(),
            todo: "changed files should not be failure context without associated suites"
                .to_owned(),
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
        store
            .record_event(&crate::domain::EventRecord {
                id: None,
                run_id: "run-resumed".to_owned(),
                job_id: Some("job-resumed".to_owned()),
                todo_id: Some("todo-1".to_owned()),
                timestamp: chrono::Utc::now(),
                level: "info".to_owned(),
                phase: Some(Phase::SinglePrompt),
                msg: "Iteration git change detection".to_owned(),
                event_type: EventType::Diff,
                payload: newer_payload,
            })
            .expect("newer diff event should log");

        let latest_payload = diff_payload(&["backend/app/main.py", "backend/tests/test_api.py"]);
        store
            .record_event(&crate::domain::EventRecord {
                id: None,
                run_id: "run-current".to_owned(),
                job_id: Some("job-current".to_owned()),
                todo_id: Some("todo-1".to_owned()),
                timestamp: chrono::Utc::now(),
                level: "info".to_owned(),
                phase: Some(Phase::SinglePrompt),
                msg: "Iteration git change detection".to_owned(),
                event_type: EventType::Diff,
                payload: latest_payload,
            })
            .expect("latest diff event should log");

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

        assert!(
            !execution
                .has_previous_single_prompt_attempt_since_last_retry_reset()
                .expect("query should succeed"),
            "without history this should be treated as first attempt"
        );

        store
            .record_event(&crate::domain::EventRecord {
                id: None,
                run_id: "run-old".to_owned(),
                job_id: Some("job-old".to_owned()),
                todo_id: Some("todo-1".to_owned()),
                timestamp: chrono::Utc::now(),
                level: "info".to_owned(),
                phase: Some(Phase::SinglePrompt),
                msg: "Agent prompt (single_prompt)".to_owned(),
                event_type: EventType::AgentPrompt,
                payload: BTreeMap::new(),
            })
            .expect("old prompt event should log");

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

        let flow = build_flow(FlowKind::SinglePrompt);
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
}
