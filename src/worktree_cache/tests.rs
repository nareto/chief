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
        let path =
            std::env::temp_dir().join(format!("chief-worktree-cache-{label}-{}", Uuid::new_v4()));
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
    let metadata = fs::symlink_metadata(&hydrated).expect("hydrated cache path should be present");
    #[cfg(unix)]
    assert!(
        !metadata.file_type().is_symlink(),
        "default copy mode should not create a symlink"
    );
    #[cfg(not(unix))]
    assert!(metadata.is_dir() || metadata.is_file());
    let hydrated_file = target_worktree.join("frontend/node_modules/pkg/index.js");
    let hydrated_content =
        fs::read_to_string(&hydrated_file).expect("copied cache path should contain expected file");
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
    let metadata = fs::symlink_metadata(&hydrated).expect("hydrated cache path should be present");
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
