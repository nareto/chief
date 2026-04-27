use super::*;
use crate::config::{McpServerAuthConfig, McpServerConfig};

#[test]
fn codex_command_uses_full_permissions_without_sandbox() {
    let agent = CodexAgent {
        model: None,
        model_reasoning_effort: None,
        extra_args: Vec::new(),
        mcp_servers: None,
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
fn codex_reasoning_effort_maps_xhigh_to_high() {
    let agent = CodexAgent {
        model: Some("gpt-5".to_owned()),
        model_reasoning_effort: Some("xhigh".to_owned()),
        extra_args: Vec::new(),
        mcp_servers: None,
    };

    let command = agent.build_command(&[]);
    assert!(
        command
            .iter()
            .any(|arg| arg == "model_reasoning_effort=\"high\""),
        "codex command should normalize xhigh to high: {command:?}"
    );
}

#[test]
fn claude_command_uses_full_permissions_mode() {
    let agent = ClaudeAgent {
        model: None,
        extra_args: Vec::new(),
        mcp_servers: None,
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
fn claude_command_includes_model_from_config() {
    let config = ChiefConfig {
        model: Some("opus".to_owned()),
        ..ChiefConfig::default()
    };
    let agent = ClaudeAgent::from_config(&config, None);

    let command = agent.build_command(&[]);
    assert!(
        command
            .windows(2)
            .any(|window| window == ["--model", "opus"]),
        "claude command should include configured model: {command:?}"
    );
}

#[test]
fn claude_command_prefers_runtime_model_override() {
    let config = ChiefConfig {
        model: Some("opus".to_owned()),
        ..ChiefConfig::default()
    };
    let agent = ClaudeAgent::from_config(&config, Some("sonnet".to_owned()));

    let command = agent.build_command(&[]);
    assert!(
        command
            .windows(2)
            .any(|window| window == ["--model", "sonnet"]),
        "claude command should include runtime model override: {command:?}"
    );
    assert!(
        !command
            .windows(2)
            .any(|window| window == ["--model", "opus"]),
        "claude command should not include config model when runtime override is present: {command:?}"
    );
}

#[test]
fn claude_prepare_launch_uses_strict_mcp_config_when_managed() {
    let agent = ClaudeAgent {
        model: None,
        extra_args: Vec::new(),
        mcp_servers: Some(BTreeMap::from([(
            "sentry".to_owned(),
            McpServerConfig::StreamableHttp {
                url: "https://mcp.sentry.dev/mcp".to_owned(),
                auth: Some(McpServerAuthConfig::Jwt {
                    token: None,
                    token_env_var: Some("SENTRY_TOKEN".to_owned()),
                }),
            },
        )])),
    };
    let request = AgentRequest {
        prompt: "test".to_owned(),
        cwd: std::env::temp_dir(),
        timeout_seconds: Some(1),
        disallowed_paths: Vec::new(),
        cancel_signal: None,
        on_chunk: None,
    };

    let launch = agent
        .prepare_launch(&request)
        .expect("launch should prepare");
    assert!(
        launch
            .command
            .iter()
            .any(|arg| arg == "--strict-mcp-config")
    );
    assert!(launch.command.iter().any(|arg| arg == "--mcp-config"));
    assert!(
        launch.scratch_dir.is_some(),
        "managed Claude MCP should keep scratch files alive"
    );
}

#[test]
fn wait_with_timeout_handles_large_stdout_without_false_timeout() {
    let mut process = Command::new("sh");
    process
        .arg("-lc")
        .arg("dd if=/dev/zero bs=1024 count=512 2>/dev/null");
    configure_process_group(&mut process);
    process.stdout(Stdio::piped());
    process.stderr(Stdio::piped());
    let child = process.spawn().expect("spawn dd");

    let (output, state) =
        wait_with_timeout(child, Some(5), None, None).expect("wait_with_timeout should succeed");
    assert_eq!(state, WaitState::Completed);
    assert!(output.status.success());
    assert!(output.stdout.len() >= 512 * 1024);
}

#[test]
fn wait_with_timeout_honors_cancel_without_timeout() {
    let mut process = Command::new("sh");
    process.arg("-lc").arg("sleep 10");
    configure_process_group(&mut process);
    process.stdout(Stdio::piped());
    process.stderr(Stdio::piped());
    let child = process.spawn().expect("spawn sleep");

    let cancel_signal = Arc::new(AtomicBool::new(false));
    let trigger = cancel_signal.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        trigger.store(true, Ordering::SeqCst);
    });

    let started = Instant::now();
    let (output, state) =
        wait_with_timeout(child, None, Some(cancel_signal.as_ref()), None).expect("cancelled wait");
    assert_eq!(state, WaitState::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancelled wait should return quickly"
    );
    assert!(
        !output.status.success(),
        "cancelled process should not report success"
    );
}

#[test]
fn opencode_command_uses_json_format() {
    let agent = OpencodeAgent {
        model: None,
        extra_args: Vec::new(),
        mcp_servers: None,
    };

    let command = agent.build_command(&[]);
    assert!(
        command.iter().any(|arg| arg == "--format"),
        "opencode command should include --format flag: {command:?}"
    );
    assert!(
        command
            .windows(2)
            .any(|window| window == ["--format", "json"]),
        "opencode command should specify json format: {command:?}"
    );
}

#[test]
fn opencode_command_includes_model_from_config() {
    let config = ChiefConfig {
        model: Some("anthropic/claude-sonnet-4-20250514".to_owned()),
        ..ChiefConfig::default()
    };
    let agent = OpencodeAgent::from_config(&config, None);

    let command = agent.build_command(&[]);
    assert!(
        command
            .windows(2)
            .any(|window| window == ["-m", "anthropic/claude-sonnet-4-20250514"]),
        "opencode command should include configured model: {command:?}"
    );
}

#[test]
fn opencode_command_prefers_runtime_model_override() {
    let config = ChiefConfig {
        model: Some("anthropic/claude-sonnet-4-20250514".to_owned()),
        ..ChiefConfig::default()
    };
    let agent = OpencodeAgent::from_config(&config, Some("openai/gpt-4o".to_owned()));

    let command = agent.build_command(&[]);
    assert!(
        command
            .windows(2)
            .any(|window| window == ["-m", "openai/gpt-4o"]),
        "opencode command should include runtime model override: {command:?}"
    );
    assert!(
        !command
            .windows(2)
            .any(|window| window == ["-m", "anthropic/claude-sonnet-4-20250514"]),
        "opencode command should not include config model when runtime override is present: {command:?}"
    );
}

#[test]
fn opencode_command_includes_stdin_dash() {
    let agent = OpencodeAgent {
        model: None,
        extra_args: Vec::new(),
        mcp_servers: None,
    };

    let command = agent.build_command(&[]);
    assert!(
        command.iter().any(|arg| arg == "-"),
        "opencode command should include stdin dash: {command:?}"
    );
    assert!(
        command.first() == Some(&"opencode".to_owned()),
        "opencode command should start with 'opencode': {command:?}"
    );
    assert!(
        command.get(1) == Some(&"run".to_owned()),
        "opencode command should have 'run' as second arg: {command:?}"
    );
}

#[test]
fn parse_opencode_json_output_extracts_text_parts() {
    let output = r#"{"type":"step_start","timestamp":123}
{"type":"text","part":{"text":"Hello "}}
{"type":"text","part":{"text":"world!"}}
{"type":"step_finish"}"#;

    let result = parse_opencode_json_output(output);
    assert_eq!(result, "Hello world!");
}

#[test]
fn parse_opencode_json_output_handles_empty_input() {
    let result = parse_opencode_json_output("");
    assert_eq!(result, "");
}

#[test]
fn parse_opencode_json_output_handles_invalid_json() {
    let output = r#"not valid json at all
also not json
{"type":"text","part":{"text":"but this is"}}"#;

    let result = parse_opencode_json_output(output);
    assert_eq!(result, "but this is");
}

#[test]
fn cursor_command_uses_headless_json_mode_with_full_permissions() {
    let agent = CursorAgent {
        model: None,
        extra_args: Vec::new(),
        mcp_servers: None,
    };

    let command = agent.build_command(&[]);
    assert!(
        command
            .windows(2)
            .any(|window| window == ["-p", "--output-format"])
            || command.iter().any(|arg| arg == "-p"),
        "cursor-agent command should enable print mode: {command:?}"
    );
    assert!(
        command
            .windows(2)
            .any(|window| window == ["--output-format", "json"]),
        "cursor-agent command should request json output: {command:?}"
    );
    assert!(
        command.iter().any(|arg| arg == "--force"),
        "cursor-agent command should auto-approve shell commands: {command:?}"
    );
    assert!(
        command
            .windows(2)
            .any(|window| window == ["--sandbox", "disabled"]),
        "cursor-agent command should disable sandbox mode: {command:?}"
    );
    assert!(
        command.iter().any(|arg| arg == "--trust"),
        "cursor-agent command should trust the workspace in headless mode: {command:?}"
    );
}

#[test]
fn cursor_command_includes_model_from_config() {
    let config = ChiefConfig {
        model: Some("gpt-5.4-xhigh".to_owned()),
        ..ChiefConfig::default()
    };
    let agent = CursorAgent::from_config(&config, None);

    let command = agent.build_command(&[]);
    assert!(
        command
            .windows(2)
            .any(|window| window == ["--model", "gpt-5.4-xhigh"]),
        "cursor-agent command should include configured model: {command:?}"
    );
}

#[test]
fn cursor_prepare_launch_uses_isolated_home_when_managed_mcp_is_enabled() {
    let agent = CursorAgent {
        model: None,
        extra_args: Vec::new(),
        mcp_servers: Some(BTreeMap::from([(
            "context7".to_owned(),
            McpServerConfig::Stdio {
                command: "npx".to_owned(),
                args: vec!["-y".to_owned(), "@upstash/context7-mcp".to_owned()],
                env: BTreeMap::new(),
            },
        )])),
    };
    let request = AgentRequest {
        prompt: "test".to_owned(),
        cwd: std::env::temp_dir(),
        timeout_seconds: Some(1),
        disallowed_paths: Vec::new(),
        cancel_signal: None,
        on_chunk: None,
    };

    let launch = agent
        .prepare_launch(&request)
        .expect("launch should prepare");
    assert!(launch.env.contains_key("HOME"));
    assert!(launch.env.contains_key("XDG_CONFIG_HOME"));
    assert!(launch.env.contains_key("XDG_DATA_HOME"));
    assert!(launch.env.contains_key("XDG_CACHE_HOME"));
    let home_dir = launch
        .env
        .get("HOME")
        .expect("managed Cursor MCP launch should override HOME");
    assert_eq!(
        launch.env.get("XDG_CONFIG_HOME"),
        Some(
            &std::path::Path::new(home_dir)
                .join(".config")
                .display()
                .to_string()
        )
    );
    assert!(
        launch.scratch_dir.is_some(),
        "managed Cursor MCP should keep scratch files alive"
    );
}

#[test]
fn parse_cursor_json_output_extracts_result_field() {
    let output =
        r#"{"type":"result","subtype":"success","is_error":false,"result":"Hello Cursor!"}"#;
    let result = parse_cursor_json_output(output);
    assert_eq!(result, "Hello Cursor!");
}

#[test]
fn parse_cursor_json_output_handles_invalid_json() {
    let result = parse_cursor_json_output("not json");
    assert_eq!(result, "");
}
