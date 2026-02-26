use super::parsing::parse_readiness_row;
use super::*;
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

impl ProjectStore {
    pub fn get_readiness_state(&self) -> Result<ProjectReadinessState> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT status, summary, details, checking_started_at, checked_at, updated_at
             FROM readiness_state
             WHERE id = 1
             LIMIT 1",
        )?;
        let row = stmt
            .query_row([], parse_readiness_row)
            .optional()
            .context("failed to read readiness state")?;
        Ok(row.unwrap_or_else(ProjectReadinessState::initial_not_checked))
    }

    pub fn set_readiness_checking(&self, summary: &str) -> Result<()> {
        let conn = self.conn()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO readiness_state (id, status, summary, details, checking_started_at, checked_at, updated_at)
             VALUES (1, ?1, ?2, ?3, ?4, NULL, ?4)
             ON CONFLICT(id) DO UPDATE SET
                 status = excluded.status,
                 summary = excluded.summary,
                 details = excluded.details,
                 checking_started_at = excluded.checking_started_at,
                 checked_at = NULL,
                 updated_at = excluded.updated_at",
            params![
                ReadinessStatus::Checking.as_str(),
                summary.trim(),
                "{}",
                now,
            ],
        )?;
        Ok(())
    }

    pub fn set_readiness_result(
        &self,
        status: ReadinessStatus,
        summary: &str,
        details: &Value,
    ) -> Result<()> {
        if status == ReadinessStatus::Checking {
            return Err(anyhow!(
                "readiness result status cannot be '{}'",
                ReadinessStatus::Checking.as_str()
            ));
        }

        let conn = self.conn()?;
        let now = Utc::now().to_rfc3339();
        let details_text =
            serde_json::to_string(details).context("failed serializing readiness state details")?;
        conn.execute(
            "INSERT INTO readiness_state (id, status, summary, details, checking_started_at, checked_at, updated_at)
             VALUES (1, ?1, ?2, ?3, NULL, ?4, ?4)
             ON CONFLICT(id) DO UPDATE SET
                 status = excluded.status,
                 summary = excluded.summary,
                 details = excluded.details,
                 checking_started_at = NULL,
                 checked_at = excluded.checked_at,
                 updated_at = excluded.updated_at",
            params![status.as_str(), summary.trim(), details_text, now],
        )?;
        Ok(())
    }
}
