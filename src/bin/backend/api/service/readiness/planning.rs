use super::*;

pub(super) fn build_readiness_command_plans(
    context: &chief::service::ProjectContext,
    project_dir: &Path,
) -> Vec<ReadinessCommandPlan> {
    let mut plans = Vec::new();
    let default_timeout = context.chief_yaml.chief.suite_command_timeout_seconds;

    for suite in &context.chief_yaml.suites {
        let mut target_candidates = collect_readiness_targets(project_dir, suite);
        if let Some(default_target) = normalized_default_target(suite)
            && !target_candidates.contains(&default_target)
        {
            target_candidates.push(default_target);
        }
        let timeout_seconds = suite
            .command_timeout_seconds
            .unwrap_or(default_timeout)
            .max(1);
        let cwd = suite_command_cwd(project_dir, suite);
        let cwd_display = cwd.display().to_string();

        if let Some(command_template) = suite
            .test_init
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_owned)
        {
            plans.push(ReadinessCommandPlan {
                suite_name: suite.name.clone(),
                kind: ReadinessCommandKind::TestInit,
                uses_target_placeholder: command_template.contains("{target}"),
                command_template,
                cleanup_command: None,
                target_candidates: target_candidates.clone(),
                cwd: cwd.clone(),
                cwd_display: cwd_display.clone(),
                env: suite.env.clone(),
                timeout_seconds,
            });
        }

        if let Some(command_template) = suite
            .test_setup
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_owned)
        {
            plans.push(ReadinessCommandPlan {
                suite_name: suite.name.clone(),
                kind: ReadinessCommandKind::TestSetup,
                uses_target_placeholder: command_template.contains("{target}"),
                command_template,
                cleanup_command: None,
                target_candidates: target_candidates.clone(),
                cwd: cwd.clone(),
                cwd_display: cwd_display.clone(),
                env: suite.env.clone(),
                timeout_seconds,
            });
        }

        if let Some(command_template) = suite
            .lint_command
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_owned)
        {
            plans.push(ReadinessCommandPlan {
                suite_name: suite.name.clone(),
                kind: ReadinessCommandKind::Lint,
                uses_target_placeholder: command_template.contains("{target}"),
                command_template,
                cleanup_command: None,
                target_candidates: target_candidates.clone(),
                cwd: cwd.clone(),
                cwd_display: cwd_display.clone(),
                env: suite.env.clone(),
                timeout_seconds,
            });
        }

        if let Some(command_template) =
            Some(suite.test_command.trim().to_owned()).filter(|command| !command.is_empty())
        {
            plans.push(ReadinessCommandPlan {
                suite_name: suite.name.clone(),
                kind: ReadinessCommandKind::Test,
                uses_target_placeholder: command_template.contains("{target}"),
                command_template,
                cleanup_command: suite.cleanup_command.clone(),
                target_candidates,
                cwd,
                cwd_display,
                env: suite.env.clone(),
                timeout_seconds,
            });
        }
    }

    plans
}

pub(super) fn should_run_readiness_check(
    store: &ProjectStore,
    chief_yaml_hash: &str,
    suite_cache_inputs_hash: &str,
) -> anyhow::Result<bool> {
    let readiness = store.get_readiness_state()?;
    if readiness.status != ReadinessStatus::Ready {
        return Ok(true);
    }
    let previous_hash = readiness_chief_yaml_hash(&readiness.details);
    if previous_hash != Some(chief_yaml_hash) {
        return Ok(true);
    }
    let previous_suite_cache_hash = readiness_suite_cache_inputs_hash(&readiness.details);
    Ok(previous_suite_cache_hash
        .map(|value| value != suite_cache_inputs_hash)
        .unwrap_or(false))
}

pub(super) fn replace_target_placeholder(command_template: &str, target: &str) -> String {
    command_template.replace("{target}", target)
}

fn normalized_default_target(suite: &TestSuiteConfig) -> Option<String> {
    suite
        .default_target
        .as_deref()
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(str::to_owned)
}

fn collect_readiness_targets(project_dir: &Path, suite: &TestSuiteConfig) -> Vec<String> {
    let patterns = suite
        .file_patterns
        .iter()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
        .filter_map(|pattern| glob::Pattern::new(pattern).ok())
        .collect::<Vec<_>>();

    if patterns.is_empty() {
        return Vec::new();
    }

    let tracked_files = git_list_tracked_files(project_dir, suite.test_root.trim());
    if tracked_files.is_empty() {
        return Vec::new();
    }

    let root_prefix = normalized_root_prefix(suite.test_root.trim());
    let mut targets = std::collections::BTreeSet::new();
    for file in tracked_files {
        let relative = strip_root_prefix(&file, root_prefix.as_deref()).unwrap_or(file.as_str());
        let matches_pattern = patterns
            .iter()
            .any(|pattern| pattern.matches(relative) || pattern.matches(&file));
        if !matches_pattern {
            continue;
        }

        let selected = if suite.strip_root_from_target {
            relative.to_owned()
        } else {
            file.clone()
        };
        if !selected.trim().is_empty() {
            targets.insert(selected);
        }
    }
    targets.into_iter().collect()
}

fn readiness_suite_cache_inputs_hash(details: &serde_json::Value) -> Option<&str> {
    details
        .get("suite_cache_inputs_hash")
        .and_then(serde_json::Value::as_str)
}

fn git_list_tracked_files(project_dir: &Path, test_root: &str) -> Vec<String> {
    let output = if test_root.is_empty() || test_root == "." {
        run_git_command_with_retry(project_dir, &["ls-files", "--"])
    } else {
        run_git_command_with_retry(project_dir, &["ls-files", "--", test_root])
    };

    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    let mut files = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn normalized_root_prefix(test_root: &str) -> Option<String> {
    let trimmed = test_root.trim();
    if trimmed.is_empty() || trimmed == "." {
        return None;
    }
    Some(trimmed.trim_end_matches('/').to_owned())
}

fn strip_root_prefix<'a>(path: &'a str, root_prefix: Option<&str>) -> Option<&'a str> {
    let Some(root_prefix) = root_prefix else {
        return Some(path);
    };

    if path == root_prefix {
        return Some("");
    }
    let prefix = format!("{root_prefix}/");
    path.strip_prefix(&prefix)
}
