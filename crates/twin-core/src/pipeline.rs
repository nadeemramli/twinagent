//! The full local-detection pipeline: watch agent data dirs, tail changed
//! JSONL files, feed the right parser, emit [`AgentSnapshot`]s.
//!
//! Shared by the WSL collector daemon and the Windows (Tauri) core — both
//! sides do exactly this, only the machine tag and the roots differ. Roots
//! must be *native* paths: never point a pipeline at `\\wsl$` or `/mnt/c`
//! (file events don't cross the boundary; that's why there are two sides).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

/// Time-decay transitions (idle after silence, tool pending > 2.5s) only
/// need coarse resolution; sweeping every tracked session more often than
/// this burns a core for nothing while an active agent writes its
/// transcript many times a second (TWI-23).
const SWEEP_INTERVAL: Duration = Duration::from_millis(500);

/// Watches a set of roots and turns transcript activity into snapshots.
pub struct Pipeline {
    machine: String,
    roots: Vec<SourceRoot>,
    watcher: DirWatcher,
    files: HashMap<PathBuf, FileSession>,
    /// When the last full snapshot sweep ran.
    last_sweep: Option<Instant>,
    /// Claude Notification-hook event stream (`~/.claude/
    /// twinagent-notify.jsonl`, appended by the hook command): the
    /// authoritative permission-prompt signal. Absent file = quiet no-op.
    notify_tail: Option<TailReader>,
    /// Permission prompts whose `session_id` has no tracker yet (the
    /// transcript file has not been discovered). Newest per session_id;
    /// re-matched on every drain and dropped after [`PERMISSION_BUFFER_TTL`]
    /// so a race between the hook and file discovery never loses a prompt
    /// (BUGHUNT #13).
    pending_permissions: HashMap<String, (DateTime<Utc>, String)>,
}

/// How long (seconds) an unmatched permission prompt waits for its tracker
/// before we give up on it.
const PERMISSION_BUFFER_SECS: i64 = 300;

impl Pipeline {
    /// `rescan_interval` is the watcher's fallback sweep; a few seconds is
    /// fine for a background daemon.
    pub fn new(machine: impl Into<String>, roots: Vec<SourceRoot>, rescan_interval: Duration) -> Self {
        let watcher = DirWatcher::new(
            roots.iter().map(|r| r.path.clone()).collect(),
            rescan_interval,
        );
        // Hook events from this machine's Claude Code (TWIN_CLAUDE_NOTIFY
        // overrides, e.g. for tests). Skip whatever is already in the file:
        // old prompts were answered lifetimes ago.
        let mut notify_tail = std::env::var("TWIN_CLAUDE_NOTIFY")
            .map(PathBuf::from)
            .ok()
            .or_else(|| {
                let home = std::env::var("USERPROFILE")
                    .or_else(|_| std::env::var("HOME"))
                    .ok()?;
                Some(PathBuf::from(home).join(".claude").join("twinagent-notify.jsonl"))
            })
            .map(|path| TailReader::new(&path));
        if let Some(tail) = &mut notify_tail {
            // The PreToolUse hook appends a line per tool call, so the notify
            // file grows with usage. We skip its backlog anyway (old prompts
            // are answered), so if it has gotten large just truncate it — the
            // TailReader resets to offset 0 on a shrink. Boot-time only, so the
            // only race is a hook line landing in the same instant as a
            // collector restart (a missed resolve at worst).
            const NOTIFY_MAX_BYTES: u64 = 1_048_576;
            if std::fs::metadata(tail.path()).map(|m| m.len()).unwrap_or(0) > NOTIFY_MAX_BYTES {
                let _ = std::fs::File::create(tail.path());
            }
            let _ = tail.poll();
        }

        Self {
            machine: machine.into(),
            roots,
            watcher,
            files: HashMap::new(),
            last_sweep: None,
            notify_tail,
            pending_permissions: HashMap::new(),
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
        let mut dirty: Vec<PathBuf> = Vec::new();
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
            dirty.push(event.path);
        }

        // Tail only the files that actually changed — an active agent
        // writes its transcript many times a second, and reopening every
        // tracked file on each write is what burned a core (TWI-23).
        for path in &dirty {
            if let Some(session) = self.files.get_mut(path) {
                // A transient poll error (a read racing a rotation, say) must
                // not lose the file's tail: TailReader leaves its offset
                // untouched on `Err`, so we simply skip this round and the
                // same bytes are re-read on the next poll (BUGHUNT #11).
                if let Ok(lines) = session.reader.poll() {
                    for line in &lines {
                        session.tracker.ingest_line(line);
                    }
                }
            }
        }

        self.drain_notify_events();

        // The full sweep exists for time-decay transitions; cap its rate no
        // matter how fast events arrive. Lines ingested above surface on
        // the next due sweep, at most SWEEP_INTERVAL away.
        if self.last_sweep.is_some_and(|t| t.elapsed() < SWEEP_INTERVAL) {
            return Vec::new();
        }
        self.last_sweep = Some(Instant::now());

        let now = Utc::now();
        let mut out = Vec::new();
        for session in self.files.values_mut() {
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

    /// Route fresh Notification-hook events (permission prompts) to the
    /// matching Claude tracker. Claude Code also sends "waiting for your
    /// input" idle notifications — those are covered by done/idle states
    /// and skipped here, so only genuine permission prompts alert.
    fn drain_notify_events(&mut self) {
        // 1. Fold new hook lines into the pending buffer (newest per
        //    session_id). The prompt's receive time is its request timestamp,
        //    so a buffered prompt keeps the right age when it finally lands.
        //    PreToolUse lines are resolutions — the user answered a prompt.
        let mut resolves: Vec<String> = Vec::new();
        if let Some(tail) = &mut self.notify_tail {
            if let Ok(lines) = tail.poll() {
                let now = Utc::now();
                for line in lines {
                    let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                        continue;
                    };
                    let Some(session_id) = event.get("session_id").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let message = event
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    if message.to_lowercase().contains("permission") {
                        self.pending_permissions
                            .insert(session_id.to_string(), (now, message.to_string()));
                    } else if event.get("hook_event_name").and_then(|v| v.as_str())
                        == Some("PreToolUse")
                    {
                        resolves.push(session_id.to_string());
                    }
                }
            }
        }

        // 1b. Apply resolutions: a PreToolUse means the tool is about to run,
        //     so any prompt for that session was approved — clear it, buffered
        //     or already delivered (BUGHUNT #2).
        for id in &resolves {
            self.pending_permissions.remove(id);
            for session in self.files.values_mut() {
                if let Tracker::Claude(tracker) = &mut session.tracker {
                    if tracker.session_id() == Some(id.as_str()) {
                        tracker.clear_permission_request();
                    }
                }
            }
        }

        if self.pending_permissions.is_empty() {
            return;
        }

        // 2. Forget prompts whose tracker never appeared (a stale hook line, a
        //    session that ended before its transcript was ever discovered).
        let now = Utc::now();
        let ttl = chrono::Duration::seconds(PERMISSION_BUFFER_SECS);
        self.pending_permissions
            .retain(|_, (ts, _)| now - *ts < ttl);

        // 3. Deliver every buffered prompt whose Claude tracker now exists and
        //    drop it; the rest stay buffered for a later drain (BUGHUNT #13).
        let mut delivered: Vec<String> = Vec::new();
        for (session_id, (ts, message)) in &self.pending_permissions {
            let mut matched = false;
            for session in self.files.values_mut() {
                if let Tracker::Claude(tracker) = &mut session.tracker {
                    if tracker.session_id() == Some(session_id.as_str()) {
                        tracker.note_permission_request(*ts, message.clone());
                        matched = true;
                    }
                }
            }
            if matched {
                delivered.push(session_id.clone());
            }
        }
        for id in delivered {
            self.pending_permissions.remove(&id);
        }
    }

    /// Machine-level Claude plan-window estimate, aggregated across every
    /// tracked Claude session (TWI-16). Always tagged estimated; the widget
    /// shows it next to Codex's exact `rate_limits` numbers.
    pub fn claude_plan_estimate(
        &self,
        now: DateTime<Utc>,
    ) -> crate::plan_usage::PlanEstimate {
        // Dedup by API message id across trackers (last write wins): a message
        // duplicated across resumed/compacted transcript files must count once
        // (BUGHUNT #3).
        let mut by_id: HashMap<String, (DateTime<Utc>, crate::plan_usage::TokenCounts)> =
            HashMap::new();
        for session in self.files.values() {
            if let Tracker::Claude(t) = &session.tracker {
                for (id, ts, tokens) in t.usage_events() {
                    by_id.insert(id, (ts, tokens));
                }
            }
        }
        let events: Vec<_> = by_id.into_values().collect();
        crate::plan_usage::estimate(&events, now)
    }

    /// Newest exact Codex plan reading across every tracked rollout —
    /// plan usage is account-level, so the freshest observation wins.
    pub fn codex_rate_limits(&self) -> Option<crate::codex::RateLimits> {
        self.files
            .values()
            .filter_map(|s| match &s.tracker {
                Tracker::Codex(t) => t.rate_limits().cloned(),
                Tracker::Claude(_) => None,
            })
            .max_by_key(|rl| rl.observed_at)
    }

    /// Machine-level usage report for the hub: exact Codex windows plus the
    /// estimated Claude windows, in one envelope. The exact Claude reading
    /// (`claude_exact`) is filled in by the collector's OAuth poller — the
    /// pipeline itself stays HTTP-free.
    pub fn usage_report(&self, now: DateTime<Utc>) -> crate::model::UsageReport {
        crate::model::UsageReport {
            machine: self.machine.clone(),
            claude: Some(self.claude_plan_estimate(now)),
            claude_exact: None,
            codex: self.codex_rate_limits(),
            reported_at: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        }
    }

    /// Historical usage aggregation for the stats pane (today/7d/30d/all),
    /// from every tracked session's usage events.
    pub fn usage_stats(&self, now: DateTime<Utc>) -> crate::stats::UsageStats {
        // Claude message ids can repeat across resumed/compacted transcript
        // files; attribute each id to a single tracker (last writer wins) so
        // the stats pane doesn't double-count its tokens (BUGHUNT #3). Codex
        // deltas are per-observation and carry no shared id — pass through.
        let claude: Vec<(&ClaudeSessionTracker, Vec<_>)> = self
            .files
            .values()
            .filter_map(|s| match &s.tracker {
                Tracker::Claude(t) => Some((t, t.usage_events())),
                Tracker::Codex(_) => None,
            })
            .collect();
        let mut owner: HashMap<String, usize> = HashMap::new();
        for (i, (_, events)) in claude.iter().enumerate() {
            for (id, _, _) in events {
                owner.insert(id.clone(), i);
            }
        }

        let mut sessions: Vec<crate::stats::SessionUsage> = claude
            .iter()
            .enumerate()
            .map(|(i, (tracker, events))| crate::stats::SessionUsage {
                source: AgentSource::ClaudeCode,
                model: tracker.model().map(String::from),
                events: events
                    .iter()
                    .filter(|(id, _, _)| owner.get(id) == Some(&i))
                    .map(|(_, ts, tokens)| (*ts, *tokens))
                    .collect(),
            })
            .collect();
        for s in self.files.values() {
            if let Tracker::Codex(t) = &s.tracker {
                sessions.push(crate::stats::SessionUsage {
                    source: AgentSource::Codex,
                    model: t.model().map(String::from),
                    events: t.usage_events(),
                });
            }
        }
        crate::stats::compute(&self.machine, sessions.into_iter(), now)
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
    fn duplicate_message_id_across_files_counts_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut pipeline = Pipeline::new(
            "wsl",
            vec![SourceRoot::claude(dir.path())],
            Duration::from_millis(50),
        );
        let now = Utc::now();
        let iso = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        // The same session resumed into a second transcript file: message id
        // "m1" (from claude_line) appears in both, in two separate trackers.
        append(&dir.path().join("sess-1.jsonl"), &claude_line(&iso, "end_turn"));
        append(
            &dir.path().join("sess-1-resume.jsonl"),
            &claude_line(&iso, "end_turn"),
        );
        poll_until(&mut pipeline, Duration::from_secs(5), |s| s.len() >= 2);

        // Plan estimate: the shared id is summed a single time.
        let est = pipeline.claude_plan_estimate(now);
        assert_eq!(est.weekly.messages, 1);
        assert_eq!(est.weekly.tokens.input_tokens, 10);

        // Stats: same — one message, one message's tokens, not doubled.
        let stats = pipeline.usage_stats(now);
        let all = stats.periods.iter().find(|p| p.key == "all").unwrap();
        assert_eq!(all.claude.messages, 1);
        assert_eq!(all.claude.input_tokens, 10);
    }

    #[test]
    fn permission_prompt_buffered_until_tracker_appears() {
        let dir = tempfile::tempdir().unwrap();
        let mut pipeline = Pipeline::new(
            "wsl",
            vec![SourceRoot::claude(dir.path())],
            Duration::from_millis(50),
        );
        // Isolate from any real notify file; drive the buffer directly.
        pipeline.notify_tail = None;
        let now = Utc::now();
        // A permission prompt arrives before the session's transcript has been
        // discovered — the exact race BUGHUNT #13 was dropping.
        pipeline.pending_permissions.insert(
            "sess-1".to_string(),
            (now, "Claude needs your permission to use Bash".to_string()),
        );
        pipeline.poll(Duration::from_millis(30));
        assert_eq!(
            pipeline.pending_permissions.len(),
            1,
            "an unmatched prompt must stay buffered, not be dropped"
        );

        // The transcript appears; the buffered prompt must now drive needs-you.
        let iso = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        append(&dir.path().join("sess-1.jsonl"), &claude_line(&iso, "end_turn"));
        let snaps = poll_until(&mut pipeline, Duration::from_secs(5), |s| {
            s.iter().any(|x| x.status == AgentStatus::NeedsYou)
        });
        assert!(
            snaps
                .iter()
                .any(|x| x.agent_id == "sess-1" && x.status == AgentStatus::NeedsYou),
            "buffered prompt must deliver once its tracker appears: {snaps:?}"
        );
        assert!(
            pipeline.pending_permissions.is_empty(),
            "a delivered prompt is dropped from the buffer"
        );
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
