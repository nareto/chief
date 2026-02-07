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
        for (name, body) in default_templates() {
            let path = self.root.join(name);
            if path.exists() {
                continue;
            }
            fs::write(&path, body)
                .with_context(|| format!("failed to create {}", path.display()))?;
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

fn default_templates() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "red.md",
            r#"We are in RED phase for this todo.

TODO:
{{ todo.todo }}

Expectations:
{{ todo.expectations }}

Suites:
{% for suite in suites %}
- {{ suite.name }} ({{ suite.framework }}): {{ suite.test_command }}
{% endfor %}

Previous log:
{{ previous_steps_log }}

Write or refine tests only.
If no edits are needed, respond exactly with: NO CHANGES
"#,
        ),
        (
            "green.md",
            r#"We are in GREEN phase for this todo.

TODO:
{{ todo.todo }}

Expectations:
{{ todo.expectations }}

Previous log:
{{ previous_steps_log }}

Implement code to satisfy tests and requirements.
"#,
        ),
        (
            "post_green.md",
            r#"We are in POST_GREEN phase for this todo.

TODO:
{{ todo.todo }}

Post-green commands:
{% for command in post_green_commands %}
- {{ command }}
{% endfor %}

Previous log:
{{ previous_steps_log }}

Fix any remaining validation failures.
"#,
        ),
        (
            "lint_fix.md",
            r#"Linting failed.

Commands:
{% for command in lint_commands %}
- {{ command }}
{% endfor %}

Recent lint output:
{{ lint_errors }}

Apply fixes so lint passes.
"#,
        ),
        (
            "requirements.md",
            r#"You are processing new requirements into todos.

Tasks:
1. Inspect existing project state.
2. Optionally scaffold if needed.
3. Update chief.toml if required.
4. Update {{ todos_path }} with granular todos, each with expectations and priority.

Requirements:
{{ requirements_text }}
"#,
        ),
        (
            "todo_select.md",
            r#"You are worker {{ worker_index }} in multi-agent mode.

Available todos (not done, not in progress):
{% for todo in available_todos %}
- [{{ todo.id }}] priority={{ todo.priority }} :: {{ todo.todo }}
{% endfor %}

Already in progress by other workers:
{% for todo in in_progress_todos %}
- [{{ todo.id }}] {{ todo.todo }}
{% endfor %}

Select ONE todo id that is least likely to conflict with in-progress work.
Respond with ONLY the selected todo id.
"#,
        ),
    ]
}
