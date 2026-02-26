use super::{ProjectReadinessState, ReadinessStatus};
use crate::domain::{EventRecord, EventType, JobStatus, Phase, Todo, TodoStatus};
use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use rusqlite::Row;
use serde_json::Value;
use std::collections::BTreeMap;

fn parse_todo_status(value: &str) -> TodoStatus {
    match value {
        "in_progress" => TodoStatus::InProgress,
        "done" => TodoStatus::Done,
        _ => TodoStatus::Pending,
    }
}

pub(super) fn parse_todo_row(row: &Row<'_>) -> rusqlite::Result<Todo> {
    let suites_text: String = row.get(4)?;
    let suites: Vec<String> = serde_json::from_str(&suites_text).unwrap_or_default();
    let status_text: String = row.get(5)?;
    Ok(Todo {
        id: row.get(0)?,
        priority: row.get(1)?,
        todo: row.get(2)?,
        expectations: row.get(3)?,
        test_suites: suites,
        status: parse_todo_status(&status_text),
        done_at_commit: row.get(6)?,
    })
}

pub(super) fn parse_job_status(value: &str) -> JobStatus {
    match value {
        "selecting" => JobStatus::Selecting,
        "running" => JobStatus::Running,
        "merging" => JobStatus::Merging,
        "completed" => JobStatus::Completed,
        "failed" => JobStatus::Failed,
        "cancelled" => JobStatus::Cancelled,
        _ => JobStatus::Queued,
    }
}

fn parse_readiness_status(value: &str) -> ReadinessStatus {
    match value {
        "checking" => ReadinessStatus::Checking,
        "ready" => ReadinessStatus::Ready,
        _ => ReadinessStatus::NotReady,
    }
}

pub(super) fn parse_readiness_row(row: &Row<'_>) -> rusqlite::Result<ProjectReadinessState> {
    let status = row
        .get::<_, Option<String>>(0)?
        .map(|value| parse_readiness_status(&value))
        .unwrap_or(ReadinessStatus::NotReady);
    let summary = row.get::<_, Option<String>>(1)?.unwrap_or_default();
    let details_text = row.get::<_, Option<String>>(2)?;
    let details = details_text
        .as_deref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    let checking_started_at = row
        .get::<_, Option<String>>(3)?
        .as_deref()
        .and_then(|value| parse_datetime(value).ok());
    let checked_at = row
        .get::<_, Option<String>>(4)?
        .as_deref()
        .and_then(|value| parse_datetime(value).ok());
    let updated_at = row
        .get::<_, Option<String>>(5)?
        .as_deref()
        .and_then(|value| parse_datetime(value).ok())
        .unwrap_or_else(Utc::now);

    Ok(ProjectReadinessState {
        status,
        summary,
        details,
        checking_started_at,
        checked_at,
        updated_at,
    })
}

pub(super) fn parse_event_row(row: &Row<'_>) -> rusqlite::Result<EventRecord> {
    let payload_text: Option<String> = row.get(9)?;
    let payload: BTreeMap<String, serde_json::Value> = payload_text
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_default();
    let phase_text: Option<String> = row.get(6)?;
    let event_type_text: Option<String> = row.get(8)?;
    let timestamp_text: Option<String> = row.get(4)?;
    let level_text: Option<String> = row.get(5)?;
    let msg_text: Option<String> = row.get(7)?;

    Ok(EventRecord {
        id: row.get(0)?,
        run_id: row.get(1)?,
        job_id: row.get(2)?,
        todo_id: row.get(3)?,
        timestamp: timestamp_text
            .as_deref()
            .and_then(|value| parse_datetime(value).ok())
            .unwrap_or_else(Utc::now),
        level: level_text.unwrap_or_else(|| "info".to_owned()),
        phase: phase_text.as_deref().map(parse_phase),
        msg: msg_text.unwrap_or_default(),
        event_type: parse_event_type(event_type_text.as_deref().unwrap_or("msg")),
        payload,
    })
}

fn parse_phase(value: &str) -> Phase {
    match value {
        "todo_selection" => Phase::TodoSelection,
        "red" => Phase::Red,
        "green" => Phase::Green,
        "single_prompt" => Phase::SinglePrompt,
        "loop_file" => Phase::LoopFile,
        "post_green" => Phase::PostGreen,
        "exit" => Phase::Exit,
        _ => Phase::Start,
    }
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

pub(super) fn parse_datetime(value: &str) -> Result<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|err| anyhow!("invalid datetime {value}: {err}"))?;
    Ok(parsed.with_timezone(&Utc))
}

pub(super) fn json_to_sql_value(value: Value) -> rusqlite::types::Value {
    match value {
        Value::Null => rusqlite::types::Value::Null,
        Value::Bool(b) => rusqlite::types::Value::Integer(if b { 1 } else { 0 }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                rusqlite::types::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                rusqlite::types::Value::Real(f)
            } else {
                rusqlite::types::Value::Null
            }
        }
        Value::String(s) => rusqlite::types::Value::Text(s),
        Value::Array(arr) => {
            rusqlite::types::Value::Text(serde_json::to_string(&arr).unwrap_or_default())
        }
        Value::Object(obj) => {
            rusqlite::types::Value::Text(serde_json::to_string(&obj).unwrap_or_default())
        }
    }
}
