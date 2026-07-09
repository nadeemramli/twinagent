//! Golden tests: replay sanitized real transcripts from `tests/fixtures/`
//! and compare the parser's full output — state timeline, session metadata,
//! token totals — against committed snapshots.
//!
//! The transcript JSONL schema is an undocumented Claude Code internal; these
//! tests are the tripwire for format drift. When a change is intentional,
//! regenerate with:
//!
//! ```sh
//! UPDATE_GOLDEN=1 cargo test -p twin-core --test golden
//! ```

use std::path::PathBuf;

use serde::Serialize;
use twin_core::claude_code::{ClaudeSessionTracker, SessionState};
use twin_core::UsageMetrics;

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
    sidechain_records: u64,
    todos: usize,
    usage: UsageMetrics,
    final_state: SessionState,
    timeline: Vec<TimelineEntry>,
}

/// Replay a fixture and summarize what the widget would have shown: the
/// state is evaluated at each record's own timestamp, and only transitions
/// are recorded.
fn replay(name: &str) -> String {
    let path = fixture_path(&format!("{name}.jsonl"));
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    let mut tracker = ClaudeSessionTracker::new();
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

    let end = tracker.last_activity().unwrap_or_default();
    let summary = Summary {
        session_id: tracker.session_id().map(String::from),
        cwd: tracker.cwd().map(String::from),
        git_branch: tracker.git_branch().map(String::from),
        model: tracker.model().map(String::from),
        parse_errors: tracker.parse_errors(),
        sidechain_records: tracker.sidechain_records(),
        todos: tracker.todos().len(),
        usage: tracker.usage(),
        final_state: tracker.state_at(end),
        timeline,
    };
    let mut json = serde_json::to_string_pretty(&summary).unwrap();
    json.push('\n');
    json
}

fn fixture_path(file: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(file)
}

fn check(name: &str) {
    let actual = replay(name);
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
    check("tool_heavy_done");
}

#[test]
fn interrupted() {
    check("interrupted");
}

#[test]
fn plain_chat_done() {
    check("plain_chat_done");
}
