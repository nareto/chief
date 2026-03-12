use super::*;
use anyhow::bail;
use std::process::Command;

#[derive(Debug, Clone)]
struct BdReadySnapshot {
    raw_json: String,
    tickets: Vec<Value>,
}

impl BdReadySnapshot {
    fn ticket_count(&self) -> usize {
        self.tickets.len()
    }
}

#[derive(Debug, Clone, Default)]
pub(in crate::flow) struct BdPhaseStrategy {
    next_snapshot: Option<BdReadySnapshot>,
    attempts: usize,
    performed_agent_run: bool,
}

impl BdPhaseStrategy {
    pub(in crate::flow) fn new() -> Self {
        Self::default()
    }

    pub(in crate::flow) fn performed_agent_run(&self) -> bool {
        self.performed_agent_run
    }

    fn command_path(project_dir: &Path) -> PathBuf {
        let local = project_dir.join("bd");
        if local.is_file() {
            local
        } else {
            PathBuf::from("bd")
        }
    }

    fn load_ready_tickets(project_dir: &Path) -> Result<BdReadySnapshot> {
        let command_path = Self::command_path(project_dir);
        let output = Command::new(&command_path)
            .arg("ready")
            .arg("--json")
            .current_dir(project_dir)
            .output()
            .with_context(|| {
                format!(
                    "failed to execute `{}` for ready bd tickets",
                    command_path.display()
                )
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let mut details = Vec::new();
            if !stdout.is_empty() {
                details.push(format!("stdout: {stdout}"));
            }
            if !stderr.is_empty() {
                details.push(format!("stderr: {stderr}"));
            }
            let suffix = if details.is_empty() {
                String::new()
            } else {
                format!(" ({})", details.join("; "))
            };
            bail!(
                "`{} ready --json` failed with status {}{}",
                command_path.display(),
                output.status,
                suffix
            );
        }

        let stdout =
            String::from_utf8(output.stdout).context("bd ready output was not valid UTF-8")?;
        let raw_json = if stdout.trim().is_empty() {
            "[]".to_owned()
        } else {
            stdout.trim().to_owned()
        };
        let tickets = serde_json::from_str::<Vec<Value>>(&raw_json)
            .context("failed to parse `bd ready --json` output as a JSON array")?;

        Ok(BdReadySnapshot { raw_json, tickets })
    }
}

impl PhaseStrategy for BdPhaseStrategy {
    fn phase(&self) -> Phase {
        Phase::Bd
    }

    fn check_goal_before_loop(&self) -> bool {
        true
    }

    fn attempt_fix(&mut self, execution: &mut FlowExecution<'_>) -> Result<AgentOutput> {
        let snapshot = match self.next_snapshot.take() {
            Some(snapshot) => snapshot,
            None => Self::load_ready_tickets(&execution.project_dir)?,
        };
        let prompt = execution.prompts.render_json(
            "bd.md",
            &json!({
                "work_item": execution.work_item(),
                "todo": execution.work_item_prompt_payload(),
                "bd_tickets": snapshot.raw_json,
                "bd_ticket_items": snapshot.tickets,
                "bd_ticket_count": snapshot.ticket_count(),
                "iteration": self.attempts + 1,
                "run_id": execution.run_id,
            }),
        )?;

        let output = execution.run_agent(Phase::Bd, prompt, Vec::new())?;
        self.attempts += 1;
        self.performed_agent_run = true;
        Ok(output)
    }

    fn check_goal(
        &mut self,
        execution: &mut FlowExecution<'_>,
        iteration_idx: isize,
        output: &AgentOutput,
    ) -> Result<LoopDecision> {
        let snapshot = Self::load_ready_tickets(&execution.project_dir)?;

        if iteration_idx < 0 {
            if snapshot.tickets.is_empty() {
                execution.log_event(
                    "info",
                    Some(Phase::Bd),
                    EventType::PhaseChange,
                    "bd: no ready tickets remain",
                    BTreeMap::new(),
                )?;
                return Ok(LoopDecision::Success);
            }

            self.next_snapshot = Some(snapshot);
            return Ok(LoopDecision::Retry);
        }

        if output.exit_code != 0 {
            self.next_snapshot = Some(snapshot.clone());
            execution.log_event(
                "warning",
                Some(Phase::Bd),
                EventType::PhaseFailure,
                "bd agent step failed",
                payload_from_json(json!({
                    "exit_code": output.exit_code,
                    "command": output.command,
                    "bd_ticket_count": snapshot.ticket_count(),
                    "bd_tickets": snapshot.tickets,
                })),
            )?;
            return Ok(LoopDecision::Retry);
        }

        if snapshot.tickets.is_empty() {
            execution.log_event(
                "info",
                Some(Phase::Bd),
                EventType::PhaseChange,
                "bd: no ready tickets remain",
                BTreeMap::new(),
            )?;
            return Ok(LoopDecision::Success);
        }

        let ticket_count = snapshot.ticket_count();
        let tickets = snapshot.tickets.clone();
        self.next_snapshot = Some(snapshot);
        execution.log_event(
            "warning",
            Some(Phase::Bd),
            EventType::PhaseChange,
            format!("{ticket_count} ready bd ticket(s); restarting convergence loop"),
            payload_from_json(json!({
                "bd_ticket_count": ticket_count,
                "bd_tickets": tickets,
            })),
        )?;
        Ok(LoopDecision::Retry)
    }
}
