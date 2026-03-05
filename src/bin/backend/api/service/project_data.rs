use super::*;

impl ApiService {
    pub async fn get_jobs(&self, project: &str) -> Result<JobsResponse, ApiError> {
        let context = self.project_context(project).await?;
        let jobs = context.store.list_jobs(200).map_err(ApiError::internal)?;
        Ok(JobsResponse { jobs })
    }

    pub async fn get_logs(
        &self,
        project: &str,
        query: LogQuery,
    ) -> Result<EventsResponse, ApiError> {
        let context = self.project_context(project).await?;

        let event_type = query
            .event_type
            .as_deref()
            .map(parse_event_type)
            .transpose()?;

        let phase = query.phase.as_deref().map(parse_phase).transpose()?;

        let events = context
            .store
            .query_events(EventQuery {
                limit: query.limit.unwrap_or(200),
                event_type,
                phase,
                level: query.level,
                contains_text: query.q,
            })
            .map_err(ApiError::internal)?;

        Ok(EventsResponse { events })
    }

    pub async fn process_requirements(
        &self,
        project: &str,
        payload: RequirementsRequest,
    ) -> Result<RequirementsResponse, ApiError> {
        let context = self.project_context(project).await?;
        let engine = ChiefEngine::new(context.clone());

        let diff = tokio::task::spawn_blocking(move || {
            engine.process_requirements(&payload.text, payload.model)
        })
        .await
        .map_err(|err| ApiError::internal(anyhow!(err.to_string())))?
        .map_err(ApiError::internal)?;

        Ok(RequirementsResponse { diff })
    }

    pub async fn get_state(&self, project: &str) -> Result<StateResponse, ApiError> {
        let mut context = self.project_context(project).await?;
        context.refresh().map_err(ApiError::internal)?;
        let views = self.scheduler.list_project_views().await;
        let runtime = views.into_iter().find(|view| view.name == project);

        let todos = context.store.list_todos().map_err(ApiError::internal)?;
        let jobs = context.store.list_jobs(200).map_err(ApiError::internal)?;
        let recent_events = context
            .store
            .query_events(EventQuery {
                limit: 200,
                ..EventQuery::default()
            })
            .map_err(ApiError::internal)?;

        let current_phase = recent_events
            .iter()
            .find_map(|event| event.phase.map(Phase::as_str))
            .unwrap_or(Phase::Start.as_str())
            .to_owned();

        let phase_iteration = recent_events.iter().find_map(|event| {
            if event.event_type == EventType::PhaseChange {
                parse_loop_iteration(&event.msg)
            } else {
                None
            }
        });

        let dirty_files = context
            .git
            .changed_files(&context.project_dir)
            .map_err(ApiError::internal)?;
        let chief_db_size_bytes = fs::metadata(&context.store.db_path)
            .map(|metadata| metadata.len())
            .ok();
        let readiness = context
            .store
            .get_readiness_state()
            .map_err(ApiError::internal)?;

        let active_job = jobs
            .iter()
            .find(|job| {
                matches!(
                    job.status,
                    JobStatus::Queued
                        | JobStatus::Selecting
                        | JobStatus::Running
                        | JobStatus::Merging
                )
            })
            .map(|job| ActiveJobResponse {
                job_id: job.id.clone(),
                todo_id: job.todo_id.clone(),
                worker_index: job.worker_index,
                status: job.status.as_str().to_owned(),
            });

        let completed_todos = todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::Done)
            .count();
        let available_todos = todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::Pending)
            .count();
        let last_done_todo_committed_at =
            resolve_last_done_todo_committed_at(&context.git, &context.project_dir, &todos);

        let configured_flow_name = FlowKind::resolve_name(&context.chief_yaml.chief.flow);

        Ok(StateResponse {
            project: project.to_owned(),
            running: runtime.as_ref().map(|view| view.running).unwrap_or(false),
            stop_requested: runtime
                .as_ref()
                .map(|view| view.stop_requested)
                .unwrap_or(false),
            stop_mode: runtime
                .as_ref()
                .map(|view| view.stop_mode)
                .unwrap_or(StopMode::None),
            active_agents: runtime
                .as_ref()
                .map(|view| view.active_workers)
                .unwrap_or(0),
            desired_agents: runtime
                .as_ref()
                .map(|view| view.desired_agents)
                .unwrap_or(1),
            flow_name: runtime
                .as_ref()
                .map(|view| view.flow_name.clone())
                .unwrap_or_else(|| configured_flow_name.clone()),
            last_error: runtime.as_ref().and_then(|view| view.last_error.clone()),
            phase: current_phase,
            phase_iteration,
            last_activity: recent_events
                .first()
                .map(|event| event.timestamp.to_rfc3339()),
            last_done_todo_committed_at,
            chief_db_size_bytes,
            dirty_files,
            todos: TodoProgress {
                available: available_todos,
                completed: completed_todos,
                total: todos.len(),
            },
            active_job,
            readiness: project_readiness_response(readiness),
        })
    }

    pub async fn get_events(
        &self,
        project: &str,
        query: EventsQuery,
    ) -> Result<EventsResponse, ApiError> {
        let context = self.project_context(project).await?;
        let limit = query.limit.unwrap_or(50).clamp(1, 500);
        let sample_size = (limit.saturating_mul(8)).min(1_000);
        let requested_types = parse_requested_types(query.types.as_deref());

        let events = context
            .store
            .query_events(EventQuery {
                limit: sample_size,
                contains_text: query.q,
                ..EventQuery::default()
            })
            .map_err(ApiError::internal)?;

        let filtered = events
            .into_iter()
            .filter(|event| matches_requested_type(event.event_type, &requested_types))
            .take(limit)
            .collect::<Vec<_>>();

        Ok(EventsResponse { events: filtered })
    }

    pub async fn get_file_diff(
        &self,
        project: &str,
        query: FileDiffQuery,
    ) -> Result<FileDiffResponse, ApiError> {
        let context = self.project_context(project).await?;
        let file = query.file.unwrap_or_default().trim().to_owned();

        let diff = if file.is_empty() {
            context
                .git
                .diff(&context.project_dir, None)
                .map_err(ApiError::internal)?
        } else {
            run_git_capture(&context.project_dir, &["diff", "--", &file])?
        };

        Ok(FileDiffResponse { file, diff })
    }
}
