use super::*;

impl ApiService {
    pub async fn get_todos(&self, project: &str) -> Result<TodosResponse, ApiError> {
        let mut context = self.project_context(project).await?;
        context.refresh().map_err(ApiError::internal)?;
        let todos = context.store.list_todos().map_err(ApiError::internal)?;
        Ok(TodosResponse { todos })
    }

    pub async fn add_todo(
        &self,
        project: &str,
        payload: AddTodoRequest,
    ) -> Result<TodoResponse, ApiError> {
        let context = self.project_context(project).await?;
        let todo = Todo {
            id: String::new(),
            todo: payload.todo,
            expectations: payload.expectations.unwrap_or_default(),
            priority: payload.priority.unwrap_or(0),
            test_suites: payload.test_suites.unwrap_or_default(),
            status: TodoStatus::Pending,
            done_at_commit: None,
        }
        .normalize();
        let todo = context
            .store
            .append_todo(todo)
            .map_err(ApiError::internal)?;
        Ok(TodoResponse { todo })
    }

    pub async fn update_todo(
        &self,
        project: &str,
        todo_id: &str,
        payload: UpdateTodoRequest,
    ) -> Result<TodoResponse, ApiError> {
        let context = self.project_context(project).await?;
        let current = context
            .store
            .list_todos()
            .map_err(ApiError::internal)?
            .into_iter()
            .find(|todo| todo.id == todo_id)
            .ok_or_else(|| ApiError::not_found(format!("todo '{todo_id}' not found")))?;

        let status = match payload.status {
            Some(raw) => parse_todo_status_input(&raw)
                .ok_or_else(|| ApiError::unprocessable(format!("invalid todo status '{raw}'")))?,
            None => current.status,
        };

        let done_at_commit = match payload.done_at_commit {
            Some(Some(raw)) => {
                let value = raw.trim();
                if value.is_empty() {
                    None
                } else {
                    Some(value.to_owned())
                }
            }
            Some(None) => None,
            None => current.done_at_commit.clone(),
        };

        let todo = Todo {
            id: payload.id.unwrap_or(current.id),
            todo: payload.todo.unwrap_or(current.todo),
            expectations: payload.expectations.unwrap_or(current.expectations),
            priority: payload.priority.unwrap_or(current.priority),
            test_suites: payload.test_suites.unwrap_or(current.test_suites),
            status,
            done_at_commit,
        }
        .normalize();

        if todo.todo.trim().is_empty() {
            return Err(ApiError::unprocessable("todo text cannot be empty"));
        }

        let updated = context
            .store
            .update_todo(todo_id, todo)
            .map_err(ApiError::classify_store_error)?;

        Ok(TodoResponse { todo: updated })
    }

    pub async fn delete_todo(
        &self,
        project: &str,
        todo_id: &str,
    ) -> Result<MessageResponse, ApiError> {
        let context = self.project_context(project).await?;

        context
            .store
            .delete_todo(todo_id)
            .map_err(ApiError::classify_store_error)?;

        Ok(MessageResponse {
            message: format!("deleted todo '{todo_id}'"),
        })
    }

    pub async fn delete_done_todos(&self, project: &str) -> Result<MessageResponse, ApiError> {
        let context = self.project_context(project).await?;
        let deleted = context
            .store
            .delete_done_todos()
            .map_err(ApiError::internal)?;

        Ok(MessageResponse {
            message: format!("deleted {deleted} done todo(s)"),
        })
    }
}
