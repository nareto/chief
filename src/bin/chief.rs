#[allow(dead_code)]
mod api;
#[path = "chief/init_files.rs"]
mod init_files;
#[path = "chief/suite_commands.rs"]
mod suite_commands;
#[cfg(test)]
#[path = "chief/tests.rs"]
mod tests;

use agentusage::{ApprovalPolicy, UsageConfig, run_claude, run_codex};
use anyhow::{Context, Result, bail};
use chief::config::{ChiefConfigOverrides, ChiefYaml, McpServerConfig, TestSuiteConfig};
use chief::domain::{JobStatus, RunExitStatus, Todo, TodoStatus};
use chief::flow::FlowKind;
use chief::git::GitOps;
use chief::orchestrator::OrchestratorError;
use chief::paths;
use chief::scheduler::Scheduler;
use chief::service::{ChiefEngine, ProjectContext, ProjectRegistry};
use chief::storage::{EventQuery, ProjectStore, db_reset_required_from_anyhow};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const CHIEF_LONG_ABOUT: &str = "Chief orchestrates project flows, suite commands, and readiness checks.\n\n\
The default `chief` invocation resolves its flow from config and then runs one of two behaviors:\n\
- `loop_file`: execute a single convergence task from `--file` or `--prompt`\n\
- `refactor`: process queued todos from the SQLite project queue\n\n\
For CLI-only discovery, use `chief schema --json`, `chief config show --resolved`,\n\
`chief list suites`, `chief explain flow`, and `chief doctor`.";

const CHIEF_AFTER_HELP: &str = "Examples:
  chief --flow loop_file --prompt \"Tighten the parser error messages\"
  chief --agent cursor-agent --model gpt-5.4-xhigh --prompt \"Tighten the parser error messages\"
  chief loop_file --file prompts/task.md
  chief suite test --suite backend --target src/bin/chief.rs
  chief schema --json
  chief config show --resolved
  chief list suites --json
  chief explain flow --flow refactor
  chief doctor

Config precedence:
  defaults < .chief/chief.yaml < CLI flags

Exit codes:
  0  Success
  1  Command or runtime error";

mod chief_option_help {
    #[cfg(test)]
    #[derive(Debug, Clone, Copy)]
    pub(super) struct ChiefOptionHelpSpec {
        pub key: &'static str,
        pub help: &'static str,
    }

    pub(super) const FLOW: &str = "Flow to run (`loop_file` or `refactor`).";
    pub(super) const AGENT: &str = "Agent binary to use (`codex`, `claude`, `opencode`, or `cursor-agent`; `cursor` is also accepted).";
    pub(super) const MODEL: &str = "Model override passed to the selected agent (for `cursor-agent`, use Cursor's exact model id such as `gpt-5.4-xhigh`).";
    pub(super) const MODEL_REASONING_EFFORT: &str = "Reasoning effort for model adapters that support it (`low`, `medium`, `high`, or `xhigh`).";
    pub(super) const AGENT_EXTRA_ARGS: &str =
        "Extra CLI args forwarded to the selected agent (YAML/JSON string list).";
    pub(super) const MCP_SERVERS: &str =
        "MCP server config (`personal` or YAML/JSON object; `{}` means no servers).";
    pub(super) const MAX_RETRIES: &str =
        "Retry budget for queued todo flows (minimum effective value is 1).";
    pub(super) const MAX_LOOP_ITERATIONS: &str =
        "Maximum convergence iterations for loop-style flows.";
    pub(super) const REQUIRED_STABLE_ITERATIONS: &str =
        "Consecutive stable iterations required before convergence is done.";
    pub(super) const CHANGE_EXCLUDE: &str =
        "Additional glob patterns to exclude from convergence change detection (repeatable).";
    pub(super) const AGENT_TIMEOUT_SECONDS: &str =
        "Per-agent invocation timeout in seconds (0 disables timeout).";
    pub(super) const AGENT_WAIT_SECONDS: &str = "Fixed wait in seconds between agent calls; when set, it overrides respect_limits logic (0 means no wait).";
    pub(super) const SUITE_COMMAND_TIMEOUT_SECONDS: &str =
        "Default timeout in seconds for suite/readiness commands.";
    pub(super) const AGENT_LOG_MAX_OUTPUT_LINES: &str =
        "Tail line count kept when truncating agent output in logs.";
    pub(super) const AGENT_LOG_MAX_OUTPUT_CHARS: &str =
        "Tail character count kept when truncating agent output in logs.";
    pub(super) const RESPECT_LIMITS: &str =
        "When true, wait for agentusage limits before launching agent calls.";
    pub(super) const USE_AGENT_LOG_TRUNCATION_FOR_STDOUT_LOGS: &str =
        "When true, apply agent log truncation to stdout log output too.";

    #[cfg(test)]
    pub(super) const SPECS: [ChiefOptionHelpSpec; 17] = [
        ChiefOptionHelpSpec {
            key: "flow",
            help: FLOW,
        },
        ChiefOptionHelpSpec {
            key: "agent",
            help: AGENT,
        },
        ChiefOptionHelpSpec {
            key: "model",
            help: MODEL,
        },
        ChiefOptionHelpSpec {
            key: "model_reasoning_effort",
            help: MODEL_REASONING_EFFORT,
        },
        ChiefOptionHelpSpec {
            key: "agent_extra_args",
            help: AGENT_EXTRA_ARGS,
        },
        ChiefOptionHelpSpec {
            key: "mcp_servers",
            help: MCP_SERVERS,
        },
        ChiefOptionHelpSpec {
            key: "max_retries",
            help: MAX_RETRIES,
        },
        ChiefOptionHelpSpec {
            key: "max_loop_iterations",
            help: MAX_LOOP_ITERATIONS,
        },
        ChiefOptionHelpSpec {
            key: "required_stable_iterations",
            help: REQUIRED_STABLE_ITERATIONS,
        },
        ChiefOptionHelpSpec {
            key: "change_exclude",
            help: CHANGE_EXCLUDE,
        },
        ChiefOptionHelpSpec {
            key: "agent_timeout_seconds",
            help: AGENT_TIMEOUT_SECONDS,
        },
        ChiefOptionHelpSpec {
            key: "agent_wait_seconds",
            help: AGENT_WAIT_SECONDS,
        },
        ChiefOptionHelpSpec {
            key: "suite_command_timeout_seconds",
            help: SUITE_COMMAND_TIMEOUT_SECONDS,
        },
        ChiefOptionHelpSpec {
            key: "agent_log_max_output_lines",
            help: AGENT_LOG_MAX_OUTPUT_LINES,
        },
        ChiefOptionHelpSpec {
            key: "agent_log_max_output_chars",
            help: AGENT_LOG_MAX_OUTPUT_CHARS,
        },
        ChiefOptionHelpSpec {
            key: "respect_limits",
            help: RESPECT_LIMITS,
        },
        ChiefOptionHelpSpec {
            key: "use_agent_log_truncation_for_stdout_logs",
            help: USE_AGENT_LOG_TRUNCATION_FOR_STDOUT_LOGS,
        },
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
enum CliFlowValue {
    #[value(name = "loop_file", alias = "loop-file")]
    LoopFile,
    #[value(name = "refactor")]
    Refactor,
}

impl CliFlowValue {
    fn as_str(self) -> &'static str {
        match self {
            Self::LoopFile => "loop_file",
            Self::Refactor => "refactor",
        }
    }

    fn from_flow_kind(flow: FlowKind) -> Self {
        match flow {
            FlowKind::LoopFile => Self::LoopFile,
            FlowKind::Refactor => Self::Refactor,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
enum CliAgentValue {
    #[value(name = "codex")]
    Codex,
    #[value(name = "claude")]
    Claude,
    #[value(name = "opencode")]
    Opencode,
    #[value(name = "cursor-agent", alias = "cursor")]
    CursorAgent,
}

impl CliAgentValue {
    fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Opencode => "opencode",
            Self::CursorAgent => "cursor-agent",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
enum CliReasoningEffortValue {
    #[value(name = "low")]
    Low,
    #[value(name = "medium")]
    Medium,
    #[value(name = "high")]
    High,
    #[value(name = "xhigh")]
    Xhigh,
}

impl CliReasoningEffortValue {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
}

#[derive(Debug, Clone, Args, Default)]
struct CliChiefOverrides {
    #[arg(
        long,
        global = true,
        value_enum,
        help = chief_option_help::FLOW,
        help_heading = "Chief Overrides"
    )]
    flow: Option<CliFlowValue>,
    #[arg(
        long,
        global = true,
        value_enum,
        help = chief_option_help::AGENT,
        help_heading = "Chief Overrides"
    )]
    agent: Option<CliAgentValue>,
    #[arg(long, global = true, help = chief_option_help::MODEL, help_heading = "Chief Overrides")]
    model: Option<String>,
    #[arg(
        long,
        global = true,
        value_enum,
        help = chief_option_help::MODEL_REASONING_EFFORT,
        help_heading = "Chief Overrides"
    )]
    model_reasoning_effort: Option<CliReasoningEffortValue>,
    #[arg(
        long,
        global = true,
        help = chief_option_help::AGENT_EXTRA_ARGS,
        help_heading = "Chief Overrides"
    )]
    agent_extra_args: Option<String>,
    #[arg(
        long,
        global = true,
        help = chief_option_help::MCP_SERVERS,
        help_heading = "Chief Overrides"
    )]
    mcp_servers: Option<String>,
    #[arg(
        long,
        global = true,
        help = chief_option_help::MAX_RETRIES,
        help_heading = "Chief Overrides"
    )]
    max_retries: Option<usize>,
    #[arg(
        long,
        global = true,
        help = chief_option_help::MAX_LOOP_ITERATIONS,
        help_heading = "Chief Overrides"
    )]
    max_loop_iterations: Option<usize>,
    #[arg(
        long,
        global = true,
        help = chief_option_help::REQUIRED_STABLE_ITERATIONS,
        help_heading = "Chief Overrides"
    )]
    required_stable_iterations: Option<usize>,
    #[arg(
        long = "change-exclude",
        visible_alias = "watch-exclude",
        global = true,
        help = chief_option_help::CHANGE_EXCLUDE,
        help_heading = "Chief Overrides"
    )]
    change_exclude: Vec<String>,
    #[arg(
        long,
        global = true,
        help = chief_option_help::AGENT_TIMEOUT_SECONDS,
        help_heading = "Chief Overrides"
    )]
    agent_timeout_seconds: Option<u64>,
    #[arg(
        long,
        global = true,
        help = chief_option_help::AGENT_WAIT_SECONDS,
        help_heading = "Chief Overrides"
    )]
    agent_wait_seconds: Option<u64>,
    #[arg(
        long,
        global = true,
        help = chief_option_help::SUITE_COMMAND_TIMEOUT_SECONDS,
        help_heading = "Chief Overrides"
    )]
    suite_command_timeout_seconds: Option<u64>,
    #[arg(
        long,
        global = true,
        help = chief_option_help::AGENT_LOG_MAX_OUTPUT_LINES,
        help_heading = "Chief Overrides"
    )]
    agent_log_max_output_lines: Option<usize>,
    #[arg(
        long,
        global = true,
        help = chief_option_help::AGENT_LOG_MAX_OUTPUT_CHARS,
        help_heading = "Chief Overrides"
    )]
    agent_log_max_output_chars: Option<usize>,
    #[arg(
        long,
        global = true,
        help = chief_option_help::RESPECT_LIMITS,
        help_heading = "Chief Overrides"
    )]
    respect_limits: Option<bool>,
    #[arg(
        long,
        global = true,
        help = chief_option_help::USE_AGENT_LOG_TRUNCATION_FOR_STDOUT_LOGS,
        help_heading = "Chief Overrides"
    )]
    use_agent_log_truncation_for_stdout_logs: Option<bool>,
}

#[derive(Debug, Parser)]
#[command(name = "chief", version)]
#[command(about = "Chief orchestration CLI")]
#[command(long_about = CHIEF_LONG_ABOUT, after_help = CHIEF_AFTER_HELP)]
struct Cli {
    #[arg(long, default_value = ".", help_heading = "Global Options")]
    project_dir: PathBuf,
    #[command(flatten)]
    chief: CliChiefOverrides,
    /// Markdown file used when running flow=loop_file via the default `chief` command.
    #[arg(long, conflicts_with = "prompt", help_heading = "Loop File Inputs")]
    file: Option<PathBuf>,
    /// Prompt text used when running flow=loop_file via the default `chief` command.
    /// Mutually exclusive with --file.
    #[arg(long, conflicts_with = "file", help_heading = "Loop File Inputs")]
    prompt: Option<String>,
    /// Scope convergence to these paths only (repeatable). Ignored for non-loop_file flows.
    #[arg(long = "watch-only", help_heading = "Loop File Inputs")]
    watch_only: Vec<String>,
    /// Inline requirements text to process before any flow execution.
    #[arg(long = "requirements", help_heading = "Requirements")]
    requirements: Vec<String>,
    /// File whose contents are appended to inline requirements before processing.
    #[arg(long = "requirements-file", help_heading = "Requirements")]
    requirements_file: Vec<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Initialize Chief config files in a new project directory.
    Init(InitArgs),
    /// Move legacy root-level `chief.yaml`, `chief.example.yaml`, and `chief.db` into `.chief/`.
    Migrate,
    /// Remove completed todos that have a commit hash.
    CleanDone,
    /// Run project pre-run checks (same readiness checks used by backend start).
    Check(CheckArgs),
    /// Print recent project events.
    TailEvents(TailEventsArgs),
    /// Run suite-level commands from chief.yaml for a specific suite.
    Suite(SuiteArgs),
    /// Execute one loop_file flow run from a markdown file.
    #[command(name = "loop_file", visible_alias = "loop-file")]
    LoopFile(LoopFileArgs),
    /// Run queued todos using the refactor flow.
    Refactor,
    /// Print a self-describing schema for the CLI.
    Schema(SchemaArgs),
    /// Inspect chief.yaml content and effective overrides.
    Config(ConfigArgs),
    /// List discoverable entities from chief.yaml.
    List(ListArgs),
    /// Explain flow behavior and expected inputs.
    Explain(ExplainArgs),
    /// Validate local project and tool readiness for Chief.
    Doctor(DoctorArgs),
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Deprecated compatibility option; `chief init` uses embedded example files.
    #[arg(long, default_value = "../chief")]
    chief_root: PathBuf,
}

#[derive(Debug, Args)]
struct TailEventsArgs {
    /// Maximum number of most-recent events to print.
    #[arg(long, short = 'n', default_value_t = 50)]
    limit: usize,
}

#[derive(Debug, Args)]
struct CheckArgs {
    /// Force executing checks even when cached readiness is still valid.
    #[arg(long, default_value_t = false)]
    force: bool,
}

#[derive(Debug, Args)]
struct SuiteArgs {
    #[command(subcommand)]
    command: SuiteCommand,
}

#[derive(Debug, Subcommand)]
enum SuiteCommand {
    /// Run the suite test command.
    Test(SuiteRunArgs),
    /// Run the suite test_init command.
    #[command(name = "test_init", visible_alias = "test-init")]
    TestInit(SuiteRunNoTargetArgs),
    /// Run the suite test_setup command.
    #[command(name = "test_setup", visible_alias = "test-setup")]
    TestSetup(SuiteRunNoTargetArgs),
    /// Run the suite lint command.
    #[command(visible_alias = "linting")]
    Lint(SuiteRunArgs),
    /// Run the suite lint_fix command.
    #[command(
        name = "lint_fix",
        visible_alias = "lint-fix",
        visible_alias = "linting_fix",
        visible_alias = "linting-fix"
    )]
    LintFix(SuiteRunArgs),
}

#[derive(Debug, Args)]
struct SuiteRunArgs {
    /// Suite name as configured in chief.yaml.
    #[arg(long)]
    suite: String,
    /// Optional target value used for {target} placeholder replacement.
    #[arg(long)]
    target: Option<String>,
}

#[derive(Debug, Args)]
struct SuiteRunNoTargetArgs {
    /// Suite name as configured in chief.yaml.
    #[arg(long)]
    suite: String,
}

#[derive(Debug, Args)]
struct LoopFileArgs {
    /// Markdown file path to load as the loop_file task body.
    #[arg(long, conflicts_with = "prompt")]
    file: Option<PathBuf>,
    /// Prompt text for the loop_file task body.
    /// Mutually exclusive with --file.
    #[arg(long, conflicts_with = "file")]
    prompt: Option<String>,
    /// Scope convergence to these paths only (repeatable). When set, an iteration
    /// is considered stable only if none of the specified paths were modified.
    #[arg(long = "watch-only")]
    watch_only: Vec<String>,
}

#[derive(Debug, Args)]
struct SchemaArgs {
    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long, default_value_t = false)]
    json: bool,
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Show raw or resolved chief.yaml content.
    Show(ConfigShowArgs),
}

#[derive(Debug, Args)]
struct ConfigShowArgs {
    /// Render defaults + chief.yaml + CLI overrides instead of the raw file content.
    #[arg(long, default_value_t = false)]
    resolved: bool,
    /// Emit machine-readable JSON instead of YAML/text.
    #[arg(long, default_value_t = false)]
    json: bool,
}

#[derive(Debug, Args)]
struct ListArgs {
    #[command(subcommand)]
    command: ListCommand,
}

#[derive(Debug, Subcommand)]
enum ListCommand {
    /// List suites defined in chief.yaml.
    Suites(ListSuitesArgs),
}

#[derive(Debug, Args)]
struct ListSuitesArgs {
    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long, default_value_t = false)]
    json: bool,
}

#[derive(Debug, Args)]
struct ExplainArgs {
    #[command(subcommand)]
    command: ExplainCommand,
}

#[derive(Debug, Subcommand)]
enum ExplainCommand {
    /// Explain how a flow behaves and which inputs it expects.
    Flow(ExplainFlowArgs),
}

#[derive(Debug, Args)]
struct ExplainFlowArgs {
    /// Flow to describe. Defaults to the resolved flow for this project.
    #[arg(long, value_enum)]
    flow: Option<CliFlowValue>,
    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long, default_value_t = false)]
    json: bool,
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long, default_value_t = false)]
    json: bool,
}

impl CliChiefOverrides {
    fn to_config_overrides(&self) -> Result<ChiefConfigOverrides> {
        let Self {
            flow,
            agent,
            model,
            model_reasoning_effort,
            agent_extra_args,
            mcp_servers,
            max_retries,
            max_loop_iterations,
            required_stable_iterations,
            change_exclude,
            agent_timeout_seconds,
            agent_wait_seconds,
            suite_command_timeout_seconds,
            agent_log_max_output_lines,
            agent_log_max_output_chars,
            respect_limits,
            use_agent_log_truncation_for_stdout_logs,
        } = self.clone();

        let agent_extra_args = agent_extra_args
            .as_deref()
            .map(parse_agent_extra_args_override)
            .transpose()?;
        let mcp_servers = mcp_servers
            .as_deref()
            .map(parse_mcp_servers_override)
            .transpose()?;

        Ok(ChiefConfigOverrides {
            flow: flow.map(|value| value.as_str().to_owned()),
            agent: agent.map(|value| value.as_str().to_owned()),
            model,
            model_reasoning_effort: model_reasoning_effort.map(|value| value.as_str().to_owned()),
            agent_extra_args,
            mcp_servers,
            max_retries,
            max_loop_iterations,
            required_stable_iterations,
            change_exclude,
            agent_timeout_seconds,
            agent_wait_seconds,
            suite_command_timeout_seconds,
            agent_log_max_output_lines,
            agent_log_max_output_chars,
            respect_limits,
            use_agent_log_truncation_for_stdout_logs,
        })
    }
}

fn parse_agent_extra_args_override(raw: &str) -> Result<Vec<String>> {
    serde_yaml::from_str::<Vec<String>>(raw).with_context(
        || "--agent-extra-args must be a YAML/JSON string list (for example: [] or [\"--foo\"])",
    )
}

fn parse_mcp_servers_override(raw: &str) -> Result<Option<BTreeMap<String, McpServerConfig>>> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("personal") {
        return Ok(None);
    }

    serde_yaml::from_str::<BTreeMap<String, McpServerConfig>>(trimmed)
        .map(Some)
        .with_context(|| {
            "--mcp-servers must be `personal` or a YAML/JSON object (for example: {} or {docs: {transport: stdio, command: npx}})"
        })
}

fn apply_cli_overrides_to_context(context: &mut ProjectContext, cli: &Cli) -> Result<()> {
    let overrides = cli.chief.to_config_overrides()?;
    let current = std::mem::take(&mut context.chief_yaml.chief);
    context.chief_yaml.chief = current.apply_overrides(overrides);
    Ok(())
}

fn load_chief_yaml_with_cli_overrides(project_dir: &Path, cli: &Cli) -> Result<ChiefYaml> {
    let config_path = paths::chief_yaml_path(project_dir);
    let mut chief_yaml = ChiefYaml::load_or_default(&config_path)?;
    let overrides = cli.chief.to_config_overrides()?;
    let current = std::mem::take(&mut chief_yaml.chief);
    chief_yaml.chief = current.apply_overrides(overrides);
    Ok(chief_yaml)
}

fn load_context_with_cli_overrides(project_dir: &Path, cli: &Cli) -> Result<ProjectContext> {
    let mut context = ProjectContext::load(project_dir)?;
    apply_cli_overrides_to_context(&mut context, cli)?;
    Ok(context)
}

#[derive(Debug, Clone, Serialize)]
struct SchemaOption {
    long: Option<String>,
    short: Option<String>,
    value_name: Option<String>,
    help: String,
    required: bool,
    repeatable: bool,
    default: Option<String>,
    possible_values: Vec<String>,
    conflicts_with: Vec<String>,
    notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SchemaCommand {
    name: String,
    aliases: Vec<String>,
    about: String,
    options: Vec<SchemaOption>,
    subcommands: Vec<SchemaCommand>,
    notes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SchemaExitCode {
    code: i32,
    meaning: String,
}

#[derive(Debug, Serialize)]
struct CliSchema {
    name: String,
    about: String,
    long_about: String,
    config_precedence: Vec<String>,
    examples: Vec<String>,
    exit_codes: Vec<SchemaExitCode>,
    global_options: Vec<SchemaOption>,
    commands: Vec<SchemaCommand>,
}

#[derive(Debug, Serialize)]
struct SuiteSummary {
    name: String,
    language: String,
    framework: String,
    test_root: String,
    target_type: String,
    default_target: Option<String>,
    test_command: String,
    test_init: Option<String>,
    test_setup: Option<String>,
    lint_command: Option<String>,
    lint_fix_command: Option<String>,
    cache_mode: String,
    command_timeout_seconds: Option<u64>,
}

impl From<&TestSuiteConfig> for SuiteSummary {
    fn from(suite: &TestSuiteConfig) -> Self {
        Self {
            name: suite.name.clone(),
            language: suite.language.clone(),
            framework: suite.framework.clone(),
            test_root: suite.test_root.clone(),
            target_type: match suite.target_type {
                chief::domain::TargetType::File => "file",
                chief::domain::TargetType::Package => "package",
                chief::domain::TargetType::Project => "project",
                chief::domain::TargetType::Repo => "repo",
            }
            .to_owned(),
            default_target: suite.default_target.clone(),
            test_command: suite.test_command.clone(),
            test_init: suite.test_init.clone(),
            test_setup: suite.test_setup.clone(),
            lint_command: suite.lint_command.clone(),
            lint_fix_command: suite.lint_fix_command.clone(),
            cache_mode: suite.cache_mode.as_str().to_owned(),
            command_timeout_seconds: suite.command_timeout_seconds,
        }
    }
}

#[derive(Debug, Serialize)]
struct FlowExplanation {
    flow: String,
    summary: String,
    execution_model: String,
    inputs: Vec<String>,
    disallowed_inputs: Vec<String>,
    prompt_sources: Vec<String>,
    examples: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DoctorCheck {
    name: String,
    status: String,
    detail: String,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    project_dir: String,
    config_path: String,
    overall_status: String,
    checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    fn has_failures(&self) -> bool {
        self.checks.iter().any(|check| check.status == "fail")
    }
}

fn schema_option(
    long: Option<&str>,
    short: Option<char>,
    value_name: Option<&str>,
    help: &str,
    required: bool,
    repeatable: bool,
    default: Option<&str>,
    possible_values: Vec<String>,
    conflicts_with: &[&str],
    notes: &[&str],
) -> SchemaOption {
    SchemaOption {
        long: long.map(str::to_owned),
        short: short.map(|value| value.to_string()),
        value_name: value_name.map(str::to_owned),
        help: help.to_owned(),
        required,
        repeatable,
        default: default.map(str::to_owned),
        possible_values,
        conflicts_with: conflicts_with
            .iter()
            .map(|value| (*value).to_owned())
            .collect(),
        notes: notes.iter().map(|value| (*value).to_owned()).collect(),
    }
}

fn enum_possible_values<T: ValueEnum>() -> Vec<String> {
    T::value_variants()
        .iter()
        .filter_map(|value| value.to_possible_value())
        .map(|value| value.get_name().to_owned())
        .collect()
}

fn bool_possible_values() -> Vec<String> {
    vec!["true".to_owned(), "false".to_owned()]
}

fn build_cli_schema() -> CliSchema {
    let global_options = vec![
        schema_option(
            Some("project-dir"),
            None,
            Some("PROJECT_DIR"),
            "Project directory to inspect or run.",
            false,
            false,
            Some("."),
            Vec::new(),
            &[],
            &["Relative paths are resolved from the current working directory."],
        ),
        schema_option(
            Some("flow"),
            None,
            Some("FLOW"),
            chief_option_help::FLOW,
            false,
            false,
            None,
            enum_possible_values::<CliFlowValue>(),
            &[],
            &["When omitted, the flow comes from chief.yaml or defaults."],
        ),
        schema_option(
            Some("agent"),
            None,
            Some("AGENT"),
            chief_option_help::AGENT,
            false,
            false,
            None,
            enum_possible_values::<CliAgentValue>(),
            &[],
            &[],
        ),
        schema_option(
            Some("model"),
            None,
            Some("MODEL"),
            chief_option_help::MODEL,
            false,
            false,
            None,
            Vec::new(),
            &[],
            &[],
        ),
        schema_option(
            Some("model-reasoning-effort"),
            None,
            Some("MODEL_REASONING_EFFORT"),
            chief_option_help::MODEL_REASONING_EFFORT,
            false,
            false,
            None,
            enum_possible_values::<CliReasoningEffortValue>(),
            &[],
            &[],
        ),
        schema_option(
            Some("agent-extra-args"),
            None,
            Some("AGENT_EXTRA_ARGS"),
            chief_option_help::AGENT_EXTRA_ARGS,
            false,
            false,
            None,
            Vec::new(),
            &[],
            &["Accepts a YAML/JSON string list such as [] or [\"--sandbox\",\"workspace-write\"]."],
        ),
        schema_option(
            Some("mcp-servers"),
            None,
            Some("MCP_SERVERS"),
            chief_option_help::MCP_SERVERS,
            false,
            false,
            None,
            vec!["personal".to_owned()],
            &[],
            &["Structured values accept a YAML/JSON object describing MCP server definitions."],
        ),
        schema_option(
            Some("max-retries"),
            None,
            Some("MAX_RETRIES"),
            chief_option_help::MAX_RETRIES,
            false,
            false,
            None,
            Vec::new(),
            &[],
            &[],
        ),
        schema_option(
            Some("max-loop-iterations"),
            None,
            Some("MAX_LOOP_ITERATIONS"),
            chief_option_help::MAX_LOOP_ITERATIONS,
            false,
            false,
            None,
            Vec::new(),
            &[],
            &[],
        ),
        schema_option(
            Some("required-stable-iterations"),
            None,
            Some("REQUIRED_STABLE_ITERATIONS"),
            chief_option_help::REQUIRED_STABLE_ITERATIONS,
            false,
            false,
            None,
            Vec::new(),
            &[],
            &[],
        ),
        schema_option(
            Some("change-exclude"),
            None,
            Some("CHANGE_EXCLUDE"),
            chief_option_help::CHANGE_EXCLUDE,
            false,
            true,
            None,
            Vec::new(),
            &[],
            &[
                "Built-in excludes always ignore Chief SQLite state such as `.chief/chief.db` and `.chief/chief.db-*`.",
                "`--watch-exclude` is accepted as an alias.",
            ],
        ),
        schema_option(
            Some("agent-timeout-seconds"),
            None,
            Some("AGENT_TIMEOUT_SECONDS"),
            chief_option_help::AGENT_TIMEOUT_SECONDS,
            false,
            false,
            None,
            Vec::new(),
            &[],
            &[],
        ),
        schema_option(
            Some("agent-wait-seconds"),
            None,
            Some("AGENT_WAIT_SECONDS"),
            chief_option_help::AGENT_WAIT_SECONDS,
            false,
            false,
            None,
            Vec::new(),
            &[],
            &["When present, Chief skips agentusage-based respect_limits pacing."],
        ),
        schema_option(
            Some("suite-command-timeout-seconds"),
            None,
            Some("SUITE_COMMAND_TIMEOUT_SECONDS"),
            chief_option_help::SUITE_COMMAND_TIMEOUT_SECONDS,
            false,
            false,
            None,
            Vec::new(),
            &[],
            &[],
        ),
        schema_option(
            Some("agent-log-max-output-lines"),
            None,
            Some("AGENT_LOG_MAX_OUTPUT_LINES"),
            chief_option_help::AGENT_LOG_MAX_OUTPUT_LINES,
            false,
            false,
            None,
            Vec::new(),
            &[],
            &[],
        ),
        schema_option(
            Some("agent-log-max-output-chars"),
            None,
            Some("AGENT_LOG_MAX_OUTPUT_CHARS"),
            chief_option_help::AGENT_LOG_MAX_OUTPUT_CHARS,
            false,
            false,
            None,
            Vec::new(),
            &[],
            &[],
        ),
        schema_option(
            Some("respect-limits"),
            None,
            Some("RESPECT_LIMITS"),
            chief_option_help::RESPECT_LIMITS,
            false,
            false,
            None,
            bool_possible_values(),
            &[],
            &[],
        ),
        schema_option(
            Some("use-agent-log-truncation-for-stdout-logs"),
            None,
            Some("USE_AGENT_LOG_TRUNCATION_FOR_STDOUT_LOGS"),
            chief_option_help::USE_AGENT_LOG_TRUNCATION_FOR_STDOUT_LOGS,
            false,
            false,
            None,
            bool_possible_values(),
            &[],
            &[],
        ),
        schema_option(
            Some("file"),
            None,
            Some("FILE"),
            "Markdown file used when running flow=loop_file via the default `chief` command.",
            false,
            false,
            None,
            Vec::new(),
            &["prompt"],
            &["Only valid when the resolved flow is `loop_file`."],
        ),
        schema_option(
            Some("prompt"),
            None,
            Some("PROMPT"),
            "Prompt text used when running flow=loop_file via the default `chief` command.",
            false,
            false,
            None,
            Vec::new(),
            &["file"],
            &["Only valid when the resolved flow is `loop_file`."],
        ),
        schema_option(
            Some("watch-only"),
            None,
            Some("WATCH_ONLY"),
            "Scope convergence to these paths only (repeatable). Ignored for non-loop_file flows.",
            false,
            true,
            None,
            Vec::new(),
            &[],
            &["A stability pass only counts when none of the watched paths changed."],
        ),
        schema_option(
            Some("requirements"),
            None,
            Some("REQUIREMENTS"),
            "Inline requirements text to process before any flow execution.",
            false,
            true,
            None,
            Vec::new(),
            &[],
            &[
                "If any requirements are provided, Chief processes requirements and exits without running a flow.",
            ],
        ),
        schema_option(
            Some("requirements-file"),
            None,
            Some("REQUIREMENTS_FILE"),
            "File whose contents are appended to inline requirements before processing.",
            false,
            true,
            None,
            Vec::new(),
            &[],
            &["The file content is appended after inline `--requirements` chunks."],
        ),
    ];

    let suite_local_options = vec![
        schema_option(
            Some("suite"),
            None,
            Some("SUITE"),
            "Suite name as configured in chief.yaml.",
            true,
            false,
            None,
            Vec::new(),
            &[],
            &[],
        ),
        schema_option(
            Some("target"),
            None,
            Some("TARGET"),
            "Optional target value used for {target} placeholder replacement.",
            false,
            false,
            None,
            Vec::new(),
            &[],
            &[],
        ),
    ];

    CliSchema {
        name: "chief".to_owned(),
        about: "Chief orchestration CLI".to_owned(),
        long_about: CHIEF_LONG_ABOUT.to_owned(),
        config_precedence: vec![
            "defaults".to_owned(),
            ".chief/chief.yaml".to_owned(),
            "CLI flags".to_owned(),
        ],
        examples: vec![
            "chief --flow loop_file --prompt \"Tighten the parser error messages\"".to_owned(),
            "chief loop_file --file prompts/task.md".to_owned(),
            "chief suite test --suite backend --target src/bin/chief.rs".to_owned(),
            "chief schema --json".to_owned(),
            "chief config show --resolved".to_owned(),
            "chief list suites --json".to_owned(),
            "chief explain flow --flow refactor".to_owned(),
            "chief doctor".to_owned(),
        ],
        exit_codes: vec![
            SchemaExitCode {
                code: 0,
                meaning: "Success".to_owned(),
            },
            SchemaExitCode {
                code: 1,
                meaning: "Command or runtime error".to_owned(),
            },
        ],
        global_options,
        commands: vec![
            SchemaCommand {
                name: "init".to_owned(),
                aliases: Vec::new(),
                about: "Initialize Chief config files in a new project directory.".to_owned(),
                options: vec![schema_option(
                    Some("chief-root"),
                    None,
                    Some("CHIEF_ROOT"),
                    "Deprecated compatibility option; `chief init` uses embedded example files.",
                    false,
                    false,
                    Some("../chief"),
                    Vec::new(),
                    &[],
                    &[],
                )],
                subcommands: Vec::new(),
                notes: vec!["Global override flags are also accepted.".to_owned()],
            },
            SchemaCommand {
                name: "migrate".to_owned(),
                aliases: Vec::new(),
                about: "Move legacy root-level chief files into .chief/.".to_owned(),
                options: Vec::new(),
                subcommands: Vec::new(),
                notes: Vec::new(),
            },
            SchemaCommand {
                name: "clean-done".to_owned(),
                aliases: Vec::new(),
                about: "Remove completed todos that have a commit hash.".to_owned(),
                options: Vec::new(),
                subcommands: Vec::new(),
                notes: Vec::new(),
            },
            SchemaCommand {
                name: "check".to_owned(),
                aliases: Vec::new(),
                about: "Run project pre-run checks.".to_owned(),
                options: vec![schema_option(
                    Some("force"),
                    None,
                    Some("FORCE"),
                    "Force executing checks even when cached readiness is still valid.",
                    false,
                    false,
                    Some("false"),
                    bool_possible_values(),
                    &[],
                    &[],
                )],
                subcommands: Vec::new(),
                notes: vec!["Global override flags are also accepted.".to_owned()],
            },
            SchemaCommand {
                name: "tail-events".to_owned(),
                aliases: Vec::new(),
                about: "Print recent project events.".to_owned(),
                options: vec![schema_option(
                    Some("limit"),
                    Some('n'),
                    Some("LIMIT"),
                    "Maximum number of most-recent events to print.",
                    false,
                    false,
                    Some("50"),
                    Vec::new(),
                    &[],
                    &[],
                )],
                subcommands: Vec::new(),
                notes: Vec::new(),
            },
            SchemaCommand {
                name: "suite".to_owned(),
                aliases: Vec::new(),
                about: "Run suite-level commands from chief.yaml for a specific suite.".to_owned(),
                options: Vec::new(),
                subcommands: vec![
                    SchemaCommand {
                        name: "test".to_owned(),
                        aliases: Vec::new(),
                        about: "Run the suite test command.".to_owned(),
                        options: suite_local_options.clone(),
                        subcommands: Vec::new(),
                        notes: vec!["Global override flags are also accepted.".to_owned()],
                    },
                    SchemaCommand {
                        name: "test_init".to_owned(),
                        aliases: vec!["test-init".to_owned()],
                        about: "Run the suite test_init command.".to_owned(),
                        options: vec![schema_option(
                            Some("suite"),
                            None,
                            Some("SUITE"),
                            "Suite name as configured in chief.yaml.",
                            true,
                            false,
                            None,
                            Vec::new(),
                            &[],
                            &[],
                        )],
                        subcommands: Vec::new(),
                        notes: vec!["Global override flags are also accepted.".to_owned()],
                    },
                    SchemaCommand {
                        name: "test_setup".to_owned(),
                        aliases: vec!["test-setup".to_owned()],
                        about: "Run the suite test_setup command.".to_owned(),
                        options: vec![schema_option(
                            Some("suite"),
                            None,
                            Some("SUITE"),
                            "Suite name as configured in chief.yaml.",
                            true,
                            false,
                            None,
                            Vec::new(),
                            &[],
                            &[],
                        )],
                        subcommands: Vec::new(),
                        notes: vec!["Global override flags are also accepted.".to_owned()],
                    },
                    SchemaCommand {
                        name: "lint".to_owned(),
                        aliases: vec!["linting".to_owned()],
                        about: "Run the suite lint command.".to_owned(),
                        options: suite_local_options.clone(),
                        subcommands: Vec::new(),
                        notes: vec!["Global override flags are also accepted.".to_owned()],
                    },
                    SchemaCommand {
                        name: "lint_fix".to_owned(),
                        aliases: vec![
                            "lint-fix".to_owned(),
                            "linting_fix".to_owned(),
                            "linting-fix".to_owned(),
                        ],
                        about: "Run the suite lint_fix command.".to_owned(),
                        options: suite_local_options,
                        subcommands: Vec::new(),
                        notes: vec!["Global override flags are also accepted.".to_owned()],
                    },
                ],
                notes: Vec::new(),
            },
            SchemaCommand {
                name: "loop_file".to_owned(),
                aliases: vec!["loop-file".to_owned()],
                about: "Execute one loop_file flow run from a markdown file.".to_owned(),
                options: vec![
                    schema_option(
                        Some("file"),
                        None,
                        Some("FILE"),
                        "Markdown file path to load as the loop_file task body.",
                        false,
                        false,
                        None,
                        Vec::new(),
                        &["prompt"],
                        &[],
                    ),
                    schema_option(
                        Some("prompt"),
                        None,
                        Some("PROMPT"),
                        "Prompt text for the loop_file task body.",
                        false,
                        false,
                        None,
                        Vec::new(),
                        &["file"],
                        &[],
                    ),
                    schema_option(
                        Some("watch-only"),
                        None,
                        Some("WATCH_ONLY"),
                        "Scope convergence to these paths only (repeatable).",
                        false,
                        true,
                        None,
                        Vec::new(),
                        &[],
                        &["Stability only counts when none of the watched paths changed."],
                    ),
                ],
                subcommands: Vec::new(),
                notes: vec!["Global override flags are also accepted.".to_owned()],
            },
            SchemaCommand {
                name: "refactor".to_owned(),
                aliases: Vec::new(),
                about: "Run queued todos using the refactor flow.".to_owned(),
                options: Vec::new(),
                subcommands: Vec::new(),
                notes: vec!["Global override flags are also accepted.".to_owned()],
            },
            SchemaCommand {
                name: "schema".to_owned(),
                aliases: Vec::new(),
                about: "Print a self-describing schema for the CLI.".to_owned(),
                options: vec![schema_option(
                    Some("json"),
                    None,
                    Some("JSON"),
                    "Emit machine-readable JSON instead of human-readable text.",
                    false,
                    false,
                    Some("false"),
                    bool_possible_values(),
                    &[],
                    &[],
                )],
                subcommands: Vec::new(),
                notes: Vec::new(),
            },
            SchemaCommand {
                name: "config".to_owned(),
                aliases: Vec::new(),
                about: "Inspect chief.yaml content and effective overrides.".to_owned(),
                options: Vec::new(),
                subcommands: vec![SchemaCommand {
                    name: "show".to_owned(),
                    aliases: Vec::new(),
                    about: "Show raw or resolved chief.yaml content.".to_owned(),
                    options: vec![
                        schema_option(
                            Some("resolved"),
                            None,
                            Some("RESOLVED"),
                            "Render defaults + chief.yaml + CLI overrides instead of the raw file content.",
                            false,
                            false,
                            Some("false"),
                            bool_possible_values(),
                            &[],
                            &[],
                        ),
                        schema_option(
                            Some("json"),
                            None,
                            Some("JSON"),
                            "Emit machine-readable JSON instead of YAML/text.",
                            false,
                            false,
                            Some("false"),
                            bool_possible_values(),
                            &[],
                            &[],
                        ),
                    ],
                    subcommands: Vec::new(),
                    notes: vec!["Global override flags are also accepted.".to_owned()],
                }],
                notes: Vec::new(),
            },
            SchemaCommand {
                name: "list".to_owned(),
                aliases: Vec::new(),
                about: "List discoverable entities from chief.yaml.".to_owned(),
                options: Vec::new(),
                subcommands: vec![SchemaCommand {
                    name: "suites".to_owned(),
                    aliases: Vec::new(),
                    about: "List suites defined in chief.yaml.".to_owned(),
                    options: vec![schema_option(
                        Some("json"),
                        None,
                        Some("JSON"),
                        "Emit machine-readable JSON instead of human-readable text.",
                        false,
                        false,
                        Some("false"),
                        bool_possible_values(),
                        &[],
                        &[],
                    )],
                    subcommands: Vec::new(),
                    notes: vec!["Global override flags are also accepted.".to_owned()],
                }],
                notes: Vec::new(),
            },
            SchemaCommand {
                name: "explain".to_owned(),
                aliases: Vec::new(),
                about: "Explain flow behavior and expected inputs.".to_owned(),
                options: Vec::new(),
                subcommands: vec![SchemaCommand {
                    name: "flow".to_owned(),
                    aliases: Vec::new(),
                    about: "Explain how a flow behaves and which inputs it expects.".to_owned(),
                    options: vec![
                        schema_option(
                            Some("flow"),
                            None,
                            Some("FLOW"),
                            "Flow to describe. Defaults to the resolved flow for this project.",
                            false,
                            false,
                            None,
                            enum_possible_values::<CliFlowValue>(),
                            &[],
                            &[],
                        ),
                        schema_option(
                            Some("json"),
                            None,
                            Some("JSON"),
                            "Emit machine-readable JSON instead of human-readable text.",
                            false,
                            false,
                            Some("false"),
                            bool_possible_values(),
                            &[],
                            &[],
                        ),
                    ],
                    subcommands: Vec::new(),
                    notes: vec!["Global override flags are also accepted.".to_owned()],
                }],
                notes: Vec::new(),
            },
            SchemaCommand {
                name: "doctor".to_owned(),
                aliases: Vec::new(),
                about: "Validate local project and tool readiness for Chief.".to_owned(),
                options: vec![schema_option(
                    Some("json"),
                    None,
                    Some("JSON"),
                    "Emit machine-readable JSON instead of human-readable text.",
                    false,
                    false,
                    Some("false"),
                    bool_possible_values(),
                    &[],
                    &[],
                )],
                subcommands: Vec::new(),
                notes: vec!["Global override flags are also accepted.".to_owned()],
            },
        ],
    }
}

fn render_schema_text(schema: &CliSchema) -> String {
    let mut lines = Vec::new();
    lines.push(format!("{}: {}", schema.name, schema.about));
    lines.push(String::new());
    lines.push("Config precedence:".to_owned());
    for step in &schema.config_precedence {
        lines.push(format!("  - {step}"));
    }
    lines.push(String::new());
    lines.push("Global options:".to_owned());
    for option in &schema.global_options {
        let mut line = format!("  --{}", option.long.as_deref().unwrap_or_default());
        if let Some(value_name) = &option.value_name {
            line.push(' ');
            line.push('<');
            line.push_str(value_name);
            line.push('>');
        }
        line.push_str(": ");
        line.push_str(&option.help);
        if !option.possible_values.is_empty() {
            line.push_str(" Possible values: ");
            line.push_str(&option.possible_values.join(", "));
            line.push('.');
        }
        lines.push(line);
    }
    lines.push(String::new());
    lines.push("Commands:".to_owned());
    for command in &schema.commands {
        let mut command_line = format!("  {}", command.name);
        if !command.aliases.is_empty() {
            command_line.push_str(" (aliases: ");
            command_line.push_str(&command.aliases.join(", "));
            command_line.push(')');
        }
        command_line.push_str(": ");
        command_line.push_str(&command.about);
        lines.push(command_line);
        for subcommand in &command.subcommands {
            let mut subcommand_line = format!("    {} {}", command.name, subcommand.name);
            if !subcommand.aliases.is_empty() {
                subcommand_line.push_str(" (aliases: ");
                subcommand_line.push_str(&subcommand.aliases.join(", "));
                subcommand_line.push(')');
            }
            subcommand_line.push_str(": ");
            subcommand_line.push_str(&subcommand.about);
            lines.push(subcommand_line);
        }
    }
    lines.join("\n")
}

fn build_flow_explanation(flow: CliFlowValue) -> FlowExplanation {
    match flow {
        CliFlowValue::LoopFile => FlowExplanation {
            flow: flow.as_str().to_owned(),
            summary: "Run a single convergence task from inline prompt text or a markdown file."
                .to_owned(),
            execution_model:
                "Chief creates a synthetic todo, runs loop_file convergence, and prints a run report."
                    .to_owned(),
            inputs: vec![
                "--file <path> or --prompt <text>".to_owned(),
                "--watch-only <path> (repeatable)".to_owned(),
                "--change-exclude <glob> (repeatable)".to_owned(),
            ],
            disallowed_inputs: vec![
                "Providing both --file and --prompt together.".to_owned(),
                "Using root-level --file/--prompt when the resolved flow is not loop_file."
                    .to_owned(),
            ],
            prompt_sources: vec![
                "The provided markdown file content.".to_owned(),
                "The provided CLI prompt text.".to_owned(),
            ],
            examples: vec![
                "chief loop_file --file prompts/task.md".to_owned(),
                "chief --flow loop_file --prompt \"Refine the API error handling\"".to_owned(),
            ],
        },
        CliFlowValue::Refactor => FlowExplanation {
            flow: flow.as_str().to_owned(),
            summary: "Run queued todos using the refactor flow.".to_owned(),
            execution_model:
                "Chief claims pending todos from storage, runs them with retry logic, and prints a run report."
                    .to_owned(),
            inputs: vec!["No flow-specific CLI inputs.".to_owned()],
            disallowed_inputs: vec![
                "--file and --prompt are not used by refactor.".to_owned(),
                "--watch-only is ignored by refactor.".to_owned(),
            ],
            prompt_sources: vec!["Queued todos from storage/todos state.".to_owned()],
            examples: vec!["chief refactor".to_owned(), "chief --flow refactor".to_owned()],
        },
    }
}

fn resolve_project_dir(project_dir: &Path) -> Result<PathBuf> {
    if project_dir.is_absolute() {
        return Ok(project_dir.to_path_buf());
    }

    Ok(std::env::current_dir()
        .context("failed resolving current directory for --project-dir")?
        .join(project_dir))
}

fn ensure_project_dir_exists(project_dir: &Path) -> Result<()> {
    if !project_dir.exists() {
        bail!(
            "project directory does not exist: {}",
            project_dir.display()
        );
    }
    if !project_dir.is_dir() {
        bail!("project path is not a directory: {}", project_dir.display());
    }
    Ok(())
}

fn active_override_names(overrides: &CliChiefOverrides) -> Vec<&'static str> {
    let override_fields = [
        ("flow", overrides.flow.is_some()),
        ("agent", overrides.agent.is_some()),
        ("model", overrides.model.is_some()),
        (
            "model_reasoning_effort",
            overrides.model_reasoning_effort.is_some(),
        ),
        ("agent_extra_args", overrides.agent_extra_args.is_some()),
        ("mcp_servers", overrides.mcp_servers.is_some()),
        ("max_retries", overrides.max_retries.is_some()),
        (
            "max_loop_iterations",
            overrides.max_loop_iterations.is_some(),
        ),
        (
            "required_stable_iterations",
            overrides.required_stable_iterations.is_some(),
        ),
        ("change_exclude", !overrides.change_exclude.is_empty()),
        (
            "agent_timeout_seconds",
            overrides.agent_timeout_seconds.is_some(),
        ),
        ("agent_wait_seconds", overrides.agent_wait_seconds.is_some()),
        (
            "suite_command_timeout_seconds",
            overrides.suite_command_timeout_seconds.is_some(),
        ),
        (
            "agent_log_max_output_lines",
            overrides.agent_log_max_output_lines.is_some(),
        ),
        (
            "agent_log_max_output_chars",
            overrides.agent_log_max_output_chars.is_some(),
        ),
        ("respect_limits", overrides.respect_limits.is_some()),
        (
            "use_agent_log_truncation_for_stdout_logs",
            overrides.use_agent_log_truncation_for_stdout_logs.is_some(),
        ),
    ];

    override_fields
        .iter()
        .filter(|(_, active)| *active)
        .map(|(name, _)| *name)
        .collect()
}

fn resolved_agent_binary(agent_name: &str) -> (&'static str, Option<String>) {
    match agent_name.trim().to_ascii_lowercase().as_str() {
        "claude" => ("claude", None),
        "opencode" => ("opencode", None),
        "cursor" | "cursor-agent" => ("cursor-agent", None),
        "codex" => ("codex", None),
        other => (
            "codex",
            Some(format!(
                "unsupported agent '{other}' falls back to codex at runtime"
            )),
        ),
    }
}

fn command_presence_detail(command: &str) -> Result<String> {
    match ProcessCommand::new(command).arg("--version").output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if !stdout.is_empty() {
                Ok(stdout.lines().next().unwrap_or(&stdout).to_owned())
            } else if !stderr.is_empty() {
                Ok(stderr.lines().next().unwrap_or(&stderr).to_owned())
            } else {
                Ok("installed".to_owned())
            }
        }
        Err(err) => Err(err).with_context(|| format!("failed to run `{command} --version`")),
    }
}

fn build_doctor_report(project_dir: &Path, cli: &Cli) -> DoctorReport {
    let config_path = paths::chief_yaml_path(project_dir);
    let mut checks = Vec::new();

    if project_dir.exists() && project_dir.is_dir() {
        checks.push(DoctorCheck {
            name: "project_dir".to_owned(),
            status: "ok".to_owned(),
            detail: project_dir.display().to_string(),
        });
    } else if project_dir.exists() {
        checks.push(DoctorCheck {
            name: "project_dir".to_owned(),
            status: "fail".to_owned(),
            detail: format!("{} exists but is not a directory", project_dir.display()),
        });
    } else {
        checks.push(DoctorCheck {
            name: "project_dir".to_owned(),
            status: "fail".to_owned(),
            detail: format!("{} does not exist", project_dir.display()),
        });
    }

    if project_dir.exists() && project_dir.is_dir() {
        match chief::git::ShellGitOps::discover(project_dir) {
            Ok(_) => checks.push(DoctorCheck {
                name: "git_repository".to_owned(),
                status: "ok".to_owned(),
                detail: "git repository discovered".to_owned(),
            }),
            Err(err) => checks.push(DoctorCheck {
                name: "git_repository".to_owned(),
                status: "fail".to_owned(),
                detail: err.to_string(),
            }),
        }
    }

    if config_path.is_file() {
        checks.push(DoctorCheck {
            name: "chief_yaml".to_owned(),
            status: "ok".to_owned(),
            detail: config_path.display().to_string(),
        });
    } else {
        checks.push(DoctorCheck {
            name: "chief_yaml".to_owned(),
            status: "fail".to_owned(),
            detail: format!(
                "missing {}; run `chief init` or copy .chief/chief.example.yaml",
                config_path.display()
            ),
        });
    }

    if config_path.is_file() {
        match load_chief_yaml_with_cli_overrides(project_dir, cli) {
            Ok(chief_yaml) => {
                checks.push(DoctorCheck {
                    name: "config_parse".to_owned(),
                    status: "ok".to_owned(),
                    detail: "chief.yaml parsed successfully".to_owned(),
                });

                let flow_name = chief_yaml.chief.flow.trim().to_owned();
                match flow_name.parse::<FlowKind>() {
                    Ok(flow_kind) => checks.push(DoctorCheck {
                        name: "flow".to_owned(),
                        status: "ok".to_owned(),
                        detail: format!("resolved flow: {}", flow_kind.as_str()),
                    }),
                    Err(err) => checks.push(DoctorCheck {
                        name: "flow".to_owned(),
                        status: "fail".to_owned(),
                        detail: err.to_string(),
                    }),
                }

                let (agent_binary, fallback_note) = resolved_agent_binary(&chief_yaml.chief.agent);
                checks.push(DoctorCheck {
                    name: "agent_config".to_owned(),
                    status: if fallback_note.is_some() {
                        "warn"
                    } else {
                        "ok"
                    }
                    .to_owned(),
                    detail: fallback_note.unwrap_or_else(|| {
                        format!("configured agent resolves to `{agent_binary}`")
                    }),
                });

                match command_presence_detail(agent_binary) {
                    Ok(detail) => checks.push(DoctorCheck {
                        name: "agent_binary".to_owned(),
                        status: "ok".to_owned(),
                        detail: format!("{agent_binary}: {detail}"),
                    }),
                    Err(err) => checks.push(DoctorCheck {
                        name: "agent_binary".to_owned(),
                        status: "fail".to_owned(),
                        detail: err.to_string(),
                    }),
                }

                checks.push(DoctorCheck {
                    name: "suite_count".to_owned(),
                    status: if chief_yaml.suites.is_empty() {
                        "warn"
                    } else {
                        "ok"
                    }
                    .to_owned(),
                    detail: format!("{} suite(s) configured", chief_yaml.suites.len()),
                });
            }
            Err(err) => checks.push(DoctorCheck {
                name: "config_parse".to_owned(),
                status: "fail".to_owned(),
                detail: err.to_string(),
            }),
        }
    }

    let overall_status = if checks.iter().any(|check| check.status == "fail") {
        "fail"
    } else if checks.iter().any(|check| check.status == "warn") {
        "warn"
    } else {
        "ok"
    };

    DoctorReport {
        project_dir: project_dir.display().to_string(),
        config_path: config_path.display().to_string(),
        overall_status: overall_status.to_owned(),
        checks,
    }
}

fn render_doctor_report_text(report: &DoctorReport) -> String {
    let mut lines = vec![
        format!("project_dir: {}", report.project_dir),
        format!("config_path: {}", report.config_path),
        format!("overall_status: {}", report.overall_status),
        String::new(),
    ];

    for check in &report.checks {
        lines.push(format!(
            "[{}] {}: {}",
            check.status, check.name, check.detail
        ));
    }

    lines.join("\n")
}

fn main() {
    if let Err(err) = run_with_db_reset_prompt() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run_with_db_reset_prompt() -> Result<()> {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => Ok(()),
        Err(err) => {
            let Some(reset) = db_reset_required_from_anyhow(&err) else {
                return Err(err);
            };
            eprintln!(
                "warning: chief.db is inconsistent at {}\nreason: {}\n",
                reset.db_path.display(),
                reset.reason
            );
            if !confirm_db_reset(&reset.db_path)? {
                eprintln!("cancelled: chief.db reset declined by user");
                return Ok(());
            }
            let store = ProjectStore::new(&cli.project_dir);
            store.reset_db()?;
            eprintln!("reset complete. retrying...\n");
            run(&cli)
        }
    }
}

fn command_requires_chief_yaml(command: &Commands) -> bool {
    !matches!(
        command,
        Commands::Init(_)
            | Commands::Migrate
            | Commands::Schema(_)
            | Commands::Config(_)
            | Commands::List(_)
            | Commands::Explain(_)
            | Commands::Doctor(_)
            | Commands::LoopFile(_)
    )
}

fn default_invocation_requires_chief_yaml(cli: &Cli) -> bool {
    let has_one_shot_input = cli.file.is_some()
        || cli.prompt.is_some()
        || !cli.requirements.is_empty()
        || !cli.requirements_file.is_empty();
    !has_one_shot_input
}

fn invocation_requires_chief_yaml(cli: &Cli) -> bool {
    cli.command
        .as_ref()
        .map(command_requires_chief_yaml)
        .unwrap_or_else(|| default_invocation_requires_chief_yaml(cli))
}

fn run_command(cli: &Cli, command: &Commands) -> Result<()> {
    match command {
        Commands::Init(args) => init_files::run_init(cli, args),
        Commands::Migrate => run_migrate(cli),
        Commands::CleanDone => run_clean_done(cli),
        Commands::Check(args) => run_check(cli, args),
        Commands::TailEvents(args) => run_tail_events(cli, args),
        Commands::Suite(args) => suite_commands::run_suite_command(cli, args),
        Commands::LoopFile(args) => run_loop_file(cli, args),
        Commands::Refactor => run_refactor(cli),
        Commands::Schema(args) => run_schema(args),
        Commands::Config(args) => run_config(cli, args),
        Commands::List(args) => run_list(cli, args),
        Commands::Explain(args) => run_explain(cli, args),
        Commands::Doctor(args) => run_doctor(cli, args),
    }
}

fn ensure_chief_yaml_exists(project_dir: &Path) -> Result<()> {
    let config_path = paths::chief_yaml_path(project_dir);
    if config_path.is_file() {
        return Ok(());
    }
    bail!(
        "missing required chief config at {}. create .chief/chief.yaml (run `chief init` or copy .chief/chief.example.yaml)",
        config_path.display()
    )
}

fn run(cli: &Cli) -> Result<()> {
    if invocation_requires_chief_yaml(cli) {
        ensure_chief_yaml_exists(&cli.project_dir)?;
    }

    if let Some(command) = &cli.command {
        return run_command(cli, command);
    }

    let context = load_context_with_cli_overrides(&cli.project_dir, cli)?;
    let flow_input = context.chief_yaml.chief.flow.trim();
    let flow_kind: FlowKind = flow_input
        .parse()
        .with_context(|| format!("invalid flow '{flow_input}'"))?;

    let requirements_text = load_requirements_text(&cli.requirements, &cli.requirements_file)?;
    if !requirements_text.trim().is_empty() {
        let engine = ChiefEngine::new(context.clone());
        let diff = engine.process_requirements(&requirements_text, cli.chief.model.clone())?;
        println!("=== git diff HEAD ===");
        if diff.trim().is_empty() {
            println!("(no diff)");
        } else {
            println!("{diff}");
        }
        return Ok(());
    }

    if matches!(flow_kind, FlowKind::LoopFile) {
        let file = cli.file.clone();
        let prompt = cli.prompt.clone();
        if file.is_none() && prompt.is_none() {
            bail!(
                "flow 'loop_file' requires either --file <path> or --prompt <text> (or use `chief loop_file --file <path>` or `chief loop_file --prompt <text>`)"
            );
        }
        if file.is_some() && prompt.is_some() {
            bail!("--file and --prompt are mutually exclusive for loop_file flow");
        }
        return run_loop_file(
            cli,
            &LoopFileArgs {
                file,
                prompt,
                watch_only: cli.watch_only.clone(),
            },
        );
    }
    if cli.file.is_some() || cli.prompt.is_some() {
        bail!("--file and --prompt are only supported when flow resolves to 'loop_file'");
    }
    run_todo_queue_flow(cli, context, flow_kind)
}

fn run_todo_queue_flow(cli: &Cli, mut context: ProjectContext, flow_kind: FlowKind) -> Result<()> {
    let report_started_at = Utc::now();
    context.chief_yaml.chief.flow = flow_kind.as_str().to_owned();
    print_config_summary(&context, &cli.chief);
    let engine = ChiefEngine::new(context.clone());
    let max_retries = context.chief_yaml.chief.max_retries.max(1);
    let head_before = context.git.head_commit(&context.project_dir).ok();
    let latest_run_before = latest_run_id(&context.store)?;

    let queue_result = engine.run_todos_until_done_with_retries(
        flow_kind,
        cli.chief.model.clone(),
        max_retries,
        |outcome| {
            println!(
                "completed todo {}{}",
                outcome.todo_id,
                outcome
                    .commit_hash
                    .as_deref()
                    .map(|hash| format!(" @ {hash}"))
                    .unwrap_or_default()
            );
        },
        |attempt, total, err| {
            eprintln!("run failed ({attempt}/{total}): {err:#}");
        },
    );

    let latest_run_after = latest_run_id(&context.store)?;
    let run_id = latest_run_after
        .as_deref()
        .filter(|candidate| latest_run_before.as_deref() != Some(*candidate));

    let exit_status = match &queue_result {
        Ok(()) => RunExitStatus::Success,
        Err(OrchestratorError::Retryable(_)) => RunExitStatus::Failure,
        Err(OrchestratorError::Unrecoverable(_)) => RunExitStatus::UnrecoverableFailure,
    };
    let exit_reason = match &queue_result {
        Ok(()) => Some("todo queue drained".to_owned()),
        Err(OrchestratorError::Retryable(err)) => Some(err.to_string()),
        Err(OrchestratorError::Unrecoverable(err)) => Some(err.to_string()),
    };

    match &queue_result {
        Ok(()) => {
            println!("all todos are done");
        }
        Err(_) => {}
    }
    if let Err(err) = print_cli_run_report(
        &context,
        run_id,
        head_before.as_deref(),
        report_started_at,
        exit_status,
        exit_reason.as_deref(),
    ) {
        eprintln!("warning: failed to print run report: {err:#}");
    }

    match queue_result {
        Ok(()) => Ok(()),
        Err(OrchestratorError::Retryable(err)) => Err(err).context("maximum retry count reached"),
        Err(OrchestratorError::Unrecoverable(err)) => Err(err).context("unrecoverable failure"),
    }
}

fn run_refactor(cli: &Cli) -> Result<()> {
    let context = load_context_with_cli_overrides(&cli.project_dir, cli)?;
    if cli.file.is_some() || cli.prompt.is_some() {
        bail!("--file and --prompt are only supported when flow resolves to 'loop_file'");
    }
    run_todo_queue_flow(cli, context, FlowKind::Refactor)
}

fn run_clean_done(cli: &Cli) -> Result<()> {
    let context = load_context_with_cli_overrides(&cli.project_dir, cli)?;
    let removed = context.store.clean_completed_todos_with_commit()?;
    println!("cleaned completed todos ({removed} removed)");
    Ok(())
}

fn run_tail_events(cli: &Cli, args: &TailEventsArgs) -> Result<()> {
    let context = load_context_with_cli_overrides(&cli.project_dir, cli)?;
    let events = context.store.query_events(EventQuery {
        limit: args.limit,
        ..EventQuery::default()
    })?;
    if events.is_empty() {
        println!("No events recorded.");
        return Ok(());
    }
    for event in events.into_iter().rev() {
        println!(
            "[{}] {} {} {} - {}",
            event.timestamp.to_rfc3339(),
            event.level,
            event.phase.map(|phase| phase.as_str()).unwrap_or("-"),
            event.event_type.as_str(),
            event.msg
        );
        if let Some(output) = event.payload.get("output").and_then(|value| value.as_str()) {
            let mut lines: Vec<_> = output
                .lines()
                .rev()
                .take(context.chief_yaml.chief.agent_log_max_output_lines)
                .collect();
            lines.reverse();
            let tail = lines.join("\n");
            if !tail.trim().is_empty() {
                println!("{tail}");
            }
        }
        println!();
    }
    Ok(())
}

fn run_check(cli: &Cli, args: &CheckArgs) -> Result<()> {
    let project_dir = resolve_project_dir(&cli.project_dir)?;
    ensure_project_dir_exists(&project_dir)?;

    let context = load_context_with_cli_overrides(&project_dir, cli)?;
    let project_name = context.name.clone();
    let projects_dir = project_dir.clone();
    let registry = ProjectRegistry::discover(&projects_dir, std::slice::from_ref(&project_dir))
        .with_context(|| {
            format!(
                "failed discovering project registry for {}",
                project_dir.display()
            )
        })?;
    let scheduler = Scheduler::new(registry, 1);
    let service = api::service::ApiService::new(scheduler, 1);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to initialize async runtime for `chief check`")?;

    let result = runtime.block_on(async {
        use tokio::sync::broadcast::error::{RecvError, TryRecvError};

        let mut readiness_receiver = service.subscribe_readiness_stream();
        let readiness_check = service.run_readiness_check(&project_name, args.force);
        tokio::pin!(readiness_check);

        loop {
            tokio::select! {
                readiness_result = &mut readiness_check => {
                    // Drain any trailing chunks queued right before completion.
                    loop {
                        match readiness_receiver.try_recv() {
                            Ok(api::service::ReadinessStreamMessage::Chunk { project: stream_project, text })
                                if stream_project == project_name =>
                            {
                                print!("{text}");
                                let _ = io::stdout().flush();
                            }
                            Ok(_) => {}
                            Err(TryRecvError::Lagged(_)) => continue,
                            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                        }
                    }
                    break readiness_result.map_err(anyhow::Error::new);
                }
                readiness_message = readiness_receiver.recv() => {
                    match readiness_message {
                        Ok(api::service::ReadinessStreamMessage::Chunk { project: stream_project, text })
                            if stream_project == project_name =>
                        {
                            print!("{text}");
                            let _ = io::stdout().flush();
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => {}
                    }
                }
            }
        }
    })?;

    println!("project: {project_name}");
    println!("status: {}", result.readiness.status);
    println!("summary: {}", result.readiness.summary);
    println!(
        "executed: {}",
        if result.ran { "yes" } else { "no (cached)" }
    );
    if let Some(checked_at) = result.readiness.checked_at {
        println!("checked_at: {checked_at}");
    }
    if let Some(checking_started_at) = result.readiness.checking_started_at {
        println!("checking_started_at: {checking_started_at}");
    }
    println!("updated_at: {}", result.readiness.updated_at);

    if result.readiness.status != "ready" {
        bail!("{}", result.readiness.summary);
    }

    Ok(())
}

fn run_schema(args: &SchemaArgs) -> Result<()> {
    let schema = build_cli_schema();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&schema)?);
    } else {
        println!("{}", render_schema_text(&schema));
    }
    Ok(())
}

fn run_config(cli: &Cli, args: &ConfigArgs) -> Result<()> {
    match &args.command {
        ConfigCommand::Show(args) => run_config_show(cli, args),
    }
}

fn run_config_show(cli: &Cli, args: &ConfigShowArgs) -> Result<()> {
    let project_dir = resolve_project_dir(&cli.project_dir)?;
    let config_path = paths::chief_yaml_path(&project_dir);

    if args.resolved {
        let chief_yaml = load_chief_yaml_with_cli_overrides(&project_dir, cli)?;
        if args.json {
            let payload = serde_json::json!({
                "project_dir": project_dir.display().to_string(),
                "config_path": config_path.display().to_string(),
                "resolved": true,
                "active_overrides": active_override_names(&cli.chief),
                "config": chief_yaml,
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
        } else {
            println!("project_dir: {}", project_dir.display());
            println!("config_path: {}", config_path.display());
            println!("resolved: true");
            let active_overrides = active_override_names(&cli.chief);
            if !active_overrides.is_empty() {
                println!("active_overrides: {}", active_overrides.join(", "));
            }
            println!();
            print!("{}", serde_yaml::to_string(&chief_yaml)?);
        }
        return Ok(());
    }

    match fs::read_to_string(&config_path) {
        Ok(raw) => {
            if args.json {
                let payload = serde_json::json!({
                    "project_dir": project_dir.display().to_string(),
                    "config_path": config_path.display().to_string(),
                    "resolved": false,
                    "exists": true,
                    "raw_yaml": raw,
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!("project_dir: {}", project_dir.display());
                println!("config_path: {}", config_path.display());
                println!("resolved: false");
                println!();
                print!("{raw}");
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if args.json {
                let payload = serde_json::json!({
                    "project_dir": project_dir.display().to_string(),
                    "config_path": config_path.display().to_string(),
                    "resolved": false,
                    "exists": false,
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!("chief.yaml not found at {}", config_path.display());
            }
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", config_path.display()));
        }
    }

    Ok(())
}

fn run_list(cli: &Cli, args: &ListArgs) -> Result<()> {
    match &args.command {
        ListCommand::Suites(args) => run_list_suites(cli, args),
    }
}

fn run_list_suites(cli: &Cli, args: &ListSuitesArgs) -> Result<()> {
    let project_dir = resolve_project_dir(&cli.project_dir)?;
    let chief_yaml = load_chief_yaml_with_cli_overrides(&project_dir, cli)?;
    let suites: Vec<SuiteSummary> = chief_yaml.suites.iter().map(SuiteSummary::from).collect();

    if args.json {
        let payload = serde_json::json!({
            "project_dir": project_dir.display().to_string(),
            "suite_count": suites.len(),
            "suites": suites,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("project_dir: {}", project_dir.display());
    println!("suite_count: {}", suites.len());
    if suites.is_empty() {
        println!();
        println!("No suites configured.");
        return Ok(());
    }

    for suite in suites {
        println!();
        println!("suite: {}", suite.name);
        println!("  language: {}", suite.language);
        println!("  framework: {}", suite.framework);
        println!("  test_root: {}", suite.test_root);
        println!("  target_type: {}", suite.target_type);
        if let Some(default_target) = suite.default_target {
            println!("  default_target: {}", default_target);
        }
        println!("  test_command: {}", suite.test_command);
        if let Some(command) = suite.test_init {
            println!("  test_init: {}", command);
        }
        if let Some(command) = suite.test_setup {
            println!("  test_setup: {}", command);
        }
        if let Some(command) = suite.lint_command {
            println!("  lint_command: {}", command);
        }
        if let Some(command) = suite.lint_fix_command {
            println!("  lint_fix_command: {}", command);
        }
        println!("  cache_mode: {}", suite.cache_mode);
        if let Some(timeout) = suite.command_timeout_seconds {
            println!("  command_timeout_seconds: {}", timeout);
        }
    }

    Ok(())
}

fn run_explain(cli: &Cli, args: &ExplainArgs) -> Result<()> {
    match &args.command {
        ExplainCommand::Flow(args) => run_explain_flow(cli, args),
    }
}

fn run_explain_flow(cli: &Cli, args: &ExplainFlowArgs) -> Result<()> {
    let project_dir = resolve_project_dir(&cli.project_dir)?;
    let flow = if let Some(flow) = args.flow {
        flow
    } else {
        let chief_yaml = load_chief_yaml_with_cli_overrides(&project_dir, cli)?;
        let flow_kind: FlowKind = chief_yaml
            .chief
            .flow
            .trim()
            .parse()
            .with_context(|| format!("invalid flow '{}'", chief_yaml.chief.flow.trim()))?;
        CliFlowValue::from_flow_kind(flow_kind)
    };
    let explanation = build_flow_explanation(flow);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&explanation)?);
        return Ok(());
    }

    println!("flow: {}", explanation.flow);
    println!("summary: {}", explanation.summary);
    println!("execution_model: {}", explanation.execution_model);
    println!();
    println!("inputs:");
    for value in explanation.inputs {
        println!("  - {value}");
    }
    println!("disallowed_inputs:");
    for value in explanation.disallowed_inputs {
        println!("  - {value}");
    }
    println!("prompt_sources:");
    for value in explanation.prompt_sources {
        println!("  - {value}");
    }
    println!("examples:");
    for value in explanation.examples {
        println!("  - {value}");
    }
    Ok(())
}

fn run_doctor(cli: &Cli, args: &DoctorArgs) -> Result<()> {
    let project_dir = resolve_project_dir(&cli.project_dir)?;
    let report = build_doctor_report(&project_dir, cli);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", render_doctor_report_text(&report));
    }

    if report.has_failures() {
        bail!("doctor found failing checks")
    }

    Ok(())
}

fn run_loop_file(cli: &Cli, args: &LoopFileArgs) -> Result<()> {
    if args.file.is_none() && args.prompt.is_none() {
        bail!("loop_file requires either --file or --prompt");
    }
    if args.file.is_some() && args.prompt.is_some() {
        bail!("--file and --prompt are mutually exclusive for loop_file");
    }

    let report_started_at = Utc::now();
    let mut context = load_context_with_cli_overrides(&cli.project_dir, cli)?;
    print_config_summary(&context, &cli.chief);

    let (source_desc, expectations): (String, String) = if let Some(ref file) = args.file {
        let file_path = if file.is_absolute() {
            file.clone()
        } else {
            context.project_dir.join(file)
        };
        let contents = fs::read_to_string(&file_path)
            .with_context(|| format!("failed to read loop_file input {}", file_path.display()))?;
        (file_path.display().to_string(), contents)
    } else if let Some(ref prompt) = args.prompt {
        ("cli-prompt".to_string(), prompt.clone())
    } else {
        unreachable!("loop_file input is validated before context loading");
    };

    context.chief_yaml.chief.flow = FlowKind::LoopFile.as_str().to_owned();
    println!("loop_file: started {}", source_desc);
    let head_before = context.git.head_commit(&context.project_dir).ok();

    let synthetic_todo = Todo {
        id: Todo::compute_id(&format!("loop_file:{}", source_desc), expectations.as_str()),
        todo: format!("loop_file: {}", source_desc),
        expectations,
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    };

    let engine = ChiefEngine::new(context.clone());
    let run_id = engine.start_run()?;
    let mut job = context.create_job(
        &run_id,
        1,
        FlowKind::LoopFile,
        Some(synthetic_todo.id.clone()),
        None,
    )?;
    job = context.set_job_status(job, JobStatus::Running, None)?;

    let result = engine.run_single_todo_with_retries(
        &run_id,
        &job.id,
        1,
        synthetic_todo,
        FlowKind::LoopFile,
        context.project_dir.clone(),
        cli.chief.model.clone(),
        args.watch_only.clone(),
        Arc::new(AtomicBool::new(false)),
        1,
        |attempt, total, err| {
            println!("loop_file: retry {attempt}/{total} failed: {err:#}");
        },
    );

    let exit_status = if let Err(err) = &result {
        if err.is_unrecoverable() {
            RunExitStatus::UnrecoverableFailure
        } else {
            RunExitStatus::Failure
        }
    } else {
        RunExitStatus::Success
    };
    let exit_reason = result
        .as_ref()
        .err()
        .map(|err| err.to_string())
        .or_else(|| Some("loop_file flow completed".to_owned()));
    let finalize_result = match &result {
        Ok(_) => context
            .set_job_status(job, JobStatus::Completed, None)
            .and_then(|_| engine.finish_run(&run_id, RunExitStatus::Success)),
        Err(err) => {
            let _ = context.set_job_status(job, JobStatus::Failed, Some(err.to_string()));
            engine.finish_run(&run_id, exit_status)
        }
    };

    if let Ok(outcome) = &result {
        println!(
            "completed loop_file {}{}",
            outcome.todo_id,
            outcome
                .commit_hash
                .as_deref()
                .map(|hash| format!(" @ {hash}"))
                .unwrap_or_default()
        );
    }
    if let Err(err) = &finalize_result {
        eprintln!("warning: failed to finalize loop_file run state: {err:#}");
    }
    if let Err(err) = print_cli_run_report(
        &context,
        Some(run_id.as_str()),
        head_before.as_deref(),
        report_started_at,
        exit_status,
        exit_reason.as_deref(),
    ) {
        eprintln!("warning: failed to print run report: {err:#}");
    }
    finalize_result?;

    match result {
        Ok(_) => Ok(()),
        Err(err) => Err(err.into_error()).context("loop_file execution failed"),
    }
}

fn load_requirements_text(inline: &[String], files: &[PathBuf]) -> Result<String> {
    let mut chunks = Vec::new();
    for item in inline {
        let trimmed = item.trim();
        if !trimmed.is_empty() {
            chunks.push(trimmed.to_owned());
        }
    }

    for file in files {
        let content = fs::read_to_string(file)
            .with_context(|| format!("failed to read requirements file {}", file.display()))?;
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            chunks.push(trimmed.to_owned());
        }
    }

    Ok(chunks.join("\n\n"))
}

fn confirm_db_reset(db_path: &Path) -> Result<bool> {
    eprint!(
        "Delete {} and rebuild empty schema? [y/N]: ",
        db_path.display()
    );
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn run_migrate(cli: &Cli) -> Result<()> {
    let project_dir = &cli.project_dir;
    if !project_dir.exists() {
        bail!(
            "project directory does not exist: {}",
            project_dir.display()
        );
    }
    if !project_dir.is_dir() {
        bail!("project path is not a directory: {}", project_dir.display());
    }

    let chief_dir = paths::chief_dir(project_dir);
    fs::create_dir_all(&chief_dir)
        .with_context(|| format!("failed to create {}", chief_dir.display()))?;

    let legacy_file_names = [
        paths::CHIEF_DB_FILE_NAME,
        paths::CHIEF_YAML_FILE_NAME,
        paths::CHIEF_EXAMPLE_FILE_NAME,
    ];

    let mut moved = 0usize;
    let mut skipped = 0usize;
    for file_name in legacy_file_names {
        let source = paths::legacy_root_file_path(project_dir, file_name);
        if !source.exists() {
            skipped += 1;
            continue;
        }

        let destination = chief_dir.join(file_name);
        if destination.exists() {
            bail!(
                "cannot migrate {} because destination already exists: {}",
                source.display(),
                destination.display()
            );
        }

        fs::rename(&source, &destination).with_context(|| {
            format!(
                "failed to migrate {} -> {}",
                source.display(),
                destination.display()
            )
        })?;
        moved += 1;
    }

    init_files::ensure_gitignore_entries(
        &project_dir.join(".gitignore"),
        &init_files::INIT_GITIGNORE_ENTRIES,
    )?;
    println!(
        "migrated chief files to {} (moved {moved}, skipped {skipped})",
        chief_dir.display()
    );
    Ok(())
}

fn latest_run_id(store: &ProjectStore) -> Result<Option<String>> {
    if !store.db_path.exists() {
        return Ok(None);
    }
    let conn = rusqlite::Connection::open(&store.db_path)
        .with_context(|| format!("failed to open {}", store.db_path.display()))?;
    let mut stmt = match conn.prepare("SELECT run_id FROM runs ORDER BY started_at DESC LIMIT 1") {
        Ok(stmt) => stmt,
        Err(err) if err.to_string().contains("no such table: runs") => return Ok(None),
        Err(err) => return Err(err).context("failed to prepare latest run query"),
    };
    stmt.query_row([], |row| row.get::<_, String>(0))
        .optional()
        .context("failed to read latest run id")
}

#[derive(Debug, Clone)]
struct ReportRunRecord {
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    exit_status: Option<String>,
}

#[derive(Debug, Clone)]
struct ReportEventRecord {
    level: String,
    msg: String,
    event_type: String,
    payload: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StableIterationProgress {
    current: usize,
    required: usize,
    converged: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct ReportAgentUsageLimit {
    label: String,
    percent_used: u32,
    percent_remaining: u32,
    reset_info: String,
    #[serde(default)]
    reset_minutes: Option<i64>,
    #[serde(default)]
    spent: Option<String>,
    #[serde(default)]
    requests: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct ReportAgentUsageSnapshot {
    provider: String,
    limits: Vec<ReportAgentUsageLimit>,
}

#[derive(Debug, Clone)]
struct CliRunReport {
    flow_name: String,
    project_dir: PathBuf,
    run_id: Option<String>,
    status: String,
    exit_reason: String,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    iterations: usize,
    stable_progress: Option<StableIterationProgress>,
    agent_calls: usize,
    warning_count: usize,
    lint_passed: usize,
    lint_failed: usize,
    test_passed: usize,
    test_failed: usize,
    wait_seconds_applied: f64,
    commits: Vec<String>,
    agent_name: String,
    usage_snapshot: Option<ReportAgentUsageSnapshot>,
    usage_source: Option<&'static str>,
}

fn parse_rfc3339_utc(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .with_context(|| format!("invalid RFC3339 timestamp '{value}'"))
}

fn load_run_record(store: &ProjectStore, run_id: &str) -> Result<Option<ReportRunRecord>> {
    let conn = rusqlite::Connection::open(&store.db_path)
        .with_context(|| format!("failed to open {}", store.db_path.display()))?;
    let mut stmt = conn
        .prepare(
            "SELECT started_at, ended_at, exit_status
             FROM runs
             WHERE run_id = ?1
             LIMIT 1",
        )
        .context("failed to prepare run record query")?;
    stmt.query_row([run_id], |row| {
        let started_at: String = row.get(0)?;
        let ended_at: Option<String> = row.get(1)?;
        let exit_status: Option<String> = row.get(2)?;
        Ok((started_at, ended_at, exit_status))
    })
    .optional()
    .context("failed to query run record")?
    .map(|(started_at, ended_at, exit_status)| {
        Ok(ReportRunRecord {
            started_at: parse_rfc3339_utc(&started_at)?,
            ended_at: ended_at.as_deref().map(parse_rfc3339_utc).transpose()?,
            exit_status,
        })
    })
    .transpose()
}

fn load_run_events(store: &ProjectStore, run_id: &str) -> Result<Vec<ReportEventRecord>> {
    let conn = rusqlite::Connection::open(&store.db_path)
        .with_context(|| format!("failed to open {}", store.db_path.display()))?;
    let mut stmt = conn
        .prepare(
            "SELECT level, msg, event_type, payload
             FROM events
             WHERE run_id = ?1
             ORDER BY id ASC",
        )
        .context("failed to prepare run event query")?;
    let rows = stmt
        .query_map([run_id], |row| {
            let payload_text: Option<String> = row.get(3)?;
            Ok((
                row.get::<_, Option<String>>(0)?
                    .unwrap_or_else(|| "info".to_owned()),
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                payload_text,
            ))
        })
        .context("failed to query run events")?;

    rows.map(|row| {
        let (level, msg, event_type, payload_text) = row?;
        let payload = payload_text
            .as_deref()
            .and_then(|text| serde_json::from_str::<BTreeMap<String, Value>>(text).ok())
            .unwrap_or_default();
        Ok(ReportEventRecord {
            level,
            msg,
            event_type,
            payload,
        })
    })
    .collect::<Result<Vec<_>>>()
}

fn git_commits_since(project_dir: &Path, head_before: Option<&str>) -> Result<Vec<String>> {
    let mut command = std::process::Command::new("git");
    command.arg("log").arg("--oneline");
    if let Some(head_before) = head_before {
        let range = format!("{head_before}..HEAD");
        command.arg(range);
    }

    let output = command
        .current_dir(project_dir)
        .output()
        .context("failed to run git log --oneline")?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

fn parse_stable_iteration_progress(msg: &str) -> Option<StableIterationProgress> {
    let (converged, tail) = if let Some((_, tail)) = msg.rsplit_once("stable result ") {
        (true, tail)
    } else if let Some((_, tail)) = msg.rsplit_once("phase stable ") {
        (false, tail)
    } else {
        return None;
    };

    let ratio = tail
        .split([';', ' '])
        .find(|segment| segment.contains('/'))?
        .trim();
    let (current, required) = ratio.split_once('/')?;
    Some(StableIterationProgress {
        current: current.parse().ok()?,
        required: required.parse().ok()?,
        converged,
    })
}

fn duration_from_datetimes(started_at: DateTime<Utc>, ended_at: DateTime<Utc>) -> Duration {
    ended_at
        .signed_duration_since(started_at)
        .to_std()
        .unwrap_or(Duration::ZERO)
}

fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn should_use_color_stdout() -> bool {
    if let Ok(force) = std::env::var("CLICOLOR_FORCE")
        && force.trim() != "0"
        && !force.trim().is_empty()
    {
        return true;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    io::stdout().is_terminal()
}

fn style(text: &str, code: &str, enabled: bool) -> String {
    if !enabled {
        return text.to_owned();
    }
    format!("{code}{text}\x1b[0m")
}

fn format_status_label(status: &str, enabled: bool) -> String {
    match status {
        "success" => style("SUCCESS", "\x1b[32m", enabled),
        "failure" => style("FAILURE", "\x1b[31m", enabled),
        "unrecoverable_failure" => style("UNRECOVERABLE", "\x1b[31;1m", enabled),
        other => style(&other.to_ascii_uppercase(), "\x1b[33m", enabled),
    }
}

fn format_report_key(key: &str, enabled: bool) -> String {
    style(key, "\x1b[36m", enabled)
}

fn format_report_header(enabled: bool) -> String {
    style("CHIEF RUN REPORT", "\x1b[1;37m", enabled)
}

fn format_usage_snapshot_lines(snapshot: &ReportAgentUsageSnapshot) -> Vec<String> {
    snapshot
        .limits
        .iter()
        .map(|limit| {
            let mut line = format!(
                "{}: {}% remaining, {}% used, {}",
                limit.label, limit.percent_remaining, limit.percent_used, limit.reset_info
            );
            if let Some(requests) = &limit.requests {
                line.push_str(&format!(", requests {requests}"));
            }
            if let Some(spent) = &limit.spent {
                line.push_str(&format!(", spent {spent}"));
            }
            line
        })
        .collect()
}

fn derive_exit_reason(
    status: &str,
    events: &[ReportEventRecord],
    fallback: Option<&str>,
) -> String {
    if status != "success" {
        if let Some(event) = events
            .iter()
            .rev()
            .find(|event| event.event_type == "phase_failure" || event.level == "error")
        {
            return event.msg.clone();
        }
        return fallback
            .map(str::to_owned)
            .unwrap_or_else(|| "run failed".to_owned());
    }

    if let Some(progress) = events
        .iter()
        .rev()
        .find_map(|event| parse_stable_iteration_progress(&event.msg))
        && progress.converged
    {
        return format!(
            "converged after {} stable iteration(s) out of {} required",
            progress.current, progress.required
        );
    }

    if let Some(event) = events.iter().rev().find(|event| {
        event.msg.contains("phase done on iteration")
            || event
                .msg
                .contains("found no ready tickets during pre-check")
            || event.msg.contains("skipping commit")
            || event.msg.contains("loop done; preparing commit")
    }) {
        return event.msg.clone();
    }

    fallback
        .map(str::to_owned)
        .unwrap_or_else(|| "run completed successfully".to_owned())
}

fn probe_current_agent_usage(
    agent_name: &str,
    project_dir: &Path,
) -> Result<ReportAgentUsageSnapshot> {
    let config = UsageConfig {
        timeout: 45,
        verbose: false,
        approval_policy: ApprovalPolicy::Fail,
        directory: Some(project_dir.display().to_string()),
    };

    let usage = if agent_name.eq_ignore_ascii_case("claude") {
        run_claude(&config)?
    } else if agent_name.eq_ignore_ascii_case("codex") {
        run_codex(&config)?
    } else {
        bail!("unsupported agent '{}' for usage reporting", agent_name);
    };

    Ok(ReportAgentUsageSnapshot {
        provider: usage.provider,
        limits: usage
            .entries
            .into_iter()
            .map(|entry| ReportAgentUsageLimit {
                label: entry.label,
                percent_used: entry.percent_used,
                percent_remaining: entry.percent_remaining,
                reset_info: entry.reset_info,
                reset_minutes: entry.reset_minutes,
                spent: entry.spent,
                requests: entry.requests,
            })
            .collect(),
    })
}

fn build_cli_run_report(
    context: &ProjectContext,
    run_id: Option<&str>,
    head_before: Option<&str>,
    started_at_fallback: DateTime<Utc>,
    exit_status_fallback: RunExitStatus,
    exit_reason_fallback: Option<&str>,
) -> Result<CliRunReport> {
    let run_record = match run_id {
        Some(run_id) => load_run_record(&context.store, run_id)?,
        None => None,
    };
    let events = match run_id {
        Some(run_id) => load_run_events(&context.store, run_id)?,
        None => Vec::new(),
    };
    let ended_at_fallback = Utc::now();
    let status = run_record
        .as_ref()
        .and_then(|record| record.exit_status.clone())
        .unwrap_or_else(|| exit_status_fallback.as_str().to_owned());
    let commits = git_commits_since(&context.project_dir, head_before)?;
    let started_at = run_record
        .as_ref()
        .map(|record| record.started_at)
        .unwrap_or(started_at_fallback);
    let ended_at = run_record
        .as_ref()
        .and_then(|record| record.ended_at)
        .unwrap_or(ended_at_fallback);
    let stable_progress = events
        .iter()
        .rev()
        .find_map(|event| parse_stable_iteration_progress(&event.msg));
    let usage_snapshot_from_events = events.iter().rev().find_map(|event| {
        event
            .payload
            .get("usage")
            .cloned()
            .and_then(|value| serde_json::from_value::<ReportAgentUsageSnapshot>(value).ok())
    });
    let (usage_snapshot, usage_source) =
        match probe_current_agent_usage(&context.chief_yaml.chief.agent, &context.project_dir) {
            Ok(snapshot) => (Some(snapshot), Some("live")),
            Err(_) => (usage_snapshot_from_events, Some("cached")),
        };
    let usage_source = if usage_snapshot.is_some() {
        usage_source
    } else {
        None
    };

    let mut lint_passed = 0usize;
    let mut lint_failed = 0usize;
    let mut test_passed = 0usize;
    let mut test_failed = 0usize;
    let mut wait_seconds_applied = 0.0f64;

    for event in &events {
        match event.event_type.as_str() {
            "lint" => {
                if event
                    .payload
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .unwrap_or(1)
                    == 0
                {
                    lint_passed += 1;
                } else {
                    lint_failed += 1;
                }
            }
            "test_run" => {
                if event
                    .payload
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .unwrap_or(1)
                    == 0
                {
                    test_passed += 1;
                } else {
                    test_failed += 1;
                }
            }
            "agent_cmd" => {
                wait_seconds_applied += event
                    .payload
                    .get("wait_seconds_applied")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
            }
            _ => {}
        }
    }

    Ok(CliRunReport {
        flow_name: context.chief_yaml.chief.flow.clone(),
        project_dir: context.project_dir.clone(),
        run_id: run_id.map(str::to_owned),
        status: status.clone(),
        exit_reason: derive_exit_reason(&status, &events, exit_reason_fallback),
        started_at,
        ended_at,
        iterations: events
            .iter()
            .filter(|event| {
                event.event_type == "phase_change" && event.msg.contains(" loop iteration ")
            })
            .count(),
        stable_progress,
        agent_calls: events
            .iter()
            .filter(|event| event.event_type == "agent_prompt")
            .count(),
        warning_count: events
            .iter()
            .filter(|event| {
                matches!(
                    event.level.to_ascii_lowercase().as_str(),
                    "warning" | "warn" | "error"
                )
            })
            .count(),
        lint_passed,
        lint_failed,
        test_passed,
        test_failed,
        wait_seconds_applied,
        commits,
        agent_name: context.chief_yaml.chief.agent.clone(),
        usage_snapshot,
        usage_source,
    })
}

fn render_cli_run_report(report: &CliRunReport) -> String {
    let color = should_use_color_stdout();
    let mut lines = Vec::new();
    let divider = style(
        "================================================================",
        "\x1b[90m",
        color,
    );
    lines.push(divider.clone());
    lines.push(format_report_header(color));
    lines.push(divider.clone());
    lines.push(format!(
        "{} {}",
        format_report_key("status", color),
        format_status_label(&report.status, color)
    ));
    lines.push(format!(
        "{} {}",
        format_report_key("reason", color),
        report.exit_reason
    ));
    lines.push(format!(
        "{} {}",
        format_report_key("flow", color),
        report.flow_name
    ));
    if let Some(run_id) = &report.run_id {
        lines.push(format!("{} {}", format_report_key("run_id", color), run_id));
    }
    lines.push(format!(
        "{} {}",
        format_report_key("project", color),
        report.project_dir.display()
    ));
    lines.push(String::new());
    lines.push(format!(
        "{} {}",
        format_report_key("started_at", color),
        report.started_at.to_rfc3339()
    ));
    lines.push(format!(
        "{} {}",
        format_report_key("ended_at", color),
        report.ended_at.to_rfc3339()
    ));
    lines.push(format!(
        "{} {}",
        format_report_key("duration", color),
        format_duration(duration_from_datetimes(report.started_at, report.ended_at))
    ));
    lines.push(String::new());
    lines.push(format!(
        "{} {}",
        format_report_key("iterations", color),
        report.iterations
    ));
    if let Some(progress) = &report.stable_progress {
        lines.push(format!(
            "{} {}/{}{}",
            format_report_key("stable_iterations", color),
            progress.current,
            progress.required,
            if progress.converged {
                " (converged)"
            } else {
                ""
            }
        ));
    }
    lines.push(format!(
        "{} {}",
        format_report_key("agent_calls", color),
        report.agent_calls
    ));
    lines.push(format!(
        "{} {}",
        format_report_key("warnings", color),
        report.warning_count
    ));
    lines.push(format!(
        "{} {} passed, {} failed",
        format_report_key("lint", color),
        report.lint_passed,
        report.lint_failed
    ));
    lines.push(format!(
        "{} {} passed, {} failed",
        format_report_key("tests", color),
        report.test_passed,
        report.test_failed
    ));
    lines.push(format!(
        "{} {}",
        format_report_key("limit_wait", color),
        format_duration(Duration::from_secs_f64(
            report.wait_seconds_applied.max(0.0)
        ))
    ));
    lines.push(String::new());
    lines.push(format!(
        "{} {}",
        format_report_key("commits", color),
        report.commits.len()
    ));
    if report.commits.is_empty() {
        lines.push("  (no new commits)".to_owned());
    } else {
        for commit in &report.commits {
            lines.push(format!("  {commit}"));
        }
    }
    lines.push(String::new());
    lines.push(format!(
        "{} {}",
        format_report_key("agent", color),
        report.agent_name
    ));
    match (&report.usage_snapshot, report.usage_source) {
        (Some(snapshot), Some(source)) => {
            lines.push(format!(
                "{} {} ({source})",
                format_report_key("usage_provider", color),
                snapshot.provider
            ));
            for line in format_usage_snapshot_lines(snapshot) {
                lines.push(format!("  {line}"));
            }
        }
        _ => lines.push(format!("{} unavailable", format_report_key("usage", color))),
    }
    lines.push(divider);
    lines.join("\n")
}

fn print_cli_run_report(
    context: &ProjectContext,
    run_id: Option<&str>,
    head_before: Option<&str>,
    started_at_fallback: DateTime<Utc>,
    exit_status_fallback: RunExitStatus,
    exit_reason_fallback: Option<&str>,
) -> Result<()> {
    let report = build_cli_run_report(
        context,
        run_id,
        head_before,
        started_at_fallback,
        exit_status_fallback,
        exit_reason_fallback,
    )?;
    println!("{}", render_cli_run_report(&report));
    Ok(())
}

fn print_config_summary(context: &ProjectContext, overrides: &CliChiefOverrides) {
    let color = should_use_color_stdout();
    let divider = style(
        "================================================================",
        "\x1b[90m",
        color,
    );
    println!("{}", divider);
    println!(
        "{}",
        style("\x1b[1mChief Configuration\x1b[0m", "\x1b[1m", color)
    );
    println!("{}", divider);
    println!(
        "{} {}",
        format_report_key("project", color),
        context.project_dir.display()
    );
    println!(
        "{} {}",
        format_report_key("flow", color),
        context.chief_yaml.chief.flow
    );
    println!(
        "{} {}",
        format_report_key("agent", color),
        context.chief_yaml.chief.agent
    );
    if let Some(ref model) = context.chief_yaml.chief.model {
        println!("{} {}", format_report_key("model", color), model);
    }
    if let Some(ref reasoning) = context.chief_yaml.chief.model_reasoning_effort {
        println!(
            "{} {}",
            format_report_key("reasoning_effort", color),
            reasoning
        );
    }
    if !context.chief_yaml.chief.agent_extra_args.is_empty() {
        println!(
            "{} {}",
            format_report_key("agent_extra_args", color),
            context.chief_yaml.chief.agent_extra_args.join(" ")
        );
    }
    if let Some(ref mcp) = context.chief_yaml.chief.mcp_servers {
        println!(
            "{} {:?}",
            format_report_key("mcp_servers", color),
            mcp.keys().collect::<Vec<_>>()
        );
    }
    println!(
        "{} {}",
        format_report_key("max_retries", color),
        context.chief_yaml.chief.max_retries
    );
    println!(
        "{} {}",
        format_report_key("max_loop_iterations", color),
        context.chief_yaml.chief.max_loop_iterations
    );
    println!(
        "{} {}",
        format_report_key("required_stable_iterations", color),
        context.chief_yaml.chief.required_stable_iterations
    );
    println!(
        "{} {}s",
        format_report_key("agent_timeout", color),
        context.chief_yaml.chief.agent_timeout_seconds
    );
    if let Some(agent_wait_seconds) = context.chief_yaml.chief.agent_wait_seconds {
        println!(
            "{} {}s",
            format_report_key("agent_wait", color),
            agent_wait_seconds
        );
    }
    println!(
        "{} {}s",
        format_report_key("suite_timeout", color),
        context.chief_yaml.chief.suite_command_timeout_seconds
    );
    println!(
        "{} {}",
        format_report_key("respect_limits", color),
        context.chief_yaml.chief.respect_limits
    );
    println!(
        "{} {}",
        format_report_key("log_truncation", color),
        context
            .chief_yaml
            .chief
            .use_agent_log_truncation_for_stdout_logs
    );

    let active_overrides = active_override_names(overrides);
    if !active_overrides.is_empty() {
        println!(
            "{}",
            style("\x1b[90m--- CLI Overrides ---\x1b[0m", "\x1b[90m", color)
        );
        println!(
            "{} {}",
            format_report_key("active_overrides", color),
            active_overrides.join(", ")
        );
    }
    println!("{}", divider);
}
