use super::*;

#[derive(Debug, Clone)]
pub(super) struct RedPhaseStrategy {
    suites: Vec<TestSuiteConfig>,
}

impl RedPhaseStrategy {
    pub(super) fn new(suites: Vec<TestSuiteConfig>) -> Self {
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
                "work_item": execution.work_item(),
                "todo": execution.work_item_prompt_payload(),
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
pub(super) struct GreenPhaseStrategy {
    suites: Vec<TestSuiteConfig>,
}

impl GreenPhaseStrategy {
    pub(super) fn new(suites: Vec<TestSuiteConfig>) -> Self {
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
                "work_item": execution.work_item(),
                "todo": execution.work_item_prompt_payload(),
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
pub(super) struct PostGreenPhaseStrategy {
    suites: Vec<TestSuiteConfig>,
}

impl PostGreenPhaseStrategy {
    pub(super) fn new(suites: Vec<TestSuiteConfig>) -> Self {
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
                "work_item": execution.work_item(),
                "todo": execution.work_item_prompt_payload(),
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
