//! The unified tool layer. Built-in tools and (later) MCP tools implement one [`Tool`] trait,
//! so the agent loop treats them identically and the approval engine gates them uniformly.

pub mod builtins;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use grokforge_protocol::{DenialClass, SandboxPolicy, ToolCallId};
use grokforge_sandbox::SandboxRunner;
use grokforge_xai::ToolDef;

use crate::approvals::ApprovalNeed;

/// A tool's advertised interface.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: serde_json::Value,
    /// Whether the tool can change the workspace (affects approval + parallelism).
    pub mutating: bool,
    /// Whether the tool is safe to run concurrently with other parallel-safe tools.
    pub parallel_safe: bool,
}

/// Everything a tool needs from the running turn.
#[derive(Clone)]
pub struct TurnContext {
    pub workspace_root: PathBuf,
    pub policy: SandboxPolicy,
    pub sandbox: Arc<dyn SandboxRunner>,
    /// Paths the agent has written this turn, collected so only those are auto-committed.
    pub touched: Arc<std::sync::Mutex<Vec<PathBuf>>>,
}

impl std::fmt::Debug for TurnContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnContext")
            .field("workspace_root", &self.workspace_root)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl TurnContext {
    /// Resolve a possibly-relative path against the workspace root.
    #[must_use]
    pub fn resolve(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            p
        } else {
            self.workspace_root.join(p)
        }
    }

    /// Record that a path was written this turn (for auto-commit).
    pub fn record_touched(&self, path: PathBuf) {
        if let Ok(mut touched) = self.touched.lock() {
            if !touched.contains(&path) {
                touched.push(path);
            }
        }
    }

    /// The paths written this turn.
    #[must_use]
    pub fn touched_paths(&self) -> Vec<PathBuf> {
        self.touched.lock().map(|t| t.clone()).unwrap_or_default()
    }
}

/// A single tool call to execute.
#[derive(Debug)]
pub struct ToolInvocation<'a> {
    pub call_id: ToolCallId,
    pub args: serde_json::Value,
    pub ctx: &'a TurnContext,
}

/// The result of running a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutput {
    /// Success, with content to feed back to the model.
    Success { content: String },
    /// Failure. `denial` is set when the sandbox (not the command itself) caused it.
    Failure {
        error: String,
        denial: Option<DenialClass>,
    },
}

impl ToolOutput {
    #[must_use]
    pub fn success(content: impl Into<String>) -> Self {
        ToolOutput::Success {
            content: content.into(),
        }
    }

    #[must_use]
    pub fn failure(error: impl Into<String>) -> Self {
        ToolOutput::Failure {
            error: error.into(),
            denial: None,
        }
    }

    #[must_use]
    pub fn is_error(&self) -> bool {
        matches!(self, ToolOutput::Failure { .. })
    }

    /// The text fed back to the model (content or error message).
    #[must_use]
    pub fn content(&self) -> &str {
        match self {
            ToolOutput::Success { content } => content,
            ToolOutput::Failure { error, .. } => error,
        }
    }
}

/// A tool that the model can call.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The advertised interface.
    fn spec(&self) -> ToolSpec;

    /// What approval this call needs, given its arguments and the turn context.
    fn approval(&self, args: &serde_json::Value, ctx: &TurnContext) -> ApprovalNeed;

    /// Execute the call.
    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput;
}

/// The set of tools available to a session. Enforces the ≤200-tool xAI request cap.
#[derive(Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// The maximum number of tools xAI accepts in one request.
pub const MAX_TOOLS: usize = 200;

impl ToolRegistry {
    /// A registry with all built-in tools registered.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut tools: BTreeMap<String, Arc<dyn Tool>> = BTreeMap::new();
        for tool in builtins::all() {
            tools.insert(tool.spec().name, tool);
        }
        Self { tools }
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    #[must_use]
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    /// Convert the registered tools to xAI function-tool definitions (capped at [`MAX_TOOLS`]).
    #[must_use]
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        self.specs()
            .into_iter()
            .take(MAX_TOOLS)
            .map(|s| ToolDef::function(s.name, s.description, s.parameters))
            .collect()
    }
}

/// Parse a required string field from a tool's JSON arguments.
pub(crate) fn arg_str<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str, ToolOutput> {
    args.get(key).and_then(|v| v.as_str()).ok_or_else(|| {
        ToolOutput::failure(format!("missing or non-string required argument `{key}`"))
    })
}
