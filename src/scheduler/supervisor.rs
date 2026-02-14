use super::{Scheduler, WorkerResult, worker};
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
                    context.log_project_event(
                        &run_id,
                        None,
                        None,
                        "info",
                        None,
                        crate::domain::EventType::Job,
                        format!("Stop requested for {project_name}; cancellation complete"),
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
                            state.cancel_signal.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }

            if cancel_signal.load(Ordering::SeqCst) {
                let mut states = self.states.lock().await;
                if let Some(state) = states.get_mut(&project_name) {
                    state.stop_requested = true;
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
            state.cancel_signal.store(false, Ordering::SeqCst);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Scheduler, WorkerInvocation, WorkerResult, WorkerRunner};
    use crate::domain::{JobStatus, Todo, TodoStatus};
    use crate::flow::FlowKind;
    use crate::service::{ProjectContext, ProjectRegistry};
    use rusqlite::Connection;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("chief-supervisor-{label}-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("failed to create temporary directory");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn init_git_repo(path: &Path) {
        fs::create_dir_all(path).expect("failed to create project directory");
        let init = Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(path)
            .output()
            .expect("failed to run git init");
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );

        let email = Command::new("git")
            .args(["config", "user.email", "supervisor-test@example.com"])
            .current_dir(path)
            .output()
            .expect("failed to configure git user email");
        assert!(
            email.status.success(),
            "git config user.email failed: {}",
            String::from_utf8_lossy(&email.stderr)
        );

        let name = Command::new("git")
            .args(["config", "user.name", "Supervisor Test"])
            .current_dir(path)
            .output()
            .expect("failed to configure git user name");
        assert!(
            name.status.success(),
            "git config user.name failed: {}",
            String::from_utf8_lossy(&name.stderr)
        );

        fs::write(path.join("README.md"), "seed\n").expect("failed to write seed file");
        fs::write(path.join("chief.yaml"), "chief: {}\n")
            .expect("failed to write chief.yaml fixture");
        let add = Command::new("git")
            .args(["add", "README.md", "chief.yaml"])
            .current_dir(path)
            .output()
            .expect("failed to git add seed file");
        assert!(
            add.status.success(),
            "git add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );

        let commit = Command::new("git")
            .args(["commit", "-m", "init", "-q"])
            .current_dir(path)
            .output()
            .expect("failed to commit seed file");
        assert!(
            commit.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&commit.stderr)
        );
    }

    fn pending_todo(text: &str) -> Todo {
        Todo {
            id: String::new(),
            todo: text.to_owned(),
            expectations: String::new(),
            priority: 0,
            test_suites: Vec::new(),
            status: TodoStatus::Pending,
            done_at_commit: None,
        }
        .normalize()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unrecoverable_worker_failure_stops_scheduler_and_sets_unrecoverable_run_status() {
        let root = TempDir::new("unrecoverable");
        let project_dir = root.path.join("project-alpha");
        init_git_repo(&project_dir);

        let context = ProjectContext::load(&project_dir).expect("failed to load project context");
        context
            .store
            .append_todo(pending_todo("first todo"))
            .expect("failed to append first todo");
        context
            .store
            .append_todo(pending_todo("second todo"))
            .expect("failed to append second todo");
        let project_name = context.name.clone();

        let registry = ProjectRegistry::discover(&root.path, &[])
            .expect("failed to discover project registry");
        let scheduler = Scheduler::new(registry, 4);

        {
            let mut states = scheduler.states.lock().await;
            let state = states
                .entry(project_name.clone())
                .or_insert_with(super::super::ProjectRuntime::new);
            state.running = true;
            state.desired_agents = 1;
            state.flow_kind = FlowKind::SinglePrompt;
            state.model_override = None;
            state.stop_requested = false;
            state.active_workers = 0;
            state.last_error = None;
            state.cancel_signal.store(false, Ordering::SeqCst);
        }

        let worker_invocations = Arc::new(AtomicUsize::new(0));
        let terminal_error = "transient lock/contention retry budget exhausted after 3 retries: git commit failed: Unable to create '/tmp/repo/.git/index.lock': File exists.\nAnother git process seems to be running in this repository".to_owned();

        let worker_runner: WorkerRunner = {
            let worker_invocations = worker_invocations.clone();
            let terminal_error = terminal_error.clone();
            Arc::new(move |invocation: WorkerInvocation| {
                worker_invocations.fetch_add(1, Ordering::SeqCst);
                invocation
                    .context
                    .store
                    .update_todo_status(&invocation.todo.id, TodoStatus::Pending, None)
                    .expect("failed to update todo status");
                invocation
                    .context
                    .set_job_status(
                        invocation.job.clone(),
                        JobStatus::Failed,
                        Some(terminal_error.clone()),
                    )
                    .expect("failed to update job status");
                WorkerResult {
                    job_id: invocation.job.id,
                    todo_id: invocation.todo.id,
                    status: "failed".to_owned(),
                    error: Some(terminal_error.clone()),
                    commit_hash: None,
                    unrecoverable: true,
                }
            })
        };

        scheduler
            .supervise_project_with_worker_runner(project_name.clone(), worker_runner)
            .await
            .expect("supervisor should complete");

        assert_eq!(
            worker_invocations.load(Ordering::SeqCst),
            1,
            "scheduler should not start new workers after unrecoverable failure"
        );

        let refreshed = scheduler
            .get_project_context(&project_name)
            .await
            .expect("failed to reload project context");
        let jobs = refreshed.store.list_jobs(20).expect("failed to list jobs");
        assert_eq!(jobs.len(), 1, "only one job should run before shutdown");
        let failed_job = &jobs[0];
        assert_eq!(failed_job.status, JobStatus::Failed);
        let failed_error = failed_job.error.clone().unwrap_or_default();
        assert!(
            failed_error.contains(".git/index.lock"),
            "failed job should retain lock path details: {failed_error}"
        );

        let todos = refreshed.store.list_todos().expect("failed to list todos");
        let pending_count = todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::Pending)
            .count();
        assert_eq!(
            pending_count, 2,
            "todos should be pending after unrecoverable shutdown"
        );

        let conn = Connection::open(&refreshed.store.db_path).expect("failed to open chief.db");
        let (run_status, run_exit_status): (String, Option<String>) = conn
            .query_row(
                "SELECT status, exit_status FROM runs ORDER BY started_at DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("failed to query latest run");
        assert_eq!(run_status, "finished");
        assert_eq!(
            run_exit_status.as_deref(),
            Some("unrecoverable_failure"),
            "run should finish as unrecoverable failure"
        );

        let runtime = scheduler
            .list_project_views()
            .await
            .into_iter()
            .find(|view| view.name == project_name)
            .expect("runtime view should exist");
        assert!(!runtime.running, "runtime should not remain running");
        assert_eq!(
            runtime.active_workers, 0,
            "active workers should be drained"
        );
        let runtime_error = runtime.last_error.unwrap_or_default();
        assert!(
            !runtime_error.is_empty(),
            "runtime last_error should remain populated after shutdown"
        );
        assert!(
            runtime_error.contains(".git/index.lock"),
            "runtime last_error should preserve lock path details: {runtime_error}"
        );
        assert!(
            runtime_error.contains("retry budget exhausted"),
            "runtime last_error should preserve retry exhaustion detail: {runtime_error}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "placeholder until multi-agent supervisor refactor lands"]
    async fn multi_agent_terminal_failure_cancels_peer_workers_and_stops_run() {
        // TODO: After the multi-agent supervisor refactor, cover desired_agents > 1 and assert:
        // - one worker terminal failure flips stop/cancel for the project run,
        // - peer workers are cancelled/drained,
        // - no new todo claims occur after cancellation,
        // - the run exits with failure and pending todos are preserved.
    }
}
