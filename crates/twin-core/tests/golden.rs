//! Golden tests: replay sanitized real transcripts from `tests/fixtures/`
//! and compare the parser's full output — state timeline, session metadata,
//! token totals — against committed snapshots.
//!
//! Claude Code transcripts and Codex rollouts are undocumented internal
//! formats; these tests are the tripwire for format drift. When a change is
//! intentional, regenerate with:
//!
//! ```sh
//! UPDATE_GOLDEN=1 cargo test -p twin-core --test golden
//! ```

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;
use twin_core::claude_code::ClaudeSessionTracker;
use twin_core::codex::{CodexSessionTracker, RateLimits};
use twin_core::{SessionState, UsageMetrics};

/// The shared surface of both session trackers, for the replay loop.
trait Replayable {
    fn ingest_line(&mut self, line: &str);
    fn last_activity(&self) -> Option<DateTime<Utc>>;
    fn state_at(&self, now: DateTime<Utc>) -> SessionState;
    fn current_task(&self) -> Option<String>;
    fn summarize(&self, timeline: Vec<TimelineEntry>) -> Summary;
}

#[derive(Serialize)]
struct TimelineEntry {
    ts: String,
    state: SessionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<String>,
}

#[derive(Serialize)]
struct Summary {
    session_id: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    model: Option<String>,
    parse_errors: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    sidechain_records: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    todos: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    approval_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rate_limits: Option<RateLimits>,
    usage: UsageMetrics,
    final_state: SessionState,
    timeline: Vec<TimelineEntry>,
}

impl Replayable for ClaudeSessionTracker {
    fn ingest_line(&mut self, line: &str) {
        ClaudeSessionTracker::ingest_line(self, line)
    }
    fn last_activity(&self) -> Option<DateTime<Utc>> {
        ClaudeSessionTracker::last_activity(self)
    }
    fn state_at(&self, now: DateTime<Utc>) -> SessionState {
        ClaudeSessionTracker::state_at(self, now)
    }
    fn current_task(&self) -> Option<String> {
        ClaudeSessionTracker::current_task(self)
    }
    fn summarize(&self, timeline: Vec<TimelineEntry>) -> Summary {
        let end = self.last_activity().unwrap_or_default();
        Summary {
            session_id: self.session_id().map(String::from),
            cwd: self.cwd().map(String::from),
            git_branch: self.git_branch().map(String::from),
            model: self.model().map(String::from),
            parse_errors: self.parse_errors(),
            sidechain_records: Some(self.sidechain_records()),
            todos: Some(self.todos().len()),
            approval_policy: None,
            rate_limits: None,
            usage: self.usage(),
            final_state: self.state_at(end),
            timeline,
        }
    }
}

impl Replayable for CodexSessionTracker {
    fn ingest_line(&mut self, line: &str) {
        CodexSessionTracker::ingest_line(self, line)
    }
    fn last_activity(&self) -> Option<DateTime<Utc>> {
        CodexSessionTracker::last_activity(self)
    }
    fn state_at(&self, now: DateTime<Utc>) -> SessionState {
        CodexSessionTracker::state_at(self, now)
    }
    fn current_task(&self) -> Option<String> {
        CodexSessionTracker::current_task(self)
    }
    fn summarize(&self, timeline: Vec<TimelineEntry>) -> Summary {
        let end = self.last_activity().unwrap_or_default();
        Summary {
            session_id: self.session_id().map(String::from),
            cwd: self.cwd().map(String::from),
            git_branch: self.git_branch().map(String::from),
            model: self.model().map(String::from),
            parse_errors: self.parse_errors(),
            sidechain_records: None,
            todos: None,
            approval_policy: self.approval_policy().map(String::from),
            rate_limits: self.rate_limits().cloned(),
            usage: self.usage(),
            final_state: self.state_at(end),
            timeline,
        }
    }
}

/// Replay a fixture and summarize what the widget would have shown: the
/// state is evaluated at each record's own timestamp, and only transitions
/// are recorded.
fn replay(mut tracker: impl Replayable, name: &str) -> String {
    let path = fixture_path(&format!("{name}.jsonl"));
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    let mut timeline = Vec::new();
    let mut last_state = None;
    for line in content.lines() {
        tracker.ingest_line(line);
        let Some(ts) = tracker.last_activity() else {
            continue;
        };
        let state = tracker.state_at(ts);
        if last_state != Some(state) {
            timeline.push(TimelineEntry {
                ts: ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                state,
                task: tracker.current_task(),
            });
            last_state = Some(state);
        }
    }

    let mut json = serde_json::to_string_pretty(&tracker.summarize(timeline)).unwrap();
    json.push('\n');
    json
}

fn fixture_path(file: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(file)
}

fn check(tracker: impl Replayable, name: &str) {
    let actual = replay(tracker, name);
    let expected_path = fixture_path(&format!("{name}.expected.json"));

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&expected_path, &actual).unwrap();
        return;
    }

    let expected = std::fs::read_to_string(&expected_path).unwrap_or_else(|e| {
        panic!(
            "read {}: {e}\nrun UPDATE_GOLDEN=1 cargo test -p twin-core --test golden to create it",
            expected_path.display()
        )
    });
    assert_eq!(
        actual, expected,
        "parser output for {name} drifted from the golden snapshot; if the \
         change is intentional, regenerate with UPDATE_GOLDEN=1"
    );
}

#[test]
fn tool_heavy_done() {
    check(ClaudeSessionTracker::new(), "tool_heavy_done");
}

#[test]
fn interrupted() {
    check(ClaudeSessionTracker::new(), "interrupted");
}

#[test]
fn plain_chat_done() {
    check(ClaudeSessionTracker::new(), "plain_chat_done");
}

#[test]
fn codex_tool_session() {
    check(CodexSessionTracker::new(), "codex_tool_session");
}

#[test]
fn codex_aborted() {
    check(CodexSessionTracker::new(), "codex_aborted");
}
