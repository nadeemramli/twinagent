//! Claude plan-window estimates from local JSONL usage (TWI-16).
//!
//! Claude Pro/Max limits work in 5-hour billing blocks plus a weekly cap,
//! but there is no public API for them. Like ccusage, we reconstruct the
//! windows from the per-message `usage` records already on disk:
//!
//! - **5h block**: anchored to the top of the hour of the first message
//!   after the previous block ended (ccusage's block rule, matching how the
//!   plan window actually starts on first activity).
//! - **weekly**: rolling 7-day sum (the account's true weekly reset time is
//!   unknowable from disk).
//!
//! These are *estimates* — always tagged [`ReadingConfidence::Estimated`],
//! never presented as exact. Exact readings come from Codex `rate_limits`
//! (already parsed) or, in Phase 2, reused OAuth against the internal
//! `/usage` endpoint. Known undercount: subagent/sidechain transcripts are
//! not aggregated in Phase 1.

use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};

use crate::model::ReadingConfidence;

/// Length of one plan billing block.
pub const BLOCK: Duration = Duration::hours(5);
/// Length of the weekly window.
pub const WEEK: Duration = Duration::days(7);

/// Raw token counts of one API message (no derived fields).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCounts {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

impl TokenCounts {
    fn add(&mut self, other: &TokenCounts) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_creation_tokens += other.cache_creation_tokens;
    }

    /// Everything the plan meters, including cache traffic.
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens
            + self.cache_creation_tokens
    }
}

/// Aggregated usage inside one window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowUsage {
    pub tokens: TokenCounts,
    /// API messages observed in the window.
    pub messages: u64,
}

/// The estimate the widget renders next to Codex's exact numbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanEstimate {
    pub five_hour: WindowUsage,
    /// End of the current 5h block, if one is active — the countdown target.
    pub five_hour_resets_at: Option<DateTime<Utc>>,
    pub weekly: WindowUsage,
    /// Always [`ReadingConfidence::Estimated`] for JSONL-derived numbers.
    pub confidence: ReadingConfidence,
}

/// Floor a timestamp to the top of its hour — the block anchor rule.
fn floor_to_hour(ts: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(
        ts.year(),
        ts.month(),
        ts.day(),
        ts.hour(),
        0,
        0,
    )
    .single()
    .unwrap_or(ts)
}

/// Estimate the current plan windows from `(timestamp, tokens)` usage
/// events. Events may arrive in any order; duplicates must already be
/// resolved (the trackers dedupe by API message id).
pub fn estimate(events: &[(DateTime<Utc>, TokenCounts)], now: DateTime<Utc>) -> PlanEstimate {
    let mut sorted: Vec<&(DateTime<Utc>, TokenCounts)> = events.iter().collect();
    sorted.sort_by_key(|(ts, _)| *ts);

    // Walk history reconstructing block boundaries: a message beyond the
    // current block's end opens a new block anchored to its hour.
    let mut block_start: Option<DateTime<Utc>> = None;
    let mut block_usage = WindowUsage::default();
    let mut weekly = WindowUsage::default();

    for (ts, tokens) in sorted {
        if *ts > now {
            // Clock skew guard: ignore events from the future.
            continue;
        }
        match block_start {
            Some(start) if *ts < start + BLOCK => {}
            _ => {
                block_start = Some(floor_to_hour(*ts));
                block_usage = WindowUsage::default();
            }
        }
        block_usage.tokens.add(tokens);
        block_usage.messages += 1;

        if now - *ts <= WEEK {
            weekly.tokens.add(tokens);
            weekly.messages += 1;
        }
    }

    // The last block only counts if `now` still falls inside it; otherwise
    // the 5h window is fresh and empty.
    let (five_hour, five_hour_resets_at) = match block_start {
        Some(start) if now < start + BLOCK => (block_usage, Some(start + BLOCK)),
        _ => (WindowUsage::default(), None),
    };

    PlanEstimate {
        five_hour,
        five_hour_resets_at,
        weekly,
        confidence: ReadingConfidence::Estimated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(iso: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(iso).unwrap().with_timezone(&Utc)
    }

    fn tokens(n: u64) -> TokenCounts {
        TokenCounts {
            input_tokens: n,
            output_tokens: n / 2,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        }
    }

    #[test]
    fn empty_history_is_a_zeroed_estimate() {
        let e = estimate(&[], at("2026-07-10T12:00:00Z"));
        assert_eq!(e.five_hour, WindowUsage::default());
        assert_eq!(e.five_hour_resets_at, None);
        assert_eq!(e.weekly.messages, 0);
        assert_eq!(e.confidence, ReadingConfidence::Estimated);
    }

    #[test]
    fn block_is_anchored_to_the_hour_of_first_activity() {
        let events = vec![(at("2026-07-10T10:23:00Z"), tokens(100))];
        let e = estimate(&events, at("2026-07-10T11:00:00Z"));
        assert_eq!(e.five_hour.tokens.input_tokens, 100);
        // First message at 10:23 → block 10:00–15:00.
        assert_eq!(e.five_hour_resets_at, Some(at("2026-07-10T15:00:00Z")));
    }

    #[test]
    fn activity_after_block_end_opens_a_fresh_block() {
        let events = vec![
            (at("2026-07-10T02:10:00Z"), tokens(1000)),
            // 02:00 block ends 07:00; this opens the 09:00 block.
            (at("2026-07-10T09:40:00Z"), tokens(7)),
        ];
        let e = estimate(&events, at("2026-07-10T10:00:00Z"));
        assert_eq!(e.five_hour.tokens.input_tokens, 7);
        assert_eq!(e.five_hour.messages, 1);
        assert_eq!(e.five_hour_resets_at, Some(at("2026-07-10T14:00:00Z")));
        // Weekly still sees both.
        assert_eq!(e.weekly.tokens.input_tokens, 1007);
    }

    #[test]
    fn expired_block_reports_a_fresh_empty_window() {
        let events = vec![(at("2026-07-10T02:10:00Z"), tokens(1000))];
        let e = estimate(&events, at("2026-07-10T12:00:00Z"));
        assert_eq!(e.five_hour, WindowUsage::default());
        assert_eq!(e.five_hour_resets_at, None);
        assert_eq!(e.weekly.tokens.input_tokens, 1000);
    }

    #[test]
    fn weekly_is_a_rolling_seven_days() {
        let events = vec![
            (at("2026-07-01T12:00:00Z"), tokens(500)), // 9 days ago: out
            (at("2026-07-05T12:00:00Z"), tokens(300)), // 5 days ago: in
            (at("2026-07-10T09:00:00Z"), tokens(200)),
        ];
        let e = estimate(&events, at("2026-07-10T12:00:00Z"));
        assert_eq!(e.weekly.tokens.input_tokens, 500);
        assert_eq!(e.weekly.messages, 2);
    }

    #[test]
    fn future_events_are_ignored() {
        let events = vec![(at("2026-07-10T13:00:00Z"), tokens(999))];
        let e = estimate(&events, at("2026-07-10T12:00:00Z"));
        assert_eq!(e.five_hour, WindowUsage::default());
        assert_eq!(e.weekly.messages, 0);
    }

    #[test]
    fn unsorted_input_produces_the_same_blocks() {
        let a = vec![
            (at("2026-07-10T09:40:00Z"), tokens(7)),
            (at("2026-07-10T02:10:00Z"), tokens(1000)),
        ];
        let mut b = a.clone();
        b.reverse();
        let now = at("2026-07-10T10:00:00Z");
        assert_eq!(estimate(&a, now), estimate(&b, now));
    }
}
