use super::WorkerResult;
use crate::agent::is_agent_cancelled_error;
use crate::domain::{EventType, JobRecord, JobStatus, Todo, TodoStatus};
use crate::flow::{FlowKind, TodoOutcome};
use crate::git::GitOps;
use crate::orchestrator::{OrchestratorError, OrchestratorResult};
use crate::service::{ChiefEngine, ProjectContext};
use crate::worktree_cache;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;
use tracing::warn;

pub(super) fn run_worker(
    context: ProjectContext,
    run_id: String,
    job: JobRecord,
    todo: Todo,
    flow_kind: FlowKind,
    model_override: Option<String>,
    use_worktree: bool,
    merge_lock: Arc<Mutex<()>>,
    cancel_signal: Arc<AtomicBool>,
) -> WorkerResult {
    run_worker_with_executor(
        context,
        run_id,
        job,
        todo,
        flow_kind,
        model_override,
        use_worktree,
        merge_lock,
        cancel_signal,
        |context, run_id, job, todo, flow_kind, work_dir, model_override, cancel_signal| {
            let engine = ChiefEngine::new(context.clone());
            let max_retries = context.chief_yaml.chief.max_retries.max(1);
            engine.run_single_todo_with_retries(
                run_id,
                &job.id,
                job.worker_index,
                todo,
                flow_kind,
                work_dir,
                model_override,
                cancel_signal,
                max_retries,
                |_attempt, _total, _err| {},
            )
        },
    )
}

fn run_worker_with_executor<F>(
    context: ProjectContext,
    run_id: String,
    mut job: JobRecord,
    todo: Todo,
    flow_kind: FlowKind,
    model_override: Option<String>,
    use_worktree: bool,
    merge_lock: Arc<Mutex<()>>,
    cancel_signal: Arc<AtomicBool>,
    mut execute_todo: F,
) -> WorkerResult
where
    F: FnMut(
        &ProjectContext,
        &str,
        &JobRecord,
        Todo,
        FlowKind,
        PathBuf,
        Option<String>,
        Arc<AtomicBool>,
    ) -> OrchestratorResult<TodoOutcome>,
{
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
        let worktree_root = worktree_root_for_project(&context.project_dir, &context.name);
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
                    .update_todo_status(&todo.id, TodoStatus::Pending, None)
            {
                report_state_update_error(
                    &context,
                    &run_id,
                    Some(&job.id),
                    Some(&todo.id),
                    "failed to mark todo pending after worktree-root failure",
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
        let worktree_path = worktree_root.join(worker_worktree_dir_name(&job.id));

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
                    .update_todo_status(&todo.id, TodoStatus::Pending, None)
            {
                report_state_update_error(
                    &context,
                    &run_id,
                    Some(&job.id),
                    Some(&todo.id),
                    "failed to mark todo pending after worktree creation failure",
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

        match worktree_cache::file_content_md5(&context.config_path).and_then(|chief_yaml_hash| {
            worktree_cache::hydrate_suite_caches_into_worktree(
                &context.project_dir,
                &context.name,
                &context.chief_yaml.suites,
                &worktree_path,
                &chief_yaml_hash,
            )
        }) {
            Ok(cache_report) => {
                if cache_report.linked_paths > 0 {
                    let mut payload = std::collections::BTreeMap::new();
                    payload.insert(
                        "linked_paths".to_owned(),
                        serde_json::Value::from(cache_report.linked_paths as u64),
                    );
                    payload.insert(
                        "skipped_existing_paths".to_owned(),
                        serde_json::Value::from(cache_report.skipped_existing_paths as u64),
                    );
                    payload.insert(
                        "missing_cache_paths".to_owned(),
                        serde_json::Value::from(cache_report.missing_cache_paths as u64),
                    );
                    payload.insert(
                        "suites_considered".to_owned(),
                        serde_json::Value::from(cache_report.suites_considered as u64),
                    );
                    payload.insert(
                        "invalid_paths".to_owned(),
                        serde_json::Value::from(cache_report.invalid_paths as u64),
                    );
                    let _ = context.log_project_event(
                        &run_id,
                        Some(job.id.clone()),
                        Some(todo.id.clone()),
                        "info",
                        None,
                        EventType::Msg,
                        "Hydrated suite dependency cache into worker worktree",
                        payload,
                    );
                }
            }
            Err(err) => {
                report_state_update_error(
                    &context,
                    &run_id,
                    Some(&job.id),
                    Some(&todo.id),
                    "failed to hydrate suite dependency cache into worker worktree",
                    &err,
                );
            }
        }

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
    let outcome = execute_todo(
        &context,
        &run_id,
        &job,
        todo,
        flow_kind,
        work_dir.clone(),
        model_override,
        cancel_signal.clone(),
    );

    match outcome {
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
                        .update_todo_status(&todo_id, TodoStatus::Pending, None)
                {
                    report_state_update_error(
                        &context,
                        &run_id,
                        Some(&job.id),
                        Some(&todo_id),
                        "failed to mark todo pending after merge error",
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
                if let Some(branch) = &branch_name
                    && let Err(remove_err) = context.git.remove_worktree(&work_dir, branch)
                {
                    report_state_update_error(
                        &context,
                        &run_id,
                        Some(&job.id),
                        Some(&todo_id),
                        "failed to cleanup worker worktree after cancellation",
                        &remove_err,
                    );
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
                    .update_todo_status(&todo_id, TodoStatus::Pending, None)
            {
                report_state_update_error(
                    &context,
                    &run_id,
                    Some(&job.id),
                    Some(&todo_id),
                    "failed to mark todo pending after worker failure",
                    &status_err,
                );
            }
            if let Some(branch) = &branch_name
                && let Err(remove_err) = context.git.remove_worktree(&work_dir, branch)
            {
                report_state_update_error(
                    &context,
                    &run_id,
                    Some(&job.id),
                    Some(&todo_id),
                    "failed to cleanup worker worktree",
                    &remove_err,
                );
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
    }
}

fn worktree_root_for_project(project_dir: &Path, project_name: &str) -> PathBuf {
    let parent_dir = project_dir.parent().unwrap_or(project_dir);
    parent_dir.join(format!("{project_name}__worktrees"))
}

fn worker_worktree_dir_name(job_id: &str) -> String {
    format!("chief_{job_id}")
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

#[cfg(test)]
#[path = "worker/tests.rs"]
mod tests;
