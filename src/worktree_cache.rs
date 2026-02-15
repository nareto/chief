use crate::config::{SuiteCacheMode, TestSuiteConfig};
use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

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

#[derive(Debug, Serialize)]
struct SuiteCacheKeyFileFingerprint {
    path: String,
    digest: String,
}

#[derive(Debug, Serialize)]
struct SuiteCacheKeyFingerprint {
    suite_name: String,
    test_root: String,
    test_init: Option<String>,
    test_setup: Option<String>,
    cache_paths: Vec<String>,
    cache_key_files: Vec<SuiteCacheKeyFileFingerprint>,
    cache_mode: String,
    chief_yaml_hash: String,
}

#[derive(Debug)]
struct SuiteCachePlan {
    suite_name: String,
    suite_cache_dir_name: String,
    suite_root: PathBuf,
    cache_paths: Vec<PathBuf>,
    cache_key: String,
    cache_mode: SuiteCacheMode,
    invalid_paths: usize,
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

fn build_suite_cache_plan(
    worktree_dir: &Path,
    suite: &TestSuiteConfig,
    chief_yaml_hash: &str,
) -> Result<Option<SuiteCachePlan>> {
    let mut invalid_paths = 0usize;
    let cache_paths = normalize_relative_paths(&suite.cache_paths, &mut invalid_paths);
    if cache_paths.is_empty() {
        return Ok(None);
    }

    let suite_root = worktree_dir.join(&suite.test_root);
    let cache_key_files = normalize_relative_paths(&suite.cache_key_files, &mut invalid_paths);
    let key_fingerprint = SuiteCacheKeyFingerprint {
        suite_name: suite.name.clone(),
        test_root: suite.test_root.clone(),
        test_init: suite.test_init.clone(),
        test_setup: suite.test_setup.clone(),
        cache_paths: cache_paths
            .iter()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect(),
        cache_key_files: cache_key_files
            .iter()
            .map(|relative_path| SuiteCacheKeyFileFingerprint {
                path: relative_path.to_string_lossy().replace('\\', "/"),
                digest: digest_for_cache_key_file(&suite_root.join(relative_path)),
            })
            .collect(),
        cache_mode: suite.cache_mode.as_str().to_owned(),
        chief_yaml_hash: chief_yaml_hash.to_owned(),
    };
    let key = format!(
        "{:x}",
        md5::compute(serde_json::to_vec(&key_fingerprint).unwrap_or_default())
    );

    Ok(Some(SuiteCachePlan {
        suite_name: suite.name.clone(),
        suite_cache_dir_name: sanitize_path_component(&suite.name),
        suite_root,
        cache_paths,
        cache_key: key,
        cache_mode: suite.cache_mode,
        invalid_paths,
    }))
}

fn normalize_relative_paths(paths: &[String], invalid_paths: &mut usize) -> Vec<PathBuf> {
    let mut unique = BTreeSet::new();
    let mut normalized = Vec::new();

    for raw in paths {
        let Some(path) = normalize_relative_path(raw) else {
            *invalid_paths += 1;
            continue;
        };
        let rendered = path.to_string_lossy().replace('\\', "/");
        if unique.insert(rendered) {
            normalized.push(path);
        }
    }

    normalized
}

fn normalize_relative_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let path = Path::new(trimmed);
    if path.is_absolute() {
        return None;
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

fn digest_for_cache_key_file(path: &Path) -> String {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::read(path)
            .map(|bytes| format!("symlink-file:{:x}", md5::compute(bytes)))
            .unwrap_or_else(|_| "symlink".to_owned()),
        Ok(metadata) if metadata.is_file() => fs::read(path)
            .map(|bytes| format!("file:{:x}", md5::compute(bytes)))
            .unwrap_or_else(|_| "file-unreadable".to_owned()),
        Ok(metadata) if metadata.is_dir() => "dir".to_owned(),
        Ok(_) => "other".to_owned(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => "missing".to_owned(),
        Err(_) => "error".to_owned(),
    }
}

fn worktree_cache_root_for_project(project_dir: &Path, project_name: &str) -> PathBuf {
    let parent_dir = project_dir.parent().unwrap_or(project_dir);
    parent_dir.join(format!("{project_name}__worktree_cache"))
}

fn suite_cache_directory(cache_root: &Path, suite_name: &str, cache_key: &str) -> PathBuf {
    cache_root.join(suite_name).join(cache_key)
}

fn sanitize_path_component(value: &str) -> String {
    let out = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if out.is_empty() {
        "suite".to_owned()
    } else {
        out
    }
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed to inspect existing path before cache hydration {}",
                path.display()
            )
        }),
    }
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("failed reading {}", path.display())),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
            .with_context(|| format!("failed removing directory {}", path.display()))?;
    } else {
        fs::remove_file(path).with_context(|| format!("failed removing {}", path.display()))?;
    }
    Ok(())
}

fn copy_path_recursive(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed reading {}", source.display()))?;

    if metadata.file_type().is_symlink() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed creating directory {}", parent.display()))?;
        }
        let target = fs::read_link(source)
            .with_context(|| format!("failed reading symlink {}", source.display()))?;
        create_raw_symlink(&target, destination)
            .with_context(|| format!("failed creating symlink {}", destination.display()))?;
        return Ok(());
    }

    if metadata.is_dir() {
        fs::create_dir_all(destination)
            .with_context(|| format!("failed creating directory {}", destination.display()))?;
        for entry in fs::read_dir(source)
            .with_context(|| format!("failed reading directory {}", source.display()))?
        {
            let entry = entry.with_context(|| {
                format!("failed reading directory entry under {}", source.display())
            })?;
            let nested_source = entry.path();
            let nested_destination = destination.join(entry.file_name());
            copy_path_recursive(&nested_source, &nested_destination)?;
        }
        return Ok(());
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating directory {}", parent.display()))?;
    }
    fs::copy(source, destination).with_context(|| {
        format!(
            "failed copying {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn copy_path_with_tar_fallback(source: &Path, destination: &Path) -> Result<()> {
    if let Err(_err) = copy_path_with_tar(source, destination) {
        copy_path_recursive(source, destination)?;
    }
    Ok(())
}

fn copy_path_with_tar(source: &Path, destination: &Path) -> Result<()> {
    let source_parent = source.parent().ok_or_else(|| {
        anyhow!(
            "cannot copy {} with tar pipe because parent directory is missing",
            source.display()
        )
    })?;
    let source_name = source.file_name().ok_or_else(|| {
        anyhow!(
            "cannot copy {} with tar pipe because file name is missing",
            source.display()
        )
    })?;
    let destination_parent = destination.parent().ok_or_else(|| {
        anyhow!(
            "cannot copy {} with tar pipe because destination parent is missing",
            destination.display()
        )
    })?;

    fs::create_dir_all(destination_parent).with_context(|| {
        format!(
            "failed creating destination parent directory {}",
            destination_parent.display()
        )
    })?;

    let mut producer = Command::new("tar")
        .arg("-cf")
        .arg("-")
        .arg("-C")
        .arg(source_parent)
        .arg(source_name)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed starting tar producer for {}", source.display()))?;
    let producer_stdout = producer
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed attaching tar producer stdout"))?;

    let consumer = Command::new("tar")
        .arg("-xf")
        .arg("-")
        .arg("-C")
        .arg(destination_parent)
        .stdin(Stdio::from(producer_stdout))
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "failed starting tar consumer for destination {}",
                destination_parent.display()
            )
        })?;

    let consumer_output = consumer
        .wait_with_output()
        .context("failed waiting for tar consumer process")?;
    let producer_output = producer
        .wait_with_output()
        .context("failed waiting for tar producer process")?;

    if !producer_output.status.success() || !consumer_output.status.success() {
        return Err(anyhow!(
            "tar pipe copy failed for {} -> {} (producer_exit={:?}, consumer_exit={:?}, producer_stderr={}, consumer_stderr={})",
            source.display(),
            destination.display(),
            producer_output.status.code(),
            consumer_output.status.code(),
            String::from_utf8_lossy(&producer_output.stderr).trim(),
            String::from_utf8_lossy(&consumer_output.stderr).trim()
        ));
    }

    if !destination.exists() {
        return Err(anyhow!(
            "tar pipe copy reported success but destination is missing: {}",
            destination.display()
        ));
    }
    Ok(())
}

fn create_symlink(source: &Path, destination: &Path) -> Result<()> {
    create_raw_symlink(source, destination).with_context(|| {
        format!(
            "failed creating symlink {} -> {}",
            destination.display(),
            source.display()
        )
    })
}

#[cfg(unix)]
fn create_raw_symlink(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, destination)
}

#[cfg(windows)]
fn create_raw_symlink(source: &Path, destination: &Path) -> std::io::Result<()> {
    let metadata = fs::metadata(source)?;
    if metadata.is_dir() {
        std::os::windows::fs::symlink_dir(source, destination)
    } else {
        std::os::windows::fs::symlink_file(source, destination)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        file_content_md5, hydrate_suite_caches_into_worktree, prime_suite_caches_from_worktree,
        suite_cache_inputs_hash,
    };
    use crate::config::{SuiteCacheMode, TestSuiteConfig};
    use crate::domain::TargetType;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use uuid::Uuid;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("chief-worktree-cache-{label}-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("failed creating temp dir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn suite_fixture() -> TestSuiteConfig {
        TestSuiteConfig {
            name: "frontend".to_owned(),
            language: "TypeScript".to_owned(),
            framework: "Vitest".to_owned(),
            test_root: "frontend".to_owned(),
            test_command: "npm test".to_owned(),
            target_type: TargetType::Project,
            default_target: None,
            file_patterns: Vec::new(),
            disallow_write_globs: Vec::new(),
            test_init: Some("npm ci".to_owned()),
            test_setup: None,
            cache_paths: vec!["node_modules".to_owned()],
            cache_key_files: vec!["package-lock.json".to_owned()],
            cache_mode: SuiteCacheMode::Copy,
            post_green_command: None,
            cleanup_command: None,
            command_timeout_seconds: None,
            lint_command: None,
            lint_fix_command: None,
            env: BTreeMap::new(),
            strip_root_from_target: true,
        }
    }

    #[test]
    fn suite_cache_inputs_hash_changes_when_lockfile_changes() {
        let temp = TempDir::new("inputs-hash");
        let project_dir = temp.path.join("project");
        fs::create_dir_all(project_dir.join("frontend")).expect("failed creating suite root");
        fs::write(project_dir.join("frontend/package-lock.json"), "v1")
            .expect("failed writing lockfile");
        let suite = suite_fixture();
        let chief_yaml_hash = "abc123";

        let before =
            suite_cache_inputs_hash(&project_dir, std::slice::from_ref(&suite), chief_yaml_hash);
        fs::write(project_dir.join("frontend/package-lock.json"), "v2")
            .expect("failed updating lockfile");
        let after =
            suite_cache_inputs_hash(&project_dir, std::slice::from_ref(&suite), chief_yaml_hash);
        assert_ne!(before, after);
    }

    #[test]
    fn prime_then_hydrate_copies_cache_by_default() {
        let temp = TempDir::new("prime-hydrate");
        let project_dir = temp.path.join("project");
        let source_worktree = temp.path.join("source");
        let target_worktree = temp.path.join("target");
        fs::create_dir_all(source_worktree.join("frontend/node_modules/pkg"))
            .expect("failed creating source cache path");
        fs::create_dir_all(target_worktree.join("frontend"))
            .expect("failed creating target suite root");
        fs::write(source_worktree.join("frontend/package-lock.json"), "locked")
            .expect("failed writing source lockfile");
        fs::write(target_worktree.join("frontend/package-lock.json"), "locked")
            .expect("failed writing target lockfile");
        fs::write(
            source_worktree.join("frontend/node_modules/pkg/index.js"),
            "module.exports = 1;\n",
        )
        .expect("failed writing cached dependency fixture");

        let suite = suite_fixture();
        let chief_yaml_hash = "chief-hash";

        let prime = prime_suite_caches_from_worktree(
            &project_dir,
            "demo",
            std::slice::from_ref(&suite),
            &source_worktree,
            chief_yaml_hash,
        )
        .expect("prime should succeed");
        assert_eq!(prime.cached_paths, 1);

        let hydrate = hydrate_suite_caches_into_worktree(
            &project_dir,
            "demo",
            std::slice::from_ref(&suite),
            &target_worktree,
            chief_yaml_hash,
        )
        .expect("hydrate should succeed");
        assert_eq!(hydrate.linked_paths, 1);

        let hydrated = target_worktree.join("frontend/node_modules");
        let metadata =
            fs::symlink_metadata(&hydrated).expect("hydrated cache path should be present");
        #[cfg(unix)]
        assert!(
            !metadata.file_type().is_symlink(),
            "default copy mode should not create a symlink"
        );
        #[cfg(not(unix))]
        assert!(metadata.is_dir() || metadata.is_file());
        let hydrated_file = target_worktree.join("frontend/node_modules/pkg/index.js");
        let hydrated_content = fs::read_to_string(&hydrated_file)
            .expect("copied cache path should contain expected file");
        assert_eq!(hydrated_content, "module.exports = 1;\n");
    }

    #[test]
    fn prime_then_hydrate_uses_symlink_mode_when_configured() {
        let temp = TempDir::new("prime-hydrate-symlink");
        let project_dir = temp.path.join("project");
        let source_worktree = temp.path.join("source");
        let target_worktree = temp.path.join("target");
        fs::create_dir_all(source_worktree.join("frontend/node_modules/pkg"))
            .expect("failed creating source cache path");
        fs::create_dir_all(target_worktree.join("frontend"))
            .expect("failed creating target suite root");
        fs::write(source_worktree.join("frontend/package-lock.json"), "locked")
            .expect("failed writing source lockfile");
        fs::write(target_worktree.join("frontend/package-lock.json"), "locked")
            .expect("failed writing target lockfile");
        fs::write(
            source_worktree.join("frontend/node_modules/pkg/index.js"),
            "module.exports = 2;\n",
        )
        .expect("failed writing cached dependency fixture");

        let mut suite = suite_fixture();
        suite.cache_mode = SuiteCacheMode::Symlink;
        let chief_yaml_hash = "chief-hash";

        prime_suite_caches_from_worktree(
            &project_dir,
            "demo",
            std::slice::from_ref(&suite),
            &source_worktree,
            chief_yaml_hash,
        )
        .expect("prime should succeed");

        let hydrate = hydrate_suite_caches_into_worktree(
            &project_dir,
            "demo",
            std::slice::from_ref(&suite),
            &target_worktree,
            chief_yaml_hash,
        )
        .expect("hydrate should succeed");
        assert_eq!(hydrate.linked_paths, 1);

        let hydrated = target_worktree.join("frontend/node_modules");
        let metadata =
            fs::symlink_metadata(&hydrated).expect("hydrated cache path should be present");
        #[cfg(unix)]
        assert!(
            metadata.file_type().is_symlink(),
            "symlink mode should create symlinked hydration paths"
        );
    }

    #[test]
    fn file_content_md5_hashes_file_bytes() {
        let temp = TempDir::new("file-hash");
        let path = temp.path.join("data.txt");
        fs::write(&path, "hello").expect("failed writing fixture file");
        let digest = file_content_md5(&path).expect("hash should compute");
        assert_eq!(digest, "5d41402abc4b2a76b9719d911017c592");
    }
}
