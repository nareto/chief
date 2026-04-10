use super::*;

mod bd_phase;
mod loop_file_phase;
mod refactor_phase;

use bd_phase::BdPhaseStrategy;
use loop_file_phase::LoopFilePhaseStrategy;
use refactor_phase::RefactorPhaseStrategy;

pub trait ExecutionFlow: Send + Sync {
    fn name(&self) -> &'static str;
    fn run(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome>;

    fn run_todo(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome> {
        self.run(execution)
    }
}

pub trait TodoFlow: ExecutionFlow {}

impl<T: ExecutionFlow + ?Sized> TodoFlow for T {}

#[derive(Debug, Clone)]
pub struct LoopFileFlow {
    loop_policy: ConvergenceLoopPolicy,
}

impl Default for LoopFileFlow {
    fn default() -> Self {
        Self::with_loop_policy(20, 2)
    }
}

impl LoopFileFlow {
    pub fn with_loop_policy(max_loop: usize, required_stable_iterations: usize) -> Self {
        let loop_policy = ConvergenceLoopPolicy {
            max_loops: max_loop.max(1),
            required_stable_iterations: required_stable_iterations.max(1),
        };
        Self { loop_policy }
    }
}

impl ExecutionFlow for LoopFileFlow {
    fn name(&self) -> &'static str {
        "loop_file"
    }

    fn run(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome> {
        let mut strategy = LoopFilePhaseStrategy::new();
        self.loop_policy.run(&mut strategy, execution)?;
        strategy.run_post_green_for_involved_suites(execution)?;

        execution.log_event(
            "info",
            Some(Phase::LoopFile),
            EventType::PhaseChange,
            "LOOP_FILE loop done; preparing commit",
            BTreeMap::new(),
        )?;

        let commit_hash = execution
            .git
            .commit_and_tag(
                &execution.project_dir,
                &format!("chief(loop_file): {}", execution.work_item_title()),
            )
            .context("failed to commit work item")?;

        execution.log_event(
            "info",
            Some(Phase::Exit),
            EventType::GitOp,
            format!("Committed work item {}", execution.work_item_id()),
            payload_from_json(json!({ "commit_hash": commit_hash })),
        )?;

        Ok(TodoOutcome {
            todo_id: execution.work_item_id().to_owned(),
            commit_hash: Some(commit_hash),
        })
    }
}

#[derive(Debug, Clone)]
pub struct BdFlow {
    loop_policy: UntilPassLoopPolicy,
}

impl Default for BdFlow {
    fn default() -> Self {
        Self::with_loop_policy(20)
    }
}

impl BdFlow {
    pub fn with_loop_policy(max_loop: usize) -> Self {
        Self {
            loop_policy: UntilPassLoopPolicy {
                max_loops: max_loop.max(1),
            },
        }
    }
}

impl ExecutionFlow for BdFlow {
    fn name(&self) -> &'static str {
        "bd"
    }

    fn run(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome> {
        let mut strategy = BdPhaseStrategy::new();
        self.loop_policy.run(&mut strategy, execution)?;

        execution.log_event(
            "info",
            Some(Phase::Bd),
            EventType::PhaseChange,
            "BD loop done; preparing commit",
            BTreeMap::new(),
        )?;

        if !strategy.performed_agent_run() {
            execution.log_event(
                "info",
                Some(Phase::Bd),
                EventType::GitOp,
                "bd flow found no ready tickets during pre-check; skipping commit",
                BTreeMap::new(),
            )?;
            return Ok(TodoOutcome {
                todo_id: execution.work_item_id().to_owned(),
                commit_hash: None,
            });
        }

        let pending_files = execution
            .git
            .changed_files(&execution.project_dir)
            .context("failed to inspect git working tree after bd convergence")?;
        let commit_hash = if pending_files.is_empty() {
            execution.log_event(
                "info",
                Some(Phase::Bd),
                EventType::GitOp,
                "bd flow completed with no uncommitted git changes; skipping commit",
                BTreeMap::new(),
            )?;
            None
        } else {
            let commit_hash = execution
                .git
                .commit_and_tag(
                    &execution.project_dir,
                    &format!("chief(bd): {}", execution.work_item_title()),
                )
                .context("failed to commit bd convergence work item")?;

            execution.log_event(
                "info",
                Some(Phase::Exit),
                EventType::GitOp,
                format!("Committed work item {}", execution.work_item_id()),
                payload_from_json(json!({ "commit_hash": commit_hash })),
            )?;
            Some(commit_hash)
        };

        Ok(TodoOutcome {
            todo_id: execution.work_item_id().to_owned(),
            commit_hash,
        })
    }
}

#[derive(Debug, Clone)]
pub struct RefactorFlow {
    loop_policy: ConvergenceLoopPolicy,
}

impl Default for RefactorFlow {
    fn default() -> Self {
        Self::with_loop_policy(20, 2)
    }
}

impl RefactorFlow {
    pub fn with_loop_policy(max_loop: usize, required_stable_iterations: usize) -> Self {
        let loop_policy = ConvergenceLoopPolicy {
            max_loops: max_loop.max(1),
            required_stable_iterations: required_stable_iterations.max(1),
        };
        Self { loop_policy }
    }
}

impl ExecutionFlow for RefactorFlow {
    fn name(&self) -> &'static str {
        "refactor"
    }

    fn run(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome> {
        let candidate_suites = execution.selected_suites();
        for suite in &candidate_suites {
            execution.ensure_suite_prepared(suite, Phase::Refactor)?;
        }

        let mut strategy = RefactorPhaseStrategy::new(candidate_suites);
        self.loop_policy.run(&mut strategy, execution)?;
        strategy.run_post_green_for_involved_suites(execution)?;

        execution.log_event(
            "info",
            Some(Phase::Refactor),
            EventType::PhaseChange,
            "REFACTOR loop done; preparing commit",
            BTreeMap::new(),
        )?;

        let commit_hash = execution
            .git
            .commit_and_tag(
                &execution.project_dir,
                &format!("chief(refactor): {}", execution.work_item_title()),
            )
            .context("failed to commit work item")?;

        execution.log_event(
            "info",
            Some(Phase::Exit),
            EventType::GitOp,
            format!("Committed work item {}", execution.work_item_id()),
            payload_from_json(json!({ "commit_hash": commit_hash })),
        )?;

        Ok(TodoOutcome {
            todo_id: execution.work_item_id().to_owned(),
            commit_hash: Some(commit_hash),
        })
    }
}

pub fn build_flow(
    flow_kind: FlowKind,
    max_loop: usize,
    required_stable_iterations: usize,
) -> Box<dyn TodoFlow> {
    let max_loop = max_loop.max(1);
    let required_stable_iterations = required_stable_iterations.max(1);
    match flow_kind {
        FlowKind::LoopFile => Box::new(LoopFileFlow::with_loop_policy(
            max_loop,
            required_stable_iterations,
        )),
        FlowKind::Bd => Box::new(BdFlow::with_loop_policy(max_loop)),
        FlowKind::Refactor => Box::new(RefactorFlow::with_loop_policy(
            max_loop,
            required_stable_iterations,
        )),
    }
}
