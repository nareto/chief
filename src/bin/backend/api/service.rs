use crate::api::error::ApiError;
use crate::api::types::{
    ActiveJobResponse, AddTodoRequest, ChiefYamlResponse, EventsQuery, EventsResponse,
    FileDiffQuery, FileDiffResponse, JobsResponse, LogQuery, MessageResponse, PhaseIteration,
    ProjectsResponse, RequirementsRequest, RequirementsResponse, RunSuiteCheckRequest,
    RunSuiteCheckResponse, RunSuiteCheckStreamEvent, StartProjectRequest, StateResponse,
    SuiteCheckOutputStream, TodoProgress, TodoResponse, TodosResponse, UpdateChiefYamlRequest,
    UpdateTodoRequest,
};
use anyhow::{Context, anyhow};
use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use chief::domain::{EventType, JobStatus, Phase, Todo, TodoStatus};
use chief::flow::{
    FlowKind, SuiteCommandKind, execute_suite_command, suite_command_cwd, suite_command_for_kind,
};
use chief::git::GitOps;
use chief::scheduler::Scheduler;
use chief::service::ChiefEngine;
use chief::storage::EventQuery;
use futures_util::stream;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{error, info};

#[derive(Clone)]
pub struct ApiService {
    scheduler: Scheduler,
    default_agents_per_project: usize,
}

struct SuiteCheckPlan {
    suite_name: String,
    kind: SuiteCommandKind,
    command: String,
    cwd: PathBuf,
    cwd_display: String,
    env: BTreeMap<String, String>,
    timeout_seconds: u64,
}

impl ApiService {
    pub fn new(scheduler: Scheduler, default_agents_per_project: usize) -> Self {
        Self {
            scheduler,
            default_agents_per_project: default_agents_per_project.max(1),
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
                .ok_or_else(|| ApiError::unprocessable(format!("invalid todo status '{}'", raw)))?,
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

        let updated = context.store.update_todo(todo_id, todo).map_err(|err| {
            let message = err.to_string();
            if message.contains("not found") {
                ApiError::not_found(message)
            } else if message.contains("already exists") {
                ApiError::unprocessable(message)
            } else {
                ApiError::internal(err)
            }
        })?;

        Ok(TodoResponse { todo: updated })
    }

    pub async fn delete_todo(
        &self,
        project: &str,
        todo_id: &str,
    ) -> Result<MessageResponse, ApiError> {
        let context = self.project_context(project).await?;

        context.store.delete_todo(todo_id).map_err(|err| {
            let message = err.to_string();
            if message.contains("not found") {
                ApiError::not_found(message)
            } else {
                ApiError::internal(err)
            }
        })?;

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
            .filter(|todo| todo.status != TodoStatus::Done && todo.status != TodoStatus::InProgress)
            .count();

        let configured_flow = context.chief_yaml.chief.flow.trim();
        let configured_flow_name = configured_flow
            .parse::<FlowKind>()
            .map(|kind| kind.as_str().to_owned())
            .unwrap_or_else(|_| {
                if configured_flow.is_empty() {
                    FlowKind::SinglePrompt.as_str().to_owned()
                } else {
                    configured_flow.to_owned()
                }
            });

        Ok(StateResponse {
            project: project.to_owned(),
            running: runtime.as_ref().map(|view| view.running).unwrap_or(false),
            stop_requested: runtime
                .as_ref()
                .map(|view| view.stop_requested)
                .unwrap_or(false),
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
            chief_db_size_bytes,
            dirty_files,
            todos: TodoProgress {
                available: available_todos,
                completed: completed_todos,
                total: todos.len(),
            },
            active_job,
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
        fs::write(&context.config_path, payload.content).with_context(|| {
            format!(
                "failed to write chief config at {}",
                context.config_path.display()
            )
        })?;

        Ok(MessageResponse {
            message: "chief.yaml updated".to_owned(),
        })
    }

    pub async fn run_suite_check(
        &self,
        project: &str,
        payload: RunSuiteCheckRequest,
    ) -> Result<RunSuiteCheckResponse, ApiError> {
        let SuiteCheckPlan {
            suite_name,
            kind,
            command,
            cwd,
            cwd_display,
            env,
            timeout_seconds,
        } = self.prepare_suite_check_plan(project, &payload).await?;
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
            execute_suite_command(&command, &cwd, &env, &cancel_signal, Some(timeout_seconds))
        })
        .await
        .map_err(|err| {
            error!(
                project,
                suite = %suite_name,
                kind = %kind_label,
                error = %err,
                "suite command task join failed"
            );
            ApiError::internal(anyhow!("suite command task failed: {err}"))
        })?;

        let output = match output {
            Ok(result) => result,
            Err(err) => {
                error!(
                    project,
                    suite = %suite_name,
                    kind = %kind_label,
                    error = %err,
                    "suite command execution failed"
                );
                return Err(ApiError::internal(err));
            }
        };

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

    pub async fn run_suite_check_stream(
        &self,
        project: &str,
        payload: RunSuiteCheckRequest,
    ) -> Result<Response, ApiError> {
        let plan = self.prepare_suite_check_plan(project, &payload).await?;
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
                cwd,
                cwd_display,
                env,
                timeout_seconds,
            } = plan;
            let kind_label = kind.as_str().to_owned();

            match execute_suite_command_streaming(
                &suite_name,
                kind,
                &command,
                &cwd,
                &cwd_display,
                &env,
                timeout_seconds,
                &sender,
            ) {
                Ok(result) => {
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

    async fn prepare_suite_check_plan(
        &self,
        project: &str,
        payload: &RunSuiteCheckRequest,
    ) -> Result<SuiteCheckPlan, ApiError> {
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

        let cwd = suite_command_cwd(&context.project_dir, &suite);
        let cwd_display = cwd.display().to_string();

        Ok(SuiteCheckPlan {
            suite_name: suite.name,
            kind: payload.kind,
            command,
            cwd,
            cwd_display,
            env: suite.env,
            timeout_seconds: suite
                .command_timeout_seconds
                .unwrap_or(context.chief_yaml.chief.suite_command_timeout_seconds)
                .max(1),
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
            .map_err(|err| {
                let message = err.to_string();
                if message.contains("not found") {
                    ApiError::not_found(message)
                } else {
                    ApiError::internal(err)
                }
            })
    }
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

fn execute_suite_command_streaming(
    suite_name: &str,
    kind: SuiteCommandKind,
    command: &str,
    cwd: &std::path::Path,
    cwd_display: &str,
    env: &BTreeMap<String, String>,
    timeout_seconds: u64,
    stream_sender: &tokio_mpsc::Sender<Vec<u8>>,
) -> anyhow::Result<RunSuiteCheckResponse> {
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

    while !(stdout_done && stderr_done) {
        let chunk = match chunk_receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => chunk,
            Err(mpsc::RecvTimeoutError::Timeout) => {
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
                send_stream_event_blocking(
                    stream_sender,
                    RunSuiteCheckStreamEvent::Chunk { stream, text },
                );
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

    if read_error.is_some() || timed_out {
        terminate_process_tree(&mut child);
    }
    let status = child.wait().context("failed waiting for suite command")?;
    join_suite_stream_reader(stdout_reader, "stdout")?;
    join_suite_stream_reader(stderr_reader, "stderr")?;
    if let Some(message) = read_error {
        return Err(anyhow!(message));
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

fn configure_process_group(process: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.process_group(0);
    }
}

fn terminate_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        if let Ok(pid) = i32::try_from(child.id()) {
            let pgid = nix::unistd::Pid::from_raw(pid);
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGTERM);
            std::thread::sleep(Duration::from_millis(200));
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
        }
    }
    let _ = child.kill();
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
    let output = Command::new("git")
        .arg("-c")
        .arg("safe.directory=*")
        .args(args)
        .current_dir(project_dir)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))
        .map_err(ApiError::internal)?;

    if !output.status.success() {
        return Err(ApiError::bad_request(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
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
        "attempted" => Some(TodoStatus::Attempted),
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
            "unsupported event_type '{}', see /api/projects/{{project}}/events for valid values",
            other
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
            "unsupported phase '{}'",
            other
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::ApiService;
    use crate::api::error::ApiError;
    use crate::api::types::StartProjectRequest;
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use chief::scheduler::Scheduler;
    use chief::service::ProjectRegistry;
    use chief::storage::ProjectStore;
    use rusqlite::Connection;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use uuid::Uuid;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("chief-api-service-{label}-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("failed to create temporary directory");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn run_git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-c")
            .arg("safe.directory=*")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap_or_else(|err| panic!("failed to run git {}: {err}", args.join(" ")));
        assert!(
            output.status.success(),
            "git {} failed: stdout={} stderr={}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn init_git_repo(project_dir: &Path) {
        run_git(project_dir, &["init"]);
        run_git(
            project_dir,
            &[
                "config",
                "user.email",
                "chief-api-service-tests@example.com",
            ],
        );
        run_git(
            project_dir,
            &["config", "user.name", "Chief API Service Tests"],
        );
    }

    fn write_todos(project_dir: &Path, todos_yaml: &str) {
        fs::write(project_dir.join("todos.yaml"), format!("{todos_yaml}\n"))
            .expect("failed to write todos.yaml");
    }

    fn write_chief_yaml(project_dir: &Path, chief_yaml: &str) {
        fs::write(project_dir.join("chief.yaml"), format!("{chief_yaml}\n"))
            .expect("failed to write chief.yaml");
    }

    fn setup_service(initial_todos_yaml: &str) -> (TempDir, ApiService, String, PathBuf) {
        let workspace = TempDir::new("workspace");
        let project_name = format!("project-{}", Uuid::new_v4());
        let project_dir = workspace.path.join(&project_name);
        fs::create_dir_all(&project_dir).expect("failed to create project directory");

        init_git_repo(&project_dir);
        let store = ProjectStore::new(&project_dir);
        store.init().expect("store init should succeed");
        write_todos(&project_dir, initial_todos_yaml);

        run_git(&project_dir, &["add", "--all"]);
        run_git(&project_dir, &["commit", "-m", "chore: baseline"]);

        store
            .reset_db_from_todos_file()
            .expect("reset_db_from_todos_file should seed sqlite from todos.yaml");

        let registry = ProjectRegistry::discover(&workspace.path, &[])
            .expect("project discovery should succeed");
        let scheduler = Scheduler::new(registry, 4);
        let service = ApiService::new(scheduler, 1);
        (workspace, service, project_name, project_dir)
    }

    async fn assert_invalid_yaml_api_error(err: ApiError) {
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error response body should be readable");
        let payload: serde_json::Value =
            serde_json::from_slice(&body).expect("error response should be JSON");
        let message = payload
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(
            message.contains("invalid YAML in"),
            "expected invalid YAML error, got: {message}"
        );
    }

    #[tokio::test]
    async fn start_project_rejects_missing_chief_yaml_without_creating_run_or_job_records() {
        let (_workspace, service, project, project_dir) = setup_service(
            r#"todos:
  - id: pending-1
    todo: Example pending todo
    expectations: Example expectations
    priority: 1
    test_suites: []
    status: pending"#,
        );
        let expected_config_path = project_dir.join("chief.yaml").display().to_string();
        assert!(
            !project_dir.join("chief.yaml").exists(),
            "fixture should intentionally omit chief.yaml"
        );

        let err = service
            .start_project(
                project.clone(),
                StartProjectRequest {
                    agents: Some(1),
                    flow: None,
                    model: None,
                },
            )
            .await
            .expect_err("start_project should reject when chief.yaml is missing");

        let response = err.into_response();
        assert_eq!(
            response.status(),
            StatusCode::CONFLICT,
            "missing chief.yaml should return HTTP 409"
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error response body should be readable");
        let payload: serde_json::Value =
            serde_json::from_slice(&body).expect("error response should be JSON");
        assert_eq!(
            payload.get("code").and_then(serde_json::Value::as_str),
            Some("chief_yaml_missing"),
            "error code should identify missing chief.yaml"
        );
        let details = payload
            .get("details")
            .expect("error payload should include details");
        assert_eq!(
            details
                .get("config_path")
                .and_then(serde_json::Value::as_str),
            Some(expected_config_path.as_str()),
            "error details should include the missing config path"
        );
        let hint = details
            .get("hint")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(
            hint.contains("chief init"),
            "error details should include a remediation hint: {hint}"
        );

        let store = ProjectStore::new(&project_dir);
        let jobs = store.list_jobs(50).expect("jobs should be readable");
        assert!(
            jobs.is_empty(),
            "rejected start_project should not create job records"
        );
        let conn = Connection::open(&store.db_path).expect("chief.db should be readable");
        let run_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
            .expect("runs table should be queryable");
        assert_eq!(
            run_count, 0,
            "rejected start_project should not create run records"
        );
    }

    #[tokio::test]
    async fn start_project_uses_latest_flow_from_chief_yaml_on_disk() {
        let workspace = TempDir::new("workspace");
        let project_name = format!("project-{}", Uuid::new_v4());
        let project_dir = workspace.path.join(&project_name);
        fs::create_dir_all(&project_dir).expect("failed to create project directory");

        init_git_repo(&project_dir);
        let store = ProjectStore::new(&project_dir);
        store.init().expect("store init should succeed");
        write_todos(
            &project_dir,
            r#"todos:
  - id: done-1
    todo: Completed todo
    expectations: Already done
    priority: 1
    test_suites: []
    status: done"#,
        );
        write_chief_yaml(
            &project_dir,
            r#"chief:
  flow: tdd"#,
        );

        run_git(&project_dir, &["add", "--all"]);
        run_git(&project_dir, &["commit", "-m", "chore: baseline"]);

        store
            .reset_db_from_todos_file()
            .expect("reset_db_from_todos_file should seed sqlite from todos.yaml");

        let registry = ProjectRegistry::discover(&workspace.path, &[])
            .expect("project discovery should succeed");
        let scheduler = Scheduler::new(registry, 4);
        let service = ApiService::new(scheduler, 1);

        write_chief_yaml(
            &project_dir,
            r#"chief:
  flow: single_prompt"#,
        );

        let response = service
            .start_project(
                project_name.clone(),
                StartProjectRequest {
                    agents: Some(1),
                    flow: None,
                    model: None,
                },
            )
            .await
            .expect("start_project should succeed");

        assert!(
            response.message.contains("flow=single_prompt"),
            "start_project should use refreshed chief.yaml flow, got message: {}",
            response.message
        );
    }

    #[tokio::test]
    async fn get_todos_refreshes_from_todos_yaml_without_db_reset() {
        let (_workspace, service, project, project_dir) = setup_service(
            r#"todos:
  - id: todo-in-db
    todo: Existing todo
    expectations: Existing expectations
    priority: 1
    test_suites: []
    status: pending"#,
        );

        write_todos(
            &project_dir,
            r#"todos:
  - id: todo-in-db
    todo: Existing todo
    expectations: Existing expectations
    priority: 1
    test_suites: []
    status: pending
  - id: manual-new
    todo: Manually added todo
    expectations: Appears without reset_db
    priority: 8
    test_suites: []
    status: pending"#,
        );

        let response = service
            .get_todos(&project)
            .await
            .expect("get_todos should succeed after manual todos.yaml edit");

        assert!(
            response.todos.iter().any(|todo| todo.id == "manual-new"),
            "new todo from todos.yaml should be visible via get_todos"
        );
    }

    #[tokio::test]
    async fn get_todos_removes_items_deleted_from_todos_yaml() {
        let (_workspace, service, project, project_dir) = setup_service(
            r#"todos:
  - id: todo-keep
    todo: Keep this todo
    expectations: Keep this expectations
    priority: 5
    test_suites: []
    status: pending
  - id: todo-remove
    todo: Remove this todo
    expectations: Remove this expectations
    priority: 2
    test_suites: []
    status: pending"#,
        );

        write_todos(
            &project_dir,
            r#"todos:
  - id: todo-keep
    todo: Keep this todo
    expectations: Keep this expectations
    priority: 5
    test_suites: []
    status: pending"#,
        );

        let response = service
            .get_todos(&project)
            .await
            .expect("get_todos should sync file removals");
        assert_eq!(
            response.todos.len(),
            1,
            "response should exactly match todos.yaml after manual removal"
        );
        assert_eq!(
            response.todos[0].id, "todo-keep",
            "remaining todo should still be visible"
        );
        assert!(
            response.todos.iter().all(|todo| todo.id != "todo-remove"),
            "removed todo should not be returned by get_todos"
        );

        let store = ProjectStore::new(&project_dir);
        let sqlite_todos = store
            .list_todos()
            .expect("sqlite todos should be readable after sync");
        assert!(
            sqlite_todos.iter().all(|todo| todo.id != "todo-remove"),
            "removed todo should also be deleted from sqlite after refresh sync"
        );
    }

    #[tokio::test]
    async fn get_todos_and_get_state_refresh_between_calls_after_manual_todo_edits() {
        let (_workspace, service, project, project_dir) = setup_service(
            r#"todos:
  - id: todo-alpha
    todo: Original todo text
    expectations: Original expectations
    priority: 1
    test_suites: []
    status: pending
  - id: todo-beta
    todo: Already done
    expectations: Keep done
    priority: 2
    test_suites: []
    status: done
  - id: todo-remove
    todo: To be removed by manual edit
    expectations: Should disappear on next read
    priority: 3
    test_suites: []
    status: pending"#,
        );

        let initial_todos = service
            .get_todos(&project)
            .await
            .expect("first get_todos should succeed before manual edit");
        assert!(
            initial_todos
                .todos
                .iter()
                .any(|todo| todo.id == "todo-remove"),
            "first get_todos should reflect baseline todo set"
        );

        let initial_state = service
            .get_state(&project)
            .await
            .expect("first get_state should succeed before manual edit");
        assert_eq!(initial_state.todos.total, 3, "baseline total should match");
        assert_eq!(
            initial_state.todos.completed, 1,
            "baseline completed count should match"
        );
        assert_eq!(
            initial_state.todos.available, 2,
            "baseline available count should match"
        );

        write_todos(
            &project_dir,
            r#"todos:
  - id: todo-alpha
    todo: Edited todo text from file
    expectations: Edited expectations from file
    priority: 9
    test_suites: []
    status: done
    done_at_commit: manual-edit-commit
  - id: todo-gamma
    todo: Newly added todo from file
    expectations: Added between API reads
    priority: 6
    test_suites: []
    status: pending
  - id: todo-beta
    todo: Already done
    expectations: Keep done
    priority: 2
    test_suites: []
    status: done"#,
        );

        let todos = service
            .get_todos(&project)
            .await
            .expect("get_todos should reflect manual todo edits");
        let edited = todos
            .todos
            .iter()
            .find(|todo| todo.id == "todo-alpha")
            .expect("edited todo should exist");
        assert_eq!(edited.todo, "Edited todo text from file");
        assert_eq!(edited.expectations, "Edited expectations from file");
        assert_eq!(edited.priority, 9);
        assert_eq!(edited.done_at_commit.as_deref(), Some("manual-edit-commit"));
        assert!(
            todos.todos.iter().any(|todo| todo.id == "todo-gamma"),
            "second get_todos should include manually added todos"
        );
        assert!(
            todos.todos.iter().all(|todo| todo.id != "todo-remove"),
            "second get_todos should remove todos deleted from todos.yaml"
        );

        let state = service
            .get_state(&project)
            .await
            .expect("get_state should refresh todos before progress calculation");
        assert_eq!(
            state.todos.total, 3,
            "total todos should reflect file edits"
        );
        assert_eq!(
            state.todos.completed, 2,
            "completed count should reflect edited todo status"
        );
        assert_eq!(
            state.todos.available, 1,
            "available count should reflect add/update/remove reconciliation"
        );
    }

    #[tokio::test]
    async fn read_endpoints_return_api_errors_for_invalid_todos_yaml() {
        let (_workspace, service, project, project_dir) = setup_service(
            r#"todos:
  - id: valid-todo
    todo: Valid baseline
    expectations: Baseline expectations
    priority: 3
    test_suites: []
    status: pending"#,
        );

        let initial_todos = service
            .get_todos(&project)
            .await
            .expect("first get_todos should succeed for valid yaml");
        assert_eq!(
            initial_todos.todos.len(),
            1,
            "baseline get_todos should return the seeded todo"
        );

        let initial_state = service
            .get_state(&project)
            .await
            .expect("first get_state should succeed for valid yaml");
        assert_eq!(initial_state.todos.total, 1, "baseline total should match");
        assert_eq!(
            initial_state.todos.available, 1,
            "baseline available should match"
        );
        assert_eq!(
            initial_state.todos.completed, 0,
            "baseline completed should match"
        );

        fs::write(
            project_dir.join("todos.yaml"),
            "todos:\n  - id: broken\n    todo: [missing quote\n",
        )
        .expect("failed to write invalid todos.yaml");

        let todos_error = service
            .get_todos(&project)
            .await
            .expect_err("get_todos should fail for invalid todos.yaml");
        assert_invalid_yaml_api_error(todos_error).await;

        let state_error = service
            .get_state(&project)
            .await
            .expect_err("get_state should fail for invalid todos.yaml");
        assert_invalid_yaml_api_error(state_error).await;

        write_todos(
            &project_dir,
            r#"todos:
  - id: recovered-todo
    todo: Recovered after fixing yaml
    expectations: Reads should recover after parse error
    priority: 7
    test_suites: []
    status: pending"#,
        );

        let recovered_todos = service
            .get_todos(&project)
            .await
            .expect("get_todos should recover after restoring valid yaml");
        assert_eq!(
            recovered_todos
                .todos
                .iter()
                .map(|todo| todo.id.as_str())
                .collect::<Vec<_>>(),
            vec!["recovered-todo"],
            "post-recovery read should reflect latest synchronized file content"
        );
    }
}
