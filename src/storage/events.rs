use super::parsing::{json_to_sql_value, parse_event_row};
use super::*;
use crate::domain::{EventRecord, Phase};
use anyhow::{Context, Result};
use rusqlite::params;
use serde_json::Value;
use std::io::{self, IsTerminal, Write};

impl ProjectStore {
    pub fn record_event(&self, event: &EventRecord) -> Result<()> {
        if !self.sqlite_log_enabled() {
            emit_event_to_stdout(event);
            return Ok(());
        }
        let conn = self.conn()?;
        let payload = serde_json::to_string(&event.payload)?;
        conn.execute(
            "INSERT INTO events (run_id, job_id, todo_id, timestamp, level, phase, msg, event_type, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                event.run_id,
                event.job_id,
                event.todo_id,
                event.timestamp.to_rfc3339(),
                event.level,
                event.phase.map(Phase::as_str),
                event.msg,
                event.event_type.as_str(),
                payload,
            ],
        )?;
        emit_event_to_stdout(event);
        Ok(())
    }

    pub fn query_events(&self, query: EventQuery) -> Result<Vec<EventRecord>> {
        if !self.sqlite_log_enabled() {
            return Ok(Vec::new());
        }
        let limit = if query.limit == 0 {
            100
        } else {
            query.limit.min(1_000)
        };
        let conn = self.conn()?;

        let mut sql = String::from(
            "SELECT id, run_id, job_id, todo_id, timestamp, level, phase, msg, event_type, payload
             FROM events WHERE 1=1",
        );
        let mut bind_values: Vec<Value> = Vec::new();

        if let Some(event_type) = query.event_type {
            sql.push_str(" AND event_type = ?");
            bind_values.push(Value::String(event_type.as_str().to_owned()));
        }
        if let Some(phase) = query.phase {
            sql.push_str(" AND phase = ?");
            bind_values.push(Value::String(phase.as_str().to_owned()));
        }
        if let Some(level) = &query.level {
            sql.push_str(" AND level = ?");
            bind_values.push(Value::String(level.to_owned()));
        }
        if let Some(text) = &query.contains_text {
            sql.push_str(" AND (msg LIKE ? OR payload LIKE ?)");
            let pattern = format!("%{text}%");
            bind_values.push(Value::String(pattern.clone()));
            bind_values.push(Value::String(pattern));
        }

        sql.push_str(" ORDER BY id DESC LIMIT ?");
        bind_values.push(Value::Number((limit as i64).into()));

        let mut stmt = conn.prepare(&sql)?;
        let rusqlite_values: Vec<rusqlite::types::Value> =
            bind_values.into_iter().map(json_to_sql_value).collect();

        let rows = stmt.query_map(rusqlite::params_from_iter(rusqlite_values), |row| {
            parse_event_row(row)
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to query events")
    }

    pub fn query_events_after_id(&self, after_id: i64, limit: usize) -> Result<Vec<EventRecord>> {
        if !self.sqlite_log_enabled() {
            return Ok(Vec::new());
        }
        let limit = if limit == 0 { 200 } else { limit.min(2_000) };
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, run_id, job_id, todo_id, timestamp, level, phase, msg, event_type, payload
             FROM events
             WHERE id > ?1
             ORDER BY id ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![after_id, limit as i64], parse_event_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to query events after id")
    }

    pub fn trim_events_to_recent_runs(&self, keep_runs: usize) -> Result<usize> {
        if !self.sqlite_log_enabled() {
            return Ok(0);
        }
        let keep_runs = keep_runs.max(1);
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let deleted = tx.execute(
            "WITH keep AS (
                SELECT run_id
                FROM runs
                ORDER BY started_at DESC
                LIMIT ?1
            ),
            fallback_keep AS (
                SELECT run_id
                FROM events
                GROUP BY run_id
                ORDER BY MAX(id) DESC
                LIMIT ?1
            )
            DELETE FROM events
            WHERE run_id NOT IN (
                SELECT run_id FROM keep
                UNION
                SELECT run_id FROM fallback_keep
                WHERE NOT EXISTS (SELECT 1 FROM keep)
            )",
            params![keep_runs as i64],
        )?;
        tx.commit()?;
        conn.execute_batch("VACUUM;")
            .context("failed to compact chief.db after trim")?;
        Ok(deleted)
    }
}

fn emit_event_to_stdout(event: &EventRecord) {
    if !should_emit_event_to_stdout() {
        return;
    }

    let color = should_use_color_stdout();
    let ts = event.timestamp.to_rfc3339();
    let level = format_level_label(&event.level, color);
    let phase = format_phase_label(event.phase, color);
    let event_type = style(event.event_type.as_str(), "\x1b[34m", color);
    let timestamp = style(&ts, "\x1b[90m", color);
    let line = format!("[{timestamp}] {level} {phase} {event_type} - {}", event.msg);
    println!("{line}");
    let _ = io::stdout().flush();
}

fn should_emit_event_to_stdout() -> bool {
    match std::env::var("CHIEF_EVENT_STDOUT") {
        Ok(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            !(normalized == "0" || normalized == "false" || normalized == "off")
        }
        Err(_) => true,
    }
}

fn should_use_color_stdout() -> bool {
    if let Ok(force) = std::env::var("CLICOLOR_FORCE")
        && force.trim() != "0"
        && !force.trim().is_empty()
    {
        return true;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    io::stdout().is_terminal()
}

fn format_level_label(level: &str, color: bool) -> String {
    let normalized = level.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "error" => style("ERROR", "\x1b[31m", color),
        "warning" | "warn" => style("WARN ", "\x1b[33m", color),
        "info" => style("INFO ", "\x1b[36m", color),
        other if !other.is_empty() => style(&other.to_ascii_uppercase(), "\x1b[37m", color),
        _ => style("INFO ", "\x1b[36m", color),
    }
}

fn format_phase_label(phase: Option<Phase>, color: bool) -> String {
    let raw = phase.map(Phase::as_str).unwrap_or("-");
    style(raw, "\x1b[35m", color)
}

fn style(text: &str, code: &str, enabled: bool) -> String {
    if !enabled {
        return text.to_owned();
    }
    format!("{code}{text}\x1b[0m")
}
