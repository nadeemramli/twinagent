//! Claude Code transcript parser and session state machine.
//!
//! Ports AgentNotch's parsing heuristics: transcripts under
//! `~/.claude/projects/<slug>/<session>.jsonl` are undocumented internals, so
//! everything here is lenient — unknown line types and missing fields are
//! skipped, never errors. Golden tests against real transcripts guard the
//! format assumptions.
//!
//! State heuristics (replaced by hooks in Phase 2):
//! - `tool_use` paired with `tool_result` by `tool_use_id`; a result
//!   containing "rejected" → interrupted.
//! - tool pending > 2.5 s without a result → needs-you (permission prompt).
//! - `thinking` blocks → thinking, with a 3 s decay.
//! - `stop_reason == "end_turn"` → done; `[Request interrupted by user` →
//!   interrupted; ~10 s of silence → idle; very long silence → stale.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{AgentSnapshot, AgentSource, JumpTarget, SessionState, UsageMetrics};

/// Tool pending longer than this without a result is assumed to be waiting
/// for permission.
pub const PERMISSION_WAIT: Duration = Duration::milliseconds(2500);
/// How long a thinking block keeps the session in the thinking state.
pub const THINKING_DECAY: Duration = Duration::seconds(3);
/// No new records for this long → idle.
pub const IDLE_SILENCE: Duration = Duration::seconds(10);
/// No new records for this long → the session is presumed dead.
pub const STALE_SILENCE: Duration = Duration::minutes(30);
/// Context window assumed for the gauge until model metadata says otherwise.
pub const CONTEXT_WINDOW_TOKENS: u64 = 200_000;

/// One TodoWrite entry (best-effort extraction).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    pub status: String,
    #[serde(default)]
    pub active_form: Option<String>,
}

#[derive(Debug, Clone)]
struct PendingTool {
    name: String,
    /// Short human label: the tool's `description` input when present.
    label: Option<String>,
    since: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Default)]
struct RawUsage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
}

/// Incremental parser for one Claude Code session transcript. Feed it lines
/// (from [`crate::TailReader`]) in file order, then ask for the state at any
/// wall-clock instant.
#[derive(Debug, Default)]
pub struct ClaudeSessionTracker {
    session_id: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    model: Option<String>,
    /// Usage keyed by API message id: streamed lines repeat the same message
    /// with cumulative usage, so the last write wins instead of summing.
    usage_by_msg: HashMap<String, RawUsage>,
    last_usage: RawUsage,
    pending_tools: HashMap<String, PendingTool>,
    todos: Vec<Todo>,
    /// Timestamp of the last user/assistant record (state-machine clock).
    last_activity: Option<DateTime<Utc>>,
    last_thinking: Option<DateTime<Utc>>,
    /// stop_reason of the newest assistant message; cleared by user input.
    last_stop_reason: Option<String>,
    interrupted: bool,
    sidechain_records: u64,
    parse_errors: u64,
}

impl ClaudeSessionTracker {
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

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn todos(&self) -> &[Todo] {
        &self.todos
    }

    pub fn last_activity(&self) -> Option<DateTime<Utc>> {
        self.last_activity
    }

    /// Records seen with `isSidechain: true` (subagent traffic).
    pub fn sidechain_records(&self) -> u64 {
        self.sidechain_records
    }

    pub fn parse_errors(&self) -> u64 {
        self.parse_errors
    }

    /// Ingest one JSONL line. Unparseable or unknown lines are counted and
    /// skipped — transcript formats drift and must never take the widget
    /// down.
    pub fn ingest_line(&mut self, line: &str) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            self.parse_errors += 1;
            return;
        };
        self.ingest(&value);
    }

    fn ingest(&mut self, record: &Value) {
        // Session metadata rides on most record types; grab it wherever it
        // appears.
        for (field, slot) in [
            ("sessionId", &mut self.session_id),
            ("cwd", &mut self.cwd),
            ("gitBranch", &mut self.git_branch),
        ] {
            if let Some(v) = record.get(field).and_then(Value::as_str) {
                if !v.is_empty() {
                    *slot = Some(v.to_string());
                }
            }
        }

        if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            // Subagent traffic: counted, but it must not drive the main
            // session's state machine (subagents get their own tracker over
            // their own transcript files).
            self.sidechain_records += 1;
            return;
        }

        let ts = record
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc));

        match record.get("type").and_then(Value::as_str) {
            Some("assistant") => self.ingest_assistant(record, ts),
            Some("user") => self.ingest_user(record, ts),
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

    fn ingest_assistant(&mut self, record: &Value, ts: Option<DateTime<Utc>>) {
        self.touch(ts);
        // The assistant spoke again: any earlier interruption is over.
        self.interrupted = false;

        let Some(message) = record.get("message") else {
            return;
        };

        if let Some(model) = message.get("model").and_then(Value::as_str) {
            self.model = Some(model.to_string());
        }
        if let Some(stop) = message.get("stop_reason").and_then(Value::as_str) {
            self.last_stop_reason = Some(stop.to_string());
        }

        if let Some(usage) = message.get("usage") {
            let raw = RawUsage {
                input: u64_field(usage, "input_tokens"),
                output: u64_field(usage, "output_tokens"),
                cache_read: u64_field(usage, "cache_read_input_tokens"),
                cache_creation: u64_field(usage, "cache_creation_input_tokens"),
            };
            self.last_usage = raw;
            if let Some(id) = message.get("id").and_then(Value::as_str) {
                self.usage_by_msg.insert(id.to_string(), raw);
            }
        }

        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            return;
        };
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("thinking") => {
                    if let Some(ts) = ts {
                        self.last_thinking = Some(ts);
                    }
                }
                Some("tool_use") => self.ingest_tool_use(block, ts),
                _ => {}
            }
        }
    }

    fn ingest_tool_use(&mut self, block: &Value, ts: Option<DateTime<Utc>>) {
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string();
        let input = block.get("input");

        if name == "TodoWrite" {
            if let Some(todos) = input.and_then(|i| i.get("todos")).and_then(Value::as_array) {
                self.todos = todos
                    .iter()
                    .map(|t| Todo {
                        content: str_field(t, "content"),
                        status: str_field(t, "status"),
                        active_form: t
                            .get("activeForm")
                            .and_then(Value::as_str)
                            .map(String::from),
                    })
                    .collect();
            }
        }

        if let Some(id) = block.get("id").and_then(Value::as_str) {
            let label = input
                .and_then(|i| i.get("description"))
                .and_then(Value::as_str)
                .map(String::from);
            self.pending_tools.insert(
                id.to_string(),
                PendingTool {
                    name,
                    label,
                    since: ts.or(self.last_activity).unwrap_or_default(),
                },
            );
        }
    }

    fn ingest_user(&mut self, record: &Value, ts: Option<DateTime<Utc>>) {
        self.touch(ts);
        let is_meta = record.get("isMeta").and_then(Value::as_bool) == Some(true);
        let content = record.get("message").and_then(|m| m.get("content"));

        let mut saw_tool_result = false;
        match content {
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("tool_result") => {
                            saw_tool_result = true;
                            if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                                self.pending_tools.remove(id);
                            }
                            let text = flatten_text(block.get("content"));
                            if text.to_ascii_lowercase().contains("rejected") {
                                self.interrupted = true;
                            }
                        }
                        Some("text") => {
                            let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                            self.ingest_user_text(text, is_meta);
                        }
                        _ => {}
                    }
                }
            }
            Some(Value::String(text)) => {
                let text = text.clone();
                self.ingest_user_text(&text, is_meta);
            }
            _ => {}
        }

        // A real user prompt (not tool plumbing, not injected meta) starts a
        // fresh turn: the session is no longer done or interrupted.
        if !saw_tool_result && !is_meta {
            self.last_stop_reason = None;
        }
    }

    fn ingest_user_text(&mut self, text: &str, is_meta: bool) {
        if text.contains("[Request interrupted by user") {
            self.interrupted = true;
            // Whatever was in flight was abandoned with the turn.
            self.pending_tools.clear();
        } else if !is_meta && !text.is_empty() {
            self.interrupted = false;
        }
    }

    /// Total token usage, deduped across streamed repeats of the same
    /// message.
    pub fn usage(&self) -> UsageMetrics {
        let mut total = RawUsage::default();
        for u in self.usage_by_msg.values() {
            total.input += u.input;
            total.output += u.output;
            total.cache_read += u.cache_read;
            total.cache_creation += u.cache_creation;
        }
        // Context gauge reads the newest request: prompt-side tokens are what
        // occupy the window.
        let context_tokens =
            self.last_usage.input + self.last_usage.cache_read + self.last_usage.cache_creation;
        let context_pct = if context_tokens > 0 {
            Some((context_tokens as f32 / CONTEXT_WINDOW_TOKENS as f32 * 100.0).min(100.0))
        } else {
            None
        };
        UsageMetrics {
            input_tokens: total.input,
            output_tokens: total.output,
            cache_read_tokens: total.cache_read,
            cache_creation_tokens: total.cache_creation,
            context_pct,
        }
    }

    /// Derive the session state as of `now` (heuristics documented on the
    /// module).
    pub fn state_at(&self, now: DateTime<Utc>) -> SessionState {
        let Some(last) = self.last_activity else {
            return SessionState::Idle;
        };
        let silence = now - last;

        if silence >= STALE_SILENCE {
            return SessionState::Stale;
        }
        if self.interrupted {
            return SessionState::Interrupted;
        }
        if let Some(oldest) = self.pending_tools.values().map(|t| t.since).min() {
            return if now - oldest > PERMISSION_WAIT {
                SessionState::NeedsYou
            } else {
                SessionState::ToolRunning
            };
        }
        if self.last_stop_reason.as_deref() == Some("end_turn") {
            return SessionState::Done;
        }
        if let Some(thinking) = self.last_thinking {
            if now - thinking <= THINKING_DECAY {
                return SessionState::Thinking;
            }
        }
        if silence >= IDLE_SILENCE {
            SessionState::Idle
        } else {
            SessionState::Thinking
        }
    }

    /// What the session is doing right now, for the card's one-liner: the
    /// running tool, else the in-progress todo.
    pub fn current_task(&self) -> Option<String> {
        if let Some(tool) = self.pending_tools.values().max_by_key(|t| t.since) {
            return Some(match &tool.label {
                Some(label) => format!("{}: {}", tool.name, label),
                None => tool.name.clone(),
            });
        }
        self.todos
            .iter()
            .find(|t| t.status == "in_progress")
            .map(|t| t.active_form.clone().unwrap_or_else(|| t.content.clone()))
    }

    /// Normalize to the adapter contract.
    pub fn snapshot(&self, machine: &str, now: DateTime<Utc>) -> AgentSnapshot {
        let state = self.state_at(now);
        let needs_user_reason = match state {
            SessionState::NeedsYou => {
                Some("tool pending — likely waiting for permission".to_string())
            }
            SessionState::Interrupted => Some("interrupted by user".to_string()),
            _ => None,
        };
        AgentSnapshot {
            agent_id: self.session_id.clone().unwrap_or_else(|| "unknown".into()),
            source: AgentSource::ClaudeCode,
            machine: machine.to_string(),
            project: self.cwd.clone().unwrap_or_default(),
            status: state.to_status(),
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

fn u64_field(value: &Value, field: &str) -> u64 {
    value.get(field).and_then(Value::as_u64).unwrap_or(0)
}

fn str_field(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Tool result content is either a plain string or a list of text blocks.
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

    fn assistant_tool_use(secs: i64, id: &str, name: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{}","sessionId":"s1","cwd":"/proj","message":{{"id":"msg_{id}","model":"claude-fable-5","stop_reason":"tool_use","content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{{"command":"ls","description":"List files"}}}}],"usage":{{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":1000,"cache_creation_input_tokens":200}}}}}}"#,
            ts(secs)
        )
    }

    fn tool_result(secs: i64, id: &str, content: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"{}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{id}","content":"{content}"}}]}}}}"#,
            ts(secs)
        )
    }

    #[test]
    fn tool_running_then_needs_you_after_grace() {
        let mut t = ClaudeSessionTracker::new();
        t.ingest_line(&assistant_tool_use(0, "t1", "Bash"));
        assert_eq!(t.state_at(at(1)), SessionState::ToolRunning);
        assert_eq!(t.state_at(at(3)), SessionState::NeedsYou);
        assert_eq!(
            t.current_task().as_deref(),
            Some("Bash: List files"),
            "pending tool should surface as the current task"
        );

        t.ingest_line(&tool_result(4, "t1", "ok"));
        // Result arrived: model is composing the next step.
        assert_eq!(t.state_at(at(5)), SessionState::Thinking);
    }

    #[test]
    fn rejected_tool_result_means_interrupted() {
        let mut t = ClaudeSessionTracker::new();
        t.ingest_line(&assistant_tool_use(0, "t1", "Bash"));
        t.ingest_line(&tool_result(
            1,
            "t1",
            "The user doesn't want to proceed. Tool use was rejected.",
        ));
        assert_eq!(t.state_at(at(2)), SessionState::Interrupted);
        assert_eq!(t.snapshot("wsl", at(2)).status, AgentStatus::NeedsYou);
    }

    #[test]
    fn request_interrupted_marker_means_interrupted() {
        let mut t = ClaudeSessionTracker::new();
        t.ingest_line(&assistant_tool_use(0, "t1", "Bash"));
        let line = format!(
            r#"{{"type":"user","timestamp":"{}","message":{{"role":"user","content":[{{"type":"text","text":"[Request interrupted by user]"}}]}}}}"#,
            ts(1)
        );
        t.ingest_line(&line);
        assert_eq!(t.state_at(at(2)), SessionState::Interrupted);
        // A new user prompt clears the interruption.
        let prompt = format!(
            r#"{{"type":"user","timestamp":"{}","message":{{"role":"user","content":"try again"}}}}"#,
            ts(3)
        );
        t.ingest_line(&prompt);
        assert_eq!(t.state_at(at(4)), SessionState::Thinking);
    }

    #[test]
    fn end_turn_is_done_then_idle_never() {
        let mut t = ClaudeSessionTracker::new();
        let line = format!(
            r#"{{"type":"assistant","timestamp":"{}","sessionId":"s1","message":{{"id":"m1","stop_reason":"end_turn","content":[{{"type":"text","text":"done!"}}],"usage":{{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#,
            ts(0)
        );
        t.ingest_line(&line);
        // Done sticks past the idle threshold — the turn is over, not idle.
        assert_eq!(t.state_at(at(60)), SessionState::Done);
        // ...but a dead-silent transcript eventually goes stale.
        assert_eq!(t.state_at(at(60 * 60)), SessionState::Stale);
    }

    #[test]
    fn thinking_decays_to_idle() {
        let mut t = ClaudeSessionTracker::new();
        let line = format!(
            r#"{{"type":"assistant","timestamp":"{}","message":{{"id":"m1","stop_reason":null,"content":[{{"type":"thinking","thinking":"..."}}],"usage":{{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#,
            ts(0)
        );
        t.ingest_line(&line);
        assert_eq!(t.state_at(at(2)), SessionState::Thinking);
        assert_eq!(t.state_at(at(5)), SessionState::Thinking); // <10s silence
        assert_eq!(t.state_at(at(15)), SessionState::Idle);
    }

    #[test]
    fn usage_dedupes_streamed_lines_of_same_message() {
        let mut t = ClaudeSessionTracker::new();
        // Same message id twice (streamed), then a different message.
        t.ingest_line(&assistant_tool_use(0, "a", "Bash"));
        t.ingest_line(&assistant_tool_use(1, "a", "Bash"));
        t.ingest_line(&assistant_tool_use(2, "b", "Bash"));
        let u = t.usage();
        assert_eq!(u.input_tokens, 200);
        assert_eq!(u.output_tokens, 100);
        assert_eq!(u.cache_read_tokens, 2000);
        // Context gauge reads the newest request: (100+1000+200)/200k.
        assert!((u.context_pct.unwrap() - 0.65).abs() < 0.001);
    }

    #[test]
    fn todos_are_extracted() {
        let mut t = ClaudeSessionTracker::new();
        let line = format!(
            r#"{{"type":"assistant","timestamp":"{}","message":{{"id":"m1","content":[{{"type":"tool_use","id":"td1","name":"TodoWrite","input":{{"todos":[{{"content":"Fix parser","status":"in_progress","activeForm":"Fixing parser"}},{{"content":"Ship it","status":"pending"}}]}}}}]}}}}"#,
            ts(0)
        );
        t.ingest_line(&line);
        t.ingest_line(&tool_result(1, "td1", "Todos have been modified"));
        assert_eq!(t.todos().len(), 2);
        assert_eq!(t.current_task().as_deref(), Some("Fixing parser"));
    }

    #[test]
    fn sidechain_records_do_not_drive_state() {
        let mut t = ClaudeSessionTracker::new();
        t.ingest_line(&assistant_tool_use(0, "t1", "Bash"));
        t.ingest_line(&tool_result(1, "t1", "ok"));
        let side = format!(
            r#"{{"type":"assistant","isSidechain":true,"timestamp":"{}","message":{{"id":"side","stop_reason":"end_turn","content":[],"usage":{{"input_tokens":999,"output_tokens":999,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#,
            ts(2)
        );
        t.ingest_line(&side);
        assert_eq!(t.sidechain_records(), 1);
        assert_eq!(t.usage().output_tokens, 50, "sidechain usage not mixed in");
        assert_ne!(t.state_at(at(3)), SessionState::Done);
    }

    #[test]
    fn garbage_lines_are_counted_not_fatal() {
        let mut t = ClaudeSessionTracker::new();
        t.ingest_line("not json at all");
        t.ingest_line("{\"type\":\"file-history-snapshot\",\"snapshot\":{}}");
        assert_eq!(t.parse_errors(), 1);
        assert_eq!(t.state_at(at(0)), SessionState::Idle);
    }
}
