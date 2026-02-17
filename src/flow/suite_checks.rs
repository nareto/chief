use super::*;

pub(crate) fn run_lint_checks(
    execution: &FlowExecution<'_>,
    suites: &[TestSuiteConfig],
    phase: Phase,
) -> Result<bool> {
    let mut all_ok = true;

    for suite in suites {
        let Some(out) = execution.run_lint_suite(suite, phase)? else {
            continue;
        };

        execution.log_event(
            if out.exit_code == 0 {
                "info"
            } else {
                "warning"
            },
            Some(phase),
            EventType::Lint,
            format!(
                "Lint {} ({})",
                if out.exit_code == 0 {
                    "passed"
                } else {
                    "failed"
                },
                suite.name
            ),
            payload_from_json(json!({
                "suite": suite.name,
                "command": out.command,
                "exit_code": out.exit_code,
                "output": out.merged_output,
            })),
        )?;

        if out.exit_code != 0 {
            if let Some(fix_cmd) = &suite.lint_fix_command {
                let cwd = suite_command_cwd(&execution.project_dir, suite);
                let timeout_seconds = execution.suite_command_timeout_seconds(suite);
                let fix_out =
                    execution.run_suite_command(fix_cmd, &cwd, &suite.env, timeout_seconds)?;

                execution.log_event(
                    if fix_out.exit_code == 0 {
                        "info"
                    } else {
                        "warning"
                    },
                    Some(phase),
                    EventType::LintFix,
                    format!(
                        "Lint fix {} ({})",
                        if fix_out.exit_code == 0 {
                            "succeeded"
                        } else {
                            "failed"
                        },
                        suite.name
                    ),
                    payload_from_json(json!({
                        "suite": suite.name,
                        "command": fix_out.command,
                        "exit_code": fix_out.exit_code,
                        "output": fix_out.merged_output,
                    })),
                )?;

                if fix_out.exit_code == 0 {
                    // Re-run lint after successful fix.
                    if let Some(recheck) = execution.run_lint_suite(suite, phase)? {
                        execution.log_event(
                            if recheck.exit_code == 0 {
                                "info"
                            } else {
                                "warning"
                            },
                            Some(phase),
                            EventType::Lint,
                            format!(
                                "Lint re-check {} ({})",
                                if recheck.exit_code == 0 {
                                    "passed"
                                } else {
                                    "still failing"
                                },
                                suite.name
                            ),
                            payload_from_json(json!({
                                "suite": suite.name,
                                "command": recheck.command,
                                "exit_code": recheck.exit_code,
                                "output": recheck.merged_output,
                            })),
                        )?;

                        if recheck.exit_code == 0 {
                            continue; // Fixed — count as pass.
                        }
                    }
                }
            }

            all_ok = false;
        }
    }

    Ok(all_ok)
}

pub(crate) fn run_test_and_lint(
    execution: &FlowExecution<'_>,
    suites: &[TestSuiteConfig],
    phase: Phase,
) -> Result<bool> {
    let lint_ok = run_lint_checks(execution, suites, phase)?;

    let mut tests_ok = true;
    for suite in suites {
        let out = execution.run_test_suite(suite, phase)?;
        execution.log_event(
            if out.exit_code == 0 {
                "info"
            } else {
                "warning"
            },
            Some(phase),
            EventType::TestRun,
            format!(
                "Test run {} ({})",
                if out.exit_code == 0 {
                    "passed"
                } else {
                    "failed"
                },
                suite.name
            ),
            payload_from_json(json!({
                "suite": suite.name,
                "command": out.command,
                "exit_code": out.exit_code,
                "output": out.merged_output,
            })),
        )?;

        if out.exit_code != 0 {
            tests_ok = false;
        }
    }

    Ok(lint_ok && tests_ok)
}
