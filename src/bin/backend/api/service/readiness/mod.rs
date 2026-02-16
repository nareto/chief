use super::*;

mod execution;
mod planning;
mod reporting;

use execution::execute_readiness_command_plans;
use planning::{build_readiness_command_plans, should_run_readiness_check};
use reporting::{
    build_readiness_details, build_readiness_summary, readiness_payload, record_readiness_event,
    trim_leading_bytes,
};

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

pub(super) fn chief_yaml_content_hash(config_path: &Path) -> anyhow::Result<String> {
    let content = fs::read(config_path)
        .with_context(|| format!("failed to read chief config at {}", config_path.display()))?;
    Ok(format!("{:x}", md5::compute(content)))
}

pub(super) fn readiness_chief_yaml_hash(details: &serde_json::Value) -> Option<&str> {
    details
        .get("chief_yaml_hash")
        .and_then(serde_json::Value::as_str)
}
