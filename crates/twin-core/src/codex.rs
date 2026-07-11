//! Codex rollout parser and session state machine.
//!
//! Parses `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`. Every line is
//! `{timestamp, type, payload}` with `type` one of `session_meta`,
//! `turn_context`, `event_msg`, `response_item`; the interesting variants
//! live in `payload.type`. Like the Claude Code parser this is lenient —
//! the schema is an undocumented internal, so unknown records are skipped
//! and golden tests guard the assumptions.
//!
//! The headline extraction is `event_msg.token_count.rate_limits`: exact
//! plan-window usage (5h primary / weekly secondary, `plan_type`,
//! `resets_at`) straight from disk, no API call — tag these
//! [`crate::ReadingConfidence::Exact`].

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{AgentSnapshot, AgentSource, JumpTarget, SessionState, UsageMetrics};

// (No ">2.5s pending = approval prompt" heuristic here either: long
// commands false-alarmed constantly, and Codex rollouts record no
// approval-request events to key off, so a pending call is simply a call
// running.)
/// How long a reasoning record keeps the session in the thinking state.
pub const THINKING_DECAY: Duration = Duration::seconds(3);
/// No new records for this long → idle.
pub const IDLE_SILENCE: Duration = Duration::seconds(10);
/// No new records for this long → the session is presumed dead.
pub const STALE_SILENCE: Duration = Duration::minutes(30);

/// One plan-usage window from `rate_limits` (primary = 5h, secondary =
/// weekly).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RateLimitWindow {
    pub used_percent: f64,
    pub window_minutes: Option<u64>,
    /// Unix seconds when the window resets.
    pub resets_at: Option<i64>,
}

/// Exact plan usage as reported by the Codex CLI itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimits {
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub plan_type: Option<String>,
    /// Transcript timestamp of the reading, for staleness checks.
    pub observed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct PendingCall {
    name: String,
    since: DateTime<Utc>,
}

/// Incremental parser for one Codex rollout file. Feed it lines (from
/// [`crate::TailReader`]) in file order, then ask for the state at any
/// wall-clock instant.
#[derive(Debug, Default)]
pub struct CodexSessionTracker {
    session_id: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    repository_url: Option<String>,
    model: Option<String>,
    approval_policy: Option<String>,
    originator: Option<String>,
    context_window: Option<u64>,
    /// `total_token_usage` is already cumulative per session: last one wins.
    total_usage: Option<Value>,
    last_usage: Option<Value>,
    rate_limits: Option<RateLimits>,
    pending_calls: HashMap<String, PendingCall>,
    /// Set by `task_started`, cleared by `task_complete`/`turn_aborted`.
    turn_active: bool,
    done: bool,
    interrupted: bool,
    last_activity: Option<DateTime<Utc>>,
    last_reasoning: Option<DateTime<Utc>>,
    parse_errors: u64,
}

impl CodexSessionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    pub fn git_branch(&self) -> Option<&str> {
        self.git_branch.as_deref()
    }

    pub fn repository_url(&self) -> Option<&str> {
        self.repository_url.as_deref()
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn approval_policy(&self) -> Option<&str> {
        self.approval_policy.as_deref()
    }

    pub fn originator(&self) -> Option<&str> {
        self.originator.as_deref()
    }

    pub fn last_activity(&self) -> Option<DateTime<Utc>> {
        self.last_activity
    }

    /// Exact plan-window usage from the newest `token_count` record.
    pub fn rate_limits(&self) -> Option<&RateLimits> {
        self.rate_limits.as_ref()
    }

    pub fn parse_errors(&self) -> u64 {
        self.parse_errors
    }

    /// Ingest one JSONL line; unparseable lines are counted, never fatal.
    pub fn ingest_line(&mut self, line: &str) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            self.parse_errors += 1;
            return;
        };
        let ts = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc));
        let Some(payload) = value.get("payload") else {
            return;
        };

        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => self.ingest_session_meta(payload, ts),
            Some("turn_context") => self.ingest_turn_context(payload, ts),
            Some("event_msg") => self.ingest_event(payload, ts),
            Some("response_item") => self.ingest_response_item(payload, ts),
            _ => {}
        }
    }

    fn touch(&mut self, ts: Option<DateTime<Utc>>) {
        if let Some(ts) = ts {
            if self.last_activity.is_none_or(|prev| ts > prev) {
                self.last_activity = Some(ts);
            }
        }
    }

    fn ingest_session_meta(&mut self, payload: &Value, ts: Option<DateTime<Utc>>) {
        self.touch(ts);
        // Resumed sessions append a fresh session_meta to the same rollout
        // file: last one wins, matching what the CLI is doing now.
        for (field, slot) in [
            ("id", &mut self.session_id),
            ("cwd", &mut self.cwd),
            ("originator", &mut self.originator),
        ] {
            if let Some(v) = payload.get(field).and_then(Value::as_str) {
                *slot = Some(v.to_string());
            }
        }
        if let Some(git) = payload.get("git") {
            self.git_branch = git
                .get("branch")
                .and_then(Value::as_str)
                .map(String::from)
                .or(self.git_branch.take());
            self.repository_url = git
                .get("repository_url")
                .and_then(Value::as_str)
                .map(String::from)
                .or(self.repository_url.take());
        }
    }

    fn ingest_turn_context(&mut self, payload: &Value, ts: Option<DateTime<Utc>>) {
        self.touch(ts);
        for (field, slot) in [
            ("cwd", &mut self.cwd),
            ("model", &mut self.model),
            ("approval_policy", &mut self.approval_policy),
        ] {
            if let Some(v) = payload.get(field).and_then(Value::as_str) {
                *slot = Some(v.to_string());
            }
        }
    }

    fn ingest_event(&mut self, payload: &Value, ts: Option<DateTime<Utc>>) {
        self.touch(ts);
        match payload.get("type").and_then(Value::as_str) {
            Some("task_started") => {
                self.turn_active = true;
                self.done = false;
                self.interrupted = false;
                self.pending_calls.clear();
                if let Some(w) = payload.get("model_context_window").and_then(Value::as_u64) {
                    self.context_window = Some(w);
                }
            }
            Some("task_complete") => {
                self.turn_active = false;
                self.done = true;
                self.pending_calls.clear();
            }
            Some("turn_aborted") => {
                self.turn_active = false;
                self.interrupted = true;
                self.pending_calls.clear();
            }
            Some("user_message") => {
                // A fresh prompt: previous turn's terminal state is over.
                self.done = false;
                self.interrupted = false;
            }
            Some("token_count") => {
                if let Some(info) = payload.get("info").filter(|i| !i.is_null()) {
                    if let Some(total) = info.get("total_token_usage") {
                        self.total_usage = Some(total.clone());
                    }
                    if let Some(last) = info.get("last_token_usage") {
                        self.last_usage = Some(last.clone());
                    }
                    if let Some(w) = info.get("model_context_window").and_then(Value::as_u64) {
                        self.context_window = Some(w);
                    }
                }
                if let Some(rl) = payload.get("rate_limits").filter(|r| !r.is_null()) {
                    self.rate_limits = Some(RateLimits {
                        primary: parse_window(rl.get("primary")),
                        secondary: parse_window(rl.get("secondary")),
                        plan_type: rl.get("plan_type").and_then(Value::as_str).map(String::from),
                        observed_at: ts,
                    });
                }
            }
            _ => {}
        }
    }

    fn ingest_response_item(&mut self, payload: &Value, ts: Option<DateTime<Utc>>) {
        self.touch(ts);
        match payload.get("type").and_then(Value::as_str) {
            Some("function_call") | Some("custom_tool_call") => {
                if let Some(id) = payload.get("call_id").and_then(Value::as_str) {
                    let name = payload
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string();
                    self.pending_calls.insert(
                        id.to_string(),
                        PendingCall {
                            name,
                            since: ts.or(self.last_activity).unwrap_or_default(),
                        },
                    );
                }
            }
            Some("function_call_output") | Some("custom_tool_call_output") => {
                if let Some(id) = payload.get("call_id").and_then(Value::as_str) {
                    self.pending_calls.remove(id);
                }
            }
            Some("reasoning") => {
                if let Some(ts) = ts {
                    self.last_reasoning = Some(ts);
                }
            }
            Some("message") => {
                // The CLI also injects the abort marker as a user message.
                if payload.get("role").and_then(Value::as_str) == Some("user")
                    && flatten_text(payload.get("content")).contains("<turn_aborted>")
                {
                    self.turn_active = false;
                    self.interrupted = true;
                    self.pending_calls.clear();
                }
            }
            _ => {}
        }
    }

    /// Token totals. Codex reports cumulative usage per session, so this is
    /// a direct read of the newest `token_count`, not a sum.
    pub fn usage(&self) -> UsageMetrics {
        let total = self.total_usage.as_ref();
        let get = |v: Option<&Value>, f: &str| {
            v.and_then(|u| u.get(f)).and_then(Value::as_u64).unwrap_or(0)
        };
        // Codex counts cached tokens inside input_tokens; split them out so
        // the widget's cache counter means the same thing for every source.
        let input = get(total, "input_tokens");
        let cached = get(total, "cached_input_tokens");
        let context_pct = match (&self.last_usage, self.context_window) {
            (Some(last), Some(window)) if window > 0 => {
                let used = last.get("total_tokens").and_then(Value::as_u64).unwrap_or(0);
                Some((used as f32 / window as f32 * 100.0).min(100.0))
            }
            _ => None,
        };
        UsageMetrics {
            input_tokens: input.saturating_sub(cached),
            output_tokens: get(total, "output_tokens"),
            cache_read_tokens: cached,
            cache_creation_tokens: 0,
            context_pct,
        }
    }

    /// Derive the session state as of `now` (same ladder as Claude Code).
    pub fn state_at(&self, now: DateTime<Utc>) -> SessionState {
        let Some(last) = self.last_activity else {
            return SessionState::Idle;
        };
        // User-facing terminal states outrank stale (see claude_code):
        // done/interrupted/needs-you wait for the user indefinitely.
        if self.interrupted {
            return SessionState::Interrupted;
        }
        if !self.pending_calls.is_empty() {
            return SessionState::ToolRunning;
        }
        if self.done && !self.turn_active {
            return SessionState::Done;
        }
        if now - last >= STALE_SILENCE {
            return SessionState::Stale;
        }
        if let Some(reasoning) = self.last_reasoning {
            if now - reasoning <= THINKING_DECAY {
                return SessionState::Thinking;
            }
        }
        if now - last >= IDLE_SILENCE {
            SessionState::Idle
        } else {
            SessionState::Thinking
        }
    }

    /// The running tool call, for the card's one-liner.
    pub fn current_task(&self) -> Option<String> {
        self.pending_calls
            .values()
            .max_by_key(|c| c.since)
            .map(|c| c.name.clone())
    }

    /// Normalize to the adapter contract.
    pub fn snapshot(&self, machine: &str, now: DateTime<Utc>) -> AgentSnapshot {
        let state = self.state_at(now);
        let needs_user_reason = match state {
            SessionState::NeedsYou => Some("waiting for you".to_string()),
            SessionState::Interrupted => Some("interrupted by user".to_string()),
            _ => None,
        };
        AgentSnapshot {
            agent_id: self.session_id.clone().unwrap_or_else(|| "unknown".into()),
            source: AgentSource::Codex,
            machine: machine.to_string(),
            project: self.cwd.clone().unwrap_or_default(),
            status: state.to_status(),
            git_branch: self.git_branch.clone(),
            current_task: self.current_task(),
            needs_user: needs_user_reason.is_some(),
            needs_user_reason,
            usage: self.usage(),
            last_activity: self
                .last_activity
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
                .unwrap_or_default(),
            jump: self.cwd.clone().map(JumpTarget::Terminal),
        }
    }
}

fn parse_window(value: Option<&Value>) -> Option<RateLimitWindow> {
    let v = value?;
    Some(RateLimitWindow {
        used_percent: v.get("used_percent")?.as_f64()?,
        window_minutes: v.get("window_minutes").and_then(Value::as_u64),
        resets_at: v.get("resets_at").and_then(Value::as_i64),
    })
}

/// Response-item message content is a list of `{type, text}` blocks.
fn flatten_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AgentStatus;

    fn ts(secs: i64) -> String {
        DateTime::<Utc>::from_timestamp(1_700_000_000 + secs, 0)
            .unwrap()
            .to_rfc3339()
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_700_000_000 + secs, 0).unwrap()
    }

    fn event(secs: i64, payload: &str) -> String {
        format!(
            r#"{{"timestamp":"{}","type":"event_msg","payload":{payload}}}"#,
            ts(secs)
        )
    }

    fn response(secs: i64, payload: &str) -> String {
        format!(
            r#"{{"timestamp":"{}","type":"response_item","payload":{payload}}}"#,
            ts(secs)
        )
    }

    #[test]
    fn session_meta_and_turn_context_metadata() {
        let mut t = CodexSessionTracker::new();
        let meta = format!(
            r#"{{"timestamp":"{}","type":"session_meta","payload":{{"id":"019f-abc","cwd":"C:\\proj","originator":"Codex Desktop","git":{{"repository_url":"https://github.com/x/y","branch":"main"}}}}}}"#,
            ts(0)
        );
        t.ingest_line(&meta);
        let ctx = format!(
            r#"{{"timestamp":"{}","type":"turn_context","payload":{{"turn_id":"t1","cwd":"C:\\proj","approval_policy":"never","model":"gpt-5.2-codex"}}}}"#,
            ts(1)
        );
        t.ingest_line(&ctx);
        assert_eq!(t.session_id(), Some("019f-abc"));
        assert_eq!(t.cwd(), Some("C:\\proj"));
        assert_eq!(t.model(), Some("gpt-5.2-codex"));
        assert_eq!(t.approval_policy(), Some("never"));
        assert_eq!(t.git_branch(), Some("main"));
    }

    #[test]
    fn function_call_pairing_drives_tool_state() {
        let mut t = CodexSessionTracker::new();
        t.ingest_line(&event(0, r#"{"type":"task_started","turn_id":"t1"}"#));
        t.ingest_line(&response(
            1,
            r#"{"type":"function_call","name":"shell_command","arguments":"{}","call_id":"c1"}"#,
        ));
        assert_eq!(t.state_at(at(2)), SessionState::ToolRunning);
        assert_eq!(t.current_task().as_deref(), Some("shell_command"));
        // Still just running, however long it takes — no timing-based
        // approval guess (long commands false-alarmed constantly).
        assert_eq!(t.state_at(at(300)), SessionState::ToolRunning);

        t.ingest_line(&response(
            6,
            r#"{"type":"function_call_output","call_id":"c1","output":"ok"}"#,
        ));
        assert_eq!(t.state_at(at(7)), SessionState::Thinking);
    }

    #[test]
    fn task_complete_is_done_and_new_prompt_resets() {
        let mut t = CodexSessionTracker::new();
        t.ingest_line(&event(0, r#"{"type":"task_started","turn_id":"t1"}"#));
        t.ingest_line(&event(5, r#"{"type":"task_complete","turn_id":"t1"}"#));
        assert_eq!(t.state_at(at(20)), SessionState::Done);

        t.ingest_line(&event(30, r#"{"type":"user_message","message":"more"}"#));
        assert_ne!(t.state_at(at(31)), SessionState::Done);
    }

    #[test]
    fn turn_aborted_means_interrupted() {
        let mut t = CodexSessionTracker::new();
        t.ingest_line(&event(0, r#"{"type":"task_started","turn_id":"t1"}"#));
        t.ingest_line(&response(
            1,
            r#"{"type":"function_call","name":"shell_command","arguments":"{}","call_id":"c1"}"#,
        ));
        t.ingest_line(&event(
            2,
            r#"{"type":"turn_aborted","turn_id":"t1","reason":"interrupted"}"#,
        ));
        assert_eq!(t.state_at(at(3)), SessionState::Interrupted);
        assert_eq!(t.snapshot("windows", at(3)).status, AgentStatus::NeedsYou);
    }

    #[test]
    fn abort_marker_in_user_message_also_interrupts() {
        let mut t = CodexSessionTracker::new();
        t.ingest_line(&event(0, r#"{"type":"task_started","turn_id":"t1"}"#));
        t.ingest_line(&response(
            1,
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"<turn_aborted>\nThe user interrupted the previous turn on purpose.\n</turn_aborted>"}]}"#,
        ));
        assert_eq!(t.state_at(at(2)), SessionState::Interrupted);
    }

    #[test]
    fn token_count_gives_cumulative_usage_and_exact_rate_limits() {
        let mut t = CodexSessionTracker::new();
        t.ingest_line(&event(
            0,
            r#"{"type":"token_count","info":{"total_token_usage":{"input_tokens":16889,"cached_input_tokens":7040,"output_tokens":183,"total_tokens":17072},"last_token_usage":{"input_tokens":16889,"cached_input_tokens":7040,"output_tokens":183,"total_tokens":17072},"model_context_window":258400},"rate_limits":{"limit_id":"codex","primary":{"used_percent":13.0,"window_minutes":300,"resets_at":1783586575},"secondary":{"used_percent":37.0,"window_minutes":10080,"resets_at":1784032980},"plan_type":"plus"}}"#,
        ));
        let u = t.usage();
        assert_eq!(u.input_tokens, 16889 - 7040);
        assert_eq!(u.cache_read_tokens, 7040);
        assert_eq!(u.output_tokens, 183);
        assert!((u.context_pct.unwrap() - (17072.0 / 258400.0 * 100.0)).abs() < 0.01);

        let rl = t.rate_limits().unwrap();
        assert_eq!(rl.plan_type.as_deref(), Some("plus"));
        let primary = rl.primary.unwrap();
        assert_eq!(primary.used_percent, 13.0);
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(primary.resets_at, Some(1_783_586_575));
        assert_eq!(rl.secondary.unwrap().used_percent, 37.0);

        // Later token_count wins: cumulative, not summed.
        t.ingest_line(&event(
            10,
            r#"{"type":"token_count","info":{"total_token_usage":{"input_tokens":20000,"cached_input_tokens":8000,"output_tokens":500,"total_tokens":20500},"last_token_usage":{"input_tokens":3111,"cached_input_tokens":960,"output_tokens":317,"total_tokens":3428},"model_context_window":258400},"rate_limits":null}"#,
        ));
        assert_eq!(t.usage().output_tokens, 500);
        // Null rate_limits does not clobber the last good reading.
        assert!(t.rate_limits().is_some());
    }

    #[test]
    fn garbage_lines_are_counted_not_fatal() {
        let mut t = CodexSessionTracker::new();
        t.ingest_line("truncated {\"type\":");
        t.ingest_line(r#"{"timestamp":"2026-07-09T06:10:08.038Z","type":"compacted"}"#);
        assert_eq!(t.parse_errors(), 1);
    }
}
