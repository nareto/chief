use super::ChiefEngine;
use crate::domain::{EventType, RunExitStatus, payload_from_json};
use crate::git::GitOps;
use crate::prompt::PromptStore;
use anyhow::{Context, Result, anyhow};
use std::path::Path;

impl ChiefEngine {
    pub fn process_requirements(
        &self,
        requirements_text: &str,
        todos_path: &Path,
        model_override: Option<String>,
    ) -> Result<String> {
        let run_id = self.start_run()?;

        let out = (|| -> Result<String> {
            let agent = self.project.build_agent(model_override);
            let prompt = self.project.prompts.render_json(
                "requirements.md",
                &serde_json::json!({
                    "requirements_text": requirements_text,
                    "todos_path": todos_path.display().to_string(),
                }),
            )?;
            self.log_runtime_event(
                &run_id,
                None,
                None,
                "info",
                None,
                EventType::AgentPrompt,
                "Agent prompt (requirements)",
                payload_from_json(serde_json::json!({
                    "prompt": &prompt,
                })),
            );
            let response = match agent.run(crate::agent::AgentRequest {
                prompt,
                cwd: self.project.project_dir.clone(),
                timeout_seconds: Some(self.project.chief_yaml.chief.agent_timeout_seconds),
                disallowed_paths: Vec::new(),
                cancel_signal: None,
                on_chunk: None,
            }) {
                Ok(response) => response,
                Err(err) => {
                    self.log_runtime_event(
                        &run_id,
                        None,
                        None,
                        "error",
                        None,
                        EventType::Error,
                        "Agent execution failed during requirements processing",
                        payload_from_json(serde_json::json!({
                            "error": err.to_string(),
                        })),
                    );
                    return Err(err);
                }
            };

            self.log_runtime_event(
                &run_id,
                None,
                None,
                if response.exit_code == 0 {
                    "info"
                } else {
                    "warning"
                },
                None,
                EventType::AgentResponse,
                "Agent response (requirements)",
                payload_from_json(serde_json::json!({
                    "exit_code": response.exit_code,
                    "command": &response.command,
                    "output": &response.merged_output,
                    "stdout": &response.stdout,
                    "stderr": &response.stderr,
                })),
            );

            if response.exit_code != 0 {
                return Err(anyhow!(
                    "requirements processing failed (exit code {}): {}",
                    response.exit_code,
                    response.merged_output
                ));
            }

            self.project.store.sync_todos_from_file().context(
                "failed syncing todo DB from .chief/todos.yaml after requirements processing",
            )?;

            let diff = self
                .project
                .git
                .diff(&self.project.project_dir, Some("HEAD"))?;
            Ok(diff)
        })();

        self.finish_run(
            &run_id,
            if out.is_ok() {
                RunExitStatus::Success
            } else {
                RunExitStatus::Failure
            },
        )?;

        out
    }
}
