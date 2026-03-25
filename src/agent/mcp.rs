use crate::config::{McpServerAuthConfig, McpServerConfig};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use toml::Table;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub(super) struct AgentScratchDir {
    path: PathBuf,
}

impl AgentScratchDir {
    pub(super) fn new(prefix: &str) -> Result<Self> {
        let path = env::temp_dir().join(format!(
            "chief-{prefix}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create scratch dir {}", path.display()))?;
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&path)
                .with_context(|| format!("failed to stat {}", path.display()))?
                .permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&path, permissions)
                .with_context(|| format!("failed to chmod {}", path.display()))?;
        }
        Ok(Self { path })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    fn write_text_file(&self, file_name: &str, content: &str) -> Result<PathBuf> {
        let path = self.path.join(file_name);
        fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))?;
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&path)
                .with_context(|| format!("failed to stat {}", path.display()))?
                .permissions();
            permissions.set_mode(0o600);
            fs::set_permissions(&path, permissions)
                .with_context(|| format!("failed to chmod {}", path.display()))?;
        }
        Ok(path)
    }
}

impl Drop for AgentScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(super) struct ClaudeMcpRuntime {
    pub(super) config_path: PathBuf,
    pub(super) scratch_dir: AgentScratchDir,
}

pub(super) fn prepare_claude_mcp_runtime(
    servers: &BTreeMap<String, McpServerConfig>,
) -> Result<ClaudeMcpRuntime> {
    let scratch_dir = AgentScratchDir::new("claude-mcp")?;
    let config = ClaudeMcpConfig {
        mcp_servers: build_claude_servers(servers)?,
    };
    let json = serde_json::to_string_pretty(&config).context("failed to serialize Claude MCP")?;
    let config_path = scratch_dir.write_text_file("mcp.json", &json)?;
    Ok(ClaudeMcpRuntime {
        config_path,
        scratch_dir,
    })
}

pub(super) struct CodexMcpRuntime {
    pub(super) home_dir: PathBuf,
    pub(super) scratch_dir: AgentScratchDir,
}

pub(super) fn prepare_codex_mcp_runtime(
    servers: &BTreeMap<String, McpServerConfig>,
    cwd: &Path,
) -> Result<CodexMcpRuntime> {
    prepare_codex_mcp_runtime_with_source_home(servers, cwd, current_codex_home().as_deref())
}

fn prepare_codex_mcp_runtime_with_source_home(
    servers: &BTreeMap<String, McpServerConfig>,
    cwd: &Path,
    source_home: Option<&Path>,
) -> Result<CodexMcpRuntime> {
    let scratch_dir = AgentScratchDir::new("codex-home")?;
    let config_text = build_codex_config(source_home, servers, cwd)?;
    scratch_dir.write_text_file("config.toml", &config_text)?;
    copy_codex_auth_json(source_home, scratch_dir.path())?;
    Ok(CodexMcpRuntime {
        home_dir: scratch_dir.path().to_path_buf(),
        scratch_dir,
    })
}

#[derive(Serialize)]
struct ClaudeMcpConfig {
    #[serde(rename = "mcpServers")]
    mcp_servers: BTreeMap<String, ClaudeMcpServer>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClaudeMcpServer {
    Stdio {
        command: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        #[serde(skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
    },
}

#[derive(Serialize)]
struct CodexMcpServer {
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bearer_token_env_var: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    http_headers: BTreeMap<String, String>,
}

fn build_claude_servers(
    servers: &BTreeMap<String, McpServerConfig>,
) -> Result<BTreeMap<String, ClaudeMcpServer>> {
    servers
        .iter()
        .map(|(name, server)| {
            let rendered = match server {
                McpServerConfig::Stdio { command, args, env } => ClaudeMcpServer::Stdio {
                    command: command.clone(),
                    args: args.clone(),
                    env: env.clone(),
                },
                McpServerConfig::StreamableHttp { url, auth } => {
                    let mut headers = BTreeMap::new();
                    if let Some(token) = resolve_jwt_auth(auth)? {
                        headers.insert(
                            "Authorization".to_owned(),
                            match token {
                                JwtToken::Static(token) => format!("Bearer {token}"),
                                JwtToken::EnvVar(token_env_var) => {
                                    format!("Bearer ${{{token_env_var}}}")
                                }
                            },
                        );
                    }
                    ClaudeMcpServer::Http {
                        url: url.clone(),
                        headers,
                    }
                }
            };
            Ok((name.clone(), rendered))
        })
        .collect()
}

fn build_codex_servers(
    servers: &BTreeMap<String, McpServerConfig>,
) -> Result<BTreeMap<String, CodexMcpServer>> {
    servers
        .iter()
        .map(|(name, server)| {
            let rendered = match server {
                McpServerConfig::Stdio { command, args, env } => CodexMcpServer {
                    command: Some(command.clone()),
                    args: args.clone(),
                    env: env.clone(),
                    url: None,
                    bearer_token_env_var: None,
                    http_headers: BTreeMap::new(),
                },
                McpServerConfig::StreamableHttp { url, auth } => {
                    let mut http_headers = BTreeMap::new();
                    let mut bearer_token_env_var = None;
                    if let Some(token) = resolve_jwt_auth(auth)? {
                        match token {
                            JwtToken::Static(token) => {
                                http_headers
                                    .insert("Authorization".to_owned(), format!("Bearer {token}"));
                            }
                            JwtToken::EnvVar(token_env_var) => {
                                bearer_token_env_var = Some(token_env_var);
                            }
                        }
                    }
                    CodexMcpServer {
                        command: None,
                        args: Vec::new(),
                        env: BTreeMap::new(),
                        url: Some(url.clone()),
                        bearer_token_env_var,
                        http_headers,
                    }
                }
            };
            Ok((name.clone(), rendered))
        })
        .collect()
}

fn build_codex_config(
    source_home: Option<&Path>,
    servers: &BTreeMap<String, McpServerConfig>,
    cwd: &Path,
) -> Result<String> {
    let mut root = load_codex_config_table(source_home)?;
    root.remove("mcp_servers");
    root.insert(
        "mcp_servers".to_owned(),
        toml::Value::try_from(build_codex_servers(servers)?)
            .context("failed to encode Codex MCP config")?,
    );
    mark_codex_project_untrusted(&mut root, cwd)?;
    toml::to_string_pretty(&toml::Value::Table(root)).context("failed to serialize Codex config")
}

fn load_codex_config_table(source_home: Option<&Path>) -> Result<Table> {
    let Some(source_home) = source_home else {
        return Ok(Table::new());
    };
    let config_path = source_home.join("config.toml");
    if !config_path.is_file() {
        return Ok(Table::new());
    }
    let content = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    if content.trim().is_empty() {
        return Ok(Table::new());
    }
    let value: toml::Value = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    match value {
        toml::Value::Table(table) => Ok(table),
        _ => bail!(
            "Codex config {} must have a table at the root",
            config_path.display()
        ),
    }
}

fn mark_codex_project_untrusted(root: &mut Table, cwd: &Path) -> Result<()> {
    let projects = root
        .entry("projects".to_owned())
        .or_insert_with(|| toml::Value::Table(Table::new()));
    let projects = projects
        .as_table_mut()
        .ok_or_else(|| anyhow!("Codex config field `projects` must be a table"))?;
    let project = projects
        .entry(cwd.display().to_string())
        .or_insert_with(|| toml::Value::Table(Table::new()));
    let project = project.as_table_mut().ok_or_else(|| {
        anyhow!(
            "Codex config field `projects.{}` must be a table",
            cwd.display()
        )
    })?;
    project.insert(
        "trust_level".to_owned(),
        toml::Value::String("untrusted".to_owned()),
    );
    Ok(())
}

fn copy_codex_auth_json(source_home: Option<&Path>, dest_home: &Path) -> Result<()> {
    let Some(source_home) = source_home else {
        return Ok(());
    };
    let auth_path = source_home.join("auth.json");
    if !auth_path.is_file() {
        return Ok(());
    }
    let dest_path = dest_home.join("auth.json");
    fs::copy(&auth_path, &dest_path).with_context(|| {
        format!(
            "failed to copy Codex auth from {} to {}",
            auth_path.display(),
            dest_path.display()
        )
    })?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&dest_path)
            .with_context(|| format!("failed to stat {}", dest_path.display()))?
            .permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&dest_path, permissions)
            .with_context(|| format!("failed to chmod {}", dest_path.display()))?;
    }
    Ok(())
}

fn current_codex_home() -> Option<PathBuf> {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .or_else(|| env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".codex")))
}

#[derive(Debug)]
enum JwtToken {
    Static(String),
    EnvVar(String),
}

fn resolve_jwt_auth(auth: &Option<McpServerAuthConfig>) -> Result<Option<JwtToken>> {
    match auth {
        None => Ok(None),
        Some(McpServerAuthConfig::Jwt {
            token,
            token_env_var,
        }) => {
            let token = token
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let token_env_var = token_env_var
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            match (token, token_env_var) {
                (Some(_), Some(_)) => {
                    bail!("MCP JWT auth must use either `token` or `token_env_var`, not both")
                }
                (Some(token), None) => Ok(Some(JwtToken::Static(token.to_owned()))),
                (None, Some(token_env_var)) => Ok(Some(JwtToken::EnvVar(token_env_var.to_owned()))),
                (None, None) => {
                    bail!("MCP JWT auth requires either `token` or `token_env_var` to be set")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let path = env::temp_dir().join(format!(
                "chief-mcp-test-{prefix}-{}-{}",
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
    fn prepare_claude_runtime_renders_http_jwt_env_var() {
        let servers = BTreeMap::from([(
            "sentry".to_owned(),
            McpServerConfig::StreamableHttp {
                url: "https://mcp.sentry.dev/mcp".to_owned(),
                auth: Some(McpServerAuthConfig::Jwt {
                    token: None,
                    token_env_var: Some("SENTRY_TOKEN".to_owned()),
                }),
            },
        )]);

        let runtime = prepare_claude_mcp_runtime(&servers).expect("Claude MCP runtime should work");
        let rendered =
            fs::read_to_string(&runtime.config_path).expect("Claude MCP JSON should be readable");
        assert!(rendered.contains("\"type\": \"http\""));
        assert!(rendered.contains("\"Authorization\": \"Bearer ${SENTRY_TOKEN}\""));
    }

    #[test]
    fn prepare_codex_runtime_preserves_non_mcp_config_and_copies_auth() {
        let source_home = TempDir::new("codex-source-home");
        fs::write(
            source_home.path.join("config.toml"),
            r#"model = "gpt-5"

[mcp_servers.user_defined]
command = "npx"
args = ["-y", "user-server"]
"#,
        )
        .expect("source config should be written");
        fs::write(source_home.path.join("auth.json"), "{\"token\":\"abc\"}")
            .expect("auth file should be written");

        let cwd = TempDir::new("codex-project");
        let servers = BTreeMap::from([(
            "context7".to_owned(),
            McpServerConfig::Stdio {
                command: "npx".to_owned(),
                args: vec!["-y".to_owned(), "@upstash/context7-mcp".to_owned()],
                env: BTreeMap::new(),
            },
        )]);

        let runtime = prepare_codex_mcp_runtime_with_source_home(
            &servers,
            &cwd.path,
            Some(source_home.path.as_path()),
        )
        .expect("Codex MCP runtime should work");
        let rendered = fs::read_to_string(runtime.home_dir.join("config.toml"))
            .expect("generated Codex config should be readable");

        assert!(rendered.contains("model = \"gpt-5\""));
        assert!(rendered.contains("[mcp_servers.context7]"));
        assert!(!rendered.contains("user_defined"));
        assert!(rendered.contains("trust_level = \"untrusted\""));
        assert!(runtime.home_dir.join("auth.json").is_file());
    }

    #[test]
    fn invalid_jwt_auth_is_rejected() {
        let auth = Some(McpServerAuthConfig::Jwt {
            token: Some("secret".to_owned()),
            token_env_var: Some("TOKEN".to_owned()),
        });
        let err = resolve_jwt_auth(&auth).expect_err("invalid auth should fail");
        assert!(
            err.to_string()
                .contains("either `token` or `token_env_var`")
        );
    }
}
