//! Adapts MCP server tools to GrokForge's [`Tool`] trait, so the agent loop and approval engine
//! treat them exactly like built-ins. MCP tools are always approval-gated (their side effects are
//! outside our sandbox) and their calls are recorded in the ledger like any other request.

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
        format!("mcp__{server}__{tool}")
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
        match self.conn.call_tool(&self.tool.name, inv.args).await {
            Ok(text) => ToolOutput::success(text),
            Err(e) => ToolOutput::failure(format!("mcp `{}` error: {e}", self.server)),
        }
    }
}
