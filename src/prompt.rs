use anyhow::{Context, Result, anyhow};
use minijinja::Environment;
use minijinja::value::Value as JinjaValue;
use serde_json::Value;

pub trait PromptStore: Send + Sync {
    fn render_json(&self, template_name: &str, data: &Value) -> Result<String>;

    fn exists(&self, template_name: &str) -> bool;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EmbeddedPromptStore;

#[derive(Debug, Clone, Copy)]
struct EmbeddedPrompt {
    name: &'static str,
    source: &'static str,
}

const REQUIRED_PROMPT_FILES: [&str; 4] = [
    "loop_file_prompt.md",
    "structural_cleanup.md",
    "mechanical_cleanup.md",
    "requirements.md",
];

const EMBEDDED_PROMPTS: &[EmbeddedPrompt] = &[
    EmbeddedPrompt {
        name: "loop_file_prompt.md",
        source: include_str!("../prompts/loop_file_prompt.md"),
    },
    EmbeddedPrompt {
        name: "mechanical_cleanup.md",
        source: include_str!("../prompts/mechanical_cleanup.md"),
    },
    EmbeddedPrompt {
        name: "requirements.md",
        source: include_str!("../prompts/requirements.md"),
    },
    EmbeddedPrompt {
        name: "structural_cleanup.md",
        source: include_str!("../prompts/structural_cleanup.md"),
    },
];

impl EmbeddedPromptStore {
    pub fn from_embedded_prompts() -> Result<Self> {
        let store = Self;
        store.validate_required_templates()?;
        Ok(store)
    }

    pub fn list_templates(&self) -> Result<Vec<String>> {
        let mut names = EMBEDDED_PROMPTS
            .iter()
            .map(|prompt| prompt.name.to_owned())
            .collect::<Vec<_>>();
        names.sort();
        Ok(names)
    }

    pub fn validate_required_templates(&self) -> Result<()> {
        for template_name in REQUIRED_PROMPT_FILES {
            if embedded_prompt_source(template_name).is_none() {
                return Err(anyhow!("missing embedded prompt template {template_name}"));
            }
        }
        Ok(())
    }

    fn template_source(&self, template_name: &str) -> Result<&'static str> {
        embedded_prompt_source(template_name)
            .ok_or_else(|| anyhow!("missing embedded prompt template {template_name}"))
    }
}

impl PromptStore for EmbeddedPromptStore {
    fn render_json(&self, template_name: &str, data: &Value) -> Result<String> {
        let source = self.template_source(template_name)?;

        let mut env = Environment::new();
        env.add_template(template_name, source)
            .with_context(|| format!("invalid template {template_name}"))?;

        let tmpl = env
            .get_template(template_name)
            .map_err(|err| anyhow!("template load failed {template_name}: {err}"))?;

        tmpl.render(JinjaValue::from_serialize(data))
            .map_err(|err| anyhow!("template render failed {template_name}: {err}"))
    }

    fn exists(&self, template_name: &str) -> bool {
        embedded_prompt_source(template_name).is_some()
    }
}

fn embedded_prompt_source(template_name: &str) -> Option<&'static str> {
    EMBEDDED_PROMPTS
        .iter()
        .find(|prompt| prompt.name == template_name)
        .map(|prompt| prompt.source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn embedded_prompt_store_lists_required_templates() {
        let store =
            EmbeddedPromptStore::from_embedded_prompts().expect("embedded prompts should validate");

        for template_name in REQUIRED_PROMPT_FILES {
            assert!(
                store.exists(template_name),
                "embedded prompt should exist: {template_name}"
            );
        }

        assert_eq!(
            store
                .list_templates()
                .expect("embedded prompts should list"),
            vec![
                "loop_file_prompt.md",
                "mechanical_cleanup.md",
                "requirements.md",
                "structural_cleanup.md",
            ]
        );
    }

    #[test]
    fn embedded_prompt_store_renders_templates() {
        let store =
            EmbeddedPromptStore::from_embedded_prompts().expect("embedded prompts should validate");

        let rendered = store
            .render_json(
                "requirements.md",
                &json!({"requirements_text": "Ship release binaries"}),
            )
            .expect("embedded prompt should render");

        assert!(rendered.contains("Ship release binaries"));
    }

    #[test]
    fn embedded_prompt_store_renders_loop_file_prompt() {
        let store =
            EmbeddedPromptStore::from_embedded_prompts().expect("embedded prompts should validate");

        let rendered = store
            .render_json(
                "loop_file_prompt.md",
                &json!({"file_contents": "Tighten parser errors"}),
            )
            .expect("loop_file prompt should render");

        assert!(rendered.contains("Tighten parser errors"));
    }

    #[test]
    fn embedded_prompt_store_reports_missing_templates() {
        let store =
            EmbeddedPromptStore::from_embedded_prompts().expect("embedded prompts should validate");

        let err = store
            .render_json("missing.md", &json!({}))
            .expect_err("missing prompt should fail");

        assert!(
            err.to_string()
                .contains("missing embedded prompt template missing.md"),
            "error should mention the missing embedded template: {err}"
        );
    }
}
