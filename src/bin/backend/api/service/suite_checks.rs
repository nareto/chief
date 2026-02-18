use super::*;

struct SuiteCheckPlan {
    suite_name: String,
    kind: SuiteCommandKind,
    command: String,
    cleanup_command: Option<String>,
    cwd: PathBuf,
    cwd_display: String,
    env: BTreeMap<String, String>,
    timeout_seconds: u64,
}

struct SuiteCheckExecution {
    plan: SuiteCheckPlan,
    git: ShellGitOps,
    worktree: TempWorktree,
}

fn log_cleanup_result(
    project: &str,
    suite_name: &str,
    kind_label: &str,
    cleanup_result: anyhow::Result<Option<chief::domain::AgentOutput>>,
    after_command_failure: bool,
) {
    let finished_msg = if after_command_failure {
        "suite cleanup command finished after command failure"
    } else {
        "suite cleanup command finished"
    };
    let failed_msg = if after_command_failure {
        "suite cleanup command failed after command failure"
    } else {
        "suite cleanup command failed"
    };
    let execution_failed_msg = if after_command_failure {
        "suite cleanup command execution failed after command failure"
    } else {
        "suite cleanup command execution failed"
    };

    match cleanup_result {
        Ok(Some(cleanup_out)) => {
            if cleanup_out.exit_code == 0 {
                info!(
                    project,
                    suite = %suite_name,
                    kind = %kind_label,
                    command = %cleanup_out.command,
                    "{finished_msg}"
                );
            } else {
                warn!(
                    project,
                    suite = %suite_name,
                    kind = %kind_label,
                    command = %cleanup_out.command,
                    exit_code = cleanup_out.exit_code,
                    "{failed_msg}"
                );
            }
        }
        Ok(None) => {}
        Err(err) => {
            warn!(
                project,
                suite = %suite_name,
                kind = %kind_label,
                error = %err,
                "{execution_failed_msg}"
            );
        }
    }
}

impl ApiService {
    pub async fn run_suite_check(
        &self,
        project: &str,
        payload: RunSuiteCheckRequest,
    ) -> Result<RunSuiteCheckResponse, ApiError> {
        let SuiteCheckExecution {
            plan:
                SuiteCheckPlan {
                    suite_name,
                    kind,
                    command,
                    cleanup_command,
                    cwd,
                    cwd_display,
                    env,
                    timeout_seconds,
                },
            git,
            worktree,
        } = self
            .prepare_suite_check_execution(project, &payload)
            .await?;
        let kind_label = kind.as_str();
        info!(
            project,
            suite = %suite_name,
            kind = %kind_label,
            cwd = %cwd_display,
            command = %command,
            "running suite check command"
        );
        let cancel_signal = Arc::new(AtomicBool::new(false));

        let output = tokio::task::spawn_blocking(move || {
            let test_result =
                execute_suite_command(&command, &cwd, &env, &cancel_signal, Some(timeout_seconds));
            let cleanup_result = if kind == SuiteCommandKind::Test {
                execute_suite_cleanup_command(
                    cleanup_command.as_deref(),
                    &cwd,
                    &env,
                    Some(timeout_seconds),
                )
            } else {
                Ok(None)
            };
            (test_result, cleanup_result)
        })
        .await;

        let response = match output {
            Ok((Ok(output), cleanup_result)) => {
                log_cleanup_result(project, &suite_name, kind_label, cleanup_result, false);
                info!(
                    project,
                    suite = %suite_name,
                    kind = %kind_label,
                    exit_code = output.exit_code,
                    stdout_len = output.stdout.len(),
                    stderr_len = output.stderr.len(),
                    "suite check command finished"
                );

                Ok(RunSuiteCheckResponse {
                    suite: suite_name,
                    kind,
                    command: output.command,
                    cwd: cwd_display,
                    exit_code: output.exit_code,
                    output: output.merged_output,
                    stdout: output.stdout,
                    stderr: output.stderr,
                })
            }
            Ok((Err(err), cleanup_result)) => {
                log_cleanup_result(project, &suite_name, kind_label, cleanup_result, true);
                error!(
                    project,
                    suite = %suite_name,
                    kind = %kind_label,
                    error = %err,
                    "suite command execution failed"
                );
                Err(ApiError::internal(err))
            }
            Err(err) => {
                error!(
                    project,
                    suite = %suite_name,
                    kind = %kind_label,
                    error = %err,
                    "suite command task join failed"
                );
                Err(ApiError::internal(anyhow!(
                    "suite command task failed: {err}"
                )))
            }
        };

        if let Err(err) = cleanup_temp_worktree(&git, &worktree) {
            warn!(
                project,
                branch = %worktree.branch,
                worktree = %worktree.path.display(),
                error = %err,
                "failed to cleanup suite check worktree"
            );
        }

        response
    }

    pub async fn run_suite_check_stream(
        &self,
        project: &str,
        payload: RunSuiteCheckRequest,
    ) -> Result<Response, ApiError> {
        let SuiteCheckExecution {
            plan,
            git,
            worktree,
        } = self
            .prepare_suite_check_execution(project, &payload)
            .await?;
        info!(
            project,
            suite = %plan.suite_name,
            kind = %plan.kind.as_str(),
            cwd = %plan.cwd_display,
            command = %plan.command,
            "running suite check command (stream)"
        );

        let (sender, receiver) = tokio_mpsc::channel::<Vec<u8>>(128);
        send_stream_event_async(
            &sender,
            RunSuiteCheckStreamEvent::Started {
                suite: plan.suite_name.clone(),
                kind: plan.kind,
                command: plan.command.clone(),
                cwd: plan.cwd_display.clone(),
            },
        )
        .await;

        let project_name = project.to_owned();
        tokio::task::spawn_blocking(move || {
            let SuiteCheckPlan {
                suite_name,
                kind,
                command,
                cleanup_command,
                cwd,
                cwd_display,
                env,
                timeout_seconds,
            } = plan;
            let kind_label = kind.as_str().to_owned();

            let stream_sender = sender.clone();
            let command_result = execute_suite_command_streaming(
                &suite_name,
                kind,
                &command,
                &cwd,
                &cwd_display,
                &env,
                timeout_seconds,
                None,
                |stream, text| {
                    let _ = send_stream_event_blocking(
                        &stream_sender,
                        RunSuiteCheckStreamEvent::Chunk {
                            stream,
                            text: text.to_owned(),
                        },
                    );
                },
            );

            let cleanup_result = if kind == SuiteCommandKind::Test {
                execute_suite_cleanup_command(
                    cleanup_command.as_deref(),
                    &cwd,
                    &env,
                    Some(timeout_seconds),
                )
            } else {
                Ok(None)
            };

            match command_result {
                Ok(result) => {
                    log_cleanup_result(
                        &project_name,
                        &suite_name,
                        &kind_label,
                        cleanup_result,
                        false,
                    );
                    info!(
                        project = %project_name,
                        suite = %result.suite,
                        kind = %kind_label,
                        exit_code = result.exit_code,
                        stdout_len = result.stdout.len(),
                        stderr_len = result.stderr.len(),
                        "suite check stream command finished"
                    );
                    send_stream_event_blocking(
                        &sender,
                        RunSuiteCheckStreamEvent::Completed { result },
                    );
                }
                Err(err) => {
                    log_cleanup_result(
                        &project_name,
                        &suite_name,
                        &kind_label,
                        cleanup_result,
                        true,
                    );
                    error!(
                        project = %project_name,
                        suite = %suite_name,
                        kind = %kind_label,
                        error = %err,
                        "suite check stream command failed"
                    );
                    send_stream_event_blocking(
                        &sender,
                        RunSuiteCheckStreamEvent::Error {
                            error: err.to_string(),
                        },
                    );
                }
            }

            if let Err(err) = cleanup_temp_worktree(&git, &worktree) {
                warn!(
                    project = %project_name,
                    branch = %worktree.branch,
                    worktree = %worktree.path.display(),
                    error = %err,
                    "failed to cleanup suite check worktree"
                );
            }
        });

        let body_stream = stream::unfold(receiver, |mut receiver| async move {
            receiver
                .recv()
                .await
                .map(|chunk| (Ok::<Vec<u8>, std::convert::Infallible>(chunk), receiver))
        });

        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-ndjson; charset=utf-8"),
        );
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));

        Ok((headers, Body::from_stream(body_stream)).into_response())
    }

    async fn prepare_suite_check_execution(
        &self,
        project: &str,
        payload: &RunSuiteCheckRequest,
    ) -> Result<SuiteCheckExecution, ApiError> {
        let mut context = self.project_context(project).await?;
        context.refresh().map_err(ApiError::internal)?;

        let suite_name = payload.suite.trim();
        if suite_name.is_empty() {
            return Err(ApiError::unprocessable("suite is required"));
        }
        if payload.kind == SuiteCommandKind::PostGreen {
            return Err(ApiError::unprocessable(
                "kind 'post_green' is not supported by this endpoint",
            ));
        }

        let suite = context
            .chief_yaml
            .suites
            .iter()
            .find(|suite| suite.name == suite_name)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("suite '{}' not found", payload.suite)))?;

        let target_override = payload.target.as_deref();
        let command =
            suite_command_for_kind(&suite, payload.kind, target_override).ok_or_else(|| {
                ApiError::unprocessable(format!(
                    "suite '{}' has no {} command configured",
                    suite.name,
                    match payload.kind {
                        SuiteCommandKind::Lint => "lint",
                        SuiteCommandKind::Test => "test",
                        SuiteCommandKind::PostGreen => "post-green",
                    }
                ))
            })?;

        let worktree = create_temp_worktree(&context, "suite-check").map_err(ApiError::internal)?;
        let cwd = suite_command_cwd(&worktree.path, &suite);
        let cwd_display = cwd.display().to_string();

        Ok(SuiteCheckExecution {
            plan: SuiteCheckPlan {
                suite_name: suite.name,
                kind: payload.kind,
                command,
                cleanup_command: if payload.kind == SuiteCommandKind::Test {
                    suite.cleanup_command.clone()
                } else {
                    None
                },
                cwd,
                cwd_display,
                env: suite.env,
                timeout_seconds: suite
                    .command_timeout_seconds
                    .unwrap_or(context.chief_yaml.chief.suite_command_timeout_seconds)
                    .max(1),
            },
            git: context.git.clone(),
            worktree,
        })
    }
}
