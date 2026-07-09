//! Shared types, JSONL parsers, and the session state machine.
//!
//! Everything that must behave identically in the WSL collector, the VPS
//! collector, and the Windows app core lives here.

pub mod claude_code;
pub mod model;
pub mod tail;
pub mod watch;

pub use claude_code::{ClaudeSessionTracker, SessionState};
pub use model::*;
pub use tail::TailReader;
pub use watch::{DirWatcher, FileEvent, FileEventKind};
