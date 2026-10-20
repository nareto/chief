use super::*;
use crate::review::{REVIEW_PROMPT_TEMPLATE, load_findings};

/// Inputs for one `chief review` run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSpec {
    /// Base ref as given by the user (or the default that resolved).
    pub base_ref: String,
    /// Merge-base commit of `base_ref` and `HEAD`.
    pub merge_base: String,
    /// Findings path as shown to the agent and used as the convergence watch path.
    pub findings_path: String,
    /// Absolute findings path used for validation.
    pub findings_abs_path: PathBuf,
}

/// The review agent broke the read-only contract (edited other tracked files or committed).
#[derive(Debug, Clone)]
pub struct ReviewContractViolation {
    message: String,
}

impl fmt::Display for ReviewContractViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ReviewContractViolation {}

pub(in crate::flow) struct ReviewPhaseStrategy {
    spec: ReviewSpec,
    attempts: usize,
    last_agent_run: Option<AgentRunWithGitChanges>,
    validation_error: Option<String>,
}

impl ReviewPhaseStrategy {
    pub(in crate::flow) fn new(spec: ReviewSpec) -> Self {
        Self {
            spec,
            attempts: 0,
            last_agent_run: None,
            validation_error: None,
        }
    }

    fn is_findings_path(&self, execution: &FlowExecution<'_>, path: &str) -> bool {
        let normalized = path.replace('\\', "/");
        if normalized == self.spec.findings_path.replace('\\', "/") {
            return true;
        }
        execution.project_dir.join(&normalized) == self.spec.findings_abs_path
    }

    fn tracked_snapshot(execution: &FlowExecution<'_>) -> Result<BTreeMap<String, String>> {
        let files = execution
            .git
            .tracked_changed_files(&execution.project_dir)?;
        let mut snapshot = BTreeMap::new();
        for file in files {
            let path = execution.project_dir.join(&file);
            let signature = if path.is_file() {
                let content = fs::read(&path)
                    .with_context(|| format!("failed reading changed file {}", path.display()))?;
                format!("file:{:x}", md5::compute(content))
            } else {
                "missing".to_owned()
            };
            snapshot.insert(file, signature);
        }
        Ok(snapshot)
    }

    fn violation(
        execution: &FlowExecution<'_>,
        message: String,
        payload: Value,
    ) -> Result<AgentOutput> {
        execution.log_event(
            "error",
            Some(Phase::LoopFile),
            EventType::PhaseFailure,
            message.clone(),
            payload_from_json(payload),
        )?;
        Err(anyhow!(ReviewContractViolation { message }))
    }
}

impl PhaseStrategy for ReviewPhaseStrategy {
    fn phase(&self) -> Phase {
        Phase::LoopFile
    }

    fn attempt_fix(&mut self, execution: &mut FlowExecution<'_>) -> Result<AgentOutput> {
        let prompt = execution.prompts.render_json(
            REVIEW_PROMPT_TEMPLATE,
            &json!({
                "base_ref": self.spec.base_ref,
                "merge_base": self.spec.merge_base,
                "findings_path": self.spec.findings_path,
                "iteration": self.attempts + 1,
                "findings_validation_error": self.validation_error,
            }),
        )?;

        let tracked_before = Self::tracked_snapshot(execution)?;
        let run = execution.run_agent_with_git_changes(Phase::LoopFile, prompt, Vec::new())?;
        self.attempts += 1;

        if run.head_commit_changed {
            return Self::violation(
                execution,
                format!(
                    "review agent moved HEAD from {} to {}; chief review must not commit. Inspect and reset the branch yourself",
                    run.head_commit_before, run.head_commit_after
                ),
                json!({
                    "head_commit_before": run.head_commit_before,
                    "head_commit_after": run.head_commit_after,
                }),
            );
        }

        let tracked_after = Self::tracked_snapshot(execution)?;
        let modified = changed_paths_between_snapshots(&tracked_before, &tracked_after)
            .into_iter()
            .filter(|path| !self.is_findings_path(execution, path))
            .collect::<Vec<_>>();
        if !modified.is_empty() {
            return Self::violation(
                execution,
                format!(
                    "review agent modified tracked files outside the findings file: {}. Chief left them as they are; review and revert them yourself",
                    modified.join(", ")
                ),
                json!({ "modified_tracked_files": modified }),
            );
        }

        let output = run.output.clone();
        self.last_agent_run = Some(run);
        Ok(output)
    }

    fn check_goal(
        &mut self,
        execution: &mut FlowExecution<'_>,
        _iteration_idx: isize,
        _output: &AgentOutput,
    ) -> Result<LoopDecision> {
        let had_changes = self
            .last_agent_run
            .take()
            .map(|run| run.had_git_changes)
            .unwrap_or(true);

        if let Err(err) = load_findings(&self.spec.findings_abs_path) {
            let message = format!("{err:#}");
            execution.log_event(
                "warning",
                Some(Phase::LoopFile),
                EventType::PhaseChange,
                "review: findings file is invalid; asking the next pass to repair it",
                payload_from_json(json!({ "error": message })),
            )?;
            self.validation_error = Some(message);
            return Ok(LoopDecision::Retry);
        }
        self.validation_error = None;

        if had_changes {
            execution.log_event(
                "info",
                Some(Phase::LoopFile),
                EventType::PhaseChange,
                "review: pass recorded new findings",
                BTreeMap::new(),
            )?;
            return Ok(LoopDecision::Retry);
        }

        execution.log_event(
            "info",
            Some(Phase::LoopFile),
            EventType::PhaseChange,
            "review: pass added no new findings",
            BTreeMap::new(),
        )?;
        Ok(LoopDecision::Stable)
    }
}
