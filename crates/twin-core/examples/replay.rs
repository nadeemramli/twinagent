//! Replay a Claude Code transcript and print the derived state timeline.
//!
//! Usage: cargo run -p twin-core --example replay -- <transcript.jsonl>
//!
//! Evaluates the state machine at each record's own timestamp, so the
//! timeline shows what the widget would have displayed live.

use std::io::BufRead;

use twin_core::claude_code::ClaudeSessionTracker;

fn main() {
    let path = std::env::args().nth(1).expect("usage: replay <file.jsonl>");
    let file = std::fs::File::open(&path).expect("open transcript");

    let mut tracker = ClaudeSessionTracker::new();
    let mut last_state = None;
    for line in std::io::BufReader::new(file).lines() {
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
    println!("sidechain_records: {}", tracker.sidechain_records());
    println!("todos: {}", tracker.todos().len());
    println!(
        "final snapshot @ last activity:\n{}",
        serde_json::to_string_pretty(&tracker.snapshot("wsl", now)).unwrap()
    );
}
