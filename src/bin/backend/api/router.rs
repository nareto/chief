use crate::api::handlers;
use crate::app::AppState;
use axum::Router;
use axum::routing::{get, post, put};
use std::sync::Arc;

pub fn build_router(state: Arc<AppState>) -> Router {
    let mut app = Router::new()
        .route("/api/backend/settings", get(handlers::get_backend_settings))
        .route("/api/projects", get(handlers::list_projects))
        .route("/api/projects/refresh", post(handlers::refresh_projects))
        .route(
            "/api/projects/{project}/start",
            post(handlers::start_project),
        )
        .route("/api/projects/{project}/stop", post(handlers::stop_project))
        .route(
            "/api/projects/{project}/todos",
            get(handlers::get_todos).post(handlers::add_todo),
        )
        .route(
            "/api/projects/{project}/todos/{todo_id}",
            put(handlers::update_todo),
        )
        .route("/api/projects/{project}/jobs", get(handlers::get_jobs))
        .route("/api/projects/{project}/logs", get(handlers::get_logs))
        .route(
            "/api/projects/{project}/requirements",
            post(handlers::process_requirements),
        )
        .route("/api/projects/{project}/state", get(handlers::get_state))
        .route("/api/projects/{project}/events", get(handlers::get_events))
        .route(
            "/api/projects/{project}/events/ws",
            get(handlers::events_ws),
        )
        .route(
            "/api/projects/{project}/file_diff",
            get(handlers::get_file_diff),
        )
        .route(
            "/api/projects/{project}/chief_toml",
            get(handlers::get_chief_toml).put(handlers::update_chief_toml),
        )
        .route(
            "/api/projects/{project}/suite_checks",
            post(handlers::run_suite_check),
        )
        .route(
            "/api/projects/{project}/reset_db",
            post(handlers::reset_project_db),
        );

    if state.terminal_enabled {
        app = app
            .route(
                "/api/projects/{project}/terminal",
                get(handlers::terminal_ws),
            )
            .route(
                "/api/projects/{project}/terminal/ws",
                get(handlers::terminal_ws),
            );
    }

    app.with_state(state)
}
