//! Cross-side session dedup: one logical session, one card.
//!
//! Agent CLIs run on BOTH sides of this machine (WSL and Windows dotdirs are
//! both active), and the same session can occasionally be observed twice —
//! e.g. both sides end up watching the same directory through a mount, or a
//! project is opened via its WSL path on one side and its `/mnt/c` (or
//! `\\wsl$`) alias on the other.
//!
//! Merge rules, in order:
//! 1. **Session UUID**: same `(source, agent_id)` seen from two machines is
//!    the same underlying transcript — keep the freshest observation.
//! 2. **Project + time overlap (fallback)**: same source, same *normalized*
//!    project path, different machines, both active within a tight window,
//!    and the pairing is unambiguous (exactly one candidate per side). Two
//!    genuinely different sessions in the same repo remain two cards —
//!    that's the normal both-sides workflow, not a duplicate.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use twin_core::AgentSnapshot;

/// Two same-project observations closer than this are merge candidates.
pub const OVERLAP_WINDOW: Duration = Duration::seconds(90);

/// Identity of a logical session, machine-agnostic. UI clients key their
/// cards by this.
pub fn logical_key(snapshot: &AgentSnapshot) -> String {
    format!("{}/{}", snapshot.source, snapshot.agent_id)
}

/// Normalize a project path so its WSL and Windows spellings compare equal:
/// lowercased, forward slashes, `/mnt/c/...` → `c:/...`,
/// `//wsl$/<distro>/...` and `//wsl.localhost/<distro>/...` → `/...`.
pub fn normalize_project(path: &str) -> String {
    let mut p = path.replace('\\', "/").to_lowercase();
    while p.ends_with('/') && p.len() > 1 {
        p.pop();
    }
    // \\wsl$\Ubuntu\home\x and \\wsl.localhost\Ubuntu\home\x → /home/x
    for prefix in ["//wsl$/", "//wsl.localhost/"] {
        if let Some(rest) = p.strip_prefix(prefix) {
            if let Some(slash) = rest.find('/') {
                return rest[slash..].to_string();
            }
        }
    }
    // /mnt/c/users/x → c:/users/x
    if let Some(rest) = p.strip_prefix("/mnt/") {
        let mut chars = rest.chars();
        if let (Some(drive), Some('/')) = (chars.next(), chars.clone().next()) {
            if drive.is_ascii_alphabetic() {
                return format!("{drive}:{}", chars.as_str());
            }
        }
    }
    p
}

fn activity(snapshot: &AgentSnapshot) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&snapshot.last_activity)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Keep the fresher observation; attention is never lost in a merge.
fn merge(a: AgentSnapshot, b: AgentSnapshot) -> AgentSnapshot {
    let (ta, tb) = (activity(&a), activity(&b));
    let (mut keep, other) = if tb > ta { (b, a) } else { (a, b) };
    if !keep.needs_user && other.needs_user {
        keep.needs_user = true;
        keep.needs_user_reason = other.needs_user_reason;
    }
    keep
}

/// Collapse duplicate observations of the same logical session.
pub fn dedupe(sessions: Vec<AgentSnapshot>) -> Vec<AgentSnapshot> {
    // Rule 1: same (source, agent_id) — keep the freshest.
    let mut by_uuid: HashMap<String, AgentSnapshot> = HashMap::new();
    for s in sessions {
        let merged = match by_uuid.remove(&logical_key(&s)) {
            None => s,
            Some(prev) => merge(prev, s),
        };
        by_uuid.insert(logical_key(&merged), merged);
    }

    // Rule 2 (fallback): same source + normalized project from two machines,
    // active within the overlap window, unambiguous pairing.
    let mut by_project: HashMap<(String, String), Vec<AgentSnapshot>> = HashMap::new();
    for s in by_uuid.into_values() {
        by_project
            .entry((s.source.to_string(), normalize_project(&s.project)))
            .or_default()
            .push(s);
    }

    let mut out = Vec::new();
    for (_, mut group) in by_project {
        let mergeable = group.len() == 2
            && group[0].machine != group[1].machine
            && match (activity(&group[0]), activity(&group[1])) {
                (Some(a), Some(b)) => (a - b).abs() <= OVERLAP_WINDOW,
                _ => false,
            };
        if mergeable {
            let b = group.pop().unwrap();
            let a = group.pop().unwrap();
            out.push(merge(a, b));
        } else {
            out.append(&mut group);
        }
    }
    out.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use twin_core::{AgentSource, AgentStatus, UsageMetrics};

    fn snapshot(id: &str, machine: &str, project: &str, at: &str) -> AgentSnapshot {
        AgentSnapshot {
            agent_id: id.into(),
            source: AgentSource::ClaudeCode,
            machine: machine.into(),
            project: project.into(),
            status: AgentStatus::Thinking,
            current_task: None,
            needs_user: false,
            needs_user_reason: None,
            usage: UsageMetrics::default(),
            last_activity: at.into(),
            jump: None,
        }
    }

    #[test]
    fn normalizes_wsl_and_windows_spellings() {
        assert_eq!(
            normalize_project("/mnt/c/Users/Nadeem/proj"),
            "c:/users/nadeem/proj"
        );
        assert_eq!(
            normalize_project("C:\\Users\\Nadeem\\proj\\"),
            "c:/users/nadeem/proj"
        );
        assert_eq!(
            normalize_project("\\\\wsl$\\Ubuntu\\home\\nadeemramli\\ws"),
            "/home/nadeemramli/ws"
        );
        assert_eq!(
            normalize_project("\\\\wsl.localhost\\Ubuntu\\home\\nadeemramli\\ws"),
            "/home/nadeemramli/ws"
        );
        assert_eq!(normalize_project("/home/nadeemramli/ws"), "/home/nadeemramli/ws");
    }

    #[test]
    fn same_uuid_across_machines_keeps_freshest() {
        let old = snapshot("u1", "wsl", "/home/n/p", "2026-07-10T09:00:00Z");
        let mut new = snapshot("u1", "windows", "C:\\p", "2026-07-10T09:05:00Z");
        new.needs_user = true;
        let out = dedupe(vec![old, new]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].machine, "windows");
        assert!(out[0].needs_user);
    }

    #[test]
    fn same_project_alias_with_overlap_merges() {
        let wsl = snapshot(
            "u1",
            "wsl",
            "/mnt/c/Users/Nadeem/proj",
            "2026-07-10T09:00:00Z",
        );
        let win = snapshot(
            "u2",
            "windows",
            "C:\\Users\\Nadeem\\proj",
            "2026-07-10T09:00:30Z",
        );
        let out = dedupe(vec![wsl, win]);
        assert_eq!(out.len(), 1, "aliased project within window must merge");
        assert_eq!(out[0].agent_id, "u2");
    }

    #[test]
    fn same_project_but_stale_gap_stays_separate() {
        let wsl = snapshot(
            "u1",
            "wsl",
            "/mnt/c/Users/Nadeem/proj",
            "2026-07-10T08:00:00Z",
        );
        let win = snapshot(
            "u2",
            "windows",
            "C:\\Users\\Nadeem\\proj",
            "2026-07-10T09:00:30Z",
        );
        assert_eq!(dedupe(vec![wsl, win]).len(), 2);
    }

    #[test]
    fn two_sessions_same_machine_same_project_stay_separate() {
        // The normal workflow: two CLI sessions in one repo are two cards.
        let a = snapshot("u1", "wsl", "/home/n/p", "2026-07-10T09:00:00Z");
        let b = snapshot("u2", "wsl", "/home/n/p", "2026-07-10T09:00:10Z");
        assert_eq!(dedupe(vec![a, b]).len(), 2);
    }

    #[test]
    fn ambiguous_triple_does_not_merge() {
        let a = snapshot("u1", "wsl", "/mnt/c/u/p", "2026-07-10T09:00:00Z");
        let b = snapshot("u2", "windows", "C:\\u\\p", "2026-07-10T09:00:10Z");
        let c = snapshot("u3", "wsl", "/mnt/c/u/p", "2026-07-10T09:00:20Z");
        assert_eq!(dedupe(vec![a, b, c]).len(), 3);
    }
}
