use super::*;

impl ApiService {
    pub async fn reset_project_workspace(
        &self,
        project: &str,
    ) -> Result<MessageResponse, ApiError> {
        let runtime = self
            .scheduler
            .list_project_views()
            .await
            .into_iter()
            .find(|view| view.name == project)
            .ok_or_else(|| ApiError::not_found(format!("project '{project}' not found")))?;
        if runtime.running {
            return Err(ApiError::unprocessable(
                "project must be stopped before resetting workspace",
            ));
        }

        let mut context = self.project_context(project).await?;
        context.refresh().map_err(ApiError::internal)?;

        let changed_files = context
            .git
            .changed_files(&context.project_dir)
            .map_err(ApiError::internal)?
            .into_iter()
            .filter(|path| !is_internal_workspace_state_file(path))
            .collect::<Vec<_>>();
        if !changed_files.is_empty() {
            run_git_capture(&context.project_dir, &["reset", "--hard", "HEAD"])?;
            run_git_capture(
                &context.project_dir,
                &[
                    "clean",
                    "-fd",
                    "-e",
                    ".chief/chief.db",
                    "-e",
                    ".chief/chief.db-*",
                    "-e",
                    "chief.db",
                    "-e",
                    "chief.db-*",
                ],
            )?;
        }

        let marker_message = format!("{RETRY_CLEANUP_DISCARDED_MSG_PREFIX} manual/1");
        let mut marker_payload = BTreeMap::new();
        marker_payload.insert(
            "files".to_owned(),
            serde_json::Value::Array(
                changed_files
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
        let todo_ids = context
            .store
            .list_todos()
            .map_err(ApiError::internal)?
            .into_iter()
            .filter(|todo| todo.status != TodoStatus::Done)
            .map(|todo| todo.id)
            .collect::<Vec<_>>();
        let run_id = format!("manual-workspace-reset-{}", Uuid::new_v4());
        context
            .store
            .start_run(&run_id)
            .map_err(ApiError::internal)?;

        let log_result = if todo_ids.is_empty() {
            context.log_project_event(
                &run_id,
                None,
                None,
                "warning",
                Some(Phase::Red),
                EventType::GitOp,
                marker_message,
                marker_payload,
            )
        } else {
            for todo_id in &todo_ids {
                context.log_project_event(
                    &run_id,
                    None,
                    Some(todo_id.clone()),
                    "warning",
                    Some(Phase::Red),
                    EventType::GitOp,
                    marker_message.clone(),
                    marker_payload.clone(),
                )?;
            }
            Ok(())
        }
        .map_err(ApiError::internal);

        let run_exit_status = if log_result.is_ok() {
            RunExitStatus::Success
        } else {
            RunExitStatus::Failure
        };
        context
            .store
            .finish_run(&run_id, run_exit_status)
            .map_err(ApiError::internal)?;
        log_result?;

        Ok(MessageResponse {
            message: if changed_files.is_empty() {
                format!(
                    "workspace already clean; recorded reset marker for {} todo(s)",
                    todo_ids.len()
                )
            } else {
                format!(
                    "discarded {} local git change(s); recorded reset marker for {} todo(s)",
                    changed_files.len(),
                    todo_ids.len()
                )
            },
        })
    }

    pub async fn get_chief_yaml(&self, project: &str) -> Result<ChiefYamlResponse, ApiError> {
        let context = self.project_context(project).await?;
        let content = fs::read_to_string(&context.config_path).with_context(|| {
            format!(
                "failed to read chief config at {}",
                context.config_path.display()
            )
        })?;
        Ok(ChiefYamlResponse { content })
    }

    pub async fn update_chief_yaml(
        &self,
        project: &str,
        payload: UpdateChiefYamlRequest,
    ) -> Result<MessageResponse, ApiError> {
        let context = self.project_context(project).await?;
        fs::write(&context.config_path, &payload.content).with_context(|| {
            format!(
                "failed to write chief config at {}",
                context.config_path.display()
            )
        })?;

        if let Err(err) = context.git.commit_paths(
            &context.project_dir,
            &[".chief/chief.yaml"],
            "chore: update .chief/chief.yaml via settings",
        ) {
            info!(
                project,
                error = %err,
                "skipped git commit for .chief/chief.yaml settings update"
            );
        }

        Ok(MessageResponse {
            message: ".chief/chief.yaml updated".to_owned(),
        })
    }

    pub async fn reset_project_db(&self, project: &str) -> Result<MessageResponse, ApiError> {
        let context = self.project_context(project).await?;
        context
            .store
            .reset_db_from_todos_file()
            .map_err(ApiError::internal)?;
        Ok(MessageResponse {
            message: format!("reset .chief/chief.db for project {project}"),
        })
    }

    pub async fn trim_project_db(
        &self,
        project: &str,
        keep_runs: usize,
    ) -> Result<MessageResponse, ApiError> {
        if keep_runs == 0 {
            return Err(ApiError::unprocessable("keep_runs must be at least 1"));
        }
        let context = self.project_context(project).await?;
        let deleted = context
            .store
            .trim_events_to_recent_runs(keep_runs)
            .map_err(ApiError::internal)?;
        Ok(MessageResponse {
            message: format!("trimmed {deleted} events; kept the last {keep_runs} runs"),
        })
    }

    pub async fn project_dir_for_terminal(&self, project: &str) -> Result<PathBuf, ApiError> {
        let context = self.project_context(project).await?;
        Ok(context.project_dir)
    }

    pub async fn project_store_for_events(
        &self,
        project: &str,
    ) -> Result<chief::storage::ProjectStore, ApiError> {
        let context = self.project_context(project).await?;
        Ok(context.store)
    }
}
