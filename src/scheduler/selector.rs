use crate::agent::AgentRequest;
use crate::domain::Todo;
use crate::prompt::PromptStore;
use crate::service::ProjectContext;
use anyhow::{Context, Result, anyhow};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub(super) async fn select_todo_id(
    context: &ProjectContext,
    worker_index: usize,
    available: &[Todo],
    in_progress: &[Todo],
    model_override: Option<String>,
    cancel_signal: Arc<AtomicBool>,
) -> Result<String> {
    if worker_index <= 1 || in_progress.is_empty() {
        return highest_priority_todo_id(available).ok_or_else(|| anyhow!("no available todo"));
    }

    let prompt = context.prompts.render_json(
        "todo_select.md",
        &serde_json::json!({
            "worker_index": worker_index,
            "available_todos": available,
            "in_progress_todos": in_progress,
        }),
    )?;

    let agent = context.build_agent(model_override);
    let timeout_seconds = context.chief_toml.chief.agent_timeout_seconds;
    let response = tokio::task::spawn_blocking({
        let project_dir = context.project_dir.clone();
        move || {
            agent.run(AgentRequest {
                prompt,
                cwd: project_dir,
                timeout_seconds: Some(timeout_seconds),
                disallowed_paths: Vec::new(),
                cancel_signal: Some(cancel_signal),
            })
        }
    })
    .await
    .context("todo selector join error")??;

    if response.exit_code != 0 {
        return highest_priority_todo_id(available)
            .ok_or_else(|| anyhow!("todo selector failed and no todo available"));
    }

    let selected_id = response
        .merged_output
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();

    if available.iter().any(|todo| todo.id == selected_id) {
        Ok(selected_id)
    } else {
        highest_priority_todo_id(available).ok_or_else(|| anyhow!("no available todo"))
    }
}

fn highest_priority_todo_id(todos: &[Todo]) -> Option<String> {
    todos
        .iter()
        .max_by(|a, b| a.priority.cmp(&b.priority).then_with(|| b.id.cmp(&a.id)))
        .map(|todo| todo.id.clone())
}
