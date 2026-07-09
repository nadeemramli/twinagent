//! The adapter contract: the one JSON shape every agent source normalizes to.
//! See the PRD ("Other Agents" section) — collectors produce these snapshots,
//! the hub stores and diffs them, the widget renders them.

use serde::{Deserialize, Serialize};

/// Where an agent's activity was observed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentSource {
    ClaudeCode,
    Codex,
    Gemini,
    Vps,
    Script,
    Cron,
}

/// Session lifecycle state, derived by the per-source state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Thinking,
    ToolRunning,
    NeedsYou,
    Done,
    Failed,
    Idle,
    /// Heartbeat missed (remote agents) or transcript went silent for too long.
    Stale,
}

impl std::fmt::Display for AgentSource {
    /// Kebab-case, matching the serde representation — used in registry keys
    /// and SQLite rows.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            AgentSource::ClaudeCode => "claude-code",
            AgentSource::Codex => "codex",
            AgentSource::Gemini => "gemini",
            AgentSource::Vps => "vps",
            AgentSource::Script => "script",
            AgentSource::Cron => "cron",
        };
        f.write_str(s)
    }
}

/// Session lifecycle as derived from a transcript alone. Richer than
/// [`AgentStatus`] (interrupted is its own thing here) so the widget can
/// distinguish "you stopped it" from "it wants permission". Shared by every
/// transcript-based parser (Claude Code, Codex).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Thinking,
    ToolRunning,
    NeedsYou,
    Interrupted,
    Done,
    Idle,
    Stale,
}

impl SessionState {
    pub fn to_status(self) -> AgentStatus {
        match self {
            SessionState::Thinking => AgentStatus::Thinking,
            SessionState::ToolRunning => AgentStatus::ToolRunning,
            SessionState::NeedsYou | SessionState::Interrupted => AgentStatus::NeedsYou,
            SessionState::Done => AgentStatus::Done,
            SessionState::Idle => AgentStatus::Idle,
            SessionState::Stale => AgentStatus::Stale,
        }
    }
}

/// Token/context accounting for one session.
///
/// `context_pct` drives the gauge (green <50, yellow 50-70, orange 70-90,
/// red >90 — AgentNotch thresholds).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageMetrics {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub context_pct: Option<f32>,
}

/// How confident we are in a usage reading, per the UsageSource ladder:
/// local files -> reused OAuth -> provider API -> cookies/manual.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadingConfidence {
    Exact,
    Estimated,
    Stale,
}

/// Deep link back into the right work surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "target", rename_all = "snake_case")]
pub enum JumpTarget {
    /// `vscode://` or `vscode-remote://wsl+...` folder URI.
    VsCode(String),
    /// Directory for `wt.exe -d <dir>`.
    Terminal(String),
    Url(String),
    /// `user@host` for VPS agents.
    Ssh(String),
}

/// One normalized observation of an agent session — the unit collectors send
/// to the hub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSnapshot {
    /// Stable per-session id (Claude Code session UUID, Codex session id, ...).
    pub agent_id: String,
    pub source: AgentSource,
    /// Machine tag: "windows", "wsl", or a VPS hostname.
    pub machine: String,
    /// Project path or human name.
    pub project: String,
    pub status: AgentStatus,
    /// Git branch of the session's working tree, when the transcript
    /// records it (drives the card's branch badge).
    #[serde(default)]
    pub git_branch: Option<String>,
    pub current_task: Option<String>,
    pub needs_user: bool,
    pub needs_user_reason: Option<String>,
    pub usage: UsageMetrics,
    /// RFC 3339 timestamp of the last observed activity.
    pub last_activity: String,
    pub jump: Option<JumpTarget>,
}

/// Machine-level plan usage, one per collector: Codex numbers are exact
/// (straight from `rate_limits` on disk), Claude numbers are JSONL-derived
/// estimates. Each side carries its own [`ReadingConfidence`] — the widget
/// must never present an estimate as exact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    /// Machine tag, same vocabulary as [`AgentSnapshot::machine`].
    pub machine: String,
    pub claude: Option<crate::plan_usage::PlanEstimate>,
    pub codex: Option<crate::codex::RateLimits>,
    /// RFC 3339 timestamp of when the report was assembled.
    pub reported_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_through_json() {
        let snapshot = AgentSnapshot {
            agent_id: "3c1f6f0e-test".into(),
            source: AgentSource::ClaudeCode,
            machine: "wsl".into(),
            project: "~/workspace/github.com/nadeemramli/twinagent".into(),
            status: AgentStatus::ToolRunning,
            git_branch: None,
            current_task: Some("cargo build".into()),
            needs_user: false,
            needs_user_reason: None,
            usage: UsageMetrics {
                input_tokens: 1200,
                output_tokens: 450,
                cache_read_tokens: 9000,
                cache_creation_tokens: 300,
                context_pct: Some(12.5),
            },
            last_activity: "2026-07-10T09:00:00Z".into(),
            jump: Some(JumpTarget::Terminal("/home/nadeemramli/workspace".into())),
        };

        let json = serde_json::to_string(&snapshot).unwrap();
        let back: AgentSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent_id, snapshot.agent_id);
        assert_eq!(back.status, AgentStatus::ToolRunning);
        assert_eq!(back.usage, snapshot.usage);
        assert_eq!(back.jump, snapshot.jump);
    }

    #[test]
    fn status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&AgentStatus::NeedsYou).unwrap(),
            "\"needs_you\""
        );
    }
}
