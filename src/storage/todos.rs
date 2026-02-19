use super::parsing::parse_todo_row;
use super::*;
use crate::domain::{Todo, TodoFile, TodoStatus};
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;

impl ProjectStore {
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
        if changed > 0 {
            self.auto_commit_todos_yaml()?;
        }
        Ok(changed)
    }

    pub fn load_todo_file(&self) -> Result<TodoFile> {
        let content = fs::read_to_string(&self.todos_path)
            .with_context(|| format!("failed to read {}", self.todos_path.display()))?;
        if content.trim().is_empty() {
            return Ok(TodoFile::default());
        }
        let parsed: TodoFile = serde_yaml::from_str(&content)
            .with_context(|| format!("invalid YAML in {}", self.todos_path.display()))?;
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
        let body = serde_yaml::to_string(&normalized)?;
        fs::write(&self.todos_path, format!("{body}\n"))
            .with_context(|| format!("failed to write {}", self.todos_path.display()))?;
        Ok(())
    }

    pub fn sync_todos_from_file(&self) -> Result<()> {
        let todos = self.load_todo_file()?.todos;
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let todo_ids = todos
            .iter()
            .map(|todo| todo.id.as_str())
            .collect::<Vec<_>>();

        for todo in &todos {
            self.upsert_todo_row(&tx, todo)?;
        }

        if todo_ids.is_empty() {
            tx.execute("DELETE FROM todos", [])?;
        } else {
            let placeholders = std::iter::repeat_n("?", todo_ids.len())
                .collect::<Vec<_>>()
                .join(", ");
            let delete_sql = format!("DELETE FROM todos WHERE id NOT IN ({placeholders})");
            tx.execute(
                &delete_sql,
                rusqlite::params_from_iter(todo_ids.iter().copied()),
            )?;
        }

        tx.commit()?;
        self.auto_commit_todos_yaml()?;
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
            .filter(|todo| todo.status == TodoStatus::Pending)
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
            return Err(anyhow!("todo '{todo_id}' not found"));
        }
        self.sync_todos_file_from_conn(&tx)?;
        tx.commit()?;
        self.auto_commit_todos_yaml()?;
        Ok(())
    }

    pub fn append_todo(&self, todo: Todo) -> Result<Todo> {
        let normalized = todo.normalize();
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        self.upsert_todo_row(&tx, &normalized)?;
        self.sync_todos_file_from_conn(&tx)?;
        tx.commit()?;
        self.auto_commit_todos_yaml()?;
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
            return Err(anyhow!("todo '{existing_id}' not found"));
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
        self.auto_commit_todos_yaml()?;
        Ok(next)
    }

    pub fn delete_todo(&self, todo_id: &str) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;

        let changed = tx.execute("DELETE FROM todos WHERE id = ?1", params![todo_id])?;
        if changed == 0 {
            return Err(anyhow!("todo '{todo_id}' not found"));
        }

        self.sync_todos_file_from_conn(&tx)?;
        tx.commit()?;
        self.auto_commit_todos_yaml()?;
        Ok(())
    }

    pub fn delete_done_todos(&self) -> Result<usize> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;

        let deleted = tx.execute(
            "DELETE FROM todos WHERE status = ?1",
            params![TodoStatus::Done.as_str()],
        )?;
        self.sync_todos_file_from_conn(&tx)?;
        tx.commit()?;
        self.auto_commit_todos_yaml()?;
        Ok(deleted)
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
        self.auto_commit_todos_yaml()?;
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

    pub(super) fn sync_todos_file_from_conn(&self, conn: &Connection) -> Result<()> {
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
}
