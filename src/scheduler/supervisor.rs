use super::{Scheduler, WorkerResult, selector, worker};
use crate::domain::{JobStatus, RunExitStatus};
use crate::service::ChiefEngine;
use anyhow::Result;
use chrono::Utc;
use tokio::task::JoinSet;
use tokio::time::{Duration, sleep};

impl Scheduler {
    pub(super) async fn supervise_project(&self, project_name: String) -> Result<()> {
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
                    "Recovered {} stale in-progress todo(s) to pending before supervisor start",
                    recovered_in_progress
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
                selection_lock,
                merge_lock,
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
                    state.selection_lock.clone(),
                    state.merge_lock.clone(),
                )
            };

            while workers.len() < desired_agents && !stop_requested {
                let _selection_guard = selection_lock.lock().await;
                let available = context.store.list_available_todos()?;
                if available.is_empty() {
                    break;
                }

                let in_progress = context.store.list_in_progress_todos()?;
                spawn_count += 1;
                let selected_id = selector::select_todo_id(
                    &context,
                    spawn_count,
                    &available,
                    &in_progress,
                    model_override.clone(),
                )
                .await
                .unwrap_or_else(|_| available[0].id.clone());

                let Some(claimed) = context.claim_todo(&selected_id)? else {
                    continue;
                };

                let use_worktree = desired_agents > 1;
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

                workers.spawn(async move {
                    tokio::task::spawn_blocking(move || {
                        worker::run_worker(
                            worker_context,
                            worker_run_id,
                            job,
                            claimed,
                            worker_flow,
                            worker_model,
                            use_worktree,
                            worker_merge_lock,
                        )
                    })
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
                    context.log_project_event(
                        &run_id,
                        None,
                        None,
                        "info",
                        None,
                        crate::domain::EventType::Job,
                        format!("Stop requested for {project_name}; supervisor exiting"),
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
                        if result.status != "completed" {
                            any_failure = true;
                            if result.unrecoverable {
                                unrecoverable_failure = true;
                            }
                            let mut states = self.states.lock().await;
                            if let Some(state) = states.get_mut(&project_name) {
                                state.last_error = result.error.clone();
                                if result.unrecoverable {
                                    state.stop_requested = true;
                                }
                            }
                        }
                    }
                    Err(err) => {
                        any_failure = true;
                        let mut states = self.states.lock().await;
                        if let Some(state) = states.get_mut(&project_name) {
                            state.last_error = Some(format!("worker join error: {err}"));
                        }
                    }
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
        }

        Ok(())
    }
}
