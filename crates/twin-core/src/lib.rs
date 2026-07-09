//! Shared types, JSONL parsers, and the session state machine.
//!
//! Everything that must behave identically in the WSL collector, the VPS
//! collector, and the Windows app core lives here.

pub mod model;

pub use model::*;
