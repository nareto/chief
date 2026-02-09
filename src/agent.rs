use crate::config::ChiefConfig;
use crate::domain::AgentOutput;
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct AgentRequest {
    pub prompt: String,
    pub cwd: PathBuf,
    pub timeout_seconds: Option<u64>,
    pub disallowed_paths: Vec<String>,
    pub cancel_signal: Option<Arc<AtomicBool>>,
}

pub trait CodingAgent: Send + Sync {
    fn name(&self) -> &str;
    fn run(&self, request: AgentRequest) -> Result<AgentOutput>;
}

#[derive(Debug, Clone)]
pub struct CodexAgent {
    model: Option<String>,
    model_reasoning_effort: Option<String>,
    extra_args: Vec<String>,
}

impl CodexAgent {
    pub fn from_config(config: &ChiefConfig, model_override: Option<String>) -> Self {
        Self {
            model: model_override.or_else(|| config.model.clone()),
            model_reasoning_effort: config.model_reasoning_effort.clone(),
            extra_args: config.agent_extra_args.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeAgent {
    extra_args: Vec<String>,
}

impl ClaudeAgent {
    pub fn from_config(config: &ChiefConfig, _model_override: Option<String>) -> Self {
        Self {
            extra_args: config.agent_extra_args.clone(),
        }
    }
}

trait CommandBackedAgent {
    fn build_command(&self, disallowed_paths: &[String]) -> Vec<String>;
    fn parse_output(&self, raw_stdout: &str, raw_stderr: &str) -> String;
}

impl CommandBackedAgent for CodexAgent {
    fn build_command(&self, _disallowed_paths: &[String]) -> Vec<String> {
        let mut cmd = vec![
            "codex".to_owned(),
            "exec".to_owned(),
            "--json".to_owned(),
            "--dangerously-bypass-approvals-and-sandbox".to_owned(),
        ];
        cmd.extend(self.extra_args.iter().cloned());
        if let Some(model) = &self.model {
            cmd.push("-m".to_owned());
            cmd.push(model.clone());
        }
        if let Some(reasoning_effort) = &self.model_reasoning_effort {
            cmd.push("--config".to_owned());
            cmd.push(format!("model_reasoning_effort=\"{reasoning_effort}\""));
        }
        cmd.push("-".to_owned());
        cmd
    }

    fn parse_output(&self, raw_stdout: &str, _raw_stderr: &str) -> String {
        let parsed = parse_codex_json_output(raw_stdout);
        if parsed.trim().is_empty() {
            raw_stdout.trim().to_owned()
        } else {
            parsed
        }
    }
}

impl CommandBackedAgent for ClaudeAgent {
    fn build_command(&self, _disallowed_paths: &[String]) -> Vec<String> {
        let mut cmd = vec![
            "claude".to_owned(),
            "-p".to_owned(),
            "-".to_owned(),
            "--dangerously-skip-permissions".to_owned(),
            "--verbose".to_owned(),
        ];
        cmd.extend(self.extra_args.iter().cloned());
        cmd
    }

    fn parse_output(&self, raw_stdout: &str, raw_stderr: &str) -> String {
        if raw_stdout.trim().is_empty() {
            raw_stderr.trim().to_owned()
        } else {
            raw_stdout.trim().to_owned()
        }
    }
}

fn run_command_backed_agent(
    agent: &impl CommandBackedAgent,
    request: AgentRequest,
) -> Result<AgentOutput> {
    let command = agent.build_command(&request.disallowed_paths);
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

    let (output, wait_state) = wait_with_timeout(
        child,
        request.timeout_seconds,
        request.cancel_signal.as_deref(),
    )
    .context("failed while waiting for agent output")?;

    if wait_state == WaitState::Cancelled {
        return Err(anyhow!(AgentCancelledError));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let merged = agent.parse_output(&stdout, &stderr);

    let mut merged_output = merged;
    if wait_state == WaitState::TimedOut {
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
        exit_code: if wait_state == WaitState::TimedOut {
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

impl CodingAgent for CodexAgent {
    fn name(&self) -> &str {
        "codex"
    }

    fn run(&self, request: AgentRequest) -> Result<AgentOutput> {
        run_command_backed_agent(self, request)
    }
}

impl CodingAgent for ClaudeAgent {
    fn name(&self) -> &str {
        "claude"
    }

    fn run(&self, request: AgentRequest) -> Result<AgentOutput> {
        run_command_backed_agent(self, request)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AgentCancelledError;

impl std::fmt::Display for AgentCancelledError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "agent execution cancelled by stop request")
    }
}

impl std::error::Error for AgentCancelledError {}

pub fn is_agent_cancelled_error(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.downcast_ref::<AgentCancelledError>().is_some())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitState {
    Completed,
    TimedOut,
    Cancelled,
}

fn wait_with_timeout(
    mut child: Child,
    timeout_seconds: Option<u64>,
    cancel_signal: Option<&AtomicBool>,
) -> Result<(Output, WaitState)> {
    let Some(timeout_seconds) = timeout_seconds else {
        let output = child.wait_with_output()?;
        return Ok((output, WaitState::Completed));
    };

    if timeout_seconds == 0 {
        let output = child.wait_with_output()?;
        return Ok((output, WaitState::Completed));
    }

    let mut stdout_handle = child.stdout.take().map(spawn_reader_thread);
    let mut stderr_handle = child.stderr.take().map(spawn_reader_thread);

    let timeout = Duration::from_secs(timeout_seconds);
    let started = Instant::now();

    loop {
        if let Some(status) = child.try_wait()? {
            return collect_output(
                status,
                stdout_handle.take(),
                stderr_handle.take(),
                WaitState::Completed,
            );
        }

        if cancel_signal
            .map(|flag| flag.load(Ordering::SeqCst))
            .unwrap_or(false)
        {
            let _ = child.kill();
            let status = child.wait()?;
            return collect_output(
                status,
                stdout_handle.take(),
                stderr_handle.take(),
                WaitState::Cancelled,
            );
        }

        if started.elapsed() >= timeout {
            let _ = child.kill();
            let status = child.wait()?;
            return collect_output(
                status,
                stdout_handle.take(),
                stderr_handle.take(),
                WaitState::TimedOut,
            );
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}

fn collect_output(
    status: ExitStatus,
    stdout_handle: Option<JoinHandle<io::Result<Vec<u8>>>>,
    stderr_handle: Option<JoinHandle<io::Result<Vec<u8>>>>,
    wait_state: WaitState,
) -> Result<(Output, WaitState)> {
    let stdout = join_reader(stdout_handle, "stdout")?;
    let stderr = join_reader(stderr_handle, "stderr")?;
    Ok((
        Output {
            status,
            stdout,
            stderr,
        },
        wait_state,
    ))
}

fn spawn_reader_thread<R>(mut reader: R) -> JoinHandle<io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut output = Vec::new();
        reader.read_to_end(&mut output)?;
        Ok(output)
    })
}

fn join_reader(
    handle: Option<JoinHandle<io::Result<Vec<u8>>>>,
    stream_name: &str,
) -> Result<Vec<u8>> {
    let Some(handle) = handle else {
        return Ok(Vec::new());
    };
    let read_result = handle
        .join()
        .map_err(|_| anyhow!("agent {stream_name} reader thread panicked"))?;
    read_result.with_context(|| format!("failed to read agent {stream_name}"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_command_uses_full_permissions_without_sandbox() {
        let agent = CodexAgent {
            model: None,
            model_reasoning_effort: None,
            extra_args: Vec::new(),
        };

        let command = agent.build_command(&["src".to_owned()]);
        assert!(
            command
                .iter()
                .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"),
            "codex command should include no-sandbox full-permissions flag: {command:?}"
        );
        assert!(
            !command
                .iter()
                .any(|arg| arg.contains("sandbox_disallow_path")),
            "codex command should not include sandbox disallow path when full-permissions mode is enabled: {command:?}"
        );
    }

    #[test]
    fn claude_command_uses_full_permissions_mode() {
        let agent = ClaudeAgent {
            extra_args: Vec::new(),
        };

        let command = agent.build_command(&["src".to_owned()]);
        assert!(
            command
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions"),
            "claude command should include full-permissions flag: {command:?}"
        );
        assert!(
            !command.iter().any(|arg| arg == "--disallowedTools"),
            "claude command should not include tool-level write restrictions in full-permissions mode: {command:?}"
        );
    }

    #[test]
    fn wait_with_timeout_handles_large_stdout_without_false_timeout() {
        let mut process = Command::new("sh");
        process
            .arg("-lc")
            .arg("dd if=/dev/zero bs=1024 count=512 2>/dev/null");
        process.stdout(Stdio::piped());
        process.stderr(Stdio::piped());
        let child = process.spawn().expect("spawn dd");

        let (output, state) =
            wait_with_timeout(child, Some(5), None).expect("wait_with_timeout should succeed");
        assert_eq!(state, WaitState::Completed);
        assert!(output.status.success());
        assert!(output.stdout.len() >= 512 * 1024);
    }
}
