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

#[path = "service/project_admin.rs"]
mod project_admin;
#[path = "service/project_data.rs"]
mod project_data;
#[path = "service/readiness/mod.rs"]
mod readiness;
#[path = "service/suite_checks.rs"]
mod suite_checks;
#[path = "service/todos.rs"]
mod todos;

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
        if matches!(flow_kind, FlowKind::LoopFile) {
            return Err(ApiError::unprocessable(
                "flow 'loop_file' is CLI-only; use `chief loop_file --file <path>`",
            ));
        }

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
