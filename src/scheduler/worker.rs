use super::WorkerResult;
use crate::domain::{EventType, JobRecord, JobStatus, Todo, TodoStatus};
use crate::flow::FlowKind;
use crate::git::GitOps;
use crate::service::{ChiefEngine, ProjectContext};
use std::fs;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::warn;

pub(super) fn run_worker(
    context: ProjectContext,
    run_id: String,
    mut job: JobRecord,
    todo: Todo,
    flow_kind: FlowKind,
    model_override: Option<String>,
    use_worktree: bool,
    merge_lock: Arc<Mutex<()>>,
) -> WorkerResult {
    let engine = ChiefEngine::new(context.clone());

    let update_job = |ctx: &ProjectContext,
                      current: &mut JobRecord,
                      status: JobStatus,
                      error: Option<String>| {
        if let Ok(updated) = ctx.set_job_status(current.clone(), status, error) {
            *current = updated;
        }
    };

    update_job(&context, &mut job, JobStatus::Running, None);

    let main_branch = context
        .git
        .current_branch()
        .unwrap_or_else(|_| "main".to_owned());

    let mut work_dir = context.project_dir.clone();
    let mut branch_name = None::<String>;

    if use_worktree {
        let worktree_root = context.project_dir.join(".chief-worktrees");
        if let Err(err) = fs::create_dir_all(&worktree_root) {
            update_job(
                &context,
                &mut job,
                JobStatus::Failed,
                Some(format!("failed to create worktree root: {err}")),
            );
            if let Err(status_err) =
                context
                    .store
                    .update_todo_status(&todo.id, TodoStatus::Attempted, None)
            {
                report_state_update_error(
                    &context,
                    &run_id,
                    Some(&job.id),
                    Some(&todo.id),
                    "failed to mark todo attempted after worktree-root failure",
                    &status_err,
                );
            }
            return WorkerResult {
                job_id: job.id,
                todo_id: todo.id,
                status: "failed".to_owned(),
                error: Some(err.to_string()),
                commit_hash: None,
            };
        }

        let branch = format!("chief/{}/{}", context.name, job.id);
        let worktree_path = worktree_root.join(&job.id);

        if let Err(err) = context.git.create_worktree(&branch, &worktree_path) {
            update_job(
                &context,
                &mut job,
                JobStatus::Failed,
                Some(format!("failed to create worktree: {err}")),
            );
            if let Err(status_err) =
                context
                    .store
                    .update_todo_status(&todo.id, TodoStatus::Attempted, None)
            {
                report_state_update_error(
                    &context,
                    &run_id,
                    Some(&job.id),
                    Some(&todo.id),
                    "failed to mark todo attempted after worktree creation failure",
                    &status_err,
                );
            }
            return WorkerResult {
                job_id: job.id,
                todo_id: todo.id,
                status: "failed".to_owned(),
                error: Some(err.to_string()),
                commit_hash: None,
            };
        }

        work_dir = worktree_path.clone();
        branch_name = Some(branch);

        let mut updated_job = job.clone();
        updated_job.worktree_path = Some(worktree_path.display().to_string());
        if let Err(err) = context.store.upsert_job(&updated_job) {
            report_state_update_error(
                &context,
                &run_id,
                Some(&job.id),
                Some(&todo.id),
                "failed to persist worker worktree path",
                &err,
            );
        }
        job = updated_job;
    }

    let todo_id = todo.id.clone();
    let outcome = engine.run_single_todo(
        &run_id,
        &job.id,
        job.worker_index,
        todo,
        flow_kind,
        work_dir.clone(),
        model_override,
    );

    let result = match outcome {
        Ok(outcome) => {
            let mut merge_error = None;

            if let Some(branch) = &branch_name {
                let merge_guard = merge_lock.blocking_lock();
                let merge_result = context
                    .git
                    .merge_branch_into_main(branch, &main_branch)
                    .and_then(|_| context.git.remove_worktree(&work_dir, branch));
                drop(merge_guard);

                if let Err(err) = merge_result {
                    merge_error = Some(err.to_string());
                }
            }

            if let Some(err) = merge_error {
                if let Err(status_err) =
                    context
                        .store
                        .update_todo_status(&todo_id, TodoStatus::Attempted, None)
                {
                    report_state_update_error(
                        &context,
                        &run_id,
                        Some(&job.id),
                        Some(&todo_id),
                        "failed to mark todo attempted after merge error",
                        &status_err,
                    );
                }
                update_job(&context, &mut job, JobStatus::Failed, Some(err.clone()));
                WorkerResult {
                    job_id: job.id,
                    todo_id,
                    status: "failed".to_owned(),
                    error: Some(err),
                    commit_hash: outcome.commit_hash,
                }
            } else {
                if let Err(status_err) = context.store.update_todo_status(
                    &todo_id,
                    TodoStatus::Done,
                    outcome.commit_hash.as_deref(),
                ) {
                    report_state_update_error(
                        &context,
                        &run_id,
                        Some(&job.id),
                        Some(&todo_id),
                        "failed to mark todo done",
                        &status_err,
                    );
                }
                update_job(&context, &mut job, JobStatus::Completed, None);
                WorkerResult {
                    job_id: job.id,
                    todo_id,
                    status: "completed".to_owned(),
                    error: None,
                    commit_hash: outcome.commit_hash,
                }
            }
        }
        Err(err) => {
            if let Err(status_err) =
                context
                    .store
                    .update_todo_status(&todo_id, TodoStatus::Attempted, None)
            {
                report_state_update_error(
                    &context,
                    &run_id,
                    Some(&job.id),
                    Some(&todo_id),
                    "failed to mark todo attempted after worker failure",
                    &status_err,
                );
            }
            if let Some(branch) = &branch_name {
                if let Err(remove_err) = context.git.remove_worktree(&work_dir, branch) {
                    report_state_update_error(
                        &context,
                        &run_id,
                        Some(&job.id),
                        Some(&todo_id),
                        "failed to cleanup worker worktree",
                        &remove_err,
                    );
                }
            }
            update_job(&context, &mut job, JobStatus::Failed, Some(err.to_string()));
            WorkerResult {
                job_id: job.id,
                todo_id,
                status: "failed".to_owned(),
                error: Some(err.to_string()),
                commit_hash: None,
            }
        }
    };

    result
}

fn report_state_update_error(
    context: &ProjectContext,
    run_id: &str,
    job_id: Option<&str>,
    todo_id: Option<&str>,
    msg: &str,
    err: &anyhow::Error,
) {
    warn!("{msg}: {err:#}");
    let mut payload = std::collections::BTreeMap::new();
    payload.insert(
        "error".to_owned(),
        serde_json::Value::String(err.to_string()),
    );
    if let Err(log_err) = context.log_project_event(
        run_id,
        job_id.map(str::to_owned),
        todo_id.map(str::to_owned),
        "warning",
        None,
        EventType::Error,
        msg.to_owned(),
        payload,
    ) {
        warn!("failed to record state-update error event: {log_err:#}");
    }
}
