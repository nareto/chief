use crate::api::auth;
use crate::api::error::ApiError;
use crate::api::events_ws;
use crate::api::terminal_ws;
use crate::api::types::{
    AddTodoRequest, EventsQuery, FileDiffQuery, LogQuery, RequirementsRequest, StartProjectRequest,
    UpdateChiefTomlRequest, UpdateTodoRequest,
};
use crate::app::AppState;
use axum::Json;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use std::sync::Arc;

pub async fn list_projects(
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::api::types::ProjectsResponse>, ApiError> {
    Ok(Json(state.service.list_projects().await))
}

pub async fn refresh_projects(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<crate::api::types::MessageResponse>, ApiError> {
    auth::require_sensitive_access(&state, &headers)?;
    Ok(Json(state.service.refresh_projects().await?))
}

pub async fn start_project(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<StartProjectRequest>,
) -> Result<Json<crate::api::types::MessageResponse>, ApiError> {
    auth::require_sensitive_access(&state, &headers)?;
    Ok(Json(state.service.start_project(project, payload).await?))
}

pub async fn stop_project(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<crate::api::types::MessageResponse>, ApiError> {
    auth::require_sensitive_access(&state, &headers)?;
    Ok(Json(state.service.stop_project(&project).await?))
}

pub async fn get_todos(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::api::types::TodosResponse>, ApiError> {
    Ok(Json(state.service.get_todos(&project).await?))
}

pub async fn add_todo(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<AddTodoRequest>,
) -> Result<Json<crate::api::types::TodoResponse>, ApiError> {
    auth::require_sensitive_access(&state, &headers)?;
    Ok(Json(state.service.add_todo(&project, payload).await?))
}

pub async fn update_todo(
    Path((project, todo_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<UpdateTodoRequest>,
) -> Result<Json<crate::api::types::TodoResponse>, ApiError> {
    auth::require_sensitive_access(&state, &headers)?;
    Ok(Json(
        state
            .service
            .update_todo(&project, &todo_id, payload)
            .await?,
    ))
}

pub async fn get_jobs(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::api::types::JobsResponse>, ApiError> {
    Ok(Json(state.service.get_jobs(&project).await?))
}

pub async fn get_logs(
    Path(project): Path<String>,
    Query(query): Query<LogQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::api::types::EventsResponse>, ApiError> {
    Ok(Json(state.service.get_logs(&project, query).await?))
}

pub async fn process_requirements(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<RequirementsRequest>,
) -> Result<Json<crate::api::types::RequirementsResponse>, ApiError> {
    auth::require_sensitive_access(&state, &headers)?;
    Ok(Json(
        state
            .service
            .process_requirements(&project, payload)
            .await?,
    ))
}

pub async fn get_state(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::api::types::StateResponse>, ApiError> {
    Ok(Json(state.service.get_state(&project).await?))
}

pub async fn get_events(
    Path(project): Path<String>,
    Query(query): Query<EventsQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::api::types::EventsResponse>, ApiError> {
    Ok(Json(state.service.get_events(&project, query).await?))
}

pub async fn events_ws(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    events_ws::events_ws(project, state, ws).await
}

pub async fn get_file_diff(
    Path(project): Path<String>,
    Query(query): Query<FileDiffQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::api::types::FileDiffResponse>, ApiError> {
    Ok(Json(state.service.get_file_diff(&project, query).await?))
}

pub async fn get_chief_toml(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::api::types::ChiefTomlResponse>, ApiError> {
    Ok(Json(state.service.get_chief_toml(&project).await?))
}

pub async fn update_chief_toml(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<UpdateChiefTomlRequest>,
) -> Result<Json<crate::api::types::MessageResponse>, ApiError> {
    auth::require_sensitive_access(&state, &headers)?;
    Ok(Json(
        state.service.update_chief_toml(&project, payload).await?,
    ))
}

pub async fn reset_project_db(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<crate::api::types::MessageResponse>, ApiError> {
    auth::require_sensitive_access(&state, &headers)?;
    Ok(Json(state.service.reset_project_db(&project).await?))
}

pub async fn terminal_ws(
    Path(project): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    auth::require_sensitive_access(&state, &headers)?;
    terminal_ws::terminal_ws(project, state, ws).await
}
