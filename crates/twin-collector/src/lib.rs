//! Collector internals: config from env, and a hub forwarder that buffers
//! through outages. The binary in `main.rs` is a thin loop over these.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use twin_core::{AgentSnapshot, SourceRoot, UsageReport};

/// Runtime configuration, entirely from environment variables so the
/// systemd unit is the single place to tweak it.
#[derive(Debug, Clone)]
pub struct Config {
    /// Hub ingest endpoint, e.g. `http://127.0.0.1:8787`.
    pub hub_url: String,
    /// Machine tag stamped on every snapshot (default `wsl`).
    pub machine: String,
    pub roots: Vec<SourceRoot>,
}

impl Config {
    /// Read config from env, defaulting to this user's `~/.claude` and
    /// `~/.codex` inside the current filesystem namespace. Roots that don't
    /// exist are kept — the watcher picks them up if they appear later.
    pub fn from_env() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let claude = std::env::var("TWIN_CLAUDE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".claude/projects"));
        let codex = std::env::var("TWIN_CODEX_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".codex/sessions"));
        Self {
            hub_url: std::env::var("TWIN_HUB_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8787".into()),
            machine: std::env::var("TWIN_MACHINE").unwrap_or_else(|_| "wsl".into()),
            roots: vec![SourceRoot::claude(claude), SourceRoot::codex(codex)],
        }
    }
}

/// Sends snapshots to the hub, buffering the latest state per session while
/// the hub is unreachable and resending everything once it's back.
pub struct Forwarder {
    endpoint: String,
    usage_endpoint: String,
    /// Latest unacknowledged snapshot per session key.
    pending: HashMap<String, AgentSnapshot>,
    /// Latest unacknowledged plan-usage report (state, so newest wins).
    pending_usage: Option<UsageReport>,
    backoff: Duration,
    next_attempt: Instant,
}

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

impl Forwarder {
    pub fn new(hub_url: &str) -> Self {
        let base = hub_url.trim_end_matches('/');
        Self {
            endpoint: format!("{base}/v1/snapshots"),
            usage_endpoint: format!("{base}/v1/usage"),
            pending: HashMap::new(),
            pending_usage: None,
            backoff: BACKOFF_MIN,
            next_attempt: Instant::now(),
        }
    }

    /// Queue snapshots (newest wins per session) and try to flush.
    pub fn send(&mut self, snapshots: Vec<AgentSnapshot>) {
        for s in snapshots {
            let key = format!("{}/{}/{}", s.machine, s.source, s.agent_id);
            self.pending.insert(key, s);
        }
        self.flush();
    }

    /// Queue the machine's plan-usage report (newest wins) and try to flush.
    /// The hub dedupes unchanged reports, so a fixed cadence is fine.
    pub fn send_usage(&mut self, report: UsageReport) {
        self.pending_usage = Some(report);
        self.flush();
    }

    /// How many snapshots are waiting on the hub to come back.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    fn flush(&mut self) {
        if (self.pending.is_empty() && self.pending_usage.is_none())
            || Instant::now() < self.next_attempt
        {
            return;
        }

        let result = (|| -> Result<(), ureq::Error> {
            if !self.pending.is_empty() {
                let batch: Vec<&AgentSnapshot> = self.pending.values().collect();
                ureq::post(&self.endpoint)
                    .timeout(Duration::from_secs(5))
                    .send_json(&batch)?;
                self.pending.clear();
            }
            if let Some(report) = &self.pending_usage {
                ureq::post(&self.usage_endpoint)
                    .timeout(Duration::from_secs(5))
                    .send_json(report)?;
                self.pending_usage = None;
            }
            Ok(())
        })();

        match result {
            Ok(()) => self.backoff = BACKOFF_MIN,
            Err(err) => {
                // Keep the buffers; retry with exponential backoff. The hub
                // restarting is normal (the Tauri app owns it).
                eprintln!(
                    "twin-collector: hub unreachable ({err}); {} snapshot(s) buffered, retrying in {:?}",
                    self.pending.len(),
                    self.backoff
                );
                self.next_attempt = Instant::now() + self.backoff;
                self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}
