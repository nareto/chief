use super::*;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

fn mirror_agent_chunk_to_stdout(text: &str) {
    if text.is_empty() {
        return;
    }

    let mut stdout = io::stdout().lock();
    if stdout.write_all(text.as_bytes()).is_ok() {
        let _ = stdout.flush();
    }
}

impl<'a> FlowExecution<'a> {
    pub fn run_agent(
        &self,
        phase: Phase,
        prompt: String,
        disallowed_paths: Vec<String>,
    ) -> Result<AgentOutput> {
        let permit = self.prepare_agent_call(phase)?;
        self.ensure_not_cancelled()?;

        let query_id = Uuid::new_v4().to_string();
        let project_name = self
            .store
            .project_dir
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("project")
            .to_owned();
        let todo_id = self.work_item_id().to_owned();

        agent_stream::start_query(
            &project_name,
            &query_id,
            &self.run_id,
            &self.job_id,
            &todo_id,
            phase.as_str(),
        );

        if let Err(err) = self.log_event(
            "info",
            Some(phase),
            EventType::AgentPrompt,
            format!("Agent prompt ({})", phase.as_str()),
            payload_from_json(json!({
                "prompt": prompt,
                "agent_query_id": query_id,
            })),
        ) {
            agent_stream::complete_query(&project_name, &query_id, None, Some(err.to_string()));
            return Err(err);
        }

        if let Some(decision) = permit.fixed_wait_decision() {
            if let Err(err) = self.log_agent_fixed_wait_event(phase, decision) {
                agent_stream::complete_query(&project_name, &query_id, None, Some(err.to_string()));
                return Err(err);
            }
        }

        if let Some(decision) = permit.decision() {
            if let Err(err) = self.log_agent_usage_event(phase, decision) {
                agent_stream::complete_query(&project_name, &query_id, None, Some(err.to_string()));
                return Err(err);
            }
        }

        let before_files = self
            .git
            .changed_files(&self.project_dir)
            .unwrap_or_default();

        self.log_event(
            "info",
            Some(phase),
            EventType::AgentCmd,
            format!("Invoking agent ({})", phase.as_str()),
            payload_from_json(json!({
                "agent_name": self.agent.name(),
                "query_id": query_id,
            })),
        )?;

        let stream_project = project_name.clone();
        let stream_query_id = query_id.clone();
        let out = match self.agent.run(AgentRequest {
            prompt,
            cwd: self.project_dir.clone(),
            timeout_seconds: Some(self.chief_config.agent_timeout_seconds),
            disallowed_paths,
            cancel_signal: Some(self.cancel_signal.clone()),
            on_chunk: Some(Arc::new(move |stream, text| {
                agent_stream::push_chunk(&stream_project, &stream_query_id, stream, text);
                mirror_agent_chunk_to_stdout(text);
            })),
        }) {
            Ok(out) => out,
            Err(err) => {
                agent_stream::complete_query(&project_name, &query_id, None, Some(err.to_string()));
                return Err(err);
            }
        };

        agent_stream::complete_query(&project_name, &query_id, Some(out.exit_code), None);

        self.log_event(
            if out.exit_code == 0 {
                "info"
            } else {
                "warning"
            },
            Some(phase),
            EventType::AgentResponse,
            format!("Agent response ({})", phase.as_str()),
            payload_from_json(json!({
                "exit_code": out.exit_code,
                "command": out.command,
                "output": out.merged_output,
                "stdout": out.stdout,
                "stderr": out.stderr,
                "agent_query_id": query_id,
            })),
        )?;

        let after_files = self
            .git
            .changed_files(&self.project_dir)
            .unwrap_or_default();
        let new_files = after_files
            .iter()
            .filter(|file| !before_files.contains(file))
            .cloned()
            .collect::<Vec<_>>();

        let diff_summary = self
            .git
            .diff_summary_for_files(&self.project_dir, &new_files)
            .unwrap_or_default();

        self.log_event(
            "info",
            Some(phase),
            EventType::Diff,
            "Diff after agent run",
            payload_from_json(json!({
                "files": new_files,
                "summary": diff_summary,
            })),
        )?;

        Ok(out)
    }

    pub(in crate::flow) fn run_agent_with_git_changes(
        &self,
        phase: Phase,
        prompt: String,
        disallowed_paths: Vec<String>,
    ) -> Result<AgentRunWithGitChanges> {
        let head_commit_before = self.git.head_commit(&self.project_dir)?;
        let before = self.working_tree_snapshot()?;
        let output = self.run_agent(phase, prompt, disallowed_paths)?;
        let after = self.working_tree_snapshot()?;
        let head_commit_after = self.git.head_commit(&self.project_dir)?;
        let touched_files = changed_paths_between_snapshots(&before, &after);
        let had_git_changes = !touched_files.is_empty();
        let head_commit_changed = head_commit_before != head_commit_after;

        self.log_event(
            "info",
            Some(phase),
            EventType::Diff,
            "Iteration git change detection",
            payload_from_json(json!({
                "touched_files": &touched_files,
                "had_git_changes": had_git_changes,
                "head_commit_before": &head_commit_before,
                "head_commit_after": &head_commit_after,
                "head_commit_changed": head_commit_changed,
            })),
        )?;

        Ok(AgentRunWithGitChanges {
            output,
            touched_files,
            had_git_changes,
            head_commit_before,
            head_commit_after,
            head_commit_changed,
        })
    }

    fn working_tree_snapshot(&self) -> Result<BTreeMap<String, String>> {
        if !self.convergence_watch_paths.is_empty() {
            return self.watch_path_snapshot();
        }

        self.git_changed_files_snapshot()
    }

    fn git_changed_files_snapshot(&self) -> Result<BTreeMap<String, String>> {
        let files = self.git.changed_files(&self.project_dir)?;
        let mut snapshot = BTreeMap::new();
        for file in files {
            let path = self.project_dir.join(&file);
            let signature = if path.is_file() {
                file_signature(&path)?
            } else if path.is_dir() {
                "dir".to_owned()
            } else {
                "missing".to_owned()
            };
            snapshot.insert(file, signature);
        }
        Ok(snapshot)
    }

    fn watch_path_snapshot(&self) -> Result<BTreeMap<String, String>> {
        let mut snapshot = BTreeMap::new();
        let mut seen_watch_paths = BTreeSet::new();

        for raw_watch_path in &self.convergence_watch_paths {
            let Some((watch_key, watch_path)) =
                normalize_watch_path(raw_watch_path, &self.project_dir)
            else {
                continue;
            };

            if !seen_watch_paths.insert(watch_key.clone()) {
                continue;
            }

            capture_path_signature(&watch_key, &watch_path, &mut snapshot)?;
        }

        Ok(snapshot)
    }

    pub(super) fn ensure_not_cancelled(&self) -> Result<()> {
        if self.cancel_signal.load(Ordering::SeqCst) {
            return Err(anyhow!(AgentCancelledError));
        }
        Ok(())
    }
}

fn file_signature(path: &Path) -> Result<String> {
    let content = fs::read(path)
        .with_context(|| format!("failed reading changed file {}", path.display()))?;
    Ok(format!("file:{:x}", md5::compute(content)))
}

fn capture_path_signature(
    key: &str,
    path: &Path,
    snapshot: &mut BTreeMap<String, String>,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            snapshot.insert(key.to_owned(), "missing".to_owned());
            return Ok(());
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!("failed reading watched path metadata {}", path.display())
            });
        }
    };

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path)
            .map(|value| value.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        snapshot.insert(key.to_owned(), format!("symlink:{target}"));
        return Ok(());
    }

    if metadata.is_file() {
        snapshot.insert(key.to_owned(), file_signature(path)?);
        return Ok(());
    }

    if metadata.is_dir() {
        snapshot.insert(key.to_owned(), "dir".to_owned());
        capture_directory_signatures(key, path, snapshot)?;
        return Ok(());
    }

    snapshot.insert(key.to_owned(), "other".to_owned());
    Ok(())
}

fn capture_directory_signatures(
    prefix: &str,
    directory: &Path,
    snapshot: &mut BTreeMap<String, String>,
) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("failed reading watched directory {}", directory.display()))?
    {
        let entry = entry.with_context(|| {
            format!(
                "failed reading entry in watched directory {}",
                directory.display()
            )
        })?;
        let child_name = entry.file_name().to_string_lossy().to_string();
        let child_key = format!("{prefix}/{child_name}");
        let child_path = entry.path();
        capture_path_signature(&child_key, &child_path, snapshot)?;
    }

    Ok(())
}

fn normalize_watch_path(raw_path: &str, project_dir: &Path) -> Option<(String, PathBuf)> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return None;
    }

    let raw = Path::new(trimmed);
    if raw.is_absolute() {
        let path = normalize_absolute_watch_path(raw)?;
        let key = watch_path_key(&path, project_dir);
        return Some((key, path));
    }

    let normalized = normalize_relative_watch_path(raw)?;
    let key = normalized.to_string_lossy().replace('\\', "/");
    Some((key, project_dir.join(normalized)))
}

fn normalize_relative_watch_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    if normalized.as_os_str().is_empty() {
        return None;
    }

    Some(normalized)
}

fn watch_path_key(path: &Path, project_dir: &Path) -> String {
    path.strip_prefix(project_dir)
        .ok()
        .and_then(normalize_relative_watch_path)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| path.to_string_lossy().replace('\\', "/"))
}

fn normalize_absolute_watch_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    let mut normal_components = 0usize;

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Normal(part) => {
                normalized.push(part);
                normal_components += 1;
            }
            Component::ParentDir => {
                if normal_components == 0 || !normalized.pop() {
                    return None;
                }
                normal_components -= 1;
            }
        }
    }

    if normal_components == 0 {
        return None;
    }

    Some(normalized)
}
