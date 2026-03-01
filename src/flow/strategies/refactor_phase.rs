use super::*;

#[derive(Debug, Clone)]
pub(in crate::flow) struct RefactorPhaseStrategy {
    candidate_suites: Vec<TestSuiteConfig>,
    involved_suite_names: BTreeSet<String>,
    pub(in crate::flow) last_agent_run: Option<AgentRunWithGitChanges>,
    attempts: usize,
}

impl RefactorPhaseStrategy {
    pub(in crate::flow) fn new(candidate_suites: Vec<TestSuiteConfig>) -> Self {
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

    pub(super) fn run_post_green_for_involved_suites(
        &self,
        execution: &FlowExecution<'_>,
    ) -> Result<()> {
        let involved_suites = self.involved_suites();
        if involved_suites.is_empty() {
            execution.log_event(
                "info",
                Some(Phase::PostGreen),
                EventType::PhaseChange,
                "refactor: no involved suites; skipping post-green commands",
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
                "refactor post-green checks failed for involved suites"
            ))
        }
    }

    fn template_name_for_iteration(&self) -> &'static str {
        if self.attempts % 2 == 0 {
            "structural_cleanup.md"
        } else {
            "mechanical_cleanup.md"
        }
    }
}

impl PhaseStrategy for RefactorPhaseStrategy {
    fn phase(&self) -> Phase {
        Phase::Refactor
    }

    fn attempt_fix(&mut self, execution: &mut FlowExecution<'_>) -> Result<AgentOutput> {
        let template_name = self.template_name_for_iteration();
        let prompt = execution.prompts.render_json(
            template_name,
            &json!({
                "work_item": execution.work_item(),
                "todo": execution.work_item_prompt_payload(),
                "suites": self.candidate_suites,
                "iteration": self.attempts + 1,
                "run_id": execution.run_id,
                "prompt_name": template_name,
            }),
        )?;

        let run = execution.run_agent_with_git_changes(Phase::Refactor, prompt, Vec::new())?;
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
                head_commit_before: String::new(),
                head_commit_after: String::new(),
                head_commit_changed: true,
            });

        let suites_for_checks = self.candidate_suites.clone();
        for suite in &suites_for_checks {
            self.involved_suite_names.insert(suite.name.clone());
        }

        if suites_for_checks.is_empty() {
            execution.log_event(
                "info",
                Some(Phase::Refactor),
                EventType::PhaseChange,
                "refactor: no associated suites; skipping lint+test commands",
                BTreeMap::new(),
            )?;
        } else {
            let all_pass = run_test_and_lint(execution, &suites_for_checks, Phase::Refactor)?;
            if !all_pass {
                return Ok(LoopDecision::Retry);
            }
        }

        if run.output.exit_code != 0 {
            execution.log_event(
                "warning",
                Some(Phase::Refactor),
                EventType::PhaseFailure,
                "refactor agent step failed",
                payload_from_json(json!({
                    "exit_code": run.output.exit_code,
                    "command": run.output.command,
                })),
            )?;
            return Ok(LoopDecision::Retry);
        }

        if run.had_git_changes {
            let has_associated_test_suites = !execution.work_item_test_suites().is_empty();
            execution.log_event(
                "warning",
                Some(Phase::Refactor),
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
            Some(Phase::Refactor),
            EventType::PhaseChange,
            "refactor iteration had no git file changes",
            BTreeMap::new(),
        )?;
        Ok(LoopDecision::Stable)
    }
}
