use super::*;

mod loop_file_phase;
mod refactor_phase;

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
        let mut convergence_pass = 0usize;
        loop {
            convergence_pass += 1;

            let mut strategy = LoopFilePhaseStrategy::new();
            self.loop_policy.run(&mut strategy, execution)?;
            strategy.run_post_green_for_involved_suites(execution)?;

            let convergence_review = strategy.run_convergence_review(execution)?;
            if matches!(convergence_review, LoopDecision::Retry) {
                execution.log_event(
                    "warning",
                    Some(Phase::LoopFile),
                    EventType::PhaseChange,
                    "loop_file convergence check requested another convergence pass",
                    payload_from_json(json!({ "convergence_pass": convergence_pass })),
                )?;
                strategy.reset_prompt_history(execution, "manual/1")?;
                continue;
            }

            let ready_bd_count = strategy.ready_bd_ticket_count(execution)?.unwrap_or(0);
            if ready_bd_count > 0 {
                execution.log_event(
                    "warning",
                    Some(Phase::LoopFile),
                    EventType::PhaseChange,
                    format!(
                        "loop_file convergence check found {ready_bd_count} ready bd ticket(s); restarting convergence loop"
                    ),
                    payload_from_json(json!({
                        "ready_bd_tickets": ready_bd_count,
                        "convergence_pass": convergence_pass,
                    })),
                )?;
                let reset_marker = format!("bd/{}/{}", convergence_pass, ready_bd_count);
                strategy.reset_prompt_history(execution, reset_marker.as_str())?;
                continue;
            }
            break;
        }

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
        FlowKind::Refactor => Box::new(RefactorFlow::with_loop_policy(
            max_loop,
            required_stable_iterations,
        )),
    }
}
