use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chief::domain::{EventType, Phase, Todo};
use chief::scheduler::Scheduler;
use chief::service::ChiefEngine;
use chief::storage::EventQuery;
use clap::Parser;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "chief-backend")]
#[command(about = "Chief multi-project backend server")]
struct BackendCli {
    #[arg(long, default_value = ".")]
    parent_dir: PathBuf,
    #[arg(long, default_value = "0.0.0.0")]
    host: String,
    #[arg(long, default_value_t = 8000)]
    port: u16,
    #[arg(long, default_value = "frontend")]
    frontend_dir: PathBuf,
}

#[derive(Clone)]
struct AppState {
    scheduler: Scheduler,
}

#[derive(Debug, Deserialize)]
struct StartProjectRequest {
    agents: Option<usize>,
    flow: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RequirementsRequest {
    text: String,
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AddTodoRequest {
    todo: String,
    expectations: Option<String>,
    priority: Option<i64>,
    test_suites: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct LogQuery {
    limit: Option<usize>,
    event_type: Option<String>,
    phase: Option<String>,
    level: Option<String>,
    q: Option<String>,
}

#[derive(Debug, Serialize)]
struct ApiMessage {
    message: String,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("backend error: {err:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = BackendCli::parse();

    let registry =
        chief::service::ProjectRegistry::discover(&cli.parent_dir).with_context(|| {
            format!(
                "failed discovering projects from {}",
                cli.parent_dir.display()
            )
        })?;

    let scheduler = Scheduler::new(registry);
    let state = Arc::new(AppState { scheduler });

    let mut app = Router::new()
        .route("/api/projects", get(list_projects))
        .route("/api/projects/refresh", post(refresh_projects))
        .route("/api/projects/:project/start", post(start_project))
        .route("/api/projects/:project/stop", post(stop_project))
        .route(
            "/api/projects/:project/todos",
            get(get_todos).post(add_todo),
        )
        .route("/api/projects/:project/jobs", get(get_jobs))
        .route("/api/projects/:project/logs", get(get_logs))
        .route(
            "/api/projects/:project/requirements",
            post(process_requirements),
        )
        .route("/api/projects/:project/terminal/ws", get(terminal_ws))
        .with_state(state)
        .layer(CorsLayer::permissive());

    if cli.frontend_dir.exists() {
        app = app.nest_service("/", ServeDir::new(&cli.frontend_dir));
    }

    let bind = format!("{}:{}", cli.host, cli.port);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("failed binding {bind}"))?;

    tracing::info!("backend listening on http://{}", bind);
    axum::serve(listener, app).await?;

    Ok(())
}

async fn list_projects(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let views = state.scheduler.list_project_views().await;
    Ok(Json(json!({ "projects": views })))
}

async fn refresh_projects(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiMessage>, ApiError> {
    state.scheduler.refresh_registry().await?;
    Ok(Json(ApiMessage {
        message: "registry refreshed".to_owned(),
    }))
}

async fn start_project(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<StartProjectRequest>,
) -> Result<Json<ApiMessage>, ApiError> {
    let context = state.scheduler.get_project_context(&project).await?;
    let agents = payload
        .agents
        .unwrap_or(context.chief_toml.backend.default_agents_per_project)
        .max(1);
    let flow = payload.flow.unwrap_or_else(|| "tdd".to_owned());

    state
        .scheduler
        .start_project(project.clone(), agents, flow.clone(), payload.model)
        .await?;

    Ok(Json(ApiMessage {
        message: format!("started project {project} with {agents} agent(s), flow={flow}"),
    }))
}

async fn stop_project(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiMessage>, ApiError> {
    state.scheduler.stop_project(&project).await?;
    Ok(Json(ApiMessage {
        message: format!("stop requested for project {project}"),
    }))
}

async fn get_todos(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let context = state.scheduler.get_project_context(&project).await?;
    let todos = context.store.list_todos()?;
    Ok(Json(json!({ "todos": todos })))
}

async fn add_todo(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<AddTodoRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let context = state.scheduler.get_project_context(&project).await?;
    let mut todo_file = context.store.load_todo_file()?;

    let todo = Todo {
        id: String::new(),
        todo: payload.todo,
        expectations: payload.expectations.unwrap_or_default(),
        priority: payload.priority.unwrap_or(0),
        test_suites: payload.test_suites.unwrap_or_default(),
        status: chief::domain::TodoStatus::Pending,
        done_at_commit: None,
    }
    .normalize();

    todo_file.todos.push(todo.clone());
    context.store.save_todo_file(&todo_file)?;
    context.store.sync_todos_from_file()?;

    Ok(Json(json!({ "todo": todo })))
}

async fn get_jobs(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let context = state.scheduler.get_project_context(&project).await?;
    let jobs = context.store.list_jobs(200)?;
    Ok(Json(json!({ "jobs": jobs })))
}

async fn get_logs(
    Path(project): Path<String>,
    Query(query): Query<LogQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let context = state.scheduler.get_project_context(&project).await?;
    let events = context.store.query_events(EventQuery {
        limit: query.limit.unwrap_or(200),
        event_type: query.event_type.as_deref().map(parse_event_type),
        phase: query.phase.as_deref().map(parse_phase),
        level: query.level,
        contains_text: query.q,
    })?;

    Ok(Json(json!({ "events": events })))
}

async fn process_requirements(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<RequirementsRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let context = state.scheduler.get_project_context(&project).await?;
    let engine = ChiefEngine::new(context.clone());

    let diff = tokio::task::spawn_blocking(move || {
        engine.process_requirements(&payload.text, &context.store.todos_path, payload.model)
    })
    .await
    .map_err(|err| ApiError::new(anyhow::anyhow!(err.to_string())))??;

    Ok(Json(json!({ "diff": diff })))
}

async fn terminal_ws(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let context = state.scheduler.get_project_context(&project).await?;
    Ok(ws.on_upgrade(move |socket| handle_terminal(socket, context.project_dir)))
}

async fn handle_terminal(mut socket: WebSocket, project_dir: PathBuf) {
    let _ = socket
        .send(Message::Text(
            "Connected terminal. Send one shell command per message.".into(),
        ))
        .await;

    while let Some(Ok(message)) = socket.next().await {
        match message {
            Message::Text(command) => {
                let command = command.trim();
                if command.is_empty() {
                    continue;
                }

                let result = run_terminal_command(&project_dir, command);
                let payload = match result {
                    Ok(output) => output,
                    Err(err) => format!("error: {err}"),
                };

                if socket.send(Message::Text(payload.into())).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}

fn run_terminal_command(project_dir: &PathBuf, command: &str) -> Result<String> {
    let output = Command::new("sh")
        .arg("-lc")
        .arg(command)
        .current_dir(project_dir)
        .output()
        .with_context(|| format!("failed to run command: {command}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(format!(
        "$ {command}\nexit_code={}\n{}{}",
        output.status.code().unwrap_or(1),
        stdout,
        stderr
    ))
}

fn parse_event_type(value: &str) -> EventType {
    match value {
        "test_run" => EventType::TestRun,
        "post_green_output" => EventType::PostGreenOutput,
        "lint" => EventType::Lint,
        "phase_change" => EventType::PhaseChange,
        "git_op" => EventType::GitOp,
        "diff" => EventType::Diff,
        "agent_cmd" => EventType::AgentCmd,
        "agent_prompt" => EventType::AgentPrompt,
        "agent_response" => EventType::AgentResponse,
        "phase_failure" => EventType::PhaseFailure,
        "error" => EventType::Error,
        "job" => EventType::Job,
        _ => EventType::Msg,
    }
}

fn parse_phase(value: &str) -> Phase {
    match value {
        "todo_selection" => Phase::TodoSelection,
        "red" => Phase::Red,
        "green" => Phase::Green,
        "post_green" => Phase::PostGreen,
        "exit" => Phase::Exit,
        _ => Phase::Start,
    }
}

#[derive(Debug)]
struct ApiError(anyhow::Error);

impl ApiError {
    fn new(error: anyhow::Error) -> Self {
        Self(error)
    }
}

impl<E> From<E> for ApiError
where
    E: Into<anyhow::Error>,
{
    fn from(value: E) -> Self {
        Self(value.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let message = self.0.to_string();
        let body = Json(json!({ "error": message }));
        (StatusCode::BAD_REQUEST, body).into_response()
    }
}
