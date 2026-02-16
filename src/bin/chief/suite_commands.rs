use super::{Cli, SuiteArgs, SuiteCommand};
use anyhow::{Result, bail};
use chief::config::TestSuiteConfig;
use chief::flow::{
    SuiteCommandKind, execute_suite_cleanup_command, execute_suite_command, suite_command_cwd,
    suite_command_for_kind,
};
use chief::service::ProjectContext;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Debug, Clone, Copy)]
pub(super) enum SuiteCliCommandKind {
    Test,
    TestInit,
    TestSetup,
    Lint,
    LintFix,
}

impl SuiteCliCommandKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::TestInit => "test_init",
            Self::TestSetup => "test_setup",
            Self::Lint => "lint",
            Self::LintFix => "lint_fix",
        }
    }

    fn config_field_name(self) -> &'static str {
        match self {
            Self::Test => "test_command",
            Self::TestInit => "test_init",
            Self::TestSetup => "test_setup",
            Self::Lint => "lint_command",
            Self::LintFix => "lint_fix_command",
        }
    }
}

pub(super) fn run_suite_command(cli: &Cli, args: &SuiteArgs) -> Result<()> {
    match &args.command {
        SuiteCommand::Test(args) => run_suite_command_kind(
            cli,
            &args.suite,
            args.target.as_deref(),
            SuiteCliCommandKind::Test,
        ),
        SuiteCommand::TestInit(args) => {
            run_suite_command_kind(cli, &args.suite, None, SuiteCliCommandKind::TestInit)
        }
        SuiteCommand::TestSetup(args) => {
            run_suite_command_kind(cli, &args.suite, None, SuiteCliCommandKind::TestSetup)
        }
        SuiteCommand::Lint(args) => run_suite_command_kind(
            cli,
            &args.suite,
            args.target.as_deref(),
            SuiteCliCommandKind::Lint,
        ),
        SuiteCommand::LintFix(args) => run_suite_command_kind(
            cli,
            &args.suite,
            args.target.as_deref(),
            SuiteCliCommandKind::LintFix,
        ),
    }
}

fn run_suite_command_kind(
    cli: &Cli,
    suite_name: &str,
    target_override: Option<&str>,
    kind: SuiteCliCommandKind,
) -> Result<()> {
    let mut context = ProjectContext::load(&cli.project_dir)?;
    context.refresh()?;

    let suite_name = suite_name.trim();
    if suite_name.is_empty() {
        bail!("--suite cannot be empty");
    }

    let available_suites = context
        .chief_yaml
        .suites
        .iter()
        .map(|suite| suite.name.as_str())
        .collect::<Vec<_>>();
    let suite = match context
        .chief_yaml
        .suites
        .iter()
        .find(|suite| suite.name == suite_name)
    {
        Some(suite) => suite,
        None if available_suites.is_empty() => {
            bail!(
                "no suites are configured in {}",
                context.config_path.display()
            )
        }
        None => bail!(
            "suite '{}' not found; available suites: {}",
            suite_name,
            available_suites.join(", ")
        ),
    };

    let command = resolve_suite_command(suite, kind, target_override).ok_or_else(|| {
        anyhow::anyhow!(
            "suite '{}' has no {} command configured (chief.yaml field '{}')",
            suite.name,
            kind.as_str(),
            kind.config_field_name()
        )
    })?;

    let cwd = suite_command_cwd(&context.project_dir, suite);
    if !cwd.exists() {
        bail!(
            "suite '{}' {} command working directory does not exist: {}",
            suite.name,
            kind.as_str(),
            cwd.display()
        );
    }
    if !cwd.is_dir() {
        bail!(
            "suite '{}' {} command working directory is not a directory: {}",
            suite.name,
            kind.as_str(),
            cwd.display()
        );
    }

    let timeout_seconds = suite
        .command_timeout_seconds
        .unwrap_or(context.chief_yaml.chief.suite_command_timeout_seconds)
        .max(1);
    let cancel_signal = Arc::new(AtomicBool::new(false));
    let out = execute_suite_command(
        &command,
        &cwd,
        &suite.env,
        &cancel_signal,
        Some(timeout_seconds),
    )?;

    if matches!(kind, SuiteCliCommandKind::Test) {
        match execute_suite_cleanup_command(
            suite.cleanup_command.as_deref(),
            &cwd,
            &suite.env,
            Some(timeout_seconds),
        ) {
            Ok(Some(cleanup_out)) => {
                eprintln!(
                    "cleanup_command exit_code={} command={}",
                    cleanup_out.exit_code, cleanup_out.command
                );
                if !cleanup_out.merged_output.trim().is_empty() {
                    eprintln!("{}", cleanup_out.merged_output);
                }
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("warning: cleanup_command failed to execute: {err}");
            }
        }
    }

    println!("suite: {}", suite.name);
    println!("kind: {}", kind.as_str());
    println!("cwd: {}", cwd.display());
    println!("command: {command}");
    println!("timeout_seconds: {timeout_seconds}");
    if !out.merged_output.trim().is_empty() {
        println!();
        println!("{}", out.merged_output);
    }
    println!();
    println!("exit_code: {}", out.exit_code);

    if out.exit_code != 0 {
        bail!(
            "suite '{}' {} command failed with exit code {}",
            suite.name,
            kind.as_str(),
            out.exit_code
        );
    }

    Ok(())
}

pub(super) fn resolve_suite_command(
    suite: &TestSuiteConfig,
    kind: SuiteCliCommandKind,
    target_override: Option<&str>,
) -> Option<String> {
    match kind {
        SuiteCliCommandKind::Test => {
            suite_command_for_kind(suite, SuiteCommandKind::Test, target_override)
        }
        SuiteCliCommandKind::Lint => {
            suite_command_for_kind(suite, SuiteCommandKind::Lint, target_override)
        }
        SuiteCliCommandKind::TestInit => suite
            .test_init
            .as_deref()
            .map(|command| replace_suite_target_placeholder(command, suite, target_override)),
        SuiteCliCommandKind::TestSetup => suite
            .test_setup
            .as_deref()
            .map(|command| replace_suite_target_placeholder(command, suite, target_override)),
        SuiteCliCommandKind::LintFix => suite
            .lint_fix_command
            .as_deref()
            .map(|command| replace_suite_target_placeholder(command, suite, target_override)),
    }
}

fn replace_suite_target_placeholder(
    command: &str,
    suite: &TestSuiteConfig,
    target_override: Option<&str>,
) -> String {
    let target = target_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| suite.default_target.clone())
        .unwrap_or_else(|| ".".to_owned());
    command.replace("{target}", &target)
}
