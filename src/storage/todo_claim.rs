use super::parsing::parse_todo_row;
use super::*;
use crate::domain::{Todo, TodoStatus};
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};

impl ProjectStore {
    pub fn claim_next_pending_todo(&self) -> Result<Option<Todo>> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        self.normalize_legacy_attempted_todos_to_pending(&tx)?;

        let mut stmt = tx.prepare(
            "SELECT id, priority, todo, expectations, test_suites, status, done_at_commit
             FROM todos
             WHERE status = ?1
             ORDER BY priority DESC, id ASC
             LIMIT 1",
        )?;

        let mut claimed = stmt
            .query_row(params![TodoStatus::Pending.as_str()], parse_todo_row)
            .optional()
            .context("failed to fetch next pending todo for claim")?;
        drop(stmt);

        if let Some(todo) = claimed.as_mut() {
            let changed = tx.execute(
                "UPDATE todos
                 SET status = ?1, updated_at = ?2
                 WHERE id = ?3 AND status = ?4",
                params![
                    TodoStatus::InProgress.as_str(),
                    Utc::now().to_rfc3339(),
                    &todo.id,
                    TodoStatus::Pending.as_str(),
                ],
            )?;
            if changed == 0 {
                claimed = None;
            } else {
                todo.status = TodoStatus::InProgress;
            }
        }

        tx.commit()?;
        Ok(claimed)
    }

    pub fn claim_todo(&self, todo_id: &str) -> Result<Option<Todo>> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        self.normalize_legacy_attempted_todos_to_pending(&tx)?;

        let mut stmt = tx.prepare(
            "SELECT id, priority, todo, expectations, test_suites, status, done_at_commit
             FROM todos
             WHERE id = ?1
             LIMIT 1",
        )?;

        let mut claimed = stmt
            .query_row(params![todo_id], parse_todo_row)
            .optional()
            .context("failed to fetch todo for claim")?;
        drop(stmt);

        if let Some(todo) = claimed.as_mut() {
            if todo.status == TodoStatus::Pending {
                let changed = tx.execute(
                    "UPDATE todos
                     SET status = ?1, updated_at = ?2
                     WHERE id = ?3 AND status = ?4",
                    params![
                        TodoStatus::InProgress.as_str(),
                        Utc::now().to_rfc3339(),
                        todo_id,
                        TodoStatus::Pending.as_str(),
                    ],
                )?;
                if changed == 0 {
                    claimed = None;
                } else {
                    todo.status = TodoStatus::InProgress;
                }
            } else {
                claimed = None;
            }
        }

        tx.commit()?;
        Ok(claimed)
    }

    fn normalize_legacy_attempted_todos_to_pending(&self, conn: &Connection) -> Result<usize> {
        conn.execute(
            "UPDATE todos
             SET status = ?1, updated_at = ?2
             WHERE status = 'attempted'",
            params![TodoStatus::Pending.as_str(), Utc::now().to_rfc3339()],
        )
        .context("failed to normalize legacy attempted todos to pending")
    }
}
