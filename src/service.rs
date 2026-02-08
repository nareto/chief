use crate::agent::{CodingAgent, CommandAgent};
use crate::config::ChiefToml;
use crate::domain::{
    EventRecord, EventType, JobRecord, JobStatus, Phase, RunExitStatus, Todo, TodoStatus,
};
use crate::flow::{FlowExecution, FlowKind, TodoOutcome, build_flow};
use crate::git::{GitOps, ShellGitOps};
use crate::prompt::{FsPromptStore, PromptStore};
use crate::storage::ProjectStore;
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

        let prompts = FsPromptStore::new(project_dir.join("prompts"));
        prompts.ensure_default_templates()?;

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

    pub fn run_single_todo(
        &self,
        run_id: &str,
        job_id: &str,
        worker_index: usize,
        todo: Todo,
        flow_kind: FlowKind,
        work_dir: PathBuf,
        model_override: Option<String>,
    ) -> Result<TodoOutcome> {
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
        };

        flow.run_todo(&mut execution)
    }

    pub fn run_next_todo(
        &self,
        flow_kind: FlowKind,
        model_override: Option<String>,
    ) -> Result<Option<TodoOutcome>> {
        let run_id = self.start_run()?;

        let result = (|| -> Result<Option<TodoOutcome>> {
            let Some(next) = self.project.pick_next_todo_priority()? else {
                return Ok(None);
            };
            let Some(todo) = self.project.claim_todo(&next.id)? else {
                return Ok(None);
            };

            let mut job =
                self.project
                    .create_job(&run_id, 1, flow_kind, Some(todo.id.clone()), None)?;
            job = self
                .project
                .set_job_status(job, JobStatus::Running, None)
                .context("failed to set job status running")?;

            match self.run_single_todo(
                &run_id,
                &job.id,
                1,
                todo.clone(),
                flow_kind,
                self.project.project_dir.clone(),
                model_override,
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
            if result.is_ok() {
                RunExitStatus::Success
            } else {
                RunExitStatus::Failure
            },
        )?;

        result
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
