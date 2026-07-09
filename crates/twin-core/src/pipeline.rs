//! The full local-detection pipeline: watch agent data dirs, tail changed
//! JSONL files, feed the right parser, emit [`AgentSnapshot`]s.
//!
//! Shared by the WSL collector daemon and the Windows (Tauri) core — both
//! sides do exactly this, only the machine tag and the roots differ. Roots
//! must be *native* paths: never point a pipeline at `\\wsl$` or `/mnt/c`
//! (file events don't cross the boundary; that's why there are two sides).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::claude_code::ClaudeSessionTracker;
use crate::codex::CodexSessionTracker;
use crate::model::{AgentSnapshot, AgentSource};
use crate::tail::TailReader;
use crate::watch::DirWatcher;

/// One watched directory tree and the transcript format found inside it.
#[derive(Debug, Clone)]
pub struct SourceRoot {
    pub path: PathBuf,
    pub source: AgentSource,
}

impl SourceRoot {
    pub fn claude(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            source: AgentSource::ClaudeCode,
        }
    }

    pub fn codex(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            source: AgentSource::Codex,
        }
    }
}

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

    fn snapshot(&self, machine: &str, now: DateTime<Utc>) -> AgentSnapshot {
        match self {
            Tracker::Claude(t) => t.snapshot(machine, now),
            Tracker::Codex(t) => t.snapshot(machine, now),
        }
    }
}

struct FileSession {
    reader: TailReader,
    tracker: Tracker,
    /// What we last told the hub, to emit heuristic-driven transitions
    /// (e.g. tool pending crossing 2.5s) even when the file is silent.
    last_emitted: Option<AgentSnapshot>,
}

/// Watches a set of roots and turns transcript activity into snapshots.
pub struct Pipeline {
    machine: String,
    roots: Vec<SourceRoot>,
    watcher: DirWatcher,
    files: HashMap<PathBuf, FileSession>,
}

impl Pipeline {
    /// `rescan_interval` is the watcher's fallback sweep; a few seconds is
    /// fine for a background daemon.
    pub fn new(machine: impl Into<String>, roots: Vec<SourceRoot>, rescan_interval: Duration) -> Self {
        let watcher = DirWatcher::new(
            roots.iter().map(|r| r.path.clone()).collect(),
            rescan_interval,
        );
        Self {
            machine: machine.into(),
            roots,
            watcher,
            files: HashMap::new(),
        }
    }

    fn source_for(&self, path: &Path) -> Option<AgentSource> {
        self.roots
            .iter()
            .filter(|r| path.starts_with(&r.path))
            // Nested roots: the deepest match wins.
            .max_by_key(|r| r.path.components().count())
            .map(|r| r.source.clone())
    }

    /// Claude Code subagent transcripts live under `<session>/subagents/`.
    /// They are sidechains of the parent session, not sessions of their own —
    /// skip them so the widget doesn't grow a card per subagent. (Surfacing
    /// live subagents on the parent card is a Phase 2 nicety.)
    fn is_subagent_file(path: &Path) -> bool {
        path.parent()
            .and_then(Path::file_name)
            .is_some_and(|d| d == "subagents")
    }

    /// Pump the pipeline once: collect file events (waiting up to `wait`),
    /// ingest new lines, and return a snapshot for every session whose
    /// visible state changed — including pure time-decay transitions.
    pub fn poll(&mut self, wait: Duration) -> Vec<AgentSnapshot> {
        for event in self.watcher.poll(wait) {
            if Self::is_subagent_file(&event.path) {
                continue;
            }
            let Some(source) = self.source_for(&event.path) else {
                continue;
            };
            self.files.entry(event.path.clone()).or_insert_with(|| {
                // New discovery (including catch-up after a restart): read
                // from the beginning so token totals and metadata are
                // complete. TailReader keeps us incremental from here on.
                FileSession {
                    reader: TailReader::new(&event.path),
                    tracker: match source {
                        AgentSource::Codex => Tracker::Codex(CodexSessionTracker::new()),
                        _ => Tracker::Claude(ClaudeSessionTracker::new()),
                    },
                    last_emitted: None,
                }
            });
        }

        let now = Utc::now();
        let mut out = Vec::new();
        for session in self.files.values_mut() {
            if let Ok(lines) = session.reader.poll() {
                for line in &lines {
                    session.tracker.ingest_line(line);
                }
            }
            let snapshot = session.tracker.snapshot(&self.machine, now);
            // Skip files that produced no session yet (empty/foreign files).
            if snapshot.agent_id == "unknown" && snapshot.last_activity.is_empty() {
                continue;
            }
            let changed = match &session.last_emitted {
                None => true,
                Some(prev) => {
                    prev.status != snapshot.status
                        || prev.current_task != snapshot.current_task
                        || prev.usage != snapshot.usage
                        || prev.last_activity != snapshot.last_activity
                        || prev.needs_user != snapshot.needs_user
                }
            };
            if changed {
                session.last_emitted = Some(snapshot.clone());
                out.push(snapshot);
            }
        }
        out
    }

    /// Everything currently tracked, evaluated now — for a full resend after
    /// the hub connection drops.
    pub fn all_snapshots(&self) -> Vec<AgentSnapshot> {
        let now = Utc::now();
        self.files
            .values()
            .filter(|s| s.last_emitted.is_some())
            .map(|s| s.tracker.snapshot(&self.machine, now))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AgentStatus;
    use std::fs::OpenOptions;
    use std::io::Write;

    fn append(path: &Path, line: &str) {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    fn claude_line(ts: &str, stop: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","sessionId":"sess-1","cwd":"/proj","message":{{"id":"m1","model":"claude-fable-5","stop_reason":"{stop}","content":[{{"type":"text","text":"hi"}}],"usage":{{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#
        )
    }

    fn poll_until(
        pipeline: &mut Pipeline,
        deadline: Duration,
        pred: impl Fn(&[AgentSnapshot]) -> bool,
    ) -> Vec<AgentSnapshot> {
        let start = std::time::Instant::now();
        let mut all = Vec::new();
        while start.elapsed() < deadline {
            all.extend(pipeline.poll(Duration::from_millis(30)));
            if pred(&all) {
                break;
            }
        }
        all
    }

    #[test]
    fn discovers_parses_and_emits_once_per_change() {
        let dir = tempfile::tempdir().unwrap();
        let mut pipeline = Pipeline::new(
            "wsl",
            vec![SourceRoot::claude(dir.path())],
            Duration::from_millis(50),
        );

        let file = dir.path().join("sess-1.jsonl");
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        append(&file, &claude_line(&now, "end_turn"));

        let snaps = poll_until(&mut pipeline, Duration::from_secs(5), |s| !s.is_empty());
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].agent_id, "sess-1");
        assert_eq!(snaps[0].machine, "wsl");
        assert_eq!(snaps[0].status, AgentStatus::Done);
        assert_eq!(snaps[0].usage.input_tokens, 10);

        // Nothing changed: quiet.
        assert!(pipeline.poll(Duration::from_millis(30)).is_empty());
        assert_eq!(pipeline.all_snapshots().len(), 1);
    }

    #[test]
    fn subagent_transcripts_are_not_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sess-1").join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        let mut pipeline = Pipeline::new(
            "wsl",
            vec![SourceRoot::claude(dir.path())],
            Duration::from_millis(50),
        );
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        append(&sub.join("agent-x.jsonl"), &claude_line(&now, "end_turn"));

        let snaps = poll_until(&mut pipeline, Duration::from_millis(400), |s| !s.is_empty());
        assert!(snaps.is_empty(), "subagent file must not become a card");
    }

    #[test]
    fn restart_rebuilds_complete_state_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sess-1.jsonl");
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        append(&file, &claude_line(&now, "end_turn"));

        // First run sees the file...
        let mut first = Pipeline::new(
            "wsl",
            vec![SourceRoot::claude(dir.path())],
            Duration::from_millis(50),
        );
        assert!(!poll_until(&mut first, Duration::from_secs(5), |s| !s.is_empty()).is_empty());
        drop(first);

        // ...a second pipeline (fresh process) catches up from the start and
        // reproduces the same totals.
        let mut second = Pipeline::new(
            "wsl",
            vec![SourceRoot::claude(dir.path())],
            Duration::from_millis(50),
        );
        let snaps = poll_until(&mut second, Duration::from_secs(5), |s| !s.is_empty());
        assert_eq!(snaps[0].usage.input_tokens, 10);
    }
}
