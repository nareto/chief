use super::*;
use chief::config::{ChiefConfig, TestSuiteConfig};
use chief::domain::{EventType, Phase, RunExitStatus, TargetType};
use chief::service::ProjectContext;
use clap::CommandFactory;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;
use uuid::Uuid;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "chief-{prefix}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&path).expect("temp dir should be created");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn init_git_repo(path: &std::path::Path) {
    let status = Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(path)
        .status()
        .expect("git init should run");
    assert!(status.success(), "git init should succeed");
}

fn git_head(path: &std::path::Path) -> String {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(path)
        .output()
        .expect("git rev-parse should run");
    assert!(
        output.status.success(),
        "git rev-parse should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git_commit_file(path: &std::path::Path, file_name: &str, content: &str, message: &str) {
    fs::write(path.join(file_name), content).expect("test file should be written");
    let add_status = Command::new("git")
        .args(["add", file_name])
        .current_dir(path)
        .status()
        .expect("git add should run");
    assert!(add_status.success(), "git add should succeed");

    let commit_output = Command::new("git")
        .args([
            "-c",
            "user.name=Chief Test",
            "-c",
            "user.email=chief-tests@example.com",
            "commit",
            "-q",
            "-m",
            message,
        ])
        .current_dir(path)
        .output()
        .expect("git commit should run");
    assert!(
        commit_output.status.success(),
        "git commit should succeed: {}",
        String::from_utf8_lossy(&commit_output.stderr)
    );
}

fn write_chief_yaml(project_dir: &std::path::Path, content: &str) {
    fs::create_dir_all(chief::paths::chief_dir(project_dir))
        .expect(".chief directory should be created");
    fs::write(chief::paths::chief_yaml_path(project_dir), content)
        .expect("chief.yaml should be written");
}

fn json_object_payload(value: serde_json::Value) -> BTreeMap<String, Value> {
    match value {
        Value::Object(map) => map.into_iter().collect(),
        _ => BTreeMap::new(),
    }
}

#[test]
fn run_non_init_fails_fast_when_chief_yaml_is_missing() {
    let temp = TempDir::new("run-missing-chief-yaml");
    let cli = Cli {
        project_dir: temp.path.clone(),
        chief: CliChiefOverrides::default(),
        file: None,
        prompt: None,
        watch_only: Vec::new(),
        requirements: Vec::new(),
        requirements_file: Vec::new(),
        command: None,
    };

    let err = run(&cli).expect_err("run should reject projects missing chief.yaml");
    let rendered = err.to_string();
    assert!(
        rendered.contains("missing required chief config"),
        "error should clearly explain missing chief.yaml: {rendered}"
    );
    assert!(
        rendered.contains("chief.yaml"),
        "error should include chief.yaml path: {rendered}"
    );
    assert!(
        !chief::paths::chief_db_path(&temp.path).exists(),
        "rejected run should not execute todo processing or create chief.db"
    );
}

#[test]
fn run_requires_file_when_loop_file_flow_is_selected() {
    let temp = TempDir::new("run-loop-file-missing-file");
    init_git_repo(&temp.path);
    write_chief_yaml(&temp.path, "chief:\n  flow: loop_file\n");

    let cli = Cli {
        project_dir: temp.path.clone(),
        chief: CliChiefOverrides::default(),
        file: None,
        prompt: None,
        watch_only: Vec::new(),
        requirements: Vec::new(),
        requirements_file: Vec::new(),
        command: None,
    };

    let err = run(&cli)
        .expect_err("run should require --file or --prompt when loop_file flow is selected");
    let rendered = err.to_string();
    assert!(
        rendered.contains("requires either --file")
            || rendered.contains("requires --file")
            || rendered.contains("requires --prompt"),
        "error should direct users to provide --file or --prompt: {rendered}"
    );
}

#[test]
fn ensure_gitignore_entries_creates_file_when_missing() {
    let temp = TempDir::new("gitignore-create");
    let gitignore_path = temp.path.join(".gitignore");

    let changed =
        init_files::ensure_gitignore_entries(&gitignore_path, &init_files::INIT_GITIGNORE_ENTRIES)
            .expect("ok");

    assert!(changed);
    assert_eq!(
        fs::read_to_string(gitignore_path).expect("gitignore should exist"),
        ".chief/chief.db\n.chief/chief.example.yaml\n.chief/codex-home\n"
    );
}

#[test]
fn ensure_gitignore_entries_appends_only_missing_entries() {
    let temp = TempDir::new("gitignore-append");
    let gitignore_path = temp.path.join(".gitignore");
    fs::write(&gitignore_path, "target/\n.chief/chief.db")
        .expect("seed gitignore should be written");

    let changed =
        init_files::ensure_gitignore_entries(&gitignore_path, &init_files::INIT_GITIGNORE_ENTRIES)
            .expect("ok");

    assert!(changed);
    assert_eq!(
        fs::read_to_string(&gitignore_path).expect("gitignore should be readable"),
        "target/\n.chief/chief.db\n.chief/chief.example.yaml\n.chief/codex-home\n"
    );
}

#[test]
fn ensure_gitignore_entries_is_idempotent() {
    let temp = TempDir::new("gitignore-idempotent");
    let gitignore_path = temp.path.join(".gitignore");
    fs::write(
        &gitignore_path,
        "/.chief/chief.db\n./.chief/chief.example.yaml\n.chief/codex-home\n",
    )
    .expect("seed gitignore should be written");

    let changed =
        init_files::ensure_gitignore_entries(&gitignore_path, &init_files::INIT_GITIGNORE_ENTRIES)
            .expect("ok");

    assert!(!changed);
    assert_eq!(
        fs::read_to_string(&gitignore_path).expect("gitignore should be readable"),
        "/.chief/chief.db\n./.chief/chief.example.yaml\n.chief/codex-home\n"
    );
}

fn suite_fixture() -> TestSuiteConfig {
    TestSuiteConfig {
        name: "backend".to_owned(),
        language: "Rust".to_owned(),
        framework: "cargo".to_owned(),
        test_root: ".".to_owned(),
        test_command: "cargo test {target}".to_owned(),
        target_type: TargetType::Project,
        default_target: Some(".".to_owned()),
        file_patterns: Vec::new(),
        disallow_write_globs: Vec::new(),
        test_init: Some("echo init {target}".to_owned()),
        test_setup: Some("echo setup {target}".to_owned()),
        cache_paths: Vec::new(),
        cache_key_files: Vec::new(),
        cache_mode: chief::config::SuiteCacheMode::Copy,
        post_green_command: None,
        cleanup_command: None,
        command_timeout_seconds: None,
        lint_command: Some("cargo clippy -- {target}".to_owned()),
        lint_fix_command: Some("cargo fmt -- {target}".to_owned()),
        env: Default::default(),
        strip_root_from_target: true,
    }
}

#[test]
fn parse_suite_test_init_with_underscore_name() {
    let cli = Cli::try_parse_from(["chief", "suite", "test_init", "--suite", "backend"])
        .expect("suite test_init command should parse");

    let Some(Commands::Suite(args)) = cli.command else {
        panic!("expected suite command");
    };
    match args.command {
        SuiteCommand::TestInit(args) => {
            assert_eq!(args.suite, "backend");
        }
        _ => panic!("expected test_init suite command"),
    }
}

#[test]
fn parse_suite_lint_fix_with_dash_alias() {
    let cli = Cli::try_parse_from([
        "chief", "suite", "lint-fix", "--suite", "backend", "--target", "src",
    ])
    .expect("suite lint-fix alias should parse");

    let Some(Commands::Suite(args)) = cli.command else {
        panic!("expected suite command");
    };
    match args.command {
        SuiteCommand::LintFix(args) => {
            assert_eq!(args.suite, "backend");
            assert_eq!(args.target.as_deref(), Some("src"));
        }
        _ => panic!("expected lint_fix suite command"),
    }
}

#[test]
fn parse_suite_lint_with_linting_alias() {
    let cli = Cli::try_parse_from(["chief", "suite", "linting", "--suite", "backend"])
        .expect("suite linting alias should parse");

    let Some(Commands::Suite(args)) = cli.command else {
        panic!("expected suite command");
    };
    match args.command {
        SuiteCommand::Lint(args) => {
            assert_eq!(args.suite, "backend");
            assert_eq!(args.target, None);
        }
        _ => panic!("expected lint suite command"),
    }
}

#[test]
fn parse_check_with_force_flag() {
    let cli = Cli::try_parse_from(["chief", "check", "--force"])
        .expect("check command with --force should parse");

    let Some(Commands::Check(args)) = cli.command else {
        panic!("expected check command");
    };
    assert!(args.force, "check --force should set force=true");
}

#[test]
fn parse_root_file_option() {
    let cli = Cli::try_parse_from(["chief", "--file", "plan.md"])
        .expect("root --file option should parse");
    assert_eq!(cli.file, Some(PathBuf::from("plan.md")));
}

#[test]
fn parse_root_flow_option_accepts_loop_file_alias() {
    let cli = Cli::try_parse_from(["chief", "--flow", "loop-file", "--prompt", "ship it"])
        .expect("root --flow should accept loop-file alias");

    assert_eq!(cli.chief.flow, Some(CliFlowValue::LoopFile));
    assert_eq!(cli.prompt.as_deref(), Some("ship it"));
}

#[test]
fn parse_agent_and_reasoning_options_use_typed_values() {
    let cli = Cli::try_parse_from([
        "chief",
        "--agent",
        "cursor",
        "--model-reasoning-effort",
        "xhigh",
        "--prompt",
        "inspect",
    ])
    .expect("typed agent and reasoning options should parse");

    assert_eq!(cli.chief.agent, Some(CliAgentValue::CursorAgent));
    assert_eq!(
        cli.chief.model_reasoning_effort,
        Some(CliReasoningEffortValue::Xhigh)
    );
}

#[test]
fn parse_agent_wait_seconds_override() {
    let cli = Cli::try_parse_from(["chief", "--agent-wait-seconds", "45", "--prompt", "inspect"])
        .expect("agent wait option should parse");

    let overrides = cli
        .chief
        .to_config_overrides()
        .expect("agent wait override should convert");
    assert_eq!(overrides.agent_wait_seconds, Some(45));
}

#[test]
fn parse_loop_file_command() {
    let cli = Cli::try_parse_from(["chief", "loop_file", "--file", "plan.md"])
        .expect("loop_file command should parse");

    let Some(Commands::LoopFile(args)) = cli.command else {
        panic!("expected loop_file command");
    };
    assert_eq!(args.file, Some(PathBuf::from("plan.md")));
}

#[test]
fn parse_refactor_command() {
    let cli = Cli::try_parse_from(["chief", "refactor"]).expect("refactor command should parse");

    let Some(Commands::Refactor) = cli.command else {
        panic!("expected refactor command");
    };
}

#[test]
fn parse_introspection_commands() {
    let schema =
        Cli::try_parse_from(["chief", "schema", "--json"]).expect("schema command should parse");
    assert!(matches!(schema.command, Some(Commands::Schema(_))));

    let config = Cli::try_parse_from(["chief", "config", "show", "--resolved", "--json"])
        .expect("config show command should parse");
    assert!(matches!(config.command, Some(Commands::Config(_))));

    let list = Cli::try_parse_from(["chief", "list", "suites", "--json"])
        .expect("list suites command should parse");
    assert!(matches!(list.command, Some(Commands::List(_))));

    let explain = Cli::try_parse_from(["chief", "explain", "flow", "--flow", "refactor"])
        .expect("explain flow command should parse");
    assert!(matches!(explain.command, Some(Commands::Explain(_))));

    let doctor =
        Cli::try_parse_from(["chief", "doctor", "--json"]).expect("doctor command should parse");
    assert!(matches!(doctor.command, Some(Commands::Doctor(_))));
}

#[test]
fn cli_exposes_every_chief_config_key_as_a_flag() {
    let config_keys: BTreeSet<String> =
        match serde_json::to_value(ChiefConfig::default()).expect("ChiefConfig should serialize") {
            Value::Object(map) => map.keys().cloned().collect(),
            other => panic!("ChiefConfig should serialize to object, got {other:?}"),
        };

    let cli_keys: BTreeSet<String> = Cli::command()
        .get_arguments()
        .filter_map(|arg| arg.get_long())
        .map(|name| name.replace('-', "_"))
        .collect();

    let missing: Vec<String> = config_keys.difference(&cli_keys).cloned().collect();
    assert!(
        missing.is_empty(),
        "CLI is missing flags for ChiefConfig keys: {missing:?}"
    );
}

#[test]
fn cli_help_text_matches_chief_example_comments() {
    let mut command = Cli::command();
    let mut help_buffer = Vec::new();
    command
        .write_long_help(&mut help_buffer)
        .expect("long help should render");
    let help = String::from_utf8(help_buffer).expect("help should be UTF-8");
    let example_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".chief/chief.example.yaml");
    let example = fs::read_to_string(&example_path).expect("chief.example.yaml should be readable");

    for spec in chief_option_help::SPECS {
        assert!(
            help.contains(spec.help),
            "CLI help should include '{}' for key '{}'; help output:\n{}",
            spec.help,
            spec.key,
            help
        );

        let expected_comment = format!("# {}", spec.help);
        assert!(
            example.contains(&expected_comment),
            "chief.example.yaml should include comment '{}' for key '{}'; file: {}",
            expected_comment,
            spec.key,
            example_path.display()
        );

        let expected_key = format!("{}:", spec.key);
        assert!(
            example.contains(&expected_key),
            "chief.example.yaml should include key '{}'",
            spec.key
        );
    }

    assert!(
        help.contains("chief --agent cursor-agent --model gpt-5.4-xhigh --prompt"),
        "CLI help should include a concrete cursor-agent model example; help output:\n{}",
        help
    );
}

#[test]
fn cli_schema_lists_typed_values_and_introspection_commands() {
    let schema = build_cli_schema();
    let flow_option = schema
        .global_options
        .iter()
        .find(|option| option.long.as_deref() == Some("flow"))
        .expect("flow option should exist");
    assert_eq!(flow_option.possible_values, vec!["loop_file", "refactor"]);

    let agent_option = schema
        .global_options
        .iter()
        .find(|option| option.long.as_deref() == Some("agent"))
        .expect("agent option should exist");
    assert_eq!(
        agent_option.possible_values,
        vec!["codex", "claude", "opencode", "cursor-agent"]
    );

    let command_names: BTreeSet<_> = schema
        .commands
        .iter()
        .map(|command| command.name.as_str())
        .collect();
    for expected in ["schema", "config", "list", "explain", "doctor"] {
        assert!(
            command_names.contains(expected),
            "schema should list introspection command {expected}"
        );
    }
}

#[test]
fn cli_chief_overrides_parse_complex_fields() {
    let cli = Cli::try_parse_from([
        "chief",
        "--agent-extra-args",
        r#"["--sandbox","workspace-write"]"#,
        "--mcp-servers",
        "personal",
    ])
    .expect("CLI should parse complex override flags");

    let overrides = cli
        .chief
        .to_config_overrides()
        .expect("CLI overrides should convert");

    assert_eq!(
        overrides.agent_extra_args,
        Some(vec!["--sandbox".to_owned(), "workspace-write".to_owned()])
    );
    assert_eq!(overrides.mcp_servers, Some(None));
}

#[test]
fn load_chief_yaml_with_cli_overrides_does_not_require_git_context() {
    let temp = TempDir::new("load-chief-yaml-overrides");
    write_chief_yaml(&temp.path, "chief:\n  flow: loop_file\n  agent: codex\n");

    let cli = Cli {
        project_dir: temp.path.clone(),
        chief: CliChiefOverrides {
            flow: Some(CliFlowValue::Refactor),
            model: Some("gpt-5.4".to_owned()),
            ..CliChiefOverrides::default()
        },
        file: None,
        prompt: None,
        watch_only: Vec::new(),
        requirements: Vec::new(),
        requirements_file: Vec::new(),
        command: Some(Commands::Config(ConfigArgs {
            command: ConfigCommand::Show(ConfigShowArgs {
                resolved: true,
                json: false,
            }),
        })),
    };

    let chief_yaml = load_chief_yaml_with_cli_overrides(&temp.path, &cli)
        .expect("resolved config should load without git context");
    assert_eq!(chief_yaml.chief.flow, "refactor");
    assert_eq!(chief_yaml.chief.model.as_deref(), Some("gpt-5.4"));
}

#[test]
fn build_flow_explanation_describes_loop_file_inputs() {
    let explanation = build_flow_explanation(CliFlowValue::LoopFile);
    assert_eq!(explanation.flow, "loop_file");
    assert!(
        explanation
            .inputs
            .iter()
            .any(|value| value.contains("--file <path>") || value.contains("--prompt <text>")),
        "loop_file explanation should describe file/prompt inputs"
    );
    assert!(
        explanation
            .disallowed_inputs
            .iter()
            .any(|value| value.contains("Providing both --file and --prompt")),
        "loop_file explanation should document mutual exclusion"
    );
}

#[test]
fn loop_file_fails_when_input_file_is_missing() {
    let temp = TempDir::new("loop-file-missing-input");
    init_git_repo(&temp.path);
    write_chief_yaml(&temp.path, "chief: {}\n");

    let cli = Cli {
        project_dir: temp.path.clone(),
        chief: CliChiefOverrides::default(),
        file: None,
        prompt: None,
        watch_only: Vec::new(),
        requirements: Vec::new(),
        requirements_file: Vec::new(),
        command: Some(Commands::LoopFile(LoopFileArgs {
            file: Some(PathBuf::from("missing-plan.md")),
            prompt: None,
            watch_only: Vec::new(),
        })),
    };

    let err = run(&cli).expect_err("loop_file should fail for missing file");
    let rendered = err.to_string();
    assert!(
        rendered.contains("failed to read loop_file input"),
        "error should explain loop_file input read failure: {rendered}"
    );
}

#[test]
fn init_writes_full_default_chief_yaml_block() {
    let temp = TempDir::new("init-default-chief-yaml");

    let cli = Cli {
        project_dir: temp.path.clone(),
        chief: CliChiefOverrides::default(),
        file: None,
        prompt: None,
        watch_only: Vec::new(),
        requirements: Vec::new(),
        requirements_file: Vec::new(),
        command: Some(Commands::Init(InitArgs {
            chief_root: PathBuf::from("/definitely/not/a/chief/checkout"),
        })),
    };

    let args = match &cli.command {
        Some(Commands::Init(args)) => args,
        _ => panic!("expected init command"),
    };
    init_files::run_init(&cli, args).expect("init should succeed");

    let chief_yaml = fs::read_to_string(chief::paths::chief_yaml_path(&temp.path))
        .expect("chief.yaml should be readable");
    assert_eq!(chief_yaml, init_files::INIT_CHIEF_YAML_CONTENT);
    let chief_example_path = chief::paths::chief_example_path(&temp.path);
    let chief_example =
        fs::read_to_string(&chief_example_path).expect("chief.example.yaml should be readable");
    assert_eq!(chief_example, init_files::CHIEF_EXAMPLE_YAML_CONTENT);
    assert!(
        !fs::symlink_metadata(&chief_example_path)
            .expect("chief.example.yaml metadata should be readable")
            .file_type()
            .is_symlink(),
        "init should write an embedded example config instead of symlinking to a checkout"
    );
    assert!(
        !chief_yaml.contains("\n  max_retries:"),
        "init default chief.yaml should not include max_retries"
    );
    assert!(
        !temp.path.join(".chief/todos.yaml").exists(),
        "init should not create todos.yaml"
    );
    assert!(
        !chief_yaml.contains("\nsuites:"),
        "init default chief.yaml should only contain global chief options"
    );
}

#[test]
fn parse_stable_iteration_progress_handles_pending_and_converged_states() {
    assert_eq!(
        parse_stable_iteration_progress("LOOP_FILE phase stable 1/2; retrying to confirm"),
        Some(StableIterationProgress {
            current: 1,
            required: 2,
            converged: false,
        })
    );
    assert_eq!(
        parse_stable_iteration_progress("REFACTOR phase done after stable result 2/2"),
        Some(StableIterationProgress {
            current: 2,
            required: 2,
            converged: true,
        })
    );
    assert_eq!(parse_stable_iteration_progress("unrelated"), None);
}

#[test]
fn build_cli_run_report_counts_until_pass_iterations_and_uses_cached_usage_snapshot() {
    let temp = TempDir::new("run-report");
    init_git_repo(&temp.path);
    write_chief_yaml(
        &temp.path,
        "chief:\n  flow: loop_file\n  agent: unsupported\n",
    );

    let context = ProjectContext::load(&temp.path).expect("project context should load");
    let run_id = "run-report-1";
    context
        .store
        .start_run(run_id)
        .expect("run should start for report test");

    context
        .log_project_event(
            run_id,
            None,
            None,
            "info",
            Some(Phase::LoopFile),
            EventType::PhaseChange,
            "convergence loop iteration 1/50",
            BTreeMap::new(),
        )
        .expect("convergence iteration should be recorded");
    context
        .log_project_event(
            run_id,
            None,
            None,
            "info",
            Some(Phase::Refactor),
            EventType::PhaseChange,
            "until_pass loop iteration 2/50",
            BTreeMap::new(),
        )
        .expect("until_pass iteration should be recorded");
    context
        .log_project_event(
            run_id,
            None,
            None,
            "info",
            Some(Phase::LoopFile),
            EventType::PhaseChange,
            "LOOP_FILE phase stable 1/2; retrying to confirm",
            BTreeMap::new(),
        )
        .expect("stable progress should be recorded");
    context
        .log_project_event(
            run_id,
            None,
            None,
            "info",
            Some(Phase::LoopFile),
            EventType::AgentPrompt,
            "Agent prompt (loop_file)",
            BTreeMap::new(),
        )
        .expect("agent prompt should be recorded");
    context
        .log_project_event(
            run_id,
            None,
            None,
            "warning",
            Some(Phase::LoopFile),
            EventType::Lint,
            "Lint failed (backend)",
            json_object_payload(serde_json::json!({
                "exit_code": 1,
                "command": "cargo clippy",
            })),
        )
        .expect("lint event should be recorded");
    context
        .log_project_event(
            run_id,
            None,
            None,
            "info",
            Some(Phase::LoopFile),
            EventType::TestRun,
            "Test run passed (backend)",
            json_object_payload(serde_json::json!({
                "exit_code": 0,
                "command": "cargo test",
            })),
        )
        .expect("test event should be recorded");
    context
        .log_project_event(
            run_id,
            None,
            None,
            "info",
            Some(Phase::LoopFile),
            EventType::AgentCmd,
            "Agent usage limits before call",
            json_object_payload(serde_json::json!({
                "wait_seconds_applied": 12.0,
                "usage": {
                    "provider": "codex",
                    "limits": [{
                        "label": "5h limit",
                        "percent_used": 70,
                        "percent_remaining": 30,
                        "reset_info": "resets in 80 minutes"
                    }]
                }
            })),
        )
        .expect("agent usage event should be recorded");
    context
        .store
        .finish_run(run_id, RunExitStatus::Success)
        .expect("run should finish for report test");

    let report = build_cli_run_report(
        &context,
        Some(run_id),
        None,
        chrono::Utc::now(),
        RunExitStatus::Success,
        Some("fallback reason"),
    )
    .expect("report should build from persisted data");

    assert_eq!(report.iterations, 2, "both loop policies should count");
    assert_eq!(
        report.stable_progress,
        Some(StableIterationProgress {
            current: 1,
            required: 2,
            converged: false,
        })
    );
    assert_eq!(report.agent_calls, 1);
    assert_eq!(report.lint_passed, 0);
    assert_eq!(report.lint_failed, 1);
    assert_eq!(report.test_passed, 1);
    assert_eq!(report.test_failed, 0);
    assert_eq!(report.wait_seconds_applied, 12.0);
    assert_eq!(report.usage_source, Some("cached"));
    assert_eq!(
        report
            .usage_snapshot
            .as_ref()
            .expect("cached usage snapshot should be used")
            .limits[0]
            .percent_remaining,
        30
    );
}

#[test]
fn run_rejects_file_option_for_non_loop_file_flows() {
    let temp = TempDir::new("run-file-option-invalid-flow");
    init_git_repo(&temp.path);
    write_chief_yaml(&temp.path, "chief:\n  flow: refactor\n");

    let cli = Cli {
        project_dir: temp.path.clone(),
        chief: CliChiefOverrides {
            flow: Some(CliFlowValue::Refactor),
            ..CliChiefOverrides::default()
        },
        file: Some(PathBuf::from("plan.md")),
        prompt: None,
        watch_only: Vec::new(),
        requirements: Vec::new(),
        requirements_file: Vec::new(),
        command: None,
    };

    let err = run(&cli).expect_err("run should reject --file when flow is not loop_file");
    let rendered = err.to_string();
    assert!(
        rendered.contains("only supported"),
        "error should explain that --file and --prompt are flow-specific: {rendered}"
    );
}

#[test]
fn resolve_suite_command_replaces_target_placeholder() {
    let suite = suite_fixture();

    let lint_fix_default = suite_commands::resolve_suite_command(
        &suite,
        suite_commands::SuiteCliCommandKind::LintFix,
        None,
    )
    .expect("lint_fix");
    assert_eq!(lint_fix_default, "cargo fmt -- .");

    let test_init_override = suite_commands::resolve_suite_command(
        &suite,
        suite_commands::SuiteCliCommandKind::TestInit,
        Some("crate::mod"),
    )
    .expect("test_init");
    assert_eq!(test_init_override, "echo init crate::mod");
}

#[test]
fn migrate_moves_legacy_root_files_into_dot_chief() {
    let temp = TempDir::new("migrate-legacy-files");
    fs::write(temp.path.join("chief.yaml"), "chief: {}\n").expect("legacy chief.yaml should exist");
    fs::write(temp.path.join("todos.yaml"), "todos: []\n").expect("legacy todos.yaml should exist");
    fs::write(temp.path.join("chief.example.yaml"), "chief: {}\n")
        .expect("legacy chief.example.yaml should exist");
    fs::write(temp.path.join("todos.example.yaml"), "todos: []\n")
        .expect("legacy todos.example.yaml should exist");
    fs::write(temp.path.join("chief.db"), "sqlite").expect("legacy chief.db should exist");

    let cli = Cli {
        project_dir: temp.path.clone(),
        chief: CliChiefOverrides::default(),
        file: None,
        prompt: None,
        watch_only: Vec::new(),
        requirements: Vec::new(),
        requirements_file: Vec::new(),
        command: Some(Commands::Migrate),
    };

    run(&cli).expect("migrate command should succeed");

    assert!(
        !temp.path.join("chief.yaml").exists(),
        "legacy chief.yaml should be moved"
    );
    assert!(
        temp.path.join("todos.yaml").exists(),
        "legacy todos.yaml should remain untouched"
    );
    assert!(
        temp.path.join("todos.example.yaml").exists(),
        "legacy todos.example.yaml should remain untouched"
    );
    assert!(
        chief::paths::chief_yaml_path(&temp.path).exists(),
        "migrated .chief/chief.yaml should exist"
    );
    assert!(
        !temp.path.join(".chief/todos.yaml").exists(),
        "migrate should not create .chief/todos.yaml"
    );
    assert!(
        chief::paths::chief_db_path(&temp.path).exists(),
        "migrated .chief/chief.db should exist"
    );

    let gitignore = fs::read_to_string(temp.path.join(".gitignore"))
        .expect(".gitignore should be created during migrate");
    assert!(
        gitignore.contains(".chief/chief.db"),
        "migrate should add .chief/chief.db ignore entry"
    );
}

#[test]
fn git_commits_since_without_head_before_returns_recent_commits() {
    let temp = TempDir::new("git-commits-since-no-head-before");
    init_git_repo(&temp.path);
    git_commit_file(&temp.path, "one.txt", "1\n", "test commit one");
    git_commit_file(&temp.path, "two.txt", "2\n", "test commit two");

    let commits =
        git_commits_since(&temp.path, None).expect("git_commits_since should return commits");
    assert_eq!(commits.len(), 2, "all commits should be included");
    assert!(
        commits[0].contains("test commit two"),
        "latest commit should be listed first"
    );
    assert!(
        commits[1].contains("test commit one"),
        "older commit should still be included"
    );
}

#[test]
fn git_commits_since_with_head_before_filters_history() {
    let temp = TempDir::new("git-commits-since-range");
    init_git_repo(&temp.path);
    git_commit_file(&temp.path, "one.txt", "1\n", "base commit");
    let baseline_head = git_head(&temp.path);
    git_commit_file(&temp.path, "two.txt", "2\n", "new commit");

    let commits = git_commits_since(&temp.path, Some(baseline_head.as_str()))
        .expect("git_commits_since should return commits after baseline");
    assert_eq!(commits.len(), 1, "only new commits should be returned");
    assert!(
        commits[0].contains("new commit"),
        "range output should include the new commit message"
    );
}
