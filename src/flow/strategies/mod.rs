use super::*;

mod loop_file_phase;
mod phase_strategies;
mod refactor_phase;
mod single_prompt_phase;

use loop_file_phase::LoopFilePhaseStrategy;
use phase_strategies::{GreenPhaseStrategy, PostGreenPhaseStrategy, RedPhaseStrategy};
use refactor_phase::RefactorPhaseStrategy;
pub(in crate::flow) use single_prompt_phase::SinglePromptPhaseStrategy;

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
pub struct TddFlow {
    red_loop: ConvergenceLoopPolicy,
    green_loop: UntilPassLoopPolicy,
    post_green_loop: UntilPassLoopPolicy,
}

impl Default for TddFlow {
    fn default() -> Self {
        Self::with_loop_policy(6, 2)
    }
}

impl TddFlow {
    pub fn with_loop_policy(max_loop: usize, required_stable_iterations: usize) -> Self {
        let red_loop = ConvergenceLoopPolicy {
            max_loops: max_loop.max(1),
            required_stable_iterations: required_stable_iterations.max(1),
        };
        Self {
            red_loop,
            green_loop: UntilPassLoopPolicy {
                max_loops: max_loop.max(1),
            },
            post_green_loop: UntilPassLoopPolicy {
                max_loops: max_loop.max(1),
            },
        }
    }
}

impl ExecutionFlow for TddFlow {
    fn name(&self) -> &'static str {
        "tdd"
    }

    fn run(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome> {
        let suites = execution.selected_suites();
        for suite in &suites {
            execution.ensure_suite_prepared(suite, Phase::Red)?;
        }

        if !suites.is_empty() {
            let mut red = RedPhaseStrategy::new(suites.clone());
            self.red_loop.run(&mut red, execution)?;
            execution.log_event(
                "info",
                Some(Phase::Red),
                EventType::PhaseChange,
                "RED phase done; starting GREEN phase",
                BTreeMap::new(),
            )?;
        }

        let mut green = GreenPhaseStrategy::new(suites.clone());
        self.green_loop.run(&mut green, execution)?;
        execution.log_event(
            "info",
            Some(Phase::Green),
            EventType::PhaseChange,
            "GREEN phase done; starting POST_GREEN phase",
            BTreeMap::new(),
        )?;

        let mut post_green = PostGreenPhaseStrategy::new(suites);
        self.post_green_loop.run(&mut post_green, execution)?;
        execution.log_event(
            "info",
            Some(Phase::PostGreen),
            EventType::PhaseChange,
            "POST_GREEN phase done; preparing commit",
            BTreeMap::new(),
        )?;

        let commit_hash = execution
            .git
            .commit_and_tag(
                &execution.project_dir,
                &format!("chief: {}", execution.work_item_title()),
            )
            .context("failed to commit completed work item")?;

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
pub struct SinglePromptFlow {
    loop_policy: ConvergenceLoopPolicy,
}

impl Default for SinglePromptFlow {
    fn default() -> Self {
        Self::with_loop_policy(6, 2)
    }
}

impl SinglePromptFlow {
    pub fn with_loop_policy(max_loop: usize, required_stable_iterations: usize) -> Self {
        let loop_policy = ConvergenceLoopPolicy {
            max_loops: max_loop.max(1),
            required_stable_iterations: required_stable_iterations.max(1),
        };
        Self { loop_policy }
    }
}

impl ExecutionFlow for SinglePromptFlow {
    fn name(&self) -> &'static str {
        "single_prompt"
    }

    fn run(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome> {
        let candidate_suites = execution.selected_suites();
        for suite in &candidate_suites {
            execution.ensure_suite_prepared(suite, Phase::SinglePrompt)?;
        }

        let mut strategy = SinglePromptPhaseStrategy::new(candidate_suites);
        self.loop_policy.run(&mut strategy, execution)?;
        strategy.run_post_green_for_involved_suites(execution)?;

        execution.log_event(
            "info",
            Some(Phase::SinglePrompt),
            EventType::PhaseChange,
            "SINGLE_PROMPT loop done; preparing commit",
            BTreeMap::new(),
        )?;

        let commit_hash = execution
            .git
            .commit_and_tag(
                &execution.project_dir,
                &format!("chief(single_prompt): {}", execution.work_item_title()),
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
        FlowKind::SinglePrompt => Box::new(SinglePromptFlow::with_loop_policy(
            max_loop,
            required_stable_iterations,
        )),
        FlowKind::Tdd => Box::new(TddFlow::with_loop_policy(
            max_loop,
            required_stable_iterations,
        )),
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
