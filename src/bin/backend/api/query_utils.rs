use std::collections::HashSet;

use chief::domain::{EventType, Phase, Todo, TodoStatus};
use chief::git::GitOps;

use crate::api::error::ApiError;
use crate::api::types::PhaseIteration;

pub(crate) fn is_internal_workspace_state_file(path: &str) -> bool {
    path == ".chief/chief.db"
        || path.starts_with(".chief/chief.db-")
        || path == ".chief/todos.yaml"
        || path == "chief.db"
        || path.starts_with("chief.db-")
        || path == "todos.yaml"
}

pub(crate) fn resolve_last_done_todo_committed_at(
    git: &impl GitOps,
    project_dir: &std::path::Path,
    todos: &[Todo],
) -> Option<String> {
    let mut seen_commits = HashSet::new();
    let mut latest_timestamp: Option<chrono::DateTime<chrono::Utc>> = None;

    for commit_hash in todos
        .iter()
        .filter(|todo| todo.status == TodoStatus::Done)
        .filter_map(|todo| todo.done_at_commit.as_deref())
        .map(str::trim)
        .filter(|commit_hash| !commit_hash.is_empty())
    {
        if !seen_commits.insert(commit_hash.to_owned()) {
            continue;
        }

        let timestamp = match git.commit_committer_timestamp_rfc3339(project_dir, commit_hash) {
            Ok(value) => value,
            Err(_) => continue,
        };

        let parsed = match chrono::DateTime::parse_from_rfc3339(&timestamp) {
            Ok(value) => value.with_timezone(&chrono::Utc),
            Err(_) => continue,
        };

        if latest_timestamp
            .as_ref()
            .map(|current| parsed > *current)
            .unwrap_or(true)
        {
            latest_timestamp = Some(parsed);
        }
    }

    latest_timestamp.map(|timestamp| timestamp.to_rfc3339())
}

pub(crate) fn parse_loop_iteration(msg: &str) -> Option<PhaseIteration> {
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

pub(crate) fn parse_todo_status_input(value: &str) -> Option<TodoStatus> {
    match value.trim() {
        "pending" => Some(TodoStatus::Pending),
        "in_progress" => Some(TodoStatus::InProgress),
        "done" => Some(TodoStatus::Done),
        _ => None,
    }
}

pub(crate) fn parse_requested_types(input: Option<&str>) -> Vec<String> {
    let Some(raw) = input else {
        return Vec::new();
    };

    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_lowercase)
        .collect()
}

pub(crate) fn matches_requested_type(event_type: EventType, requested: &[String]) -> bool {
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
        | EventType::LintFix
        | EventType::PhaseFailure => "test",
        EventType::Msg | EventType::PhaseChange | EventType::Error | EventType::Job => "log",
    }
}

pub(crate) fn parse_event_type(value: &str) -> Result<EventType, ApiError> {
    match value {
        "msg" => Ok(EventType::Msg),
        "test_run" => Ok(EventType::TestRun),
        "post_green_output" => Ok(EventType::PostGreenOutput),
        "lint" => Ok(EventType::Lint),
        "lint_fix" => Ok(EventType::LintFix),
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
            "unsupported event_type '{other}', see /api/projects/{{project}}/events for valid values"
        ))),
    }
}

pub(crate) fn parse_phase(value: &str) -> Result<Phase, ApiError> {
    match value {
        "start" => Ok(Phase::Start),
        "todo_selection" => Ok(Phase::TodoSelection),
        "red" => Ok(Phase::Red),
        "green" => Ok(Phase::Green),
        "single_prompt" => Ok(Phase::SinglePrompt),
        "loop_file" => Ok(Phase::LoopFile),
        "refactor" => Ok(Phase::Refactor),
        "post_green" => Ok(Phase::PostGreen),
        "exit" => Ok(Phase::Exit),
        other => Err(ApiError::unprocessable(format!(
            "unsupported phase '{other}'"
        ))),
    }
}
