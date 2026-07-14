//! The unified tool layer. Built-in tools and (later) MCP tools implement one [`Tool`] trait,
//! so the agent loop treats them identically and the approval engine gates them uniformly.

pub mod builtins;
pub mod mcp;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use grokforge_protocol::{DenialClass, SandboxPolicy, ToolCallId};
use grokforge_sandbox::SandboxRunner;
use grokforge_xai::ToolDef;

use crate::TurnCancellation;
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
    /// Canonical physical paths approved for an elevated host-process write. File tools bind the
    /// actual descriptor-relative mutation to these targets, closing approval/use symlink races.
    pub bound_write_targets: Vec<PathBuf>,
    /// Cooperative cancellation for network and sandbox operations in this turn.
    pub cancellation: TurnCancellation,
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
        let joined = if p.is_absolute() {
            p
        } else {
            self.workspace_root.join(p)
        };
        crate::path_safety::normalize(&joined)
    }

    /// Record that a path was written this turn (for auto-commit).
    pub fn record_touched(&self, path: PathBuf) {
        if let Ok(mut touched) = self.touched.lock()
            && !touched.contains(&path)
        {
            touched.push(path);
        }
    }

    /// The paths written this turn.
    #[must_use]
    pub fn touched_paths(&self) -> Vec<PathBuf> {
        self.touched.lock().map(|t| t.clone()).unwrap_or_default()
    }

    #[must_use]
    pub fn bound_write_target(&self, path: &std::path::Path) -> Option<PathBuf> {
        self.bound_write_targets
            .iter()
            .find(|target| target.as_path() == path)
            .cloned()
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

    /// Secret-pattern matches removed from the advertised name, description, or schema. Built-ins
    /// are static and default to zero; dynamic adapters override this for ledger accounting.
    fn metadata_redactions(&self) -> usize {
        0
    }

    /// What approval this call needs, given its arguments and the turn context.
    fn approval(&self, args: &serde_json::Value, ctx: &TurnContext) -> ApprovalNeed;

    /// Execute the call.
    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput;
}

/// The set of tools available to a session. Enforces the provider tool-count cap.
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

/// The maximum number of tools xAI accepts in one Responses API request.
pub const MAX_TOOLS: usize = 128;

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

    /// Register an additional tool (e.g. an MCP adapter).
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.spec().name;
        if builtins::is_builtin(&name) {
            tracing::warn!(tool = %name, "refusing to replace an existing or built-in tool");
            return;
        }
        if self.tools.len() >= MAX_TOOLS {
            tracing::warn!(tool = %name, limit = MAX_TOOLS, "refusing tool beyond registry limit");
            return;
        }
        match self.tools.entry(name) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(tool);
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                tracing::warn!(tool = %entry.key(), "refusing to replace an existing tool");
            }
        }
    }

    #[must_use]
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    /// Convert the registered tools to xAI function-tool definitions (capped at [`MAX_TOOLS`]).
    #[must_use]
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        self.defs_where(|_| true)
    }

    /// Only the non-mutating (read-only) tools — used in plan mode.
    #[must_use]
    pub fn readonly_tool_defs(&self) -> Vec<ToolDef> {
        self.defs_where(|s| !s.mutating)
    }

    fn defs_where(&self, keep: impl Fn(&ToolSpec) -> bool) -> Vec<ToolDef> {
        let mut specs: Vec<(ToolSpec, usize)> = self
            .tools
            .values()
            .map(|tool| (tool.spec(), tool.metadata_redactions()))
            .filter(|(spec, _)| keep(spec))
            .collect();
        // Built-ins are the agent's basic safety/repair surface; a large MCP registry must not
        // push `read_file`/`write_file` out of the provider's tool limit.
        specs.sort_by_key(|(spec, _)| (!builtins::is_builtin(&spec.name), spec.name.clone()));
        specs
            .into_iter()
            .take(MAX_TOOLS)
            .map(|(spec, redactions)| {
                ToolDef::function_with_metadata_redactions(
                    spec.name,
                    spec.description,
                    spec.parameters,
                    redactions,
                )
            })
            .collect()
    }
}

/// Parse a required string field from a tool's JSON arguments.
pub(crate) fn arg_str<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str, ToolOutput> {
    args.get(key).and_then(|v| v.as_str()).ok_or_else(|| {
        ToolOutput::failure(format!("missing or non-string required argument `{key}`"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct ExtraTool(String);

    #[async_trait]
    impl Tool for ExtraTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.clone(),
                description: "test".into(),
                parameters: serde_json::json!({"type":"object"}),
                mutating: false,
                parallel_safe: true,
            }
        }

        fn approval(&self, _args: &serde_json::Value, _ctx: &TurnContext) -> ApprovalNeed {
            ApprovalNeed::None
        }

        async fn invoke(&self, _inv: ToolInvocation<'_>) -> ToolOutput {
            ToolOutput::success("ok")
        }
    }

    #[test]
    fn provider_tool_cap_preserves_all_builtins() {
        let mut registry = ToolRegistry::with_builtins();
        for index in 0..200 {
            registry.register(Arc::new(ExtraTool(format!("extra_{index:03}"))));
        }
        let defs = registry.tool_defs();
        assert_eq!(defs.len(), MAX_TOOLS);
        let names: Vec<&str> = defs.iter().filter_map(ToolDef::function_name).collect();
        for builtin in [
            "read_file",
            "write_file",
            "edit",
            "shell",
            "list",
            "glob",
            "grep",
            "git_status",
            "git_diff",
            builtins::SPAWN_TASK,
        ] {
            assert!(names.contains(&builtin), "missing built-in {builtin}");
        }
    }

    #[test]
    fn additional_tools_cannot_replace_a_builtin() {
        let mut registry = ToolRegistry::with_builtins();
        registry.register(Arc::new(ExtraTool("write_file".into())));
        let write = registry.get("write_file").unwrap().spec();
        assert_ne!(write.description, "test");
        assert!(write.mutating);
    }
}
