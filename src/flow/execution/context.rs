use super::*;

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

    pub(in crate::flow) fn todo_context_hash(&self) -> String {
        let fingerprint = TodoContextFingerprint {
            id: self.todo.id.trim().to_owned(),
            todo: self.todo.todo.trim().to_owned(),
            expectations: self.todo.expectations.trim().to_owned(),
            test_suites: normalized_suite_names(&self.todo.test_suites),
        };
        md5_hex_of_serializable(&fingerprint)
    }

    pub(in crate::flow) fn execution_context_hash(&self) -> String {
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

    pub(super) fn event_matches_current_context(
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
}
