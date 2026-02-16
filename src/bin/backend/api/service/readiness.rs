use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadinessCommandKind {
    TestInit,
    TestSetup,
    Lint,
    Test,
}

impl ReadinessCommandKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::TestInit => "test_init",
            Self::TestSetup => "test_setup",
            Self::Lint => "lint",
            Self::Test => "test",
        }
    }
}

#[derive(Debug, Clone)]
struct ReadinessCommandPlan {
    suite_name: String,
    kind: ReadinessCommandKind,
    command_template: String,
    cleanup_command: Option<String>,
    uses_target_placeholder: bool,
    target_candidates: Vec<String>,
    cwd: PathBuf,
    cwd_display: String,
    env: BTreeMap<String, String>,
    timeout_seconds: u64,
}

#[derive(Debug, Clone)]
struct ReadinessCommandResult {
    suite_name: String,
    kind: ReadinessCommandKind,
    command: String,
    cwd: String,
    target: Option<String>,
    exit_code: i32,
    blocking_failure: bool,
    output_tail: String,
}

const READINESS_UNCHECKED_SUMMARY: &str = "Pre-run checks have not run yet.";
const READINESS_EVENT_SOURCE: &str = "pre_run_checks";
const READINESS_STREAM_MAX_BUFFER_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
struct ReadinessLogContext {
    run_id: String,
    store: ProjectStore,
}

#[derive(Debug, Clone)]
struct ReadinessStreamContext {
    project: String,
    sender: broadcast::Sender<ReadinessStreamMessage>,
    snapshots: Arc<std::sync::Mutex<HashMap<String, String>>>,
}

impl ReadinessStreamContext {
    fn reset(&self) {
        if let Ok(mut snapshots) = self.snapshots.lock() {
            snapshots.insert(self.project.clone(), String::new());
        }
        let _ = self.sender.send(ReadinessStreamMessage::Reset {
            project: self.project.clone(),
        });
    }

    fn push_text(&self, text: impl AsRef<str>) {
        let text = text.as_ref();
        if text.is_empty() {
            return;
        }

        if let Ok(mut snapshots) = self.snapshots.lock() {
            let entry = snapshots.entry(self.project.clone()).or_default();
            entry.push_str(text);
            trim_leading_bytes(entry, READINESS_STREAM_MAX_BUFFER_BYTES);
        }

        let _ = self.sender.send(ReadinessStreamMessage::Chunk {
            project: self.project.clone(),
            text: text.to_owned(),
        });
    }
}

impl ApiService {
    pub async fn stop_readiness_check(&self, project: &str) -> Result<MessageResponse, ApiError> {
        let context = self.project_context(project).await?;
        let signal = {
            let signals = self.readiness_cancel_signals.lock().await;
            signals.get(project).cloned()
        };

        let Some(signal) = signal else {
            return Err(ApiError::unprocessable(
                "no pre-run checks are currently running for this project",
            ));
        };

        signal.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = context.store.set_readiness_result(
            ReadinessStatus::NotReady,
            "Not ready: pre-run checks cancelled by user.",
            &json!({ "cancelled_by_user": true }),
        );

        Ok(MessageResponse {
            message: format!("requested pre-run checks stop for project {project}"),
        })
    }

    #[allow(dead_code)]
    pub async fn run_readiness_check(
        &self,
        project: &str,
        force: bool,
    ) -> Result<ReadinessCheckResult, ApiError> {
        let mut context = self.project_context(project).await?;
        if !context.config_path.is_file() {
            return Err(ApiError::chief_yaml_missing(
                context.config_path.display().to_string(),
            ));
        }
        context.refresh().map_err(ApiError::internal)?;
        let chief_yaml_hash =
            chief_yaml_content_hash(&context.config_path).map_err(ApiError::internal)?;
        let ran = self
            .ensure_project_readiness(project, &context, &chief_yaml_hash, force)
            .await?;
        let readiness = context
            .store
            .get_readiness_state()
            .map_err(ApiError::internal)?;

        Ok(ReadinessCheckResult {
            readiness: project_readiness_response(readiness),
            ran,
        })
    }

    async fn run_and_persist_readiness_check(
        &self,
        project: &str,
        context: &chief::service::ProjectContext,
        chief_yaml_hash: &str,
        suite_cache_inputs_hash: &str,
    ) -> Result<(), ApiError> {
        let readiness_worktree =
            create_temp_worktree(context, "pre-run-checks").map_err(ApiError::internal)?;
        let command_plans = build_readiness_command_plans(context, &readiness_worktree.path);
        let suite_count = context.chief_yaml.suites.len();
        let command_count = command_plans.len();
        let checking_summary = if suite_count == 0 {
            "Running pre-run checks (no suites configured)".to_owned()
        } else {
            format!("Running pre-run checks across {suite_count} suite(s)")
        };

        context
            .store
            .set_readiness_checking(&checking_summary)
            .map_err(ApiError::internal)?;

        let readiness_run_id = format!("pre-run-checks-{}", Uuid::new_v4());
        context
            .store
            .start_run(&readiness_run_id)
            .map_err(ApiError::internal)?;
        let readiness_log = ReadinessLogContext {
            run_id: readiness_run_id.clone(),
            store: context.store.clone(),
        };
        let readiness_stream = ReadinessStreamContext {
            project: project.to_owned(),
            sender: self.readiness_stream_sender.clone(),
            snapshots: self.readiness_stream_snapshots.clone(),
        };
        readiness_stream.reset();

        let started_message = format!("Started pre-run checks for {suite_count} suite(s).\n");
        readiness_stream.push_text(&started_message);

        let mut started_payload = readiness_payload("pre_run_checks_started");
        started_payload.insert(
            "suite_count".to_owned(),
            serde_json::Value::from(suite_count as u64),
        );
        started_payload.insert(
            "command_count".to_owned(),
            serde_json::Value::from(command_count as u64),
        );
        record_readiness_event(
            Some(&readiness_log),
            "info",
            started_message,
            started_payload,
        );

        info!(
            project,
            suites = suite_count,
            commands = command_count,
            "running project pre-run checks before start"
        );

        let finish_readiness_run = |status: RunExitStatus| {
            if let Err(err) = context.store.finish_run(&readiness_run_id, status) {
                warn!(
                    project,
                    run_id = %readiness_run_id,
                    error = %err,
                    "failed to finish pre-run checks event run"
                );
            }
        };

        let cancel_signal = match self.register_readiness_cancel_signal(project).await {
            Ok(signal) => signal,
            Err(err) => {
                if let Err(cleanup_err) = cleanup_temp_worktree(&context.git, &readiness_worktree) {
                    warn!(
                        project,
                        branch = %readiness_worktree.branch,
                        worktree = %readiness_worktree.path.display(),
                        error = %cleanup_err,
                        "failed to cleanup pre-run checks worktree after cancel registration failure"
                    );
                }
                finish_readiness_run(RunExitStatus::Failure);
                return Err(ApiError::internal(err));
            }
        };
        let readiness_task = {
            let service = self.clone();
            let project_name = project.to_owned();
            let store = context.store.clone();
            let readiness_run_id_for_task = readiness_run_id.clone();
            let chief_yaml_hash_for_task = chief_yaml_hash.to_owned();
            let suite_cache_inputs_hash_for_task = suite_cache_inputs_hash.to_owned();
            let readiness_git = context.git.clone();
            let readiness_worktree_for_task = readiness_worktree.clone();
            let project_dir_for_task = context.project_dir.clone();
            let project_name_for_task = context.name.clone();
            let suites_for_task = context.chief_yaml.suites.clone();
            tokio::spawn(async move {
                service
                    .execute_and_persist_readiness_check(
                        project_name,
                        store,
                        readiness_run_id_for_task,
                        chief_yaml_hash_for_task,
                        suite_cache_inputs_hash_for_task,
                        readiness_log,
                        readiness_stream,
                        command_plans,
                        suite_count,
                        command_count,
                        cancel_signal,
                        readiness_git,
                        readiness_worktree_for_task,
                        project_dir_for_task,
                        project_name_for_task,
                        suites_for_task,
                    )
                    .await
            })
        };

        match readiness_task.await {
            Ok(result) => result,
            Err(err) => {
                if let Err(cleanup_err) = cleanup_temp_worktree(&context.git, &readiness_worktree) {
                    warn!(
                        project,
                        branch = %readiness_worktree.branch,
                        worktree = %readiness_worktree.path.display(),
                        error = %cleanup_err,
                        "failed to cleanup pre-run checks worktree after task join failure"
                    );
                }
                let summary = format!("Not ready: pre-run checks task failed ({err})");
                let details = json!({
                    "error": err.to_string(),
                    "commands_total": command_count,
                    "suite_count": suite_count,
                    "chief_yaml_hash": chief_yaml_hash,
                    "suite_cache_inputs_hash": suite_cache_inputs_hash,
                });
                let _ = context.store.set_readiness_result(
                    ReadinessStatus::NotReady,
                    &summary,
                    &details,
                );
                finish_readiness_run(RunExitStatus::Failure);
                Err(ApiError::internal(anyhow!(
                    "pre-run checks task failed: {err}"
                )))
            }
        }
    }

    async fn execute_and_persist_readiness_check(
        &self,
        project: String,
        store: ProjectStore,
        readiness_run_id: String,
        chief_yaml_hash: String,
        suite_cache_inputs_hash: String,
        readiness_log: ReadinessLogContext,
        readiness_stream: ReadinessStreamContext,
        command_plans: Vec<ReadinessCommandPlan>,
        suite_count: usize,
        command_count: usize,
        cancel_signal: Arc<AtomicBool>,
        readiness_git: ShellGitOps,
        readiness_worktree: TempWorktree,
        project_dir: PathBuf,
        project_name: String,
        suites: Vec<TestSuiteConfig>,
    ) -> Result<(), ApiError> {
        let finish_readiness_run = |status: RunExitStatus| {
            if let Err(err) = store.finish_run(&readiness_run_id, status) {
                warn!(
                    project = %project,
                    run_id = %readiness_run_id,
                    error = %err,
                    "failed to finish pre-run checks event run"
                );
            }
        };

        let execution = tokio::task::spawn_blocking({
            let cancel_signal = cancel_signal.clone();
            let readiness_stream = readiness_stream.clone();
            move || {
                execute_readiness_command_plans(
                    command_plans,
                    cancel_signal,
                    Some(readiness_stream),
                )
            }
        })
        .await;
        self.clear_readiness_cancel_signal(&project, &cancel_signal)
            .await;
        let cleanup_readiness_worktree = || {
            if let Err(err) = cleanup_temp_worktree(&readiness_git, &readiness_worktree) {
                warn!(
                    project = %project,
                    branch = %readiness_worktree.branch,
                    worktree = %readiness_worktree.path.display(),
                    error = %err,
                    "failed to cleanup pre-run checks worktree"
                );
                let cleanup_message = format!(
                    "Warning: failed to cleanup pre-run checks worktree {}: {}\n",
                    readiness_worktree.path.display(),
                    err
                );
                readiness_stream.push_text(cleanup_message.clone());
                let mut payload = readiness_payload("pre_run_checks_worktree_cleanup_failed");
                payload.insert(
                    "branch".to_owned(),
                    serde_json::Value::String(readiness_worktree.branch.clone()),
                );
                payload.insert(
                    "worktree".to_owned(),
                    serde_json::Value::String(readiness_worktree.path.display().to_string()),
                );
                payload.insert(
                    "error".to_owned(),
                    serde_json::Value::String(err.to_string()),
                );
                record_readiness_event(Some(&readiness_log), "warning", cleanup_message, payload);
            }
        };

        let results = match execution {
            Ok(Ok(results)) => results,
            Ok(Err(err)) => {
                let cancelled_by_user = err
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("cancelled by user");
                let summary = if cancelled_by_user {
                    "Not ready: pre-run checks cancelled by user.".to_owned()
                } else {
                    format!("Not ready: pre-run checks execution failed ({err})")
                };
                let details = json!({
                    "error": err.to_string(),
                    "commands_total": command_count,
                    "suite_count": suite_count,
                    "cancelled_by_user": cancelled_by_user,
                    "chief_yaml_hash": chief_yaml_hash.as_str(),
                    "suite_cache_inputs_hash": suite_cache_inputs_hash.as_str(),
                });
                let _ = store.set_readiness_result(ReadinessStatus::NotReady, &summary, &details);
                let mut payload = readiness_payload("pre_run_checks_failed");
                payload.insert(
                    "error".to_owned(),
                    serde_json::Value::String(err.to_string()),
                );
                payload.insert(
                    "cancelled_by_user".to_owned(),
                    serde_json::Value::Bool(cancelled_by_user),
                );
                let failure_message = if cancelled_by_user {
                    "Pre-run checks cancelled by user.\n".to_owned()
                } else {
                    format!("Pre-run checks failed: {err}\n")
                };
                record_readiness_event(
                    Some(&readiness_log),
                    if cancelled_by_user {
                        "warning"
                    } else {
                        "error"
                    },
                    failure_message.clone(),
                    payload,
                );
                readiness_stream.push_text(failure_message);
                cleanup_readiness_worktree();
                finish_readiness_run(RunExitStatus::Failure);
                if cancelled_by_user {
                    return Err(ApiError::unprocessable(summary));
                }
                return Err(ApiError::internal(err));
            }
            Err(err) => {
                let summary = format!("Not ready: pre-run checks task failed ({err})");
                let details = json!({
                    "error": err.to_string(),
                    "commands_total": command_count,
                    "suite_count": suite_count,
                    "chief_yaml_hash": chief_yaml_hash.as_str(),
                    "suite_cache_inputs_hash": suite_cache_inputs_hash.as_str(),
                });
                let _ = store.set_readiness_result(ReadinessStatus::NotReady, &summary, &details);
                let mut payload = readiness_payload("pre_run_checks_failed");
                payload.insert(
                    "error".to_owned(),
                    serde_json::Value::String(err.to_string()),
                );
                let failure_message = format!("Pre-run checks task failed: {err}\n");
                record_readiness_event(
                    Some(&readiness_log),
                    "error",
                    failure_message.clone(),
                    payload,
                );
                readiness_stream.push_text(failure_message);
                cleanup_readiness_worktree();
                finish_readiness_run(RunExitStatus::Failure);
                return Err(ApiError::internal(anyhow!(
                    "pre-run checks task failed: {err}"
                )));
            }
        };

        let failed_commands = results
            .iter()
            .filter(|result| result.blocking_failure)
            .count();
        let summary = build_readiness_summary(&results, suite_count);
        let details = build_readiness_details(
            &results,
            suite_count,
            &chief_yaml_hash,
            &suite_cache_inputs_hash,
        );
        let final_status = if failed_commands == 0 {
            ReadinessStatus::Ready
        } else {
            ReadinessStatus::NotReady
        };

        if let Err(err) = store.set_readiness_result(final_status, &summary, &details) {
            cleanup_readiness_worktree();
            finish_readiness_run(RunExitStatus::Failure);
            return Err(ApiError::internal(err));
        }

        info!(
            project = %project,
            suites = suite_count,
            commands = results.len(),
            failed_commands,
            status = final_status.as_str(),
            "project pre-run checks finished"
        );

        if final_status == ReadinessStatus::Ready {
            if let Err(err) = worktree_cache::prime_suite_caches_from_worktree(
                &project_dir,
                &project_name,
                &suites,
                &readiness_worktree.path,
                &chief_yaml_hash,
            ) {
                let cache_message = format!(
                    "Warning: failed to snapshot suite dependency cache from pre-run checks: {err}\n"
                );
                readiness_stream.push_text(cache_message.clone());
                let mut payload = readiness_payload("pre_run_checks_cache_snapshot_failed");
                payload.insert(
                    "error".to_owned(),
                    serde_json::Value::String(err.to_string()),
                );
                record_readiness_event(Some(&readiness_log), "warning", cache_message, payload);
            }

            let mut payload = readiness_payload("pre_run_checks_completed");
            payload.insert(
                "status".to_owned(),
                serde_json::Value::String("ready".to_owned()),
            );
            payload.insert(
                "failed_commands".to_owned(),
                serde_json::Value::from(failed_commands as u64),
            );
            payload.insert(
                "command_count".to_owned(),
                serde_json::Value::from(results.len() as u64),
            );
            record_readiness_event(
                Some(&readiness_log),
                "info",
                format!("{summary}\n"),
                payload,
            );
            readiness_stream.push_text(format!("{summary}\n"));
            cleanup_readiness_worktree();
            finish_readiness_run(RunExitStatus::Success);
            return Ok(());
        }

        let first_failure = results
            .iter()
            .find(|result| result.blocking_failure)
            .map(|result| {
                format!(
                    "suite '{}' {} exited {}",
                    result.suite_name,
                    result.kind.as_str(),
                    result.exit_code
                )
            })
            .unwrap_or_default();

        let mut payload = readiness_payload("pre_run_checks_completed");
        payload.insert(
            "status".to_owned(),
            serde_json::Value::String("not_ready".to_owned()),
        );
        payload.insert(
            "failed_commands".to_owned(),
            serde_json::Value::from(failed_commands as u64),
        );
        payload.insert(
            "command_count".to_owned(),
            serde_json::Value::from(results.len() as u64),
        );
        record_readiness_event(
            Some(&readiness_log),
            "warning",
            format!("{summary}\n"),
            payload,
        );
        readiness_stream.push_text(format!("{summary}\n"));
        cleanup_readiness_worktree();
        finish_readiness_run(RunExitStatus::Failure);

        Err(ApiError::unprocessable(if first_failure.is_empty() {
            summary
        } else {
            format!("{summary}. First failed command: {first_failure}")
        }))
    }

    pub fn subscribe_readiness_stream(&self) -> broadcast::Receiver<ReadinessStreamMessage> {
        self.readiness_stream_sender.subscribe()
    }

    pub fn readiness_stream_snapshot(&self, project: &str) -> Option<String> {
        self.readiness_stream_snapshots
            .lock()
            .ok()
            .and_then(|snapshots| snapshots.get(project).cloned())
            .filter(|snapshot| !snapshot.is_empty())
    }

    async fn register_readiness_cancel_signal(
        &self,
        project: &str,
    ) -> anyhow::Result<Arc<AtomicBool>> {
        let signal = Arc::new(AtomicBool::new(false));
        let mut signals = self.readiness_cancel_signals.lock().await;
        if let Some(existing) = signals.insert(project.to_owned(), signal.clone()) {
            existing.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(signal)
    }

    async fn clear_readiness_cancel_signal(&self, project: &str, signal: &Arc<AtomicBool>) {
        let mut signals = self.readiness_cancel_signals.lock().await;
        let should_remove = signals
            .get(project)
            .map(|current| Arc::ptr_eq(current, signal))
            .unwrap_or(false);
        if should_remove {
            signals.remove(project);
        }
    }

    pub(super) async fn ensure_project_readiness(
        &self,
        project: &str,
        context: &chief::service::ProjectContext,
        chief_yaml_hash: &str,
        force: bool,
    ) -> Result<bool, ApiError> {
        let suite_cache_inputs_hash = worktree_cache::suite_cache_inputs_hash(
            &context.project_dir,
            &context.chief_yaml.suites,
            chief_yaml_hash,
        );
        let should_run = force
            || should_run_readiness_check(
                &context.store,
                chief_yaml_hash,
                &suite_cache_inputs_hash,
            )
            .map_err(ApiError::internal)?;
        if should_run {
            self.run_and_persist_readiness_check(
                project,
                context,
                chief_yaml_hash,
                &suite_cache_inputs_hash,
            )
            .await?;
            return Ok(true);
        }

        info!(
            project,
            "skipping pre-run checks because previous checks succeeded and chief.yaml is unchanged"
        );
        Ok(false)
    }
}

pub(super) fn project_readiness_response(
    readiness: ProjectReadinessState,
) -> ProjectReadinessResponse {
    ProjectReadinessResponse {
        status: readiness.status.as_str().to_owned(),
        summary: if readiness.summary.trim().is_empty() {
            READINESS_UNCHECKED_SUMMARY.to_owned()
        } else {
            readiness.summary
        },
        checking_started_at: readiness
            .checking_started_at
            .map(|value| value.to_rfc3339()),
        checked_at: readiness.checked_at.map(|value| value.to_rfc3339()),
        updated_at: readiness.updated_at.to_rfc3339(),
    }
}

fn build_readiness_command_plans(
    context: &chief::service::ProjectContext,
    project_dir: &Path,
) -> Vec<ReadinessCommandPlan> {
    let mut plans = Vec::new();
    let default_timeout = context.chief_yaml.chief.suite_command_timeout_seconds;

    for suite in &context.chief_yaml.suites {
        let mut target_candidates = collect_readiness_targets(project_dir, suite);
        if let Some(default_target) = normalized_default_target(suite)
            && !target_candidates.contains(&default_target)
        {
            target_candidates.push(default_target);
        }
        let timeout_seconds = suite
            .command_timeout_seconds
            .unwrap_or(default_timeout)
            .max(1);
        let cwd = suite_command_cwd(project_dir, suite);
        let cwd_display = cwd.display().to_string();

        if let Some(command_template) = suite
            .test_init
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_owned)
        {
            plans.push(ReadinessCommandPlan {
                suite_name: suite.name.clone(),
                kind: ReadinessCommandKind::TestInit,
                uses_target_placeholder: command_template.contains("{target}"),
                command_template,
                cleanup_command: None,
                target_candidates: target_candidates.clone(),
                cwd: cwd.clone(),
                cwd_display: cwd_display.clone(),
                env: suite.env.clone(),
                timeout_seconds,
            });
        }

        if let Some(command_template) = suite
            .test_setup
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_owned)
        {
            plans.push(ReadinessCommandPlan {
                suite_name: suite.name.clone(),
                kind: ReadinessCommandKind::TestSetup,
                uses_target_placeholder: command_template.contains("{target}"),
                command_template,
                cleanup_command: None,
                target_candidates: target_candidates.clone(),
                cwd: cwd.clone(),
                cwd_display: cwd_display.clone(),
                env: suite.env.clone(),
                timeout_seconds,
            });
        }

        if let Some(command_template) = suite
            .lint_command
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_owned)
        {
            plans.push(ReadinessCommandPlan {
                suite_name: suite.name.clone(),
                kind: ReadinessCommandKind::Lint,
                uses_target_placeholder: command_template.contains("{target}"),
                command_template,
                cleanup_command: None,
                target_candidates: target_candidates.clone(),
                cwd: cwd.clone(),
                cwd_display: cwd_display.clone(),
                env: suite.env.clone(),
                timeout_seconds,
            });
        }

        if let Some(command_template) =
            Some(suite.test_command.trim().to_owned()).filter(|command| !command.is_empty())
        {
            plans.push(ReadinessCommandPlan {
                suite_name: suite.name.clone(),
                kind: ReadinessCommandKind::Test,
                uses_target_placeholder: command_template.contains("{target}"),
                command_template,
                cleanup_command: suite.cleanup_command.clone(),
                target_candidates,
                cwd,
                cwd_display,
                env: suite.env.clone(),
                timeout_seconds,
            });
        }
    }

    plans
}

fn normalized_default_target(suite: &TestSuiteConfig) -> Option<String> {
    suite
        .default_target
        .as_deref()
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(str::to_owned)
}

fn collect_readiness_targets(project_dir: &Path, suite: &TestSuiteConfig) -> Vec<String> {
    let patterns = suite
        .file_patterns
        .iter()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
        .filter_map(|pattern| glob::Pattern::new(pattern).ok())
        .collect::<Vec<_>>();

    if patterns.is_empty() {
        return Vec::new();
    }

    let tracked_files = git_list_tracked_files(project_dir, suite.test_root.trim());
    if tracked_files.is_empty() {
        return Vec::new();
    }

    let root_prefix = normalized_root_prefix(suite.test_root.trim());
    let mut targets = std::collections::BTreeSet::new();
    for file in tracked_files {
        let relative = strip_root_prefix(&file, root_prefix.as_deref()).unwrap_or(file.as_str());
        let matches_pattern = patterns
            .iter()
            .any(|pattern| pattern.matches(relative) || pattern.matches(&file));
        if !matches_pattern {
            continue;
        }

        let selected = if suite.strip_root_from_target {
            relative.to_owned()
        } else {
            file.clone()
        };
        if !selected.trim().is_empty() {
            targets.insert(selected);
        }
    }
    targets.into_iter().collect()
}

pub(super) fn chief_yaml_content_hash(config_path: &Path) -> anyhow::Result<String> {
    let content = fs::read(config_path)
        .with_context(|| format!("failed to read chief config at {}", config_path.display()))?;
    Ok(format!("{:x}", md5::compute(content)))
}

fn should_run_readiness_check(
    store: &ProjectStore,
    chief_yaml_hash: &str,
    suite_cache_inputs_hash: &str,
) -> anyhow::Result<bool> {
    let readiness = store.get_readiness_state()?;
    if readiness.status != ReadinessStatus::Ready {
        return Ok(true);
    }
    let previous_hash = readiness_chief_yaml_hash(&readiness.details);
    if previous_hash != Some(chief_yaml_hash) {
        return Ok(true);
    }
    let previous_suite_cache_hash = readiness_suite_cache_inputs_hash(&readiness.details);
    Ok(previous_suite_cache_hash
        .map(|value| value != suite_cache_inputs_hash)
        .unwrap_or(false))
}

pub(super) fn readiness_chief_yaml_hash(details: &serde_json::Value) -> Option<&str> {
    details
        .get("chief_yaml_hash")
        .and_then(serde_json::Value::as_str)
}

fn readiness_suite_cache_inputs_hash(details: &serde_json::Value) -> Option<&str> {
    details
        .get("suite_cache_inputs_hash")
        .and_then(serde_json::Value::as_str)
}

fn git_list_tracked_files(project_dir: &Path, test_root: &str) -> Vec<String> {
    let output = if test_root.is_empty() || test_root == "." {
        run_git_command_with_retry(project_dir, &["ls-files", "--"])
    } else {
        run_git_command_with_retry(project_dir, &["ls-files", "--", test_root])
    };

    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    let mut files = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn normalized_root_prefix(test_root: &str) -> Option<String> {
    let trimmed = test_root.trim();
    if trimmed.is_empty() || trimmed == "." {
        return None;
    }
    Some(trimmed.trim_end_matches('/').to_owned())
}

fn strip_root_prefix<'a>(path: &'a str, root_prefix: Option<&str>) -> Option<&'a str> {
    let Some(root_prefix) = root_prefix else {
        return Some(path);
    };

    if path == root_prefix {
        return Some("");
    }
    let prefix = format!("{root_prefix}/");
    path.strip_prefix(&prefix)
}

fn replace_target_placeholder(command_template: &str, target: &str) -> String {
    command_template.replace("{target}", target)
}

fn readiness_payload(stage: &str) -> BTreeMap<String, serde_json::Value> {
    let mut payload = BTreeMap::new();
    payload.insert(
        "source".to_owned(),
        serde_json::Value::String(READINESS_EVENT_SOURCE.to_owned()),
    );
    payload.insert(
        "stage".to_owned(),
        serde_json::Value::String(stage.to_owned()),
    );
    payload
}

fn record_readiness_event(
    log_context: Option<&ReadinessLogContext>,
    level: &str,
    msg: impl Into<String>,
    mut payload: BTreeMap<String, serde_json::Value>,
) {
    let Some(log_context) = log_context else {
        return;
    };

    payload.insert(
        "source".to_owned(),
        serde_json::Value::String(READINESS_EVENT_SOURCE.to_owned()),
    );
    let event = EventRecord {
        id: None,
        run_id: log_context.run_id.clone(),
        job_id: None,
        todo_id: None,
        timestamp: Utc::now(),
        level: level.to_owned(),
        phase: None,
        msg: msg.into(),
        event_type: EventType::Msg,
        payload,
    };

    if let Err(err) = log_context.store.record_event(&event) {
        warn!(
            run_id = %log_context.run_id,
            error = %err,
            "failed to record readiness event"
        );
    }
}

fn suite_kind_for_readiness(kind: ReadinessCommandKind) -> SuiteCommandKind {
    match kind {
        ReadinessCommandKind::Lint => SuiteCommandKind::Lint,
        ReadinessCommandKind::TestInit
        | ReadinessCommandKind::TestSetup
        | ReadinessCommandKind::Test => SuiteCommandKind::Test,
    }
}

fn execute_readiness_command_plans(
    plans: Vec<ReadinessCommandPlan>,
    cancel_signal: Arc<AtomicBool>,
    stream_context: Option<ReadinessStreamContext>,
) -> anyhow::Result<Vec<ReadinessCommandResult>> {
    let mut results = Vec::with_capacity(plans.len());

    for plan in plans {
        if cancel_signal.load(std::sync::atomic::Ordering::SeqCst) {
            if let Some(stream_context) = stream_context.as_ref() {
                stream_context.push_text("Pre-run checks cancelled by user.\n");
            }
            return Err(anyhow!("pre-run checks cancelled by user"));
        }

        if !plan.cwd.exists() {
            if let Some(stream_context) = stream_context.as_ref() {
                stream_context.push_text(format!(
                    "[pre-run-checks:{}:{}] working directory does not exist: {}\n",
                    plan.suite_name,
                    plan.kind.as_str(),
                    plan.cwd.display()
                ));
            }
            results.push(ReadinessCommandResult {
                suite_name: plan.suite_name,
                kind: plan.kind,
                command: plan.command_template,
                cwd: plan.cwd_display,
                target: None,
                exit_code: 127,
                blocking_failure: true,
                output_tail: format!("working directory does not exist: {}", plan.cwd.display()),
            });
            continue;
        }

        if !plan.cwd.is_dir() {
            if let Some(stream_context) = stream_context.as_ref() {
                stream_context.push_text(format!(
                    "[pre-run-checks:{}:{}] working directory is not a directory: {}\n",
                    plan.suite_name,
                    plan.kind.as_str(),
                    plan.cwd.display()
                ));
            }
            results.push(ReadinessCommandResult {
                suite_name: plan.suite_name,
                kind: plan.kind,
                command: plan.command_template,
                cwd: plan.cwd_display,
                target: None,
                exit_code: 127,
                blocking_failure: true,
                output_tail: format!(
                    "working directory is not a directory: {}",
                    plan.cwd.display()
                ),
            });
            continue;
        }

        if !plan.uses_target_placeholder {
            results.push(run_readiness_command_attempt(
                &plan,
                plan.command_template.clone(),
                None,
                &cancel_signal,
                stream_context.as_ref(),
            ));
            if cancel_signal.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(stream_context) = stream_context.as_ref() {
                    stream_context.push_text("Pre-run checks cancelled by user.\n");
                }
                return Err(anyhow!("pre-run checks cancelled by user"));
            }
            continue;
        }

        if plan.target_candidates.is_empty() {
            if let Some(stream_context) = stream_context.as_ref() {
                stream_context.push_text(format!(
                    "[pre-run-checks:{}:{}] command uses {{target}}, but no file_patterns target matched and default_target is not set\n",
                    plan.suite_name,
                    plan.kind.as_str()
                ));
            }
            results.push(ReadinessCommandResult {
                suite_name: plan.suite_name,
                kind: plan.kind,
                command: plan.command_template,
                cwd: plan.cwd_display,
                target: None,
                exit_code: 127,
                blocking_failure: true,
                output_tail: "command uses {target}, but no file_patterns target matched and default_target is not set".to_owned(),
            });
            continue;
        }

        let mut selected: Option<ReadinessCommandResult> = None;
        for target in &plan.target_candidates {
            if cancel_signal.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(stream_context) = stream_context.as_ref() {
                    stream_context.push_text("Pre-run checks cancelled by user.\n");
                }
                return Err(anyhow!("pre-run checks cancelled by user"));
            }
            let command = replace_target_placeholder(&plan.command_template, target);
            let attempt = run_readiness_command_attempt(
                &plan,
                command,
                Some(target.clone()),
                &cancel_signal,
                stream_context.as_ref(),
            );
            let runnable = !attempt.blocking_failure;
            selected = Some(attempt);
            if runnable {
                break;
            }
        }
        if let Some(result) = selected {
            results.push(result);
            if cancel_signal.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(stream_context) = stream_context.as_ref() {
                    stream_context.push_text("Pre-run checks cancelled by user.\n");
                }
                return Err(anyhow!("pre-run checks cancelled by user"));
            }
        }
    }

    Ok(results)
}

fn run_readiness_command_attempt(
    plan: &ReadinessCommandPlan,
    command: String,
    target: Option<String>,
    cancel_signal: &Arc<AtomicBool>,
    stream_context: Option<&ReadinessStreamContext>,
) -> ReadinessCommandResult {
    if let Some(stream_context) = stream_context {
        stream_context.push_text(format!(
            "[pre-run-checks:{}:{}]$ {} (cwd: {})\n",
            plan.suite_name,
            plan.kind.as_str(),
            command,
            plan.cwd_display
        ));
    }

    let out = execute_suite_command_streaming(
        &plan.suite_name,
        suite_kind_for_readiness(plan.kind),
        &command,
        &plan.cwd,
        &plan.cwd_display,
        &plan.env,
        plan.timeout_seconds,
        Some(cancel_signal),
        |_stream, text| {
            if let Some(stream_context) = stream_context {
                stream_context.push_text(text);
            }
        },
    );

    let cleanup_out = if plan.kind == ReadinessCommandKind::Test {
        execute_suite_cleanup_command(
            plan.cleanup_command.as_deref(),
            &plan.cwd,
            &plan.env,
            Some(plan.timeout_seconds),
        )
    } else {
        Ok(None)
    };

    match out {
        Ok(out) => {
            match cleanup_out {
                Ok(Some(cleanup)) => {
                    if let Some(stream_context) = stream_context {
                        stream_context.push_text(format!(
                            "[pre-run-checks:{}:cleanup] exit={}\n",
                            plan.suite_name, cleanup.exit_code
                        ));
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    if let Some(stream_context) = stream_context {
                        stream_context.push_text(format!(
                            "[pre-run-checks:{}:cleanup] failed: {}\n",
                            plan.suite_name, err
                        ));
                    }
                }
            }
            let blocking_failure = readiness_exit_code_is_blocking(plan.kind, out.exit_code);
            if let Some(stream_context) = stream_context {
                stream_context.push_text(format!(
                    "[pre-run-checks:{}:{}] exit={}{}\n",
                    plan.suite_name,
                    plan.kind.as_str(),
                    out.exit_code,
                    if blocking_failure { " (blocking)" } else { "" }
                ));
            }
            ReadinessCommandResult {
                suite_name: plan.suite_name.clone(),
                kind: plan.kind,
                command: out.command,
                cwd: plan.cwd_display.clone(),
                target,
                exit_code: out.exit_code,
                blocking_failure,
                output_tail: readiness_output_tail(&out.output),
            }
        }
        Err(err) => {
            match cleanup_out {
                Ok(Some(cleanup)) => {
                    if let Some(stream_context) = stream_context {
                        stream_context.push_text(format!(
                            "[pre-run-checks:{}:cleanup] exit={}\n",
                            plan.suite_name, cleanup.exit_code
                        ));
                    }
                }
                Ok(None) => {}
                Err(cleanup_err) => {
                    if let Some(stream_context) = stream_context {
                        stream_context.push_text(format!(
                            "[pre-run-checks:{}:cleanup] failed: {}\n",
                            plan.suite_name, cleanup_err
                        ));
                    }
                }
            }
            if let Some(stream_context) = stream_context {
                stream_context.push_text(format!(
                    "[pre-run-checks:{}:{}] failed: {}\n",
                    plan.suite_name,
                    plan.kind.as_str(),
                    err
                ));
            }
            ReadinessCommandResult {
                suite_name: plan.suite_name.clone(),
                kind: plan.kind,
                command,
                cwd: plan.cwd_display.clone(),
                target,
                exit_code: 127,
                blocking_failure: true,
                output_tail: readiness_output_tail(&err.to_string()),
            }
        }
    }
}

fn readiness_exit_code_is_blocking(kind: ReadinessCommandKind, exit_code: i32) -> bool {
    match kind {
        ReadinessCommandKind::TestInit | ReadinessCommandKind::TestSetup => exit_code != 0,
        ReadinessCommandKind::Lint | ReadinessCommandKind::Test => !matches!(exit_code, 0 | 1 | 5),
    }
}

fn readiness_output_tail(output: &str) -> String {
    let lines = output
        .lines()
        .rev()
        .take(25)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");

    if lines.chars().count() > 2_000 {
        let reversed = lines.chars().rev().take(2_000).collect::<String>();
        return reversed.chars().rev().collect();
    }

    lines
}

fn build_readiness_summary(results: &[ReadinessCommandResult], suite_count: usize) -> String {
    if suite_count == 0 {
        return "Ready: no suites configured, so pre-run checks skipped command execution."
            .to_owned();
    }

    if results.is_empty() {
        return format!(
            "Ready: no runnable suite commands detected across {suite_count} suite(s)."
        );
    }

    let failed_commands = results
        .iter()
        .filter(|result| result.blocking_failure)
        .count();

    if failed_commands == 0 {
        format!(
            "Ready: validated {} command(s) across {suite_count} suite(s).",
            results.len()
        )
    } else {
        format!(
            "Not ready: {} command(s) failed across {} checked command(s).",
            failed_commands,
            results.len()
        )
    }
}

fn build_readiness_details(
    results: &[ReadinessCommandResult],
    suite_count: usize,
    chief_yaml_hash: &str,
    suite_cache_inputs_hash: &str,
) -> serde_json::Value {
    let failed_commands = results
        .iter()
        .filter(|result| result.blocking_failure)
        .count();

    json!({
        "suite_count": suite_count,
        "commands_total": results.len(),
        "commands_failed": failed_commands,
        "chief_yaml_hash": chief_yaml_hash,
        "suite_cache_inputs_hash": suite_cache_inputs_hash,
        "commands": results
            .iter()
            .map(|result| json!({
                "suite": result.suite_name,
                "kind": result.kind.as_str(),
                "command": result.command,
                "cwd": result.cwd,
                "target": result.target,
                "exit_code": result.exit_code,
                "failed": result.blocking_failure,
                "output_tail": result.output_tail,
            }))
            .collect::<Vec<_>>()
    })
}

fn trim_leading_bytes(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }

    let bytes_to_trim = text.len().saturating_sub(max_bytes);
    let split_at = text
        .char_indices()
        .find_map(|(index, _)| (index >= bytes_to_trim).then_some(index))
        .unwrap_or(text.len());
    text.drain(..split_at);
}
