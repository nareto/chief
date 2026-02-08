use anyhow::{Context, Result, anyhow};
use minijinja::Environment;
use minijinja::value::Value as JinjaValue;
use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub trait PromptStore: Send + Sync {
    fn render_json(&self, template_name: &str, data: &Value) -> Result<String>;

    fn exists(&self, template_name: &str) -> bool;
}

#[derive(Debug, Clone)]
pub struct FsPromptStore {
    root: PathBuf,
}

const REQUIRED_PROMPT_FILES: [&str; 6] = [
    "red.md",
    "green.md",
    "post_green.md",
    "lint_fix.md",
    "requirements.md",
    "todo_select.md",
];

impl FsPromptStore {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn list_templates(&self) -> Result<Vec<String>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|file| file.to_str()) {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    pub fn ensure_default_templates(&self) -> Result<()> {
        if !self.root.exists() {
            fs::create_dir_all(&self.root)
                .with_context(|| format!("failed to create {}", self.root.display()))?;
        }
        let template_source_root = resolve_template_source_root()?;
        for template_name in REQUIRED_PROMPT_FILES {
            let path = self.root.join(template_name);
            if path.exists() {
                continue;
            }
            let source_path = template_source_root.join(template_name);
            if !source_path.exists() {
                return Err(anyhow!(
                    "required prompt template '{}' is missing from {}",
                    template_name,
                    template_source_root.display()
                ));
            }
            fs::copy(&source_path, &path).with_context(|| {
                format!(
                    "failed to copy default prompt {} -> {}",
                    source_path.display(),
                    path.display()
                )
            })?;
        }
        Ok(())
    }

    fn template_path(&self, template_name: &str) -> PathBuf {
        self.root.join(template_name)
    }
}

impl PromptStore for FsPromptStore {
    fn render_json(&self, template_name: &str, data: &Value) -> Result<String> {
        let path = self.template_path(template_name);
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read prompt {}", path.display()))?;

        let mut env = Environment::new();
        env.add_template(template_name, &source)
            .with_context(|| format!("invalid template {template_name}"))?;

        let tmpl = env
            .get_template(template_name)
            .map_err(|err| anyhow!("template load failed {template_name}: {err}"))?;

        tmpl.render(JinjaValue::from_serialize(data))
            .map_err(|err| anyhow!("template render failed {template_name}: {err}"))
    }

    fn exists(&self, template_name: &str) -> bool {
        self.template_path(template_name).exists()
    }
}

fn resolve_template_source_root() -> Result<PathBuf> {
    if let Ok(path) = env::var("CHIEF_PROMPTS_DIR") {
        let explicit = PathBuf::from(path);
        if explicit.is_dir() {
            return Ok(explicit);
        }
        return Err(anyhow!(
            "CHIEF_PROMPTS_DIR points to a non-directory: {}",
            explicit.display()
        ));
    }

    let repo_prompts = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("prompts");
    if repo_prompts.is_dir() {
        return Ok(repo_prompts);
    }

    let cwd_prompts = env::current_dir()
        .context("failed to resolve current working directory")?
        .join("prompts");
    if cwd_prompts.is_dir() {
        return Ok(cwd_prompts);
    }

    Err(anyhow!(
        "could not locate prompt templates; expected a prompts directory or CHIEF_PROMPTS_DIR"
    ))
}
