use crate::api::error::ApiError;
use crate::api::types::{
    ActiveJobResponse, AddTodoRequest, ChiefYamlResponse, EventsQuery, EventsResponse,
    FileDiffQuery, FileDiffResponse, JobsResponse, LogQuery, MessageResponse, PhaseIteration,
    ProjectReadinessResponse, ProjectsResponse, RequirementsRequest, RequirementsResponse,
    RunSuiteCheckRequest, RunSuiteCheckResponse, RunSuiteCheckStreamEvent, StartProjectRequest,
    StateResponse, SuiteCheckOutputStream, TodoProgress, TodoResponse, TodosResponse,
    UpdateChiefYamlRequest, UpdateTodoRequest,
};
use anyhow::{Context, anyhow};
use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use chief::config::TestSuiteConfig;
use chief::domain::{EventRecord, EventType, JobStatus, Phase, RunExitStatus, Todo, TodoStatus};
use chief::flow::{
    FlowKind, SuiteCommandKind, configure_process_group, execute_suite_cleanup_command,
    execute_suite_command, suite_command_cwd, suite_command_for_kind, terminate_process_tree,
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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc as tokio_mpsc};
use tracing::{error, info, warn};
use uuid::Uuid;

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

struct SuiteCheckPlan {
    suite_name: String,
    kind: SuiteCommandKind,
    command: String,
    cleanup_command: Option<String>,
    cwd: PathBuf,
    cwd_display: String,
    env: BTreeMap<String, String>,
    timeout_seconds: u64,
}

struct SuiteCheckExecution {
    plan: SuiteCheckPlan,
    git: ShellGitOps,
    worktree: TempWorktree,
}

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

#[derive(Debug, Clone)]
struct TempWorktree {
    branch: String,
    path: PathBuf,
}

const RETRY_CLEANUP_DISCARDED_MSG_PREFIX: &str =
    "Retry cleanup: discarded local git changes before loop";
const READINESS_UNCHECKED_SUMMARY: &str = "Pre-run checks have not run yet.";
const READINESS_EVENT_SOURCE: &str = "pre_run_checks";
const READINESS_STREAM_MAX_BUFFER_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
struct ReadinessLogContext {
    run_id: String,
    store: ProjectStore,
}

#[derive(Debug, Clone)]
pub enum ReadinessStreamMessage {
    Reset { project: String },
    Chunk { project: String, text: String },
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
            chief_yaml_content_hash(&context.config_path).map_err(ApiError::internal)?;
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

    pub async fn run_suite_check(
        &self,
        project: &str,
        payload: RunSuiteCheckRequest,
    ) -> Result<RunSuiteCheckResponse, ApiError> {
        let SuiteCheckExecution {
            plan:
                SuiteCheckPlan {
                    suite_name,
                    kind,
                    command,
                    cleanup_command,
                    cwd,
                    cwd_display,
                    env,
                    timeout_seconds,
                },
            git,
            worktree,
        } = self
            .prepare_suite_check_execution(project, &payload)
            .await?;
        let kind_label = kind.as_str();
        info!(
            project,
            suite = %suite_name,
            kind = %kind_label,
            cwd = %cwd_display,
            command = %command,
            "running suite check command"
        );
        let cancel_signal = Arc::new(AtomicBool::new(false));

        let output = tokio::task::spawn_blocking(move || {
            let test_result =
                execute_suite_command(&command, &cwd, &env, &cancel_signal, Some(timeout_seconds));
            let cleanup_result = if kind == SuiteCommandKind::Test {
                execute_suite_cleanup_command(
                    cleanup_command.as_deref(),
                    &cwd,
                    &env,
                    Some(timeout_seconds),
                )
            } else {
                Ok(None)
            };
            (test_result, cleanup_result)
        })
        .await;

        let response = match output {
            Ok((Ok(output), cleanup_result)) => {
                match cleanup_result {
                    Ok(Some(cleanup_out)) => {
                        if cleanup_out.exit_code == 0 {
                            info!(
                                project,
                                suite = %suite_name,
                                kind = %kind_label,
                                command = %cleanup_out.command,
                                "suite cleanup command finished"
                            );
                        } else {
                            warn!(
                                project,
                                suite = %suite_name,
                                kind = %kind_label,
                                command = %cleanup_out.command,
                                exit_code = cleanup_out.exit_code,
                                "suite cleanup command failed"
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        warn!(
                            project,
                            suite = %suite_name,
                            kind = %kind_label,
                            error = %err,
                            "suite cleanup command execution failed"
                        );
                    }
                }
                info!(
                    project,
                    suite = %suite_name,
                    kind = %kind_label,
                    exit_code = output.exit_code,
                    stdout_len = output.stdout.len(),
                    stderr_len = output.stderr.len(),
                    "suite check command finished"
                );

                Ok(RunSuiteCheckResponse {
                    suite: suite_name,
                    kind,
                    command: output.command,
                    cwd: cwd_display,
                    exit_code: output.exit_code,
                    output: output.merged_output,
                    stdout: output.stdout,
                    stderr: output.stderr,
                })
            }
            Ok((Err(err), cleanup_result)) => {
                match cleanup_result {
                    Ok(Some(cleanup_out)) => {
                        if cleanup_out.exit_code == 0 {
                            info!(
                                project,
                                suite = %suite_name,
                                kind = %kind_label,
                                command = %cleanup_out.command,
                                "suite cleanup command finished after command failure"
                            );
                        } else {
                            warn!(
                                project,
                                suite = %suite_name,
                                kind = %kind_label,
                                command = %cleanup_out.command,
                                exit_code = cleanup_out.exit_code,
                                "suite cleanup command failed after command failure"
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(cleanup_err) => {
                        warn!(
                            project,
                            suite = %suite_name,
                            kind = %kind_label,
                            error = %cleanup_err,
                            "suite cleanup command execution failed after command failure"
                        );
                    }
                }
                error!(
                    project,
                    suite = %suite_name,
                    kind = %kind_label,
                    error = %err,
                    "suite command execution failed"
                );
                Err(ApiError::internal(err))
            }
            Err(err) => {
                error!(
                    project,
                    suite = %suite_name,
                    kind = %kind_label,
                    error = %err,
                    "suite command task join failed"
                );
                Err(ApiError::internal(anyhow!(
                    "suite command task failed: {err}"
                )))
            }
        };

        if let Err(err) = cleanup_temp_worktree(&git, &worktree) {
            warn!(
                project,
                branch = %worktree.branch,
                worktree = %worktree.path.display(),
                error = %err,
                "failed to cleanup suite check worktree"
            );
        }

        response
    }

    pub async fn run_suite_check_stream(
        &self,
        project: &str,
        payload: RunSuiteCheckRequest,
    ) -> Result<Response, ApiError> {
        let SuiteCheckExecution {
            plan,
            git,
            worktree,
        } = self
            .prepare_suite_check_execution(project, &payload)
            .await?;
        info!(
            project,
            suite = %plan.suite_name,
            kind = %plan.kind.as_str(),
            cwd = %plan.cwd_display,
            command = %plan.command,
            "running suite check command (stream)"
        );

        let (sender, receiver) = tokio_mpsc::channel::<Vec<u8>>(128);
        send_stream_event_async(
            &sender,
            RunSuiteCheckStreamEvent::Started {
                suite: plan.suite_name.clone(),
                kind: plan.kind,
                command: plan.command.clone(),
                cwd: plan.cwd_display.clone(),
            },
        )
        .await;

        let project_name = project.to_owned();
        tokio::task::spawn_blocking(move || {
            let SuiteCheckPlan {
                suite_name,
                kind,
                command,
                cleanup_command,
                cwd,
                cwd_display,
                env,
                timeout_seconds,
            } = plan;
            let kind_label = kind.as_str().to_owned();

            let stream_sender = sender.clone();
            let command_result = execute_suite_command_streaming(
                &suite_name,
                kind,
                &command,
                &cwd,
                &cwd_display,
                &env,
                timeout_seconds,
                None,
                |stream, text| {
                    let _ = send_stream_event_blocking(
                        &stream_sender,
                        RunSuiteCheckStreamEvent::Chunk {
                            stream,
                            text: text.to_owned(),
                        },
                    );
                },
            );

            let cleanup_result = if kind == SuiteCommandKind::Test {
                execute_suite_cleanup_command(
                    cleanup_command.as_deref(),
                    &cwd,
                    &env,
                    Some(timeout_seconds),
                )
            } else {
                Ok(None)
            };

            match command_result {
                Ok(result) => {
                    match cleanup_result {
                        Ok(Some(cleanup_out)) => {
                            if cleanup_out.exit_code == 0 {
                                info!(
                                    project = %project_name,
                                    suite = %suite_name,
                                    kind = %kind_label,
                                    command = %cleanup_out.command,
                                    "suite cleanup command finished"
                                );
                            } else {
                                warn!(
                                    project = %project_name,
                                    suite = %suite_name,
                                    kind = %kind_label,
                                    command = %cleanup_out.command,
                                    exit_code = cleanup_out.exit_code,
                                    "suite cleanup command failed"
                                );
                            }
                        }
                        Ok(None) => {}
                        Err(err) => {
                            warn!(
                                project = %project_name,
                                suite = %suite_name,
                                kind = %kind_label,
                                error = %err,
                                "suite cleanup command execution failed"
                            );
                        }
                    }
                    info!(
                        project = %project_name,
                        suite = %result.suite,
                        kind = %kind_label,
                        exit_code = result.exit_code,
                        stdout_len = result.stdout.len(),
                        stderr_len = result.stderr.len(),
                        "suite check stream command finished"
                    );
                    send_stream_event_blocking(
                        &sender,
                        RunSuiteCheckStreamEvent::Completed { result },
                    );
                }
                Err(err) => {
                    match cleanup_result {
                        Ok(Some(cleanup_out)) => {
                            if cleanup_out.exit_code == 0 {
                                info!(
                                    project = %project_name,
                                    suite = %suite_name,
                                    kind = %kind_label,
                                    command = %cleanup_out.command,
                                    "suite cleanup command finished after command failure"
                                );
                            } else {
                                warn!(
                                    project = %project_name,
                                    suite = %suite_name,
                                    kind = %kind_label,
                                    command = %cleanup_out.command,
                                    exit_code = cleanup_out.exit_code,
                                    "suite cleanup command failed after command failure"
                                );
                            }
                        }
                        Ok(None) => {}
                        Err(cleanup_err) => {
                            warn!(
                                project = %project_name,
                                suite = %suite_name,
                                kind = %kind_label,
                                error = %cleanup_err,
                                "suite cleanup command execution failed after command failure"
                            );
                        }
                    }
                    error!(
                        project = %project_name,
                        suite = %suite_name,
                        kind = %kind_label,
                        error = %err,
                        "suite check stream command failed"
                    );
                    send_stream_event_blocking(
                        &sender,
                        RunSuiteCheckStreamEvent::Error {
                            error: err.to_string(),
                        },
                    );
                }
            }

            if let Err(err) = cleanup_temp_worktree(&git, &worktree) {
                warn!(
                    project = %project_name,
                    branch = %worktree.branch,
                    worktree = %worktree.path.display(),
                    error = %err,
                    "failed to cleanup suite check worktree"
                );
            }
        });

        let body_stream = stream::unfold(receiver, |mut receiver| async move {
            receiver
                .recv()
                .await
                .map(|chunk| (Ok::<Vec<u8>, std::convert::Infallible>(chunk), receiver))
        });

        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-ndjson; charset=utf-8"),
        );
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));

        Ok((headers, Body::from_stream(body_stream)).into_response())
    }

    async fn prepare_suite_check_execution(
        &self,
        project: &str,
        payload: &RunSuiteCheckRequest,
    ) -> Result<SuiteCheckExecution, ApiError> {
        let mut context = self.project_context(project).await?;
        context.refresh().map_err(ApiError::internal)?;

        let suite_name = payload.suite.trim();
        if suite_name.is_empty() {
            return Err(ApiError::unprocessable("suite is required"));
        }
        if payload.kind == SuiteCommandKind::PostGreen {
            return Err(ApiError::unprocessable(
                "kind 'post_green' is not supported by this endpoint",
            ));
        }

        let suite = context
            .chief_yaml
            .suites
            .iter()
            .find(|suite| suite.name == suite_name)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("suite '{}' not found", payload.suite)))?;

        let target_override = payload.target.as_deref();
        let command =
            suite_command_for_kind(&suite, payload.kind, target_override).ok_or_else(|| {
                ApiError::unprocessable(format!(
                    "suite '{}' has no {} command configured",
                    suite.name,
                    match payload.kind {
                        SuiteCommandKind::Lint => "lint",
                        SuiteCommandKind::Test => "test",
                        SuiteCommandKind::PostGreen => "post-green",
                    }
                ))
            })?;

        let worktree = create_temp_worktree(&context, "suite-check").map_err(ApiError::internal)?;
        let cwd = suite_command_cwd(&worktree.path, &suite);
        let cwd_display = cwd.display().to_string();

        Ok(SuiteCheckExecution {
            plan: SuiteCheckPlan {
                suite_name: suite.name,
                kind: payload.kind,
                command,
                cleanup_command: if payload.kind == SuiteCommandKind::Test {
                    suite.cleanup_command.clone()
                } else {
                    None
                },
                cwd,
                cwd_display,
                env: suite.env,
                timeout_seconds: suite
                    .command_timeout_seconds
                    .unwrap_or(context.chief_yaml.chief.suite_command_timeout_seconds)
                    .max(1),
            },
            git: context.git.clone(),
            worktree,
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

    async fn project_context(
        &self,
        project: &str,
    ) -> Result<chief::service::ProjectContext, ApiError> {
        self.scheduler
            .get_project_context(project)
            .await
            .map_err(ApiError::classify_store_error)
    }

    async fn ensure_project_readiness(
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

fn project_readiness_response(readiness: ProjectReadinessState) -> ProjectReadinessResponse {
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

fn build_readiness_command_plans(
    context: &chief::service::ProjectContext,
    project_dir: &Path,
) -> Vec<ReadinessCommandPlan> {
    let mut plans = Vec::new();
    let default_timeout = context.chief_yaml.chief.suite_command_timeout_seconds;

    for suite in &context.chief_yaml.suites {
        let mut target_candidates = collect_readiness_targets(project_dir, suite);
        if let Some(default_target) = normalized_default_target(suite)
            && !target_candidates.contains(&default_target)
        {
            target_candidates.push(default_target);
        }
        let timeout_seconds = suite
            .command_timeout_seconds
            .unwrap_or(default_timeout)
            .max(1);
        let cwd = suite_command_cwd(project_dir, suite);
        let cwd_display = cwd.display().to_string();

        if let Some(command_template) = suite
            .test_init
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_owned)
        {
            plans.push(ReadinessCommandPlan {
                suite_name: suite.name.clone(),
                kind: ReadinessCommandKind::TestInit,
                uses_target_placeholder: command_template.contains("{target}"),
                command_template,
                cleanup_command: None,
                target_candidates: target_candidates.clone(),
                cwd: cwd.clone(),
                cwd_display: cwd_display.clone(),
                env: suite.env.clone(),
                timeout_seconds,
            });
        }

        if let Some(command_template) = suite
            .test_setup
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_owned)
        {
            plans.push(ReadinessCommandPlan {
                suite_name: suite.name.clone(),
                kind: ReadinessCommandKind::TestSetup,
                uses_target_placeholder: command_template.contains("{target}"),
                command_template,
                cleanup_command: None,
                target_candidates: target_candidates.clone(),
                cwd: cwd.clone(),
                cwd_display: cwd_display.clone(),
                env: suite.env.clone(),
                timeout_seconds,
            });
        }

        if let Some(command_template) = suite
            .lint_command
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_owned)
        {
            plans.push(ReadinessCommandPlan {
                suite_name: suite.name.clone(),
                kind: ReadinessCommandKind::Lint,
                uses_target_placeholder: command_template.contains("{target}"),
                command_template,
                cleanup_command: None,
                target_candidates: target_candidates.clone(),
                cwd: cwd.clone(),
                cwd_display: cwd_display.clone(),
                env: suite.env.clone(),
                timeout_seconds,
            });
        }

        if let Some(command_template) =
            Some(suite.test_command.trim().to_owned()).filter(|command| !command.is_empty())
        {
            plans.push(ReadinessCommandPlan {
                suite_name: suite.name.clone(),
                kind: ReadinessCommandKind::Test,
                uses_target_placeholder: command_template.contains("{target}"),
                command_template,
                cleanup_command: suite.cleanup_command.clone(),
                target_candidates,
                cwd,
                cwd_display,
                env: suite.env.clone(),
                timeout_seconds,
            });
        }
    }

    plans
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

fn normalized_default_target(suite: &TestSuiteConfig) -> Option<String> {
    suite
        .default_target
        .as_deref()
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(str::to_owned)
}

fn collect_readiness_targets(project_dir: &Path, suite: &TestSuiteConfig) -> Vec<String> {
    let patterns = suite
        .file_patterns
        .iter()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
        .filter_map(|pattern| glob::Pattern::new(pattern).ok())
        .collect::<Vec<_>>();

    if patterns.is_empty() {
        return Vec::new();
    }

    let tracked_files = git_list_tracked_files(project_dir, suite.test_root.trim());
    if tracked_files.is_empty() {
        return Vec::new();
    }

    let root_prefix = normalized_root_prefix(suite.test_root.trim());
    let mut targets = std::collections::BTreeSet::new();
    for file in tracked_files {
        let relative = strip_root_prefix(&file, root_prefix.as_deref()).unwrap_or(file.as_str());
        let matches_pattern = patterns
            .iter()
            .any(|pattern| pattern.matches(relative) || pattern.matches(&file));
        if !matches_pattern {
            continue;
        }

        let selected = if suite.strip_root_from_target {
            relative.to_owned()
        } else {
            file.clone()
        };
        if !selected.trim().is_empty() {
            targets.insert(selected);
        }
    }
    targets.into_iter().collect()
}

fn chief_yaml_content_hash(config_path: &Path) -> anyhow::Result<String> {
    let content = fs::read(config_path)
        .with_context(|| format!("failed to read chief config at {}", config_path.display()))?;
    Ok(format!("{:x}", md5::compute(content)))
}

fn should_run_readiness_check(
    store: &ProjectStore,
    chief_yaml_hash: &str,
    suite_cache_inputs_hash: &str,
) -> anyhow::Result<bool> {
    let readiness = store.get_readiness_state()?;
    if readiness.status != ReadinessStatus::Ready {
        return Ok(true);
    }
    let previous_hash = readiness_chief_yaml_hash(&readiness.details);
    if previous_hash != Some(chief_yaml_hash) {
        return Ok(true);
    }
    let previous_suite_cache_hash = readiness_suite_cache_inputs_hash(&readiness.details);
    Ok(previous_suite_cache_hash
        .map(|value| value != suite_cache_inputs_hash)
        .unwrap_or(false))
}

fn readiness_chief_yaml_hash(details: &serde_json::Value) -> Option<&str> {
    details
        .get("chief_yaml_hash")
        .and_then(serde_json::Value::as_str)
}

fn readiness_suite_cache_inputs_hash(details: &serde_json::Value) -> Option<&str> {
    details
        .get("suite_cache_inputs_hash")
        .and_then(serde_json::Value::as_str)
}

fn git_list_tracked_files(project_dir: &Path, test_root: &str) -> Vec<String> {
    let output = if test_root.is_empty() || test_root == "." {
        run_git_command_with_retry(project_dir, &["ls-files", "--"])
    } else {
        run_git_command_with_retry(project_dir, &["ls-files", "--", test_root])
    };

    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    let mut files = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn normalized_root_prefix(test_root: &str) -> Option<String> {
    let trimmed = test_root.trim();
    if trimmed.is_empty() || trimmed == "." {
        return None;
    }
    Some(trimmed.trim_end_matches('/').to_owned())
}

fn strip_root_prefix<'a>(path: &'a str, root_prefix: Option<&str>) -> Option<&'a str> {
    let Some(root_prefix) = root_prefix else {
        return Some(path);
    };

    if path == root_prefix {
        return Some("");
    }
    let prefix = format!("{root_prefix}/");
    path.strip_prefix(&prefix)
}

fn replace_target_placeholder(command_template: &str, target: &str) -> String {
    command_template.replace("{target}", target)
}

fn readiness_payload(stage: &str) -> BTreeMap<String, serde_json::Value> {
    let mut payload = BTreeMap::new();
    payload.insert(
        "source".to_owned(),
        serde_json::Value::String(READINESS_EVENT_SOURCE.to_owned()),
    );
    payload.insert(
        "stage".to_owned(),
        serde_json::Value::String(stage.to_owned()),
    );
    payload
}

fn record_readiness_event(
    log_context: Option<&ReadinessLogContext>,
    level: &str,
    msg: impl Into<String>,
    mut payload: BTreeMap<String, serde_json::Value>,
) {
    let Some(log_context) = log_context else {
        return;
    };

    payload.insert(
        "source".to_owned(),
        serde_json::Value::String(READINESS_EVENT_SOURCE.to_owned()),
    );
    let event = EventRecord {
        id: None,
        run_id: log_context.run_id.clone(),
        job_id: None,
        todo_id: None,
        timestamp: Utc::now(),
        level: level.to_owned(),
        phase: None,
        msg: msg.into(),
        event_type: EventType::Msg,
        payload,
    };

    if let Err(err) = log_context.store.record_event(&event) {
        warn!(
            run_id = %log_context.run_id,
            error = %err,
            "failed to record readiness event"
        );
    }
}

fn suite_kind_for_readiness(kind: ReadinessCommandKind) -> SuiteCommandKind {
    match kind {
        ReadinessCommandKind::Lint => SuiteCommandKind::Lint,
        ReadinessCommandKind::TestInit
        | ReadinessCommandKind::TestSetup
        | ReadinessCommandKind::Test => SuiteCommandKind::Test,
    }
}

fn execute_readiness_command_plans(
    plans: Vec<ReadinessCommandPlan>,
    cancel_signal: Arc<AtomicBool>,
    stream_context: Option<ReadinessStreamContext>,
) -> anyhow::Result<Vec<ReadinessCommandResult>> {
    let mut results = Vec::with_capacity(plans.len());

    for plan in plans {
        if cancel_signal.load(std::sync::atomic::Ordering::SeqCst) {
            if let Some(stream_context) = stream_context.as_ref() {
                stream_context.push_text("Pre-run checks cancelled by user.\n");
            }
            return Err(anyhow!("pre-run checks cancelled by user"));
        }

        if !plan.cwd.exists() {
            if let Some(stream_context) = stream_context.as_ref() {
                stream_context.push_text(format!(
                    "[pre-run-checks:{}:{}] working directory does not exist: {}\n",
                    plan.suite_name,
                    plan.kind.as_str(),
                    plan.cwd.display()
                ));
            }
            results.push(ReadinessCommandResult {
                suite_name: plan.suite_name,
                kind: plan.kind,
                command: plan.command_template,
                cwd: plan.cwd_display,
                target: None,
                exit_code: 127,
                blocking_failure: true,
                output_tail: format!("working directory does not exist: {}", plan.cwd.display()),
            });
            continue;
        }

        if !plan.cwd.is_dir() {
            if let Some(stream_context) = stream_context.as_ref() {
                stream_context.push_text(format!(
                    "[pre-run-checks:{}:{}] working directory is not a directory: {}\n",
                    plan.suite_name,
                    plan.kind.as_str(),
                    plan.cwd.display()
                ));
            }
            results.push(ReadinessCommandResult {
                suite_name: plan.suite_name,
                kind: plan.kind,
                command: plan.command_template,
                cwd: plan.cwd_display,
                target: None,
                exit_code: 127,
                blocking_failure: true,
                output_tail: format!(
                    "working directory is not a directory: {}",
                    plan.cwd.display()
                ),
            });
            continue;
        }

        if !plan.uses_target_placeholder {
            results.push(run_readiness_command_attempt(
                &plan,
                plan.command_template.clone(),
                None,
                &cancel_signal,
                stream_context.as_ref(),
            ));
            if cancel_signal.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(stream_context) = stream_context.as_ref() {
                    stream_context.push_text("Pre-run checks cancelled by user.\n");
                }
                return Err(anyhow!("pre-run checks cancelled by user"));
            }
            continue;
        }

        if plan.target_candidates.is_empty() {
            if let Some(stream_context) = stream_context.as_ref() {
                stream_context.push_text(format!(
                    "[pre-run-checks:{}:{}] command uses {{target}}, but no file_patterns target matched and default_target is not set\n",
                    plan.suite_name,
                    plan.kind.as_str()
                ));
            }
            results.push(ReadinessCommandResult {
                suite_name: plan.suite_name,
                kind: plan.kind,
                command: plan.command_template,
                cwd: plan.cwd_display,
                target: None,
                exit_code: 127,
                blocking_failure: true,
                output_tail: "command uses {target}, but no file_patterns target matched and default_target is not set".to_owned(),
            });
            continue;
        }

        let mut selected: Option<ReadinessCommandResult> = None;
        for target in &plan.target_candidates {
            if cancel_signal.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(stream_context) = stream_context.as_ref() {
                    stream_context.push_text("Pre-run checks cancelled by user.\n");
                }
                return Err(anyhow!("pre-run checks cancelled by user"));
            }
            let command = replace_target_placeholder(&plan.command_template, target);
            let attempt = run_readiness_command_attempt(
                &plan,
                command,
                Some(target.clone()),
                &cancel_signal,
                stream_context.as_ref(),
            );
            let runnable = !attempt.blocking_failure;
            selected = Some(attempt);
            if runnable {
                break;
            }
        }
        if let Some(result) = selected {
            results.push(result);
            if cancel_signal.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(stream_context) = stream_context.as_ref() {
                    stream_context.push_text("Pre-run checks cancelled by user.\n");
                }
                return Err(anyhow!("pre-run checks cancelled by user"));
            }
        }
    }

    Ok(results)
}

fn run_readiness_command_attempt(
    plan: &ReadinessCommandPlan,
    command: String,
    target: Option<String>,
    cancel_signal: &Arc<AtomicBool>,
    stream_context: Option<&ReadinessStreamContext>,
) -> ReadinessCommandResult {
    if let Some(stream_context) = stream_context {
        stream_context.push_text(format!(
            "[pre-run-checks:{}:{}]$ {} (cwd: {})\n",
            plan.suite_name,
            plan.kind.as_str(),
            command,
            plan.cwd_display
        ));
    }

    let out = execute_suite_command_streaming(
        &plan.suite_name,
        suite_kind_for_readiness(plan.kind),
        &command,
        &plan.cwd,
        &plan.cwd_display,
        &plan.env,
        plan.timeout_seconds,
        Some(cancel_signal),
        |_stream, text| {
            if let Some(stream_context) = stream_context {
                stream_context.push_text(text);
            }
        },
    );

    let cleanup_out = if plan.kind == ReadinessCommandKind::Test {
        execute_suite_cleanup_command(
            plan.cleanup_command.as_deref(),
            &plan.cwd,
            &plan.env,
            Some(plan.timeout_seconds),
        )
    } else {
        Ok(None)
    };

    match out {
        Ok(out) => {
            match cleanup_out {
                Ok(Some(cleanup)) => {
                    if let Some(stream_context) = stream_context {
                        stream_context.push_text(format!(
                            "[pre-run-checks:{}:cleanup] exit={}\n",
                            plan.suite_name, cleanup.exit_code
                        ));
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    if let Some(stream_context) = stream_context {
                        stream_context.push_text(format!(
                            "[pre-run-checks:{}:cleanup] failed: {}\n",
                            plan.suite_name, err
                        ));
                    }
                }
            }
            let blocking_failure = readiness_exit_code_is_blocking(plan.kind, out.exit_code);
            if let Some(stream_context) = stream_context {
                stream_context.push_text(format!(
                    "[pre-run-checks:{}:{}] exit={}{}\n",
                    plan.suite_name,
                    plan.kind.as_str(),
                    out.exit_code,
                    if blocking_failure { " (blocking)" } else { "" }
                ));
            }
            ReadinessCommandResult {
                suite_name: plan.suite_name.clone(),
                kind: plan.kind,
                command: out.command,
                cwd: plan.cwd_display.clone(),
                target,
                exit_code: out.exit_code,
                blocking_failure,
                output_tail: readiness_output_tail(&out.output),
            }
        }
        Err(err) => {
            match cleanup_out {
                Ok(Some(cleanup)) => {
                    if let Some(stream_context) = stream_context {
                        stream_context.push_text(format!(
                            "[pre-run-checks:{}:cleanup] exit={}\n",
                            plan.suite_name, cleanup.exit_code
                        ));
                    }
                }
                Ok(None) => {}
                Err(cleanup_err) => {
                    if let Some(stream_context) = stream_context {
                        stream_context.push_text(format!(
                            "[pre-run-checks:{}:cleanup] failed: {}\n",
                            plan.suite_name, cleanup_err
                        ));
                    }
                }
            }
            if let Some(stream_context) = stream_context {
                stream_context.push_text(format!(
                    "[pre-run-checks:{}:{}] failed: {}\n",
                    plan.suite_name,
                    plan.kind.as_str(),
                    err
                ));
            }
            ReadinessCommandResult {
                suite_name: plan.suite_name.clone(),
                kind: plan.kind,
                command,
                cwd: plan.cwd_display.clone(),
                target,
                exit_code: 127,
                blocking_failure: true,
                output_tail: readiness_output_tail(&err.to_string()),
            }
        }
    }
}

fn readiness_exit_code_is_blocking(kind: ReadinessCommandKind, exit_code: i32) -> bool {
    match kind {
        ReadinessCommandKind::TestInit | ReadinessCommandKind::TestSetup => exit_code != 0,
        ReadinessCommandKind::Lint | ReadinessCommandKind::Test => !matches!(exit_code, 0 | 1 | 5),
    }
}

fn readiness_output_tail(output: &str) -> String {
    let lines = output
        .lines()
        .rev()
        .take(25)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");

    if lines.chars().count() > 2_000 {
        let reversed = lines.chars().rev().take(2_000).collect::<String>();
        return reversed.chars().rev().collect();
    }

    lines
}

fn build_readiness_summary(results: &[ReadinessCommandResult], suite_count: usize) -> String {
    if suite_count == 0 {
        return "Ready: no suites configured, so pre-run checks skipped command execution."
            .to_owned();
    }

    if results.is_empty() {
        return format!(
            "Ready: no runnable suite commands detected across {suite_count} suite(s)."
        );
    }

    let failed_commands = results
        .iter()
        .filter(|result| result.blocking_failure)
        .count();

    if failed_commands == 0 {
        format!(
            "Ready: validated {} command(s) across {suite_count} suite(s).",
            results.len()
        )
    } else {
        format!(
            "Not ready: {} command(s) failed across {} checked command(s).",
            failed_commands,
            results.len()
        )
    }
}

fn build_readiness_details(
    results: &[ReadinessCommandResult],
    suite_count: usize,
    chief_yaml_hash: &str,
    suite_cache_inputs_hash: &str,
) -> serde_json::Value {
    let failed_commands = results
        .iter()
        .filter(|result| result.blocking_failure)
        .count();

    json!({
        "suite_count": suite_count,
        "commands_total": results.len(),
        "commands_failed": failed_commands,
        "chief_yaml_hash": chief_yaml_hash,
        "suite_cache_inputs_hash": suite_cache_inputs_hash,
        "commands": results
            .iter()
            .map(|result| json!({
                "suite": result.suite_name,
                "kind": result.kind.as_str(),
                "command": result.command,
                "cwd": result.cwd,
                "target": result.target,
                "exit_code": result.exit_code,
                "failed": result.blocking_failure,
                "output_tail": result.output_tail,
            }))
            .collect::<Vec<_>>()
    })
}

fn trim_leading_bytes(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }

    let bytes_to_trim = text.len().saturating_sub(max_bytes);
    let split_at = text
        .char_indices()
        .find_map(|(index, _)| (index >= bytes_to_trim).then_some(index))
        .unwrap_or(text.len());
    text.drain(..split_at);
}

enum SuiteStreamChunk {
    Chunk {
        stream: SuiteCheckOutputStream,
        text: String,
    },
    Done {
        stream: SuiteCheckOutputStream,
    },
    Error {
        stream: SuiteCheckOutputStream,
        message: String,
    },
}

fn execute_suite_command_streaming<F>(
    suite_name: &str,
    kind: SuiteCommandKind,
    command: &str,
    cwd: &std::path::Path,
    cwd_display: &str,
    env: &BTreeMap<String, String>,
    timeout_seconds: u64,
    cancel_signal: Option<&Arc<AtomicBool>>,
    mut on_chunk: F,
) -> anyhow::Result<RunSuiteCheckResponse>
where
    F: FnMut(SuiteCheckOutputStream, &str),
{
    let mut process = Command::new("sh");
    process.arg("-lc").arg(command);
    process.current_dir(cwd);
    process.envs(env.iter());
    process.stdout(Stdio::piped());
    process.stderr(Stdio::piped());
    configure_process_group(&mut process);
    let mut child = process
        .spawn()
        .with_context(|| format!("failed to run command: {command}"))?;

    let (chunk_sender, chunk_receiver) = mpsc::channel::<SuiteStreamChunk>();
    let stdout_reader = spawn_suite_stream_reader(
        child.stdout.take(),
        SuiteCheckOutputStream::Stdout,
        chunk_sender.clone(),
    );
    let stderr_reader = spawn_suite_stream_reader(
        child.stderr.take(),
        SuiteCheckOutputStream::Stderr,
        chunk_sender,
    );

    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut merged_output = String::new();
    let mut read_error: Option<String> = None;
    let timeout = Duration::from_secs(timeout_seconds.max(1));
    let started = Instant::now();
    let mut timed_out = false;
    let mut cancelled_by_user = false;

    while !(stdout_done && stderr_done) {
        if cancel_signal.is_some_and(|signal| signal.load(std::sync::atomic::Ordering::SeqCst)) {
            cancelled_by_user = true;
            break;
        }

        let chunk = match chunk_receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => chunk,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if cancel_signal
                    .is_some_and(|signal| signal.load(std::sync::atomic::Ordering::SeqCst))
                {
                    cancelled_by_user = true;
                    break;
                }
                if started.elapsed() >= timeout {
                    timed_out = true;
                    break;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow!("suite command output stream disconnected"));
            }
        };

        match chunk {
            SuiteStreamChunk::Chunk { stream, text } => {
                match stream {
                    SuiteCheckOutputStream::Stdout => stdout.push_str(&text),
                    SuiteCheckOutputStream::Stderr => stderr.push_str(&text),
                }
                merged_output.push_str(&text);
                on_chunk(stream, &text);
            }
            SuiteStreamChunk::Done { stream } => match stream {
                SuiteCheckOutputStream::Stdout => stdout_done = true,
                SuiteCheckOutputStream::Stderr => stderr_done = true,
            },
            SuiteStreamChunk::Error { stream, message } => {
                read_error = Some(format!(
                    "failed reading {} stream: {message}",
                    suite_stream_label(stream)
                ));
                break;
            }
        }
    }

    if read_error.is_some() || timed_out || cancelled_by_user {
        terminate_process_tree(&mut child);
    }
    let status = child.wait().context("failed waiting for suite command")?;
    join_suite_stream_reader(stdout_reader, "stdout")?;
    join_suite_stream_reader(stderr_reader, "stderr")?;
    if let Some(message) = read_error {
        return Err(anyhow!(message));
    }
    if cancelled_by_user {
        return Err(anyhow!("suite command cancelled by user"));
    }

    if timed_out {
        let timeout_message = format!(
            "suite command timed out after {} second(s) and was terminated.",
            timeout_seconds.max(1)
        );
        merged_output = if merged_output.trim().is_empty() {
            timeout_message.clone()
        } else {
            format!("{timeout_message}\n{}", merged_output.trim())
        };
        if !stderr.contains(&timeout_message) {
            if stderr.trim().is_empty() {
                stderr = timeout_message;
            } else {
                stderr = format!("{stderr}\n{timeout_message}");
            }
        }
    }

    Ok(RunSuiteCheckResponse {
        suite: suite_name.to_owned(),
        kind,
        command: command.to_owned(),
        cwd: cwd_display.to_owned(),
        exit_code: if timed_out {
            124
        } else {
            status.code().unwrap_or(1)
        },
        output: merged_output.trim().to_owned(),
        stdout,
        stderr,
    })
}

fn spawn_suite_stream_reader<T>(
    pipe: Option<T>,
    stream: SuiteCheckOutputStream,
    sender: mpsc::Sender<SuiteStreamChunk>,
) -> JoinHandle<anyhow::Result<()>>
where
    T: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let Some(pipe) = pipe else {
            let _ = sender.send(SuiteStreamChunk::Done { stream });
            return Ok(());
        };

        let mut reader = BufReader::new(pipe);
        loop {
            let mut bytes = Vec::new();
            match reader.read_until(b'\n', &mut bytes) {
                Ok(0) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    if sender
                        .send(SuiteStreamChunk::Chunk { stream, text })
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                Err(err) => {
                    let _ = sender.send(SuiteStreamChunk::Error {
                        stream,
                        message: err.to_string(),
                    });
                    return Ok(());
                }
            }
        }

        let _ = sender.send(SuiteStreamChunk::Done { stream });
        Ok(())
    })
}

fn join_suite_stream_reader(
    handle: JoinHandle<anyhow::Result<()>>,
    stream_name: &str,
) -> anyhow::Result<()> {
    match handle.join() {
        Ok(result) => result.with_context(|| format!("failed reading {stream_name} stream")),
        Err(_) => Err(anyhow!("{stream_name} stream reader thread panicked")),
    }
}

fn suite_stream_label(stream: SuiteCheckOutputStream) -> &'static str {
    match stream {
        SuiteCheckOutputStream::Stdout => "stdout",
        SuiteCheckOutputStream::Stderr => "stderr",
    }
}

fn send_stream_event_blocking(
    sender: &tokio_mpsc::Sender<Vec<u8>>,
    event: RunSuiteCheckStreamEvent,
) -> bool {
    let payload = match stream_event_payload(&event) {
        Ok(payload) => payload,
        Err(err) => {
            error!(error = %err, "failed to encode suite stream event");
            return false;
        }
    };
    sender.blocking_send(payload).is_ok()
}

async fn send_stream_event_async(
    sender: &tokio_mpsc::Sender<Vec<u8>>,
    event: RunSuiteCheckStreamEvent,
) -> bool {
    let payload = match stream_event_payload(&event) {
        Ok(payload) => payload,
        Err(err) => {
            error!(error = %err, "failed to encode suite stream event");
            return false;
        }
    };
    sender.send(payload).await.is_ok()
}

fn stream_event_payload(event: &RunSuiteCheckStreamEvent) -> anyhow::Result<Vec<u8>> {
    let mut payload = serde_json::to_vec(event).context("failed serializing suite stream event")?;
    payload.push(b'\n');
    Ok(payload)
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

fn is_internal_workspace_state_file(path: &str) -> bool {
    path == "chief.db" || path.starts_with("chief.db-")
}

fn resolve_last_done_todo_committed_at(
    git: &impl GitOps,
    project_dir: &std::path::Path,
    todos: &[Todo],
) -> Option<String> {
    let mut seen_commits = HashSet::new();
    let mut latest_timestamp: Option<chrono::DateTime<chrono::Utc>> = None;

    for commit_hash in todos
        .iter()
        .filter(|todo| todo.status == TodoStatus::Done)
        .filter_map(|todo| todo.done_at_commit.as_deref())
        .map(str::trim)
        .filter(|commit_hash| !commit_hash.is_empty())
    {
        if !seen_commits.insert(commit_hash.to_owned()) {
            continue;
        }

        let timestamp = match git.commit_committer_timestamp_rfc3339(project_dir, commit_hash) {
            Ok(value) => value,
            Err(_) => continue,
        };

        let parsed = match chrono::DateTime::parse_from_rfc3339(&timestamp) {
            Ok(value) => value.with_timezone(&chrono::Utc),
            Err(_) => continue,
        };

        if latest_timestamp
            .as_ref()
            .map(|current| parsed > *current)
            .unwrap_or(true)
        {
            latest_timestamp = Some(parsed);
        }
    }

    latest_timestamp.map(|timestamp| timestamp.to_rfc3339())
}

fn parse_loop_iteration(msg: &str) -> Option<PhaseIteration> {
    let marker = "iteration ";
    let idx = msg.find(marker)?;
    let segment = msg[idx + marker.len()..]
        .split_whitespace()
        .next()
        .unwrap_or_default();
    let mut parts = segment.split('/');
    let current = parts.next()?.trim().parse::<usize>().ok()?;
    let max = parts.next()?.trim().parse::<usize>().ok()?;
    Some(PhaseIteration { current, max })
}

fn parse_todo_status_input(value: &str) -> Option<TodoStatus> {
    match value.trim() {
        "pending" => Some(TodoStatus::Pending),
        "in_progress" => Some(TodoStatus::InProgress),
        "done" => Some(TodoStatus::Done),
        _ => None,
    }
}

fn parse_requested_types(input: Option<&str>) -> Vec<String> {
    let Some(raw) = input else {
        return Vec::new();
    };

    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn matches_requested_type(event_type: EventType, requested: &[String]) -> bool {
    if requested.is_empty() {
        return true;
    }

    let event_name = event_type.as_str();
    let group = event_group(event_type);

    requested.iter().any(|item| {
        item == event_name
            || item == group
            || (item == "prompts" && group == "prompt")
            || (item == "tests" && group == "test")
            || (item == "logs" && group == "log")
    })
}

fn event_group(event_type: EventType) -> &'static str {
    match event_type {
        EventType::AgentCmd | EventType::AgentPrompt | EventType::AgentResponse => "prompt",
        EventType::Diff | EventType::GitOp => "code",
        EventType::TestRun
        | EventType::PostGreenOutput
        | EventType::Lint
        | EventType::LintFix
        | EventType::PhaseFailure => "test",
        EventType::Msg | EventType::PhaseChange | EventType::Error | EventType::Job => "log",
    }
}

fn parse_event_type(value: &str) -> Result<EventType, ApiError> {
    match value {
        "msg" => Ok(EventType::Msg),
        "test_run" => Ok(EventType::TestRun),
        "post_green_output" => Ok(EventType::PostGreenOutput),
        "lint" => Ok(EventType::Lint),
        "lint_fix" => Ok(EventType::LintFix),
        "phase_change" => Ok(EventType::PhaseChange),
        "git_op" => Ok(EventType::GitOp),
        "diff" => Ok(EventType::Diff),
        "agent_cmd" => Ok(EventType::AgentCmd),
        "agent_prompt" => Ok(EventType::AgentPrompt),
        "agent_response" => Ok(EventType::AgentResponse),
        "phase_failure" => Ok(EventType::PhaseFailure),
        "error" => Ok(EventType::Error),
        "job" => Ok(EventType::Job),
        other => Err(ApiError::unprocessable(format!(
            "unsupported event_type '{other}', see /api/projects/{{project}}/events for valid values"
        ))),
    }
}

fn parse_phase(value: &str) -> Result<Phase, ApiError> {
    match value {
        "start" => Ok(Phase::Start),
        "todo_selection" => Ok(Phase::TodoSelection),
        "red" => Ok(Phase::Red),
        "green" => Ok(Phase::Green),
        "single_prompt" => Ok(Phase::SinglePrompt),
        "post_green" => Ok(Phase::PostGreen),
        "exit" => Ok(Phase::Exit),
        other => Err(ApiError::unprocessable(format!(
            "unsupported phase '{other}'"
        ))),
    }
}

#[cfg(test)]
#[path = "service/tests.rs"]
mod tests;
