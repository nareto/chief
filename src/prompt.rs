use anyhow::{Context, Result, anyhow};
use minijinja::Environment;
use minijinja::value::Value as JinjaValue;
use serde_json::Value;
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
    pub fn from_workspace_prompts() -> Result<Self> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("prompts");
        let store = Self { root };
        store.validate_required_templates()?;
        Ok(store)
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

    pub fn validate_required_templates(&self) -> Result<()> {
        if !self.root.is_dir() {
            return Err(anyhow!(
                "missing required prompts directory at {}",
                self.root.display()
            ));
        }
        for template_name in REQUIRED_PROMPT_FILES {
            let path = self.root.join(template_name);
            if !path.is_file() {
                return Err(anyhow!(
                    "missing required prompt template {}",
                    path.display()
                ));
            }
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
