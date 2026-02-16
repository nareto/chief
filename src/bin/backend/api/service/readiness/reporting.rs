use super::*;

pub(super) fn readiness_payload(stage: &str) -> BTreeMap<String, serde_json::Value> {
    let mut payload = BTreeMap::new();
    payload.insert(
        "source".to_owned(),
        serde_json::Value::String(READINESS_EVENT_SOURCE.to_owned()),
    );
    payload.insert(
        "stage".to_owned(),
        serde_json::Value::String(stage.to_owned()),
    );
    payload
}

pub(super) fn record_readiness_event(
    log_context: Option<&ReadinessLogContext>,
    level: &str,
    msg: impl Into<String>,
    mut payload: BTreeMap<String, serde_json::Value>,
) {
    let Some(log_context) = log_context else {
        return;
    };

    payload.insert(
        "source".to_owned(),
        serde_json::Value::String(READINESS_EVENT_SOURCE.to_owned()),
    );
    let event = EventRecord {
        id: None,
        run_id: log_context.run_id.clone(),
        job_id: None,
        todo_id: None,
        timestamp: Utc::now(),
        level: level.to_owned(),
        phase: None,
        msg: msg.into(),
        event_type: EventType::Msg,
        payload,
    };

    if let Err(err) = log_context.store.record_event(&event) {
        warn!(
            run_id = %log_context.run_id,
            error = %err,
            "failed to record readiness event"
        );
    }
}

pub(super) fn readiness_exit_code_is_blocking(kind: ReadinessCommandKind, exit_code: i32) -> bool {
    match kind {
        ReadinessCommandKind::TestInit | ReadinessCommandKind::TestSetup => exit_code != 0,
        ReadinessCommandKind::Lint | ReadinessCommandKind::Test => !matches!(exit_code, 0 | 1 | 5),
    }
}

pub(super) fn readiness_output_tail(output: &str) -> String {
    let lines = output
        .lines()
        .rev()
        .take(25)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");

    if lines.chars().count() > 2_000 {
        let reversed = lines.chars().rev().take(2_000).collect::<String>();
        return reversed.chars().rev().collect();
    }

    lines
}

pub(super) fn build_readiness_summary(
    results: &[ReadinessCommandResult],
    suite_count: usize,
) -> String {
    if suite_count == 0 {
        return "Ready: no suites configured, so pre-run checks skipped command execution."
            .to_owned();
    }

    if results.is_empty() {
        return format!(
            "Ready: no runnable suite commands detected across {suite_count} suite(s)."
        );
    }

    let failed_commands = results
        .iter()
        .filter(|result| result.blocking_failure)
        .count();

    if failed_commands == 0 {
        format!(
            "Ready: validated {} command(s) across {suite_count} suite(s).",
            results.len()
        )
    } else {
        format!(
            "Not ready: {} command(s) failed across {} checked command(s).",
            failed_commands,
            results.len()
        )
    }
}

pub(super) fn build_readiness_details(
    results: &[ReadinessCommandResult],
    suite_count: usize,
    chief_yaml_hash: &str,
    suite_cache_inputs_hash: &str,
) -> serde_json::Value {
    let failed_commands = results
        .iter()
        .filter(|result| result.blocking_failure)
        .count();

    json!({
        "suite_count": suite_count,
        "commands_total": results.len(),
        "commands_failed": failed_commands,
        "chief_yaml_hash": chief_yaml_hash,
        "suite_cache_inputs_hash": suite_cache_inputs_hash,
        "commands": results
            .iter()
            .map(|result| json!({
                "suite": result.suite_name,
                "kind": result.kind.as_str(),
                "command": result.command,
                "cwd": result.cwd,
                "target": result.target,
                "exit_code": result.exit_code,
                "failed": result.blocking_failure,
                "output_tail": result.output_tail,
            }))
            .collect::<Vec<_>>()
    })
}

pub(super) fn trim_leading_bytes(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }

    let bytes_to_trim = text.len().saturating_sub(max_bytes);
    let split_at = text
        .char_indices()
        .find_map(|(index, _)| (index >= bytes_to_trim).then_some(index))
        .unwrap_or(text.len());
    text.drain(..split_at);
}
