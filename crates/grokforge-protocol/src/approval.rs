//! Approval requests and the policy enum. The decision *logic* (the 4×3 matrix) lives in
//! `grokforge-core`; these are the shared value types a frontend renders and answers.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::ids::{ApprovalId, ToolCallId};
use crate::sandbox::{DenialClass, SandboxMode};

/// When the agent should stop to ask the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// Ask before essentially everything. (`strict` preset.)
    Untrusted,
    /// Auto-run what fits the sandbox; ask to exceed it. (`readonly`/`auto` presets.)
    OnRequest,
    /// Run sandboxed; ask only when the sandbox actually blocks something.
    OnFailure,
    /// Never ask. (`yolo` preset.)
    Never,
}

/// What the agent wants to do that may require approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalKind {
    /// Run a shell command.
    ExecCommand {
        command: Vec<String>,
        cwd: PathBuf,
        sandbox: SandboxMode,
        /// Set when this is a re-run after the sandbox blocked the command.
        escalation_of: Option<DenialClass>,
    },
    /// Write or overwrite a file.
    WriteFile { path: PathBuf },
    /// Apply a patch touching these files.
    ApplyPatch { files: Vec<PathBuf> },
    /// A git mutation performed by the host process.
    GitMutation {
        description: String,
        command: Vec<String>,
    },
    /// Reach a network host.
    Network { host: String },
    /// Call an MCP tool.
    McpToolCall { server: String, tool: String },
}

/// A pending approval the frontend must answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: ApprovalId,
    pub call_id: Option<ToolCallId>,
    pub kind: ApprovalKind,
    /// The model's stated reason for wanting this.
    pub reason: String,
}

/// The user's answer to an [`ApprovalRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    /// Approve this one action.
    Approve,
    /// Approve and stop asking for this boundary for the rest of the session.
    ApproveForSession,
    /// Reject.
    Deny,
    /// Reject and tell the model what to do instead.
    DenyWithFeedback(String),
    /// Abort the whole turn.
    Abort,
}

impl Decision {
    /// Whether the action should proceed.
    #[must_use]
    pub fn is_approved(&self) -> bool {
        matches!(self, Decision::Approve | Decision::ApproveForSession)
    }
}
