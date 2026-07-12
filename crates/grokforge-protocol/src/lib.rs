//! `grokforge-protocol` — the shared vocabulary spoken between the GrokForge agent core and
//! its frontends (TUI, headless, a future ACP adapter).
//!
//! This is a leaf crate: serde types only, no async, no I/O. Everything here is part of a
//! compatibility surface and must be treated as **append-only after a release** (ADR 0005).

pub mod approval;
pub mod event;
pub mod ids;
pub mod items;
pub mod ledger;
pub mod op;
pub mod sandbox;
pub mod usage;

pub use approval::{ApprovalKind, ApprovalPolicy, ApprovalRequest, Decision};
pub use event::EventMsg;
pub use ids::{ApprovalId, SessionId, SubId, ToolCallId, TurnId};
pub use items::ResponseItem;
pub use ledger::{LedgerEntry, RequestLedger};
pub use op::{Op, Submission, TurnMode};
pub use sandbox::{DenialClass, NetworkMode, SandboxMode, SandboxPolicy, default_secret_globs};
pub use usage::{StopReason, Usage};
