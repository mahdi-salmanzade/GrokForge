//! `grokforge-core` — the agent. Everything a frontend needs to run Grok against a project,
//! with zero terminal dependencies (this is the reuse unit a future ACP adapter would share).
//!
//! Load-bearing invariants:
//! - **The ledger choke point** ([`context::assemble`]) is the only path that builds a request
//!   body, so every outbound byte is accounted for (ADR 0003).
//! - **Redaction at ingress** ([`redaction`]) scrubs secrets from tool output and user input
//!   before they enter the transcript.
//! - **The approval decision table** ([`approvals::gate`]) is pure and exhaustively tested.
//! - **Sessions persist to append-only JSONL rollouts** ([`store`], ADR 0002).

pub mod agents_md;
pub mod approvals;
mod cancellation;
pub mod compaction;
pub mod context;
pub mod mcp_config;
mod path_safety;
pub mod redaction;
pub mod session;
pub mod store;
pub mod tools;
pub mod turn;

pub use approvals::{AllowRule, ApprovalNeed, Approver, AutoApprover, Gate, gate};
pub use cancellation::TurnCancellation;
pub use redaction::{Redacted, Redactor};
pub use session::{DEFAULT_SYSTEM_PROMPT, Session, SessionConfig};
pub use store::{LogRotation, RolloutWriter, SessionMeta, rollout_path, sessions_dir};
pub use tools::{Tool, ToolInvocation, ToolOutput, ToolRegistry, ToolSpec, TurnContext};
pub use turn::Agent;
