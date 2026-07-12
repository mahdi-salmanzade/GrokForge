//! Session configuration and in-memory conversation state.

use std::path::PathBuf;

use grokforge_protocol::{ApprovalPolicy, NetworkMode, ResponseItem, SandboxMode, SessionId};
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
    /// Network capability granted to sandboxed commands for this session.
    pub network: NetworkMode,
    pub effort: Option<Effort>,
    pub system_prompt: String,
    /// Hard cap on tool-call iterations within one turn.
    pub max_iterations: u32,
    /// Auto-commit the agent's edits at the end of a mutating turn (when in a git repo).
    pub auto_commit: bool,
    /// Whether this session owns an isolated Git worktree.
    ///
    /// Auto-commit is only safe when no user or sibling session can race a write to the same
    /// path between the tool operation and staging. Foreground sessions therefore leave this
    /// false; subagents set it after creating their dedicated worktree.
    pub isolated_worktree: bool,
    /// Compact history at turn end once it exceeds this estimated byte size.
    pub compaction_trigger_bytes: usize,
    /// How many recent items to keep verbatim when compacting.
    pub compaction_keep_tail: usize,
    /// Whether to auto-compact at turn end.
    pub auto_compact: bool,
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
            network: NetworkMode::Isolated,
            effort: None,
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            max_iterations: 32,
            auto_commit: true,
            isolated_worktree: false,
            // ~100k tokens at 4 bytes/token; well below the smallest model context.
            compaction_trigger_bytes: 400_000,
            compaction_keep_tail: 8,
            auto_compact: true,
        }
    }

    #[must_use]
    pub fn with_policy(mut self, policy: ApprovalPolicy, mode: SandboxMode) -> Self {
        self.approval_policy = policy;
        self.sandbox_mode = mode;
        self.network = if mode == SandboxMode::DangerFullAccess {
            NetworkMode::Full
        } else {
            NetworkMode::Isolated
        };
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
    ///
    /// This constructor mints a new id and is therefore appropriate for a fork. Use
    /// [`Self::with_id_and_history`] when continuing the same persisted rollout.
    #[must_use]
    pub fn with_history(config: SessionConfig, history: Vec<ResponseItem>) -> Self {
        Self {
            id: SessionId::new(),
            config,
            history,
        }
    }

    /// Continue a persisted session while preserving its stable id/cache key and rollout path.
    pub fn with_id_and_history(
        config: SessionConfig,
        session_id: &str,
        history: Vec<ResponseItem>,
    ) -> Result<Self, String> {
        let id = SessionId::parse_str(session_id).map_err(|e| e.to_string())?;
        Ok(Self {
            id,
            config,
            history,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_constructor_preserves_session_id() {
        let original = SessionId::new();
        let session = Session::with_id_and_history(
            SessionConfig::new(PathBuf::from("/tmp"), "m"),
            &original.as_uuid().to_string(),
            vec![ResponseItem::user("prior")],
        )
        .unwrap();
        assert_eq!(session.id, original);
        assert_eq!(session.history.len(), 1);
    }
}
