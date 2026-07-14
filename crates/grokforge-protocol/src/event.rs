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
    /// The host process created an auto-commit for the agent's edits.
    Committed { sha: String, message: String },
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

    /// A subagent lane began running as part of a parallel `spawn_task` fan-out. `agent_id` is
    /// the spawning tool call's id (stable for the lane's lifetime); `label` is a short preview
    /// of the subtask prompt; `index`/`total` position the lane within the current batch.
    SubagentStarted {
        agent_id: String,
        label: String,
        index: usize,
        total: usize,
    },
    /// An event produced by a running subagent, tagged with its lane so a frontend can render
    /// per-agent progress instead of interleaving every subagent's stream into one transcript.
    /// Accounting-relevant inner events (`TokenUsage`, `LedgerAppended`) are still folded into
    /// global totals by frontends; the wrapper only changes attribution and display.
    SubagentUpdate {
        agent_id: String,
        inner: Box<EventMsg>,
    },
    /// A subagent lane finished. `summary` is a one-line result and `ok` reflects clean
    /// completion (committed cleanly, no leftover uncommitted work).
    SubagentFinished {
        agent_id: String,
        ok: bool,
        summary: String,
    },
}
