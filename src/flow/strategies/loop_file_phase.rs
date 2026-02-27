use super::*;

#[derive(Debug, Clone)]
pub(in crate::flow) struct LoopFilePhaseStrategy {
    involved_suite_names: BTreeSet<String>,
    pub(in crate::flow) last_agent_run: Option<AgentRunWithGitChanges>,
    attempts: usize,
}

impl LoopFilePhaseStrategy {
    pub(in crate::flow) fn new() -> Self {
        Self {
            involved_suite_names: BTreeSet::new(),
            last_agent_run: None,
            attempts: 0,
        }
    }

    fn reload_configured_suites(execution: &FlowExecution<'_>) -> Result<Vec<TestSuiteConfig>> {
        let config_path = execution.project_dir.join("chief.yaml");
        let reloaded = ChiefYaml::load_or_default(&config_path).with_context(|| {
            format!(
                "failed to reload chief config from active worktree {}",
                config_path.display()
            )
        })?;
        Ok(reloaded.suites)
    }

    fn involved_suites(&self, execution: &FlowExecution<'_>) -> Result<Vec<TestSuiteConfig>> {
        if self.involved_suite_names.is_empty() {
            return Ok(Vec::new());
        }

        let suites = Self::reload_configured_suites(execution)?;
        Ok(suites
            .iter()
            .filter(|suite| self.involved_suite_names.contains(&suite.name))
            .cloned()
            .collect())
    }

    pub(super) fn run_post_green_for_involved_suites(
        &self,
        execution: &FlowExecution<'_>,
    ) -> Result<()> {
        let involved_suites = self.involved_suites(execution)?;
        if involved_suites.is_empty() {
            execution.log_event(
                "info",
                Some(Phase::PostGreen),
                EventType::PhaseChange,
                "loop_file: no involved suites; skipping post-green commands",
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
                "loop_file post-green checks failed for involved suites"
            ))
        }
    }
}

impl PhaseStrategy for LoopFilePhaseStrategy {
    fn phase(&self) -> Phase {
        Phase::LoopFile
    }

    fn attempt_fix(&mut self, execution: &mut FlowExecution<'_>) -> Result<AgentOutput> {
        let failure_context = execution.latest_single_prompt_failure_context()?;
        let has_previous_attempts = self.attempts > 0
            || execution.has_previous_single_prompt_attempt_since_last_retry_reset()?;
        let suites_for_prompt = Self::reload_configured_suites(execution)?;

        let prompt = execution.prompts.render_json(
            "singleprompt_loadfile.md",
            &json!({
                "work_item": execution.work_item(),
                "todo": execution.work_item_prompt_payload(),
                "file_contents": execution.work_item_details(),
                "suites": suites_for_prompt,
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

        let run = execution.run_agent_with_git_changes(Phase::LoopFile, prompt, Vec::new())?;
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

        let suites_for_checks = Self::reload_configured_suites(execution)?;
        for suite in &suites_for_checks {
            self.involved_suite_names.insert(suite.name.clone());
        }

        if suites_for_checks.is_empty() {
            execution.log_event(
                "info",
                Some(Phase::LoopFile),
                EventType::PhaseChange,
                "loop_file: no configured suites; skipping lint+test commands",
                BTreeMap::new(),
            )?;
        } else {
            for suite in &suites_for_checks {
                execution.ensure_suite_prepared(suite, Phase::LoopFile)?;
            }
            let all_pass = run_test_and_lint(execution, &suites_for_checks, Phase::LoopFile)?;
            if !all_pass {
                return Ok(LoopDecision::Retry);
            }
        }

        if run.output.exit_code != 0 {
            execution.log_event(
                "warning",
                Some(Phase::LoopFile),
                EventType::PhaseFailure,
                "loop_file agent step failed",
                payload_from_json(json!({
                    "exit_code": run.output.exit_code,
                    "command": run.output.command,
                })),
            )?;
            return Ok(LoopDecision::Retry);
        }

        if run.had_git_changes {
            let has_associated_test_suites = !suites_for_checks.is_empty();
            execution.log_event(
                "warning",
                Some(Phase::LoopFile),
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
            Some(Phase::LoopFile),
            EventType::PhaseChange,
            "loop_file iteration had no git file changes",
            BTreeMap::new(),
        )?;
        Ok(LoopDecision::Stable)
    }
}
