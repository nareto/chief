use super::WorkerResult;
use crate::agent::is_agent_cancelled_error;
use crate::domain::{EventType, JobRecord, JobStatus, Todo, TodoStatus};
use crate::flow::FlowKind;
use crate::git::GitOps;
use crate::orchestrator::OrchestratorError;
use crate::service::{ChiefEngine, ProjectContext};
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    cancel_signal: Arc<AtomicBool>,
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

    if cancel_signal.load(Ordering::SeqCst) {
        let _ = context
            .store
            .update_todo_status(&todo.id, TodoStatus::Pending, None);
        update_job(&context, &mut job, JobStatus::Cancelled, None);
        return WorkerResult {
            job_id: job.id,
            todo_id: todo.id,
            status: "cancelled".to_owned(),
            error: Some("cancelled by stop request".to_owned()),
            commit_hash: None,
            unrecoverable: false,
        };
    }

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
                unrecoverable: true,
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
                unrecoverable: true,
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
    let max_retries = context.chief_toml.chief.max_retries.max(1);
    let outcome = engine.run_single_todo_with_retries(
        &run_id,
        &job.id,
        job.worker_index,
        todo,
        flow_kind,
        work_dir.clone(),
        model_override,
        cancel_signal.clone(),
        max_retries,
        |attempt, total, err| {
            let msg = format!(
                "worker todo execution failed ({attempt}/{total}), retrying non-deterministic loop"
            );
            report_state_update_error(&context, &run_id, Some(&job.id), Some(&todo_id), &msg, err);
        },
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
                    unrecoverable: false,
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
                    unrecoverable: false,
                }
            }
        }
        Err(err) => {
            let cancelled =
                cancel_signal.load(Ordering::SeqCst) || is_agent_cancelled_error(err.as_error());
            if cancelled {
                if let Err(status_err) =
                    context
                        .store
                        .update_todo_status(&todo_id, TodoStatus::Pending, None)
                {
                    report_state_update_error(
                        &context,
                        &run_id,
                        Some(&job.id),
                        Some(&todo_id),
                        "failed to mark todo pending after cancellation",
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
                            "failed to cleanup worker worktree after cancellation",
                            &remove_err,
                        );
                    }
                }
                update_job(&context, &mut job, JobStatus::Cancelled, None);
                return WorkerResult {
                    job_id: job.id,
                    todo_id,
                    status: "cancelled".to_owned(),
                    error: Some("cancelled by stop request".to_owned()),
                    commit_hash: None,
                    unrecoverable: false,
                };
            }

            let unrecoverable = matches!(err, OrchestratorError::Unrecoverable(_));
            let err_string = err.as_error().to_string();
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
            update_job(
                &context,
                &mut job,
                JobStatus::Failed,
                Some(err_string.clone()),
            );
            WorkerResult {
                job_id: job.id,
                todo_id,
                status: "failed".to_owned(),
                error: Some(err_string),
                commit_hash: None,
                unrecoverable,
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
