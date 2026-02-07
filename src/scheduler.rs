use crate::agent::AgentRequest;
use crate::domain::{JobRecord, JobStatus, RunExitStatus, Todo, TodoStatus};
use crate::git::GitOps;
use crate::prompt::PromptStore;
use crate::service::{ChiefEngine, ProjectContext, ProjectRegistry};
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinSet;
use tokio::time::{Duration, sleep};

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
    flow_name: String,
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
            flow_name: "tdd".to_owned(),
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
                        .map(|v| v.flow_name.clone())
                        .unwrap_or_else(|| "tdd".to_owned()),
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
        flow_name: String,
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
            state.flow_name = flow_name.clone();
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

    async fn supervise_project(&self, project_name: String) -> Result<()> {
        let mut context = self.get_project_context(&project_name).await?;
        context.refresh()?;
        let engine = ChiefEngine::new(context.clone());

        let run_id = engine.start_run()?;
        context.log_project_event(
            &run_id,
            None,
            None,
            "info",
            None,
            crate::domain::EventType::Job,
            format!("Starting project supervisor for {project_name}"),
            std::collections::BTreeMap::new(),
        )?;

        let mut workers: JoinSet<WorkerResult> = JoinSet::new();
        let mut spawn_count = 0usize;
        let mut any_failure = false;

        loop {
            let (
                desired_agents,
                flow_name,
                model_override,
                stop_requested,
                selection_lock,
                merge_lock,
            ) = {
                let states = self.states.lock().await;
                let Some(state) = states.get(&project_name) else {
                    break;
                };
                (
                    state.desired_agents,
                    state.flow_name.clone(),
                    state.model_override.clone(),
                    state.stop_requested,
                    state.selection_lock.clone(),
                    state.merge_lock.clone(),
                )
            };

            while workers.len() < desired_agents && !stop_requested {
                let _selection_guard = selection_lock.lock().await;
                let available = context.store.list_available_todos()?;
                if available.is_empty() {
                    break;
                }

                let in_progress = context.store.list_in_progress_todos()?;
                spawn_count += 1;
                let selected_id = select_todo_id(
                    &context,
                    spawn_count,
                    &available,
                    &in_progress,
                    model_override.clone(),
                )
                .await
                .unwrap_or_else(|_| available[0].id.clone());

                let Some(claimed) = context.claim_todo(&selected_id)? else {
                    continue;
                };

                let use_worktree = desired_agents > 1;
                let mut job = context.create_job(
                    &run_id,
                    spawn_count,
                    &flow_name,
                    Some(claimed.id.clone()),
                    None,
                )?;
                job = context.set_job_status(job, JobStatus::Selecting, None)?;

                let worker_context = context.clone();
                let worker_run_id = run_id.clone();
                let worker_flow = flow_name.clone();
                let worker_model = model_override.clone();
                let worker_merge_lock = merge_lock.clone();

                workers.spawn(async move {
                    tokio::task::spawn_blocking(move || {
                        run_worker(
                            worker_context,
                            worker_run_id,
                            job,
                            claimed,
                            worker_flow,
                            worker_model,
                            use_worktree,
                            worker_merge_lock,
                        )
                    })
                    .await
                    .unwrap_or_else(|join_err| WorkerResult {
                        job_id: format!("join-error-{}", Utc::now().timestamp_millis()),
                        todo_id: "unknown".to_owned(),
                        status: "failed".to_owned(),
                        error: Some(format!("worker task panicked: {join_err}")),
                        commit_hash: None,
                    })
                });
            }

            {
                let mut states = self.states.lock().await;
                if let Some(state) = states.get_mut(&project_name) {
                    state.active_workers = workers.len();
                }
            }

            if workers.is_empty() {
                let no_more_work = context.store.list_available_todos()?.is_empty();
                if stop_requested || no_more_work {
                    break;
                }
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            if let Some(joined) = workers.join_next().await {
                match joined {
                    Ok(result) => {
                        if result.status != "completed" {
                            any_failure = true;
                            let mut states = self.states.lock().await;
                            if let Some(state) = states.get_mut(&project_name) {
                                state.last_error = result.error.clone();
                            }
                        }
                    }
                    Err(err) => {
                        any_failure = true;
                        let mut states = self.states.lock().await;
                        if let Some(state) = states.get_mut(&project_name) {
                            state.last_error = Some(format!("worker join error: {err}"));
                        }
                    }
                }
            }
        }

        engine.finish_run(
            &run_id,
            if any_failure {
                RunExitStatus::Failure
            } else {
                RunExitStatus::Success
            },
        )?;

        context.log_project_event(
            &run_id,
            None,
            None,
            "info",
            None,
            crate::domain::EventType::Job,
            format!("Supervisor completed for {project_name}"),
            std::collections::BTreeMap::new(),
        )?;

        let mut states = self.states.lock().await;
        if let Some(state) = states.get_mut(&project_name) {
            state.running = false;
            state.active_workers = 0;
            state.stop_requested = false;
        }

        Ok(())
    }
}

fn run_worker(
    context: ProjectContext,
    run_id: String,
    mut job: JobRecord,
    todo: Todo,
    flow_name: String,
    model_override: Option<String>,
    use_worktree: bool,
    merge_lock: Arc<Mutex<()>>,
) -> WorkerResult {
    let engine = ChiefEngine::new(context.clone());

    let update_job = |ctx: &ProjectContext,
                      current: &mut JobRecord,
                      status: JobStatus,
                      error: Option<String>| {
        if let Ok(updated) = ctx.set_job_status(current.clone(), status, error) {
            *current = updated;
        }
    };

    update_job(&context, &mut job, JobStatus::Running, None);

    let main_branch = context
        .git
        .current_branch()
        .unwrap_or_else(|_| "main".to_owned());

    let mut work_dir = context.project_dir.clone();
    let mut branch_name = None::<String>;

    if use_worktree {
        let worktree_root = context.project_dir.join(".chief-worktrees");
        if let Err(err) = fs::create_dir_all(&worktree_root) {
            update_job(
                &context,
                &mut job,
                JobStatus::Failed,
                Some(format!("failed to create worktree root: {err}")),
            );
            let _ = context
                .store
                .update_todo_status(&todo.id, TodoStatus::Attempted, None);
            return WorkerResult {
                job_id: job.id,
                todo_id: todo.id,
                status: "failed".to_owned(),
                error: Some(err.to_string()),
                commit_hash: None,
            };
        }

        let branch = format!("chief/{}/{}", context.name, job.id);
        let worktree_path = worktree_root.join(&job.id);

        if let Err(err) = context.git.create_worktree(&branch, &worktree_path) {
            update_job(
                &context,
                &mut job,
                JobStatus::Failed,
                Some(format!("failed to create worktree: {err}")),
            );
            let _ = context
                .store
                .update_todo_status(&todo.id, TodoStatus::Attempted, None);
            return WorkerResult {
                job_id: job.id,
                todo_id: todo.id,
                status: "failed".to_owned(),
                error: Some(err.to_string()),
                commit_hash: None,
            };
        }

        work_dir = worktree_path.clone();
        branch_name = Some(branch);

        let mut updated_job = job.clone();
        updated_job.worktree_path = Some(worktree_path.display().to_string());
        let _ = context.store.upsert_job(&updated_job);
        job = updated_job;
    }

    let todo_id = todo.id.clone();
    let outcome = engine.run_single_todo(
        &run_id,
        &job.id,
        job.worker_index,
        todo,
        &flow_name,
        work_dir.clone(),
        model_override,
    );

    let result = match outcome {
        Ok(outcome) => {
            let mut merge_error = None;

            if let Some(branch) = &branch_name {
                let merge_guard = merge_lock.blocking_lock();
                let merge_result = context
                    .git
                    .merge_branch_into_main(branch, &main_branch)
                    .and_then(|_| context.git.remove_worktree(&work_dir, branch));
                drop(merge_guard);

                if let Err(err) = merge_result {
                    merge_error = Some(err.to_string());
                }
            }

            if let Some(err) = merge_error {
                let _ = context
                    .store
                    .update_todo_status(&todo_id, TodoStatus::Attempted, None);
                update_job(&context, &mut job, JobStatus::Failed, Some(err.clone()));
                WorkerResult {
                    job_id: job.id,
                    todo_id,
                    status: "failed".to_owned(),
                    error: Some(err),
                    commit_hash: outcome.commit_hash,
                }
            } else {
                let _ = context.store.update_todo_status(
                    &todo_id,
                    TodoStatus::Done,
                    outcome.commit_hash.as_deref(),
                );
                update_job(&context, &mut job, JobStatus::Completed, None);
                WorkerResult {
                    job_id: job.id,
                    todo_id,
                    status: "completed".to_owned(),
                    error: None,
                    commit_hash: outcome.commit_hash,
                }
            }
        }
        Err(err) => {
            let _ = context
                .store
                .update_todo_status(&todo_id, TodoStatus::Attempted, None);
            if let Some(branch) = &branch_name {
                let _ = context.git.remove_worktree(&work_dir, branch);
            }
            update_job(&context, &mut job, JobStatus::Failed, Some(err.to_string()));
            WorkerResult {
                job_id: job.id,
                todo_id,
                status: "failed".to_owned(),
                error: Some(err.to_string()),
                commit_hash: None,
            }
        }
    };

    result
}

async fn select_todo_id(
    context: &ProjectContext,
    worker_index: usize,
    available: &[Todo],
    in_progress: &[Todo],
    model_override: Option<String>,
) -> Result<String> {
    if worker_index <= 1 || in_progress.is_empty() {
        return highest_priority_todo_id(available).ok_or_else(|| anyhow!("no available todo"));
    }

    let prompt = context.prompts.render_json(
        "todo_select.md",
        &serde_json::json!({
            "worker_index": worker_index,
            "available_todos": available,
            "in_progress_todos": in_progress,
        }),
    )?;

    let agent = context.build_agent(model_override);
    let timeout_seconds = context.chief_toml.chief.agent_timeout_seconds;
    let response = tokio::task::spawn_blocking({
        let project_dir = context.project_dir.clone();
        move || {
            agent.run(AgentRequest {
                prompt,
                cwd: project_dir,
                timeout_seconds: Some(timeout_seconds),
                disallowed_paths: Vec::new(),
            })
        }
    })
    .await
    .context("todo selector join error")??;

    if response.exit_code != 0 {
        return highest_priority_todo_id(available)
            .ok_or_else(|| anyhow!("todo selector failed and no todo available"));
    }

    let selected_id = response
        .merged_output
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();

    if available.iter().any(|todo| todo.id == selected_id) {
        Ok(selected_id)
    } else {
        highest_priority_todo_id(available).ok_or_else(|| anyhow!("no available todo"))
    }
}

fn highest_priority_todo_id(todos: &[Todo]) -> Option<String> {
    todos
        .iter()
        .max_by(|a, b| a.priority.cmp(&b.priority).then_with(|| b.id.cmp(&a.id)))
        .map(|todo| todo.id.clone())
}
