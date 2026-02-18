use super::*;

mod execution;
mod planning;
mod reporting;
mod runner;

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
