mod mcp;

use crate::config::{ChiefConfig, McpServerConfig};
use crate::domain::{AgentOutput, WaitState};
use crate::flow::{configure_process_group, terminate_process_tree};
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct AgentRequest {
    pub prompt: String,
    pub cwd: PathBuf,
    pub timeout_seconds: Option<u64>,
    pub disallowed_paths: Vec<String>,
    pub cancel_signal: Option<Arc<AtomicBool>>,
    pub on_chunk: Option<AgentChunkCallback>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentOutputStream {
    Stdout,
    Stderr,
}

impl AgentOutputStream {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

pub type AgentChunkCallback = Arc<dyn Fn(AgentOutputStream, &str) + Send + Sync + 'static>;

pub trait CodingAgent: Send + Sync {
    fn name(&self) -> &str;
    fn run(&self, request: AgentRequest) -> Result<AgentOutput>;
}

#[derive(Debug, Clone)]
pub struct CodexAgent {
    model: Option<String>,
    model_reasoning_effort: Option<String>,
    extra_args: Vec<String>,
    mcp_servers: Option<BTreeMap<String, McpServerConfig>>,
}

impl CodexAgent {
    pub fn from_config(config: &ChiefConfig, model_override: Option<String>) -> Self {
        Self {
            model: model_override.or_else(|| config.model.clone()),
            model_reasoning_effort: config.model_reasoning_effort.clone(),
            extra_args: config.agent_extra_args.clone(),
            mcp_servers: config.mcp_servers.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeAgent {
    model: Option<String>,
    extra_args: Vec<String>,
    mcp_servers: Option<BTreeMap<String, McpServerConfig>>,
}

impl ClaudeAgent {
    pub fn from_config(config: &ChiefConfig, model_override: Option<String>) -> Self {
        Self {
            model: model_override.or_else(|| config.model.clone()),
            extra_args: config.agent_extra_args.clone(),
            mcp_servers: config.mcp_servers.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpencodeAgent {
    model: Option<String>,
    extra_args: Vec<String>,
    mcp_servers: Option<BTreeMap<String, McpServerConfig>>,
}

impl OpencodeAgent {
    pub fn from_config(config: &ChiefConfig, model_override: Option<String>) -> Self {
        Self {
            model: model_override.or_else(|| config.model.clone()),
            extra_args: config.agent_extra_args.clone(),
            mcp_servers: config.mcp_servers.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CursorAgent {
    model: Option<String>,
    extra_args: Vec<String>,
    mcp_servers: Option<BTreeMap<String, McpServerConfig>>,
}

impl CursorAgent {
    pub fn from_config(config: &ChiefConfig, model_override: Option<String>) -> Self {
        Self {
            model: model_override.or_else(|| config.model.clone()),
            extra_args: config.agent_extra_args.clone(),
            mcp_servers: config.mcp_servers.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PiAgent {
    model: Option<String>,
    extra_args: Vec<String>,
    mcp_servers: Option<BTreeMap<String, McpServerConfig>>,
}

impl PiAgent {
    pub fn from_config(config: &ChiefConfig, model_override: Option<String>) -> Self {
        Self {
            model: model_override.or_else(|| config.model.clone()),
            extra_args: config.agent_extra_args.clone(),
            mcp_servers: config.mcp_servers.clone(),
        }
    }

    fn validate_protocol_extra_args(&self) -> Result<()> {
        for arg in &self.extra_args {
            if matches!(arg.as_str(), "-p" | "--print" | "--mode") || arg.starts_with("--mode=") {
                return Err(anyhow!(
                    "pi agent_extra_args must not override Chief's JSON protocol mode: {arg}"
                ));
            }
        }
        Ok(())
    }
}

struct PreparedAgentLaunch {
    command: Vec<String>,
    env: BTreeMap<String, String>,
    scratch_dir: Option<mcp::AgentScratchDir>,
}

impl PreparedAgentLaunch {
    fn new(command: Vec<String>) -> Self {
        Self {
            command,
            env: BTreeMap::new(),
            scratch_dir: None,
        }
    }
}

trait CommandBackedAgent {
    fn build_command(&self, disallowed_paths: &[String]) -> Vec<String>;
    fn prepare_launch(&self, request: &AgentRequest) -> Result<PreparedAgentLaunch> {
        Ok(PreparedAgentLaunch::new(
            self.build_command(&request.disallowed_paths),
        ))
    }
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
            cmd.push(format!(
                "model_reasoning_effort=\"{}\"",
                reasoning_effort.trim()
            ));
        }
        cmd.push("-".to_owned());
        cmd
    }

    fn prepare_launch(&self, request: &AgentRequest) -> Result<PreparedAgentLaunch> {
        let mut launch = PreparedAgentLaunch::new(self.build_command(&request.disallowed_paths));
        if let Some(servers) = &self.mcp_servers {
            let runtime = mcp::prepare_codex_mcp_runtime(servers, &request.cwd)?;
            launch.env.insert(
                "CODEX_HOME".to_owned(),
                runtime.home_dir.display().to_string(),
            );
            launch.scratch_dir = Some(runtime.scratch_dir);
        }
        Ok(launch)
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
        if let Some(model) = &self.model {
            cmd.push("--model".to_owned());
            cmd.push(model.clone());
        }
        cmd
    }

    fn prepare_launch(&self, request: &AgentRequest) -> Result<PreparedAgentLaunch> {
        let mut launch = PreparedAgentLaunch::new(self.build_command(&request.disallowed_paths));
        if let Some(servers) = &self.mcp_servers {
            let runtime = mcp::prepare_claude_mcp_runtime(servers)?;
            launch.command.extend([
                "--mcp-config".to_owned(),
                runtime.config_path.display().to_string(),
                "--strict-mcp-config".to_owned(),
            ]);
            launch.scratch_dir = Some(runtime.scratch_dir);
        }
        Ok(launch)
    }

    fn parse_output(&self, raw_stdout: &str, raw_stderr: &str) -> String {
        if raw_stdout.trim().is_empty() {
            raw_stderr.trim().to_owned()
        } else {
            raw_stdout.trim().to_owned()
        }
    }
}

impl CommandBackedAgent for OpencodeAgent {
    fn build_command(&self, _disallowed_paths: &[String]) -> Vec<String> {
        let mut cmd = vec![
            "opencode".to_owned(),
            "run".to_owned(),
            "-".to_owned(),
            "--format".to_owned(),
            "json".to_owned(),
        ];
        cmd.extend(self.extra_args.iter().cloned());
        if let Some(model) = &self.model {
            cmd.push("-m".to_owned());
            cmd.push(model.clone());
        }
        cmd
    }

    fn parse_output(&self, raw_stdout: &str, raw_stderr: &str) -> String {
        let parsed = parse_opencode_json_output(raw_stdout);
        if parsed.trim().is_empty() {
            raw_stderr.trim().to_owned()
        } else {
            parsed
        }
    }
}

impl CommandBackedAgent for CursorAgent {
    fn build_command(&self, _disallowed_paths: &[String]) -> Vec<String> {
        let mut cmd = vec![
            "cursor-agent".to_owned(),
            "-p".to_owned(),
            "--output-format".to_owned(),
            "json".to_owned(),
            "--force".to_owned(),
            "--trust".to_owned(),
            "--approve-mcps".to_owned(),
            "--sandbox".to_owned(),
            "disabled".to_owned(),
        ];
        cmd.extend(self.extra_args.iter().cloned());
        if let Some(model) = &self.model {
            cmd.push("--model".to_owned());
            cmd.push(model.clone());
        }
        cmd
    }

    fn prepare_launch(&self, request: &AgentRequest) -> Result<PreparedAgentLaunch> {
        let mut launch = PreparedAgentLaunch::new(self.build_command(&request.disallowed_paths));
        if let Some(servers) = &self.mcp_servers {
            let runtime = mcp::prepare_cursor_mcp_runtime(servers)?;
            let home_dir = runtime.home_dir.display().to_string();
            launch.env.insert("HOME".to_owned(), home_dir.clone());
            launch.env.insert(
                "XDG_CONFIG_HOME".to_owned(),
                Path::new(&home_dir).join(".config").display().to_string(),
            );
            launch.env.insert(
                "XDG_DATA_HOME".to_owned(),
                Path::new(&home_dir)
                    .join(".local/share")
                    .display()
                    .to_string(),
            );
            launch.env.insert(
                "XDG_CACHE_HOME".to_owned(),
                Path::new(&home_dir).join(".cache").display().to_string(),
            );
            launch.scratch_dir = Some(runtime.scratch_dir);
        }
        Ok(launch)
    }

    fn parse_output(&self, raw_stdout: &str, raw_stderr: &str) -> String {
        let parsed = parse_cursor_json_output(raw_stdout);
        if parsed.trim().is_empty() {
            if raw_stdout.trim().is_empty() {
                raw_stderr.trim().to_owned()
            } else {
                raw_stdout.trim().to_owned()
            }
        } else {
            parsed
        }
    }
}

impl CommandBackedAgent for PiAgent {
    fn build_command(&self, _disallowed_paths: &[String]) -> Vec<String> {
        let mut cmd = vec![
            "pi".to_owned(),
            "--mode".to_owned(),
            "json".to_owned(),
            "--no-session".to_owned(),
            "--approve".to_owned(),
        ];
        cmd.extend(self.extra_args.iter().cloned());
        if let Some(model) = &self.model {
            cmd.push("--model".to_owned());
            cmd.push(model.clone());
        }
        cmd
    }

    fn prepare_launch(&self, request: &AgentRequest) -> Result<PreparedAgentLaunch> {
        self.validate_protocol_extra_args()?;
        let mut launch = PreparedAgentLaunch::new(self.build_command(&request.disallowed_paths));
        if let Some(servers) = &self.mcp_servers {
            let runtime = mcp::prepare_pi_mcp_runtime(servers)?;
            launch.command.extend([
                "--mcp-config".to_owned(),
                runtime.config_path.display().to_string(),
            ]);
            launch.scratch_dir = Some(runtime.scratch_dir);
        }
        Ok(launch)
    }

    fn parse_output(&self, raw_stdout: &str, raw_stderr: &str) -> String {
        parse_pi_json_output(raw_stdout, raw_stderr, 0).merged_output
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PiJsonOutput {
    exit_code: i32,
    merged_output: String,
    stop_reason: Option<String>,
    warnings: Vec<String>,
}

fn parse_pi_json_output(
    raw_stdout: &str,
    raw_stderr: &str,
    process_exit_code: i32,
) -> PiJsonOutput {
    let mut saw_agent_end = false;
    let mut final_assistant: Option<Value> = None;
    let mut last_assistant_message_end: Option<Value> = None;
    let mut malformed_lines = Vec::new();

    for (line_idx, line) in raw_stdout.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            malformed_lines.push(format!("line {} is not valid JSON", line_idx + 1));
            continue;
        };

        match value.get("type").and_then(Value::as_str) {
            Some("agent_end") => {
                saw_agent_end = true;
                final_assistant = value
                    .get("messages")
                    .and_then(last_assistant_from_messages)
                    .or(final_assistant);
            }
            Some("message_end") => {
                if let Some(message) = value.get("message")
                    && message.get("role").and_then(Value::as_str) == Some("assistant")
                {
                    last_assistant_message_end = Some(message.clone());
                }
            }
            _ => {}
        }
    }

    let mut warnings = malformed_lines;
    if !saw_agent_end {
        return pi_protocol_failure(
            process_exit_code,
            raw_stdout,
            raw_stderr,
            warnings,
            "pi JSON protocol did not emit agent_end",
        );
    }

    let assistant = final_assistant.or_else(|| {
        if last_assistant_message_end.is_some() {
            warnings.push(
                "pi agent_end did not include an assistant message; using last message_end"
                    .to_owned(),
            );
        }
        last_assistant_message_end
    });
    let Some(assistant) = assistant else {
        return pi_protocol_failure(
            process_exit_code,
            raw_stdout,
            raw_stderr,
            warnings,
            "pi JSON protocol ended without an assistant message",
        );
    };

    let stop_reason = assistant
        .get("stopReason")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let assistant_text = extract_pi_assistant_text(&assistant);
    let error_message = assistant
        .get("errorMessage")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);

    match stop_reason.as_deref() {
        Some("stop") => {
            if process_exit_code != 0 {
                warnings.push(format!(
                    "pi process exited with code {process_exit_code} after a successful assistant response"
                ));
            }
            PiJsonOutput {
                exit_code: 0,
                merged_output: assistant_text,
                stop_reason,
                warnings,
            }
        }
        Some(reason) => {
            let detail = error_message
                .or_else(|| (!assistant_text.trim().is_empty()).then_some(assistant_text))
                .unwrap_or_else(|| format!("pi assistant stopped with reason {reason}"));
            PiJsonOutput {
                exit_code: if process_exit_code != 0 {
                    process_exit_code
                } else {
                    1
                },
                merged_output: detail,
                stop_reason,
                warnings,
            }
        }
        None => pi_protocol_failure(
            process_exit_code,
            raw_stdout,
            raw_stderr,
            warnings,
            "pi assistant message did not include stopReason",
        ),
    }
}

fn pi_protocol_failure(
    process_exit_code: i32,
    raw_stdout: &str,
    raw_stderr: &str,
    warnings: Vec<String>,
    message: &str,
) -> PiJsonOutput {
    let raw_output = format!("{}\n{}", raw_stdout.trim(), raw_stderr.trim())
        .trim()
        .to_owned();
    let merged_output = if raw_output.is_empty() {
        message.to_owned()
    } else {
        format!("{message}\n{raw_output}")
    };
    PiJsonOutput {
        exit_code: if process_exit_code != 0 {
            process_exit_code
        } else {
            1
        },
        merged_output,
        stop_reason: None,
        warnings,
    }
}

fn last_assistant_from_messages(messages: &Value) -> Option<Value> {
    messages.as_array()?.iter().rev().find_map(|message| {
        (message.get("role").and_then(Value::as_str) == Some("assistant")).then(|| message.clone())
    })
}

fn extract_pi_assistant_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(chunks)) => chunks
            .iter()
            .filter_map(|chunk| match chunk {
                Value::String(text) => Some(text.as_str()),
                Value::Object(obj) => obj.get("text").and_then(Value::as_str).filter(|_| {
                    matches!(
                        obj.get("type").and_then(Value::as_str),
                        Some("text") | Some("output_text") | None
                    )
                }),
                _ => None,
            })
            .collect::<String>(),
        _ => String::new(),
    }
}

fn process_exit_code(output: &Output, wait_state: WaitState) -> i32 {
    if wait_state == WaitState::TimedOut {
        124
    } else {
        output.status.code().unwrap_or(1)
    }
}

fn run_command_backed_agent(
    agent: &impl CommandBackedAgent,
    request: AgentRequest,
) -> Result<AgentOutput> {
    let launch = agent.prepare_launch(&request)?;
    if launch.command.is_empty() {
        return Err(anyhow!("agent command is empty"));
    }
    let command = launch.command.clone();

    let mut process = Command::new(&command[0]);
    process.args(&command[1..]);
    process.envs(&launch.env);
    process.current_dir(&request.cwd);
    process.stdin(Stdio::piped());
    process.stdout(Stdio::piped());
    process.stderr(Stdio::piped());
    configure_process_group(&mut process);

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
        request.on_chunk.clone(),
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
    }

    let exit_code = process_exit_code(&output, wait_state);
    Ok(AgentOutput {
        exit_code,
        process_exit_code: exit_code,
        command: shell_join(&command),
        stdout,
        stderr,
        merged_output,
        warnings: Vec::new(),
    })
}

fn run_pi_agent(agent: &PiAgent, request: AgentRequest) -> Result<AgentOutput> {
    let launch = agent.prepare_launch(&request)?;
    if launch.command.is_empty() {
        return Err(anyhow!("agent command is empty"));
    }
    let command = launch.command.clone();

    let mut process = Command::new(&command[0]);
    process.args(&command[1..]);
    process.envs(&launch.env);
    process.current_dir(&request.cwd);
    process.stdin(Stdio::piped());
    process.stdout(Stdio::piped());
    process.stderr(Stdio::piped());
    configure_process_group(&mut process);

    let mut child = process
        .spawn()
        .with_context(|| format!("failed to spawn agent command: {}", shell_join(&command)))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(request.prompt.as_bytes())
            .context("failed to write prompt to agent stdin")?;
    }

    // Pi JSON mode is a machine protocol; keep raw JSON out of Chief's live stream.
    let (output, wait_state) = wait_with_timeout(
        child,
        request.timeout_seconds,
        request.cancel_signal.as_deref(),
        None,
    )
    .context("failed while waiting for agent output")?;

    if wait_state == WaitState::Cancelled {
        return Err(anyhow!(AgentCancelledError));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let raw_process_exit_code = process_exit_code(&output, wait_state);

    let mut parsed = parse_pi_json_output(&stdout, &stderr, raw_process_exit_code);
    if wait_state == WaitState::TimedOut {
        parsed.exit_code = 124;
        parsed.merged_output = if parsed.merged_output.trim().is_empty() {
            format!(
                "agent timed out after {} second(s) and was terminated.",
                request.timeout_seconds.unwrap_or_default()
            )
        } else {
            format!(
                "agent timed out after {} second(s) and was terminated.\n{}",
                request.timeout_seconds.unwrap_or_default(),
                parsed.merged_output
            )
        };
    }

    if parsed.exit_code == 0
        && !parsed.merged_output.is_empty()
        && let Some(callback) = request.on_chunk.as_ref()
    {
        callback(AgentOutputStream::Stdout, &parsed.merged_output);
    }

    Ok(AgentOutput {
        exit_code: parsed.exit_code,
        process_exit_code: raw_process_exit_code,
        command: shell_join(&command),
        stdout,
        stderr,
        merged_output: parsed.merged_output,
        warnings: parsed.warnings,
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

impl CodingAgent for OpencodeAgent {
    fn name(&self) -> &str {
        "opencode"
    }

    fn run(&self, request: AgentRequest) -> Result<AgentOutput> {
        run_command_backed_agent(self, request)
    }
}

impl CodingAgent for CursorAgent {
    fn name(&self) -> &str {
        "cursor-agent"
    }

    fn run(&self, request: AgentRequest) -> Result<AgentOutput> {
        run_command_backed_agent(self, request)
    }
}

impl CodingAgent for PiAgent {
    fn name(&self) -> &str {
        "pi"
    }

    fn run(&self, request: AgentRequest) -> Result<AgentOutput> {
        run_pi_agent(self, request)
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

        if let Some(item) = value.get("item")
            && let Some(obj) = item.as_object()
        {
            let item_type = obj.get("type").and_then(Value::as_str).unwrap_or_default();
            if item_type == "agent_message"
                && let Some(text) = obj.get("text").and_then(Value::as_str)
            {
                parts.push(text.to_owned());
                continue;
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
                                && let Some(text) = chunk.get("text").and_then(Value::as_str)
                            {
                                combined.push_str(text);
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

        for key in ["output_text", "text", "content"] {
            if let Some(text) = value.get(key).and_then(Value::as_str) {
                parts.push(text.to_owned());
                break;
            }
        }
    }

    parts.join("")
}

fn parse_opencode_json_output(output: &str) -> String {
    let mut parts = Vec::new();

    for line in output.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        if value.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(text) = value
                .get("part")
                .and_then(|p| p.get("text"))
                .and_then(Value::as_str)
            {
                parts.push(text.to_owned());
            }
        }
    }

    parts.join("")
}

fn parse_cursor_json_output(output: &str) -> String {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if let Ok(value) = serde_json::from_str::<Value>(trimmed)
        && let Some(text) = value.get("result").and_then(Value::as_str)
    {
        return text.to_owned();
    }

    for line in trimmed.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(text) = value.get("result").and_then(Value::as_str) {
            return text.to_owned();
        }
    }

    String::new()
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

fn wait_with_timeout(
    mut child: Child,
    timeout_seconds: Option<u64>,
    cancel_signal: Option<&AtomicBool>,
    on_chunk: Option<AgentChunkCallback>,
) -> Result<(Output, WaitState)> {
    let timeout = timeout_seconds
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs);
    let should_poll = timeout.is_some() || cancel_signal.is_some() || on_chunk.is_some();
    if !should_poll {
        let output = child.wait_with_output()?;
        return Ok((output, WaitState::Completed));
    }

    let mut stdout_handle = child
        .stdout
        .take()
        .map(|stdout| spawn_reader_thread(stdout, AgentOutputStream::Stdout, on_chunk.clone()));
    let mut stderr_handle = child
        .stderr
        .take()
        .map(|stderr| spawn_reader_thread(stderr, AgentOutputStream::Stderr, on_chunk));

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
            terminate_process_tree(&mut child);
            let status = child.wait()?;
            return collect_output(
                status,
                stdout_handle.take(),
                stderr_handle.take(),
                WaitState::Cancelled,
            );
        }

        if timeout
            .map(|limit| started.elapsed() >= limit)
            .unwrap_or(false)
        {
            terminate_process_tree(&mut child);
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

fn spawn_reader_thread<R>(
    mut reader: R,
    stream: AgentOutputStream,
    on_chunk: Option<AgentChunkCallback>,
) -> JoinHandle<io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut chunk = [0_u8; 8192];
        loop {
            let read = reader.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            output.extend_from_slice(&chunk[..read]);
            if let Some(callback) = on_chunk.as_ref() {
                let text = String::from_utf8_lossy(&chunk[..read]).into_owned();
                callback(stream, &text);
            }
        }
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
mod tests;
