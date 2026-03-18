use super::*;
use agentusage::{ApprovalPolicy, UsageConfig, UsageData, run_claude, run_codex};
use anyhow::bail;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

const AGENT_USAGE_EVENT_MSG: &str = "Agent usage limits before call";

#[derive(Debug)]
pub(super) struct AgentCallPermit {
    _guard: Option<MutexGuard<'static, ()>>,
    decision: Option<AgentPacingDecision>,
}

impl AgentCallPermit {
    pub(super) fn decision(&self) -> Option<&AgentPacingDecision> {
        self.decision.as_ref()
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

impl<'a> FlowExecution<'a> {
    pub(super) fn prepare_agent_call(&self, phase: Phase) -> Result<AgentCallPermit> {
        if !self.chief_config.respect_limits {
            return Ok(AgentCallPermit {
                _guard: None,
                decision: None,
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
                });
            }
        };

        let history = self.recent_agent_usage_history(&agent_name)?;
        let decision = compute_agent_pacing(Utc::now(), current_snapshot, &history);

        if !decision.wait_duration.is_zero() {
            self.log_event(
                "info",
                Some(phase),
                EventType::Msg,
                format!(
                    "Waiting {} second(s) before calling {} to respect usage limits",
                    duration_seconds_ceiling(decision.wait_duration),
                    agent_name
                ),
                payload_from_json(json!({
                    "agent_name": agent_name,
                    "respect_limits": true,
                    "desired_frequency_seconds": decision
                        .desired_frequency
                        .map(|duration| duration.as_secs_f64()),
                    "wait_seconds": decision.wait_duration.as_secs_f64(),
                    "limiting_usage_label": decision.limiting_usage_label,
                })),
            )?;
            self.sleep_with_cancellation(decision.wait_duration)?;
        }

        Ok(AgentCallPermit {
            _guard: Some(provider_guard),
            decision: Some(decision),
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
            AGENT_USAGE_EVENT_MSG,
            payload_from_json(json!({
                "agent_name": self.agent.name(),
                "respect_limits": true,
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

        let candidate = desired_frequency_for_limit(limit, impact.average_percent_used_per_call);
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

fn desired_frequency_for_limit(
    limit: &AgentUsageLimitSnapshot,
    average_percent_used_per_call: f64,
) -> Option<Duration> {
    let reset_minutes = limit.reset_minutes?.max(0) as f64;
    if reset_minutes <= 0.0 {
        return None;
    }

    if limit.percent_remaining == 0 {
        return Some(Duration::from_secs_f64(reset_minutes * 60.0));
    }

    if average_percent_used_per_call <= 0.0 {
        return None;
    }

    let remaining = limit.percent_remaining as f64;
    if average_percent_used_per_call >= remaining {
        return Some(Duration::from_secs_f64(reset_minutes * 60.0));
    }

    Some(Duration::from_secs_f64(
        (reset_minutes * 60.0) * average_percent_used_per_call / remaining,
    ))
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
        );

        assert_eq!(
            decision.limiting_usage_label.as_deref(),
            Some("Weekly limit")
        );
        assert_eq!(
            duration_seconds_ceiling(decision.desired_frequency.unwrap()) / 60,
            711,
            "weekly limit should dominate desired spacing"
        );
        assert_eq!(
            duration_seconds_ceiling(decision.wait_duration) / 60,
            651,
            "wait should subtract time since the previous call"
        );
    }

    #[test]
    fn compute_agent_pacing_skips_reset_pairs() {
        let now = Utc.with_ymd_and_hms(2025, 3, 18, 12, 0, 0).unwrap();
        let history = vec![HistoricalAgentUsageEvent {
            timestamp: now - ChronoDuration::minutes(20),
            snapshot: snapshot(&[("5h limit", 90, 10, 20)]),
        }];

        let decision = compute_agent_pacing(now, snapshot(&[("5h limit", 5, 95, 300)]), &history);

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
        )
        .expect("exhausted limits should produce a wait");

        assert_eq!(duration_seconds_ceiling(frequency), 7_200);
    }
}
