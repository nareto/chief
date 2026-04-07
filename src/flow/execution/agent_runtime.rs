use super::*;
use std::io::{self, Write};

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
        let had_git_changes = if self.convergence_watch_paths.is_empty() {
            !touched_files.is_empty()
        } else {
            touched_files.iter().any(|f| {
                self.convergence_watch_paths
                    .iter()
                    .any(|p| f == p || f.starts_with(&format!("{p}/")))
            })
        };
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
        let files = self.git.changed_files(&self.project_dir)?;
        let mut snapshot = BTreeMap::new();
        for file in files {
            let path = self.project_dir.join(&file);
            let signature = if path.is_file() {
                let content = fs::read(&path)
                    .with_context(|| format!("failed reading changed file {}", path.display()))?;
                format!("file:{:x}", md5::compute(content))
            } else if path.is_dir() {
                "dir".to_owned()
            } else {
                "missing".to_owned()
            };
            snapshot.insert(file, signature);
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
