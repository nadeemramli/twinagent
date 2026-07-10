//! Shared types, JSONL parsers, and the session state machine.
//!
//! Everything that must behave identically in the WSL collector, the VPS
//! collector, and the Windows app core lives here.

pub mod claude_code;
pub mod claude_plan_api;
pub mod codex;
pub mod model;
pub mod pipeline;
pub mod plan_usage;
pub mod tail;
pub mod watch;

pub use claude_code::ClaudeSessionTracker;
pub use claude_plan_api::{ClaudePlanPoller, ClaudePlanWindows, PlanWindow};
pub use codex::CodexSessionTracker;
pub use model::*;
pub use pipeline::{Pipeline, SourceRoot};
pub use plan_usage::{PlanEstimate, TokenCounts, WindowUsage};
pub use tail::TailReader;
pub use watch::{DirWatcher, FileEvent, FileEventKind};
