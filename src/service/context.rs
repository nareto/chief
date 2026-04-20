use crate::agent::{ClaudeAgent, CodexAgent, CodingAgent, CursorAgent, OpencodeAgent};
use crate::config::ChiefYaml;
use crate::domain::{EventRecord, EventType, JobRecord, JobStatus, Phase, Todo};
use crate::flow::FlowKind;
use crate::git::ShellGitOps;
use crate::paths;
use crate::prompt::FsPromptStore;
use crate::storage::ProjectStore;
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

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
        let config_path = paths::chief_yaml_path(&project_dir);
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
        Ok(())
    }

    pub fn ensure_chief_yaml_exists_for_run(&self) -> Result<()> {
        if self.config_path.is_file() {
            return Ok(());
        }
        Err(anyhow!(
            "missing required chief config at {}. create .chief/chief.yaml (run `chief init` or copy .chief/chief.example.yaml)",
            self.config_path.display()
        ))
    }

    pub fn build_agent(&self, model_override: Option<String>) -> Arc<dyn CodingAgent> {
        let agent_name = self.chief_yaml.chief.agent.to_lowercase();
        match agent_name.as_str() {
            "claude" => Arc::new(ClaudeAgent::from_config(
                &self.chief_yaml.chief,
                model_override,
            )),
            "opencode" => Arc::new(OpencodeAgent::from_config(
                &self.chief_yaml.chief,
                model_override,
            )),
            "cursor" | "cursor-agent" => Arc::new(CursorAgent::from_config(
                &self.chief_yaml.chief,
                model_override,
            )),
            _ => Arc::new(CodexAgent::from_config(
                &self.chief_yaml.chief,
                model_override,
            )),
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
