use crate::agent::{AgentCancelledError, CodingAgent, CommandAgent, is_agent_cancelled_error};
use crate::config::ChiefToml;
use crate::domain::{
    EventRecord, EventType, JobRecord, JobStatus, Phase, RunExitStatus, Todo, TodoStatus,
};
use crate::flow::{FlowExecution, FlowKind, TodoOutcome, build_flow};
use crate::git::{GitOps, ShellGitOps};
use crate::orchestrator::{OrchestratorError, OrchestratorResult, retry_with_policy_and_hook};
use crate::prompt::{FsPromptStore, PromptStore};
use crate::storage::ProjectStore;
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::warn;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ProjectContext {
    pub name: String,
    pub project_dir: PathBuf,
    pub config_path: PathBuf,
    pub chief_toml: ChiefToml,
    pub store: ProjectStore,
    pub prompts: FsPromptStore,
    pub git: ShellGitOps,
}

impl ProjectContext {
    pub fn load(project_dir: impl AsRef<Path>) -> Result<Self> {
        let project_dir = project_dir.as_ref().to_path_buf();
        let config_path = project_dir.join("chief.toml");
        let chief_toml = ChiefToml::load_or_default(&config_path)?;

        let store = ProjectStore::new(&project_dir);
        store.init()?;

        let prompts = FsPromptStore::from_workspace_prompts()?;

        let git = ShellGitOps::discover(&project_dir)
            .with_context(|| format!("{} is not a git repository", project_dir.display()))?;

        let name = project_dir
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("project")
            .to_owned();

        Ok(Self {
            name,
            project_dir,
            config_path,
            chief_toml,
            store,
            prompts,
            git,
        })
    }

    pub fn refresh(&mut self) -> Result<()> {
        self.chief_toml = ChiefToml::load_or_default(&self.config_path)?;
        self.store.sync_todos_from_file()?;
        Ok(())
    }

    pub fn build_agent(&self, model_override: Option<String>) -> Arc<dyn CodingAgent> {
        Arc::new(CommandAgent::from_config(
            &self.chief_toml.chief,
            model_override,
        ))
    }

    pub fn pick_next_todo_priority(&self) -> Result<Option<Todo>> {
        let mut candidates = self.store.list_available_todos()?;
        candidates.sort_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.id.cmp(&b.id)));
        Ok(candidates.into_iter().next())
    }

    pub fn claim_todo(&self, todo_id: &str) -> Result<Option<Todo>> {
        self.store.claim_todo(todo_id)
    }

    pub fn create_job(
        &self,
        run_id: &str,
        worker_index: usize,
        flow_kind: FlowKind,
        todo_id: Option<String>,
        worktree_path: Option<String>,
    ) -> Result<JobRecord> {
        let job = JobRecord {
            id: Uuid::new_v4().to_string(),
            run_id: run_id.to_owned(),
            todo_id,
            status: JobStatus::Queued,
            worker_index,
            flow: flow_kind.as_str().to_owned(),
            worktree_path,
            started_at: Utc::now(),
            ended_at: None,
            error: None,
        };
        self.store.upsert_job(&job)?;
        Ok(job)
    }

    pub fn set_job_status(
        &self,
        mut job: JobRecord,
        status: JobStatus,
        error: Option<String>,
    ) -> Result<JobRecord> {
        job.status = status;
        if matches!(
            status,
            JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled
        ) {
            job.ended_at = Some(Utc::now());
        }
        job.error = error;
        self.store.upsert_job(&job)?;
        Ok(job)
    }

    pub fn log_project_event(
        &self,
        run_id: &str,
        job_id: Option<String>,
        todo_id: Option<String>,
        level: &str,
        phase: Option<Phase>,
        event_type: EventType,
        msg: impl Into<String>,
        payload: BTreeMap<String, serde_json::Value>,
    ) -> Result<()> {
        let event = EventRecord {
            id: None,
            run_id: run_id.to_owned(),
            job_id,
            todo_id,
            timestamp: Utc::now(),
            level: level.to_owned(),
            phase,
            msg: msg.into(),
            event_type,
            payload,
        };
        self.store.record_event(&event)
    }
}

#[derive(Debug, Clone)]
pub struct ProjectRegistry {
    parent_dir: PathBuf,
    projects: HashMap<String, ProjectContext>,
}

impl ProjectRegistry {
    pub fn discover(parent_dir: impl AsRef<Path>) -> Result<Self> {
        let parent_dir = parent_dir.as_ref().to_path_buf();
        let mut projects = HashMap::new();

        for entry in fs::read_dir(&parent_dir)
            .with_context(|| format!("failed to read {}", parent_dir.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            if !path.join(".git").exists() {
                continue;
            }
            let Ok(context) = ProjectContext::load(&path) else {
                continue;
            };
            projects.insert(context.name.clone(), context);
        }

        Ok(Self {
            parent_dir,
            projects,
        })
    }

    pub fn parent_dir(&self) -> &Path {
        &self.parent_dir
    }

    pub fn list_projects(&self) -> Vec<ProjectContext> {
        let mut items = self.projects.values().cloned().collect::<Vec<_>>();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        items
    }

    pub fn get(&self, project_name: &str) -> Option<ProjectContext> {
        self.projects.get(project_name).cloned()
    }

    pub fn reload(&mut self) -> Result<()> {
        *self = Self::discover(&self.parent_dir)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ChiefEngine {
    pub project: ProjectContext,
}

impl ChiefEngine {
    pub fn new(project: ProjectContext) -> Self {
        Self { project }
    }

    pub fn start_run(&self) -> Result<String> {
        let run_id = Uuid::new_v4().to_string();
        self.project.store.start_run(&run_id)?;
        Ok(run_id)
    }

    pub fn finish_run(&self, run_id: &str, status: RunExitStatus) -> Result<()> {
        self.project.store.finish_run(run_id, status)
    }

    pub fn run_single_todo_once(
        &self,
        run_id: &str,
        job_id: &str,
        worker_index: usize,
        todo: Todo,
        flow_kind: FlowKind,
        work_dir: PathBuf,
        model_override: Option<String>,
        cancel_signal: Arc<AtomicBool>,
    ) -> OrchestratorResult<TodoOutcome> {
        if cancel_signal.load(Ordering::SeqCst) {
            return Err(OrchestratorError::unrecoverable(anyhow!(
                AgentCancelledError
            )));
        }

        let flow = build_flow(flow_kind);
        let agent = self.project.build_agent(model_override);

        let mut execution = FlowExecution {
            run_id: run_id.to_owned(),
            job_id: job_id.to_owned(),
            worker_index,
            project_dir: work_dir,
            store: &self.project.store,
            prompts: &self.project.prompts,
            agent: agent.as_ref(),
            git: &self.project.git,
            chief_config: &self.project.chief_toml.chief,
            all_suites: &self.project.chief_toml.suites,
            todo,
            cancel_signal,
        };

        flow.run_todo(&mut execution)
            .map_err(|err| self.classify_runtime_error(err))
    }

    pub fn run_single_todo(
        &self,
        run_id: &str,
        job_id: &str,
        worker_index: usize,
        todo: Todo,
        flow_kind: FlowKind,
        work_dir: PathBuf,
        model_override: Option<String>,
        cancel_signal: Arc<AtomicBool>,
    ) -> Result<TodoOutcome> {
        self.run_single_todo_once(
            run_id,
            job_id,
            worker_index,
            todo,
            flow_kind,
            work_dir,
            model_override,
            cancel_signal,
        )
        .map_err(OrchestratorError::into_error)
    }

    pub fn run_single_todo_with_retries<F>(
        &self,
        run_id: &str,
        job_id: &str,
        worker_index: usize,
        todo: Todo,
        flow_kind: FlowKind,
        work_dir: PathBuf,
        model_override: Option<String>,
        cancel_signal: Arc<AtomicBool>,
        max_retries: usize,
        mut on_retry: F,
    ) -> OrchestratorResult<TodoOutcome>
    where
        F: FnMut(usize, usize, &anyhow::Error),
    {
        retry_with_policy_and_hook(
            max_retries,
            |_attempt, _max_retries| {
                if cancel_signal.load(Ordering::SeqCst) {
                    return Err(OrchestratorError::unrecoverable(anyhow!(
                        AgentCancelledError
                    )));
                }
                self.run_single_todo_once(
                    run_id,
                    job_id,
                    worker_index,
                    todo.clone(),
                    flow_kind,
                    work_dir.clone(),
                    model_override.clone(),
                    cancel_signal.clone(),
                )
            },
            |attempt, total, err| on_retry(attempt, total, err),
        )
    }

    pub fn run_next_todo_once(
        &self,
        flow_kind: FlowKind,
        model_override: Option<String>,
    ) -> OrchestratorResult<Option<TodoOutcome>> {
        let run_id = self
            .start_run()
            .map_err(|err| self.classify_runtime_error(err))?;

        let result = (|| -> OrchestratorResult<Option<TodoOutcome>> {
            let Some(next) = self
                .project
                .pick_next_todo_priority()
                .map_err(|err| self.classify_runtime_error(err))?
            else {
                return Ok(None);
            };
            let Some(todo) = self
                .project
                .claim_todo(&next.id)
                .map_err(|err| self.classify_runtime_error(err))?
            else {
                return Ok(None);
            };

            let mut job = self
                .project
                .create_job(&run_id, 1, flow_kind, Some(todo.id.clone()), None)
                .map_err(|err| self.classify_runtime_error(err))?;
            job = self
                .project
                .set_job_status(job, JobStatus::Running, None)
                .context("failed to set job status running")
                .map_err(|err| self.classify_runtime_error(err))?;

            match self.run_single_todo_once(
                &run_id,
                &job.id,
                1,
                todo.clone(),
                flow_kind,
                self.project.project_dir.clone(),
                model_override,
                Arc::new(AtomicBool::new(false)),
            ) {
                Ok(outcome) => {
                    if let Some(commit_hash) = outcome.commit_hash.as_deref() {
                        if let Err(err) = self.project.store.update_todo_status(
                            &todo.id,
                            TodoStatus::Done,
                            Some(commit_hash),
                        ) {
                            self.log_state_update_error(
                                &run_id,
                                Some(&job.id),
                                Some(&todo.id),
                                "failed to mark todo done",
                                &err,
                            );
                        }
                    }
                    if let Err(err) = self.project.set_job_status(job, JobStatus::Completed, None) {
                        self.log_state_update_error(
                            &run_id,
                            None,
                            Some(&todo.id),
                            "failed to update job status to completed",
                            &err,
                        );
                    }
                    Ok(Some(outcome))
                }
                Err(err) => {
                    if let Err(status_err) =
                        self.project
                            .store
                            .update_todo_status(&todo.id, TodoStatus::Attempted, None)
                    {
                        self.log_state_update_error(
                            &run_id,
                            Some(&job.id),
                            Some(&todo.id),
                            "failed to mark todo attempted",
                            &status_err,
                        );
                    }
                    if let Err(status_err) =
                        self.project
                            .set_job_status(job, JobStatus::Failed, Some(err.to_string()))
                    {
                        self.log_state_update_error(
                            &run_id,
                            None,
                            Some(&todo.id),
                            "failed to update job status to failed",
                            &status_err,
                        );
                    }
                    Err(err)
                }
            }
        })();

        self.finish_run(
            &run_id,
            match &result {
                Ok(_) => RunExitStatus::Success,
                Err(err) if err.is_unrecoverable() => RunExitStatus::UnrecoverableFailure,
                Err(_) => RunExitStatus::Failure,
            },
        )
        .map_err(|err| self.classify_runtime_error(err))?;

        result
    }

    pub fn run_next_todo(
        &self,
        flow_kind: FlowKind,
        model_override: Option<String>,
    ) -> Result<Option<TodoOutcome>> {
        self.run_next_todo_once(flow_kind, model_override)
            .map_err(OrchestratorError::into_error)
    }

    pub fn run_todos_until_done_with_retries<FC, FR>(
        &self,
        flow_kind: FlowKind,
        model_override: Option<String>,
        max_retries: usize,
        mut on_todo_completed: FC,
        mut on_retry: FR,
    ) -> OrchestratorResult<()>
    where
        FC: FnMut(&TodoOutcome),
        FR: FnMut(usize, usize, &anyhow::Error),
    {
        loop {
            let next = retry_with_policy_and_hook(
                max_retries,
                |_attempt, _max_retries| self.run_next_todo_once(flow_kind, model_override.clone()),
                |attempt, total, err| on_retry(attempt, total, err),
            )?;

            let Some(outcome) = next else {
                return Ok(());
            };
            on_todo_completed(&outcome);
        }
    }

    pub fn process_requirements(
        &self,
        requirements_text: &str,
        todos_path: &Path,
        model_override: Option<String>,
    ) -> Result<String> {
        let run_id = self.start_run()?;

        let out = (|| -> Result<String> {
            let agent = self.project.build_agent(model_override);
            let prompt = self.project.prompts.render_json(
                "requirements.md",
                &serde_json::json!({
                    "requirements_text": requirements_text,
                    "todos_path": todos_path.display().to_string(),
                }),
            )?;
            let response = agent.run(crate::agent::AgentRequest {
                prompt,
                cwd: self.project.project_dir.clone(),
                timeout_seconds: Some(self.project.chief_toml.chief.agent_timeout_seconds),
                disallowed_paths: Vec::new(),
                cancel_signal: None,
            })?;

            if response.exit_code != 0 {
                return Err(anyhow!(
                    "requirements processing failed (exit code {}): {}",
                    response.exit_code,
                    response.merged_output
                ));
            }

            let diff = self
                .project
                .git
                .diff(&self.project.project_dir, Some("HEAD"))?;
            Ok(diff)
        })();

        self.finish_run(
            &run_id,
            if out.is_ok() {
                RunExitStatus::Success
            } else {
                RunExitStatus::Failure
            },
        )?;

        out
    }

    fn classify_runtime_error(&self, err: anyhow::Error) -> OrchestratorError {
        if is_known_unrecoverable_error(&err) || is_agent_cancelled_error(&err) {
            OrchestratorError::unrecoverable(err)
        } else {
            OrchestratorError::retryable(err)
        }
    }

    fn log_state_update_error(
        &self,
        run_id: &str,
        job_id: Option<&str>,
        todo_id: Option<&str>,
        msg: &str,
        err: &anyhow::Error,
    ) {
        warn!("{msg}: {err:#}");
        let mut payload = BTreeMap::new();
        payload.insert(
            "error".to_owned(),
            serde_json::Value::String(err.to_string()),
        );
        if let Err(log_err) = self.project.log_project_event(
            run_id,
            job_id.map(str::to_owned),
            todo_id.map(str::to_owned),
            "warning",
            None,
            EventType::Error,
            msg.to_owned(),
            payload,
        ) {
            warn!("failed to record state-update error event: {log_err:#}");
        }
    }
}

fn is_known_unrecoverable_error(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(io_err) = cause.downcast_ref::<io::Error>() {
            if matches!(
                io_err.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::NotFound
                    | io::ErrorKind::ReadOnlyFilesystem
            ) {
                return true;
            }
        }

        if let Some(sqlite_err) = cause.downcast_ref::<rusqlite::Error>() {
            if is_unrecoverable_sqlite_error(sqlite_err) {
                return true;
            }
        }
    }

    let text = err.to_string().to_ascii_lowercase();
    text.contains("agent binary")
        || text.contains("template load failed")
        || text.contains("is not a git repository")
}

fn is_unrecoverable_sqlite_error(err: &rusqlite::Error) -> bool {
    use rusqlite::ErrorCode;

    match err.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy)
        | Some(ErrorCode::DatabaseLocked)
        | Some(ErrorCode::OperationInterrupted)
        | Some(ErrorCode::OperationAborted) => false,
        Some(_) => true,
        None => matches!(
            err,
            rusqlite::Error::InvalidPath(_) | rusqlite::Error::SqliteSingleThreadedMode
        ),
    }
}
