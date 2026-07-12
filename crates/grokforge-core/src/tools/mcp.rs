//! Adapts MCP server tools to GrokForge's [`Tool`] trait, so the agent loop and approval engine
//! treat them exactly like built-ins. MCP tools are always approval-gated (their side effects are
//! outside our sandbox) and their calls are recorded in the ledger like any other request.

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use grokforge_mcp::{McpConnection, McpTool};
use grokforge_protocol::ApprovalKind;

use crate::approvals::ApprovalNeed;
use crate::tools::{Tool, ToolInvocation, ToolOutput, ToolSpec, TurnContext};

/// A single MCP tool exposed as a GrokForge tool. Named `mcp__<server>__<tool>`.
#[derive(Debug)]
pub struct McpToolAdapter {
    server: String,
    tool: McpTool,
    conn: Arc<dyn McpConnection>,
}

impl McpToolAdapter {
    #[must_use]
    pub fn new(server: impl Into<String>, tool: McpTool, conn: Arc<dyn McpConnection>) -> Self {
        Self {
            server: server.into(),
            tool,
            conn,
        }
    }

    #[must_use]
    pub fn qualified_name(server: &str, tool: &str) -> String {
        let raw = format!("mcp__{server}__{tool}");
        if raw.len() <= 64
            && raw
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return raw;
        }
        let mut safe: String = raw
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .take(51)
            .collect();
        // Stable FNV-1a suffix prevents two distinct invalid/truncated names from colliding.
        let hash = raw.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
            (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
        });
        let _ = write!(safe, "__{hash:08x}");
        safe
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn spec(&self) -> ToolSpec {
        let parameters = if self.tool.input_schema.is_object() {
            self.tool.input_schema.clone()
        } else {
            serde_json::json!({ "type": "object", "properties": {} })
        };
        ToolSpec {
            name: Self::qualified_name(&self.server, &self.tool.name),
            description: self.tool.description.clone(),
            parameters,
            // Conservative: MCP side effects are unknown, so treat as mutating + serialized.
            mutating: true,
            parallel_safe: false,
        }
    }

    fn approval(&self, _args: &serde_json::Value, _ctx: &TurnContext) -> ApprovalNeed {
        ApprovalNeed::Always(ApprovalKind::McpToolCall {
            server: self.server.clone(),
            tool: self.tool.name.clone(),
        })
    }

    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        let result = tokio::select! {
            result = self.conn.call_tool(&self.tool.name, inv.args) => Some(result),
            () = inv.ctx.cancellation.cancelled() => None,
        };
        match result {
            None => ToolOutput::failure("[turn interrupted by user; MCP call cancelled]"),
            Some(Ok(text)) => ToolOutput::success(text),
            Some(Err(e)) => ToolOutput::failure(format!("mcp `{}` error: {e}", self.server)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::McpToolAdapter;

    #[test]
    fn qualified_names_are_provider_safe_and_collision_resistant() {
        assert_eq!(
            McpToolAdapter::qualified_name("docs", "search"),
            "mcp__docs__search"
        );
        let a = McpToolAdapter::qualified_name("bad server", "tool/name");
        let b = McpToolAdapter::qualified_name("bad/server", "tool name");
        assert!(a.len() <= 64);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        );
        assert_ne!(a, b);
    }
}
