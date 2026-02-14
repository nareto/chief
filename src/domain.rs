use chrono::{DateTime, Utc};
use md5::Digest;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Start,
    TodoSelection,
    Red,
    Green,
    SinglePrompt,
    PostGreen,
    Exit,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::TodoSelection => "todo_selection",
            Self::Red => "red",
            Self::Green => "green",
            Self::SinglePrompt => "single_prompt",
            Self::PostGreen => "post_green",
            Self::Exit => "exit",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[serde(alias = "attempted")]
    Pending,
    InProgress,
    Done,
}

impl TodoStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunExitStatus {
    Success,
    Failure,
    UnrecoverableFailure,
}

impl RunExitStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::UnrecoverableFailure => "unrecoverable_failure",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Msg,
    TestRun,
    PostGreenOutput,
    Lint,
    LintFix,
    PhaseChange,
    GitOp,
    Diff,
    AgentCmd,
    AgentPrompt,
    AgentResponse,
    PhaseFailure,
    Error,
    Job,
}

impl EventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Msg => "msg",
            Self::TestRun => "test_run",
            Self::PostGreenOutput => "post_green_output",
            Self::Lint => "lint",
            Self::LintFix => "lint_fix",
            Self::PhaseChange => "phase_change",
            Self::GitOp => "git_op",
            Self::Diff => "diff",
            Self::AgentCmd => "agent_cmd",
            Self::AgentPrompt => "agent_prompt",
            Self::AgentResponse => "agent_response",
            Self::PhaseFailure => "phase_failure",
            Self::Error => "error",
            Self::Job => "job",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetType {
    File,
    Package,
    Project,
    Repo,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Selecting,
    Running,
    Merging,
    Completed,
    Failed,
    Cancelled,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Selecting => "selecting",
            Self::Running => "running",
            Self::Merging => "merging",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Todo {
    #[serde(default)]
    pub id: String,
    pub todo: String,
    #[serde(default)]
    pub expectations: String,
    #[serde(default)]
    pub priority: i64,
    #[serde(default, deserialize_with = "deserialize_test_suites")]
    pub test_suites: Vec<String>,
    #[serde(default = "default_todo_status")]
    pub status: TodoStatus,
    #[serde(default)]
    pub done_at_commit: Option<String>,
}

fn default_todo_status() -> TodoStatus {
    TodoStatus::Pending
}

fn deserialize_test_suites<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Vec<String>>::deserialize(deserializer)?;
    Ok(value.unwrap_or_default())
}

impl Todo {
    pub fn normalize(mut self) -> Self {
        self.todo = self.todo.trim().to_owned();
        self.expectations = self.expectations.trim().to_owned();
        if self.id.trim().is_empty() {
            self.id = Self::compute_id(&self.todo, &self.expectations);
        }
        self
    }

    pub fn compute_id(todo: &str, expectations: &str) -> String {
        let normalized = format!("task:{}\nexpectations:{}", todo.trim(), expectations.trim());
        let digest: Digest = md5::compute(normalized.as_bytes());
        format!("{digest:x}")
    }

    pub fn to_prompt_block(&self) -> String {
        format!("task: {}\nexpectations: {}", self.todo, self.expectations)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TodoFile {
    #[serde(default)]
    pub todos: Vec<Todo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub status: String,
    pub exit_status: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: Option<i64>,
    pub run_id: String,
    pub job_id: Option<String>,
    pub todo_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub level: String,
    pub phase: Option<Phase>,
    pub msg: String,
    pub event_type: EventType,
    #[serde(default)]
    pub payload: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub run_id: String,
    pub todo_id: Option<String>,
    pub status: JobStatus,
    pub worker_index: usize,
    pub flow: String,
    pub worktree_path: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopDecision {
    Retry,
    Stable,
    Success,
}

#[derive(Debug, Clone)]
pub struct AgentOutput {
    pub exit_code: i32,
    pub command: String,
    pub stdout: String,
    pub stderr: String,
    pub merged_output: String,
}

impl AgentOutput {
    pub fn success(command: impl Into<String>, merged_output: impl Into<String>) -> Self {
        let merged_output = merged_output.into();
        Self {
            exit_code: 0,
            command: command.into(),
            stdout: merged_output.clone(),
            stderr: String::new(),
            merged_output,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitState {
    Completed,
    TimedOut,
    Cancelled,
}

pub(crate) fn payload_from_json(value: Value) -> BTreeMap<String, Value> {
    match value {
        Value::Object(map) => map.into_iter().collect(),
        _ => BTreeMap::new(),
    }
}
