use super::ProjectContext;
use anyhow::{Context, Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ProjectRegistry {
    projects_dir: PathBuf,
    manual_project_dirs: Vec<PathBuf>,
    projects: HashMap<String, ProjectContext>,
}

impl ProjectRegistry {
    pub fn discover(
        projects_dir: impl AsRef<Path>,
        manual_project_dirs: &[PathBuf],
    ) -> Result<Self> {
        let projects_dir = projects_dir.as_ref().to_path_buf();
        let manual_project_dirs = manual_project_dirs.to_vec();
        let projects = Self::discover_projects(&projects_dir, &manual_project_dirs)?;

        Ok(Self {
            projects_dir,
            manual_project_dirs,
            projects,
        })
    }

    fn discover_projects(
        projects_dir: &Path,
        manual_project_dirs: &[PathBuf],
    ) -> Result<HashMap<String, ProjectContext>> {
        let mut projects = HashMap::new();
        let mut seen_paths = HashSet::new();

        for entry in fs::read_dir(projects_dir)
            .with_context(|| format!("failed to read {}", projects_dir.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            if !path.join(".git").exists() {
                continue;
            }

            let normalized = Self::normalize_project_path(&path);
            if !seen_paths.insert(normalized) {
                continue;
            }

            let Ok(context) = ProjectContext::load(&path) else {
                continue;
            };
            Self::insert_project(&mut projects, context)?;
        }

        let cwd =
            std::env::current_dir().context("failed resolving current directory for --project")?;
        for manual_project_dir in manual_project_dirs {
            let project_dir = if manual_project_dir.is_absolute() {
                manual_project_dir.clone()
            } else {
                cwd.join(manual_project_dir)
            };

            if !project_dir.exists() {
                return Err(anyhow!(
                    "manual project path does not exist: {}",
                    manual_project_dir.display()
                ));
            }
            if !project_dir.is_dir() {
                return Err(anyhow!(
                    "manual project path is not a directory: {}",
                    manual_project_dir.display()
                ));
            }

            let normalized = Self::normalize_project_path(&project_dir);
            if !seen_paths.insert(normalized) {
                continue;
            }

            let context = ProjectContext::load(&project_dir).with_context(|| {
                format!(
                    "failed loading manual project from {}",
                    manual_project_dir.display()
                )
            })?;
            Self::insert_project(&mut projects, context)?;
        }

        Ok(projects)
    }

    fn normalize_project_path(path: &Path) -> PathBuf {
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    fn insert_project(
        projects: &mut HashMap<String, ProjectContext>,
        context: ProjectContext,
    ) -> Result<()> {
        if let Some(existing) = projects.get(&context.name) {
            if existing.project_dir != context.project_dir {
                return Err(anyhow!(
                    "duplicate project name '{}' for '{}' and '{}'",
                    context.name,
                    existing.project_dir.display(),
                    context.project_dir.display()
                ));
            }
            return Ok(());
        }
        projects.insert(context.name.clone(), context);
        Ok(())
    }

    pub fn projects_dir(&self) -> &Path {
        &self.projects_dir
    }

    pub fn list_projects(&self) -> Vec<ProjectContext> {
        let mut items = self.projects.values().cloned().collect::<Vec<_>>();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        items
    }

    pub fn get(&self, project_name: &str) -> Option<ProjectContext> {
        self.projects.get(project_name).cloned()
    }

    pub fn reload(&mut self) -> Result<()> {
        *self = Self::discover(&self.projects_dir, &self.manual_project_dirs)?;
        Ok(())
    }
}
