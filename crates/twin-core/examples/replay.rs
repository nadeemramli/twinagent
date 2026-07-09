//! Replay an agent transcript and print the derived state timeline.
//!
//! Usage: cargo run -p twin-core --example replay -- <transcript.jsonl>
//!
//! Detects the format from the first line (Codex rollouts are
//! `{timestamp, type, payload}` envelopes; anything else is treated as a
//! Claude Code transcript) and evaluates the state machine at each record's
//! own timestamp, so the timeline shows what the widget would have displayed
//! live.

use std::io::BufRead;

use chrono::{DateTime, Utc};
use twin_core::claude_code::ClaudeSessionTracker;
use twin_core::codex::CodexSessionTracker;
use twin_core::{AgentSnapshot, SessionState};

enum Tracker {
    Claude(ClaudeSessionTracker),
    Codex(CodexSessionTracker),
}

impl Tracker {
    fn ingest_line(&mut self, line: &str) {
        match self {
            Tracker::Claude(t) => t.ingest_line(line),
            Tracker::Codex(t) => t.ingest_line(line),
        }
    }

    fn last_activity(&self) -> Option<DateTime<Utc>> {
        match self {
            Tracker::Claude(t) => t.last_activity(),
            Tracker::Codex(t) => t.last_activity(),
        }
    }

    fn state_at(&self, now: DateTime<Utc>) -> SessionState {
        match self {
            Tracker::Claude(t) => t.state_at(now),
            Tracker::Codex(t) => t.state_at(now),
        }
    }

    fn current_task(&self) -> Option<String> {
        match self {
            Tracker::Claude(t) => t.current_task(),
            Tracker::Codex(t) => t.current_task(),
        }
    }

    fn parse_errors(&self) -> u64 {
        match self {
            Tracker::Claude(t) => t.parse_errors(),
            Tracker::Codex(t) => t.parse_errors(),
        }
    }

    fn snapshot(&self, machine: &str, now: DateTime<Utc>) -> AgentSnapshot {
        match self {
            Tracker::Claude(t) => t.snapshot(machine, now),
            Tracker::Codex(t) => t.snapshot(machine, now),
        }
    }
}

fn is_codex(first_line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(first_line)
        .ok()
        .and_then(|v| v.get("payload").map(|p| !p.is_null()))
        .unwrap_or(false)
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: replay <file.jsonl>");
    let file = std::fs::File::open(&path).expect("open transcript");
    let mut lines = std::io::BufReader::new(file).lines();

    let first = lines
        .next()
        .expect("empty transcript")
        .expect("read first line");
    let mut tracker = if is_codex(&first) {
        eprintln!("(codex rollout)");
        Tracker::Codex(CodexSessionTracker::new())
    } else {
        eprintln!("(claude code transcript)");
        Tracker::Claude(ClaudeSessionTracker::new())
    };

    let mut last_state = None;
    for line in std::iter::once(Ok(first)).chain(lines) {
        let line = line.expect("read line");
        tracker.ingest_line(&line);
        let Some(ts) = tracker.last_activity() else {
            continue;
        };
        let state = tracker.state_at(ts);
        if last_state != Some(state) {
            println!(
                "{}  {:?}{}",
                ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                state,
                tracker
                    .current_task()
                    .map(|t| format!("  [{t}]"))
                    .unwrap_or_default()
            );
            last_state = Some(state);
        }
    }

    let now = tracker.last_activity().unwrap_or_default();
    println!("\nparse_errors: {}", tracker.parse_errors());
    if let Tracker::Claude(t) = &tracker {
        println!("sidechain_records: {}", t.sidechain_records());
        println!("todos: {}", t.todos().len());
    }
    if let Tracker::Codex(t) = &tracker {
        println!(
            "rate_limits: {}",
            t.rate_limits()
                .map(|rl| serde_json::to_string(rl).unwrap())
                .unwrap_or_else(|| "none".into())
        );
    }
    println!(
        "final snapshot @ last activity:\n{}",
        serde_json::to_string_pretty(&tracker.snapshot("local", now)).unwrap()
    );
}
