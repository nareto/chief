use super::*;

mod loop_file_phase;
mod phase_strategies;
mod single_prompt_phase;

use loop_file_phase::LoopFilePhaseStrategy;
use phase_strategies::{GreenPhaseStrategy, PostGreenPhaseStrategy, RedPhaseStrategy};
pub(in crate::flow) use single_prompt_phase::SinglePromptPhaseStrategy;

pub trait TodoFlow: Send + Sync {
    fn name(&self) -> &'static str;
    fn run_todo(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome>;
}

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

impl TodoFlow for TddFlow {
    fn name(&self) -> &'static str {
        "tdd"
    }

    fn run_todo(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome> {
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
                &format!("chief: {}", execution.todo.todo),
            )
            .context("failed to commit completed todo")?;

        execution.log_event(
            "info",
            Some(Phase::Exit),
            EventType::GitOp,
            format!("Committed todo {}", execution.todo.id),
            payload_from_json(json!({ "commit_hash": commit_hash })),
        )?;

        Ok(TodoOutcome {
            todo_id: execution.todo.id.clone(),
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

impl TodoFlow for SinglePromptFlow {
    fn name(&self) -> &'static str {
        "single_prompt"
    }

    fn run_todo(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome> {
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
                &format!("chief(single_prompt): {}", execution.todo.todo),
            )
            .context("failed to commit todo")?;

        execution.log_event(
            "info",
            Some(Phase::Exit),
            EventType::GitOp,
            format!("Committed todo {}", execution.todo.id),
            payload_from_json(json!({ "commit_hash": commit_hash })),
        )?;

        Ok(TodoOutcome {
            todo_id: execution.todo.id.clone(),
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

impl TodoFlow for LoopFileFlow {
    fn name(&self) -> &'static str {
        "loop_file"
    }

    fn run_todo(&self, execution: &mut FlowExecution<'_>) -> Result<TodoOutcome> {
        let candidate_suites = execution.selected_suites();
        for suite in &candidate_suites {
            execution.ensure_suite_prepared(suite, Phase::SinglePrompt)?;
        }

        let mut strategy = LoopFilePhaseStrategy::new(candidate_suites);
        self.loop_policy.run(&mut strategy, execution)?;
        strategy.run_post_green_for_involved_suites(execution)?;

        execution.log_event(
            "info",
            Some(Phase::SinglePrompt),
            EventType::PhaseChange,
            "LOOP_FILE loop done; preparing commit",
            BTreeMap::new(),
        )?;

        let commit_hash = execution
            .git
            .commit_and_tag(
                &execution.project_dir,
                &format!("chief(loop_file): {}", execution.todo.todo),
            )
            .context("failed to commit todo")?;

        execution.log_event(
            "info",
            Some(Phase::Exit),
            EventType::GitOp,
            format!("Committed todo {}", execution.todo.id),
            payload_from_json(json!({ "commit_hash": commit_hash })),
        )?;

        Ok(TodoOutcome {
            todo_id: execution.todo.id.clone(),
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
    }
}
