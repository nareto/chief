use crate::api::error::ApiError;
use crate::api::types::{
    ActiveJobResponse, AddTodoRequest, ChiefYamlResponse, EventsQuery, EventsResponse,
    FileDiffQuery, FileDiffResponse, JobsResponse, LogQuery, MessageResponse,
    ProjectReadinessResponse, ProjectsResponse, RequirementsRequest, RequirementsResponse,
    RunSuiteCheckRequest, RunSuiteCheckResponse, RunSuiteCheckStreamEvent, StartProjectRequest,
    StateResponse, TodoProgress, TodoResponse, TodosResponse, UpdateChiefYamlRequest,
    UpdateTodoRequest,
};
use anyhow::{Context, anyhow};
use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use chief::config::TestSuiteConfig;
use chief::domain::{EventRecord, EventType, JobStatus, Phase, RunExitStatus, Todo, TodoStatus};
use chief::flow::{
    FlowKind, SuiteCommandKind, execute_suite_cleanup_command, execute_suite_command,
    suite_command_cwd, suite_command_for_kind,
};
use chief::git::{
    GIT_TRANSIENT_LOCK_RETRY_ATTEMPTS, GitOps, ShellGitOps,
    git_output_has_transient_lock_contention_signature, run_git_command_with_retry,
};
use chief::scheduler::{Scheduler, StopMode};
use chief::service::ChiefEngine;
use chief::storage::{EventQuery, ProjectReadinessState, ProjectStore, ReadinessStatus};
use chief::worktree_cache;
use chrono::Utc;
use futures_util::stream;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::{broadcast, mpsc as tokio_mpsc};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::api::query_utils::{
    is_internal_workspace_state_file, matches_requested_type, parse_event_type,
    parse_loop_iteration, parse_phase, parse_requested_types, parse_todo_status_input,
    resolve_last_done_todo_committed_at,
};
use crate::api::streaming::{
    execute_suite_command_streaming, send_stream_event_async, send_stream_event_blocking,
};

#[path = "service/readiness.rs"]
mod readiness;
#[path = "service/suite_checks.rs"]
mod suite_checks;

use readiness::project_readiness_response;

#[derive(Clone)]
pub struct ApiService {
    scheduler: Scheduler,
    default_agents_per_project: usize,
    readiness_cancel_signals: Arc<tokio::sync::Mutex<HashMap<String, Arc<AtomicBool>>>>,
    readiness_stream_sender: broadcast::Sender<ReadinessStreamMessage>,
    readiness_stream_snapshots: Arc<std::sync::Mutex<HashMap<String, String>>>,
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct ReadinessCheckResult {
    pub readiness: ProjectReadinessResponse,
    pub ran: bool,
}

#[derive(Debug, Clone)]
struct TempWorktree {
    branch: String,
    path: PathBuf,
}

const RETRY_CLEANUP_DISCARDED_MSG_PREFIX: &str =
    "Retry cleanup: discarded local git changes before loop";

#[derive(Debug, Clone)]
pub enum ReadinessStreamMessage {
    Reset { project: String },
    Chunk { project: String, text: String },
}

impl ApiService {
    pub fn new(scheduler: Scheduler, default_agents_per_project: usize) -> Self {
        let (readiness_stream_sender, _) = broadcast::channel(1024);
        Self {
            scheduler,
            default_agents_per_project: default_agents_per_project.max(1),
            readiness_cancel_signals: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            readiness_stream_sender,
            readiness_stream_snapshots: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    pub async fn list_projects(&self) -> ProjectsResponse {
        let projects = self.scheduler.list_project_views().await;
        ProjectsResponse { projects }
    }

    pub async fn refresh_projects(&self) -> Result<MessageResponse, ApiError> {
        self.scheduler
            .refresh_registry()
            .await
            .map_err(ApiError::internal)?;
        Ok(MessageResponse {
            message: "registry refreshed".to_owned(),
        })
    }

    pub async fn start_project(
        &self,
        project: String,
        payload: StartProjectRequest,
    ) -> Result<MessageResponse, ApiError> {
        let mut context = self.project_context(&project).await?;
        if !context.config_path.is_file() {
            return Err(ApiError::chief_yaml_missing(
                context.config_path.display().to_string(),
            ));
        }
        context.refresh().map_err(ApiError::internal)?;
        let chief_yaml_hash =
            readiness::chief_yaml_content_hash(&context.config_path).map_err(ApiError::internal)?;
        let agents = payload
            .agents
            .unwrap_or(self.default_agents_per_project)
            .max(1);

        let configured_flow = context.chief_yaml.chief.flow.trim();
        let flow_kind = payload
            .flow
            .as_deref()
            .unwrap_or(configured_flow)
            .parse::<FlowKind>()
            .map_err(|err| ApiError::unprocessable(err.to_string()))?;

        let start_anyway = payload.start_anyway.unwrap_or(false);
        if !start_anyway {
            let _ = self
                .ensure_project_readiness(&project, &context, &chief_yaml_hash, false)
                .await?;
        }

        self.scheduler
            .start_project(project.clone(), agents, flow_kind, payload.model)
            .await
            .map_err(ApiError::internal)?;

        Ok(MessageResponse {
            message: format!(
                "started project {} with {} agent(s), flow={}",
                project,
                agents,
                flow_kind.as_str()
            ),
        })
    }

    pub async fn stop_project(&self, project: &str) -> Result<MessageResponse, ApiError> {
        self.scheduler
            .stop_project(project)
            .await
            .map_err(ApiError::internal)?;
        Ok(MessageResponse {
            message: format!("stop requested for project {project}"),
        })
    }

    pub async fn pause_project(&self, project: &str) -> Result<MessageResponse, ApiError> {
        self.scheduler
            .pause_project(project)
            .await
            .map_err(ApiError::internal)?;
        Ok(MessageResponse {
            message: format!("pause requested for project {project}"),
        })
    }

    pub async fn get_todos(&self, project: &str) -> Result<TodosResponse, ApiError> {
        let mut context = self.project_context(project).await?;
        context.refresh().map_err(ApiError::internal)?;
        let todos = context.store.list_todos().map_err(ApiError::internal)?;
        Ok(TodosResponse { todos })
    }

    pub async fn add_todo(
        &self,
        project: &str,
        payload: AddTodoRequest,
    ) -> Result<TodoResponse, ApiError> {
        let context = self.project_context(project).await?;
        let todo = Todo {
            id: String::new(),
            todo: payload.todo,
            expectations: payload.expectations.unwrap_or_default(),
            priority: payload.priority.unwrap_or(0),
            test_suites: payload.test_suites.unwrap_or_default(),
            status: TodoStatus::Pending,
            done_at_commit: None,
        }
        .normalize();
        let todo = context
            .store
            .append_todo(todo)
            .map_err(ApiError::internal)?;
        Ok(TodoResponse { todo })
    }

    pub async fn update_todo(
        &self,
        project: &str,
        todo_id: &str,
        payload: UpdateTodoRequest,
    ) -> Result<TodoResponse, ApiError> {
        let context = self.project_context(project).await?;
        let current = context
            .store
            .list_todos()
            .map_err(ApiError::internal)?
            .into_iter()
            .find(|todo| todo.id == todo_id)
            .ok_or_else(|| ApiError::not_found(format!("todo '{todo_id}' not found")))?;

        let status = match payload.status {
            Some(raw) => parse_todo_status_input(&raw)
                .ok_or_else(|| ApiError::unprocessable(format!("invalid todo status '{raw}'")))?,
            None => current.status,
        };

        let done_at_commit = match payload.done_at_commit {
            Some(Some(raw)) => {
                let value = raw.trim();
                if value.is_empty() {
                    None
                } else {
                    Some(value.to_owned())
                }
            }
            Some(None) => None,
            None => current.done_at_commit.clone(),
        };

        let todo = Todo {
            id: payload.id.unwrap_or(current.id),
            todo: payload.todo.unwrap_or(current.todo),
            expectations: payload.expectations.unwrap_or(current.expectations),
            priority: payload.priority.unwrap_or(current.priority),
            test_suites: payload.test_suites.unwrap_or(current.test_suites),
            status,
            done_at_commit,
        }
        .normalize();

        if todo.todo.trim().is_empty() {
            return Err(ApiError::unprocessable("todo text cannot be empty"));
        }

        let updated = context
            .store
            .update_todo(todo_id, todo)
            .map_err(ApiError::classify_store_error)?;

        Ok(TodoResponse { todo: updated })
    }

    pub async fn delete_todo(
        &self,
        project: &str,
        todo_id: &str,
    ) -> Result<MessageResponse, ApiError> {
        let context = self.project_context(project).await?;

        context
            .store
            .delete_todo(todo_id)
            .map_err(ApiError::classify_store_error)?;

        Ok(MessageResponse {
            message: format!("deleted todo '{todo_id}'"),
        })
    }

    pub async fn delete_done_todos(&self, project: &str) -> Result<MessageResponse, ApiError> {
        let context = self.project_context(project).await?;
        let deleted = context
            .store
            .delete_done_todos()
            .map_err(ApiError::internal)?;

        Ok(MessageResponse {
            message: format!("deleted {deleted} done todo(s)"),
        })
    }

    pub async fn get_jobs(&self, project: &str) -> Result<JobsResponse, ApiError> {
        let context = self.project_context(project).await?;
        let jobs = context.store.list_jobs(200).map_err(ApiError::internal)?;
        Ok(JobsResponse { jobs })
    }

    pub async fn get_logs(
        &self,
        project: &str,
        query: LogQuery,
    ) -> Result<EventsResponse, ApiError> {
        let context = self.project_context(project).await?;

        let event_type = query
            .event_type
            .as_deref()
            .map(parse_event_type)
            .transpose()?;

        let phase = query.phase.as_deref().map(parse_phase).transpose()?;

        let events = context
            .store
            .query_events(EventQuery {
                limit: query.limit.unwrap_or(200),
                event_type,
                phase,
                level: query.level,
                contains_text: query.q,
            })
            .map_err(ApiError::internal)?;

        Ok(EventsResponse { events })
    }

    pub async fn process_requirements(
        &self,
        project: &str,
        payload: RequirementsRequest,
    ) -> Result<RequirementsResponse, ApiError> {
        let context = self.project_context(project).await?;
        let engine = ChiefEngine::new(context.clone());

        let diff = tokio::task::spawn_blocking(move || {
            engine.process_requirements(&payload.text, &context.store.todos_path, payload.model)
        })
        .await
        .map_err(|err| ApiError::internal(anyhow!(err.to_string())))?
        .map_err(ApiError::internal)?;

        Ok(RequirementsResponse { diff })
    }

    pub async fn get_state(&self, project: &str) -> Result<StateResponse, ApiError> {
        let mut context = self.project_context(project).await?;
        context.refresh().map_err(ApiError::internal)?;
        let views = self.scheduler.list_project_views().await;
        let runtime = views.into_iter().find(|view| view.name == project);

        let todos = context.store.list_todos().map_err(ApiError::internal)?;
        let jobs = context.store.list_jobs(200).map_err(ApiError::internal)?;
        let recent_events = context
            .store
            .query_events(EventQuery {
                limit: 200,
                ..EventQuery::default()
            })
            .map_err(ApiError::internal)?;

        let current_phase = recent_events
            .iter()
            .find_map(|event| event.phase.map(Phase::as_str))
            .unwrap_or(Phase::Start.as_str())
            .to_owned();

        let phase_iteration = recent_events.iter().find_map(|event| {
            if event.event_type == EventType::PhaseChange {
                parse_loop_iteration(&event.msg)
            } else {
                None
            }
        });

        let dirty_files = context
            .git
            .changed_files(&context.project_dir)
            .map_err(ApiError::internal)?;
        let chief_db_size_bytes = fs::metadata(&context.store.db_path)
            .map(|metadata| metadata.len())
            .ok();
        let readiness = context
            .store
            .get_readiness_state()
            .map_err(ApiError::internal)?;

        let active_job = jobs
            .iter()
            .find(|job| {
                matches!(
                    job.status,
                    JobStatus::Queued
                        | JobStatus::Selecting
                        | JobStatus::Running
                        | JobStatus::Merging
                )
            })
            .map(|job| ActiveJobResponse {
                job_id: job.id.clone(),
                todo_id: job.todo_id.clone(),
                worker_index: job.worker_index,
                status: job.status.as_str().to_owned(),
            });

        let completed_todos = todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::Done)
            .count();
        let available_todos = todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::Pending)
            .count();
        let last_done_todo_committed_at =
            resolve_last_done_todo_committed_at(&context.git, &context.project_dir, &todos);

        let configured_flow_name = FlowKind::resolve_name(&context.chief_yaml.chief.flow);

        Ok(StateResponse {
            project: project.to_owned(),
            running: runtime.as_ref().map(|view| view.running).unwrap_or(false),
            stop_requested: runtime
                .as_ref()
                .map(|view| view.stop_requested)
                .unwrap_or(false),
            stop_mode: runtime
                .as_ref()
                .map(|view| view.stop_mode)
                .unwrap_or(StopMode::None),
            active_agents: runtime
                .as_ref()
                .map(|view| view.active_workers)
                .unwrap_or(0),
            desired_agents: runtime
                .as_ref()
                .map(|view| view.desired_agents)
                .unwrap_or(1),
            flow_name: runtime
                .as_ref()
                .map(|view| view.flow_name.clone())
                .unwrap_or_else(|| configured_flow_name.clone()),
            last_error: runtime.as_ref().and_then(|view| view.last_error.clone()),
            phase: current_phase,
            phase_iteration,
            last_activity: recent_events
                .first()
                .map(|event| event.timestamp.to_rfc3339()),
            last_done_todo_committed_at,
            chief_db_size_bytes,
            dirty_files,
            todos: TodoProgress {
                available: available_todos,
                completed: completed_todos,
                total: todos.len(),
            },
            active_job,
            readiness: project_readiness_response(readiness),
        })
    }

    pub async fn get_events(
        &self,
        project: &str,
        query: EventsQuery,
    ) -> Result<EventsResponse, ApiError> {
        let context = self.project_context(project).await?;
        let limit = query.limit.unwrap_or(50).clamp(1, 500);
        let sample_size = (limit.saturating_mul(8)).min(1_000);
        let requested_types = parse_requested_types(query.types.as_deref());

        let events = context
            .store
            .query_events(EventQuery {
                limit: sample_size,
                contains_text: query.q,
                ..EventQuery::default()
            })
            .map_err(ApiError::internal)?;

        let filtered = events
            .into_iter()
            .filter(|event| matches_requested_type(event.event_type, &requested_types))
            .take(limit)
            .collect::<Vec<_>>();

        Ok(EventsResponse { events: filtered })
    }

    pub async fn get_file_diff(
        &self,
        project: &str,
        query: FileDiffQuery,
    ) -> Result<FileDiffResponse, ApiError> {
        let context = self.project_context(project).await?;
        let file = query.file.unwrap_or_default().trim().to_owned();

        let diff = if file.is_empty() {
            context
                .git
                .diff(&context.project_dir, None)
                .map_err(ApiError::internal)?
        } else {
            run_git_capture(&context.project_dir, &["diff", "--", &file])?
        };

        Ok(FileDiffResponse { file, diff })
    }

    pub async fn reset_project_workspace(
        &self,
        project: &str,
    ) -> Result<MessageResponse, ApiError> {
        let runtime = self
            .scheduler
            .list_project_views()
            .await
            .into_iter()
            .find(|view| view.name == project)
            .ok_or_else(|| ApiError::not_found(format!("project '{project}' not found")))?;
        if runtime.running {
            return Err(ApiError::unprocessable(
                "project must be stopped before resetting workspace",
            ));
        }

        let mut context = self.project_context(project).await?;
        context.refresh().map_err(ApiError::internal)?;

        let changed_files = context
            .git
            .changed_files(&context.project_dir)
            .map_err(ApiError::internal)?
            .into_iter()
            .filter(|path| !is_internal_workspace_state_file(path))
            .collect::<Vec<_>>();
        if !changed_files.is_empty() {
            run_git_capture(&context.project_dir, &["reset", "--hard", "HEAD"])?;
            run_git_capture(
                &context.project_dir,
                &["clean", "-fd", "-e", "chief.db", "-e", "chief.db-*"],
            )?;
        }

        let marker_message = format!("{RETRY_CLEANUP_DISCARDED_MSG_PREFIX} manual/1");
        let mut marker_payload = BTreeMap::new();
        marker_payload.insert(
            "files".to_owned(),
            serde_json::Value::Array(
                changed_files
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
        let todo_ids = context
            .store
            .list_todos()
            .map_err(ApiError::internal)?
            .into_iter()
            .filter(|todo| todo.status != TodoStatus::Done)
            .map(|todo| todo.id)
            .collect::<Vec<_>>();
        let run_id = format!("manual-workspace-reset-{}", Uuid::new_v4());
        context
            .store
            .start_run(&run_id)
            .map_err(ApiError::internal)?;

        let log_result = if todo_ids.is_empty() {
            context.log_project_event(
                &run_id,
                None,
                None,
                "warning",
                Some(Phase::Red),
                EventType::GitOp,
                marker_message,
                marker_payload,
            )
        } else {
            for todo_id in &todo_ids {
                context.log_project_event(
                    &run_id,
                    None,
                    Some(todo_id.clone()),
                    "warning",
                    Some(Phase::Red),
                    EventType::GitOp,
                    marker_message.clone(),
                    marker_payload.clone(),
                )?;
            }
            Ok(())
        }
        .map_err(ApiError::internal);

        let run_exit_status = if log_result.is_ok() {
            RunExitStatus::Success
        } else {
            RunExitStatus::Failure
        };
        context
            .store
            .finish_run(&run_id, run_exit_status)
            .map_err(ApiError::internal)?;
        log_result?;

        Ok(MessageResponse {
            message: if changed_files.is_empty() {
                format!(
                    "workspace already clean; recorded reset marker for {} todo(s)",
                    todo_ids.len()
                )
            } else {
                format!(
                    "discarded {} local git change(s); recorded reset marker for {} todo(s)",
                    changed_files.len(),
                    todo_ids.len()
                )
            },
        })
    }

    pub async fn get_chief_yaml(&self, project: &str) -> Result<ChiefYamlResponse, ApiError> {
        let context = self.project_context(project).await?;
        let content = fs::read_to_string(&context.config_path).with_context(|| {
            format!(
                "failed to read chief config at {}",
                context.config_path.display()
            )
        })?;
        Ok(ChiefYamlResponse { content })
    }

    pub async fn update_chief_yaml(
        &self,
        project: &str,
        payload: UpdateChiefYamlRequest,
    ) -> Result<MessageResponse, ApiError> {
        let context = self.project_context(project).await?;
        fs::write(&context.config_path, &payload.content).with_context(|| {
            format!(
                "failed to write chief config at {}",
                context.config_path.display()
            )
        })?;

        if let Err(err) = context.git.commit_paths(
            &context.project_dir,
            &["chief.yaml"],
            "chore: update chief.yaml via settings",
        ) {
            info!(
                project,
                error = %err,
                "skipped git commit for chief.yaml settings update"
            );
        }

        Ok(MessageResponse {
            message: "chief.yaml updated".to_owned(),
        })
    }

    pub async fn reset_project_db(&self, project: &str) -> Result<MessageResponse, ApiError> {
        let context = self.project_context(project).await?;
        context
            .store
            .reset_db_from_todos_file()
            .map_err(ApiError::internal)?;
        Ok(MessageResponse {
            message: format!("reset chief.db for project {project}"),
        })
    }

    pub async fn trim_project_db(
        &self,
        project: &str,
        keep_runs: usize,
    ) -> Result<MessageResponse, ApiError> {
        if keep_runs == 0 {
            return Err(ApiError::unprocessable("keep_runs must be at least 1"));
        }
        let context = self.project_context(project).await?;
        let deleted = context
            .store
            .trim_events_to_recent_runs(keep_runs)
            .map_err(ApiError::internal)?;
        Ok(MessageResponse {
            message: format!("trimmed {deleted} events; kept the last {keep_runs} runs"),
        })
    }

    pub async fn project_dir_for_terminal(&self, project: &str) -> Result<PathBuf, ApiError> {
        let context = self.project_context(project).await?;
        Ok(context.project_dir)
    }

    pub async fn project_store_for_events(
        &self,
        project: &str,
    ) -> Result<chief::storage::ProjectStore, ApiError> {
        let context = self.project_context(project).await?;
        Ok(context.store)
    }

    async fn project_context(
        &self,
        project: &str,
    ) -> Result<chief::service::ProjectContext, ApiError> {
        self.scheduler
            .get_project_context(project)
            .await
            .map_err(ApiError::classify_store_error)
    }
}

fn create_temp_worktree(
    context: &chief::service::ProjectContext,
    purpose: &str,
) -> anyhow::Result<TempWorktree> {
    let worktree_root = temp_worktree_root_for_project(&context.project_dir, &context.name);
    fs::create_dir_all(&worktree_root).with_context(|| {
        format!(
            "failed to create temporary worktree root at {}",
            worktree_root.display()
        )
    })?;

    let token = Uuid::new_v4().simple().to_string();
    let purpose_branch = purpose.trim().replace('_', "-");
    let purpose_path = purpose.trim().replace('-', "_");
    let branch = format!("chief/{}/{}-{}", context.name, purpose_branch, token);
    let path = worktree_root.join(format!("chief_{}_{}", purpose_path, token));
    context
        .git
        .create_worktree(&branch, &path)
        .with_context(|| {
            format!(
                "failed to create readiness worktree {} for branch {}",
                path.display(),
                branch
            )
        })?;

    Ok(TempWorktree { branch, path })
}

fn cleanup_temp_worktree(
    git: &ShellGitOps,
    readiness_worktree: &TempWorktree,
) -> anyhow::Result<()> {
    git.remove_worktree(&readiness_worktree.path, &readiness_worktree.branch)
        .with_context(|| {
            format!(
                "failed to remove readiness worktree {}",
                readiness_worktree.path.display()
            )
        })
}

fn temp_worktree_root_for_project(project_dir: &Path, project_name: &str) -> PathBuf {
    let parent_dir = project_dir.parent().unwrap_or(project_dir);
    parent_dir.join(format!("{project_name}__worktrees"))
}

fn run_git_capture(project_dir: &PathBuf, args: &[&str]) -> Result<String, ApiError> {
    let output = run_git_command_with_retry(project_dir, args)
        .with_context(|| format!("failed to run git {}", args.join(" ")))
        .map_err(ApiError::internal)?;

    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if git_output_has_transient_lock_contention_signature(&output) {
            return Err(ApiError::bad_request(format!(
                "transient lock/contention retry budget exhausted after {GIT_TRANSIENT_LOCK_RETRY_ATTEMPTS} retries: git {} failed: {}",
                args.join(" "),
                detail
            )));
        }
        return Err(ApiError::bad_request(format!(
            "git {} failed: {}",
            args.join(" "),
            detail
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
#[path = "service/tests.rs"]
mod tests;
