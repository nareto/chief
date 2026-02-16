use super::*;
use chief::config::TestSuiteConfig;
use chief::domain::TargetType;
use std::path::PathBuf;
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

#[test]
fn run_non_init_fails_fast_when_chief_yaml_is_missing() {
    let temp = TempDir::new("run-missing-chief-yaml");
    let cli = Cli {
        project_dir: temp.path.clone(),
        flow: None,
        model: None,
        max_retries: None,
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
        !temp.path.join("chief.db").exists(),
        "rejected run should not execute todo processing or create chief.db"
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
        "chief.db\nchief.example.yaml\ntodos.example.yaml\n"
    );
}

#[test]
fn ensure_gitignore_entries_appends_only_missing_entries() {
    let temp = TempDir::new("gitignore-append");
    let gitignore_path = temp.path.join(".gitignore");
    fs::write(&gitignore_path, "target/\nchief.db").expect("seed gitignore should be written");

    let changed =
        init_files::ensure_gitignore_entries(&gitignore_path, &init_files::INIT_GITIGNORE_ENTRIES)
            .expect("ok");

    assert!(changed);
    assert_eq!(
        fs::read_to_string(&gitignore_path).expect("gitignore should be readable"),
        "target/\nchief.db\nchief.example.yaml\ntodos.example.yaml\n"
    );
}

#[test]
fn ensure_gitignore_entries_is_idempotent() {
    let temp = TempDir::new("gitignore-idempotent");
    let gitignore_path = temp.path.join(".gitignore");
    fs::write(
        &gitignore_path,
        "/chief.db\n./chief.example.yaml\ntodos.example.yaml\n",
    )
    .expect("seed gitignore should be written");

    let changed =
        init_files::ensure_gitignore_entries(&gitignore_path, &init_files::INIT_GITIGNORE_ENTRIES)
            .expect("ok");

    assert!(!changed);
    assert_eq!(
        fs::read_to_string(&gitignore_path).expect("gitignore should be readable"),
        "/chief.db\n./chief.example.yaml\ntodos.example.yaml\n"
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
