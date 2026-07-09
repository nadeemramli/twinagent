//! Print the Claude plan-window estimate from local transcripts.
//!
//! Usage: cargo run -p twin-core --example plan -- [claude-projects-dir]
//! (defaults to ~/.claude/projects)

use std::time::Duration;

use twin_core::{Pipeline, SourceRoot};

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| {
        let home = std::env::var("HOME").expect("HOME not set");
        format!("{home}/.claude/projects")
    });

    let mut pipeline = Pipeline::new("local", vec![SourceRoot::claude(&root)], Duration::from_secs(1));
    // Pump until the initial scan settles (two consecutive quiet polls).
    let mut quiet = 0;
    while quiet < 2 {
        if pipeline.poll(Duration::from_millis(200)).is_empty() {
            quiet += 1;
        } else {
            quiet = 0;
        }
    }

    let estimate = pipeline.claude_plan_estimate(chrono::Utc::now());
    println!("{}", serde_json::to_string_pretty(&estimate).unwrap());
}
