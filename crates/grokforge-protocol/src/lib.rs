//! `grokforge-protocol` — the shared vocabulary spoken between the GrokForge agent
//! core and its frontends (TUI, headless, future ACP adapter).
//!
//! This is a leaf crate: serde types only, no async, no I/O. Everything here is part of
//! a compatibility surface and must be treated as **append-only after a release**.
//!
//! Real `Op`/`Event`/approval/sandbox types land in M2. This M0 stub establishes the
//! crate and its id newtypes.

pub mod ids;

pub use ids::{ApprovalId, SessionId, SubId, ToolCallId, TurnId};
