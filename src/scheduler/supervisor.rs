use super::{Scheduler, StopMode, WorkerResult, worker};
use crate::domain::{JobRecord, JobStatus, RunExitStatus, Todo};
use crate::flow::FlowKind;
use crate::service::{ChiefEngine, ProjectContext};
use anyhow::Result;
use chrono::Utc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio::time::{Duration, sleep};

type WorkerRunner = Arc<dyn Fn(WorkerInvocation) -> WorkerResult + Send + Sync>;

#[derive(Clone)]
struct WorkerInvocation {
    context: ProjectContext,
    run_id: String,
    job: JobRecord,
    todo: Todo,
    flow_kind: FlowKind,
    model_override: Option<String>,
    use_worktree: bool,
    merge_lock: Arc<Mutex<()>>,
    cancel_signal: Arc<AtomicBool>,
}

fn default_worker_runner(invocation: WorkerInvocation) -> WorkerResult {
    worker::run_worker(
        invocation.context,
        invocation.run_id,
        invocation.job,
        invocation.todo,
        invocation.flow_kind,
        invocation.model_override,
        invocation.use_worktree,
        invocation.merge_lock,
        invocation.cancel_signal,
    )
}

impl Scheduler {
    pub(super) async fn supervise_project(&self, project_name: String) -> Result<()> {
        self.supervise_project_with_worker_runner(project_name, Arc::new(default_worker_runner))
            .await
    }

    async fn supervise_project_with_worker_runner(
        &self,
        project_name: String,
        worker_runner: WorkerRunner,
    ) -> Result<()> {
        let mut context = self.get_project_context(&project_name).await?;
        context.refresh()?;
        let engine = ChiefEngine::new(context.clone());

        let run_id = engine.start_run()?;
        let recovered_in_progress = context.store.reset_in_progress_todos_to_pending()?;
        if recovered_in_progress > 0 {
            context.log_project_event(
                &run_id,
                None,
                None,
                "info",
                None,
                crate::domain::EventType::Job,
                format!(
                    "Recovered {recovered_in_progress} stale in-progress todo(s) to pending before supervisor start"
                ),
                std::collections::BTreeMap::new(),
            )?;
        }
        context.log_project_event(
            &run_id,
            None,
            None,
            "info",
            None,
            crate::domain::EventType::Job,
            format!("Starting project supervisor for {project_name}"),
            std::collections::BTreeMap::new(),
        )?;

        let mut workers: JoinSet<WorkerResult> = JoinSet::new();
        let mut spawn_count = 0usize;
        let mut any_failure = false;
        let mut unrecoverable_failure = false;

        loop {
            let (
                desired_agents,
                flow_kind,
                model_override,
                stop_requested,
                stop_mode,
                selection_lock,
                merge_lock,
                cancel_signal,
            ) = {
                let states = self.states.lock().await;
                let Some(state) = states.get(&project_name) else {
                    break;
                };
                (
                    state.desired_agents,
                    state.flow_kind,
                    state.model_override.clone(),
                    state.stop_requested,
                    state.stop_mode,
                    state.selection_lock.clone(),
                    state.merge_lock.clone(),
                    state.cancel_signal.clone(),
                )
            };

            while workers.len() < desired_agents && !stop_requested {
                let _selection_guard = selection_lock.lock().await;
                spawn_count += 1;
                let Some(claimed) = context.claim_next_pending_todo()? else {
                    break;
                };

                let use_worktree = true;
                let mut job = context.create_job(
                    &run_id,
                    spawn_count,
                    flow_kind,
                    Some(claimed.id.clone()),
                    None,
                )?;
                job = context.set_job_status(job, JobStatus::Selecting, None)?;

                let worker_context = context.clone();
                let worker_run_id = run_id.clone();
                let worker_flow = flow_kind;
                let worker_model = model_override.clone();
                let worker_merge_lock = merge_lock.clone();
                let worker_cancel_signal = cancel_signal.clone();
                let worker_runner = worker_runner.clone();
                let worker_invocation = WorkerInvocation {
                    context: worker_context,
                    run_id: worker_run_id,
                    job,
                    todo: claimed,
                    flow_kind: worker_flow,
                    model_override: worker_model,
                    use_worktree,
                    merge_lock: worker_merge_lock,
                    cancel_signal: worker_cancel_signal,
                };

                workers.spawn(async move {
                    tokio::task::spawn_blocking(move || (worker_runner)(worker_invocation))
                        .await
                        .unwrap_or_else(|join_err| WorkerResult {
                            job_id: format!("join-error-{}", Utc::now().timestamp_millis()),
                            todo_id: "unknown".to_owned(),
                            status: "failed".to_owned(),
                            error: Some(format!("worker task panicked: {join_err}")),
                            commit_hash: None,
                            unrecoverable: false,
                        })
                });
            }

            {
                let mut states = self.states.lock().await;
                if let Some(state) = states.get_mut(&project_name) {
                    state.active_workers = workers.len();
                }
            }

            if workers.is_empty() {
                let no_more_work = context.store.list_available_todos()?.is_empty();
                if stop_requested {
                    let stop_message = if stop_mode == StopMode::Pause {
                        format!("Pause requested for {project_name}; active work drained")
                    } else {
                        format!("Stop requested for {project_name}; cancellation complete")
                    };
                    context.log_project_event(
                        &run_id,
                        None,
                        None,
                        "info",
                        None,
                        crate::domain::EventType::Job,
                        stop_message,
                        std::collections::BTreeMap::new(),
                    )?;
                    break;
                }
                if no_more_work {
                    context.log_project_event(
                        &run_id,
                        None,
                        None,
                        "info",
                        None,
                        crate::domain::EventType::Job,
                        format!("No available todos for {project_name}; supervisor exiting"),
                        std::collections::BTreeMap::new(),
                    )?;
                    break;
                }
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            if let Some(joined) = workers.join_next().await {
                match joined {
                    Ok(result) => {
                        if result.status == "cancelled" {
                            continue;
                        }
                        if result.status != "completed" {
                            any_failure = true;
                            if result.unrecoverable {
                                unrecoverable_failure = true;
                            }
                            let mut states = self.states.lock().await;
                            if let Some(state) = states.get_mut(&project_name) {
                                state.last_error = result.error.clone();
                                state.stop_requested = true;
                                state.stop_mode = StopMode::Stop;
                                state.cancel_signal.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                    Err(err) => {
                        any_failure = true;
                        let mut states = self.states.lock().await;
                        if let Some(state) = states.get_mut(&project_name) {
                            state.last_error = Some(format!("worker join error: {err}"));
                            state.stop_requested = true;
                            state.stop_mode = StopMode::Stop;
                            state.cancel_signal.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }

            if cancel_signal.load(Ordering::SeqCst) {
                let mut states = self.states.lock().await;
                if let Some(state) = states.get_mut(&project_name) {
                    state.stop_requested = true;
                    state.stop_mode = StopMode::Stop;
                }
            }
        }

        engine.finish_run(
            &run_id,
            if any_failure {
                if unrecoverable_failure {
                    RunExitStatus::UnrecoverableFailure
                } else {
                    RunExitStatus::Failure
                }
            } else {
                RunExitStatus::Success
            },
        )?;

        context.log_project_event(
            &run_id,
            None,
            None,
            "info",
            None,
            crate::domain::EventType::Job,
            format!("Supervisor completed for {project_name}"),
            std::collections::BTreeMap::new(),
        )?;

        let mut states = self.states.lock().await;
        if let Some(state) = states.get_mut(&project_name) {
            state.running = false;
            state.active_workers = 0;
            state.stop_requested = false;
            state.stop_mode = StopMode::None;
            state.cancel_signal.store(false, Ordering::SeqCst);
        }

        Ok(())
    }
}

#[cfg(test)]
#[path = "supervisor/tests.rs"]
mod tests;
