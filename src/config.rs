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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
    #[serde(default = "default_agent_timeout_seconds")]
    pub agent_timeout_seconds: u64,
    #[serde(default = "default_suite_command_timeout_seconds")]
    pub suite_command_timeout_seconds: u64,
    #[serde(default = "default_agent_log_max_output_lines")]
    pub agent_log_max_output_lines: usize,
    #[serde(default = "default_agent_log_max_output_chars")]
    pub agent_log_max_output_chars: usize,
    #[serde(default)]
    pub use_agent_log_truncation_for_stdout_logs: bool,
}

impl Default for ChiefConfig {
    fn default() -> Self {
        Self {
            flow: default_flow(),
            agent: default_agent(),
            model: None,
            model_reasoning_effort: None,
            agent_extra_args: Vec::new(),
            max_retries: default_max_retries(),
            agent_timeout_seconds: default_agent_timeout_seconds(),
            suite_command_timeout_seconds: default_suite_command_timeout_seconds(),
            agent_log_max_output_lines: default_agent_log_max_output_lines(),
            agent_log_max_output_chars: default_agent_log_max_output_chars(),
            use_agent_log_truncation_for_stdout_logs: false,
        }
    }
}

fn default_flow() -> String {
    "single_prompt".to_owned()
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
    pub post_green_command: Option<String>,
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
    10
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

fn default_test_root() -> String {
    ".".to_owned()
}

fn default_target_type() -> TargetType {
    TargetType::Project
}

fn default_strip_root_from_target() -> bool {
    true
}
