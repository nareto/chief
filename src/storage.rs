use crate::domain::{
    EventRecord, EventType, JobRecord, JobStatus, Phase, RunExitStatus, Todo, TodoFile, TodoStatus,
};
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;
use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

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
    pub todos_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct EventQuery {
    pub limit: usize,
    pub event_type: Option<EventType>,
    pub phase: Option<Phase>,
    pub level: Option<String>,
    pub contains_text: Option<String>,
}

impl ProjectStore {
    pub fn new(project_dir: impl AsRef<Path>) -> Self {
        let project_dir = project_dir.as_ref().to_path_buf();
        Self {
            db_path: project_dir.join("chief.db"),
            todos_path: project_dir.join("todos.json"),
            project_dir,
        }
    }

    pub fn init(&self) -> Result<()> {
        if !self.project_dir.exists() {
            fs::create_dir_all(&self.project_dir)
                .with_context(|| format!("failed to create {}", self.project_dir.display()))?;
        }
        if !self.todos_path.exists() {
            let initial = serde_json::to_string_pretty(&TodoFile::default())?;
            fs::write(&self.todos_path, format!("{initial}\n"))
                .with_context(|| format!("failed to initialize {}", self.todos_path.display()))?;
        }
        Ok(())
    }

    pub fn reset_db_from_todos_json(&self) -> Result<()> {
        self.reset_db_file()?;
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("failed to open {} for reset", self.db_path.display()))?;
        self.ensure_schema_ready(&conn)?;
        drop(conn);
        self.sync_todos_from_file()?;
        self.reset_in_progress_todos_to_pending()?;
        Ok(())
    }

    pub fn reset_in_progress_todos_to_pending(&self) -> Result<usize> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE todos
             SET status = ?1, done_at_commit = NULL, updated_at = ?2
             WHERE status = ?3",
            params![
                TodoStatus::Pending.as_str(),
                Utc::now().to_rfc3339(),
                TodoStatus::InProgress.as_str(),
            ],
        )?;
        if changed > 0 {
            self.sync_todos_file_from_conn(&tx)?;
        }
        tx.commit()?;
        Ok(changed)
    }

    pub fn load_todo_file(&self) -> Result<TodoFile> {
        let content = fs::read_to_string(&self.todos_path)
            .with_context(|| format!("failed to read {}", self.todos_path.display()))?;
        if content.trim().is_empty() {
            return Ok(TodoFile::default());
        }
        let parsed: TodoFile = serde_json::from_str(&content)
            .with_context(|| format!("invalid JSON in {}", self.todos_path.display()))?;
        Ok(TodoFile {
            todos: parsed.todos.into_iter().map(Todo::normalize).collect(),
        })
    }

    pub fn save_todo_file(&self, todo_file: &TodoFile) -> Result<()> {
        let normalized = TodoFile {
            todos: todo_file
                .todos
                .iter()
                .cloned()
                .map(Todo::normalize)
                .collect(),
        };
        let body = serde_json::to_string_pretty(&normalized)?;
        fs::write(&self.todos_path, format!("{body}\n"))
            .with_context(|| format!("failed to write {}", self.todos_path.display()))?;
        Ok(())
    }

    pub fn sync_todos_from_file(&self) -> Result<()> {
        let todos = self.load_todo_file()?.todos;
        let conn = self.conn()?;
        for todo in todos {
            self.upsert_todo_row(&conn, &todo)?;
        }
        Ok(())
    }

    pub fn list_todos(&self) -> Result<Vec<Todo>> {
        let conn = self.conn()?;
        self.list_todos_with_conn(&conn)
    }

    pub fn list_available_todos(&self) -> Result<Vec<Todo>> {
        let todos = self.list_todos()?;
        Ok(todos
            .into_iter()
            .filter(|todo| todo.status != TodoStatus::Done && todo.status != TodoStatus::InProgress)
            .collect())
    }

    pub fn list_in_progress_todos(&self) -> Result<Vec<Todo>> {
        let todos = self.list_todos()?;
        Ok(todos
            .into_iter()
            .filter(|todo| todo.status == TodoStatus::InProgress)
            .collect())
    }

    pub fn update_todo_status(
        &self,
        todo_id: &str,
        status: TodoStatus,
        done_at_commit: Option<&str>,
    ) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE todos
             SET status = ?1, done_at_commit = COALESCE(?2, done_at_commit), updated_at = ?3
             WHERE id = ?4",
            params![
                status.as_str(),
                done_at_commit,
                Utc::now().to_rfc3339(),
                todo_id
            ],
        )?;
        if changed == 0 {
            return Err(anyhow!("todo '{}' not found", todo_id));
        }
        self.sync_todos_file_from_conn(&tx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn append_todo(&self, todo: Todo) -> Result<Todo> {
        let normalized = todo.normalize();
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        self.upsert_todo_row(&tx, &normalized)?;
        self.sync_todos_file_from_conn(&tx)?;
        tx.commit()?;
        Ok(normalized)
    }

    pub fn update_todo(&self, existing_id: &str, todo: Todo) -> Result<Todo> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;

        let mut stmt = tx.prepare(
            "SELECT id, priority, todo, expectations, test_suites, status, done_at_commit
             FROM todos
             WHERE id = ?1
             LIMIT 1",
        )?;
        let existing = stmt
            .query_row(params![existing_id], parse_todo_row)
            .optional()
            .context("failed to fetch todo for update")?;
        drop(stmt);

        let Some(existing) = existing else {
            return Err(anyhow!("todo '{}' not found", existing_id));
        };

        let mut next = todo.normalize();
        if next.id.trim().is_empty() {
            next.id = existing.id;
        }

        if next.id != existing_id {
            let mut conflict_stmt = tx.prepare("SELECT 1 FROM todos WHERE id = ?1 LIMIT 1")?;
            let has_conflict = conflict_stmt
                .query_row(params![&next.id], |_| Ok(()))
                .optional()?
                .is_some();
            drop(conflict_stmt);
            if has_conflict {
                return Err(anyhow!("todo '{}' already exists", next.id));
            }

            tx.execute("DELETE FROM todos WHERE id = ?1", params![existing_id])?;
        }

        self.upsert_todo_row(&tx, &next)?;
        self.sync_todos_file_from_conn(&tx)?;
        tx.commit()?;
        Ok(next)
    }

    pub fn clean_completed_todos_with_commit(&self) -> Result<usize> {
        let todos = self.list_todos()?;
        let before = todos.len();
        let retained = todos
            .into_iter()
            .filter(|todo| !(todo.status == TodoStatus::Done && todo.done_at_commit.is_some()))
            .collect::<Vec<_>>();
        self.persist_todo_list(&retained)?;
        Ok(before.saturating_sub(retained.len()))
    }

    pub fn claim_todo(&self, todo_id: &str) -> Result<Option<Todo>> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;

        let mut stmt = tx.prepare(
            "SELECT id, priority, todo, expectations, test_suites, status, done_at_commit
             FROM todos
             WHERE id = ?1
             LIMIT 1",
        )?;

        let Some(mut todo) = stmt
            .query_row(params![todo_id], parse_todo_row)
            .optional()
            .context("failed to fetch todo for claim")?
        else {
            return Ok(None);
        };
        drop(stmt);

        if matches!(todo.status, TodoStatus::Done | TodoStatus::InProgress) {
            return Ok(None);
        }

        let changed = tx.execute(
            "UPDATE todos
             SET status = ?1, updated_at = ?2
             WHERE id = ?3 AND status NOT IN ('done', 'in_progress')",
            params![
                TodoStatus::InProgress.as_str(),
                Utc::now().to_rfc3339(),
                todo_id
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }

        todo.status = TodoStatus::InProgress;

        self.sync_todos_file_from_conn(&tx)?;
        tx.commit()?;
        Ok(Some(todo))
    }

    pub fn start_run(&self, run_id: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO runs (run_id, status, exit_status, started_at, ended_at) VALUES (?1, ?2, NULL, ?3, NULL)",
            params![run_id, "running", Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn finish_run(&self, run_id: &str, exit_status: RunExitStatus) -> Result<()> {
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
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, run_id, todo_id, status, worker_index, flow, worktree_path, started_at, ended_at, error
             FROM jobs
             ORDER BY started_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
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

    pub fn record_event(&self, event: &EventRecord) -> Result<()> {
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
        Ok(())
    }

    pub fn query_events(&self, query: EventQuery) -> Result<Vec<EventRecord>> {
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
            let pattern = format!("%{}%", text);
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

    fn persist_todo_list(&self, todos: &[Todo]) -> Result<()> {
        let todo_file = TodoFile {
            todos: todos.iter().cloned().map(Todo::normalize).collect(),
        };
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        for todo in todo_file.todos {
            self.upsert_todo_row(&tx, &todo)?;
        }
        self.sync_todos_file_from_conn(&tx)?;
        tx.commit()?;
        Ok(())
    }

    fn list_todos_with_conn(&self, conn: &Connection) -> Result<Vec<Todo>> {
        let mut stmt = conn.prepare(
            "SELECT id, priority, todo, expectations, test_suites, status, done_at_commit
             FROM todos
             ORDER BY priority DESC, id ASC",
        )?;
        let rows = stmt.query_map([], parse_todo_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to read todos")
    }

    fn sync_todos_file_from_conn(&self, conn: &Connection) -> Result<()> {
        let todos = self.list_todos_with_conn(conn)?;
        self.save_todo_file(&TodoFile { todos })
    }

    fn upsert_todo_row(&self, conn: &Connection, todo: &Todo) -> Result<()> {
        let suites_text = serde_json::to_string(&todo.test_suites)?;
        conn.execute(
            "INSERT INTO todos (id, priority, todo, expectations, test_suites, status, done_at_commit, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                 priority = excluded.priority,
                 todo = excluded.todo,
                 expectations = excluded.expectations,
                 test_suites = excluded.test_suites,
                 status = excluded.status,
                 done_at_commit = excluded.done_at_commit,
                 updated_at = excluded.updated_at",
            params![
                todo.id,
                todo.priority,
                todo.todo,
                todo.expectations,
                suites_text,
                todo.status.as_str(),
                todo.done_at_commit,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    fn conn(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("failed to open {}", self.db_path.display()))?;
        if let Err(err) = self.ensure_schema_ready(&conn) {
            return Err(DbResetRequiredError::new(self.db_path.clone(), err.to_string()).into());
        }
        Ok(conn)
    }

    fn ensure_schema_ready(&self, conn: &Connection) -> Result<()> {
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
                "unexpected schema for table {table}: expected {:?}, got {:?}",
                expected_vec,
                columns
            ));
        }
        Ok(())
    }

    fn table_has_any_foreign_key(&self, conn: &Connection, table: &str) -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA foreign_key_list({table})"))?;
        let mut rows = stmt.query([])?;
        Ok(rows.next()?.is_some())
    }

    fn reset_db_file(&self) -> Result<()> {
        match fs::remove_file(&self.db_path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| {
                format!("failed to remove inconsistent {}", self.db_path.display())
            }),
        }
    }
}

fn parse_todo_status(value: &str) -> TodoStatus {
    match value {
        "in_progress" => TodoStatus::InProgress,
        "attempted" => TodoStatus::Attempted,
        "done" => TodoStatus::Done,
        _ => TodoStatus::Pending,
    }
}

fn parse_todo_row(row: &Row<'_>) -> rusqlite::Result<Todo> {
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

fn parse_job_status(value: &str) -> JobStatus {
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

fn parse_event_row(row: &Row<'_>) -> rusqlite::Result<EventRecord> {
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

fn parse_datetime(value: &str) -> Result<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|err| anyhow!("invalid datetime {value}: {err}"))?;
    Ok(parsed.with_timezone(&Utc))
}

fn json_to_sql_value(value: Value) -> rusqlite::types::Value {
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

#[cfg(test)]
mod tests {
    use super::ProjectStore;
    use crate::domain::{EventRecord, EventType, Todo, TodoStatus};
    use chrono::Utc;
    use rusqlite::Connection;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn temp_project_dir() -> PathBuf {
        std::env::temp_dir().join(format!("chief-storage-test-{}", Uuid::new_v4()))
    }

    #[test]
    fn claim_todo_updates_sqlite_and_todos_file() {
        let project_dir = temp_project_dir();
        let store = ProjectStore::new(&project_dir);
        store.init().expect("store init should succeed");

        let todo = Todo {
            id: String::new(),
            todo: "test atomic claim".to_owned(),
            expectations: String::new(),
            priority: 1,
            test_suites: Vec::new(),
            status: TodoStatus::Pending,
            done_at_commit: None,
        }
        .normalize();
        let todo = store.append_todo(todo).expect("append_todo should succeed");

        let claimed = store
            .claim_todo(&todo.id)
            .expect("claim_todo should succeed")
            .expect("todo should be claimable");
        assert_eq!(claimed.status, TodoStatus::InProgress);

        let db_todo = store
            .list_todos()
            .expect("list_todos should succeed")
            .into_iter()
            .find(|item| item.id == todo.id)
            .expect("todo should exist in db");
        assert_eq!(db_todo.status, TodoStatus::InProgress);

        let file_todo = store
            .load_todo_file()
            .expect("load_todo_file should succeed")
            .todos
            .into_iter()
            .find(|item| item.id == todo.id)
            .expect("todo should exist in file");
        assert_eq!(file_todo.status, TodoStatus::InProgress);

        let _ = fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn update_todo_status_fails_for_missing_todo() {
        let project_dir = temp_project_dir();
        let store = ProjectStore::new(&project_dir);
        store.init().expect("store init should succeed");

        let err = store
            .update_todo_status("missing-id", TodoStatus::Done, None)
            .expect_err("missing todo should return error");
        assert!(
            err.to_string().contains("not found"),
            "unexpected error message: {}",
            err
        );

        let _ = fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn inconsistent_db_requires_confirmation_before_reset() {
        let project_dir = temp_project_dir();
        let store = ProjectStore::new(&project_dir);
        store.init().expect("store init should succeed");

        let todo = Todo {
            id: String::new(),
            todo: "survives db reset".to_owned(),
            expectations: String::new(),
            priority: 3,
            test_suites: Vec::new(),
            status: TodoStatus::InProgress,
            done_at_commit: None,
        }
        .normalize();
        let todo = store.append_todo(todo).expect("append_todo should succeed");

        let conn = Connection::open(project_dir.join("chief.db")).expect("db should open");
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
            DROP TABLE todos;
            CREATE TABLE todos (
                id TEXT PRIMARY KEY,
                run_id TEXT
            );
            PRAGMA foreign_keys = ON;",
        )
        .expect("should create inconsistent schema");
        drop(conn);

        let err = store
            .sync_todos_from_file()
            .expect_err("sync_todos_from_file should require db reset");
        let reset_error = super::db_reset_required_from_anyhow(&err)
            .expect("error should carry db reset required details");
        assert_eq!(
            reset_error.db_path,
            project_dir.join("chief.db"),
            "reset-required error should include db path"
        );

        store
            .reset_db_from_todos_json()
            .expect("explicit db reset should succeed");

        let conn = Connection::open(project_dir.join("chief.db")).expect("db should reopen");
        let mut stmt = conn
            .prepare("PRAGMA table_info(todos)")
            .expect("table info should prepare");
        let todo_columns = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .expect("table info should query")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("table info rows should parse");
        assert_eq!(
            todo_columns,
            vec![
                "id".to_owned(),
                "priority".to_owned(),
                "todo".to_owned(),
                "expectations".to_owned(),
                "test_suites".to_owned(),
                "status".to_owned(),
                "done_at_commit".to_owned(),
                "updated_at".to_owned(),
            ],
            "recreated todos table should match canonical schema"
        );

        let todos = store.list_todos().expect("list_todos should succeed");
        let recovered = todos.iter().find(|item| item.id == todo.id);
        assert!(
            recovered.is_some(),
            "todo should still exist after db reset"
        );
        assert_eq!(
            recovered.expect("todo should be present").status,
            TodoStatus::Pending,
            "in_progress todos should be re-queued to pending during reset"
        );

        let _ = fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn trim_events_keeps_only_latest_runs() {
        let project_dir = temp_project_dir();
        let store = ProjectStore::new(&project_dir);
        store.init().expect("store init should succeed");

        for index in 1..=4 {
            let run_id = format!("run-{index}");
            store.start_run(&run_id).expect("start_run should succeed");
            store
                .record_event(&EventRecord {
                    id: None,
                    run_id: run_id.clone(),
                    job_id: None,
                    todo_id: None,
                    timestamp: Utc::now(),
                    level: "info".to_owned(),
                    phase: None,
                    msg: format!("event for {run_id}"),
                    event_type: EventType::Msg,
                    payload: BTreeMap::new(),
                })
                .expect("record_event should succeed");
        }

        let conn = Connection::open(project_dir.join("chief.db")).expect("db should open");
        conn.execute(
            "UPDATE runs SET started_at = ?1 WHERE run_id = 'run-1'",
            ["2024-01-01T00:00:00Z"],
        )
        .expect("should update run-1 started_at");
        conn.execute(
            "UPDATE runs SET started_at = ?1 WHERE run_id = 'run-2'",
            ["2024-01-02T00:00:00Z"],
        )
        .expect("should update run-2 started_at");
        conn.execute(
            "UPDATE runs SET started_at = ?1 WHERE run_id = 'run-3'",
            ["2024-01-03T00:00:00Z"],
        )
        .expect("should update run-3 started_at");
        conn.execute(
            "UPDATE runs SET started_at = ?1 WHERE run_id = 'run-4'",
            ["2024-01-04T00:00:00Z"],
        )
        .expect("should update run-4 started_at");
        drop(conn);

        let deleted = store
            .trim_events_to_recent_runs(2)
            .expect("trim_events_to_recent_runs should succeed");
        assert_eq!(deleted, 2, "two older runs should be removed");

        let remaining = store
            .query_events(super::EventQuery {
                limit: 10,
                ..super::EventQuery::default()
            })
            .expect("query_events should succeed");
        let remaining_run_ids = remaining
            .into_iter()
            .map(|event| event.run_id)
            .collect::<Vec<_>>();
        assert_eq!(
            remaining_run_ids,
            vec!["run-4".to_owned(), "run-3".to_owned()],
            "only latest two runs should remain",
        );

        let _ = fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn trim_events_reclaims_db_space() {
        let project_dir = temp_project_dir();
        let store = ProjectStore::new(&project_dir);
        store.init().expect("store init should succeed");

        for index in 1..=8 {
            let run_id = format!("run-{index}");
            store.start_run(&run_id).expect("start_run should succeed");
            for event_index in 0..30 {
                store
                    .record_event(&EventRecord {
                        id: None,
                        run_id: run_id.clone(),
                        job_id: None,
                        todo_id: None,
                        timestamp: Utc::now(),
                        level: "info".to_owned(),
                        phase: None,
                        msg: format!("event {event_index} {}", "x".repeat(4096)),
                        event_type: EventType::Msg,
                        payload: BTreeMap::new(),
                    })
                    .expect("record_event should succeed");
            }
        }

        let db_path = project_dir.join("chief.db");
        let size_before = fs::metadata(&db_path)
            .expect("db metadata before trim should be readable")
            .len();

        let deleted = store
            .trim_events_to_recent_runs(1)
            .expect("trim_events_to_recent_runs should succeed");
        assert!(deleted > 0, "trim should delete older events");

        let size_after = fs::metadata(&db_path)
            .expect("db metadata after trim should be readable")
            .len();
        assert!(
            size_after < size_before,
            "expected chief.db to shrink after trim (before={size_before}, after={size_after})"
        );

        let _ = fs::remove_dir_all(&project_dir);
    }
}
