use super::*;

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
pub(super) struct SinglePromptPhaseStrategy {
    candidate_suites: Vec<TestSuiteConfig>,
    involved_suite_names: BTreeSet<String>,
    pub(super) last_agent_run: Option<AgentRunWithGitChanges>,
    attempts: usize,
}

impl SinglePromptPhaseStrategy {
    pub(super) fn new(candidate_suites: Vec<TestSuiteConfig>) -> Self {
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
