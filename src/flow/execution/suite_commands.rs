use super::*;

impl<'a> FlowExecution<'a> {
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
        self.run_optional_suite_command(suite, phase, SuiteCommandKind::Lint)
    }

    pub fn run_post_green_suite(
        &self,
        suite: &TestSuiteConfig,
        phase: Phase,
    ) -> Result<Option<AgentOutput>> {
        self.run_optional_suite_command(suite, phase, SuiteCommandKind::PostGreen)
    }

    fn run_optional_suite_command(
        &self,
        suite: &TestSuiteConfig,
        phase: Phase,
        kind: SuiteCommandKind,
    ) -> Result<Option<AgentOutput>> {
        self.ensure_suite_prepared(suite, phase)?;
        let Some(command) = suite_command_for_kind(suite, kind, None) else {
            return Ok(None);
        };
        let cwd = suite_command_cwd(&self.project_dir, suite);
        let timeout_seconds = self.suite_command_timeout_seconds(suite);
        self.log_suite_command_started(phase, suite, kind, &command, &cwd, timeout_seconds)?;
        let out = self.run_suite_command(&command, &cwd, &suite.env, timeout_seconds)?;
        Ok(Some(out))
    }

    pub(in crate::flow) fn suite_command_timeout_seconds(&self, suite: &TestSuiteConfig) -> u64 {
        suite
            .command_timeout_seconds
            .unwrap_or(self.chief_config.suite_command_timeout_seconds)
            .max(1)
    }

    pub(in crate::flow) fn ensure_suite_prepared(
        &self,
        suite: &TestSuiteConfig,
        phase: Phase,
    ) -> Result<()> {
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
