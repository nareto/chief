use super::parsing::parse_todo_row;
use super::*;
use crate::domain::{Todo, TodoStatus};
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};

impl ProjectStore {
    pub fn reset_in_progress_todos_to_pending(&self) -> Result<usize> {
        if !self.sqlite_log_enabled() {
            return Ok(0);
        }
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
        tx.commit()?;
        Ok(changed)
    }

    pub fn replace_todos(&self, todos: Vec<Todo>) -> Result<()> {
        if !self.sqlite_log_enabled() {
            return Ok(());
        }
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
        Ok(())
    }

    pub fn list_todos(&self) -> Result<Vec<Todo>> {
        if !self.sqlite_log_enabled() {
            return Ok(Vec::new());
        }
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
        if !self.sqlite_log_enabled() {
            return Ok(());
        }
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
        tx.commit()?;
        Ok(())
    }

    pub fn append_todo(&self, todo: Todo) -> Result<Todo> {
        let normalized = todo.normalize();
        if !self.sqlite_log_enabled() {
            return Ok(normalized);
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        self.upsert_todo_row(&tx, &normalized)?;
        tx.commit()?;
        Ok(normalized)
    }

    pub fn update_todo(&self, existing_id: &str, todo: Todo) -> Result<Todo> {
        if !self.sqlite_log_enabled() {
            return Ok(todo.normalize());
        }
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
        tx.commit()?;
        Ok(next)
    }

    pub fn delete_todo(&self, todo_id: &str) -> Result<()> {
        if !self.sqlite_log_enabled() {
            return Ok(());
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;

        let changed = tx.execute("DELETE FROM todos WHERE id = ?1", params![todo_id])?;
        if changed == 0 {
            return Err(anyhow!("todo '{todo_id}' not found"));
        }

        tx.commit()?;
        Ok(())
    }

    pub fn delete_done_todos(&self) -> Result<usize> {
        if !self.sqlite_log_enabled() {
            return Ok(0);
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;

        let deleted = tx.execute(
            "DELETE FROM todos WHERE status = ?1",
            params![TodoStatus::Done.as_str()],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    pub fn clean_completed_todos_with_commit(&self) -> Result<usize> {
        if !self.sqlite_log_enabled() {
            return Ok(0);
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let deleted = tx.execute(
            "DELETE FROM todos WHERE status = ?1 AND done_at_commit IS NOT NULL",
            params![TodoStatus::Done.as_str()],
        )?;
        tx.commit()?;
        Ok(deleted)
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
