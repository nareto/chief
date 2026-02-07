use crate::agent::{AgentRequest, CodingAgent};
use crate::config::{ChiefConfig, TestSuiteConfig};
use crate::domain::{AgentOutput, EventRecord, EventType, LoopDecision, Phase, Todo};
use crate::git::GitOps;
use crate::prompt::PromptStore;
use crate::storage::{EventQuery, ProjectStore};
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct TodoOutcome {
    pub todo_id: String,
    pub commit_hash: Option<String>,
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
            .take(limit)
            .collect::<Vec<_>>();

        filtered.reverse();

        if filtered.is_empty() {
            return Ok("No previous attempts recorded.".to_owned());
        }

        let mut lines = Vec::with_capacity(filtered.len());
        for (idx, event) in filtered.iter().enumerate() {
            let mut line = format!("[{}] {}: {}", idx + 1, event.event_type.as_str(), event.msg);
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

    pub fn run_suite_command(
        &self,
        command: &str,
        cwd: &Path,
        env: &BTreeMap<String, String>,
    ) -> Result<AgentOutput> {
        let mut process = Command::new("sh");
        process.arg("-lc").arg(command);
        process.current_dir(cwd);
        process.envs(env.iter());
        let output = process
            .output()
            .with_context(|| format!("failed to run command: {command}"))?;
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

    pub fn run_test_suite(&self, suite: &TestSuiteConfig) -> Result<AgentOutput> {
        let target = suite
            .default_target
            .clone()
            .unwrap_or_else(|| ".".to_owned());
        let cmd = suite.test_command.replace("{target}", &target);
        let cwd = self.project_dir.join(&suite.test_root);
        self.run_suite_command(&cmd, &cwd, &suite.env)
    }

    pub fn run_lint_suite(&self, suite: &TestSuiteConfig) -> Result<Option<AgentOutput>> {
        let Some(lint_command) = &suite.lint_command else {
            return Ok(None);
        };
        let target = suite
            .default_target
            .clone()
            .unwrap_or_else(|| ".".to_owned());
        let cmd = lint_command.replace("{target}", &target);
        let cwd = self.project_dir.join(&suite.test_root);
        let out = self.run_suite_command(&cmd, &cwd, &suite.env)?;
        Ok(Some(out))
    }

    pub fn run_post_green_suite(&self, suite: &TestSuiteConfig) -> Result<Option<AgentOutput>> {
        let Some(command) = &suite.post_green_command else {
            return Ok(None);
        };
        let cwd = self.project_dir.join(&suite.test_root);
        let out = self.run_suite_command(command, &cwd, &suite.env)?;
        Ok(Some(out))
    }

    pub fn run_agent(
        &self,
        phase: Phase,
        prompt: String,
        disallowed_paths: Vec<String>,
    ) -> Result<AgentOutput> {
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
        let mut stable = 0usize;
        if strategy.check_goal_before_loop() {
            let pre = strategy.check_goal(execution, -1, &AgentOutput::success("precheck", ""))?;
            if matches!(pre, LoopDecision::Success) {
                return Ok(());
            }
        }

        for iteration in 0..self.max_loops {
            execution.log_event(
                "info",
                Some(strategy.phase()),
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
                LoopDecision::Success => return Ok(()),
                LoopDecision::Stable => {
                    stable += 1;
                    if stable >= self.required_stable_iterations {
                        return Ok(());
                    }
                }
                LoopDecision::Retry => stable = 0,
            }
        }

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
        if strategy.check_goal_before_loop() {
            let pre = strategy.check_goal(execution, -1, &AgentOutput::success("precheck", ""))?;
            if matches!(pre, LoopDecision::Success) {
                return Ok(());
            }
        }

        for iteration in 0..self.max_loops {
            execution.log_event(
                "info",
                Some(strategy.phase()),
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
                return Ok(());
            }
        }

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
        }

        let mut green = GreenPhaseStrategy::new(suites.clone());
        self.green_loop.run(&mut green, execution)?;

        let mut post_green = PostGreenPhaseStrategy::new(suites);
        self.post_green_loop.run(&mut post_green, execution)?;

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
    max_loops: usize,
}

impl Default for SinglePromptFlow {
    fn default() -> Self {
        Self { max_loops: 6 }
    }
}

impl TodoFlow for SinglePromptFlow {
    fn name(&self) -> &'static str {
        "single_prompt"
    }

    fn run_todo(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome> {
        let suites = execution.selected_suites();
        for iteration in 0..self.max_loops {
            let previous_steps_log = execution.previous_steps_log(
                Phase::Green,
                &[
                    EventType::TestRun,
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
                    "suites": suites,
                    "previous_steps_log": previous_steps_log,
                    "single_prompt_mode": true,
                }),
            )?;

            let out = execution.run_agent(Phase::Green, prompt, Vec::new())?;
            if out.exit_code != 0 {
                continue;
            }

            let all_pass = run_test_and_lint(execution, &suites, Phase::Green)?;
            if all_pass {
                let commit_hash = execution
                    .git
                    .commit_and_tag(
                        &execution.project_dir,
                        &format!("chief(single_prompt): {}", execution.todo.todo),
                    )
                    .context("failed to commit todo")?;
                return Ok(TodoOutcome {
                    todo_id: execution.todo.id.clone(),
                    commit_hash: Some(commit_hash),
                });
            }

            execution.log_event(
                "warning",
                Some(Phase::Green),
                EventType::PhaseFailure,
                format!("single_prompt iteration {} did not pass", iteration + 1),
                BTreeMap::new(),
            )?;
        }

        Err(anyhow!("single_prompt flow exhausted retries"))
    }
}

pub fn build_flow(flow_name: &str) -> Box<dyn TodoFlow> {
    match flow_name {
        "single_prompt" => Box::new(SinglePromptFlow::default()),
        _ => Box::new(TddFlow::default()),
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
