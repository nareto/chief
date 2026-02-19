use super::*;

impl<'a> FlowExecution<'a> {
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

    pub(in crate::flow) fn latest_single_prompt_failure_context(
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

    pub(in crate::flow) fn has_previous_single_prompt_attempt_since_last_retry_reset(
        &self,
    ) -> Result<bool> {
        let events = self.todo_events_since_last_retry_reset(1_000)?;
        Ok(events.into_iter().any(|event| {
            event.phase == Some(Phase::SinglePrompt) && event.event_type == EventType::AgentPrompt
        }))
    }

    pub(in crate::flow) fn todo_events_since_last_retry_reset(
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

    pub(in crate::flow) fn touched_files_since_last_retry_reset(&self) -> Result<Vec<String>> {
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
}
