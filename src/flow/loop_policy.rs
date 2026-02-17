use super::*;

pub trait PhaseStrategy {
    fn phase(&self) -> Phase;
    fn check_goal_before_loop(&self) -> bool {
        false
    }
    fn attempt_fix(&mut self, execution: &mut FlowExecution<'_>) -> Result<AgentOutput>;
    fn check_goal(
        &mut self,
        execution: &mut FlowExecution<'_>,
        iteration_idx: isize,
        output: &AgentOutput,
    ) -> Result<LoopDecision>;
}

pub trait LoopPolicy: Send + Sync {
    fn name(&self) -> &'static str;
    fn run(
        &self,
        strategy: &mut dyn PhaseStrategy,
        execution: &mut FlowExecution<'_>,
    ) -> Result<()>;
}

fn phase_label_for_log(phase: Phase) -> String {
    phase.as_str().to_ascii_uppercase()
}

#[derive(Debug, Clone)]
pub struct ConvergenceLoopPolicy {
    pub required_stable_iterations: usize,
    pub max_loops: usize,
}

impl Default for ConvergenceLoopPolicy {
    fn default() -> Self {
        Self {
            required_stable_iterations: 2,
            max_loops: 6,
        }
    }
}

impl LoopPolicy for ConvergenceLoopPolicy {
    fn name(&self) -> &'static str {
        "convergence"
    }

    fn run(
        &self,
        strategy: &mut dyn PhaseStrategy,
        execution: &mut FlowExecution<'_>,
    ) -> Result<()> {
        let phase = strategy.phase();
        let phase_label = phase_label_for_log(phase);
        let mut stable = 0usize;
        if strategy.check_goal_before_loop() {
            let pre = strategy.check_goal(execution, -1, &AgentOutput::success("precheck", ""))?;
            if matches!(pre, LoopDecision::Success) {
                execution.log_event(
                    "info",
                    Some(phase),
                    EventType::PhaseChange,
                    format!("{phase_label} phase done during pre-check"),
                    BTreeMap::new(),
                )?;
                return Ok(());
            }
        }

        for iteration in 0..self.max_loops {
            execution.log_event(
                "info",
                Some(phase),
                EventType::PhaseChange,
                format!(
                    "{} loop iteration {}/{}",
                    self.name(),
                    iteration + 1,
                    self.max_loops
                ),
                BTreeMap::new(),
            )?;

            let output = strategy.attempt_fix(execution)?;
            let decision = strategy.check_goal(execution, iteration as isize, &output)?;
            match decision {
                LoopDecision::Success => {
                    execution.log_event(
                        "info",
                        Some(phase),
                        EventType::PhaseChange,
                        format!(
                            "{phase_label} phase done on iteration {}/{}",
                            iteration + 1,
                            self.max_loops
                        ),
                        BTreeMap::new(),
                    )?;
                    return Ok(());
                }
                LoopDecision::Stable => {
                    stable += 1;
                    if stable >= self.required_stable_iterations {
                        execution.log_event(
                            "info",
                            Some(phase),
                            EventType::PhaseChange,
                            format!(
                                "{phase_label} phase done after stable result {}/{}",
                                stable, self.required_stable_iterations
                            ),
                            BTreeMap::new(),
                        )?;
                        return Ok(());
                    }
                    execution.log_event(
                        "info",
                        Some(phase),
                        EventType::PhaseChange,
                        format!(
                            "{phase_label} phase stable {stable}/{}; retrying to confirm",
                            self.required_stable_iterations
                        ),
                        BTreeMap::new(),
                    )?;
                }
                LoopDecision::Retry => {
                    stable = 0;
                    execution.log_event(
                        "warning",
                        Some(phase),
                        EventType::PhaseChange,
                        format!(
                            "{phase_label} phase retrying after iteration {}/{}",
                            iteration + 1,
                            self.max_loops
                        ),
                        BTreeMap::new(),
                    )?;
                }
            }
        }

        execution.log_event(
            "warning",
            Some(phase),
            EventType::PhaseFailure,
            format!(
                "{phase_label} phase retry loop exhausted after {} iterations",
                self.max_loops
            ),
            BTreeMap::new(),
        )?;
        Err(anyhow!("convergence loop failed to converge"))
    }
}

#[derive(Debug, Clone)]
pub struct UntilPassLoopPolicy {
    pub max_loops: usize,
}

impl Default for UntilPassLoopPolicy {
    fn default() -> Self {
        Self { max_loops: 6 }
    }
}

impl LoopPolicy for UntilPassLoopPolicy {
    fn name(&self) -> &'static str {
        "until_pass"
    }

    fn run(
        &self,
        strategy: &mut dyn PhaseStrategy,
        execution: &mut FlowExecution<'_>,
    ) -> Result<()> {
        let phase = strategy.phase();
        let phase_label = phase_label_for_log(phase);
        if strategy.check_goal_before_loop() {
            let pre = strategy.check_goal(execution, -1, &AgentOutput::success("precheck", ""))?;
            if matches!(pre, LoopDecision::Success) {
                execution.log_event(
                    "info",
                    Some(phase),
                    EventType::PhaseChange,
                    format!("{phase_label} phase done during pre-check"),
                    BTreeMap::new(),
                )?;
                return Ok(());
            }
        }

        for iteration in 0..self.max_loops {
            execution.log_event(
                "info",
                Some(phase),
                EventType::PhaseChange,
                format!(
                    "{} loop iteration {}/{}",
                    self.name(),
                    iteration + 1,
                    self.max_loops
                ),
                BTreeMap::new(),
            )?;
            let output = strategy.attempt_fix(execution)?;
            let decision = strategy.check_goal(execution, iteration as isize, &output)?;
            if matches!(decision, LoopDecision::Success) {
                execution.log_event(
                    "info",
                    Some(phase),
                    EventType::PhaseChange,
                    format!(
                        "{phase_label} phase done on iteration {}/{}",
                        iteration + 1,
                        self.max_loops
                    ),
                    BTreeMap::new(),
                )?;
                return Ok(());
            }
            execution.log_event(
                "warning",
                Some(phase),
                EventType::PhaseChange,
                format!(
                    "{phase_label} phase retrying after iteration {}/{}",
                    iteration + 1,
                    self.max_loops
                ),
                BTreeMap::new(),
            )?;
        }

        execution.log_event(
            "warning",
            Some(phase),
            EventType::PhaseFailure,
            format!(
                "{phase_label} phase retry loop exhausted after {} iterations",
                self.max_loops
            ),
            BTreeMap::new(),
        )?;
        Err(anyhow!("until-pass loop failed to reach success"))
    }
}
