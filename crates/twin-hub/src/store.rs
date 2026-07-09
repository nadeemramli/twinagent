//! SQLite persistence: current sessions survive restarts, status
//! transitions accumulate as queryable history.
//!
//! Everything stays on this machine — the DB lives in the app's local data
//! dir (or next to the standalone binary). Private by construction.

use std::path::Path;

use rusqlite::{params, Connection};
use twin_core::AgentSnapshot;

use crate::registry::session_key;

/// Wraps one SQLite connection. The hub serializes access behind a mutex,
/// which is plenty for a handful of collectors on localhost.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) the hub database at `path`.
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// In-memory store, for tests and ephemeral runs.
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sessions (
                key           TEXT PRIMARY KEY,
                machine       TEXT NOT NULL,
                source        TEXT NOT NULL,
                agent_id      TEXT NOT NULL,
                project       TEXT NOT NULL,
                status        TEXT NOT NULL,
                needs_user    INTEGER NOT NULL,
                last_activity TEXT NOT NULL,
                snapshot      TEXT NOT NULL,
                updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
            );
            CREATE TABLE IF NOT EXISTS transitions (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                key         TEXT NOT NULL,
                at          TEXT NOT NULL,
                from_status TEXT,
                to_status   TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS transitions_key_at ON transitions (key, at);
            ",
        )?;
        Ok(Self { conn })
    }

    /// Write the current state of a session (insert or replace).
    pub fn save_session(&self, snapshot: &AgentSnapshot) -> rusqlite::Result<()> {
        let status = serde_json::to_value(snapshot.status)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        self.conn.execute(
            "INSERT INTO sessions
                 (key, machine, source, agent_id, project, status, needs_user,
                  last_activity, snapshot, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                     strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(key) DO UPDATE SET
                 project = excluded.project,
                 status = excluded.status,
                 needs_user = excluded.needs_user,
                 last_activity = excluded.last_activity,
                 snapshot = excluded.snapshot,
                 updated_at = excluded.updated_at",
            params![
                session_key(snapshot),
                snapshot.machine,
                snapshot.source.to_string(),
                snapshot.agent_id,
                snapshot.project,
                status,
                snapshot.needs_user as i64,
                snapshot.last_activity,
                serde_json::to_string(snapshot).expect("snapshot serializes"),
            ],
        )?;
        Ok(())
    }

    /// Append one status transition to the history table.
    pub fn record_transition(
        &self,
        key: &str,
        at: &str,
        from_status: Option<&str>,
        to_status: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO transitions (key, at, from_status, to_status)
             VALUES (?1, ?2, ?3, ?4)",
            params![key, at, from_status, to_status],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, key: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM sessions WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// Load every persisted session — the registry's warm-start state.
    pub fn load_sessions(&self) -> rusqlite::Result<Vec<AgentSnapshot>> {
        let mut stmt = self.conn.prepare("SELECT snapshot FROM sessions")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut sessions = Vec::new();
        for row in rows {
            // A row written by a newer/older schema that no longer parses is
            // skipped, not fatal — history is best-effort.
            if let Ok(snapshot) = serde_json::from_str(&row?) {
                sessions.push(snapshot);
            }
        }
        Ok(sessions)
    }

    /// Status history for one session, oldest first: `(at, from, to)`.
    pub fn transitions(
        &self,
        key: &str,
    ) -> rusqlite::Result<Vec<(String, Option<String>, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT at, from_status, to_status FROM transitions
             WHERE key = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![key], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twin_core::{AgentSource, AgentStatus, UsageMetrics};

    fn snapshot(id: &str, status: AgentStatus) -> AgentSnapshot {
        AgentSnapshot {
            agent_id: id.into(),
            source: AgentSource::Codex,
            machine: "windows".into(),
            project: "C:\\proj".into(),
            status,
            git_branch: None,
            current_task: Some("shell_command".into()),
            needs_user: false,
            needs_user_reason: None,
            usage: UsageMetrics {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 100,
                cache_creation_tokens: 0,
                context_pct: Some(12.0),
            },
            last_activity: "2026-07-10T09:00:00Z".into(),
            jump: None,
        }
    }

    #[test]
    fn sessions_round_trip_through_sqlite() {
        let store = Store::open_in_memory().unwrap();
        let snap = snapshot("s1", AgentStatus::ToolRunning);
        store.save_session(&snap).unwrap();
        // Update in place: still one row, new status.
        store
            .save_session(&snapshot("s1", AgentStatus::Done))
            .unwrap();
        store
            .save_session(&snapshot("s2", AgentStatus::Idle))
            .unwrap();

        let loaded = store.load_sessions().unwrap();
        assert_eq!(loaded.len(), 2);
        let s1 = loaded.iter().find(|s| s.agent_id == "s1").unwrap();
        assert_eq!(s1.status, AgentStatus::Done);
        assert_eq!(s1.usage.cache_read_tokens, 100);
    }

    #[test]
    fn transitions_accumulate_in_order() {
        let store = Store::open_in_memory().unwrap();
        store
            .record_transition("k", "2026-07-10T09:00:00Z", None, "thinking")
            .unwrap();
        store
            .record_transition(
                "k",
                "2026-07-10T09:00:05Z",
                Some("thinking"),
                "tool_running",
            )
            .unwrap();
        let history = store.transitions("k").unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].2, "thinking");
        assert_eq!(history[1].1.as_deref(), Some("thinking"));
    }

    #[test]
    fn delete_removes_the_row() {
        let store = Store::open_in_memory().unwrap();
        let snap = snapshot("s1", AgentStatus::Idle);
        store.save_session(&snap).unwrap();
        store.delete_session(&session_key(&snap)).unwrap();
        assert!(store.load_sessions().unwrap().is_empty());
    }

    #[test]
    fn survives_restart_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .save_session(&snapshot("s1", AgentStatus::NeedsYou))
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.load_sessions().unwrap().len(), 1);
    }
}
