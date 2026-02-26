use super::super::{worker_worktree_dir_name, worktree_root_for_project};
use super::ChiefEngine;
use crate::domain::{EventType, JobStatus, RunExitStatus, TodoStatus, payload_from_json};
use crate::flow::{FlowKind, TodoOutcome};
use crate::git::GitOps;
use crate::orchestrator::{OrchestratorError, OrchestratorResult};
use crate::worktree_cache;
use anyhow::{Context, Result};
use std::fs;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

impl ChiefEngine {
    fn run_next_todo_in_run_with_retry_hook<FR>(
        &self,
        run_id: &str,
        flow_kind: FlowKind,
        model_override: Option<String>,
        max_retries: usize,
        on_retry: &mut FR,
    ) -> OrchestratorResult<Option<TodoOutcome>>
    where
        FR: FnMut(usize, usize, &anyhow::Error),
    {
        let Some(todo) = self
            .project
            .claim_next_pending_todo()
            .map_err(|err| self.classify_runtime_error(err))?
        else {
            return Ok(None);
        };

        let mut job = self
            .project
            .create_job(run_id, 1, flow_kind, Some(todo.id.clone()), None)
            .map_err(|err| self.classify_runtime_error(err))?;
        job = self
            .project
            .set_job_status(job, JobStatus::Running, None)
            .context("failed to set job status running")
            .map_err(|err| self.classify_runtime_error(err))?;

        let main_branch = self
            .project
            .git
            .current_branch()
            .unwrap_or_else(|_| "main".to_owned());
        let worktree_root =
            worktree_root_for_project(&self.project.project_dir, &self.project.name);
        fs::create_dir_all(&worktree_root)
            .context("failed to create worktree root directory")
            .map_err(|err| self.classify_runtime_error(err))?;
        let branch = format!("chief/{}/{}", self.project.name, job.id);
        let work_dir = worktree_root.join(worker_worktree_dir_name(&job.id));
        self.project
            .git
            .create_worktree(&branch, &work_dir)
            .context("failed to create worker worktree")
            .map_err(|err| self.classify_runtime_error(err))?;

        match worktree_cache::file_content_md5(&self.project.config_path).and_then(
            |chief_yaml_hash| {
                worktree_cache::hydrate_suite_caches_into_worktree(
                    &self.project.project_dir,
                    &self.project.name,
                    &self.project.chief_yaml.suites,
                    &work_dir,
                    &chief_yaml_hash,
                )
            },
        ) {
            Ok(cache_report) => {
                if cache_report.linked_paths > 0 {
                    self.log_runtime_event(
                        run_id,
                        Some(&job.id),
                        Some(&todo.id),
                        "info",
                        None,
                        EventType::Msg,
                        "Hydrated suite dependency cache into worker worktree",
                        payload_from_json(serde_json::json!({
                            "linked_paths": cache_report.linked_paths,
                            "skipped_existing_paths": cache_report.skipped_existing_paths,
                            "missing_cache_paths": cache_report.missing_cache_paths,
                            "suites_considered": cache_report.suites_considered,
                            "invalid_paths": cache_report.invalid_paths,
                        })),
                    );
                }
            }
            Err(err) => {
                self.log_runtime_event(
                    run_id,
                    Some(&job.id),
                    Some(&todo.id),
                    "warning",
                    None,
                    EventType::Error,
                    "Failed to hydrate suite dependency cache into worker worktree",
                    payload_from_json(serde_json::json!({
                        "error": err.to_string(),
                    })),
                );
            }
        }

        let mut updated_job = job.clone();
        updated_job.worktree_path = Some(work_dir.display().to_string());
        if let Err(err) = self.project.store.upsert_job(&updated_job) {
            self.log_state_update_error(
                run_id,
                Some(&job.id),
                Some(&todo.id),
                "failed to persist worker worktree path",
                &err,
            );
        }
        job = updated_job;

        match self.run_single_todo_with_retries(
            run_id,
            &job.id,
            1,
            todo.clone(),
            flow_kind,
            work_dir.clone(),
            model_override,
            Arc::new(AtomicBool::new(false)),
            max_retries.max(1),
            |attempt, total, err| on_retry(attempt, total, err),
        ) {
            Ok(outcome) => {
                if let Err(err) = self
                    .project
                    .git
                    .merge_branch_into_main(&branch, &main_branch)
                    .and_then(|_| self.project.git.remove_worktree(&work_dir, &branch))
                {
                    let err_for_status = err.to_string();
                    if let Err(status_err) =
                        self.project
                            .store
                            .update_todo_status(&todo.id, TodoStatus::Pending, None)
                    {
                        self.log_state_update_error(
                            run_id,
                            Some(&job.id),
                            Some(&todo.id),
                            "failed to mark todo pending after merge error",
                            &status_err,
                        );
                    }
                    if let Err(status_err) = self.project.set_job_status(
                        job,
                        JobStatus::Failed,
                        Some(err_for_status.clone()),
                    ) {
                        self.log_state_update_error(
                            run_id,
                            None,
                            Some(&todo.id),
                            "failed to update job status to failed after merge error",
                            &status_err,
                        );
                    }
                    return Err(self.classify_runtime_error(err));
                }

                if let Some(commit_hash) = outcome.commit_hash.as_deref()
                    && let Err(err) = self.project.store.update_todo_status(
                        &todo.id,
                        TodoStatus::Done,
                        Some(commit_hash),
                    )
                {
                    self.log_state_update_error(
                        run_id,
                        Some(&job.id),
                        Some(&todo.id),
                        "failed to mark todo done",
                        &err,
                    );
                }
                if let Err(err) = self.project.set_job_status(job, JobStatus::Completed, None) {
                    self.log_state_update_error(
                        run_id,
                        None,
                        Some(&todo.id),
                        "failed to update job status to completed",
                        &err,
                    );
                }
                Ok(Some(outcome))
            }
            Err(err) => {
                if let Err(status_err) =
                    self.project
                        .store
                        .update_todo_status(&todo.id, TodoStatus::Pending, None)
                {
                    self.log_state_update_error(
                        run_id,
                        Some(&job.id),
                        Some(&todo.id),
                        "failed to mark todo pending after worker failure",
                        &status_err,
                    );
                }
                if let Err(remove_err) = self.project.git.remove_worktree(&work_dir, &branch) {
                    self.log_state_update_error(
                        run_id,
                        Some(&job.id),
                        Some(&todo.id),
                        "failed to cleanup worker worktree",
                        &remove_err,
                    );
                }
                if let Err(status_err) =
                    self.project
                        .set_job_status(job, JobStatus::Failed, Some(err.to_string()))
                {
                    self.log_state_update_error(
                        run_id,
                        None,
                        Some(&todo.id),
                        "failed to update job status to failed",
                        &status_err,
                    );
                }
                Err(err)
            }
        }
    }

    fn run_next_todo_once_with_retry_hook<FR>(
        &self,
        flow_kind: FlowKind,
        model_override: Option<String>,
        max_retries: usize,
        on_retry: &mut FR,
    ) -> OrchestratorResult<Option<TodoOutcome>>
    where
        FR: FnMut(usize, usize, &anyhow::Error),
    {
        let run_id = self
            .start_run()
            .map_err(|err| self.classify_runtime_error(err))?;

        let result = self.run_next_todo_in_run_with_retry_hook(
            &run_id,
            flow_kind,
            model_override,
            max_retries,
            on_retry,
        );

        self.finish_run(
            &run_id,
            match &result {
                Ok(_) => RunExitStatus::Success,
                Err(err) if err.is_unrecoverable() => RunExitStatus::UnrecoverableFailure,
                Err(_) => RunExitStatus::Failure,
            },
        )
        .map_err(|err| self.classify_runtime_error(err))?;

        result
    }

    pub fn run_next_todo_once(
        &self,
        flow_kind: FlowKind,
        model_override: Option<String>,
    ) -> OrchestratorResult<Option<TodoOutcome>> {
        self.run_next_todo_once_with_retry_hook(
            flow_kind,
            model_override,
            self.project.chief_yaml.chief.max_retries.max(1),
            &mut |_attempt, _total, _err| {},
        )
    }

    pub fn run_next_todo(
        &self,
        flow_kind: FlowKind,
        model_override: Option<String>,
    ) -> Result<Option<TodoOutcome>> {
        self.run_next_todo_once(flow_kind, model_override)
            .map_err(OrchestratorError::into_error)
    }

    fn run_todo_queue_with_runner<FC, FR, FN>(
        &self,
        run_id: &str,
        flow_kind: FlowKind,
        model_override: Option<String>,
        max_retries: usize,
        on_todo_completed: &mut FC,
        on_retry: &mut FR,
        mut run_next_todo: FN,
    ) -> OrchestratorResult<()>
    where
        FC: FnMut(&TodoOutcome),
        FR: FnMut(usize, usize, &anyhow::Error),
        FN: FnMut(
            &str,
            FlowKind,
            Option<String>,
            usize,
            &mut FR,
        ) -> OrchestratorResult<Option<TodoOutcome>>,
    {
        let effective_max_retries = Self::effective_max_retries_for_flow(flow_kind, max_retries);
        loop {
            let next = run_next_todo(
                run_id,
                flow_kind,
                model_override.clone(),
                effective_max_retries,
                on_retry,
            )?;

            let Some(outcome) = next else {
                return Ok(());
            };
            on_todo_completed(&outcome);
        }
    }

    pub(in crate::service) fn run_todos_until_done_with_retries_with_runner<FC, FR, FN>(
        &self,
        flow_kind: FlowKind,
        model_override: Option<String>,
        max_retries: usize,
        mut on_todo_completed: FC,
        mut on_retry: FR,
        mut run_next_todo: FN,
    ) -> OrchestratorResult<()>
    where
        FC: FnMut(&TodoOutcome),
        FR: FnMut(usize, usize, &anyhow::Error),
        FN: FnMut(
            &str,
            FlowKind,
            Option<String>,
            usize,
            &mut FR,
        ) -> OrchestratorResult<Option<TodoOutcome>>,
    {
        let run_id = self
            .start_run()
            .map_err(|err| self.classify_runtime_error(err))?;

        let result = self.run_todo_queue_with_runner(
            &run_id,
            flow_kind,
            model_override,
            max_retries,
            &mut on_todo_completed,
            &mut on_retry,
            |runner_run_id,
             runner_flow_kind,
             runner_model_override,
             runner_max_retries,
             runner_on_retry| {
                run_next_todo(
                    runner_run_id,
                    runner_flow_kind,
                    runner_model_override,
                    runner_max_retries,
                    runner_on_retry,
                )
            },
        );

        self.finish_run(
            &run_id,
            match &result {
                Ok(_) => RunExitStatus::Success,
                Err(err) if err.is_unrecoverable() => RunExitStatus::UnrecoverableFailure,
                Err(_) => RunExitStatus::Failure,
            },
        )
        .map_err(|err| self.classify_runtime_error(err))?;

        result
    }

    pub fn run_todos_until_done_with_retries<FC, FR>(
        &self,
        flow_kind: FlowKind,
        model_override: Option<String>,
        max_retries: usize,
        on_todo_completed: FC,
        on_retry: FR,
    ) -> OrchestratorResult<()>
    where
        FC: FnMut(&TodoOutcome),
        FR: FnMut(usize, usize, &anyhow::Error),
    {
        self.run_todos_until_done_with_retries_with_runner(
            flow_kind,
            model_override,
            max_retries,
            on_todo_completed,
            on_retry,
            |run_id, flow_kind, model_override, max_retries, retry_hook| {
                self.run_next_todo_in_run_with_retry_hook(
                    run_id,
                    flow_kind,
                    model_override,
                    max_retries,
                    retry_hook,
                )
            },
        )
    }
}
