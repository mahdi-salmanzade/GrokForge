//! Session configuration and in-memory conversation state.

use std::path::PathBuf;

use grokforge_protocol::{ApprovalPolicy, ResponseItem, SandboxMode, SessionId};
use grokforge_xai::Effort;

/// The default system prompt. Kept small on purpose (a bloated prompt burns tokens and cache).
pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are GrokForge, a terminal coding agent operating inside the user's project. \
Use the provided tools to read, search, edit, and run code. Prefer small, verifiable steps. \
Respect the sandbox and approval prompts. When the task is complete, give a short summary.";

/// Immutable-per-session configuration.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub workspace_root: PathBuf,
    pub model: String,
    pub approval_policy: ApprovalPolicy,
    pub sandbox_mode: SandboxMode,
    pub effort: Option<Effort>,
    pub system_prompt: String,
    /// Hard cap on tool-call iterations within one turn.
    pub max_iterations: u32,
    /// Auto-commit the agent's edits at the end of a mutating turn (when in a git repo).
    pub auto_commit: bool,
}

impl SessionConfig {
    /// A config for `workspace_root` and `model` with sensible defaults.
    #[must_use]
    pub fn new(workspace_root: PathBuf, model: impl Into<String>) -> Self {
        Self {
            workspace_root,
            model: model.into(),
            approval_policy: ApprovalPolicy::OnRequest,
            sandbox_mode: SandboxMode::WorkspaceWrite,
            effort: None,
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            max_iterations: 32,
            auto_commit: true,
        }
    }

    #[must_use]
    pub fn with_policy(mut self, policy: ApprovalPolicy, mode: SandboxMode) -> Self {
        self.approval_policy = policy;
        self.sandbox_mode = mode;
        self
    }
}

/// A live session: its config and the canonical conversation history.
#[derive(Debug)]
pub struct Session {
    pub id: SessionId,
    pub config: SessionConfig,
    pub history: Vec<ResponseItem>,
}

impl Session {
    #[must_use]
    pub fn new(config: SessionConfig) -> Self {
        Self {
            id: SessionId::new(),
            config,
            history: Vec::new(),
        }
    }

    /// Rebuild a session from a persisted transcript (resume).
    #[must_use]
    pub fn with_history(config: SessionConfig, history: Vec<ResponseItem>) -> Self {
        Self {
            id: SessionId::new(),
            config,
            history,
        }
    }
}
