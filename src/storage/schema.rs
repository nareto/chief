use super::*;
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use std::fs;
use std::io::ErrorKind;

impl ProjectStore {
    pub(super) fn conn(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("failed to open {}", self.db_path.display()))?;
        if let Err(err) = self.ensure_schema_ready(&conn) {
            return Err(DbResetRequiredError::new(self.db_path.clone(), err.to_string()).into());
        }
        Ok(conn)
    }

    pub(super) fn ensure_schema_ready(&self, conn: &Connection) -> Result<()> {
        self.create_schema(conn)?;
        self.assert_schema_shape(conn)?;
        conn.execute(
            "UPDATE todos SET updated_at = ?1 WHERE updated_at IS NULL OR TRIM(updated_at) = ''",
            params![Utc::now().to_rfc3339()],
        )?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .context("failed to enable sqlite foreign keys")?;
        Ok(())
    }

    fn create_schema(&self, conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS runs (
                run_id TEXT PRIMARY KEY,
                status TEXT NOT NULL,
                exit_status TEXT,
                started_at TEXT NOT NULL,
                ended_at TEXT
            );
            CREATE TABLE IF NOT EXISTS todos (
                id TEXT PRIMARY KEY,
                priority INTEGER NOT NULL,
                todo TEXT NOT NULL,
                expectations TEXT NOT NULL,
                test_suites TEXT NOT NULL,
                status TEXT NOT NULL,
                done_at_commit TEXT,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS jobs (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                todo_id TEXT,
                status TEXT NOT NULL,
                worker_index INTEGER NOT NULL,
                flow TEXT NOT NULL,
                worktree_path TEXT,
                started_at TEXT NOT NULL,
                ended_at TEXT,
                error TEXT
            );
            CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL,
                job_id TEXT,
                todo_id TEXT,
                timestamp TEXT NOT NULL,
                level TEXT NOT NULL,
                phase TEXT,
                msg TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS readiness_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                status TEXT NOT NULL,
                summary TEXT NOT NULL,
                details TEXT NOT NULL,
                checking_started_at TEXT,
                checked_at TEXT,
                updated_at TEXT NOT NULL
            );",
        )
        .context("failed to initialize sqlite schema")?;
        Ok(())
    }

    fn assert_schema_shape(&self, conn: &Connection) -> Result<()> {
        self.assert_table_columns(
            conn,
            "runs",
            &["run_id", "status", "exit_status", "started_at", "ended_at"],
        )?;
        self.assert_table_columns(
            conn,
            "todos",
            &[
                "id",
                "priority",
                "todo",
                "expectations",
                "test_suites",
                "status",
                "done_at_commit",
                "updated_at",
            ],
        )?;
        self.assert_table_columns(
            conn,
            "jobs",
            &[
                "id",
                "run_id",
                "todo_id",
                "status",
                "worker_index",
                "flow",
                "worktree_path",
                "started_at",
                "ended_at",
                "error",
            ],
        )?;
        self.assert_table_columns(
            conn,
            "events",
            &[
                "id",
                "run_id",
                "job_id",
                "todo_id",
                "timestamp",
                "level",
                "phase",
                "msg",
                "event_type",
                "payload",
            ],
        )?;
        self.assert_table_columns(
            conn,
            "readiness_state",
            &[
                "id",
                "status",
                "summary",
                "details",
                "checking_started_at",
                "checked_at",
                "updated_at",
            ],
        )?;
        if self.table_has_any_foreign_key(conn, "events")? {
            return Err(anyhow!("unexpected foreign key found on events table"));
        }
        Ok(())
    }

    fn assert_table_columns(
        &self,
        conn: &Connection,
        table: &str,
        expected: &[&str],
    ) -> Result<()> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("failed to inspect table {table}"))?;
        let expected_vec = expected
            .iter()
            .map(|col| (*col).to_owned())
            .collect::<Vec<_>>();
        if columns != expected_vec {
            return Err(anyhow!(
                "unexpected schema for table {table}: expected {expected_vec:?}, got {columns:?}"
            ));
        }
        Ok(())
    }

    fn table_has_any_foreign_key(&self, conn: &Connection, table: &str) -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA foreign_key_list({table})"))?;
        let mut rows = stmt.query([])?;
        Ok(rows.next()?.is_some())
    }

    pub(super) fn reset_db_file(&self) -> Result<()> {
        match fs::remove_file(&self.db_path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| {
                format!("failed to remove inconsistent {}", self.db_path.display())
            }),
        }
    }
}
