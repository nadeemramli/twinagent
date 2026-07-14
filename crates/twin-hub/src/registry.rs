//! In-memory session registry with change detection.
//!
//! The registry is the hub's source of truth for "what is every agent doing
//! right now". Collectors upsert [`AgentSnapshot`]s; the registry decides
//! whether anything meaningful changed, so downstream (WebSocket clients,
//! SQLite history, toasts) only hears about real transitions.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use twin_core::{AgentSnapshot, AgentStatus};

/// How a snapshot changed the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delta {
    /// First time this `(machine, source, agent_id)` was seen.
    New,
    /// Session existed and something the UI shows changed.
    Updated {
        /// Status moved (drives the history table).
        status_changed: bool,
        /// Session newly wants attention (drives toasts, TWI-14).
        entered_needs_you: bool,
    },
    /// Nothing the UI shows changed — callers should not broadcast.
    Unchanged,
}

/// Stable registry key: `machine/source/agent_id`.
pub fn session_key(snapshot: &AgentSnapshot) -> String {
    format!(
        "{}/{}/{}",
        snapshot.machine, snapshot.source, snapshot.agent_id
    )
}

/// In-memory session registry, keyed by `(machine, source, agent_id)`.
#[derive(Debug, Default)]
pub struct Hub {
    sessions: HashMap<String, AgentSnapshot>,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or update a session, reporting what changed.
    ///
    /// Transition rules:
    /// - identical snapshots (modulo `last_activity` bumps that change
    ///   nothing visible) are `Unchanged`;
    /// - a session that was terminal (`Done`/`Stale`) coming back with a
    ///   live status is just a normal update — sessions resume;
    /// - `entered_needs_you` fires only on the false→true edge of
    ///   `needs_user`, so a flapping session cannot spam alerts.
    pub fn upsert(&mut self, snapshot: AgentSnapshot) -> Delta {
        let key = session_key(&snapshot);
        match self.sessions.get(&key) {
            None => {
                self.sessions.insert(key, snapshot);
                Delta::New
            }
            Some(prev) => {
                let status_changed = prev.status != snapshot.status;
                let entered_needs_you = !prev.needs_user && snapshot.needs_user;
                let visible_change = status_changed
                    || entered_needs_you
                    || prev.needs_user != snapshot.needs_user
                    || prev.current_task != snapshot.current_task
                    || prev.usage != snapshot.usage
                    || prev.project != snapshot.project
                    || prev.git_branch != snapshot.git_branch
                    || prev.needs_user_reason != snapshot.needs_user_reason
                    || prev.jump != snapshot.jump
                    || prev.last_activity != snapshot.last_activity;
                if !visible_change {
                    return Delta::Unchanged;
                }
                self.sessions.insert(key, snapshot);
                Delta::Updated {
                    status_changed,
                    entered_needs_you,
                }
            }
        }
    }

    /// Mark sessions silent for longer than `threshold` as [`AgentStatus::Stale`].
    /// Returns the keys that transitioned, for broadcasting.
    ///
    /// States that wait on the user (done, needs-you, failed) are exempt:
    /// "finished an hour ago" is still done, not stale. Stale is for
    /// sessions that went quiet mid-run.
    pub fn mark_stale(&mut self, now: DateTime<Utc>, threshold: Duration) -> Vec<String> {
        let mut changed = Vec::new();
        for (key, s) in &mut self.sessions {
            if matches!(
                s.status,
                AgentStatus::Stale
                    | AgentStatus::Done
                    | AgentStatus::NeedsYou
                    | AgentStatus::Failed
            ) {
                continue;
            }
            // An unparseable timestamp is treated as maximally old: better to
            // stale (and later prune) a corrupt row than leak a card forever
            // (BUGHUNT #8).
            let stale_reached = match DateTime::parse_from_rfc3339(&s.last_activity) {
                Ok(last) => now - last.with_timezone(&Utc) >= threshold,
                Err(_) => true,
            };
            if stale_reached {
                s.status = AgentStatus::Stale;
                changed.push(key.clone());
            }
        }
        changed
    }

    /// Drop sessions silent for longer than `retention` (a done session from
    /// yesterday should not haunt the widget). Returns the removed keys.
    pub fn prune(&mut self, now: DateTime<Utc>, retention: Duration) -> Vec<String> {
        let expired: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| {
                // Unparseable timestamp → maximally old → prunable, so a
                // corrupt row can never haunt the widget forever (BUGHUNT #8).
                DateTime::parse_from_rfc3339(&s.last_activity)
                    .map(|last| now - last.with_timezone(&Utc) >= retention)
                    .unwrap_or(true)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in &expired {
            self.sessions.remove(key);
        }
        expired
    }

    pub fn get(&self, key: &str) -> Option<&AgentSnapshot> {
        self.sessions.get(key)
    }

    pub fn sessions(&self) -> impl Iterator<Item = &AgentSnapshot> {
        self.sessions.values()
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twin_core::{AgentSource, UsageMetrics};

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
    fn upsert_reports_new_then_unchanged() {
        let mut hub = Hub::new();
        assert_eq!(hub.upsert(snapshot("a", AgentStatus::Idle)), Delta::New);
        assert_eq!(
            hub.upsert(snapshot("a", AgentStatus::Idle)),
            Delta::Unchanged
        );
        assert_eq!(hub.len(), 1);
    }

    #[test]
    fn status_change_and_needs_you_edge_are_flagged() {
        let mut hub = Hub::new();
        hub.upsert(snapshot("a", AgentStatus::ToolRunning));
        assert_eq!(
            hub.upsert(snapshot("a", AgentStatus::NeedsYou)),
            Delta::Updated {
                status_changed: true,
                entered_needs_you: true,
            }
        );
        // Still needs-you: no second alert edge.
        let mut again = snapshot("a", AgentStatus::NeedsYou);
        again.current_task = Some("waiting".into());
        assert_eq!(
            hub.upsert(again),
            Delta::Updated {
                status_changed: false,
                entered_needs_you: false,
            }
        );
    }

    #[test]
    fn same_id_on_both_machines_is_two_sessions() {
        let mut hub = Hub::new();
        hub.upsert(snapshot("a", AgentStatus::Idle));
        let mut windows = snapshot("a", AgentStatus::Idle);
        windows.machine = "windows".into();
        assert_eq!(hub.upsert(windows), Delta::New);
        assert_eq!(hub.len(), 2);
    }

    #[test]
    fn unparseable_last_activity_is_staled_and_pruned() {
        let mut hub = Hub::new();
        let mut bad = snapshot("bad", AgentStatus::Thinking);
        bad.last_activity = "not-a-timestamp".into();
        hub.upsert(bad);
        let now = DateTime::parse_from_rfc3339("2026-07-10T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        // A corrupt timestamp counts as maximally old: staled on the sweep...
        let staled = hub.mark_stale(now, Duration::minutes(30));
        assert_eq!(staled.len(), 1);
        assert_eq!(
            hub.get("wsl/claude-code/bad").unwrap().status,
            AgentStatus::Stale
        );
        // ...and pruned rather than leaked forever.
        assert_eq!(hub.prune(now, Duration::hours(24)).len(), 1);
        assert!(hub.is_empty());
    }

    #[test]
    fn visible_change_covers_branch_reason_and_jump() {
        let mut hub = Hub::new();
        hub.upsert(snapshot("a", AgentStatus::ToolRunning));
        // A branch switch alone must broadcast (BUGHUNT #7).
        let mut branch = snapshot("a", AgentStatus::ToolRunning);
        branch.git_branch = Some("feature".into());
        assert!(matches!(hub.upsert(branch), Delta::Updated { .. }));
        // A newly-populated jump target alone must broadcast.
        let mut jump = snapshot("a", AgentStatus::ToolRunning);
        jump.git_branch = Some("feature".into());
        jump.jump = Some(twin_core::JumpTarget::VsCode("/proj".into()));
        assert!(matches!(hub.upsert(jump), Delta::Updated { .. }));
    }

    #[test]
    fn stale_marking_and_pruning() {
        let mut hub = Hub::new();
        // Mid-run session goes stale; a done one waits for the user forever.
        hub.upsert(snapshot("a", AgentStatus::Thinking));
        hub.upsert(snapshot("b", AgentStatus::Done));
        let now = DateTime::parse_from_rfc3339("2026-07-10T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let staled = hub.mark_stale(now, Duration::minutes(30));
        assert_eq!(staled.len(), 1);
        assert_eq!(
            hub.get(&staled[0]).unwrap().status,
            AgentStatus::Stale
        );
        assert_eq!(
            hub.get("wsl/claude-code/b").unwrap().status,
            AgentStatus::Done,
            "done never decays to stale"
        );
        // Second sweep is quiet.
        assert!(hub.mark_stale(now, Duration::minutes(30)).is_empty());

        assert_eq!(hub.prune(now, Duration::hours(24)).len(), 0);
        let tomorrow = now + Duration::hours(25);
        assert_eq!(hub.prune(tomorrow, Duration::hours(24)).len(), 2);
        assert!(hub.is_empty());
    }
}
