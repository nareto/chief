use super::*;
use chief::config::TestSuiteConfig;
use chief::domain::{EventType, Phase, RunExitStatus, TargetType};
use chief::service::ProjectContext;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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

#[cfg(unix)]
fn write_executable_script(path: &std::path::Path, content: &str) {
    fs::write(path, content).expect("script should be written");
    let mut permissions = fs::metadata(path)
        .expect("script metadata should be readable")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("script should be executable");
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
        flow: None,
        model: None,
        max_retries: None,
        file: None,
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
        flow: None,
        model: None,
        max_retries: None,
        file: None,
        requirements: Vec::new(),
        requirements_file: Vec::new(),
        command: None,
    };

    let err = run(&cli).expect_err("run should require --file when loop_file flow is selected");
    let rendered = err.to_string();
    assert!(
        rendered.contains("requires --file"),
        "error should direct users to provide --file: {rendered}"
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
fn parse_loop_file_command() {
    let cli = Cli::try_parse_from(["chief", "loop_file", "--file", "plan.md"])
        .expect("loop_file command should parse");

    let Some(Commands::LoopFile(args)) = cli.command else {
        panic!("expected loop_file command");
    };
    assert_eq!(args.file, PathBuf::from("plan.md"));
}

#[test]
fn parse_bd_command() {
    let cli = Cli::try_parse_from(["chief", "bd"]).expect("bd command should parse");

    let Some(Commands::Bd) = cli.command else {
        panic!("expected bd command");
    };
}

#[test]
fn parse_refactor_command() {
    let cli = Cli::try_parse_from(["chief", "refactor"]).expect("refactor command should parse");

    let Some(Commands::Refactor) = cli.command else {
        panic!("expected refactor command");
    };
}

#[test]
fn loop_file_fails_when_input_file_is_missing() {
    let temp = TempDir::new("loop-file-missing-input");
    init_git_repo(&temp.path);
    write_chief_yaml(&temp.path, "chief: {}\n");

    let cli = Cli {
        project_dir: temp.path.clone(),
        flow: None,
        model: None,
        max_retries: None,
        file: None,
        requirements: Vec::new(),
        requirements_file: Vec::new(),
        command: Some(Commands::LoopFile(LoopFileArgs {
            file: PathBuf::from("missing-plan.md"),
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
    let chief_root = temp.path.join("chief-root");
    let chief_root_config_dir = chief_root.join(".chief");
    fs::create_dir_all(&chief_root_config_dir).expect("chief-root dir should be created");
    fs::write(
        chief_root_config_dir.join("chief.example.yaml"),
        "chief: {}\n",
    )
    .expect("chief.example.yaml should be created");

    let cli = Cli {
        project_dir: temp.path.clone(),
        flow: None,
        model: None,
        max_retries: None,
        file: None,
        requirements: Vec::new(),
        requirements_file: Vec::new(),
        command: Some(Commands::Init(InitArgs {
            chief_root: chief_root.clone(),
            beads: false,
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

#[cfg(unix)]
#[test]
fn init_runs_bd_init_and_ignores_beads_directory() {
    let temp = TempDir::new("init-bd");
    let chief_root = temp.path.join("chief-root");
    let chief_root_config_dir = chief_root.join(".chief");
    fs::create_dir_all(&chief_root_config_dir).expect("chief-root dir should be created");
    fs::write(
        chief_root_config_dir.join("chief.example.yaml"),
        "chief: {}\n",
    )
    .expect("chief.example.yaml should be created");
    fs::write(chief_root.join("bd_AGENTS.md"), "# bd agents\n")
        .expect("bd_AGENTS.md should be created");

    let bd_args_log = temp.path.join("bd-args.log");
    let bd_stdin_log = temp.path.join("bd-stdin.log");
    let bd_script = temp.path.join("mock-bd");
    write_executable_script(
        &bd_script,
        &format!(
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" > \"{}\"\ncat > \"{}\"\nmkdir -p .beads\n",
            bd_args_log.display(),
            bd_stdin_log.display()
        ),
    );

    let cli = Cli {
        project_dir: temp.path.clone(),
        flow: None,
        model: None,
        max_retries: None,
        file: None,
        requirements: Vec::new(),
        requirements_file: Vec::new(),
        command: Some(Commands::Init(InitArgs {
            chief_root: PathBuf::from("chief-root"),
            beads: true,
        })),
    };

    let args = match &cli.command {
        Some(Commands::Init(args)) => args,
        _ => panic!("expected init command"),
    };
    init_files::run_init_with_bd_command(&cli, args, &bd_script).expect("init should succeed");

    assert!(
        temp.path.join(".beads").is_dir(),
        "init should create .beads via bd init"
    );
    assert_eq!(
        fs::read_to_string(&bd_args_log).expect("bd args log should be readable"),
        "init\n--agents-template\nchief-root/bd_AGENTS.md\n"
    );
    assert_eq!(
        fs::read_to_string(&bd_stdin_log).expect("bd stdin log should be readable"),
        "n\n"
    );

    let gitignore =
        fs::read_to_string(temp.path.join(".gitignore")).expect(".gitignore should exist");
    assert!(
        gitignore.contains(".beads"),
        "init should add .beads to .gitignore"
    );
}

#[cfg(unix)]
#[test]
fn init_skips_bd_init_when_beads_directory_already_exists() {
    let temp = TempDir::new("init-bd-skip");
    let chief_root = temp.path.join("chief-root");
    let chief_root_config_dir = chief_root.join(".chief");
    fs::create_dir_all(&chief_root_config_dir).expect("chief-root dir should be created");
    fs::write(
        chief_root_config_dir.join("chief.example.yaml"),
        "chief: {}\n",
    )
    .expect("chief.example.yaml should be created");
    fs::write(chief_root.join("bd_AGENTS.md"), "# bd agents\n")
        .expect("bd_AGENTS.md should be created");
    fs::create_dir_all(temp.path.join(".beads")).expect(".beads should exist");

    let bd_script = temp.path.join("mock-bd");
    let bd_log = temp.path.join("bd-called.log");
    write_executable_script(
        &bd_script,
        &format!(
            "#!/bin/sh\nset -eu\nprintf called > \"{}\"\nexit 1\n",
            bd_log.display()
        ),
    );

    let cli = Cli {
        project_dir: temp.path.clone(),
        flow: None,
        model: None,
        max_retries: None,
        file: None,
        requirements: Vec::new(),
        requirements_file: Vec::new(),
        command: Some(Commands::Init(InitArgs {
            chief_root: PathBuf::from("chief-root"),
            beads: true,
        })),
    };

    let args = match &cli.command {
        Some(Commands::Init(args)) => args,
        _ => panic!("expected init command"),
    };
    init_files::run_init_with_bd_command(&cli, args, &bd_script).expect("init should succeed");

    assert!(
        !bd_log.exists(),
        "init should not invoke bd init when .beads already exists"
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
            Some(Phase::Bd),
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
        flow: Some("refactor".to_owned()),
        model: None,
        max_retries: None,
        file: Some(PathBuf::from("plan.md")),
        requirements: Vec::new(),
        requirements_file: Vec::new(),
        command: None,
    };

    let err = run(&cli).expect_err("run should reject --file when flow is not loop_file");
    let rendered = err.to_string();
    assert!(
        rendered.contains("only supported"),
        "error should explain that --file is flow-specific: {rendered}"
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
        flow: None,
        model: None,
        max_retries: None,
        file: None,
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
