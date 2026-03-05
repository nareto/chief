use crate::domain::{EventType, Phase};
use crate::paths;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde_json::Value;
use std::error::Error as StdError;
use std::fs;
use std::path::{Path, PathBuf};

mod events;
mod parsing;
mod readiness;
mod runs;
mod schema;
mod todo_claim;
mod todos;

#[derive(Debug, Clone)]
pub struct DbResetRequiredError {
    pub db_path: PathBuf,
    pub reason: String,
}

impl DbResetRequiredError {
    fn new(db_path: PathBuf, reason: impl Into<String>) -> Self {
        Self {
            db_path,
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for DbResetRequiredError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "chief.db at {} is inconsistent: {}. reset required",
            self.db_path.display(),
            self.reason
        )
    }
}

impl StdError for DbResetRequiredError {}

pub fn db_reset_required_from_anyhow(err: &anyhow::Error) -> Option<DbResetRequiredError> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<DbResetRequiredError>())
        .cloned()
}

#[derive(Debug, Clone)]
pub struct ProjectStore {
    pub project_dir: PathBuf,
    pub db_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct EventQuery {
    pub limit: usize,
    pub event_type: Option<EventType>,
    pub phase: Option<Phase>,
    pub level: Option<String>,
    pub contains_text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessStatus {
    Checking,
    Ready,
    NotReady,
}

impl ReadinessStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Checking => "checking",
            Self::Ready => "ready",
            Self::NotReady => "not_ready",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProjectReadinessState {
    pub status: ReadinessStatus,
    pub summary: String,
    pub details: Value,
    pub checking_started_at: Option<DateTime<Utc>>,
    pub checked_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

impl ProjectReadinessState {
    fn initial_not_checked() -> Self {
        Self {
            status: ReadinessStatus::NotReady,
            summary: "Readiness check has not run yet.".to_owned(),
            details: Value::Object(serde_json::Map::new()),
            checking_started_at: None,
            checked_at: None,
            updated_at: Utc::now(),
        }
    }
}

impl ProjectStore {
    pub fn new(project_dir: impl AsRef<Path>) -> Self {
        let project_dir = project_dir.as_ref().to_path_buf();
        Self {
            db_path: paths::chief_db_path(&project_dir),
            project_dir,
        }
    }

    pub fn init(&self) -> Result<()> {
        if !self.project_dir.exists() {
            fs::create_dir_all(&self.project_dir)
                .with_context(|| format!("failed to create {}", self.project_dir.display()))?;
        }
        let chief_dir = paths::chief_dir(&self.project_dir);
        if !chief_dir.exists() {
            fs::create_dir_all(&chief_dir)
                .with_context(|| format!("failed to create {}", chief_dir.display()))?;
        }
        Ok(())
    }

    pub fn reset_db(&self) -> Result<()> {
        self.reset_db_file()?;
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("failed to open {} for reset", self.db_path.display()))?;
        self.ensure_schema_ready(&conn)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
