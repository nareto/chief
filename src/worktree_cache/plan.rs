use crate::config::{SuiteCacheMode, TestSuiteConfig};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

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
pub(super) struct SuiteCachePlan {
    pub(super) suite_name: String,
    pub(super) suite_cache_dir_name: String,
    pub(super) suite_root: PathBuf,
    pub(super) cache_paths: Vec<PathBuf>,
    pub(super) cache_key: String,
    pub(super) cache_mode: SuiteCacheMode,
    pub(super) invalid_paths: usize,
}

pub(super) fn build_suite_cache_plan(
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
