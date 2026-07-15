//! Historical usage aggregation for the stats pane: bucket every tracked
//! session's usage events into time periods (today / 7d / 30d / all) and
//! attach an API-equivalent cost estimate.
//!
//! The cost figure is what the tokens would have cost at Claude API list
//! prices — the user is on a subscription, so this is a "value extracted"
//! estimate, never a bill. Codex tokens are counted but not priced (no
//! authoritative OpenAI price table here).

use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::model::AgentSource;
use crate::plan_usage::TokenCounts;

/// Claude API list prices per MTok (input, output). Cache reads bill at
/// 0.1x input, cache writes at 1.25x input (5-minute TTL). Unknown models
/// are counted but not priced.
fn price_per_mtok(model: &str) -> Option<(f64, f64)> {
    let m = model.to_ascii_lowercase();
    if m.starts_with("claude-fable") || m.starts_with("claude-mythos") {
        Some((10.0, 50.0))
    } else if m.contains("3-opus") || m.contains("opus-3") {
        // Legacy Opus 3 billed 3x the 4.x rate; match it before the generic
        // opus arm so old transcripts aren't under-priced (BUGHUNT #g).
        Some((15.0, 75.0))
    } else if m.contains("opus") {
        Some((5.0, 25.0))
    } else if m.contains("sonnet") {
        Some((3.0, 15.0))
    } else if m.contains("haiku") {
        Some((1.0, 5.0))
    } else {
        None
    }
}

/// API-equivalent cost of one event's tokens, in USD.
fn event_cost(model: &str, tokens: &TokenCounts) -> Option<f64> {
    let (input_rate, output_rate) = price_per_mtok(model)?;
    const MTOK: f64 = 1_000_000.0;
    Some(
        tokens.input_tokens as f64 / MTOK * input_rate
            + tokens.output_tokens as f64 / MTOK * output_rate
            + tokens.cache_read_tokens as f64 / MTOK * input_rate * 0.1
            + tokens.cache_creation_tokens as f64 / MTOK * input_rate * 1.25,
    )
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PeriodTotals {
    pub sessions: u64,
    pub messages: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// API-equivalent cost estimate (Claude only; None when nothing was
    /// priceable). Always an estimate — the plan meters differently.
    pub est_cost_usd: Option<f64>,
}

impl PeriodTotals {
    fn add(&mut self, tokens: &TokenCounts, cost: Option<f64>) {
        self.messages += 1;
        self.input_tokens += tokens.input_tokens;
        self.output_tokens += tokens.output_tokens;
        self.cache_read_tokens += tokens.cache_read_tokens;
        self.cache_creation_tokens += tokens.cache_creation_tokens;
        if let Some(cost) = cost {
            *self.est_cost_usd.get_or_insert(0.0) += cost;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeriodStats {
    /// "today" | "7d" | "30d" | "all"
    pub key: String,
    pub claude: PeriodTotals,
    pub codex: PeriodTotals,
}

/// One machine's aggregated usage history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageStats {
    pub machine: String,
    pub periods: Vec<PeriodStats>,
    pub computed_at: String,
}

/// One session's contribution to the aggregation.
pub struct SessionUsage {
    pub source: AgentSource,
    /// Session-level model (Claude Code records it per message id, but the
    /// session's newest model is a fine approximation for pricing).
    pub model: Option<String>,
    pub events: Vec<(DateTime<Utc>, TokenCounts)>,
}

/// Period start boundaries, oldest-inclusive. "today" is local midnight.
fn period_starts(now: DateTime<Utc>) -> [(&'static str, Option<DateTime<Utc>>); 4] {
    use chrono::LocalResult;
    let local_now = now.with_timezone(&Local);
    // Local midnight can be ambiguous (fall-back) or nonexistent (spring-
    // forward) on a DST-change day. Take the earlier instant when ambiguous;
    // when the wall-clock midnight doesn't exist, fall back to 24h ago so
    // "today" always has a real lower bound and never collapses into "all"
    // (BUGHUNT #c).
    let midnight = match Local.with_ymd_and_hms(local_now.year(), local_now.month(), local_now.day(), 0, 0, 0) {
        LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => t.with_timezone(&Utc),
        LocalResult::None => now - Duration::days(1),
    };
    let midnight = Some(midnight);
    [
        ("today", midnight),
        ("7d", Some(now - Duration::days(7))),
        ("30d", Some(now - Duration::days(30))),
        ("all", None),
    ]
}

pub fn compute(
    machine: &str,
    sessions: impl Iterator<Item = SessionUsage>,
    now: DateTime<Utc>,
) -> UsageStats {
    let starts = period_starts(now);
    let mut periods: Vec<PeriodStats> = starts
        .iter()
        .map(|(key, _)| PeriodStats {
            key: (*key).to_string(),
            claude: PeriodTotals::default(),
            codex: PeriodTotals::default(),
        })
        .collect();

    for session in sessions {
        let mut counted_session = [false; 4];
        for (ts, tokens) in &session.events {
            if *ts > now {
                continue; // clock skew guard, same as the plan estimator
            }
            let cost = match session.source {
                AgentSource::Codex => None,
                _ => session
                    .model
                    .as_deref()
                    .and_then(|m| event_cost(m, tokens)),
            };
            for (i, (_, start)) in starts.iter().enumerate() {
                if start.is_none_or(|s| *ts >= s) {
                    let totals = match session.source {
                        AgentSource::Codex => &mut periods[i].codex,
                        _ => &mut periods[i].claude,
                    };
                    totals.add(tokens, cost);
                    if !counted_session[i] {
                        counted_session[i] = true;
                        totals.sessions += 1;
                    }
                }
            }
        }
    }

    UsageStats {
        machine: machine.to_string(),
        periods,
        computed_at: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tc(input: u64, output: u64, cache_read: u64, cache_creation: u64) -> TokenCounts {
        TokenCounts {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_creation_tokens: cache_creation,
        }
    }

    #[test]
    fn buckets_events_into_periods() {
        let now = Utc::now();
        let sessions = vec![
            SessionUsage {
                source: AgentSource::ClaudeCode,
                model: Some("claude-fable-5".into()),
                events: vec![
                    (now - Duration::hours(1), tc(1000, 500, 0, 0)),
                    (now - Duration::days(10), tc(2000, 1000, 0, 0)),
                ],
            },
            SessionUsage {
                source: AgentSource::Codex,
                model: Some("gpt-5.5".into()),
                events: vec![(now - Duration::days(2), tc(300, 100, 50, 0))],
            },
        ];
        let stats = compute("wsl", sessions.into_iter(), now);

        let get = |key: &str| stats.periods.iter().find(|p| p.key == key).unwrap();
        // 30d and all catch both claude events; 7d only the recent one.
        assert_eq!(get("all").claude.input_tokens, 3000);
        assert_eq!(get("7d").claude.input_tokens, 1000);
        assert_eq!(get("all").claude.sessions, 1);
        assert_eq!(get("all").claude.messages, 2);
        // Codex counted but never priced.
        assert_eq!(get("7d").codex.input_tokens, 300);
        assert_eq!(get("7d").codex.est_cost_usd, None);
    }

    #[test]
    fn cost_uses_model_rates_and_cache_multipliers() {
        // Fable 5: $10/MTok in, $50/MTok out; cache read 0.1x, write 1.25x.
        let cost = event_cost("claude-fable-5", &tc(1_000_000, 1_000_000, 1_000_000, 1_000_000))
            .unwrap();
        assert!((cost - (10.0 + 50.0 + 1.0 + 12.5)).abs() < 1e-9);

        let opus = event_cost("claude-opus-4-8", &tc(1_000_000, 0, 0, 0)).unwrap();
        assert!((opus - 5.0).abs() < 1e-9);

        assert_eq!(event_cost("gpt-5.5", &tc(1, 1, 1, 1)), None);
    }

    #[test]
    fn future_events_are_ignored() {
        let now = Utc::now();
        let sessions = vec![SessionUsage {
            source: AgentSource::ClaudeCode,
            model: None,
            events: vec![(now + Duration::hours(2), tc(999, 999, 0, 0))],
        }];
        let stats = compute("wsl", sessions.into_iter(), now);
        assert_eq!(stats.periods[3].claude.messages, 0);
    }
}
