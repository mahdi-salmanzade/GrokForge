//! Session configuration and in-memory conversation state.

use std::collections::BTreeSet;
use std::path::PathBuf;

use grokforge_protocol::{ApprovalPolicy, NetworkMode, ResponseItem, SandboxMode, SessionId};
use grokforge_xai::{Effort, ServerTool};

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
    /// Explicitly enabled Grok-native server tools. These are separately metered by xAI and are
    /// therefore off by default. The closed enum prevents arbitrary provider JSON from entering
    /// session configuration.
    pub enabled_server_tools: BTreeSet<ServerTool>,
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
    /// The model's advertised context window in tokens (from `GET /v1/models`), when known. Used
    /// to bound the assembled request so it never exceeds the provider's hard prompt-length limit.
    /// `None` falls back to a conservative default that is safe for every current model.
    pub context_window_tokens: Option<u64>,
}

/// Conservative bytes-per-token estimate. Real text is ~3.3–4.0 bytes/token; deliberately
/// under-estimating means a given byte budget maps to *fewer* tokens, keeping the request safely
/// under the model's token limit rather than risking a hard 400.
const BYTES_PER_TOKEN: usize = 3;
/// Tokens reserved for the model's response so the input budget still leaves room to answer.
const OUTPUT_RESERVE_TOKENS: u64 = 16_384;
/// Fallback context window when the model's real window is unknown. Matches the smallest common
/// Grok context, so it is safe (never over-large) for every model.
const FALLBACK_CONTEXT_TOKENS: u64 = 256_000;

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
            enabled_server_tools: BTreeSet::new(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            max_iterations: 32,
            auto_commit: true,
            isolated_worktree: false,
            // ~100k tokens at 4 bytes/token; well below the smallest model context.
            compaction_trigger_bytes: 400_000,
            compaction_keep_tail: 8,
            auto_compact: true,
            context_window_tokens: None,
        }
    }

    /// The maximum assembled-request size, in bytes, that stays under the model's input-token
    /// budget: the context window minus a response reserve, at a conservative bytes/token ratio.
    /// The whole serialized request body (system prompt, auto-context, tool defs, and history) is
    /// held under this so the provider never rejects the turn with a prompt-too-long 400.
    #[must_use]
    pub fn input_budget_bytes(&self) -> usize {
        let window = self
            .context_window_tokens
            .unwrap_or(FALLBACK_CONTEXT_TOKENS);
        let input_tokens = window.saturating_sub(OUTPUT_RESERVE_TOKENS);
        usize::try_from(input_tokens)
            .unwrap_or(usize::MAX)
            .saturating_mul(BYTES_PER_TOKEN)
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

    /// Enable one known, separately metered Grok-native server tool for this session.
    #[must_use]
    pub fn with_server_tool(mut self, tool: ServerTool) -> Self {
        self.enabled_server_tools.insert(tool);
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
    fn input_budget_falls_back_and_scales_with_the_context_window() {
        let mut config = SessionConfig::new(PathBuf::from("/tmp"), "m");
        // Unknown window → a non-zero conservative budget.
        let fallback = config.input_budget_bytes();
        assert!(fallback > 0);
        // A larger advertised window yields a larger budget.
        config.context_window_tokens = Some(1_000_000);
        assert!(config.input_budget_bytes() > fallback);
        // A window at or below the output reserve clamps to zero rather than underflowing.
        config.context_window_tokens = Some(1);
        assert_eq!(config.input_budget_bytes(), 0);
    }

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

    #[test]
    fn server_tools_are_opt_in_and_deduplicated() {
        let default = SessionConfig::new(PathBuf::from("/tmp"), "m");
        assert!(default.enabled_server_tools.is_empty());

        let configured = default
            .with_server_tool(ServerTool::WebSearch)
            .with_server_tool(ServerTool::WebSearch)
            .with_server_tool(ServerTool::CodeInterpreter);
        assert_eq!(configured.enabled_server_tools.len(), 2);
        assert!(
            configured
                .enabled_server_tools
                .contains(&ServerTool::WebSearch)
        );
        assert!(
            configured
                .enabled_server_tools
                .contains(&ServerTool::CodeInterpreter)
        );
    }
}
