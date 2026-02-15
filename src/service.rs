use crate::agent::{
    AgentCancelledError, ClaudeAgent, CodexAgent, CodingAgent, is_agent_cancelled_error,
};
use crate::config::ChiefYaml;
use crate::domain::{
    EventRecord, EventType, JobRecord, JobStatus, Phase, RunExitStatus, Todo, TodoStatus,
    payload_from_json,
};
use crate::flow::{FlowExecution, FlowKind, TodoOutcome, build_flow};
use crate::git::{
    GIT_TRANSIENT_LOCK_RETRY_ATTEMPTS, GitOps, ShellGitOps,
    git_output_has_transient_lock_contention_signature, has_transient_lock_contention_signature,
    run_git_command_with_retry,
};
use crate::orchestrator::{
    OrchestratorError, OrchestratorResult, retry_with_policy_and_hook_and_delay,
};
use crate::prompt::{FsPromptStore, PromptStore};
use crate::storage::ProjectStore;
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::warn;
use uuid::Uuid;

const TRANSIENT_LOCK_RETRY_ATTEMPTS: usize = 3;
const TRANSIENT_LOCK_MAX_ATTEMPTS: usize = TRANSIENT_LOCK_RETRY_ATTEMPTS + 1;
const TRANSIENT_LOCK_RETRY_DELAY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct ProjectContext {
    pub name: String,
    pub project_dir: PathBuf,
    pub config_path: PathBuf,
    pub chief_yaml: ChiefYaml,
    pub store: ProjectStore,
    pub prompts: FsPromptStore,
    pub git: ShellGitOps,
}

impl ProjectContext {
    pub fn load(project_dir: impl AsRef<Path>) -> Result<Self> {
        let project_dir = project_dir.as_ref().to_path_buf();
        let config_path = project_dir.join("chief.yaml");
        let chief_yaml = ChiefYaml::load_or_default(&config_path)?;

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
            chief_yaml,
            store,
            prompts,
            git,
        })
    }

    pub fn refresh(&mut self) -> Result<()> {
        self.chief_yaml = ChiefYaml::load_or_default(&self.config_path)?;
        self.store.sync_todos_from_file()?;
        Ok(())
    }

    pub fn ensure_chief_yaml_exists_for_run(&self) -> Result<()> {
        if self.config_path.is_file() {
            return Ok(());
        }
        Err(anyhow!(
            "missing required chief config at {}. create chief.yaml (run `chief init` or copy chief.example.yaml)",
            self.config_path.display()
        ))
    }

    pub fn build_agent(&self, model_override: Option<String>) -> Arc<dyn CodingAgent> {
        if self.chief_yaml.chief.agent.eq_ignore_ascii_case("claude") {
            Arc::new(ClaudeAgent::from_config(
                &self.chief_yaml.chief,
                model_override,
            ))
        } else {
            Arc::new(CodexAgent::from_config(
                &self.chief_yaml.chief,
                model_override,
            ))
        }
    }

    pub fn claim_next_pending_todo(&self) -> Result<Option<Todo>> {
        self.store.claim_next_pending_todo()
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
    projects_dir: PathBuf,
    manual_project_dirs: Vec<PathBuf>,
    projects: HashMap<String, ProjectContext>,
}

impl ProjectRegistry {
    pub fn discover(
        projects_dir: impl AsRef<Path>,
        manual_project_dirs: &[PathBuf],
    ) -> Result<Self> {
        let projects_dir = projects_dir.as_ref().to_path_buf();
        let manual_project_dirs = manual_project_dirs.to_vec();
        let projects = Self::discover_projects(&projects_dir, &manual_project_dirs)?;

        Ok(Self {
            projects_dir,
            manual_project_dirs,
            projects,
        })
    }

    fn discover_projects(
        projects_dir: &Path,
        manual_project_dirs: &[PathBuf],
    ) -> Result<HashMap<String, ProjectContext>> {
        let mut projects = HashMap::new();
        let mut seen_paths = HashSet::new();

        for entry in fs::read_dir(projects_dir)
            .with_context(|| format!("failed to read {}", projects_dir.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            if !path.join(".git").exists() {
                continue;
            }

            let normalized = Self::normalize_project_path(&path);
            if !seen_paths.insert(normalized) {
                continue;
            }

            let Ok(context) = ProjectContext::load(&path) else {
                continue;
            };
            Self::insert_project(&mut projects, context)?;
        }

        let cwd =
            std::env::current_dir().context("failed resolving current directory for --project")?;
        for manual_project_dir in manual_project_dirs {
            let project_dir = if manual_project_dir.is_absolute() {
                manual_project_dir.clone()
            } else {
                cwd.join(manual_project_dir)
            };

            if !project_dir.exists() {
                return Err(anyhow!(
                    "manual project path does not exist: {}",
                    manual_project_dir.display()
                ));
            }
            if !project_dir.is_dir() {
                return Err(anyhow!(
                    "manual project path is not a directory: {}",
                    manual_project_dir.display()
                ));
            }

            let normalized = Self::normalize_project_path(&project_dir);
            if !seen_paths.insert(normalized) {
                continue;
            }

            let context = ProjectContext::load(&project_dir).with_context(|| {
                format!(
                    "failed loading manual project from {}",
                    manual_project_dir.display()
                )
            })?;
            Self::insert_project(&mut projects, context)?;
        }

        Ok(projects)
    }

    fn normalize_project_path(path: &Path) -> PathBuf {
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    fn insert_project(
        projects: &mut HashMap<String, ProjectContext>,
        context: ProjectContext,
    ) -> Result<()> {
        if let Some(existing) = projects.get(&context.name) {
            if existing.project_dir != context.project_dir {
                return Err(anyhow!(
                    "duplicate project name '{}' for '{}' and '{}'",
                    context.name,
                    existing.project_dir.display(),
                    context.project_dir.display()
                ));
            }
            return Ok(());
        }
        projects.insert(context.name.clone(), context);
        Ok(())
    }

    pub fn projects_dir(&self) -> &Path {
        &self.projects_dir
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
        *self = Self::discover(&self.projects_dir, &self.manual_project_dirs)?;
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
        self.project.ensure_chief_yaml_exists_for_run()?;
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

        let flow = build_flow(
            flow_kind,
            self.project.chief_yaml.chief.max_loop_iterations,
            self.project.chief_yaml.chief.required_stable_iterations,
        );
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
            chief_config: &self.project.chief_yaml.chief,
            all_suites: &self.project.chief_yaml.suites,
            todo,
            cancel_signal,
            prepared_suites: RefCell::new(std::collections::BTreeSet::new()),
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
        let todo_id = todo.id.clone();
        retry_with_policy_and_hook_and_delay(
            max_retries,
            |attempt, total| {
                if cancel_signal.load(Ordering::SeqCst) {
                    return Err(OrchestratorError::unrecoverable(anyhow!(
                        AgentCancelledError
                    )));
                }

                if attempt > 1 {
                    self.log_runtime_event(
                        run_id,
                        Some(job_id),
                        Some(&todo_id),
                        "info",
                        Some(Phase::Red),
                        EventType::PhaseChange,
                        format!("Retry loop {attempt}/{total} started; restarting RED phase"),
                        BTreeMap::new(),
                    );

                    match self.reset_retry_workspace(&work_dir) {
                        Ok(changed_files) => {
                            if !changed_files.is_empty() {
                                let mut payload = BTreeMap::new();
                                payload.insert(
                                    "files".to_owned(),
                                    serde_json::Value::Array(
                                        changed_files
                                            .iter()
                                            .cloned()
                                            .map(serde_json::Value::String)
                                            .collect(),
                                    ),
                                );
                                self.log_runtime_event(
                                    run_id,
                                    Some(job_id),
                                    Some(&todo_id),
                                    "warning",
                                    Some(Phase::Red),
                                    EventType::GitOp,
                                    format!(
                                        "Retry cleanup: discarded local git changes before loop {attempt}/{total}"
                                    ),
                                    payload,
                                );
                            }
                        }
                        Err(err) => {
                            let mut payload = BTreeMap::new();
                            payload.insert(
                                "error".to_owned(),
                                serde_json::Value::String(err.to_string()),
                            );
                            self.log_runtime_event(
                                run_id,
                                Some(job_id),
                                Some(&todo_id),
                                "warning",
                                Some(Phase::Red),
                                EventType::Error,
                                format!("Retry cleanup failed before loop {attempt}/{total}"),
                                payload,
                            );
                            return Err(self.classify_runtime_error(err));
                        }
                    }
                }

                let outcome = self.run_single_todo_once(
                    run_id,
                    job_id,
                    worker_index,
                    todo.clone(),
                    flow_kind,
                    work_dir.clone(),
                    model_override.clone(),
                    cancel_signal.clone(),
                );

                let Err(OrchestratorError::Retryable(initial_error)) = outcome else {
                    return outcome;
                };
                if !is_transient_lock_contention_error(&initial_error) {
                    return Err(OrchestratorError::retryable(initial_error));
                }

                retry_transient_lock_contention_with_delay(
                    initial_error,
                    || {
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
                    |retry_attempt, retry_total, err, delay| {
                        let mut payload = BTreeMap::new();
                        payload.insert(
                            "attempt".to_owned(),
                            serde_json::Value::from(retry_attempt as i64),
                        );
                        payload.insert(
                            "total".to_owned(),
                            serde_json::Value::from(retry_total as i64),
                        );
                        payload.insert(
                            "delay_seconds".to_owned(),
                            serde_json::Value::from(delay.as_secs() as i64),
                        );
                        payload.insert(
                            "error".to_owned(),
                            serde_json::Value::String(err.to_string()),
                        );
                        self.log_runtime_event(
                            run_id,
                            Some(job_id),
                            Some(&todo_id),
                            "warning",
                            Some(Phase::Red),
                            EventType::PhaseChange,
                            format!(
                                "Transient lock/contention retry {retry_attempt}/{retry_total} scheduled in {}s",
                                delay.as_secs()
                            ),
                            payload,
                        );
                    },
                    std::thread::sleep,
                )
            },
            |_attempt, _total, err| {
                if is_transient_lock_contention_error(err) {
                    return None;
                }
                Some(Duration::ZERO)
            },
            |attempt, total, err, _delay| {
                let mut payload = BTreeMap::new();
                payload.insert(
                    "attempt".to_owned(),
                    serde_json::Value::from(attempt as i64),
                );
                payload.insert("total".to_owned(), serde_json::Value::from(total as i64));
                payload.insert(
                    "error".to_owned(),
                    serde_json::Value::String(err.to_string()),
                );
                self.log_runtime_event(
                    run_id,
                    Some(job_id),
                    Some(&todo_id),
                    "warning",
                    Some(Phase::Red),
                    EventType::PhaseChange,
                    format!(
                        "Retry loop {attempt}/{total} finished with recoverable failure; preparing retry loop {}/{}",
                        attempt + 1,
                        total
                    ),
                    payload,
                );
                on_retry(attempt, total, err);
            },
            |_delay| {},
        )
    }

    fn run_next_todo_in_run_with_retry_hook<FR>(
        &self,
        run_id: &str,
        flow_kind: FlowKind,
        model_override: Option<String>,
        max_retries: usize,
        on_retry: &mut FR,
    ) -> OrchestratorResult<Option<TodoOutcome>>
    where
        FR: FnMut(usize, usize, &anyhow::Error),
    {
        let Some(todo) = self
            .project
            .claim_next_pending_todo()
            .map_err(|err| self.classify_runtime_error(err))?
        else {
            return Ok(None);
        };

        let mut job = self
            .project
            .create_job(run_id, 1, flow_kind, Some(todo.id.clone()), None)
            .map_err(|err| self.classify_runtime_error(err))?;
        job = self
            .project
            .set_job_status(job, JobStatus::Running, None)
            .context("failed to set job status running")
            .map_err(|err| self.classify_runtime_error(err))?;

        let main_branch = self
            .project
            .git
            .current_branch()
            .unwrap_or_else(|_| "main".to_owned());
        let worktree_root =
            worktree_root_for_project(&self.project.project_dir, &self.project.name);
        fs::create_dir_all(&worktree_root)
            .context("failed to create worktree root directory")
            .map_err(|err| self.classify_runtime_error(err))?;
        let branch = format!("chief/{}/{}", self.project.name, job.id);
        let work_dir = worktree_root.join(worker_worktree_dir_name(&job.id));
        self.project
            .git
            .create_worktree(&branch, &work_dir)
            .context("failed to create worker worktree")
            .map_err(|err| self.classify_runtime_error(err))?;

        let mut updated_job = job.clone();
        updated_job.worktree_path = Some(work_dir.display().to_string());
        if let Err(err) = self.project.store.upsert_job(&updated_job) {
            self.log_state_update_error(
                run_id,
                Some(&job.id),
                Some(&todo.id),
                "failed to persist worker worktree path",
                &err,
            );
        }
        job = updated_job;

        match self.run_single_todo_with_retries(
            run_id,
            &job.id,
            1,
            todo.clone(),
            flow_kind,
            work_dir.clone(),
            model_override,
            Arc::new(AtomicBool::new(false)),
            max_retries.max(1),
            |attempt, total, err| on_retry(attempt, total, err),
        ) {
            Ok(outcome) => {
                if let Err(err) = self
                    .project
                    .git
                    .merge_branch_into_main(&branch, &main_branch)
                    .and_then(|_| self.project.git.remove_worktree(&work_dir, &branch))
                {
                    let err_for_status = err.to_string();
                    if let Err(status_err) =
                        self.project
                            .store
                            .update_todo_status(&todo.id, TodoStatus::Pending, None)
                    {
                        self.log_state_update_error(
                            run_id,
                            Some(&job.id),
                            Some(&todo.id),
                            "failed to mark todo pending after merge error",
                            &status_err,
                        );
                    }
                    if let Err(status_err) = self.project.set_job_status(
                        job,
                        JobStatus::Failed,
                        Some(err_for_status.clone()),
                    ) {
                        self.log_state_update_error(
                            run_id,
                            None,
                            Some(&todo.id),
                            "failed to update job status to failed after merge error",
                            &status_err,
                        );
                    }
                    return Err(self.classify_runtime_error(err));
                }

                if let Some(commit_hash) = outcome.commit_hash.as_deref()
                    && let Err(err) = self.project.store.update_todo_status(
                        &todo.id,
                        TodoStatus::Done,
                        Some(commit_hash),
                    )
                {
                    self.log_state_update_error(
                        run_id,
                        Some(&job.id),
                        Some(&todo.id),
                        "failed to mark todo done",
                        &err,
                    );
                }
                if let Err(err) = self.project.set_job_status(job, JobStatus::Completed, None) {
                    self.log_state_update_error(
                        run_id,
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
                        .update_todo_status(&todo.id, TodoStatus::Pending, None)
                {
                    self.log_state_update_error(
                        run_id,
                        Some(&job.id),
                        Some(&todo.id),
                        "failed to mark todo pending after worker failure",
                        &status_err,
                    );
                }
                if let Err(remove_err) = self.project.git.remove_worktree(&work_dir, &branch) {
                    self.log_state_update_error(
                        run_id,
                        Some(&job.id),
                        Some(&todo.id),
                        "failed to cleanup worker worktree",
                        &remove_err,
                    );
                }
                if let Err(status_err) =
                    self.project
                        .set_job_status(job, JobStatus::Failed, Some(err.to_string()))
                {
                    self.log_state_update_error(
                        run_id,
                        None,
                        Some(&todo.id),
                        "failed to update job status to failed",
                        &status_err,
                    );
                }
                Err(err)
            }
        }
    }

    fn run_next_todo_once_with_retry_hook<FR>(
        &self,
        flow_kind: FlowKind,
        model_override: Option<String>,
        max_retries: usize,
        on_retry: &mut FR,
    ) -> OrchestratorResult<Option<TodoOutcome>>
    where
        FR: FnMut(usize, usize, &anyhow::Error),
    {
        let run_id = self
            .start_run()
            .map_err(|err| self.classify_runtime_error(err))?;

        let result = self.run_next_todo_in_run_with_retry_hook(
            &run_id,
            flow_kind,
            model_override,
            max_retries,
            on_retry,
        );

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

    pub fn run_next_todo_once(
        &self,
        flow_kind: FlowKind,
        model_override: Option<String>,
    ) -> OrchestratorResult<Option<TodoOutcome>> {
        self.run_next_todo_once_with_retry_hook(
            flow_kind,
            model_override,
            self.project.chief_yaml.chief.max_retries.max(1),
            &mut |_attempt, _total, _err| {},
        )
    }

    pub fn run_next_todo(
        &self,
        flow_kind: FlowKind,
        model_override: Option<String>,
    ) -> Result<Option<TodoOutcome>> {
        self.run_next_todo_once(flow_kind, model_override)
            .map_err(OrchestratorError::into_error)
    }

    fn run_todo_queue_with_runner<FC, FR, FN>(
        &self,
        run_id: &str,
        flow_kind: FlowKind,
        model_override: Option<String>,
        max_retries: usize,
        on_todo_completed: &mut FC,
        on_retry: &mut FR,
        mut run_next_todo: FN,
    ) -> OrchestratorResult<()>
    where
        FC: FnMut(&TodoOutcome),
        FR: FnMut(usize, usize, &anyhow::Error),
        FN: FnMut(
            &str,
            FlowKind,
            Option<String>,
            usize,
            &mut FR,
        ) -> OrchestratorResult<Option<TodoOutcome>>,
    {
        loop {
            let next = run_next_todo(
                run_id,
                flow_kind,
                model_override.clone(),
                max_retries.max(1),
                on_retry,
            )?;

            let Some(outcome) = next else {
                return Ok(());
            };
            on_todo_completed(&outcome);
        }
    }

    fn run_todos_until_done_with_retries_with_runner<FC, FR, FN>(
        &self,
        flow_kind: FlowKind,
        model_override: Option<String>,
        max_retries: usize,
        mut on_todo_completed: FC,
        mut on_retry: FR,
        mut run_next_todo: FN,
    ) -> OrchestratorResult<()>
    where
        FC: FnMut(&TodoOutcome),
        FR: FnMut(usize, usize, &anyhow::Error),
        FN: FnMut(
            &str,
            FlowKind,
            Option<String>,
            usize,
            &mut FR,
        ) -> OrchestratorResult<Option<TodoOutcome>>,
    {
        let run_id = self
            .start_run()
            .map_err(|err| self.classify_runtime_error(err))?;

        let result = self.run_todo_queue_with_runner(
            &run_id,
            flow_kind,
            model_override,
            max_retries,
            &mut on_todo_completed,
            &mut on_retry,
            |runner_run_id,
             runner_flow_kind,
             runner_model_override,
             runner_max_retries,
             runner_on_retry| {
                run_next_todo(
                    runner_run_id,
                    runner_flow_kind,
                    runner_model_override,
                    runner_max_retries,
                    runner_on_retry,
                )
            },
        );

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

    pub fn run_todos_until_done_with_retries<FC, FR>(
        &self,
        flow_kind: FlowKind,
        model_override: Option<String>,
        max_retries: usize,
        on_todo_completed: FC,
        on_retry: FR,
    ) -> OrchestratorResult<()>
    where
        FC: FnMut(&TodoOutcome),
        FR: FnMut(usize, usize, &anyhow::Error),
    {
        self.run_todos_until_done_with_retries_with_runner(
            flow_kind,
            model_override,
            max_retries,
            on_todo_completed,
            on_retry,
            |run_id, flow_kind, model_override, max_retries, retry_hook| {
                self.run_next_todo_in_run_with_retry_hook(
                    run_id,
                    flow_kind,
                    model_override,
                    max_retries,
                    retry_hook,
                )
            },
        )
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
            self.log_runtime_event(
                &run_id,
                None,
                None,
                "info",
                None,
                EventType::AgentPrompt,
                "Agent prompt (requirements)",
                payload_from_json(serde_json::json!({
                    "prompt": &prompt,
                })),
            );
            let response = match agent.run(crate::agent::AgentRequest {
                prompt,
                cwd: self.project.project_dir.clone(),
                timeout_seconds: Some(self.project.chief_yaml.chief.agent_timeout_seconds),
                disallowed_paths: Vec::new(),
                cancel_signal: None,
                on_chunk: None,
            }) {
                Ok(response) => response,
                Err(err) => {
                    self.log_runtime_event(
                        &run_id,
                        None,
                        None,
                        "error",
                        None,
                        EventType::Error,
                        "Agent execution failed during requirements processing",
                        payload_from_json(serde_json::json!({
                            "error": err.to_string(),
                        })),
                    );
                    return Err(err);
                }
            };

            self.log_runtime_event(
                &run_id,
                None,
                None,
                if response.exit_code == 0 {
                    "info"
                } else {
                    "warning"
                },
                None,
                EventType::AgentResponse,
                "Agent response (requirements)",
                payload_from_json(serde_json::json!({
                    "exit_code": response.exit_code,
                    "command": &response.command,
                    "output": &response.merged_output,
                    "stdout": &response.stdout,
                    "stderr": &response.stderr,
                })),
            );

            if response.exit_code != 0 {
                return Err(anyhow!(
                    "requirements processing failed (exit code {}): {}",
                    response.exit_code,
                    response.merged_output
                ));
            }

            self.project
                .store
                .sync_todos_from_file()
                .context("failed syncing todo DB from todos.yaml after requirements processing")?;

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

    fn log_runtime_event(
        &self,
        run_id: &str,
        job_id: Option<&str>,
        todo_id: Option<&str>,
        level: &str,
        phase: Option<Phase>,
        event_type: EventType,
        msg: impl Into<String>,
        payload: BTreeMap<String, serde_json::Value>,
    ) {
        if let Err(log_err) = self.project.log_project_event(
            run_id,
            job_id.map(str::to_owned),
            todo_id.map(str::to_owned),
            level,
            phase,
            event_type,
            msg,
            payload,
        ) {
            warn!("failed to record runtime event: {log_err:#}");
        }
    }

    fn reset_retry_workspace(&self, work_dir: &Path) -> Result<Vec<String>> {
        let changed_files = self.project.git.changed_files(work_dir)?;
        if changed_files.is_empty() {
            return Ok(Vec::new());
        }

        self.run_git_command(work_dir, &["reset", "--hard", "HEAD"])?;
        self.run_git_command(work_dir, &["clean", "-fd"])?;
        Ok(changed_files)
    }

    fn run_git_command(&self, cwd: &Path, args: &[&str]) -> Result<()> {
        let output = run_git_command_with_retry(cwd, args)
            .with_context(|| format!("failed to run git {}", args.join(" ")))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if git_output_has_transient_lock_contention_signature(&output) {
                return Err(anyhow!(
                    "transient lock/contention retry budget exhausted after {GIT_TRANSIENT_LOCK_RETRY_ATTEMPTS} retries: git {} failed in {}: {}",
                    args.join(" "),
                    cwd.display(),
                    detail
                ));
            }
            return Err(anyhow!(
                "git {} failed in {}: {}",
                args.join(" "),
                cwd.display(),
                detail
            ));
        }
        Ok(())
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

fn worktree_root_for_project(project_dir: &Path, project_name: &str) -> PathBuf {
    let parent_dir = project_dir.parent().unwrap_or(project_dir);
    parent_dir.join(format!("{project_name}__worktrees"))
}

fn worker_worktree_dir_name(job_id: &str) -> String {
    format!("chief_{job_id}")
}

fn retry_transient_lock_contention_with_delay<T, F, H, S>(
    initial_error: anyhow::Error,
    mut operation: F,
    mut on_retry: H,
    sleep: S,
) -> OrchestratorResult<T>
where
    F: FnMut() -> OrchestratorResult<T>,
    H: FnMut(usize, usize, &anyhow::Error, Duration),
    S: FnMut(Duration),
{
    let mut first_error = Some(initial_error);
    let outcome = retry_with_policy_and_hook_and_delay(
        TRANSIENT_LOCK_MAX_ATTEMPTS,
        |_attempt, _total| {
            if let Some(err) = first_error.take() {
                Err(OrchestratorError::retryable(err))
            } else {
                operation()
            }
        },
        |_attempt, _total, err| {
            if is_transient_lock_contention_error(err) {
                Some(TRANSIENT_LOCK_RETRY_DELAY)
            } else {
                None
            }
        },
        |attempt, _total, err, delay| {
            on_retry(attempt, TRANSIENT_LOCK_RETRY_ATTEMPTS, err, delay);
        },
        sleep,
    );

    match outcome {
        Err(OrchestratorError::Retryable(err)) if is_transient_lock_contention_error(&err) => {
            let detail = err.to_string();
            Err(OrchestratorError::unrecoverable(anyhow!(
                "transient lock/contention retry budget exhausted after {TRANSIENT_LOCK_RETRY_ATTEMPTS} retries: {detail}"
            )))
        }
        other => other,
    }
}

fn is_transient_lock_contention_error(err: &anyhow::Error) -> bool {
    if has_transient_lock_contention_signature(&err.to_string()) {
        return true;
    }

    for cause in err.chain() {
        if let Some(io_err) = cause.downcast_ref::<io::Error>()
            && matches!(
                io_err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            )
        {
            return true;
        }

        if has_transient_lock_contention_signature(&cause.to_string()) {
            return true;
        }
    }

    false
}

fn is_known_unrecoverable_error(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(io_err) = cause.downcast_ref::<io::Error>()
            && matches!(
                io_err.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::NotFound
                    | io::ErrorKind::ReadOnlyFilesystem
            )
        {
            return true;
        }

        if let Some(sqlite_err) = cause.downcast_ref::<rusqlite::Error>()
            && is_unrecoverable_sqlite_error(sqlite_err)
        {
            return true;
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

#[cfg(test)]
mod tests {
    use super::{
        ChiefEngine, ProjectContext, ProjectRegistry, is_transient_lock_contention_error,
        retry_transient_lock_contention_with_delay,
    };
    use crate::domain::{RunExitStatus, Todo, TodoStatus};
    use crate::flow::{FlowKind, TodoOutcome};
    use crate::orchestrator::OrchestratorError;
    use anyhow::anyhow;
    use rusqlite::Connection;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use uuid::Uuid;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("chief-project-registry-{label}-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("failed creating temporary directory");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn init_git_repo(path: &Path) {
        fs::create_dir_all(path).expect("failed creating git repo directory");
        let output = Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(path)
            .output()
            .expect("failed to run git init");
        assert!(
            output.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn pending_todo(text: &str) -> Todo {
        Todo {
            id: String::new(),
            todo: text.to_owned(),
            expectations: String::new(),
            priority: 1,
            test_suites: Vec::new(),
            status: TodoStatus::Pending,
            done_at_commit: None,
        }
        .normalize()
    }

    #[test]
    fn chief_engine_start_run_requires_chief_yaml() {
        let root = TempDir::new("missing-chief-yaml");
        let project_dir = root.path.join("project");
        init_git_repo(&project_dir);
        let context = ProjectContext::load(&project_dir).expect("project context should load");
        assert!(
            !context.config_path.exists(),
            "fixture should intentionally omit chief.yaml"
        );
        assert!(
            !context.store.db_path.exists(),
            "chief.db should not exist before start_run"
        );

        let err = ChiefEngine::new(context.clone())
            .start_run()
            .expect_err("start_run should fail without chief.yaml");
        let rendered = err.to_string();
        assert!(
            rendered.contains("missing required chief config"),
            "error should explain missing config: {rendered}"
        );
        assert!(
            rendered.contains("chief.yaml"),
            "error should reference chief.yaml path: {rendered}"
        );
        assert!(
            !context.store.db_path.exists(),
            "rejected start_run should not create chief.db"
        );
    }

    #[test]
    fn worker_worktree_dir_name_uses_chief_prefix() {
        assert_eq!(
            super::worker_worktree_dir_name("abc-123"),
            "chief_abc-123".to_owned()
        );
    }

    #[test]
    fn discover_merges_projects_dir_and_manual_projects() {
        let projects_root = TempDir::new("projects-root");
        let manual_root = TempDir::new("manual-root");

        let in_tree = projects_root.path.join("in-tree");
        let ignored = projects_root.path.join("not-a-repo");
        let manual = manual_root.path.join("manual-repo");
        init_git_repo(&in_tree);
        fs::create_dir_all(&ignored).expect("failed creating non-repo directory");
        init_git_repo(&manual);

        let registry =
            ProjectRegistry::discover(&projects_root.path, std::slice::from_ref(&manual))
                .expect("project discovery should succeed");
        let names = registry
            .list_projects()
            .into_iter()
            .map(|project| project.name)
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["in-tree".to_owned(), "manual-repo".to_owned()]);
    }

    #[test]
    fn discover_dedupes_manual_project_already_in_projects_dir() {
        let projects_root = TempDir::new("dedupe-root");
        let shared = projects_root.path.join("shared");
        init_git_repo(&shared);

        let registry =
            ProjectRegistry::discover(&projects_root.path, std::slice::from_ref(&shared))
                .expect("project discovery should succeed");
        let names = registry
            .list_projects()
            .into_iter()
            .map(|project| project.name)
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["shared".to_owned()]);
    }

    #[test]
    fn discover_errors_on_duplicate_project_names() {
        let projects_root = TempDir::new("dupe-names-root");
        let manual_root = TempDir::new("dupe-names-manual");

        let in_tree = projects_root.path.join("same-name");
        let manual = manual_root.path.join("same-name");
        init_git_repo(&in_tree);
        init_git_repo(&manual);

        let err = ProjectRegistry::discover(&projects_root.path, std::slice::from_ref(&manual))
            .expect_err("discovery should fail for duplicate project names");
        assert!(
            err.to_string().contains("duplicate project name"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn run_todos_until_done_with_retries_stops_after_first_terminal_todo_failure() {
        let root = TempDir::new("cli-fail-fast");
        let project_dir = root.path.join("project");
        init_git_repo(&project_dir);
        fs::write(project_dir.join("chief.yaml"), "chief: {}\n")
            .expect("failed to write chief.yaml fixture");

        let context = ProjectContext::load(&project_dir).expect("failed to load project context");
        let first = context
            .store
            .append_todo(pending_todo("first todo"))
            .expect("failed to append first todo");
        let second = context
            .store
            .append_todo(pending_todo("second todo"))
            .expect("failed to append second todo");

        let engine = ChiefEngine::new(context.clone());
        let mut runner_calls = 0usize;
        let mut completed_ids = Vec::new();

        let result = engine.run_todos_until_done_with_retries_with_runner(
            FlowKind::SinglePrompt,
            None,
            3,
            |outcome: &TodoOutcome| completed_ids.push(outcome.todo_id.clone()),
            |_attempt, _total, _err| {},
            |_run_id, _flow_kind, _model_override, _max_retries, _retry_hook| {
                runner_calls += 1;
                Err(OrchestratorError::retryable(anyhow!(
                    "simulated terminal todo failure"
                )))
            },
        );

        assert!(
            matches!(result, Err(OrchestratorError::Retryable(_))),
            "todo queue should fail on the first terminal todo failure"
        );
        assert_eq!(
            runner_calls, 1,
            "CLI todo queue should stop immediately instead of trying another todo"
        );
        assert!(
            completed_ids.is_empty(),
            "no todo completion callback should fire on immediate terminal failure"
        );

        let todos = context.store.list_todos().expect("failed to list todos");
        let pending_ids = todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::Pending)
            .map(|todo| todo.id.clone())
            .collect::<Vec<_>>();
        assert!(
            pending_ids.contains(&first.id),
            "first todo should remain pending after terminal failure"
        );
        assert!(
            pending_ids.contains(&second.id),
            "second todo should never be picked once first todo fails terminally"
        );

        let conn = Connection::open(&context.store.db_path).expect("failed to open chief.db");
        let (run_status, run_exit_status): (String, Option<String>) = conn
            .query_row(
                "SELECT status, exit_status FROM runs ORDER BY started_at DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("failed to query latest run");
        assert_eq!(run_status, "finished");
        assert_eq!(
            run_exit_status.as_deref(),
            Some(RunExitStatus::Failure.as_str()),
            "CLI run should finish with failure after terminal todo failure"
        );
    }

    #[test]
    fn transient_lock_contention_signature_is_detected() {
        let err = anyhow!(
            "git commit failed: Unable to create '/tmp/repo/.git/index.lock': File exists.\nAnother git process seems to be running in this repository"
        );
        assert!(is_transient_lock_contention_error(&err));
    }

    #[test]
    fn transient_lock_contention_io_error_kinds_are_detected() {
        let would_block = anyhow!(io::Error::new(io::ErrorKind::WouldBlock, "would block"));
        let timed_out = anyhow!(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
        let interrupted = anyhow!(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));

        assert!(is_transient_lock_contention_error(&would_block));
        assert!(is_transient_lock_contention_error(&timed_out));
        assert!(is_transient_lock_contention_error(&interrupted));
    }

    #[test]
    fn transient_lock_retry_path_retries_three_times_with_ten_second_delays() {
        let mut operation_calls = 0usize;
        let mut retry_callbacks = Vec::new();
        let mut sleeps = Vec::new();
        let err = retry_transient_lock_contention_with_delay::<(), _, _, _>(
            anyhow!(
                "git command failed: Unable to create '/tmp/repo/.git/index.lock': File exists.\nAnother git process seems to be running in this repository"
            ),
            || {
                operation_calls += 1;
                Err(OrchestratorError::retryable(anyhow!(
                    "git command failed: Unable to create '/tmp/repo/.git/index.lock': File exists.\nAnother git process seems to be running in this repository"
                )))
            },
            |attempt, total, _err, delay| {
                retry_callbacks.push((attempt, total, delay.as_secs()));
            },
            |delay| sleeps.push(delay.as_secs()),
        )
        .expect_err("transient lock retries should eventually fail");

        assert!(matches!(err, OrchestratorError::Unrecoverable(_)));
        let rendered = err.as_error().to_string();
        assert!(
            rendered.contains("retry budget exhausted"),
            "terminal lock retry failure should mention exhausted retry budget: {rendered}"
        );
        assert!(
            rendered.contains(".git/index.lock"),
            "terminal lock retry failure should preserve index.lock details: {rendered}"
        );
        assert!(
            rendered
                .to_ascii_lowercase()
                .contains("another git process seems to be running"),
            "terminal lock retry failure should preserve conflict hint: {rendered}"
        );
        assert_eq!(
            operation_calls, 3,
            "exactly three retry executions expected"
        );
        assert_eq!(
            retry_callbacks,
            vec![(1, 3, 10), (2, 3, 10), (3, 3, 10)],
            "retry callbacks should report attempt counters and 10-second delays"
        );
        assert_eq!(
            sleeps,
            vec![10, 10, 10],
            "sleep should be invoked between retries"
        );
    }

    #[test]
    fn transient_io_retry_path_retries_three_times_with_ten_second_delays() {
        let mut operation_calls = 0usize;
        let mut retry_callbacks = Vec::new();
        let mut sleeps = Vec::new();
        let err = retry_transient_lock_contention_with_delay::<(), _, _, _>(
            anyhow!(io::Error::new(io::ErrorKind::WouldBlock, "index.lock busy")),
            || {
                operation_calls += 1;
                Err(OrchestratorError::retryable(anyhow!(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "git index lock timed out",
                ))))
            },
            |attempt, total, _err, delay| {
                retry_callbacks.push((attempt, total, delay.as_secs()));
            },
            |delay| sleeps.push(delay.as_secs()),
        )
        .expect_err("transient io retries should eventually fail");

        assert!(matches!(err, OrchestratorError::Unrecoverable(_)));
        let rendered = err.as_error().to_string();
        assert!(
            rendered.contains("retry budget exhausted"),
            "terminal io retry failure should mention exhausted retry budget: {rendered}"
        );
        assert!(
            rendered.to_ascii_lowercase().contains("timed out"),
            "terminal io retry failure should preserve final io details: {rendered}"
        );
        assert_eq!(
            operation_calls, 3,
            "exactly three retry executions expected"
        );
        assert_eq!(
            retry_callbacks,
            vec![(1, 3, 10), (2, 3, 10), (3, 3, 10)],
            "retry callbacks should report attempt counters and 10-second delays"
        );
        assert_eq!(
            sleeps,
            vec![10, 10, 10],
            "sleep should be invoked between retries"
        );
    }

    #[test]
    fn transient_lock_retry_path_can_succeed_after_retries() {
        let mut operation_calls = 0usize;
        let mut sleeps = Vec::new();
        let outcome = retry_transient_lock_contention_with_delay(
            anyhow!(
                "git command failed: Unable to create '/tmp/repo/.git/index.lock': File exists.\nAnother git process seems to be running in this repository"
            ),
            || {
                operation_calls += 1;
                if operation_calls < 2 {
                    Err(OrchestratorError::retryable(anyhow!(
                        "git command failed: Unable to create '/tmp/repo/.git/index.lock': File exists.\nAnother git process seems to be running in this repository"
                    )))
                } else {
                    Ok("ok")
                }
            },
            |_attempt, _total, _err, _delay| {},
            |delay| sleeps.push(delay.as_secs()),
        )
        .expect("transient lock retry should recover");

        assert_eq!(outcome, "ok");
        assert_eq!(operation_calls, 2);
        assert_eq!(sleeps, vec![10, 10]);
    }

    #[test]
    fn non_matching_runtime_failure_is_not_classified_as_transient_lock_contention() {
        let err = anyhow!("git merge failed: conflict in working tree");
        assert!(!is_transient_lock_contention_error(&err));

        let mut operation_calls = 0usize;
        let mut retry_callbacks = 0usize;
        let mut sleeps = Vec::new();
        let result = retry_transient_lock_contention_with_delay::<(), _, _, _>(
            err,
            || {
                operation_calls += 1;
                Ok(())
            },
            |_attempt, _total, _err, _delay| retry_callbacks += 1,
            |delay| sleeps.push(delay.as_secs()),
        );

        assert!(matches!(result, Err(OrchestratorError::Retryable(_))));
        assert_eq!(
            operation_calls, 0,
            "non-transient errors should not enter lock retry path"
        );
        assert_eq!(retry_callbacks, 0);
        assert!(sleeps.is_empty());
    }
}
