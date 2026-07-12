//! Events from the agent core to a frontend (the "EQ" half of the protocol). The TUI and the
//! headless `--json` frontend both consume these and nothing else — this is the seam an ACP
//! adapter would reuse (ADR 0005).

use serde::{Deserialize, Serialize};

use crate::approval::ApprovalRequest;
use crate::ids::{SessionId, ToolCallId, TurnId};
use crate::ledger::LedgerEntry;
use crate::sandbox::DenialClass;
use crate::usage::{StopReason, Usage};

/// One event emitted by the core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventMsg {
    /// The session is ready.
    SessionConfigured { session_id: SessionId },
    /// A turn started.
    TurnStarted { turn_id: TurnId },
    /// Incremental assistant text.
    AgentMessageDelta { delta: String },
    /// The assistant's final message text for this response.
    AgentMessageDone { text: String },
    /// Incremental reasoning-summary text.
    ReasoningDelta { delta: String },
    /// A tool call is starting.
    ToolCallBegin {
        call_id: ToolCallId,
        name: String,
        args_preview: String,
        sandboxed: bool,
    },
    /// Streamed tool output.
    ToolOutputDelta { call_id: ToolCallId, chunk: String },
    /// A tool call finished.
    ToolCallEnd {
        call_id: ToolCallId,
        ok: bool,
        summary: String,
        denial: Option<DenialClass>,
    },
    /// The core needs an approval decision.
    ApprovalRequested(ApprovalRequest),
    /// An approval was resolved (recorded for the audit trail; `auto` in headless).
    ApprovalResolved {
        summary: String,
        decision: String,
        auto: bool,
    },
    /// A ledger entry was recorded for the request being assembled.
    LedgerAppended(LedgerEntry),
    /// Token usage and cost for a response.
    TokenUsage { usage: Usage },
    /// The client is replaying the request after a failure.
    StreamRetrying { attempt: u32, reason: String },
    /// The turn finished.
    TurnComplete { turn_id: TurnId, stop: StopReason },
    /// A (possibly recoverable) error.
    Error { message: String, recoverable: bool },
    /// The session has shut down.
    ShutdownComplete,
}
