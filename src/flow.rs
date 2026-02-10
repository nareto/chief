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
use std::time::Duration;

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
    lint_tail_output: String,
    test_tail_output: String,
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
) -> Result<AgentOutput> {
    let mut process = Command::new("sh");
    process.arg("-lc").arg(command);
    process.current_dir(cwd);
    process.envs(env.iter());
    process.stdout(Stdio::piped());
    process.stderr(Stdio::piped());
    let mut child = process
        .spawn()
        .with_context(|| format!("failed to run command: {command}"))?;
    let output = wait_for_command_with_cancel(&mut child, cancel_signal)?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    Ok(AgentOutput {
        exit_code: output.status.code().unwrap_or(1),
        command: command.to_owned(),
        merged_output: format!("{stdout}\n{stderr}").trim().to_owned(),
        stdout,
        stderr,
    })
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
}

impl<'a> FlowExecution<'a> {
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
        let events = self.store.query_events(EventQuery {
            limit: 200,
            event_type: None,
            phase: Some(Phase::SinglePrompt),
            level: None,
            contains_text: None,
        })?;

        let mut lint_failure: Option<EventRecord> = None;
        let mut test_failure: Option<EventRecord> = None;

        for event in events {
            if event.run_id != self.run_id {
                continue;
            }
            if event.todo_id.as_deref() != Some(&self.todo.id) {
                continue;
            }
            if event_exit_code(&event).unwrap_or(0) == 0 {
                continue;
            }

            match event.event_type {
                EventType::Lint if lint_failure.is_none() => lint_failure = Some(event),
                EventType::TestRun if test_failure.is_none() => test_failure = Some(event),
                _ => {}
            }

            if lint_failure.is_some() && test_failure.is_some() {
                break;
            }
        }

        let max_output_lines = self.chief_config.agent_log_max_output_lines;
        let lint_tail_output = lint_failure
            .as_ref()
            .and_then(|event| event.payload.get("output").and_then(Value::as_str))
            .map(|output| tail_output_lines(output, max_output_lines))
            .unwrap_or_default();
        let test_tail_output = test_failure
            .as_ref()
            .and_then(|event| event.payload.get("output").and_then(Value::as_str))
            .map(|output| tail_output_lines(output, max_output_lines))
            .unwrap_or_default();

        Ok(SinglePromptFailureContext {
            failed_lint: lint_failure.is_some(),
            failed_test: test_failure.is_some(),
            lint_tail_output,
            test_tail_output,
        })
    }

    pub fn run_suite_command(
        &self,
        command: &str,
        cwd: &Path,
        env: &BTreeMap<String, String>,
    ) -> Result<AgentOutput> {
        self.ensure_not_cancelled()?;
        execute_suite_command(command, cwd, env, &self.cancel_signal)
    }

    pub fn run_test_suite(&self, suite: &TestSuiteConfig) -> Result<AgentOutput> {
        let cmd = suite_command_for_kind(suite, SuiteCommandKind::Test, None)
            .unwrap_or_else(|| suite.test_command.clone());
        let cwd = suite_command_cwd(&self.project_dir, suite);
        self.run_suite_command(&cmd, &cwd, &suite.env)
    }

    pub fn run_lint_suite(&self, suite: &TestSuiteConfig) -> Result<Option<AgentOutput>> {
        let Some(cmd) = suite_command_for_kind(suite, SuiteCommandKind::Lint, None) else {
            return Ok(None);
        };
        let cwd = suite_command_cwd(&self.project_dir, suite);
        let out = self.run_suite_command(&cmd, &cwd, &suite.env)?;
        Ok(Some(out))
    }

    pub fn run_post_green_suite(&self, suite: &TestSuiteConfig) -> Result<Option<AgentOutput>> {
        let Some(command) = suite_command_for_kind(suite, SuiteCommandKind::PostGreen, None) else {
            return Ok(None);
        };
        let cwd = suite_command_cwd(&self.project_dir, suite);
        let out = self.run_suite_command(&command, &cwd, &suite.env)?;
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
}

fn event_exit_code(event: &EventRecord) -> Option<i64> {
    let value = event.payload.get("exit_code")?;
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
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
        let candidate_suites = if execution.todo.test_suites.is_empty() {
            execution.all_suites.to_vec()
        } else {
            execution.selected_suites()
        };

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
            if let Some(out) = execution.run_post_green_suite(suite)? {
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

        let prompt = execution.prompts.render_json(
            "singleprompt.md",
            &json!({
                "todo": execution.todo,
                "suites": self.candidate_suites,
                "iteration": self.attempts + 1,
                "run_id": execution.run_id,
                "first_attempt": self.attempts == 0,
                "failed_lint": failure_context.failed_lint,
                "failed_test": failure_context.failed_test,
                "lint_tail_output": failure_context.lint_tail_output,
                "test_tail_output": failure_context.test_tail_output,
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

        let touched_suites = suites_touched_by_files(&self.candidate_suites, &run.touched_files);
        for suite in &touched_suites {
            self.involved_suite_names.insert(suite.name.clone());
        }

        let suites_for_checks = if !touched_suites.is_empty() {
            touched_suites
        } else {
            self.involved_suites()
        };

        if suites_for_checks.is_empty() {
            execution.log_event(
                "info",
                Some(Phase::SinglePrompt),
                EventType::PhaseChange,
                "single_prompt: no touched/involved suites; skipping lint+test commands",
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
            execution.log_event(
                "warning",
                Some(Phase::SinglePrompt),
                EventType::PhaseFailure,
                "single_prompt iteration changed files; waiting for two consecutive no-change iterations",
                payload_from_json(json!({ "touched_files": run.touched_files })),
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
            if let Some(out) = execution.run_post_green_suite(suite)? {
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
        let Some(out) = execution.run_lint_suite(suite)? else {
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

fn suites_touched_by_files(
    suites: &[TestSuiteConfig],
    touched_files: &[String],
) -> Vec<TestSuiteConfig> {
    if touched_files.is_empty() {
        return Vec::new();
    }

    let normalized_files = touched_files
        .iter()
        .map(|path| normalize_repo_relative_path(path))
        .collect::<Vec<_>>();

    suites
        .iter()
        .filter(|suite| {
            let root = normalize_repo_relative_path(&suite.test_root);
            if root.is_empty() {
                return true;
            }
            normalized_files.iter().any(|file| {
                file == &root
                    || file
                        .strip_prefix(&root)
                        .map(|suffix| suffix.starts_with('/'))
                        .unwrap_or(false)
            })
        })
        .cloned()
        .collect()
}

fn normalize_repo_relative_path(path: &str) -> String {
    path.trim()
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_matches('/')
        .to_owned()
}

fn wait_for_command_with_cancel(
    child: &mut std::process::Child,
    cancel_signal: &Arc<AtomicBool>,
) -> Result<std::process::Output> {
    let stdout_reader = spawn_pipe_reader(child.stdout.take());
    let stderr_reader = spawn_pipe_reader(child.stderr.take());
    let mut cancelled = false;

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }

        if cancel_signal.load(Ordering::SeqCst) {
            cancelled = true;
            let _ = child.kill();
            break child.wait()?;
        }

        std::thread::sleep(Duration::from_millis(50));
    };

    let stdout = join_pipe_reader(stdout_reader, "stdout")?;
    let stderr = join_pipe_reader(stderr_reader, "stderr")?;

    if cancelled {
        return Err(anyhow!(AgentCancelledError));
    }

    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
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
    let mut all_ok = run_lint_checks(execution, suites, phase)?;

    for suite in suites {
        let out = execution.run_test_suite(suite)?;
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
            all_ok = false;
        }
    }

    Ok(all_ok)
}

fn payload_from_json(value: Value) -> BTreeMap<String, Value> {
    match value {
        Value::Object(map) => map.into_iter().collect(),
        _ => BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{FlowExecution, FlowKind, build_flow};
    use crate::agent::{AgentRequest, CodingAgent};
    use crate::config::ChiefConfig;
    use crate::domain::{AgentOutput, EventType, Phase, Todo, TodoStatus};
    use crate::git::GitOps;
    use crate::prompt::PromptStore;
    use crate::storage::ProjectStore;
    use anyhow::{Result, anyhow};
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
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
}
