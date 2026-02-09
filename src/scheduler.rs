#[path = "scheduler/selector.rs"]
mod selector;
#[path = "scheduler/supervisor.rs"]
mod supervisor;
#[path = "scheduler/worker.rs"]
mod worker;

use crate::flow::FlowKind;
use crate::service::{ProjectContext, ProjectRegistry};
use crate::{domain::EventType, domain::JobStatus};
use anyhow::{Result, anyhow};
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, RwLock};
use tracing::{error, warn};

#[derive(Debug, Clone, Serialize)]
pub struct ProjectRuntimeView {
    pub name: String,
    pub project_dir: String,
    pub desired_agents: usize,
    pub active_workers: usize,
    pub running: bool,
    pub flow_name: String,
    pub model_override: Option<String>,
    pub stop_requested: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerResult {
    pub job_id: String,
    pub todo_id: String,
    pub status: String,
    pub error: Option<String>,
    pub commit_hash: Option<String>,
    pub unrecoverable: bool,
}

#[derive(Debug)]
struct ProjectRuntime {
    desired_agents: usize,
    active_workers: usize,
    running: bool,
    stop_requested: bool,
    flow_kind: FlowKind,
    model_override: Option<String>,
    last_error: Option<String>,
    selection_lock: Arc<Mutex<()>>,
    merge_lock: Arc<Mutex<()>>,
    cancel_signal: Arc<AtomicBool>,
}

impl ProjectRuntime {
    fn new() -> Self {
        Self {
            desired_agents: 1,
            active_workers: 0,
            running: false,
            stop_requested: false,
            flow_kind: FlowKind::Tdd,
            model_override: None,
            last_error: None,
            selection_lock: Arc::new(Mutex::new(())),
            merge_lock: Arc::new(Mutex::new(())),
            cancel_signal: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[derive(Clone)]
pub struct Scheduler {
    registry: Arc<RwLock<ProjectRegistry>>,
    states: Arc<Mutex<HashMap<String, ProjectRuntime>>>,
    max_agents_per_project: usize,
}

impl Scheduler {
    pub fn new(registry: ProjectRegistry, max_agents_per_project: usize) -> Self {
        Self {
            registry: Arc::new(RwLock::new(registry)),
            states: Arc::new(Mutex::new(HashMap::new())),
            max_agents_per_project: max_agents_per_project.max(1),
        }
    }

    pub async fn refresh_registry(&self) -> Result<()> {
        let mut registry = self.registry.write().await;
        registry.reload()
    }

    pub async fn list_project_views(&self) -> Vec<ProjectRuntimeView> {
        let registry = self.registry.read().await;
        let projects = registry.list_projects();
        drop(registry);

        let states = self.states.lock().await;
        projects
            .into_iter()
            .map(|project| {
                let runtime = states.get(&project.name);
                ProjectRuntimeView {
                    name: project.name.clone(),
                    project_dir: project.project_dir.display().to_string(),
                    desired_agents: runtime.map(|v| v.desired_agents).unwrap_or(1),
                    active_workers: runtime.map(|v| v.active_workers).unwrap_or(0),
                    running: runtime.map(|v| v.running).unwrap_or(false),
                    flow_name: runtime
                        .map(|v| v.flow_kind.as_str().to_owned())
                        .unwrap_or_else(|| FlowKind::Tdd.as_str().to_owned()),
                    model_override: runtime.and_then(|v| v.model_override.clone()),
                    stop_requested: runtime.map(|v| v.stop_requested).unwrap_or(false),
                    last_error: runtime.and_then(|v| v.last_error.clone()),
                }
            })
            .collect()
    }

    pub async fn get_project_context(&self, project_name: &str) -> Result<ProjectContext> {
        let registry = self.registry.read().await;
        registry
            .get(project_name)
            .ok_or_else(|| anyhow!("project '{project_name}' not found"))
    }

    pub async fn start_project(
        &self,
        project_name: String,
        agents: usize,
        flow_kind: FlowKind,
        model_override: Option<String>,
    ) -> Result<()> {
        self.get_project_context(&project_name).await?;
        let desired_agents = agents.clamp(1, self.max_agents_per_project);

        let should_spawn = {
            let mut states = self.states.lock().await;
            let state = states
                .entry(project_name.clone())
                .or_insert_with(ProjectRuntime::new);
            state.desired_agents = desired_agents;
            state.flow_kind = flow_kind;
            state.model_override = model_override.clone();
            state.stop_requested = false;
            state.cancel_signal.store(false, Ordering::SeqCst);
            state.last_error = None;
            if state.running {
                false
            } else {
                state.running = true;
                true
            }
        };

        if should_spawn {
            let this = self.clone();
            tokio::spawn(async move {
                if let Err(err) = this.supervise_project(project_name.clone()).await {
                    error!(
                        project = %project_name,
                        error = %err,
                        "project supervisor exited with error"
                    );
                    let mut states = this.states.lock().await;
                    if let Some(state) = states.get_mut(&project_name) {
                        state.last_error = Some(err.to_string());
                        state.running = false;
                        state.active_workers = 0;
                    }
                }
            });
        }

        Ok(())
    }

    pub async fn stop_project(&self, project_name: &str) -> Result<()> {
        let should_log_stop_request = {
            let mut states = self.states.lock().await;
            let Some(state) = states.get_mut(project_name) else {
                return Err(anyhow!("project '{project_name}' is not running"));
            };
            let should_log = !state.stop_requested;
            state.stop_requested = true;
            state.cancel_signal.store(true, Ordering::SeqCst);
            should_log
        };

        if !should_log_stop_request {
            return Ok(());
        }

        let context = self.get_project_context(project_name).await?;
        let run_id = context
            .store
            .list_jobs(200)?
            .into_iter()
            .find(|job| {
                matches!(
                    job.status,
                    JobStatus::Queued
                        | JobStatus::Selecting
                        | JobStatus::Running
                        | JobStatus::Merging
                )
            })
            .map(|job| job.run_id);

        if let Some(run_id) = run_id {
            if let Err(err) = context.log_project_event(
                &run_id,
                None,
                None,
                "info",
                None,
                EventType::Job,
                format!("Stop requested for {project_name}; cancelling active work now"),
                BTreeMap::new(),
            ) {
                warn!(
                    project = %project_name,
                    error = %err,
                    "failed to log immediate stop-requested event"
                );
            }
        }

        Ok(())
    }
}
