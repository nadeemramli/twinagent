//! Shared types, JSONL parsers, and the session state machine.
//!
//! Everything that must behave identically in the WSL collector, the VPS
//! collector, and the Windows app core lives here.

pub mod model;
pub mod tail;
pub mod watch;

pub use model::*;
pub use tail::TailReader;
pub use watch::{DirWatcher, FileEvent, FileEventKind};
