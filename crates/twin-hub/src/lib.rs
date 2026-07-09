//! Session registry, persistence, and push to UI clients.
//!
//! Built as a library so the Tauri app embeds it directly (local-first
//! default); the thin `twin-hub` binary target serves the Phase 3 standalone
//! VPS deployment. SQLite + WebSocket arrive with TWI-7.

use std::collections::HashMap;

use twin_core::AgentSnapshot;

/// In-memory session registry, keyed by `(machine, source, agent_id)`.
#[derive(Debug, Default)]
pub struct Hub {
    sessions: HashMap<String, AgentSnapshot>,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(snapshot: &AgentSnapshot) -> String {
        format!(
            "{}/{:?}/{}",
            snapshot.machine, snapshot.source, snapshot.agent_id
        )
    }

    /// Insert or update a session; returns true if this was a new session.
    pub fn upsert(&mut self, snapshot: AgentSnapshot) -> bool {
        self.sessions
            .insert(Self::key(&snapshot), snapshot)
            .is_none()
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
    use twin_core::{AgentSource, AgentStatus, UsageMetrics};

    fn snapshot(id: &str) -> AgentSnapshot {
        AgentSnapshot {
            agent_id: id.into(),
            source: AgentSource::ClaudeCode,
            machine: "wsl".into(),
            project: "demo".into(),
            status: AgentStatus::Idle,
            current_task: None,
            needs_user: false,
            needs_user_reason: None,
            usage: UsageMetrics::default(),
            last_activity: "2026-07-10T09:00:00Z".into(),
            jump: None,
        }
    }

    #[test]
    fn upsert_deduplicates_by_key() {
        let mut hub = Hub::new();
        assert!(hub.upsert(snapshot("a")));
        assert!(!hub.upsert(snapshot("a")));
        assert!(hub.upsert(snapshot("b")));
        assert_eq!(hub.len(), 2);
    }
}
