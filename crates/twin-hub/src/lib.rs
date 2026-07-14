//! Session registry, persistence, and push to UI clients.
//!
//! Built as a library so the Tauri app embeds it directly (local-first
//! default); the thin `twin-hub` binary serves the Phase 3 standalone VPS
//! deployment. Collectors POST [`AgentSnapshot`]s in; UI clients hold a
//! WebSocket and receive the full state once, then diffs.

pub mod dedup;
pub mod embedded;
pub mod registry;
pub mod server;
pub mod store;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use twin_core::{AgentSnapshot, UsageReport};

pub use dedup::{dedupe, logical_key, normalize_project};
pub use embedded::EmbeddedCollector;
pub use registry::{session_key, Delta, Hub};
pub use store::Store;

/// Sessions silent for this long are flipped to stale by [`HubService::sweep`].
pub const STALE_AFTER: Duration = Duration::minutes(30);
/// Sessions silent for this long are dropped entirely.
pub const RETENTION: Duration = Duration::hours(24);

/// One message on the UI push channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Complete state — sent to each WebSocket client on connect.
    Full { sessions: Vec<AgentSnapshot> },
    /// A session appeared or changed.
    Upsert {
        key: String,
        /// Machine-agnostic identity (`source/agent_id`) — UI clients key
        /// their cards by this so cross-side duplicates collapse (TWI-10).
        logical_key: String,
        session: AgentSnapshot,
        /// True on the edge into needs-you — the toast trigger.
        entered_needs_you: bool,
    },
    /// A session aged out.
    Removed { key: String },
    /// A machine's plan usage changed (exact Codex windows and/or the
    /// estimated Claude windows — each side carries its own confidence tag).
    Usage { report: UsageReport },
}

struct Inner {
    registry: Mutex<Hub>,
    store: Option<Mutex<Store>>,
    /// Latest plan-usage report per machine. Ephemeral by design — the
    /// collectors regenerate it from disk within seconds of starting.
    usage: Mutex<HashMap<String, UsageReport>>,
    /// Latest historical-usage aggregation per machine (stats pane).
    /// Ephemeral for the same reason.
    stats: Mutex<HashMap<String, twin_core::UsageStats>>,
    tx: broadcast::Sender<Event>,
}

/// The embeddable hub: registry + optional persistence + broadcast.
///
/// Clone freely — all clones share state.
#[derive(Clone)]
pub struct HubService {
    inner: Arc<Inner>,
}

impl HubService {
    /// Create a hub. With a store, previously persisted sessions are loaded
    /// so the widget shows history immediately after a restart.
    pub fn new(store: Option<Store>) -> Self {
        let (tx, _) = broadcast::channel(256);
        let mut registry = Hub::new();
        if let Some(store) = &store {
            if let Ok(sessions) = store.load_sessions() {
                for snapshot in sessions {
                    registry.upsert(snapshot);
                }
            }
        }
        Self {
            inner: Arc::new(Inner {
                registry: Mutex::new(registry),
                store: store.map(Mutex::new),
                usage: Mutex::new(HashMap::new()),
                stats: Mutex::new(HashMap::new()),
                tx,
            }),
        }
    }

    /// Ingest one snapshot: update the registry and, if anything visible
    /// changed, persist it and push a diff to subscribers.
    pub fn ingest(&self, snapshot: AgentSnapshot) -> Delta {
        let key = session_key(&snapshot);
        let (delta, prev_status) = {
            let mut registry = self.inner.registry.lock().unwrap();
            let prev_status = registry.get(&key).map(|s| s.status);
            (registry.upsert(snapshot.clone()), prev_status)
        };
        if delta == Delta::Unchanged {
            return delta;
        }

        let status_changed = match delta {
            Delta::New => true,
            Delta::Updated { status_changed, .. } => status_changed,
            Delta::Unchanged => unreachable!(),
        };
        if let Some(store) = &self.inner.store {
            let store = store.lock().unwrap();
            let _ = store.save_session(&snapshot);
            if status_changed {
                let _ = store.record_transition(
                    &key,
                    &snapshot.last_activity,
                    prev_status.map(status_str).as_deref(),
                    &status_str(snapshot.status),
                );
            }
        }

        let entered_needs_you = matches!(
            delta,
            Delta::Updated {
                entered_needs_you: true,
                ..
            }
        ) || (delta == Delta::New && snapshot.needs_user);
        let _ = self.inner.tx.send(Event::Upsert {
            key,
            logical_key: dedup::logical_key(&snapshot),
            session: snapshot,
            entered_needs_you,
        });
        delta
    }

    /// Periodic housekeeping: stale-mark silent sessions and drop expired
    /// ones, pushing diffs for everything that changed.
    pub fn sweep(&self, now: DateTime<Utc>) {
        let (staled_keys, pruned): (Vec<String>, Vec<String>) = {
            let mut registry = self.inner.registry.lock().unwrap();
            let staled = registry.mark_stale(now, STALE_AFTER);
            let pruned = registry.prune(now, RETENTION);
            (staled, pruned)
        };

        for key in staled_keys {
            // Re-read under the lock right before persisting: between
            // mark_stale and here a concurrent `ingest` may have superseded
            // this session with a newer live snapshot (or `prune` may have
            // dropped it). Only act while the registry still holds our Stale
            // value for the key, so we never clobber the DB/UI with a snapshot
            // older than the registry currently holds (BUGHUNT #6).
            let snapshot = {
                let registry = self.inner.registry.lock().unwrap();
                match registry.get(&key) {
                    Some(s) if s.status == twin_core::AgentStatus::Stale => s.clone(),
                    _ => continue,
                }
            };
            if let Some(store) = &self.inner.store {
                let store = store.lock().unwrap();
                let _ = store.save_session(&snapshot);
                let _ = store.record_transition(
                    &key,
                    &now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    None,
                    &status_str(snapshot.status),
                );
            }
            let _ = self.inner.tx.send(Event::Upsert {
                key,
                logical_key: dedup::logical_key(&snapshot),
                session: snapshot,
                entered_needs_you: false,
            });
        }
        for key in pruned {
            if let Some(store) = &self.inner.store {
                let _ = store.lock().unwrap().delete_session(&key);
            }
            let _ = self.inner.tx.send(Event::Removed { key });
        }
    }

    /// Current sessions, unordered, one entry per raw `(machine, source,
    /// agent_id)` observation.
    pub fn sessions(&self) -> Vec<AgentSnapshot> {
        self.inner
            .registry
            .lock()
            .unwrap()
            .sessions()
            .cloned()
            .collect()
    }

    /// Current sessions with cross-side duplicates collapsed (TWI-10) —
    /// what UI clients should render.
    pub fn logical_sessions(&self) -> Vec<AgentSnapshot> {
        dedup::dedupe(self.sessions())
    }

    /// Record a machine's plan usage; broadcasts only when it changed
    /// (ignoring the report timestamp, which always moves).
    pub fn report_usage(&self, report: UsageReport) {
        let changed = {
            let mut usage = self.inner.usage.lock().unwrap();
            let same = usage.get(&report.machine).is_some_and(|prev| {
                prev.claude == report.claude
                    && prev.claude_exact == report.claude_exact
                    && prev.codex == report.codex
            });
            usage.insert(report.machine.clone(), report.clone());
            !same
        };
        if changed {
            let _ = self.inner.tx.send(Event::Usage { report });
        }
    }

    /// Latest plan usage per machine.
    pub fn usage_reports(&self) -> Vec<UsageReport> {
        self.inner.usage.lock().unwrap().values().cloned().collect()
    }

    /// Record a machine's historical-usage aggregation (stats pane). No
    /// broadcast — the pane fetches on open.
    pub fn report_stats(&self, stats: twin_core::UsageStats) {
        self.inner
            .stats
            .lock()
            .unwrap()
            .insert(stats.machine.clone(), stats);
    }

    /// Latest historical-usage aggregation per machine.
    pub fn usage_stats(&self) -> Vec<twin_core::UsageStats> {
        self.inner.stats.lock().unwrap().values().cloned().collect()
    }

    /// Subscribe to diffs. Slow readers that lag more than the channel
    /// capacity miss events and should refetch the full state.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.tx.subscribe()
    }
}

fn status_str(status: twin_core::AgentStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use twin_core::{AgentSource, AgentStatus, UsageMetrics};

    fn snapshot(id: &str, status: AgentStatus) -> AgentSnapshot {
        AgentSnapshot {
            agent_id: id.into(),
            source: AgentSource::ClaudeCode,
            machine: "wsl".into(),
            project: "demo".into(),
            status,
            git_branch: None,
            current_task: None,
            needs_user: matches!(status, AgentStatus::NeedsYou),
            needs_user_reason: None,
            usage: UsageMetrics::default(),
            last_activity: "2026-07-10T09:00:00Z".into(),
            jump: None,
        }
    }

    #[test]
    fn ingest_broadcasts_diffs_and_persists_transitions() {
        let service = HubService::new(Some(Store::open_in_memory().unwrap()));
        let mut rx = service.subscribe();

        service.ingest(snapshot("a", AgentStatus::Thinking));
        service.ingest(snapshot("a", AgentStatus::NeedsYou));
        // Unchanged snapshot: no event.
        service.ingest(snapshot("a", AgentStatus::NeedsYou));

        let Event::Upsert {
            entered_needs_you: false,
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("expected initial upsert")
        };
        let Event::Upsert {
            entered_needs_you: true,
            key,
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("expected needs-you edge")
        };
        assert!(rx.try_recv().is_err(), "unchanged must not broadcast");
        assert_eq!(key, "wsl/claude-code/a");
    }

    #[test]
    fn usage_reports_broadcast_only_on_change() {
        let service = HubService::new(None);
        let mut rx = service.subscribe();
        let report_with = |pct: f64, at: &str| UsageReport {
            machine: "wsl".into(),
            claude: None,
            claude_exact: None,
            codex: Some(twin_core::codex::RateLimits {
                primary: Some(twin_core::codex::RateLimitWindow {
                    used_percent: pct,
                    window_minutes: Some(300),
                    resets_at: None,
                }),
                secondary: None,
                plan_type: Some("plus".into()),
                observed_at: None,
            }),
            reported_at: at.into(),
        };

        service.report_usage(report_with(13.0, "2026-07-10T09:00:00Z"));
        // Same numbers, newer timestamp: no rebroadcast.
        service.report_usage(report_with(13.0, "2026-07-10T09:00:30Z"));
        service.report_usage(report_with(14.0, "2026-07-10T09:01:00Z"));

        assert!(matches!(rx.try_recv().unwrap(), Event::Usage { .. }));
        let Event::Usage { report } = rx.try_recv().unwrap() else {
            panic!("expected the changed report");
        };
        assert_eq!(report.codex.unwrap().primary.unwrap().used_percent, 14.0);
        assert!(rx.try_recv().is_err());
        assert_eq!(service.usage_reports().len(), 1);

        // A change only in the exact Claude reading must broadcast too.
        let mut with_exact = report_with(14.0, "2026-07-10T09:02:00Z");
        with_exact.claude_exact = Some(twin_core::ClaudePlanWindows {
            five_hour: twin_core::PlanWindow {
                used_percent: 24.0,
                resets_at: None,
            },
            seven_day: twin_core::PlanWindow {
                used_percent: 11.0,
                resets_at: None,
            },
            seven_day_opus: None,
            seven_day_sonnet: None,
            observed_at: chrono::Utc::now(),
        });
        service.report_usage(with_exact.clone());
        let Event::Usage { report } = rx.try_recv().unwrap() else {
            panic!("expected the claude_exact change to broadcast");
        };
        assert_eq!(report.claude_exact.unwrap().five_hour.used_percent, 24.0);
        // Identical exact reading: silent.
        with_exact.reported_at = "2026-07-10T09:02:30Z".into();
        service.report_usage(with_exact);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn warm_start_reloads_persisted_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        {
            let service = HubService::new(Some(Store::open(&path).unwrap()));
            service.ingest(snapshot("a", AgentStatus::Done));
        }
        let service = HubService::new(Some(Store::open(&path).unwrap()));
        assert_eq!(service.sessions().len(), 1);
    }
}
