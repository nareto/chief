use crate::api::router::build_router;
use crate::api::service::ApiService;
use anyhow::{Context, Result, anyhow};
use axum::http::{HeaderValue, Method};
use chief::scheduler::Scheduler;
use chief::service::ProjectRegistry;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "chief_backend")]
#[command(about = "Chief multi-project backend server")]
pub struct BackendCli {
    #[arg(long = "projects-dir", alias = "parent-dir", default_value = ".")]
    pub projects_dir: PathBuf,
    #[arg(long = "project")]
    pub project: Vec<PathBuf>,
    #[arg(long, default_value = "0.0.0.0")]
    pub host: String,
    #[arg(long, default_value_t = 8000)]
    pub port: u16,
    #[arg(long, default_value = "frontend")]
    pub frontend_dir: PathBuf,
    #[arg(long = "allow-origin")]
    pub allow_origins: Vec<String>,
    #[arg(long, default_value_t = 1)]
    pub default_agents_per_project: usize,
    #[arg(long, default_value_t = 8)]
    pub max_agents_per_project: usize,
    #[arg(long, default_value_t = false)]
    pub enable_terminal: bool,
    #[arg(long, env = "CHIEF_API_TOKEN")]
    pub api_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BackendRuntimeSettings {
    pub host: String,
    pub port: u16,
    pub projects_dir: String,
    pub projects: Vec<String>,
    pub frontend_dir: String,
    pub allow_origins: Vec<String>,
    pub enable_terminal: bool,
    pub default_agents_per_project: usize,
    pub max_agents_per_project: usize,
}

#[derive(Clone)]
pub struct AppState {
    pub service: ApiService,
    pub terminal_enabled: bool,
    pub api_token: Option<String>,
    pub backend_settings: BackendRuntimeSettings,
}

pub async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = BackendCli::parse();

    let registry =
        ProjectRegistry::discover(&cli.projects_dir, &cli.project).with_context(|| {
            format!(
                "failed discovering projects from {}",
                cli.projects_dir.display()
            )
        })?;

    let default_agents_per_project = cli.default_agents_per_project.max(1);
    let max_agents_per_project = cli.max_agents_per_project.max(1);
    let effective_allow_origins = if cli.allow_origins.is_empty() {
        vec!["http://localhost:3000".to_owned()]
    } else {
        cli.allow_origins.clone()
    };

    let scheduler = Scheduler::new(registry, max_agents_per_project);
    let state = Arc::new(AppState {
        service: ApiService::new(scheduler, default_agents_per_project),
        terminal_enabled: cli.enable_terminal,
        api_token: cli.api_token.clone(),
        backend_settings: BackendRuntimeSettings {
            host: cli.host.clone(),
            port: cli.port,
            projects_dir: cli.projects_dir.display().to_string(),
            projects: cli
                .project
                .iter()
                .map(|project| project.display().to_string())
                .collect(),
            frontend_dir: cli.frontend_dir.display().to_string(),
            allow_origins: effective_allow_origins,
            enable_terminal: cli.enable_terminal,
            default_agents_per_project,
            max_agents_per_project,
        },
    });

    let app = build_router(state).layer(build_cors_layer(&cli.allow_origins)?);
    let app = if cli.frontend_dir.exists() {
        app.fallback_service(ServeDir::new(&cli.frontend_dir))
    } else {
        app
    };

    let bind = format!("{}:{}", cli.host, cli.port);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("failed binding {bind}"))?;

    tracing::info!("backend listening on http://{}", bind);
    axum::serve(listener, app).await?;
    Ok(())
}

fn build_cors_layer(allow_origins: &[String]) -> Result<CorsLayer> {
    let effective = if allow_origins.is_empty() {
        vec!["http://localhost:3000".to_owned()]
    } else {
        allow_origins.to_vec()
    };

    let base = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::OPTIONS])
        .allow_headers(Any);

    if effective.iter().any(|origin| origin == "*") {
        return Ok(base.allow_origin(Any));
    }

    let parsed = effective
        .iter()
        .map(|origin| {
            origin
                .parse::<HeaderValue>()
                .map_err(|err| anyhow!("invalid CORS origin '{}': {}", origin, err))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(base.allow_origin(AllowOrigin::list(parsed)))
}
