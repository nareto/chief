use super::*;

pub(super) fn execute_readiness_command_plans(
    plans: Vec<ReadinessCommandPlan>,
    cancel_signal: Arc<AtomicBool>,
    stream_context: Option<ReadinessStreamContext>,
) -> anyhow::Result<Vec<ReadinessCommandResult>> {
    let mut results = Vec::with_capacity(plans.len());

    for plan in plans {
        if cancel_signal.load(std::sync::atomic::Ordering::SeqCst) {
            if let Some(stream_context) = stream_context.as_ref() {
                stream_context.push_text("Pre-run checks cancelled by user.\n");
            }
            return Err(anyhow!("pre-run checks cancelled by user"));
        }

        if !plan.cwd.exists() {
            if let Some(stream_context) = stream_context.as_ref() {
                stream_context.push_text(format!(
                    "[pre-run-checks:{}:{}] working directory does not exist: {}\n",
                    plan.suite_name,
                    plan.kind.as_str(),
                    plan.cwd.display()
                ));
            }
            results.push(ReadinessCommandResult {
                suite_name: plan.suite_name,
                kind: plan.kind,
                command: plan.command_template,
                cwd: plan.cwd_display,
                target: None,
                exit_code: 127,
                blocking_failure: true,
                output_tail: format!("working directory does not exist: {}", plan.cwd.display()),
            });
            continue;
        }

        if !plan.cwd.is_dir() {
            if let Some(stream_context) = stream_context.as_ref() {
                stream_context.push_text(format!(
                    "[pre-run-checks:{}:{}] working directory is not a directory: {}\n",
                    plan.suite_name,
                    plan.kind.as_str(),
                    plan.cwd.display()
                ));
            }
            results.push(ReadinessCommandResult {
                suite_name: plan.suite_name,
                kind: plan.kind,
                command: plan.command_template,
                cwd: plan.cwd_display,
                target: None,
                exit_code: 127,
                blocking_failure: true,
                output_tail: format!(
                    "working directory is not a directory: {}",
                    plan.cwd.display()
                ),
            });
            continue;
        }

        if !plan.uses_target_placeholder {
            results.push(run_readiness_command_attempt(
                &plan,
                plan.command_template.clone(),
                None,
                &cancel_signal,
                stream_context.as_ref(),
            ));
            if cancel_signal.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(stream_context) = stream_context.as_ref() {
                    stream_context.push_text("Pre-run checks cancelled by user.\n");
                }
                return Err(anyhow!("pre-run checks cancelled by user"));
            }
            continue;
        }

        if plan.target_candidates.is_empty() {
            if let Some(stream_context) = stream_context.as_ref() {
                stream_context.push_text(format!(
                    "[pre-run-checks:{}:{}] command uses {{target}}, but no file_patterns target matched and default_target is not set\n",
                    plan.suite_name,
                    plan.kind.as_str()
                ));
            }
            results.push(ReadinessCommandResult {
                suite_name: plan.suite_name,
                kind: plan.kind,
                command: plan.command_template,
                cwd: plan.cwd_display,
                target: None,
                exit_code: 127,
                blocking_failure: true,
                output_tail: "command uses {target}, but no file_patterns target matched and default_target is not set".to_owned(),
            });
            continue;
        }

        let mut selected: Option<ReadinessCommandResult> = None;
        for target in &plan.target_candidates {
            if cancel_signal.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(stream_context) = stream_context.as_ref() {
                    stream_context.push_text("Pre-run checks cancelled by user.\n");
                }
                return Err(anyhow!("pre-run checks cancelled by user"));
            }
            let command =
                super::planning::replace_target_placeholder(&plan.command_template, target);
            let attempt = run_readiness_command_attempt(
                &plan,
                command,
                Some(target.clone()),
                &cancel_signal,
                stream_context.as_ref(),
            );
            let runnable = !attempt.blocking_failure;
            selected = Some(attempt);
            if runnable {
                break;
            }
        }
        if let Some(result) = selected {
            results.push(result);
            if cancel_signal.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(stream_context) = stream_context.as_ref() {
                    stream_context.push_text("Pre-run checks cancelled by user.\n");
                }
                return Err(anyhow!("pre-run checks cancelled by user"));
            }
        }
    }

    Ok(results)
}

fn suite_kind_for_readiness(kind: ReadinessCommandKind) -> SuiteCommandKind {
    match kind {
        ReadinessCommandKind::Lint => SuiteCommandKind::Lint,
        ReadinessCommandKind::TestInit
        | ReadinessCommandKind::TestSetup
        | ReadinessCommandKind::Test => SuiteCommandKind::Test,
    }
}

fn run_readiness_command_attempt(
    plan: &ReadinessCommandPlan,
    command: String,
    target: Option<String>,
    cancel_signal: &Arc<AtomicBool>,
    stream_context: Option<&ReadinessStreamContext>,
) -> ReadinessCommandResult {
    if let Some(stream_context) = stream_context {
        stream_context.push_text(format!(
            "[pre-run-checks:{}:{}]$ {} (cwd: {})\n",
            plan.suite_name,
            plan.kind.as_str(),
            command,
            plan.cwd_display
        ));
    }

    let out = execute_suite_command_streaming(
        &plan.suite_name,
        suite_kind_for_readiness(plan.kind),
        &command,
        &plan.cwd,
        &plan.cwd_display,
        &plan.env,
        plan.timeout_seconds,
        Some(cancel_signal),
        |_stream, text| {
            if let Some(stream_context) = stream_context {
                stream_context.push_text(text);
            }
        },
    );

    let cleanup_out = if plan.kind == ReadinessCommandKind::Test {
        execute_suite_cleanup_command(
            plan.cleanup_command.as_deref(),
            &plan.cwd,
            &plan.env,
            Some(plan.timeout_seconds),
        )
    } else {
        Ok(None)
    };

    match out {
        Ok(out) => {
            match cleanup_out {
                Ok(Some(cleanup)) => {
                    if let Some(stream_context) = stream_context {
                        stream_context.push_text(format!(
                            "[pre-run-checks:{}:cleanup] exit={}\n",
                            plan.suite_name, cleanup.exit_code
                        ));
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    if let Some(stream_context) = stream_context {
                        stream_context.push_text(format!(
                            "[pre-run-checks:{}:cleanup] failed: {}\n",
                            plan.suite_name, err
                        ));
                    }
                }
            }
            let blocking_failure =
                super::reporting::readiness_exit_code_is_blocking(plan.kind, out.exit_code);
            if let Some(stream_context) = stream_context {
                stream_context.push_text(format!(
                    "[pre-run-checks:{}:{}] exit={}{}\n",
                    plan.suite_name,
                    plan.kind.as_str(),
                    out.exit_code,
                    if blocking_failure { " (blocking)" } else { "" }
                ));
            }
            ReadinessCommandResult {
                suite_name: plan.suite_name.clone(),
                kind: plan.kind,
                command: out.command,
                cwd: plan.cwd_display.clone(),
                target,
                exit_code: out.exit_code,
                blocking_failure,
                output_tail: super::reporting::readiness_output_tail(&out.output),
            }
        }
        Err(err) => {
            match cleanup_out {
                Ok(Some(cleanup)) => {
                    if let Some(stream_context) = stream_context {
                        stream_context.push_text(format!(
                            "[pre-run-checks:{}:cleanup] exit={}\n",
                            plan.suite_name, cleanup.exit_code
                        ));
                    }
                }
                Ok(None) => {}
                Err(cleanup_err) => {
                    if let Some(stream_context) = stream_context {
                        stream_context.push_text(format!(
                            "[pre-run-checks:{}:cleanup] failed: {}\n",
                            plan.suite_name, cleanup_err
                        ));
                    }
                }
            }
            if let Some(stream_context) = stream_context {
                stream_context.push_text(format!(
                    "[pre-run-checks:{}:{}] failed: {}\n",
                    plan.suite_name,
                    plan.kind.as_str(),
                    err
                ));
            }
            ReadinessCommandResult {
                suite_name: plan.suite_name.clone(),
                kind: plan.kind,
                command,
                cwd: plan.cwd_display.clone(),
                target,
                exit_code: 127,
                blocking_failure: true,
                output_tail: super::reporting::readiness_output_tail(&err.to_string()),
            }
        }
    }
}
