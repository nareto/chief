use super::ChiefEngine;
use crate::domain::{EventType, RunExitStatus, TodoFile, payload_from_json};
use crate::git::GitOps;
use crate::prompt::PromptStore;
use anyhow::{Context, Result, anyhow};

impl ChiefEngine {
    pub fn process_requirements(
        &self,
        requirements_text: &str,
        model_override: Option<String>,
    ) -> Result<String> {
        let run_id = self.start_run()?;

        let out = (|| -> Result<String> {
            let agent = self.project.build_agent(model_override);
            let prompt = self.project.prompts.render_json(
                "requirements.md",
                &serde_json::json!({
                    "requirements_text": requirements_text,
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

            let todos = parse_requirements_todos(&response.merged_output)
                .context("failed parsing requirements output into todos")?;
            self.project
                .store
                .replace_todos(todos)
                .context("failed applying requirements todos to sqlite queue")?;

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

fn parse_requirements_todos(agent_output: &str) -> Result<Vec<crate::domain::Todo>> {
    if let Ok(todo_file) = serde_yaml::from_str::<TodoFile>(agent_output) {
        return Ok(todo_file.todos);
    }

    if let Some(block) = extract_first_yaml_code_block(agent_output) {
        let todo_file = serde_yaml::from_str::<TodoFile>(&block)
            .context("YAML code block must deserialize into { todos: [...] }")?;
        return Ok(todo_file.todos);
    }

    Err(anyhow!(
        "requirements output must be YAML with a top-level `todos:` list (raw YAML or fenced ```yaml block)"
    ))
}

fn extract_first_yaml_code_block(text: &str) -> Option<String> {
    let mut collecting = false;
    let mut buffer = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if !collecting {
            if trimmed.starts_with("```yaml") || trimmed.starts_with("```yml") || trimmed == "```" {
                collecting = true;
            }
            continue;
        }

        if trimmed.starts_with("```") {
            if !buffer.is_empty() {
                return Some(buffer.join("\n"));
            }
            collecting = false;
            continue;
        }
        buffer.push(line);
    }

    None
}
