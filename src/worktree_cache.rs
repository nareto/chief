use crate::config::{SuiteCacheMode, TestSuiteConfig};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

mod plan;
mod transfer;

use plan::build_suite_cache_plan;
use transfer::{
    copy_path_recursive, copy_path_with_tar_fallback, create_symlink, path_exists,
    remove_path_if_exists,
};

#[derive(Debug, Clone, Default)]
pub struct CachePrimeReport {
    pub suites_considered: usize,
    pub cached_paths: usize,
    pub missing_source_paths: usize,
    pub invalid_paths: usize,
}

#[derive(Debug, Clone, Default)]
pub struct CacheHydrationReport {
    pub suites_considered: usize,
    pub linked_paths: usize,
    pub skipped_existing_paths: usize,
    pub missing_cache_paths: usize,
    pub invalid_paths: usize,
}

pub fn file_content_md5(path: &Path) -> Result<String> {
    let content =
        fs::read(path).with_context(|| format!("failed reading file {}", path.display()))?;
    Ok(format!("{:x}", md5::compute(content)))
}

pub fn suite_cache_inputs_hash(
    worktree_dir: &Path,
    suites: &[TestSuiteConfig],
    chief_yaml_hash: &str,
) -> String {
    let mut suite_keys = suites
        .iter()
        .filter_map(|suite| {
            build_suite_cache_plan(worktree_dir, suite, chief_yaml_hash)
                .ok()
                .flatten()
                .map(|plan| (plan.suite_name, plan.cache_key))
        })
        .collect::<Vec<_>>();
    suite_keys.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    format!(
        "{:x}",
        md5::compute(serde_json::to_vec(&suite_keys).unwrap_or_default())
    )
}

pub fn prime_suite_caches_from_worktree(
    project_dir: &Path,
    project_name: &str,
    suites: &[TestSuiteConfig],
    source_worktree_dir: &Path,
    chief_yaml_hash: &str,
) -> Result<CachePrimeReport> {
    let mut report = CachePrimeReport::default();
    let cache_root = worktree_cache_root_for_project(project_dir, project_name);
    fs::create_dir_all(&cache_root).with_context(|| {
        format!(
            "failed to create worktree cache root directory {}",
            cache_root.display()
        )
    })?;

    for suite in suites {
        let Some(plan) = build_suite_cache_plan(source_worktree_dir, suite, chief_yaml_hash)?
        else {
            continue;
        };
        report.invalid_paths += plan.invalid_paths;
        report.suites_considered += 1;

        let destination_root =
            suite_cache_directory(&cache_root, &plan.suite_cache_dir_name, &plan.cache_key);
        remove_path_if_exists(&destination_root)?;
        fs::create_dir_all(&destination_root).with_context(|| {
            format!(
                "failed to create suite cache directory {}",
                destination_root.display()
            )
        })?;

        for relative_path in &plan.cache_paths {
            let source_path = plan.suite_root.join(relative_path);
            if !source_path.exists() {
                report.missing_source_paths += 1;
                continue;
            }
            let destination_path = destination_root.join(relative_path);
            copy_path_with_tar_fallback(&source_path, &destination_path)?;
            report.cached_paths += 1;
        }
    }

    Ok(report)
}

pub fn hydrate_suite_caches_into_worktree(
    project_dir: &Path,
    project_name: &str,
    suites: &[TestSuiteConfig],
    target_worktree_dir: &Path,
    chief_yaml_hash: &str,
) -> Result<CacheHydrationReport> {
    let mut report = CacheHydrationReport::default();
    let cache_root = worktree_cache_root_for_project(project_dir, project_name);
    if !cache_root.exists() {
        return Ok(report);
    }

    for suite in suites {
        let Some(plan) = build_suite_cache_plan(target_worktree_dir, suite, chief_yaml_hash)?
        else {
            continue;
        };
        report.invalid_paths += plan.invalid_paths;
        report.suites_considered += 1;

        let source_root =
            suite_cache_directory(&cache_root, &plan.suite_cache_dir_name, &plan.cache_key);
        for relative_path in &plan.cache_paths {
            let source_path = source_root.join(relative_path);
            if !source_path.exists() {
                report.missing_cache_paths += 1;
                continue;
            }

            let target_path = plan.suite_root.join(relative_path);
            if path_exists(&target_path)? {
                report.skipped_existing_paths += 1;
                continue;
            }

            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create parent directory {}", parent.display())
                })?;
            }

            match plan.cache_mode {
                SuiteCacheMode::Copy => {
                    copy_path_with_tar_fallback(&source_path, &target_path)?;
                }
                SuiteCacheMode::Symlink => {
                    create_symlink(&source_path, &target_path)
                        .or_else(|_| copy_path_recursive(&source_path, &target_path))?;
                }
            }
            report.linked_paths += 1;
        }
    }

    Ok(report)
}

fn worktree_cache_root_for_project(project_dir: &Path, project_name: &str) -> PathBuf {
    let parent_dir = project_dir.parent().unwrap_or(project_dir);
    parent_dir.join(format!("{project_name}__worktree_cache"))
}

fn suite_cache_directory(cache_root: &Path, suite_name: &str, cache_key: &str) -> PathBuf {
    cache_root.join(suite_name).join(cache_key)
}

#[cfg(test)]
mod tests;
