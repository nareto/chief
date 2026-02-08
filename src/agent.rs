use crate::config::ChiefConfig;
use crate::domain::AgentOutput;
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct AgentRequest {
    pub prompt: String,
    pub cwd: PathBuf,
    pub timeout_seconds: Option<u64>,
    pub disallowed_paths: Vec<String>,
}

pub trait CodingAgent: Send + Sync {
    fn name(&self) -> &str;
    fn run(&self, request: AgentRequest) -> Result<AgentOutput>;
}

#[derive(Debug, Clone)]
pub enum AgentKind {
    Codex,
    Claude,
}

#[derive(Debug, Clone)]
pub struct CommandAgent {
    kind: AgentKind,
    model: Option<String>,
    model_reasoning_effort: Option<String>,
    extra_args: Vec<String>,
}

impl CommandAgent {
    pub fn from_config(config: &ChiefConfig, model_override: Option<String>) -> Self {
        let kind = if config.agent.eq_ignore_ascii_case("claude") {
            AgentKind::Claude
        } else {
            AgentKind::Codex
        };

        let model = model_override.or_else(|| config.model.clone());
        Self {
            kind,
            model,
            model_reasoning_effort: config.model_reasoning_effort.clone(),
            extra_args: config.agent_extra_args.clone(),
        }
    }

    fn build_command(&self, disallowed_paths: &[String]) -> Vec<String> {
        match self.kind {
            AgentKind::Codex => {
                let mut cmd = vec!["codex".to_owned(), "exec".to_owned(), "--json".to_owned()];
                cmd.extend(self.extra_args.iter().cloned());
                if let Some(model) = &self.model {
                    cmd.push("-m".to_owned());
                    cmd.push(model.clone());
                }
                if let Some(reasoning_effort) = &self.model_reasoning_effort {
                    cmd.push("--config".to_owned());
                    cmd.push(format!("model_reasoning_effort=\"{reasoning_effort}\""));
                }
                if !disallowed_paths.is_empty() {
                    for path in disallowed_paths {
                        cmd.push("--config".to_owned());
                        cmd.push(format!("sandbox_disallow_path=\"{path}\""));
                    }
                }
                cmd.push("-".to_owned());
                cmd
            }
            AgentKind::Claude => {
                let mut cmd = vec![
                    "claude".to_owned(),
                    "-p".to_owned(),
                    "-".to_owned(),
                    "--permission-mode".to_owned(),
                    "acceptEdits".to_owned(),
                    "--verbose".to_owned(),
                ];
                cmd.extend(self.extra_args.iter().cloned());
                for path in disallowed_paths {
                    cmd.push("--disallowedTools".to_owned());
                    cmd.push(format!("Edit:{path}"));
                    cmd.push("--disallowedTools".to_owned());
                    cmd.push(format!("Write:{path}"));
                }
                cmd
            }
        }
    }

    fn parse_output(&self, raw_stdout: &str, raw_stderr: &str) -> String {
        match self.kind {
            AgentKind::Codex => {
                let parsed = parse_codex_json_output(raw_stdout);
                if parsed.trim().is_empty() {
                    raw_stdout.trim().to_owned()
                } else {
                    parsed
                }
            }
            AgentKind::Claude => {
                if raw_stdout.trim().is_empty() {
                    raw_stderr.trim().to_owned()
                } else {
                    raw_stdout.trim().to_owned()
                }
            }
        }
    }
}

impl CodingAgent for CommandAgent {
    fn name(&self) -> &str {
        match self.kind {
            AgentKind::Codex => "codex",
            AgentKind::Claude => "claude",
        }
    }

    fn run(&self, request: AgentRequest) -> Result<AgentOutput> {
        let command = self.build_command(&request.disallowed_paths);
        if command.is_empty() {
            return Err(anyhow!("agent command is empty"));
        }

        let mut process = Command::new(&command[0]);
        process.args(&command[1..]);
        process.current_dir(&request.cwd);
        process.stdin(Stdio::piped());
        process.stdout(Stdio::piped());
        process.stderr(Stdio::piped());

        let mut child = process
            .spawn()
            .with_context(|| format!("failed to spawn agent command: {}", shell_join(&command)))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(request.prompt.as_bytes())
                .context("failed to write prompt to agent stdin")?;
        }

        let (output, timed_out) = wait_with_timeout(child, request.timeout_seconds)
            .context("failed while waiting for agent output")?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let merged = self.parse_output(&stdout, &stderr);

        let mut merged_output = merged;
        if timed_out {
            merged_output = format!(
                "agent timed out after {} second(s) and was terminated.\n{}",
                request.timeout_seconds.unwrap_or_default(),
                merged_output
            );
        } else if request.timeout_seconds == Some(0) {
            merged_output = format!(
                "timeout_seconds=0 is invalid, run still executed.\n{}",
                merged_output
            );
        }

        Ok(AgentOutput {
            exit_code: if timed_out {
                124
            } else {
                output.status.code().unwrap_or(1)
            },
            command: shell_join(&command),
            stdout,
            stderr,
            merged_output,
        })
    }
}

fn parse_codex_json_output(output: &str) -> String {
    let mut parts = Vec::new();

    for line in output.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        if let Some(item) = value.get("item") {
            if let Some(obj) = item.as_object() {
                let item_type = obj.get("type").and_then(Value::as_str).unwrap_or_default();
                if item_type == "agent_message" {
                    if let Some(text) = obj.get("text").and_then(Value::as_str) {
                        parts.push(text.to_owned());
                        continue;
                    }
                }
                if item_type == "message"
                    && obj.get("role").and_then(Value::as_str) == Some("assistant")
                {
                    match obj.get("content") {
                        Some(Value::String(text)) => {
                            parts.push(text.to_owned());
                            continue;
                        }
                        Some(Value::Array(chunks)) => {
                            let mut combined = String::new();
                            for chunk in chunks {
                                if let Some(text) = chunk.get("text").and_then(Value::as_str) {
                                    combined.push_str(text);
                                    continue;
                                }
                                if chunk.get("type").and_then(Value::as_str) == Some("output_text")
                                {
                                    if let Some(text) = chunk.get("text").and_then(Value::as_str) {
                                        combined.push_str(text);
                                    }
                                }
                            }
                            if !combined.is_empty() {
                                parts.push(combined);
                                continue;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        for key in ["output_text", "text", "content"] {
            if let Some(text) = value.get(key).and_then(Value::as_str) {
                parts.push(text.to_owned());
                break;
            }
        }
    }

    parts.join("")
}

pub fn shell_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| shell_escape(part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_escape(part: &str) -> String {
    if part
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "-_/.:=".contains(ch))
    {
        return part.to_owned();
    }
    let escaped = part.replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn wait_with_timeout(mut child: Child, timeout_seconds: Option<u64>) -> Result<(Output, bool)> {
    let Some(timeout_seconds) = timeout_seconds else {
        let output = child.wait_with_output()?;
        return Ok((output, false));
    };

    if timeout_seconds == 0 {
        let output = child.wait_with_output()?;
        return Ok((output, false));
    }

    let timeout = Duration::from_secs(timeout_seconds);
    let started = Instant::now();

    loop {
        if let Some(status) = child.try_wait()? {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut out) = child.stdout.take() {
                out.read_to_end(&mut stdout)?;
            }
            if let Some(mut err) = child.stderr.take() {
                err.read_to_end(&mut stderr)?;
            }
            return Ok((
                Output {
                    status,
                    stdout,
                    stderr,
                },
                false,
            ));
        }

        if started.elapsed() >= timeout {
            let _ = child.kill();
            let output = child.wait_with_output()?;
            return Ok((output, true));
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn ensure_agent_binary_available(path: &Path, agent_name: &str) -> Result<()> {
    let status = Command::new("sh")
        .arg("-lc")
        .arg(format!("command -v {agent_name}"))
        .current_dir(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to probe '{agent_name}'"))?;

    if !status.success() {
        return Err(anyhow!(
            "agent binary '{agent_name}' is not available in PATH"
        ));
    }
    Ok(())
}
