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
    #[arg(long, default_value = ".")]
    pub parent_dir: PathBuf,
    #[arg(long, default_value = "0.0.0.0")]
    pub host: String,
    #[arg(long, default_value_t = 8000)]
    pub port: u16,
    #[arg(long, default_value = "frontend")]
    pub frontend_dir: PathBuf,
    #[arg(long = "allow-origin")]
    pub allow_origins: Vec<String>,
    #[arg(long, default_value_t = false)]
    pub enable_terminal: bool,
    #[arg(long, env = "CHIEF_API_TOKEN")]
    pub api_token: Option<String>,
}

#[derive(Clone)]
pub struct AppState {
    pub service: ApiService,
    pub terminal_enabled: bool,
    pub api_token: Option<String>,
}

pub async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = BackendCli::parse();

    let registry = ProjectRegistry::discover(&cli.parent_dir).with_context(|| {
        format!(
            "failed discovering projects from {}",
            cli.parent_dir.display()
        )
    })?;

    let scheduler = Scheduler::new(registry);
    let state = Arc::new(AppState {
        service: ApiService::new(scheduler),
        terminal_enabled: cli.enable_terminal,
        api_token: cli.api_token,
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
