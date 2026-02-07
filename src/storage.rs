use crate::domain::{
    EventRecord, EventType, JobRecord, JobStatus, Phase, RunExitStatus, Todo, TodoFile, TodoStatus,
};
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

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
        let conn = self.conn()?;
        self.migrate(&conn)?;
        self.sync_todos_from_file()?;
        Ok(())
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
        self.migrate(&conn)?;
        for todo in todos {
            self.upsert_todo_row(&conn, &todo)?;
        }
        Ok(())
    }

    pub fn list_todos(&self) -> Result<Vec<Todo>> {
        let conn = self.conn()?;
        self.migrate(&conn)?;
        let mut stmt = conn.prepare(
            "SELECT id, priority, todo, expectations, test_suites, status, done_at_commit
             FROM todos
             ORDER BY priority DESC, id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
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
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to read todos")
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
        let mut todo_file = self.load_todo_file()?;
        if let Some(todo) = todo_file.todos.iter_mut().find(|todo| todo.id == todo_id) {
            todo.status = status;
            if let Some(commit) = done_at_commit {
                todo.done_at_commit = Some(commit.to_owned());
            }
        }
        self.save_todo_file(&todo_file)?;

        let conn = self.conn()?;
        self.migrate(&conn)?;
        conn.execute(
            "UPDATE todos SET status = ?1, done_at_commit = COALESCE(?2, done_at_commit), updated_at = ?3 WHERE id = ?4",
            params![status.as_str(), done_at_commit, Utc::now().to_rfc3339(), todo_id],
        )?;
        Ok(())
    }

    pub fn claim_todo(&self, todo_id: &str) -> Result<Option<Todo>> {
        let mut todos = self.list_todos()?;
        let idx = todos.iter().position(|todo| todo.id == todo_id);
        let Some(idx) = idx else {
            return Ok(None);
        };

        let status = todos[idx].status;
        if status == TodoStatus::Done || status == TodoStatus::InProgress {
            return Ok(None);
        }

        todos[idx].status = TodoStatus::InProgress;
        let todo = todos[idx].clone();

        self.persist_todo_list(&todos)?;
        Ok(Some(todo))
    }

    pub fn start_run(&self, run_id: &str) -> Result<()> {
        let conn = self.conn()?;
        self.migrate(&conn)?;
        conn.execute(
            "INSERT INTO runs (run_id, status, exit_status, started_at, ended_at) VALUES (?1, ?2, NULL, ?3, NULL)",
            params![run_id, "running", Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn finish_run(&self, run_id: &str, exit_status: RunExitStatus) -> Result<()> {
        let conn = self.conn()?;
        self.migrate(&conn)?;
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
        self.migrate(&conn)?;
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
        self.migrate(&conn)?;
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
        self.migrate(&conn)?;
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
        self.migrate(&conn)?;

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
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to query events")
    }

    fn persist_todo_list(&self, todos: &[Todo]) -> Result<()> {
        let todo_file = TodoFile {
            todos: todos.iter().cloned().map(Todo::normalize).collect(),
        };
        self.save_todo_file(&todo_file)?;

        let conn = self.conn()?;
        self.migrate(&conn)?;
        for todo in todo_file.todos {
            self.upsert_todo_row(&conn, &todo)?;
        }
        Ok(())
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
        Ok(conn)
    }

    fn migrate(&self, conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
            CREATE TABLE IF NOT EXISTS runs (
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
        .context("failed to migrate sqlite schema")?;

        self.rebuild_todos_table_if_legacy(conn)?;
        self.ensure_column(conn, "todos", "done_at_commit", "TEXT")?;
        self.ensure_column(conn, "todos", "updated_at", "TEXT")?;
        self.ensure_column(conn, "events", "job_id", "TEXT")?;

        conn.execute(
            "UPDATE todos SET updated_at = ?1 WHERE updated_at IS NULL OR TRIM(updated_at) = ''",
            params![Utc::now().to_rfc3339()],
        )?;

        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .context("failed to re-enable sqlite foreign keys")?;
        Ok(())
    }

    fn ensure_column(
        &self,
        conn: &Connection,
        table: &str,
        column: &str,
        definition: &str,
    ) -> Result<()> {
        if self.table_has_column(conn, table, column)? {
            return Ok(());
        }
        let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {definition}");
        conn.execute(&sql, [])
            .with_context(|| format!("failed to add column {table}.{column}"))?;
        Ok(())
    }

    fn table_has_column(&self, conn: &Connection, table: &str, column: &str) -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn table_columns(&self, conn: &Connection, table: &str) -> Result<Vec<String>> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("failed to inspect table {table}"))
    }

    fn rebuild_todos_table_if_legacy(&self, conn: &Connection) -> Result<()> {
        let columns = self.table_columns(conn, "todos")?;
        if columns.is_empty() {
            return Ok(());
        }

        let required = [
            "id",
            "priority",
            "todo",
            "expectations",
            "test_suites",
            "status",
            "done_at_commit",
            "updated_at",
        ];
        let has_all_required = required.iter().all(|col| columns.iter().any(|c| c == col));
        let needs_rebuild = !has_all_required || columns.iter().any(|col| col == "run_id");
        if !needs_rebuild {
            return Ok(());
        }

        let src = "__chief_todos_legacy";
        conn.execute(&format!("ALTER TABLE todos RENAME TO {src}"), [])
            .context("failed to rename legacy todos table")?;
        conn.execute_batch(
            "CREATE TABLE todos (
                id TEXT PRIMARY KEY,
                priority INTEGER NOT NULL,
                todo TEXT NOT NULL,
                expectations TEXT NOT NULL,
                test_suites TEXT NOT NULL,
                status TEXT NOT NULL,
                done_at_commit TEXT,
                updated_at TEXT NOT NULL
            );",
        )
        .context("failed to create migrated todos table")?;

        let expr = |name: &str, fallback: &str| -> String {
            if columns.iter().any(|col| col == name) {
                format!("COALESCE({name}, {fallback})")
            } else {
                fallback.to_owned()
            }
        };

        let done_expr = if columns.iter().any(|col| col == "done_at_commit") {
            "done_at_commit".to_owned()
        } else {
            "NULL".to_owned()
        };

        let sql = format!(
            "INSERT INTO todos (id, priority, todo, expectations, test_suites, status, done_at_commit, updated_at)
             SELECT {id_expr}, {priority_expr}, {todo_expr}, {expectations_expr}, {suites_expr}, {status_expr}, {done_expr}, {updated_expr}
             FROM {src}",
            id_expr = expr("id", "lower(hex(randomblob(16)))"),
            priority_expr = expr("priority", "0"),
            todo_expr = expr("todo", "''"),
            expectations_expr = expr("expectations", "''"),
            suites_expr = expr("test_suites", "'[]'"),
            status_expr = expr("status", "'pending'"),
            done_expr = done_expr,
            updated_expr = expr("updated_at", "strftime('%Y-%m-%dT%H:%M:%fZ','now')"),
            src = src,
        );
        conn.execute(&sql, [])
            .context("failed to migrate legacy todos rows")?;
        conn.execute(&format!("DROP TABLE {src}"), [])
            .context("failed to drop legacy todos table")?;
        Ok(())
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

fn parse_phase(value: &str) -> Phase {
    match value {
        "todo_selection" => Phase::TodoSelection,
        "red" => Phase::Red,
        "green" => Phase::Green,
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
