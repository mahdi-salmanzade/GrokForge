//! Submissions from a frontend to the agent core (the "SQ" half of the protocol).

use serde::{Deserialize, Serialize};

use crate::approval::{ApprovalPolicy, Decision};
use crate::ids::{ApprovalId, SubId};
use crate::sandbox::SandboxMode;

/// A frontend submission wrapping one [`Op`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    pub id: SubId,
    pub op: Op,
}

/// What the frontend is asking the core to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// Start a turn with a user message.
    UserTurn { text: String, mode: TurnMode },
    /// Cancel the running turn and its tools.
    Interrupt,
    /// Answer a pending approval.
    ApprovalDecision { id: ApprovalId, decision: Decision },
    /// Change the active policy/mode mid-session.
    SetPolicy {
        approval: Option<ApprovalPolicy>,
        sandbox: Option<SandboxMode>,
    },
    /// Compact the conversation now.
    Compact,
    /// Shut the session down cleanly.
    Shutdown,
}

/// Whether a turn executes or only plans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnMode {
    #[default]
    Execute,
    Plan,
}
