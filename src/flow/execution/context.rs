use super::*;

impl<'a> FlowExecution<'a> {
    pub fn work_item(&self) -> WorkItem {
        WorkItem::from_todo(self.todo.clone())
    }

    pub fn work_item_id(&self) -> &str {
        self.todo.id.as_str()
    }

    pub fn work_item_title(&self) -> &str {
        self.todo.todo.as_str()
    }

    pub fn work_item_details(&self) -> &str {
        self.todo.expectations.as_str()
    }

    pub fn work_item_test_suites(&self) -> &[String] {
        &self.todo.test_suites
    }

    pub fn work_item_prompt_payload(&self) -> Value {
        self.work_item().to_legacy_todo_prompt_json()
    }

    pub(in crate::flow) fn reload_suite_check_context(
        &self,
        requested_suites: &[TestSuiteConfig],
    ) -> Result<(ChiefConfig, Vec<TestSuiteConfig>)> {
        let config_path = self.project_dir.join("chief.yaml");
        let reloaded = ChiefYaml::load_or_default(&config_path).with_context(|| {
            format!(
                "failed to reload chief config from active worktree {}",
                config_path.display()
            )
        })?;

        if requested_suites.is_empty() {
            return Ok((reloaded.chief, Vec::new()));
        }

        let mut reloaded_by_name = reloaded
            .suites
            .into_iter()
            .map(|suite| (suite.name.clone(), suite))
            .collect::<BTreeMap<_, _>>();

        let suites = requested_suites
            .iter()
            .map(|suite| {
                reloaded_by_name
                    .remove(&suite.name)
                    .unwrap_or_else(|| suite.clone())
            })
            .collect::<Vec<_>>();

        Ok((reloaded.chief, suites))
    }

    pub fn selected_suites(&self) -> Vec<TestSuiteConfig> {
        if self.work_item_test_suites().is_empty() {
            return Vec::new();
        }
        let names = self.work_item_test_suites().iter().collect::<HashSet<_>>();
        self.all_suites
            .iter()
            .filter(|suite| names.contains(&suite.name))
            .cloned()
            .collect()
    }

    pub(in crate::flow) fn work_item_context_hash(&self) -> String {
        let fingerprint = WorkItemContextFingerprint {
            id: self.work_item_id().trim().to_owned(),
            title: self.work_item_title().trim().to_owned(),
            details: self.work_item_details().trim().to_owned(),
            test_suites: normalized_suite_names(self.work_item_test_suites()),
        };
        md5_hex_of_serializable(&fingerprint)
    }

    pub(in crate::flow) fn todo_context_hash(&self) -> String {
        self.work_item_context_hash()
    }

    pub(in crate::flow) fn execution_context_hash(&self) -> String {
        let work_item_suite_names = normalized_suite_names(self.work_item_test_suites());
        let configured_work_item_suites = if work_item_suite_names.is_empty() {
            Vec::new()
        } else {
            let expected = work_item_suite_names.iter().collect::<HashSet<_>>();
            let mut suites = self
                .all_suites
                .iter()
                .filter(|suite| expected.contains(&suite.name))
                .cloned()
                .collect::<Vec<_>>();
            suites.sort_by(|left, right| left.name.cmp(&right.name));
            suites
        };
        let suites = configured_work_item_suites
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
            work_item_test_suites: work_item_suite_names,
            suites,
        };
        md5_hex_of_serializable(&fingerprint)
    }

    pub(super) fn event_matches_current_context(
        event: &EventRecord,
        expected_todo_hash: &str,
        expected_exec_hash: &str,
    ) -> bool {
        let work_item_hash = event
            .payload
            .get(WORK_ITEM_CONTEXT_HASH_PAYLOAD_KEY)
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !work_item_hash.is_empty() {
            if work_item_hash != expected_todo_hash {
                return false;
            }
        } else {
            let todo_hash = event
                .payload
                .get(TODO_CONTEXT_HASH_PAYLOAD_KEY)
                .and_then(Value::as_str)
                .unwrap_or_default();
            if todo_hash != expected_todo_hash {
                return false;
            }
        }

        let exec_hash = event
            .payload
            .get(EXECUTION_CONTEXT_HASH_PAYLOAD_KEY)
            .and_then(Value::as_str)
            .unwrap_or_default();
        exec_hash == expected_exec_hash
    }
}
