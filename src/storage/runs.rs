use super::parsing::{parse_datetime, parse_job_status};
use super::*;
use crate::domain::{JobRecord, RunExitStatus};
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::params;

impl ProjectStore {
    pub fn start_run(&self, run_id: &str) -> Result<()> {
        if !self.sqlite_log_enabled() {
            return Ok(());
        }
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO runs (run_id, status, exit_status, started_at, ended_at) VALUES (?1, ?2, NULL, ?3, NULL)",
            params![run_id, "running", Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn finish_run(&self, run_id: &str, exit_status: RunExitStatus) -> Result<()> {
        if !self.sqlite_log_enabled() {
            return Ok(());
        }
        let conn = self.conn()?;
        conn.execute(
            "UPDATE runs SET status = ?1, exit_status = ?2, ended_at = ?3 WHERE run_id = ?4",
            params![
                "finished",
                exit_status.as_str(),
                Utc::now().to_rfc3339(),
                run_id
            ],
        )?;
        Ok(())
    }

    pub fn upsert_job(&self, job: &JobRecord) -> Result<()> {
        if !self.sqlite_log_enabled() {
            return Ok(());
        }
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO jobs (id, run_id, todo_id, status, worker_index, flow, worktree_path, started_at, ended_at, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
               todo_id = excluded.todo_id,
               status = excluded.status,
               worker_index = excluded.worker_index,
               flow = excluded.flow,
               worktree_path = excluded.worktree_path,
               started_at = excluded.started_at,
               ended_at = excluded.ended_at,
               error = excluded.error",
            params![
                job.id,
                job.run_id,
                job.todo_id,
                job.status.as_str(),
                job.worker_index as i64,
                job.flow,
                job.worktree_path,
                job.started_at.to_rfc3339(),
                job.ended_at.map(|dt| dt.to_rfc3339()),
                job.error,
            ],
        )?;
        Ok(())
    }

    pub fn list_jobs(&self, limit: usize) -> Result<Vec<JobRecord>> {
        if !self.sqlite_log_enabled() {
            return Ok(Vec::new());
        }
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, run_id, todo_id, status, worker_index, flow, worktree_path, started_at, ended_at, error
             FROM jobs
             ORDER BY started_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row: &rusqlite::Row<'_>| {
            let started_at: Option<String> = row.get(7)?;
            let ended_at: Option<String> = row.get(8)?;
            let worker_index: i64 = row.get(4)?;
            Ok(JobRecord {
                id: row.get(0)?,
                run_id: row.get(1)?,
                todo_id: row.get(2)?,
                status: parse_job_status(&row.get::<_, String>(3)?),
                worker_index: worker_index as usize,
                flow: row.get(5)?,
                worktree_path: row.get(6)?,
                started_at: started_at
                    .as_deref()
                    .and_then(|value| parse_datetime(value).ok())
                    .unwrap_or_else(Utc::now),
                ended_at: ended_at
                    .as_deref()
                    .and_then(|value| parse_datetime(value).ok()),
                error: row.get(9)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to list jobs")
    }
}
