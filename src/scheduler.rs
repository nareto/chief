#[path = "scheduler/selector.rs"]
mod selector;
#[path = "scheduler/supervisor.rs"]
mod supervisor;
#[path = "scheduler/worker.rs"]
mod worker;

use crate::flow::FlowKind;
use crate::service::{ProjectContext, ProjectRegistry};
use anyhow::{Result, anyhow};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

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
        }
    }
}

#[derive(Clone)]
pub struct Scheduler {
    registry: Arc<RwLock<ProjectRegistry>>,
    states: Arc<Mutex<HashMap<String, ProjectRuntime>>>,
}

impl Scheduler {
    pub fn new(registry: ProjectRegistry) -> Self {
        Self {
            registry: Arc::new(RwLock::new(registry)),
            states: Arc::new(Mutex::new(HashMap::new())),
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
        let context = self.get_project_context(&project_name).await?;
        let max_agents = context.chief_toml.backend.max_agents_per_project.max(1);
        let desired_agents = agents.clamp(1, max_agents);

        let should_spawn = {
            let mut states = self.states.lock().await;
            let state = states
                .entry(project_name.clone())
                .or_insert_with(ProjectRuntime::new);
            state.desired_agents = desired_agents;
            state.flow_kind = flow_kind;
            state.model_override = model_override.clone();
            state.stop_requested = false;
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
        let mut states = self.states.lock().await;
        let Some(state) = states.get_mut(project_name) else {
            return Err(anyhow!("project '{project_name}' is not running"));
        };
        state.stop_requested = true;
        Ok(())
    }
}
