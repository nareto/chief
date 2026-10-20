//! Findings contract and helpers for `chief review`.
//!
//! The review agent appends findings to a YAML file with a top-level
//! `findings:` list. Chief validates that file after every iteration and
//! summarizes it at the end of the run.

use crate::git::run_git_command_with_retry;
use crate::paths;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub const REVIEW_DIR_NAME: &str = "review";
pub const DEFAULT_FINDINGS_FILE_NAME: &str = "findings.yaml";
pub const REVIEW_PROMPT_TEMPLATE: &str = "review.md";
/// Candidates tried, in order, when `--base` is not given.
pub const DEFAULT_BASE_CANDIDATES: [&str; 5] = [
    "origin/HEAD",
    "origin/main",
    "origin/master",
    "main",
    "master",
];
/// A review stops after this many consecutive iterations without new findings
/// unless `--required-stable-iterations` overrides it.
pub const DEFAULT_REVIEW_REQUIRED_STABLE_ITERATIONS: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Severity {
    P0,
    P1,
    P2,
    P3,
}

impl Severity {
    pub const ALL: [Severity; 4] = [Severity::P0, Severity::P1, Severity::P2, Severity::P3];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::P0 => "P0",
            Self::P1 => "P1",
            Self::P2 => "P2",
            Self::P3 => "P3",
        }
    }

    /// P0 and P1 findings should stop the merge.
    pub fn blocks_merge(self) -> bool {
        matches!(self, Self::P0 | Self::P1)
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    pub title: String,
    pub why: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingsFile {
    #[serde(default)]
    pub findings: Vec<Finding>,
}

impl FindingsFile {
    pub fn count_by_severity(&self) -> BTreeMap<Severity, usize> {
        let mut counts = Severity::ALL
            .iter()
            .map(|severity| (*severity, 0usize))
            .collect::<BTreeMap<_, _>>();
        for finding in &self.findings {
            *counts.entry(finding.severity).or_default() += 1;
        }
        counts
    }

    pub fn blocking_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.severity.blocks_merge())
            .count()
    }
}

/// Parse findings YAML. Empty or whitespace-only input means "no findings".
pub fn parse_findings(text: &str) -> Result<FindingsFile> {
    if text.trim().is_empty() {
        return Ok(FindingsFile::default());
    }
    let parsed: FindingsFile =
        serde_yaml::from_str(text).context("findings YAML does not match the review contract")?;
    for (index, finding) in parsed.findings.iter().enumerate() {
        let position = index + 1;
        if finding.file.trim().is_empty() {
            bail!("finding #{position} has an empty `file`");
        }
        if finding.title.trim().is_empty() {
            bail!("finding #{position} has an empty `title`");
        }
        if finding.why.trim().is_empty() {
            bail!("finding #{position} has an empty `why`");
        }
        if finding.line == Some(0) {
            bail!("finding #{position} has `line: 0`; lines are 1-based");
        }
    }
    Ok(parsed)
}

/// Read and parse a findings file. A missing file means "no findings".
pub fn load_findings(path: &Path) -> Result<FindingsFile> {
    match fs::read_to_string(path) {
        Ok(text) => parse_findings(&text)
            .with_context(|| format!("invalid findings file {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(FindingsFile::default()),
        Err(err) => {
            Err(err).with_context(|| format!("failed to read findings file {}", path.display()))
        }
    }
}

pub fn review_dir(project_dir: &Path) -> PathBuf {
    paths::chief_dir(project_dir).join(REVIEW_DIR_NAME)
}

pub fn default_findings_path(project_dir: &Path) -> PathBuf {
    review_dir(project_dir).join(DEFAULT_FINDINGS_FILE_NAME)
}

pub fn default_findings_relative_path() -> String {
    format!(
        "{}/{REVIEW_DIR_NAME}/{DEFAULT_FINDINGS_FILE_NAME}",
        paths::CHIEF_DIR_NAME
    )
}

/// Create `.chief/review/` with a `.gitignore` that ignores everything in it,
/// so the default findings file stays untracked in any repository.
pub fn ensure_review_dir_ignored(project_dir: &Path) -> Result<PathBuf> {
    let dir = review_dir(project_dir);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let ignore_path = dir.join(".gitignore");
    if !ignore_path.exists() {
        fs::write(
            &ignore_path,
            "# Created by `chief review`: keep review output out of git.\n*\n",
        )
        .with_context(|| format!("failed to write {}", ignore_path.display()))?;
    }
    Ok(dir)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewBase {
    /// Ref the user asked for, or the default candidate that resolved.
    pub name: String,
    /// `git merge-base <name> HEAD`; the review covers changes since this commit.
    pub merge_base: String,
}

fn git_stdout(project_dir: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = run_git_command_with_retry(project_dir, args)?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    ))
}

fn ref_exists(project_dir: &Path, name: &str) -> Result<bool> {
    let spec = format!("{name}^{{commit}}");
    Ok(git_stdout(project_dir, &["rev-parse", "--verify", "--quiet", &spec])?.is_some())
}

/// Resolve the review base: the requested ref, or the first existing ref from
/// [`DEFAULT_BASE_CANDIDATES`], then its merge-base with `HEAD`.
pub fn resolve_review_base(project_dir: &Path, requested: Option<&str>) -> Result<ReviewBase> {
    let name = match requested.map(str::trim).filter(|value| !value.is_empty()) {
        Some(requested) => {
            if !ref_exists(project_dir, requested)? {
                bail!("review base '{requested}' does not resolve to a commit");
            }
            requested.to_owned()
        }
        None => {
            let mut found = None;
            for candidate in DEFAULT_BASE_CANDIDATES {
                if ref_exists(project_dir, candidate)? {
                    found = Some(candidate.to_owned());
                    break;
                }
            }
            found.ok_or_else(|| {
                anyhow!(
                    "could not find a default review base (tried {}); pass --base <ref>",
                    DEFAULT_BASE_CANDIDATES.join(", ")
                )
            })?
        }
    };

    let merge_base = git_stdout(project_dir, &["merge-base", &name, "HEAD"])?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!("'{name}' and HEAD share no history; fetch more history or pass another --base")
        })?;

    Ok(ReviewBase { name, merge_base })
}

/// Render the findings path the agent should edit, relative to the project
/// when possible.
pub fn display_findings_path(project_dir: &Path, findings_path: &Path) -> String {
    findings_path
        .strip_prefix(project_dir)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| findings_path.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use uuid::Uuid;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chief-review-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .expect("git should run");
        assert!(status.success(), "git {args:?} should succeed");
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]);
        fs::write(dir.join("a.txt"), "a\n").expect("write");
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }

    #[test]
    fn parses_full_contract() {
        let parsed = parse_findings(
            "findings:\n  - severity: P1\n    file: src/lib.rs\n    line: 12\n    title: Unchecked unwrap\n    why: Panics on empty input.\n    suggestion: Return an error instead.\n  - severity: P3\n    file: README.md\n    title: Typo\n    why: Reads badly.\n",
        )
        .expect("valid findings should parse");
        assert_eq!(parsed.findings.len(), 2);
        assert_eq!(parsed.findings[0].severity, Severity::P1);
        assert_eq!(parsed.findings[0].line, Some(12));
        assert_eq!(parsed.findings[1].line, None);
        assert_eq!(parsed.findings[1].suggestion, None);
        assert_eq!(parsed.blocking_count(), 1);
        let counts = parsed.count_by_severity();
        assert_eq!(counts[&Severity::P0], 0);
        assert_eq!(counts[&Severity::P1], 1);
        assert_eq!(counts[&Severity::P3], 1);
    }

    #[test]
    fn empty_input_and_empty_list_mean_no_findings() {
        assert!(parse_findings("").expect("empty").findings.is_empty());
        assert!(
            parse_findings("findings: []\n")
                .expect("empty list")
                .findings
                .is_empty()
        );
    }

    #[test]
    fn rejects_unknown_severity_and_missing_fields() {
        let bad_severity = parse_findings(
            "findings:\n  - severity: high\n    file: a\n    title: t\n    why: w\n",
        );
        assert!(bad_severity.is_err());
        let missing_why =
            parse_findings("findings:\n  - severity: P2\n    file: a\n    title: t\n");
        assert!(missing_why.is_err());
        let empty_title = parse_findings(
            "findings:\n  - severity: P2\n    file: a\n    title: ' '\n    why: w\n",
        );
        assert!(empty_title.is_err());
        let zero_line = parse_findings(
            "findings:\n  - severity: P2\n    file: a\n    line: 0\n    title: t\n    why: w\n",
        );
        assert!(zero_line.is_err());
    }

    #[test]
    fn missing_findings_file_loads_as_empty() {
        let dir = temp_dir("missing");
        let loaded = load_findings(&dir.join("nope.yaml")).expect("missing file is empty");
        assert!(loaded.findings.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn review_dir_ignores_its_own_contents() {
        let dir = temp_dir("ignore");
        init_repo(&dir);
        let review = ensure_review_dir_ignored(&dir).expect("review dir should be created");
        fs::write(review.join(DEFAULT_FINDINGS_FILE_NAME), "findings: []\n").expect("write");
        let status = Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=all"])
            .current_dir(&dir)
            .output()
            .expect("git status");
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "review output should be ignored by git"
        );
        assert_eq!(
            default_findings_path(&dir),
            dir.join(".chief/review/findings.yaml")
        );
        assert_eq!(
            default_findings_relative_path(),
            ".chief/review/findings.yaml"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resolves_explicit_and_default_base() {
        let dir = temp_dir("base");
        init_repo(&dir);
        let main_head = String::from_utf8_lossy(
            &Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&dir)
                .output()
                .expect("rev-parse")
                .stdout,
        )
        .trim()
        .to_owned();
        git(&dir, &["checkout", "-q", "-b", "feature"]);
        fs::write(dir.join("b.txt"), "b\n").expect("write");
        git(&dir, &["add", "b.txt"]);
        git(&dir, &["commit", "-q", "-m", "feature"]);

        let default_base = resolve_review_base(&dir, None).expect("main should be found");
        assert_eq!(default_base.name, "main");
        assert_eq!(default_base.merge_base, main_head);

        let explicit = resolve_review_base(&dir, Some("main")).expect("explicit base");
        assert_eq!(explicit.merge_base, main_head);

        let missing = resolve_review_base(&dir, Some("does-not-exist"));
        assert!(missing.is_err());
        let _ = fs::remove_dir_all(dir);
    }
}
