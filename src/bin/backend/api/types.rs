use chief::domain::{EventRecord, JobRecord, Todo};
use chief::flow::SuiteCommandKind;
use chief::scheduler::{ProjectRuntimeView, StopMode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct StartProjectRequest {
    pub agents: Option<usize>,
    pub flow: Option<String>,
    pub model: Option<String>,
    pub start_anyway: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct RequirementsRequest {
    pub text: String,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddTodoRequest {
    pub todo: String,
    pub expectations: Option<String>,
    pub priority: Option<i64>,
    pub test_suites: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateTodoRequest {
    pub id: Option<String>,
    pub todo: Option<String>,
    pub expectations: Option<String>,
    pub priority: Option<i64>,
    pub test_suites: Option<Vec<String>>,
    pub status: Option<String>,
    pub done_at_commit: Option<Option<String>>,
}

#[derive(Debug, Deserialize)]
pub struct LogQuery {
    pub limit: Option<usize>,
    pub event_type: Option<String>,
    pub phase: Option<String>,
    pub level: Option<String>,
    pub q: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    pub limit: Option<usize>,
    pub types: Option<String>,
    pub q: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FileDiffQuery {
    pub file: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateChiefYamlRequest {
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct RunSuiteCheckRequest {
    pub suite: String,
    pub kind: SuiteCommandKind,
    pub target: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TrimProjectDbRequest {
    pub keep_runs: usize,
}

#[derive(Debug, Serialize)]
pub struct MessageResponse {
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct ProjectsResponse {
    pub projects: Vec<ProjectRuntimeView>,
}

#[derive(Debug, Serialize)]
pub struct BackendSettingsResponse {
    pub host: String,
    pub port: u16,
    pub projects_dir: String,
    pub projects: Vec<String>,
    pub allow_origins: Vec<String>,
    pub enable_terminal: bool,
    pub default_agents_per_project: usize,
    pub max_agents_per_project: usize,
}

#[derive(Debug, Serialize)]
pub struct TodoResponse {
    pub todo: Todo,
}

#[derive(Debug, Serialize)]
pub struct TodosResponse {
    pub todos: Vec<Todo>,
}

#[derive(Debug, Serialize)]
pub struct JobsResponse {
    pub jobs: Vec<JobRecord>,
}

#[derive(Debug, Serialize)]
pub struct EventsResponse {
    pub events: Vec<EventRecord>,
}

#[derive(Debug, Serialize)]
pub struct RequirementsResponse {
    pub diff: String,
}

#[derive(Debug, Serialize)]
pub struct FileDiffResponse {
    pub file: String,
    pub diff: String,
}

#[derive(Debug, Serialize)]
pub struct ChiefYamlResponse {
    pub content: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct RunSuiteCheckResponse {
    pub suite: String,
    pub kind: SuiteCommandKind,
    pub command: String,
    pub cwd: String,
    pub exit_code: i32,
    pub output: String,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum SuiteCheckOutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunSuiteCheckStreamEvent {
    Started {
        suite: String,
        kind: SuiteCommandKind,
        command: String,
        cwd: String,
    },
    Chunk {
        stream: SuiteCheckOutputStream,
        text: String,
    },
    Completed {
        result: RunSuiteCheckResponse,
    },
    Error {
        error: String,
    },
}

#[derive(Debug, Serialize)]
pub struct StateResponse {
    pub project: String,
    pub running: bool,
    pub stop_requested: bool,
    pub stop_mode: StopMode,
    pub active_agents: usize,
    pub desired_agents: usize,
    pub flow_name: String,
    pub last_error: Option<String>,
    pub phase: String,
    pub phase_iteration: Option<PhaseIteration>,
    pub last_activity: Option<String>,
    pub last_done_todo_committed_at: Option<String>,
    pub chief_db_size_bytes: Option<u64>,
    pub dirty_files: Vec<String>,
    pub todos: TodoProgress,
    pub active_job: Option<ActiveJobResponse>,
    pub readiness: ProjectReadinessResponse,
}

#[derive(Debug, Serialize)]
pub struct ProjectReadinessResponse {
    pub status: String,
    pub summary: String,
    pub checking_started_at: Option<String>,
    pub checked_at: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Serialize)]
pub struct PhaseIteration {
    pub current: usize,
    pub max: usize,
}

#[derive(Debug, Serialize)]
pub struct TodoProgress {
    pub available: usize,
    pub completed: usize,
    pub total: usize,
}

#[derive(Debug, Serialize)]
pub struct ActiveJobResponse {
    pub job_id: String,
    pub todo_id: Option<String>,
    pub worker_index: usize,
    pub status: String,
}
