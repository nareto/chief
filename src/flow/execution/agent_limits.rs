use super::*;
use agentusage::{ApprovalPolicy, UsageConfig, UsageData, run_claude, run_codex};
use anyhow::bail;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

#[derive(Debug)]
pub(super) struct AgentCallPermit {
    _guard: Option<MutexGuard<'static, ()>>,
    decision: Option<AgentPacingDecision>,
    fixed_wait_decision: Option<AgentFixedWaitDecision>,
}

impl AgentCallPermit {
    pub(super) fn decision(&self) -> Option<&AgentPacingDecision> {
        self.decision.as_ref()
    }

    pub(super) fn fixed_wait_decision(&self) -> Option<&AgentFixedWaitDecision> {
        self.fixed_wait_decision.as_ref()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AgentUsageLimitSnapshot {
    label: String,
    percent_used: u32,
    percent_remaining: u32,
    reset_info: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reset_minutes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    spent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    requests: Option<String>,
}

impl From<agentusage::UsageEntry> for AgentUsageLimitSnapshot {
    fn from(entry: agentusage::UsageEntry) -> Self {
        Self {
            label: entry.label,
            percent_used: entry.percent_used,
            percent_remaining: entry.percent_remaining,
            reset_info: entry.reset_info,
            reset_minutes: entry.reset_minutes,
            spent: entry.spent,
            requests: entry.requests,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AgentUsageSnapshot {
    provider: String,
    limits: Vec<AgentUsageLimitSnapshot>,
}

impl AgentUsageSnapshot {
    fn from_usage_data(data: UsageData) -> Self {
        Self {
            provider: data.provider,
            limits: data.entries.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone)]
struct HistoricalAgentUsageEvent {
    timestamp: chrono::DateTime<Utc>,
    snapshot: AgentUsageSnapshot,
}

#[derive(Debug, Clone, Serialize)]
struct AverageUsageImpact {
    label: String,
    average_percent_used_per_call: f64,
    samples: usize,
}

#[derive(Debug, Clone)]
pub(super) struct AgentPacingDecision {
    current_snapshot: AgentUsageSnapshot,
    recent_call_count: usize,
    observed_average_frequency: Option<Duration>,
    desired_frequency: Option<Duration>,
    wait_duration: Duration,
    limiting_usage_label: Option<String>,
    average_usage_impact: Vec<AverageUsageImpact>,
}

#[derive(Debug, Clone)]
pub(super) struct AgentFixedWaitDecision {
    configured_wait: Duration,
    last_call_at: Option<chrono::DateTime<Utc>>,
    elapsed_since_last_call: Option<Duration>,
    wait_duration: Duration,
}

impl<'a> FlowExecution<'a> {
    pub(super) fn prepare_agent_call(&self, phase: Phase) -> Result<AgentCallPermit> {
        if let Some(wait_seconds) = self.chief_config.agent_wait_seconds {
            return self.prepare_fixed_wait_agent_call(phase, Duration::from_secs(wait_seconds));
        }

        if !self.chief_config.respect_limits {
            return Ok(AgentCallPermit {
                _guard: None,
                decision: None,
                fixed_wait_decision: None,
            });
        }

        let agent_name = self.agent.name().to_owned();
        let provider_guard = agent_limit_mutex(&agent_name)
            .lock()
            .map_err(|_| anyhow!("agent usage coordination mutex poisoned"))?;

        let current_snapshot = match probe_agent_usage(&agent_name, &self.project_dir) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                self.log_event(
                    "warning",
                    Some(phase),
                    EventType::Msg,
                    format!(
                        "Skipping agent limit pacing for {} because usage lookup failed",
                        agent_name
                    ),
                    payload_from_json(json!({
                        "agent_name": agent_name,
                        "respect_limits": true,
                        "error": format!("{err:#}"),
                    })),
                )?;
                return Ok(AgentCallPermit {
                    _guard: Some(provider_guard),
                    decision: None,
                    fixed_wait_decision: None,
                });
            }
        };

        let reserve_percent = self.chief_config.limit_reserve_percent;
        ensure_valid_limit_reserve_percent(reserve_percent)?;
        if let Some(limit) = limit_below_reserve(&current_snapshot, reserve_percent) {
            self.log_event(
                "error",
                Some(phase),
                EventType::Msg,
                format!(
                    "Stopping before calling {} because usage limit '{}' is below the configured reserve",
                    agent_name, limit.label
                ),
                payload_from_json(json!({
                    "agent_name": agent_name,
                    "respect_limits": true,
                    "limit_reserve_percent": reserve_percent,
                    "limiting_usage_label": limit.label,
                    "percent_remaining": limit.percent_remaining,
                })),
            )?;
            bail!(
                "agent usage limit '{}' has {}% remaining, below configured reserve of {}%",
                limit.label,
                limit.percent_remaining,
                reserve_percent
            );
        }

        let history = self.recent_agent_usage_history(&agent_name)?;
        let now = Utc::now();
        let decision = compute_agent_pacing(now, current_snapshot, &history, reserve_percent);

        if !decision.wait_duration.is_zero() {
            let wait_until = wait_until_timestamp(now, decision.wait_duration);
            self.log_event(
                "info",
                Some(phase),
                EventType::Msg,
                waiting_for_usage_limit_message(now, &agent_name, &decision),
                payload_from_json(json!({
                    "agent_name": agent_name,
                    "respect_limits": true,
                    "limit_reserve_percent": reserve_percent,
                    "desired_frequency_seconds": decision
                        .desired_frequency
                        .map(|duration| duration.as_secs_f64()),
                    "wait_seconds": decision.wait_duration.as_secs_f64(),
                    "wait_until": wait_until.map(|timestamp| timestamp.to_rfc3339()),
                    "limiting_usage_label": decision.limiting_usage_label,
                    "usage_impact_estimation_basis": usage_impact_estimation_basis(&decision),
                })),
            )?;
            self.sleep_with_cancellation(decision.wait_duration)?;
        }

        Ok(AgentCallPermit {
            _guard: Some(provider_guard),
            decision: Some(decision),
            fixed_wait_decision: None,
        })
    }

    fn prepare_fixed_wait_agent_call(
        &self,
        phase: Phase,
        configured_wait: Duration,
    ) -> Result<AgentCallPermit> {
        let agent_name = self.agent.name().to_owned();
        let provider_guard = agent_limit_mutex(&agent_name)
            .lock()
            .map_err(|_| anyhow!("agent usage coordination mutex poisoned"))?;

        let history = self.recent_agent_invocation_history(&agent_name)?;
        let now = Utc::now();
        let decision = compute_fixed_agent_wait(now, configured_wait, history.last().copied());

        if !decision.wait_duration.is_zero() {
            let wait_until = wait_until_timestamp(now, decision.wait_duration);
            self.log_event(
                "info",
                Some(phase),
                EventType::Msg,
                waiting_for_fixed_agent_wait_message(now, &agent_name, &decision),
                payload_from_json(json!({
                    "agent_name": agent_name,
                    "pacing_mode": "fixed_wait",
                    "respect_limits": self.chief_config.respect_limits,
                    "respect_limits_overridden": self.chief_config.respect_limits,
                    "agent_wait_seconds": decision.configured_wait.as_secs_f64(),
                    "wait_seconds": decision.wait_duration.as_secs_f64(),
                    "wait_until": wait_until.map(|timestamp| timestamp.to_rfc3339()),
                    "last_agent_call_at": decision
                        .last_call_at
                        .as_ref()
                        .map(|timestamp| timestamp.to_rfc3339()),
                    "elapsed_since_last_call_seconds": decision
                        .elapsed_since_last_call
                        .map(|duration| duration.as_secs_f64()),
                })),
            )?;
            self.sleep_with_cancellation(decision.wait_duration)?;
        }

        Ok(AgentCallPermit {
            _guard: Some(provider_guard),
            decision: None,
            fixed_wait_decision: Some(decision),
        })
    }

    pub(super) fn log_agent_usage_event(
        &self,
        phase: Phase,
        decision: &AgentPacingDecision,
    ) -> Result<()> {
        self.log_event(
            "info",
            Some(phase),
            EventType::AgentCmd,
            agent_usage_event_message(decision),
            payload_from_json(json!({
                "agent_name": self.agent.name(),
                "respect_limits": true,
                "limit_reserve_percent": self.chief_config.limit_reserve_percent,
                "usage": decision.current_snapshot,
                "recent_call_count": decision.recent_call_count,
                "observed_average_frequency_seconds": decision
                    .observed_average_frequency
                    .map(|duration| duration.as_secs_f64()),
                "desired_frequency_seconds": decision
                    .desired_frequency
                    .map(|duration| duration.as_secs_f64()),
                "wait_seconds_applied": decision.wait_duration.as_secs_f64(),
                "limiting_usage_label": decision.limiting_usage_label,
                "average_usage_impact": decision.average_usage_impact,
                "usage_impact_estimation_basis": usage_impact_estimation_basis(decision),
            })),
        )
    }

    pub(super) fn log_agent_fixed_wait_event(
        &self,
        phase: Phase,
        decision: &AgentFixedWaitDecision,
    ) -> Result<()> {
        self.log_event(
            "info",
            Some(phase),
            EventType::AgentCmd,
            "Agent fixed wait before call",
            payload_from_json(json!({
                "agent_name": self.agent.name(),
                "pacing_mode": "fixed_wait",
                "respect_limits": self.chief_config.respect_limits,
                "respect_limits_overridden": self.chief_config.respect_limits,
                "agent_wait_seconds": decision.configured_wait.as_secs_f64(),
                "last_agent_call_at": decision
                    .last_call_at
                    .as_ref()
                    .map(|timestamp| timestamp.to_rfc3339()),
                "elapsed_since_last_call_seconds": decision
                    .elapsed_since_last_call
                    .map(|duration| duration.as_secs_f64()),
                "wait_seconds_applied": decision.wait_duration.as_secs_f64(),
            })),
        )
    }

    fn recent_agent_usage_history(
        &self,
        agent_name: &str,
    ) -> Result<Vec<HistoricalAgentUsageEvent>> {
        let events = self.store.query_events(EventQuery {
            limit: 128,
            event_type: Some(EventType::AgentCmd),
            phase: None,
            level: None,
            contains_text: None,
        })?;

        let mut history = Vec::new();
        for event in events.into_iter().rev() {
            let Some(event_agent_name) = event.payload.get("agent_name").and_then(Value::as_str)
            else {
                continue;
            };
            if !event_agent_name.eq_ignore_ascii_case(agent_name) {
                continue;
            }
            let Some(snapshot) = event
                .payload
                .get("usage")
                .cloned()
                .and_then(|value| serde_json::from_value::<AgentUsageSnapshot>(value).ok())
            else {
                continue;
            };
            history.push(HistoricalAgentUsageEvent {
                timestamp: event.timestamp,
                snapshot,
            });
        }

        Ok(history)
    }

    fn recent_agent_invocation_history(
        &self,
        agent_name: &str,
    ) -> Result<Vec<chrono::DateTime<Utc>>> {
        let events = self.store.query_events(EventQuery {
            limit: 256,
            event_type: Some(EventType::AgentCmd),
            phase: None,
            level: None,
            contains_text: None,
        })?;

        let mut history = Vec::new();
        for event in events.into_iter().rev() {
            let Some(event_agent_name) = event.payload.get("agent_name").and_then(Value::as_str)
            else {
                continue;
            };
            if !event_agent_name.eq_ignore_ascii_case(agent_name) {
                continue;
            }
            if !event.payload.contains_key("query_id") {
                continue;
            }
            history.push(event.timestamp);
        }

        Ok(history)
    }

    fn sleep_with_cancellation(&self, duration: Duration) -> Result<()> {
        let started = std::time::Instant::now();
        while started.elapsed() < duration {
            self.ensure_not_cancelled()?;
            let remaining = duration.saturating_sub(started.elapsed());
            std::thread::sleep(remaining.min(Duration::from_millis(250)));
        }
        self.ensure_not_cancelled()
    }
}

fn agent_limit_mutex(agent_name: &str) -> &'static Mutex<()> {
    static LOCKS: OnceLock<Mutex<BTreeMap<String, &'static Mutex<()>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut guard = locks.lock().expect("agent limit mutex registry poisoned");
    if let Some(mutex) = guard.get(agent_name) {
        return mutex;
    }
    let leaked = Box::leak(Box::new(Mutex::new(())));
    guard.insert(agent_name.to_owned(), leaked);
    leaked
}

fn probe_agent_usage(agent_name: &str, cwd: &Path) -> Result<AgentUsageSnapshot> {
    let config = UsageConfig {
        timeout: 45,
        verbose: false,
        approval_policy: ApprovalPolicy::Fail,
        directory: Some(cwd.display().to_string()),
    };

    let data = if agent_name.eq_ignore_ascii_case("claude") {
        run_claude(&config)?
    } else if agent_name.eq_ignore_ascii_case("codex") {
        run_codex(&config)?
    } else {
        bail!("unsupported agent '{}' for usage-limit checks", agent_name);
    };

    Ok(AgentUsageSnapshot::from_usage_data(data))
}

fn compute_agent_pacing(
    now: chrono::DateTime<Utc>,
    current_snapshot: AgentUsageSnapshot,
    history: &[HistoricalAgentUsageEvent],
    reserve_percent: u32,
) -> AgentPacingDecision {
    let recent_history = if history.len() > 10 {
        &history[history.len() - 10..]
    } else {
        history
    };

    let observed_average_frequency = average_frequency(recent_history);
    let average_usage_impact = average_usage_impact(recent_history, &current_snapshot);

    let mut desired_frequency = None;
    let mut limiting_usage_label = None;
    for impact in &average_usage_impact {
        let Some(limit) = current_snapshot
            .limits
            .iter()
            .find(|entry| entry.label == impact.label)
        else {
            continue;
        };

        let candidate = desired_frequency_for_limit(
            limit,
            impact.average_percent_used_per_call,
            reserve_percent,
        );
        if candidate > desired_frequency {
            desired_frequency = candidate;
            limiting_usage_label = candidate.map(|_| impact.label.clone());
        }
    }

    let elapsed_since_last_call = recent_history
        .last()
        .and_then(|event| now.signed_duration_since(event.timestamp).to_std().ok())
        .unwrap_or_default();
    let wait_duration = desired_frequency
        .map(|desired| desired.saturating_sub(elapsed_since_last_call))
        .unwrap_or_default();

    AgentPacingDecision {
        current_snapshot,
        recent_call_count: recent_history.len(),
        observed_average_frequency,
        desired_frequency,
        wait_duration,
        limiting_usage_label,
        average_usage_impact,
    }
}

fn compute_fixed_agent_wait(
    now: chrono::DateTime<Utc>,
    configured_wait: Duration,
    last_call_at: Option<chrono::DateTime<Utc>>,
) -> AgentFixedWaitDecision {
    let elapsed_since_last_call = last_call_at.as_ref().map(|timestamp| {
        now.signed_duration_since(timestamp.clone())
            .to_std()
            .unwrap_or(Duration::ZERO)
    });
    let wait_duration = elapsed_since_last_call
        .map(|elapsed| configured_wait.saturating_sub(elapsed))
        .unwrap_or(Duration::ZERO);

    AgentFixedWaitDecision {
        configured_wait,
        last_call_at,
        elapsed_since_last_call,
        wait_duration,
    }
}

fn average_frequency(history: &[HistoricalAgentUsageEvent]) -> Option<Duration> {
    let mut intervals = Vec::new();
    for window in history.windows(2) {
        let milliseconds = window[1]
            .timestamp
            .signed_duration_since(window[0].timestamp)
            .num_milliseconds();
        if milliseconds > 0 {
            intervals.push(milliseconds as f64 / 1_000.0);
        }
    }

    if intervals.is_empty() {
        None
    } else {
        Some(Duration::from_secs_f64(
            intervals.iter().sum::<f64>() / intervals.len() as f64,
        ))
    }
}

fn average_usage_impact(
    history: &[HistoricalAgentUsageEvent],
    current_snapshot: &AgentUsageSnapshot,
) -> Vec<AverageUsageImpact> {
    let mut snapshots = history
        .iter()
        .map(|entry| entry.snapshot.clone())
        .collect::<Vec<_>>();
    snapshots.push(current_snapshot.clone());

    current_snapshot
        .limits
        .iter()
        .map(|current_limit| {
            let mut total_percent_used = 0_f64;
            let mut samples = 0_usize;

            for window in snapshots.windows(2) {
                let Some(previous) = window[0]
                    .limits
                    .iter()
                    .find(|entry| entry.label == current_limit.label)
                else {
                    continue;
                };
                let Some(next) = window[1]
                    .limits
                    .iter()
                    .find(|entry| entry.label == current_limit.label)
                else {
                    continue;
                };

                if next.percent_used < previous.percent_used {
                    continue;
                }

                total_percent_used +=
                    (next.percent_used.saturating_sub(previous.percent_used)) as f64;
                samples += 1;
            }

            AverageUsageImpact {
                label: current_limit.label.clone(),
                average_percent_used_per_call: if samples == 0 {
                    0.0
                } else {
                    total_percent_used / samples as f64
                },
                samples,
            }
        })
        .collect()
}

fn ensure_valid_limit_reserve_percent(reserve_percent: u32) -> Result<()> {
    if reserve_percent > 100 {
        bail!("limit_reserve_percent must be between 0 and 100 inclusive");
    }
    Ok(())
}

fn limit_below_reserve(
    snapshot: &AgentUsageSnapshot,
    reserve_percent: u32,
) -> Option<&AgentUsageLimitSnapshot> {
    snapshot
        .limits
        .iter()
        .find(|limit| limit.percent_remaining < reserve_percent)
}

fn desired_frequency_for_limit(
    limit: &AgentUsageLimitSnapshot,
    average_percent_used_per_call: f64,
    reserve_percent: u32,
) -> Option<Duration> {
    let reset_minutes = limit.reset_minutes?.max(0) as f64;
    if reset_minutes <= 0.0 {
        return None;
    }

    if limit.percent_remaining <= reserve_percent {
        return Some(Duration::from_secs_f64(reset_minutes * 60.0));
    }

    if average_percent_used_per_call <= 0.0 {
        return None;
    }

    let spendable_remaining = (limit.percent_remaining - reserve_percent) as f64;
    if average_percent_used_per_call >= spendable_remaining {
        return Some(Duration::from_secs_f64(reset_minutes * 60.0));
    }

    Some(Duration::from_secs_f64(
        (reset_minutes * 60.0) * average_percent_used_per_call / spendable_remaining,
    ))
}

fn usage_impact_estimation_basis(decision: &AgentPacingDecision) -> &'static str {
    if decision.recent_call_count == 0 {
        "first_project_run"
    } else if decision
        .average_usage_impact
        .iter()
        .any(|impact| impact.samples > 0)
    {
        "project_history"
    } else {
        "no_usable_samples"
    }
}

fn agent_usage_event_message(decision: &AgentPacingDecision) -> &'static str {
    match usage_impact_estimation_basis(decision) {
        "first_project_run" => {
            "Agent usage limits before call (first project-local run; per-call impact not estimated yet)"
        }
        "no_usable_samples" => {
            "Agent usage limits before call (project history found, but no usable non-reset samples for per-call impact)"
        }
        "project_history" => {
            "Agent usage limits before call (per-call impact estimated from recent project-local history)"
        }
        _ => unreachable!("usage impact estimation basis must be known"),
    }
}

fn wait_until_timestamp(
    now: chrono::DateTime<Utc>,
    wait_duration: Duration,
) -> Option<chrono::DateTime<Utc>> {
    chrono::Duration::from_std(wait_duration)
        .ok()
        .and_then(|duration| now.checked_add_signed(duration))
}

fn waiting_for_usage_limit_message(
    now: chrono::DateTime<Utc>,
    agent_name: &str,
    decision: &AgentPacingDecision,
) -> String {
    let wait_seconds = duration_seconds_ceiling(decision.wait_duration);
    if let Some(wait_until) = wait_until_timestamp(now, decision.wait_duration) {
        format!(
            "Waiting until {} before calling {} to respect usage limits (~{} second(s))",
            wait_until.to_rfc3339(),
            agent_name,
            wait_seconds
        )
    } else {
        format!(
            "Waiting {} second(s) before calling {} to respect usage limits",
            wait_seconds, agent_name
        )
    }
}

fn waiting_for_fixed_agent_wait_message(
    now: chrono::DateTime<Utc>,
    agent_name: &str,
    decision: &AgentFixedWaitDecision,
) -> String {
    let wait_seconds = duration_seconds_ceiling(decision.wait_duration);
    if let Some(wait_until) = wait_until_timestamp(now, decision.wait_duration) {
        format!(
            "Waiting until {} before calling {} due to fixed agent_wait_seconds (~{} second(s))",
            wait_until.to_rfc3339(),
            agent_name,
            wait_seconds
        )
    } else {
        format!(
            "Waiting {} second(s) before calling {} due to fixed agent_wait_seconds",
            wait_seconds, agent_name
        )
    }
}

fn duration_seconds_ceiling(duration: Duration) -> u64 {
    duration.as_secs() + u64::from(duration.subsec_nanos() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, TimeZone};

    fn snapshot(entries: &[(&str, u32, u32, i64)]) -> AgentUsageSnapshot {
        AgentUsageSnapshot {
            provider: "codex".to_owned(),
            limits: entries
                .iter()
                .map(
                    |(label, used, remaining, reset_minutes)| AgentUsageLimitSnapshot {
                        label: (*label).to_owned(),
                        percent_used: *used,
                        percent_remaining: *remaining,
                        reset_info: format!("resets in {reset_minutes} minutes"),
                        reset_minutes: Some(*reset_minutes),
                        spent: None,
                        requests: None,
                    },
                )
                .collect(),
        }
    }

    #[test]
    fn compute_agent_pacing_uses_slowest_limit() {
        let now = Utc.with_ymd_and_hms(2025, 3, 18, 12, 0, 0).unwrap();
        let history = vec![
            HistoricalAgentUsageEvent {
                timestamp: now - ChronoDuration::minutes(90),
                snapshot: snapshot(&[("5h limit", 10, 90, 300), ("Weekly limit", 20, 80, 10_080)]),
            },
            HistoricalAgentUsageEvent {
                timestamp: now - ChronoDuration::minutes(60),
                snapshot: snapshot(&[("5h limit", 20, 80, 270), ("Weekly limit", 25, 75, 10_020)]),
            },
        ];

        let decision = compute_agent_pacing(
            now,
            snapshot(&[("5h limit", 30, 70, 240), ("Weekly limit", 30, 70, 9_960)]),
            &history,
            10,
        );

        assert_eq!(
            decision.limiting_usage_label.as_deref(),
            Some("Weekly limit")
        );
        assert_eq!(
            duration_seconds_ceiling(decision.desired_frequency.unwrap()) / 60,
            830,
            "weekly limit should dominate desired spacing while preserving the reserve"
        );
        assert_eq!(
            duration_seconds_ceiling(decision.wait_duration) / 60,
            770,
            "wait should subtract time since the previous call"
        );
    }

    #[test]
    fn compute_fixed_agent_wait_skips_first_call_and_waits_between_later_calls() {
        let now = Utc.with_ymd_and_hms(2025, 3, 18, 12, 0, 0).unwrap();

        let first_call = compute_fixed_agent_wait(now, Duration::from_secs(60), None);
        assert!(first_call.wait_duration.is_zero());

        let later_call = compute_fixed_agent_wait(
            now,
            Duration::from_secs(60),
            Some(now - ChronoDuration::seconds(25)),
        );
        assert_eq!(duration_seconds_ceiling(later_call.wait_duration), 35);

        let overdue_call = compute_fixed_agent_wait(
            now,
            Duration::from_secs(60),
            Some(now - ChronoDuration::seconds(90)),
        );
        assert!(overdue_call.wait_duration.is_zero());
    }

    #[test]
    fn compute_agent_pacing_skips_reset_pairs() {
        let now = Utc.with_ymd_and_hms(2025, 3, 18, 12, 0, 0).unwrap();
        let history = vec![HistoricalAgentUsageEvent {
            timestamp: now - ChronoDuration::minutes(20),
            snapshot: snapshot(&[("5h limit", 90, 10, 20)]),
        }];

        let decision =
            compute_agent_pacing(now, snapshot(&[("5h limit", 5, 95, 300)]), &history, 10);

        assert!(decision.desired_frequency.is_none());
        assert!(decision.wait_duration.is_zero());
    }

    #[test]
    fn desired_frequency_waits_until_reset_when_remaining_is_exhausted() {
        let frequency = desired_frequency_for_limit(
            &AgentUsageLimitSnapshot {
                label: "5h limit".to_owned(),
                percent_used: 100,
                percent_remaining: 0,
                reset_info: "resets soon".to_owned(),
                reset_minutes: Some(120),
                spent: None,
                requests: None,
            },
            5.0,
            10,
        )
        .expect("exhausted limits should produce a wait");

        assert_eq!(duration_seconds_ceiling(frequency), 7_200);
    }

    #[test]
    fn desired_frequency_spreads_only_spendable_usage_above_reserve() {
        let frequency = desired_frequency_for_limit(
            &AgentUsageLimitSnapshot {
                label: "Weekly limit".to_owned(),
                percent_used: 30,
                percent_remaining: 70,
                reset_info: "resets later".to_owned(),
                reset_minutes: Some(9_960),
                spent: None,
                requests: None,
            },
            5.0,
            10,
        )
        .expect("spendable usage should produce a desired frequency");

        assert_eq!(duration_seconds_ceiling(frequency) / 60, 830);
    }

    #[test]
    fn limit_below_reserve_detects_drained_limit() {
        let snapshot = snapshot(&[("5h limit", 92, 8, 120), ("Weekly limit", 50, 50, 9_960)]);

        assert_eq!(
            limit_below_reserve(&snapshot, 10).map(|limit| limit.label.as_str()),
            Some("5h limit")
        );
    }

    #[test]
    fn usage_impact_estimation_basis_marks_first_project_run() {
        let decision = AgentPacingDecision {
            current_snapshot: snapshot(&[("5h limit", 10, 90, 300)]),
            recent_call_count: 0,
            observed_average_frequency: None,
            desired_frequency: None,
            wait_duration: Duration::ZERO,
            limiting_usage_label: None,
            average_usage_impact: vec![AverageUsageImpact {
                label: "5h limit".to_owned(),
                average_percent_used_per_call: 0.0,
                samples: 0,
            }],
        };

        assert_eq!(
            usage_impact_estimation_basis(&decision),
            "first_project_run"
        );
        assert_eq!(
            agent_usage_event_message(&decision),
            "Agent usage limits before call (first project-local run; per-call impact not estimated yet)"
        );
    }

    #[test]
    fn usage_impact_estimation_basis_marks_history_without_usable_samples() {
        let decision = AgentPacingDecision {
            current_snapshot: snapshot(&[("5h limit", 10, 90, 300)]),
            recent_call_count: 2,
            observed_average_frequency: None,
            desired_frequency: None,
            wait_duration: Duration::ZERO,
            limiting_usage_label: None,
            average_usage_impact: vec![AverageUsageImpact {
                label: "5h limit".to_owned(),
                average_percent_used_per_call: 0.0,
                samples: 0,
            }],
        };

        assert_eq!(
            usage_impact_estimation_basis(&decision),
            "no_usable_samples"
        );
        assert_eq!(
            agent_usage_event_message(&decision),
            "Agent usage limits before call (project history found, but no usable non-reset samples for per-call impact)"
        );
    }

    #[test]
    fn waiting_message_includes_absolute_resume_timestamp() {
        let now = Utc.with_ymd_and_hms(2025, 3, 18, 12, 0, 0).unwrap();
        let decision = AgentPacingDecision {
            current_snapshot: snapshot(&[("5h limit", 10, 90, 300)]),
            recent_call_count: 1,
            observed_average_frequency: None,
            desired_frequency: None,
            wait_duration: Duration::from_secs(75),
            limiting_usage_label: Some("5h limit".to_owned()),
            average_usage_impact: vec![AverageUsageImpact {
                label: "5h limit".to_owned(),
                average_percent_used_per_call: 5.0,
                samples: 1,
            }],
        };

        assert_eq!(
            waiting_for_usage_limit_message(now, "codex", &decision),
            "Waiting until 2025-03-18T12:01:15+00:00 before calling codex to respect usage limits (~75 second(s))"
        );
    }
}
