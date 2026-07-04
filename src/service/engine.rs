use super::{
    ProjectContext, is_known_unrecoverable_error, is_transient_lock_contention_error,
    retry_transient_lock_contention_with_delay,
};
use crate::agent::{AgentCancelledError, is_agent_cancelled_error};
use crate::domain::{EventType, Phase, RunExitStatus, Todo};
use crate::flow::{
    FlowExecution, FlowKind, TodoOutcome, build_flow, is_agent_invocation_error,
};
use crate::git::{
    GIT_TRANSIENT_LOCK_RETRY_ATTEMPTS, GitOps, git_output_has_transient_lock_contention_signature,
    run_git_command_with_retry,
};
use crate::orchestrator::{
    OrchestratorError, OrchestratorResult, retry_with_policy_and_hook_and_delay,
};
use anyhow::{Context, Result, anyhow};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::warn;
use uuid::Uuid;

mod queue;
mod requirements;

#[derive(Debug, Clone)]
pub struct ChiefEngine {
    pub project: ProjectContext,
}

impl ChiefEngine {
    pub(crate) fn effective_max_retries_for_flow(
        flow_kind: FlowKind,
        requested_max_retries: usize,
    ) -> usize {
        if matches!(flow_kind, FlowKind::LoopFile) {
            1
        } else {
            requested_max_retries.max(1)
        }
    }

    pub fn new(project: ProjectContext) -> Self {
        Self { project }
    }

    pub fn start_run(&self) -> Result<String> {
        let run_id = Uuid::new_v4().to_string();
        self.project.store.start_run(&run_id)?;
        Ok(run_id)
    }

    pub fn finish_run(&self, run_id: &str, status: RunExitStatus) -> Result<()> {
        self.project.store.finish_run(run_id, status)
    }

    pub fn run_single_todo_once(
        &self,
        run_id: &str,
        job_id: &str,
        worker_index: usize,
        todo: Todo,
        flow_kind: FlowKind,
        work_dir: PathBuf,
        model_override: Option<String>,
        convergence_watch_paths: Vec<String>,
        cancel_signal: Arc<AtomicBool>,
    ) -> OrchestratorResult<TodoOutcome> {
        if cancel_signal.load(Ordering::SeqCst) {
            return Err(OrchestratorError::unrecoverable(anyhow!(
                AgentCancelledError
            )));
        }

        let flow = build_flow(
            flow_kind,
            self.project.chief_yaml.chief.max_loop_iterations,
            self.project.chief_yaml.chief.required_stable_iterations,
        );
        let agent = self.project.build_agent(model_override);

        let mut execution = FlowExecution {
            run_id: run_id.to_owned(),
            job_id: job_id.to_owned(),
            worker_index,
            project_dir: work_dir,
            store: &self.project.store,
            prompts: &self.project.prompts,
            agent: agent.as_ref(),
            git: &self.project.git,
            chief_config: &self.project.chief_yaml.chief,
            all_suites: &self.project.chief_yaml.suites,
            todo,
            cancel_signal,
            prepared_suites: RefCell::new(std::collections::BTreeSet::new()),
            convergence_watch_paths,
        };

        flow.run_todo(&mut execution)
            .map_err(|err| self.classify_runtime_error(err))
    }

    pub fn run_single_todo(
        &self,
        run_id: &str,
        job_id: &str,
        worker_index: usize,
        todo: Todo,
        flow_kind: FlowKind,
        work_dir: PathBuf,
        model_override: Option<String>,
        convergence_watch_paths: Vec<String>,
        cancel_signal: Arc<AtomicBool>,
    ) -> Result<TodoOutcome> {
        self.run_single_todo_once(
            run_id,
            job_id,
            worker_index,
            todo,
            flow_kind,
            work_dir,
            model_override,
            convergence_watch_paths,
            cancel_signal,
        )
        .map_err(OrchestratorError::into_error)
    }

    pub fn run_single_todo_with_retries<F>(
        &self,
        run_id: &str,
        job_id: &str,
        worker_index: usize,
        todo: Todo,
        flow_kind: FlowKind,
        work_dir: PathBuf,
        model_override: Option<String>,
        convergence_watch_paths: Vec<String>,
        cancel_signal: Arc<AtomicBool>,
        max_retries: usize,
        mut on_retry: F,
    ) -> OrchestratorResult<TodoOutcome>
    where
        F: FnMut(usize, usize, &anyhow::Error),
    {
        let max_retries = Self::effective_max_retries_for_flow(flow_kind, max_retries);
        let todo_id = todo.id.clone();
        retry_with_policy_and_hook_and_delay(
            max_retries,
            |attempt, total| {
                if cancel_signal.load(Ordering::SeqCst) {
                    return Err(OrchestratorError::unrecoverable(anyhow!(
                        AgentCancelledError
                    )));
                }

                if attempt > 1 {
                    self.log_runtime_event(
                        run_id,
                        Some(job_id),
                        Some(&todo_id),
                        "info",
                        Some(Phase::Red),
                        EventType::PhaseChange,
                        format!("Retry loop {attempt}/{total} started; restarting RED phase"),
                        BTreeMap::new(),
                    );

                    match self.reset_retry_workspace(&work_dir) {
                        Ok(changed_files) => {
                            if !changed_files.is_empty() {
                                let mut payload = BTreeMap::new();
                                payload.insert(
                                    "files".to_owned(),
                                    serde_json::Value::Array(
                                        changed_files
                                            .iter()
                                            .cloned()
                                            .map(serde_json::Value::String)
                                            .collect(),
                                    ),
                                );
                                self.log_runtime_event(
                                    run_id,
                                    Some(job_id),
                                    Some(&todo_id),
                                    "warning",
                                    Some(Phase::Red),
                                    EventType::GitOp,
                                    format!(
                                        "Retry cleanup: discarded local git changes before loop {attempt}/{total}"
                                    ),
                                    payload,
                                );
                            }
                        }
                        Err(err) => {
                            let mut payload = BTreeMap::new();
                            payload.insert(
                                "error".to_owned(),
                                serde_json::Value::String(err.to_string()),
                            );
                            self.log_runtime_event(
                                run_id,
                                Some(job_id),
                                Some(&todo_id),
                                "warning",
                                Some(Phase::Red),
                                EventType::Error,
                                format!("Retry cleanup failed before loop {attempt}/{total}"),
                                payload,
                            );
                            return Err(self.classify_runtime_error(err));
                        }
                    }
                }

                let outcome = self.run_single_todo_once(
                    run_id,
                    job_id,
                    worker_index,
                    todo.clone(),
                    flow_kind,
                    work_dir.clone(),
                    model_override.clone(),
                    convergence_watch_paths.clone(),
                    cancel_signal.clone(),
                );

                let Err(OrchestratorError::Retryable(initial_error)) = outcome else {
                    return outcome;
                };
                if !is_transient_lock_contention_error(&initial_error) {
                    return Err(OrchestratorError::retryable(initial_error));
                }

                retry_transient_lock_contention_with_delay(
                    initial_error,
                    || {
                        self.run_single_todo_once(
                            run_id,
                            job_id,
                            worker_index,
                            todo.clone(),
                            flow_kind,
                            work_dir.clone(),
                            model_override.clone(),
                            convergence_watch_paths.clone(),
                            cancel_signal.clone(),
                        )
                    },
                    |retry_attempt, retry_total, err, delay| {
                        let mut payload = BTreeMap::new();
                        payload.insert(
                            "attempt".to_owned(),
                            serde_json::Value::from(retry_attempt as i64),
                        );
                        payload.insert(
                            "total".to_owned(),
                            serde_json::Value::from(retry_total as i64),
                        );
                        payload.insert(
                            "delay_seconds".to_owned(),
                            serde_json::Value::from(delay.as_secs() as i64),
                        );
                        payload.insert(
                            "error".to_owned(),
                            serde_json::Value::String(err.to_string()),
                        );
                        self.log_runtime_event(
                            run_id,
                            Some(job_id),
                            Some(&todo_id),
                            "warning",
                            Some(Phase::Red),
                            EventType::PhaseChange,
                            format!(
                                "Transient lock/contention retry {retry_attempt}/{retry_total} scheduled in {}s",
                                delay.as_secs()
                            ),
                            payload,
                        );
                    },
                    std::thread::sleep,
                )
            },
            |_attempt, _total, err| {
                if is_transient_lock_contention_error(err) {
                    return None;
                }
                Some(Duration::ZERO)
            },
            |attempt, total, err, _delay| {
                let mut payload = BTreeMap::new();
                payload.insert(
                    "attempt".to_owned(),
                    serde_json::Value::from(attempt as i64),
                );
                payload.insert("total".to_owned(), serde_json::Value::from(total as i64));
                payload.insert(
                    "error".to_owned(),
                    serde_json::Value::String(err.to_string()),
                );
                self.log_runtime_event(
                    run_id,
                    Some(job_id),
                    Some(&todo_id),
                    "warning",
                    Some(Phase::Red),
                    EventType::PhaseChange,
                    format!(
                        "Retry loop {attempt}/{total} finished with recoverable failure; preparing retry loop {}/{}",
                        attempt + 1,
                        total
                    ),
                    payload,
                );
                on_retry(attempt, total, err);
            },
            |_delay| {},
        )
    }

    fn classify_runtime_error(&self, err: anyhow::Error) -> OrchestratorError {
        if is_known_unrecoverable_error(&err)
            || is_agent_cancelled_error(&err)
            || is_agent_invocation_error(&err)
        {
            OrchestratorError::unrecoverable(err)
        } else {
            OrchestratorError::retryable(err)
        }
    }

    fn log_runtime_event(
        &self,
        run_id: &str,
        job_id: Option<&str>,
        todo_id: Option<&str>,
        level: &str,
        phase: Option<Phase>,
        event_type: EventType,
        msg: impl Into<String>,
        payload: BTreeMap<String, serde_json::Value>,
    ) {
        if let Err(log_err) = self.project.log_project_event(
            run_id,
            job_id.map(str::to_owned),
            todo_id.map(str::to_owned),
            level,
            phase,
            event_type,
            msg,
            payload,
        ) {
            warn!("failed to record runtime event: {log_err:#}");
        }
    }

    fn reset_retry_workspace(&self, work_dir: &Path) -> Result<Vec<String>> {
        let changed_files = self.project.git.changed_files(work_dir)?;
        if changed_files.is_empty() {
            return Ok(Vec::new());
        }

        self.run_git_command(work_dir, &["reset", "--hard", "HEAD"])?;
        self.run_git_command(work_dir, &["clean", "-fd"])?;
        Ok(changed_files)
    }

    fn run_git_command(&self, cwd: &Path, args: &[&str]) -> Result<()> {
        let output = run_git_command_with_retry(cwd, args)
            .with_context(|| format!("failed to run git {}", args.join(" ")))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if git_output_has_transient_lock_contention_signature(&output) {
                return Err(anyhow!(
                    "transient lock/contention retry budget exhausted after {GIT_TRANSIENT_LOCK_RETRY_ATTEMPTS} retries: git {} failed in {}: {}",
                    args.join(" "),
                    cwd.display(),
                    detail
                ));
            }
            return Err(anyhow!(
                "git {} failed in {}: {}",
                args.join(" "),
                cwd.display(),
                detail
            ));
        }
        Ok(())
    }

    fn log_state_update_error(
        &self,
        run_id: &str,
        job_id: Option<&str>,
        todo_id: Option<&str>,
        msg: &str,
        err: &anyhow::Error,
    ) {
        warn!("{msg}: {err:#}");
        let mut payload = BTreeMap::new();
        payload.insert(
            "error".to_owned(),
            serde_json::Value::String(err.to_string()),
        );
        if let Err(log_err) = self.project.log_project_event(
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
}
