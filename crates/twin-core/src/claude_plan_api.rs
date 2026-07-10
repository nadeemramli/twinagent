//! Exact Claude plan usage from Anthropic's OAuth usage endpoint — the same
//! server-side data behind Claude Code's `/usage` command. This fulfills the
//! Phase-2 note in [`crate::plan_usage`]: reused OAuth against the provider's
//! usage endpoint, one rung up the confidence ladder from JSONL estimates.
//!
//! Privacy: the only network peer is `api.anthropic.com`, called with the
//! user's own Claude Code access token, read straight from the credentials
//! file. The token is never logged and never refreshed here — Claude Code
//! owns the refresh cycle; if the token has expired we simply report nothing
//! until Claude Code runs again.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// The `claude-code/` prefix is load-bearing: other user agents land in an
/// aggressively rate-limited bucket and draw persistent 429s.
const USER_AGENT: &str = "claude-code/2.0.31 (twinagent)";
/// Documented-safe polling floor; `TWIN_CLAUDE_USAGE_INTERVAL` (seconds) may
/// raise it but never lower it.
const MIN_INTERVAL: Duration = Duration::from_secs(180);
const BACKOFF_MAX: Duration = Duration::from_secs(1800);
/// A reading older than this is no longer presented as exact at all —
/// callers fall back to the JSONL estimate instead.
const STALE_AFTER: chrono::Duration = chrono::Duration::minutes(15);

/// One plan window as the endpoint reports it: a server-side percentage and
/// a real reset instant (no guessing).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanWindow {
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

/// Exact account-level Claude plan usage. Mirrors [`crate::codex::RateLimits`]
/// in spirit: real percentages, real resets, tagged by observation time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaudePlanWindows {
    pub five_hour: PlanWindow,
    pub seven_day: PlanWindow,
    /// Per-model weekly caps; present only on plans that have them.
    pub seven_day_opus: Option<PlanWindow>,
    pub seven_day_sonnet: Option<PlanWindow>,
    pub observed_at: DateTime<Utc>,
}

/// Wire shape of the endpoint response. The real payload carries many more
/// fields (dollar limits, experimental flags, a `limits` array) — everything
/// unknown is ignored, and a window with a null `utilization` counts as
/// absent.
#[derive(Deserialize)]
struct WireWindow {
    utilization: Option<f64>,
    resets_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct WireUsage {
    five_hour: Option<WireWindow>,
    seven_day: Option<WireWindow>,
    #[serde(default)]
    seven_day_opus: Option<WireWindow>,
    #[serde(default)]
    seven_day_sonnet: Option<WireWindow>,
}

fn window(wire: Option<WireWindow>) -> Option<PlanWindow> {
    let wire = wire?;
    Some(PlanWindow {
        used_percent: wire.utilization?,
        resets_at: wire.resets_at,
    })
}

/// Parse the endpoint body. The 5h and weekly windows are required — a
/// response without them is treated as an error rather than an empty
/// reading, so a format drift surfaces in logs instead of as "0% used".
pub fn parse_usage_response(
    body: &str,
    observed_at: DateTime<Utc>,
) -> Result<ClaudePlanWindows, String> {
    let wire: WireUsage = serde_json::from_str(body).map_err(|e| e.to_string())?;
    Ok(ClaudePlanWindows {
        five_hour: window(wire.five_hour).ok_or("missing five_hour window")?,
        seven_day: window(wire.seven_day).ok_or("missing seven_day window")?,
        seven_day_opus: window(wire.seven_day_opus),
        seven_day_sonnet: window(wire.seven_day_sonnet),
        observed_at,
    })
}

/// Pull `claudeAiOauth.accessToken` out of a Claude Code credentials file.
fn read_access_token(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let creds: serde_json::Value = serde_json::from_str(&raw).ok()?;
    creds
        .get("claudeAiOauth")?
        .get("accessToken")?
        .as_str()
        .map(str::to_owned)
}

/// Polls the usage endpoint on a safe cadence and hands out the latest
/// non-stale reading. Callers `tick()` as often as they like (e.g. every
/// 30s alongside the estimate); actual HTTP happens at most once per
/// interval, with exponential backoff after failures.
pub struct ClaudePlanPoller {
    creds_path: PathBuf,
    interval: Duration,
    latest: Option<ClaudePlanWindows>,
    next_attempt: Instant,
    backoff: Duration,
    /// Log missing/unreadable credentials once, not every 3 minutes.
    warned_creds: bool,
}

impl ClaudePlanPoller {
    pub fn new(creds_path: impl Into<PathBuf>) -> Self {
        let interval = std::env::var("TWIN_CLAUDE_USAGE_INTERVAL")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(MIN_INTERVAL)
            .max(MIN_INTERVAL);
        Self {
            creds_path: creds_path.into(),
            interval,
            latest: None,
            next_attempt: Instant::now(),
            backoff: MIN_INTERVAL,
            warned_creds: false,
        }
    }

    /// `~/.claude/.credentials.json` on this machine (`USERPROFILE` on
    /// Windows, `HOME` elsewhere).
    pub fn default_creds_path() -> PathBuf {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default();
        PathBuf::from(home).join(".claude").join(".credentials.json")
    }

    /// Fetch if due, then return the latest reading unless it has gone
    /// stale (older than [`STALE_AFTER`]).
    pub fn tick(&mut self, now: DateTime<Utc>) -> Option<ClaudePlanWindows> {
        if Instant::now() >= self.next_attempt {
            self.fetch(now);
        }
        self.latest
            .clone()
            .filter(|r| now.signed_duration_since(r.observed_at) < STALE_AFTER)
    }

    fn fetch(&mut self, now: DateTime<Utc>) {
        let Some(token) = read_access_token(&self.creds_path) else {
            if !self.warned_creds {
                eprintln!(
                    "twin-core: no Claude credentials at {} — exact plan usage disabled until they appear",
                    self.creds_path.display()
                );
                self.warned_creds = true;
            }
            self.next_attempt = Instant::now() + self.interval;
            return;
        };
        self.warned_creds = false;

        let result = ureq::get(USAGE_URL)
            .set("Authorization", &format!("Bearer {token}"))
            .set("anthropic-beta", "oauth-2025-04-20")
            .set("User-Agent", USER_AGENT)
            .set("Content-Type", "application/json")
            .timeout(Duration::from_secs(10))
            .call();

        match result.map_err(summarize_err).and_then(|resp| {
            resp.into_string()
                .map_err(|e| e.to_string())
                .and_then(|body| parse_usage_response(&body, now))
        }) {
            Ok(reading) => {
                self.latest = Some(reading);
                self.backoff = self.interval;
                self.next_attempt = Instant::now() + self.interval;
            }
            Err(err) => {
                // 401 = token expired (Claude Code will refresh it next
                // run); 429 = we're polling too eagerly. Either way: back
                // off, keep the previous reading until it goes stale.
                eprintln!("twin-core: claude usage poll failed ({err}); retrying in {:?}", self.backoff);
                self.next_attempt = Instant::now() + self.backoff;
                self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// Status-only error text — never echo response bodies or headers, which
/// could concern credentials.
fn summarize_err(err: ureq::Error) -> String {
    match err {
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        ureq::Error::Transport(t) => t.kind().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// Trimmed from a live response (2026-07-10): extra unknown fields and
    /// null experimental windows must not break parsing.
    const LIVE_BODY: &str = r#"{
        "five_hour": {"utilization": 24.0, "resets_at": "2026-07-10T11:50:00.342340+00:00",
                      "limit_dollars": null, "used_dollars": null, "remaining_dollars": null},
        "seven_day": {"utilization": 11.0, "resets_at": "2026-07-12T06:00:00.342367+00:00",
                      "limit_dollars": null, "used_dollars": null, "remaining_dollars": null},
        "seven_day_oauth_apps": null,
        "seven_day_opus": null,
        "seven_day_sonnet": {"utilization": 1.0, "resets_at": "2026-07-12T03:00:00+00:00"},
        "tangelo": null,
        "extra_usage": {"is_enabled": false, "monthly_limit": null},
        "limits": [{"kind": "session"}]
    }"#;

    #[test]
    fn parses_live_response_shape() {
        let now = ts("2026-07-10T10:00:00Z");
        let reading = parse_usage_response(LIVE_BODY, now).unwrap();
        assert_eq!(reading.five_hour.used_percent, 24.0);
        assert_eq!(
            reading.five_hour.resets_at,
            Some(ts("2026-07-10T11:50:00.342340+00:00"))
        );
        assert_eq!(reading.seven_day.used_percent, 11.0);
        assert!(reading.seven_day_opus.is_none());
        assert_eq!(reading.seven_day_sonnet.unwrap().used_percent, 1.0);
        assert_eq!(reading.observed_at, now);
    }

    #[test]
    fn missing_core_window_is_an_error_not_zero() {
        let body = r#"{"seven_day": {"utilization": 11.0, "resets_at": null}}"#;
        assert!(parse_usage_response(body, Utc::now()).is_err());
        // Null utilization counts as absent too.
        let body = r#"{"five_hour": {"utilization": null}, "seven_day": {"utilization": 1.0}}"#;
        assert!(parse_usage_response(body, Utc::now()).is_err());
    }

    #[test]
    fn credentials_token_extracted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-xyz","refreshToken":"r","expiresAt":1786259200000}}"#,
        )
        .unwrap();
        assert_eq!(read_access_token(&path).as_deref(), Some("sk-ant-oat01-xyz"));
        assert_eq!(read_access_token(&dir.path().join("missing.json")), None);
    }

    #[test]
    fn stale_reading_is_withheld() {
        let mut poller = ClaudePlanPoller::new("/nonexistent/creds.json");
        // Seed a reading and push the next HTTP attempt far away so tick()
        // only exercises the staleness filter.
        let observed = ts("2026-07-10T10:00:00Z");
        poller.latest = Some(parse_usage_response(LIVE_BODY, observed).unwrap());
        poller.next_attempt = Instant::now() + Duration::from_secs(3600);

        assert!(poller.tick(ts("2026-07-10T10:10:00Z")).is_some());
        assert!(poller.tick(ts("2026-07-10T10:20:00Z")).is_none(), "20min-old reading must not be shown as exact");
    }
}
