use crate::domain::TargetType;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChiefYaml {
    #[serde(default)]
    pub chief: ChiefConfig,
    #[serde(default)]
    pub suites: Vec<TestSuiteConfig>,
}

impl ChiefYaml {
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if content.trim().is_empty() {
            return Ok(Self::default());
        }
        let cfg: Self = serde_yaml::from_str(&content)
            .with_context(|| format!("failed to parse YAML {}", path.display()))?;
        Ok(cfg)
    }

    pub fn load_or_default(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        Self::load_from_file(path)
    }

    pub fn selected_suites(&self, suite_names: &[String]) -> Vec<TestSuiteConfig> {
        if suite_names.is_empty() {
            return Vec::new();
        }
        self.suites
            .iter()
            .filter(|suite| suite_names.iter().any(|name| name == &suite.name))
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChiefConfig {
    #[serde(default = "default_flow")]
    pub flow: String,
    #[serde(default = "default_agent")]
    pub agent: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_reasoning_effort: Option<String>,
    #[serde(default)]
    pub agent_extra_args: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Option<BTreeMap<String, McpServerConfig>>,
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
    #[serde(default = "default_max_loop_iterations", alias = "max_loop")]
    pub max_loop_iterations: usize,
    #[serde(default = "default_required_stable_iterations")]
    pub required_stable_iterations: usize,
    #[serde(default = "default_agent_timeout_seconds")]
    pub agent_timeout_seconds: u64,
    #[serde(default)]
    pub agent_wait_seconds: Option<u64>,
    #[serde(default = "default_suite_command_timeout_seconds")]
    pub suite_command_timeout_seconds: u64,
    #[serde(default = "default_agent_log_max_output_lines")]
    pub agent_log_max_output_lines: usize,
    #[serde(default = "default_agent_log_max_output_chars")]
    pub agent_log_max_output_chars: usize,
    #[serde(default = "default_respect_limits")]
    pub respect_limits: bool,
    #[serde(default)]
    pub use_agent_log_truncation_for_stdout_logs: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChiefConfigOverrides {
    pub flow: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub model_reasoning_effort: Option<String>,
    pub agent_extra_args: Option<Vec<String>>,
    pub mcp_servers: Option<Option<BTreeMap<String, McpServerConfig>>>,
    pub max_retries: Option<usize>,
    pub max_loop_iterations: Option<usize>,
    pub required_stable_iterations: Option<usize>,
    pub agent_timeout_seconds: Option<u64>,
    pub agent_wait_seconds: Option<u64>,
    pub suite_command_timeout_seconds: Option<u64>,
    pub agent_log_max_output_lines: Option<usize>,
    pub agent_log_max_output_chars: Option<usize>,
    pub respect_limits: Option<bool>,
    pub use_agent_log_truncation_for_stdout_logs: Option<bool>,
}

impl Default for ChiefConfig {
    fn default() -> Self {
        Self {
            flow: default_flow(),
            agent: default_agent(),
            model: None,
            model_reasoning_effort: None,
            agent_extra_args: Vec::new(),
            mcp_servers: None,
            max_retries: default_max_retries(),
            max_loop_iterations: default_max_loop_iterations(),
            required_stable_iterations: default_required_stable_iterations(),
            agent_timeout_seconds: default_agent_timeout_seconds(),
            agent_wait_seconds: None,
            suite_command_timeout_seconds: default_suite_command_timeout_seconds(),
            agent_log_max_output_lines: default_agent_log_max_output_lines(),
            agent_log_max_output_chars: default_agent_log_max_output_chars(),
            respect_limits: default_respect_limits(),
            use_agent_log_truncation_for_stdout_logs: false,
        }
    }
}

impl ChiefConfig {
    pub fn into_overrides(self) -> ChiefConfigOverrides {
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
            agent_timeout_seconds,
            agent_wait_seconds,
            suite_command_timeout_seconds,
            agent_log_max_output_lines,
            agent_log_max_output_chars,
            respect_limits,
            use_agent_log_truncation_for_stdout_logs,
        } = self;

        ChiefConfigOverrides {
            flow: Some(flow),
            agent: Some(agent),
            model,
            model_reasoning_effort,
            agent_extra_args: Some(agent_extra_args),
            mcp_servers: Some(mcp_servers),
            max_retries: Some(max_retries),
            max_loop_iterations: Some(max_loop_iterations),
            required_stable_iterations: Some(required_stable_iterations),
            agent_timeout_seconds: Some(agent_timeout_seconds),
            agent_wait_seconds,
            suite_command_timeout_seconds: Some(suite_command_timeout_seconds),
            agent_log_max_output_lines: Some(agent_log_max_output_lines),
            agent_log_max_output_chars: Some(agent_log_max_output_chars),
            respect_limits: Some(respect_limits),
            use_agent_log_truncation_for_stdout_logs: Some(
                use_agent_log_truncation_for_stdout_logs,
            ),
        }
    }

    pub fn apply_overrides(self, overrides: ChiefConfigOverrides) -> Self {
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
            agent_timeout_seconds,
            agent_wait_seconds,
            suite_command_timeout_seconds,
            agent_log_max_output_lines,
            agent_log_max_output_chars,
            respect_limits,
            use_agent_log_truncation_for_stdout_logs,
        } = self;

        let ChiefConfigOverrides {
            flow: flow_override,
            agent: agent_override,
            model: model_override,
            model_reasoning_effort: model_reasoning_effort_override,
            agent_extra_args: agent_extra_args_override,
            mcp_servers: mcp_servers_override,
            max_retries: max_retries_override,
            max_loop_iterations: max_loop_iterations_override,
            required_stable_iterations: required_stable_iterations_override,
            agent_timeout_seconds: agent_timeout_seconds_override,
            agent_wait_seconds: agent_wait_seconds_override,
            suite_command_timeout_seconds: suite_command_timeout_seconds_override,
            agent_log_max_output_lines: agent_log_max_output_lines_override,
            agent_log_max_output_chars: agent_log_max_output_chars_override,
            respect_limits: respect_limits_override,
            use_agent_log_truncation_for_stdout_logs:
                use_agent_log_truncation_for_stdout_logs_override,
        } = overrides;

        Self {
            flow: flow_override.unwrap_or(flow),
            agent: agent_override.unwrap_or(agent),
            model: model_override.or(model),
            model_reasoning_effort: model_reasoning_effort_override.or(model_reasoning_effort),
            agent_extra_args: agent_extra_args_override.unwrap_or(agent_extra_args),
            mcp_servers: mcp_servers_override.unwrap_or(mcp_servers),
            max_retries: max_retries_override.unwrap_or(max_retries),
            max_loop_iterations: max_loop_iterations_override.unwrap_or(max_loop_iterations),
            required_stable_iterations: required_stable_iterations_override
                .unwrap_or(required_stable_iterations),
            agent_timeout_seconds: agent_timeout_seconds_override.unwrap_or(agent_timeout_seconds),
            agent_wait_seconds: agent_wait_seconds_override.or(agent_wait_seconds),
            suite_command_timeout_seconds: suite_command_timeout_seconds_override
                .unwrap_or(suite_command_timeout_seconds),
            agent_log_max_output_lines: agent_log_max_output_lines_override
                .unwrap_or(agent_log_max_output_lines),
            agent_log_max_output_chars: agent_log_max_output_chars_override
                .unwrap_or(agent_log_max_output_chars),
            respect_limits: respect_limits_override.unwrap_or(respect_limits),
            use_agent_log_truncation_for_stdout_logs:
                use_agent_log_truncation_for_stdout_logs_override
                    .unwrap_or(use_agent_log_truncation_for_stdout_logs),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpServerConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    #[serde(alias = "http")]
    StreamableHttp {
        url: String,
        #[serde(default)]
        auth: Option<McpServerAuthConfig>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpServerAuthConfig {
    Jwt {
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        token_env_var: Option<String>,
    },
}

fn default_flow() -> String {
    "loop_file".to_owned()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SuiteCacheMode {
    #[default]
    Copy,
    Symlink,
}

impl SuiteCacheMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Symlink => "symlink",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestSuiteConfig {
    pub name: String,
    pub language: String,
    pub framework: String,
    #[serde(default = "default_test_root")]
    pub test_root: String,
    pub test_command: String,
    #[serde(default = "default_target_type")]
    pub target_type: TargetType,
    #[serde(default)]
    pub default_target: Option<String>,
    #[serde(default)]
    pub file_patterns: Vec<String>,
    #[serde(default)]
    pub disallow_write_globs: Vec<String>,
    #[serde(default)]
    pub test_init: Option<String>,
    #[serde(default)]
    pub test_setup: Option<String>,
    #[serde(default)]
    pub cache_paths: Vec<String>,
    #[serde(default)]
    pub cache_key_files: Vec<String>,
    #[serde(default)]
    pub cache_mode: SuiteCacheMode,
    #[serde(default)]
    pub post_green_command: Option<String>,
    #[serde(default)]
    pub cleanup_command: Option<String>,
    #[serde(default)]
    pub command_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub lint_command: Option<String>,
    #[serde(default)]
    pub lint_fix_command: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_strip_root_from_target")]
    pub strip_root_from_target: bool,
}

impl TestSuiteConfig {
    pub fn prompt_block(&self) -> String {
        format!(
            "name: {}\nlanguage: {}\nframework: {}\ntest_root: {}\ntest_command: {}",
            self.name, self.language, self.framework, self.test_root, self.test_command
        )
    }
}

fn default_agent() -> String {
    "codex".to_owned()
}

fn default_max_retries() -> usize {
    2
}

fn default_max_loop_iterations() -> usize {
    20
}

fn default_required_stable_iterations() -> usize {
    2
}

fn default_agent_timeout_seconds() -> u64 {
    2_700
}

fn default_suite_command_timeout_seconds() -> u64 {
    1_800
}

fn default_agent_log_max_output_lines() -> usize {
    10
}

fn default_agent_log_max_output_chars() -> usize {
    1_500
}

fn default_respect_limits() -> bool {
    true
}

fn default_test_root() -> String {
    ".".to_owned()
}

fn default_target_type() -> TargetType {
    TargetType::Project
}

fn default_strip_root_from_target() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_max_retries_is_2() {
        let config = ChiefConfig::default();
        assert_eq!(config.max_retries, 2);
    }

    #[test]
    fn parse_max_retries_default() {
        let yaml = "chief: {}\n";
        let parsed: ChiefYaml = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.chief.max_retries, 2);
    }

    #[test]
    fn parse_max_loop_iterations_default() {
        let yaml = "chief: {}\n";
        let parsed: ChiefYaml = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.chief.max_loop_iterations, 20);
    }

    #[test]
    fn parse_legacy_max_loop_alias() {
        let yaml = "chief:\n  max_loop: 4\n";
        let parsed: ChiefYaml = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.chief.max_loop_iterations, 4);
    }

    #[test]
    fn parse_required_stable_iterations_default() {
        let yaml = "chief: {}\n";
        let parsed: ChiefYaml = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.chief.required_stable_iterations, 2);
    }

    #[test]
    fn parse_required_stable_iterations_override() {
        let yaml = "chief:\n  required_stable_iterations: 4\n";
        let parsed: ChiefYaml = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.chief.required_stable_iterations, 4);
    }

    #[test]
    fn parse_agent_timeout_zero_means_no_timeout() {
        let yaml = "chief:\n  agent_timeout_seconds: 0\n";
        let parsed: ChiefYaml = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.chief.agent_timeout_seconds, 0);
    }

    #[test]
    fn parse_agent_timeout_nonzero_preserved() {
        let yaml = "chief:\n  agent_timeout_seconds: 600\n";
        let parsed: ChiefYaml = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.chief.agent_timeout_seconds, 600);
    }

    #[test]
    fn agent_wait_seconds_defaults_to_absent() {
        let yaml = "chief: {}\n";
        let parsed: ChiefYaml = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.chief.agent_wait_seconds, None);
    }

    #[test]
    fn parse_agent_wait_seconds_preserved() {
        let yaml = "chief:\n  agent_wait_seconds: 45\n";
        let parsed: ChiefYaml = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.chief.agent_wait_seconds, Some(45));
    }

    #[test]
    fn respect_limits_defaults_to_true() {
        let yaml = "chief: {}\n";
        let parsed: ChiefYaml = serde_yaml::from_str(yaml).unwrap();
        assert!(parsed.chief.respect_limits);
    }

    #[test]
    fn respect_limits_can_be_disabled() {
        let yaml = "chief:\n  respect_limits: false\n";
        let parsed: ChiefYaml = serde_yaml::from_str(yaml).unwrap();
        assert!(!parsed.chief.respect_limits);
    }

    #[test]
    fn mcp_servers_default_to_unmanaged() {
        let parsed: ChiefYaml = serde_yaml::from_str("chief: {}\n").unwrap();
        assert!(parsed.chief.mcp_servers.is_none());
    }

    #[test]
    fn parse_empty_mcp_server_map() {
        let yaml = "chief:\n  mcp_servers: {}\n";
        let parsed: ChiefYaml = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.chief.mcp_servers, Some(BTreeMap::new()));
    }

    #[test]
    fn parse_mcp_stdio_and_http_servers() {
        let yaml = r#"chief:
  mcp_servers:
    docs:
      transport: stdio
      command: npx
      args: ["-y", "@acme/docs-mcp"]
      env:
        DOCS_TOKEN: secret
    sentry:
      transport: streamable_http
      url: https://mcp.sentry.dev/mcp
      auth:
        type: jwt
        token_env_var: SENTRY_TOKEN
"#;
        let parsed: ChiefYaml = serde_yaml::from_str(yaml).unwrap();
        let servers = parsed.chief.mcp_servers.expect("mcp servers should parse");

        assert!(matches!(
            servers.get("docs"),
            Some(McpServerConfig::Stdio { command, args, env })
                if command == "npx"
                    && args == &vec!["-y".to_owned(), "@acme/docs-mcp".to_owned()]
                    && env.get("DOCS_TOKEN") == Some(&"secret".to_owned())
        ));
        assert!(matches!(
            servers.get("sentry"),
            Some(McpServerConfig::StreamableHttp { url, auth: Some(McpServerAuthConfig::Jwt { token: None, token_env_var: Some(token_env_var) }) })
                if url == "https://mcp.sentry.dev/mcp" && token_env_var == "SENTRY_TOKEN"
        ));
    }

    #[test]
    fn parse_suite_cache_mode_defaults_to_copy() {
        let yaml = r#"suites:
  - name: backend
    language: Rust
    framework: cargo
    test_root: .
    test_command: cargo test
    cache_paths: [target]"#;
        let parsed: ChiefYaml = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.suites[0].cache_mode, SuiteCacheMode::Copy);
    }

    #[test]
    fn parse_suite_cache_mode_symlink() {
        let yaml = r#"suites:
  - name: backend
    language: Rust
    framework: cargo
    test_root: .
    test_command: cargo test
    cache_paths: [target]
    cache_mode: symlink"#;
        let parsed: ChiefYaml = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.suites[0].cache_mode, SuiteCacheMode::Symlink);
    }

    #[test]
    fn parse_cleanup_command() {
        let yaml = r#"suites:
  - name: backend
    language: Rust
    framework: cargo
    test_root: .
    test_command: cargo test
    cleanup_command: pkill -f vitest || true"#;
        let parsed: ChiefYaml = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            parsed.suites[0].cleanup_command.as_deref(),
            Some("pkill -f vitest || true")
        );
    }

    #[test]
    fn chief_config_into_overrides_round_trips_every_field() {
        let config = ChiefConfig {
            flow: "refactor".to_owned(),
            agent: "claude".to_owned(),
            model: Some("sonnet".to_owned()),
            model_reasoning_effort: Some("high".to_owned()),
            agent_extra_args: vec!["--dangerously-skip-permissions".to_owned()],
            mcp_servers: Some(BTreeMap::from([(
                "docs".to_owned(),
                McpServerConfig::Stdio {
                    command: "npx".to_owned(),
                    args: vec!["-y".to_owned(), "@acme/docs-mcp".to_owned()],
                    env: BTreeMap::new(),
                },
            )])),
            max_retries: 5,
            max_loop_iterations: 7,
            required_stable_iterations: 3,
            agent_timeout_seconds: 111,
            agent_wait_seconds: Some(12),
            suite_command_timeout_seconds: 222,
            agent_log_max_output_lines: 33,
            agent_log_max_output_chars: 444,
            respect_limits: false,
            use_agent_log_truncation_for_stdout_logs: true,
        };

        let overrides = config.clone().into_overrides();
        let applied = ChiefConfig::default().apply_overrides(overrides);
        assert_eq!(applied, config);
    }

    #[test]
    fn chief_config_apply_overrides_supports_mcp_personal_mode() {
        let config = ChiefConfig {
            mcp_servers: Some(BTreeMap::new()),
            ..ChiefConfig::default()
        };
        let overrides = ChiefConfigOverrides {
            mcp_servers: Some(None),
            ..ChiefConfigOverrides::default()
        };

        let applied = config.apply_overrides(overrides);
        assert_eq!(applied.mcp_servers, None);
    }
}
