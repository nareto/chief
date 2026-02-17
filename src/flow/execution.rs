use super::*;

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

    pub(super) fn todo_context_hash(&self) -> String {
        let fingerprint = TodoContextFingerprint {
            id: self.todo.id.trim().to_owned(),
            todo: self.todo.todo.trim().to_owned(),
            expectations: self.todo.expectations.trim().to_owned(),
            test_suites: normalized_suite_names(&self.todo.test_suites),
        };
        md5_hex_of_serializable(&fingerprint)
    }

    pub(super) fn execution_context_hash(&self) -> String {
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

    pub(super) fn latest_single_prompt_failure_context(
        &self,
    ) -> Result<SinglePromptFailureContext> {
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

    pub(super) fn has_previous_single_prompt_attempt_since_last_retry_reset(&self) -> Result<bool> {
        let events = self.todo_events_since_last_retry_reset(1_000)?;
        Ok(events.into_iter().any(|event| {
            event.phase == Some(Phase::SinglePrompt) && event.event_type == EventType::AgentPrompt
        }))
    }

    pub(super) fn todo_events_since_last_retry_reset(
        &self,
        limit: usize,
    ) -> Result<Vec<EventRecord>> {
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

    pub(super) fn touched_files_since_last_retry_reset(&self) -> Result<Vec<String>> {
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

    pub(super) fn run_agent_with_git_changes(
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

    pub(super) fn suite_command_timeout_seconds(&self, suite: &TestSuiteConfig) -> u64 {
        suite
            .command_timeout_seconds
            .unwrap_or(self.chief_config.suite_command_timeout_seconds)
            .max(1)
    }

    pub(super) fn ensure_suite_prepared(
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
