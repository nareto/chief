use crate::agent::{AgentCancelledError, AgentRequest, CodingAgent};
use crate::agent_stream;
use crate::config::{ChiefConfig, ChiefYaml, TestSuiteConfig};
use crate::domain::{
    AgentOutput, EventRecord, EventType, LoopDecision, Phase, Todo, payload_from_json,
};
use crate::git::GitOps;
use crate::prompt::PromptStore;
use crate::storage::{EventQuery, ProjectStore};
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FlowKind {
    #[default]
    Tdd,
    SinglePrompt,
    LoopFile,
    Refactor,
}

impl FlowKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tdd => "tdd",
            Self::SinglePrompt => "single_prompt",
            Self::LoopFile => "loop_file",
            Self::Refactor => "refactor",
        }
    }

    /// Resolve a configured flow string to its canonical name.
    /// Known flow names are normalized, empty input defaults to `SinglePrompt`,
    /// and unrecognized values are returned as-is (custom flow names).
    pub fn resolve_name(input: &str) -> String {
        let trimmed = input.trim();
        trimmed
            .parse::<FlowKind>()
            .map(|kind| kind.as_str().to_owned())
            .unwrap_or_else(|_| {
                if trimmed.is_empty() {
                    FlowKind::SinglePrompt.as_str().to_owned()
                } else {
                    trimmed.to_owned()
                }
            })
    }
}

#[derive(Debug, Clone)]
pub struct FlowParseError {
    input: String,
}

impl fmt::Display for FlowParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown flow '{}'; expected one of: tdd, single_prompt, loop_file, refactor",
            self.input
        )
    }
}

impl std::error::Error for FlowParseError {}

impl FromStr for FlowKind {
    type Err = FlowParseError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "tdd" => Ok(Self::Tdd),
            "single_prompt" => Ok(Self::SinglePrompt),
            "loop_file" => Ok(Self::LoopFile),
            "refactor" => Ok(Self::Refactor),
            other => Err(FlowParseError {
                input: other.to_owned(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkItem {
    pub id: String,
    pub title: String,
    pub details: String,
    pub test_suites: Vec<String>,
}

impl WorkItem {
    pub fn from_todo(todo: Todo) -> Self {
        Self {
            id: todo.id,
            title: todo.todo,
            details: todo.expectations,
            test_suites: todo.test_suites,
        }
    }

    pub fn to_legacy_todo_prompt_json(&self) -> Value {
        json!({
            "id": self.id,
            "todo": self.title,
            "expectations": self.details,
            "test_suites": self.test_suites,
        })
    }
}

#[derive(Debug, Clone)]
pub struct TodoOutcome {
    pub todo_id: String,
    pub commit_hash: Option<String>,
}

#[derive(Debug, Clone)]
struct AgentRunWithGitChanges {
    output: AgentOutput,
    touched_files: Vec<String>,
    had_git_changes: bool,
    head_commit_before: String,
    head_commit_after: String,
    head_commit_changed: bool,
}

#[derive(Debug, Clone, Default)]
struct SinglePromptFailureContext {
    failed_lint: bool,
    failed_test: bool,
    failed_other: bool,
    touched_files_since_last_retry_reset: Vec<String>,
    lint_failures: Vec<SinglePromptFailureItem>,
    test_failures: Vec<SinglePromptFailureItem>,
    other_failures: Vec<SinglePromptFailureItem>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct SinglePromptFailureItem {
    event_id: i64,
    event_type: String,
    message: String,
    command: String,
    output_tail: String,
    sqlite_query: String,
}

const SINGLE_PROMPT_CHANGED_FILES_RETRY_MESSAGE: &str =
    "iteration changed files; waiting for two consecutive no-change iterations";
const SINGLE_PROMPT_RETRY_REASON_PAYLOAD_KEY: &str = "single_prompt_retry_reason";
const SINGLE_PROMPT_RETRY_REASON_CONVERGENCE_CHANGED_FILES: &str = "convergence_changed_files";
const SINGLE_PROMPT_RETRY_HAS_ASSOCIATED_TEST_SUITES_PAYLOAD_KEY: &str =
    "single_prompt_retry_has_associated_test_suites";
const WORK_ITEM_CONTEXT_HASH_PAYLOAD_KEY: &str = "work_item_context_hash";
const TODO_CONTEXT_HASH_PAYLOAD_KEY: &str = "todo_context_hash";
const EXECUTION_CONTEXT_HASH_PAYLOAD_KEY: &str = "execution_context_hash";

#[derive(Debug, Serialize)]
struct WorkItemContextFingerprint {
    id: String,
    title: String,
    details: String,
    test_suites: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SuiteExecutionFingerprint {
    name: String,
    test_root: String,
    target_type: crate::domain::TargetType,
    default_target: Option<String>,
    strip_root_from_target: bool,
    test_command: String,
    lint_command: Option<String>,
    lint_fix_command: Option<String>,
    post_green_command: Option<String>,
    cleanup_command: Option<String>,
    test_init: Option<String>,
    test_setup: Option<String>,
    cache_paths: Vec<String>,
    cache_key_files: Vec<String>,
    cache_mode: crate::config::SuiteCacheMode,
    command_timeout_seconds: Option<u64>,
    env: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct ExecutionContextFingerprint {
    flow: String,
    required_stable_iterations: usize,
    max_loop_iterations: usize,
    agent_timeout_seconds: u64,
    suite_command_timeout_seconds: u64,
    work_item_test_suites: Vec<String>,
    suites: Vec<SuiteExecutionFingerprint>,
}

fn normalized_suite_names(names: &[String]) -> Vec<String> {
    let mut normalized = names
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn md5_hex_of_serializable<T: Serialize>(value: &T) -> String {
    let encoded = serde_json::to_vec(value).unwrap_or_default();
    format!("{:x}", md5::compute(encoded))
}

fn escape_sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SuiteCommandKind {
    Test,
    Lint,
    PostGreen,
}

impl SuiteCommandKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Lint => "lint",
            Self::PostGreen => "post_green",
        }
    }
}

mod command_exec;
mod execution;
mod loop_policy;
mod strategies;
mod suite_checks;

pub use command_exec::{
    configure_process_group, execute_suite_cleanup_command, execute_suite_command,
    suite_command_cwd, suite_command_for_kind, terminate_process_tree,
};
pub use execution::FlowExecution;
pub use loop_policy::{ConvergenceLoopPolicy, LoopPolicy, PhaseStrategy, UntilPassLoopPolicy};
pub use strategies::{
    ExecutionFlow, LoopFileFlow, SinglePromptFlow, TddFlow, TodoFlow, build_flow,
};

pub(crate) use suite_checks::{run_lint_checks, run_test_and_lint};

fn event_exit_code(event: &EventRecord) -> Option<i64> {
    let value = event.payload.get("exit_code")?;
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn suite_name_from_event(event: &EventRecord) -> Option<String> {
    if let Some(suite) = event
        .payload
        .get("suite")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
    {
        return Some(suite);
    }

    if let Some(open) = event.msg.rfind('(')
        && event.msg.ends_with(')')
        && open + 1 < event.msg.len() - 1
    {
        let suite = event.msg[open + 1..event.msg.len() - 1].trim();
        if !suite.is_empty() {
            return Some(suite.to_owned());
        }
    }

    None
}

fn suite_fallback_key_from_event(event: &EventRecord) -> Option<String> {
    event
        .payload
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            let msg = event.msg.trim();
            if msg.is_empty() {
                None
            } else {
                Some(msg.to_owned())
            }
        })
}

fn is_agent_timeout_response_event(event: &EventRecord) -> bool {
    if event.event_type == EventType::AgentResponse {
        return event
            .payload
            .get("output")
            .and_then(Value::as_str)
            .map(|output| {
                output.contains("agent timed out after ") && output.contains(" second(s)")
            })
            .unwrap_or(false);
    }

    event.event_type == EventType::PhaseFailure
        && (event.msg == "single_prompt agent step failed"
            || event.msg == "loop_file agent step failed"
            || event.msg == "loop_file convergence check agent step failed"
            || event.msg == "refactor agent step failed")
        && event_exit_code(event) == Some(124)
}

fn is_single_prompt_convergence_changed_files_retry_event(event: &EventRecord) -> bool {
    if event.event_type != EventType::PhaseFailure {
        return false;
    }

    if matches!(
        event
            .payload
            .get(SINGLE_PROMPT_RETRY_REASON_PAYLOAD_KEY)
            .and_then(Value::as_str),
        Some(reason) if reason == SINGLE_PROMPT_RETRY_REASON_CONVERGENCE_CHANGED_FILES
    ) {
        return true;
    }

    // Backward-compatible fallback for old events logged before retry metadata existed.
    event.msg == SINGLE_PROMPT_CHANGED_FILES_RETRY_MESSAGE
}

fn tail_output_lines(output: &str, max_lines: usize) -> String {
    output
        .lines()
        .rev()
        .take(max_lines.max(1))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

fn single_prompt_failure_item_from_event(
    event: &EventRecord,
    max_output_lines: usize,
) -> SinglePromptFailureItem {
    let command = event
        .payload
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_default();
    let raw_output = event
        .payload
        .get("output")
        .and_then(Value::as_str)
        .or_else(|| event.payload.get("stderr").and_then(Value::as_str))
        .or_else(|| event.payload.get("stdout").and_then(Value::as_str))
        .unwrap_or("");
    let output_tail = tail_output_lines(raw_output, max_output_lines);
    let event_id = event.id.unwrap_or_default();
    let run_id = escape_sql_literal(&event.run_id);
    let todo_id = escape_sql_literal(event.todo_id.as_deref().unwrap_or_default());
    let sqlite_query = format!(
        "SELECT id,timestamp,phase,msg,payload FROM events WHERE run_id='{run_id}' AND todo_id='{todo_id}' AND id={event_id} LIMIT 1;"
    );
    SinglePromptFailureItem {
        event_id,
        event_type: event.event_type.as_str().to_owned(),
        message: event.msg.clone(),
        command,
        output_tail,
        sqlite_query,
    }
}

fn changed_paths_between_snapshots(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut touched = BTreeSet::new();

    for (path, before_signature) in before {
        let changed = match after.get(path) {
            Some(after_signature) => after_signature != before_signature,
            None => true,
        };
        if changed {
            touched.insert(path.clone());
        }
    }

    for path in after.keys() {
        if !before.contains_key(path) {
            touched.insert(path.clone());
        }
    }

    touched.into_iter().collect()
}

#[cfg(test)]
mod tests;
