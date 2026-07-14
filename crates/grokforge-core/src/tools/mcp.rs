//! Adapts MCP server tools to GrokForge's [`Tool`] trait, so the agent loop and approval engine
//! treat them exactly like built-ins. MCP tools are always approval-gated (their side effects are
//! outside our sandbox) and their calls are recorded in the ledger like any other request.

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use grokforge_mcp::{McpConnection, McpTool};
use grokforge_protocol::ApprovalKind;

use crate::approvals::ApprovalNeed;
use crate::redaction::Redactor;
use crate::tools::{Tool, ToolInvocation, ToolOutput, ToolSpec, TurnContext};

/// A single MCP tool exposed as a GrokForge tool. Named `mcp__<server>__<tool>`.
#[derive(Debug)]
pub struct McpToolAdapter {
    server: String,
    tool: McpTool,
    conn: Arc<dyn McpConnection>,
    advertised_name: String,
    advertised_description: String,
    advertised_parameters: serde_json::Value,
    metadata_redactions: usize,
}

impl McpToolAdapter {
    #[must_use]
    pub fn new(server: impl Into<String>, tool: McpTool, conn: Arc<dyn McpConnection>) -> Self {
        let server = server.into();
        let advertised_name = Self::advertised_name(&server, &tool.name);
        let redacted_server = Redactor::apply(&server);
        let redacted_tool = Redactor::apply(&tool.name);
        let description = Redactor::apply(&tool.description);
        let (parameters, parameter_redactions) = redact_json_strings(&tool.input_schema);
        let metadata_redactions = redacted_server
            .count
            .saturating_add(redacted_tool.count)
            .saturating_add(description.count)
            .saturating_add(parameter_redactions);
        Self {
            server,
            tool,
            conn,
            advertised_name,
            advertised_description: description.text,
            advertised_parameters: parameters,
            metadata_redactions,
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

    /// Provider-facing name after applying the same secret redaction as [`Self::new`]. Preflight
    /// uses this form so two distinct raw names that collapse to the same redacted name cannot be
    /// registered partially or ambiguously.
    #[must_use]
    pub(crate) fn advertised_name(server: &str, tool: &str) -> String {
        let server = Redactor::apply(server);
        let tool = Redactor::apply(tool);
        Self::qualified_name(&server.text, &tool.text)
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn spec(&self) -> ToolSpec {
        let parameters = if self.advertised_parameters.is_object() {
            self.advertised_parameters.clone()
        } else {
            serde_json::json!({ "type": "object", "properties": {} })
        };
        ToolSpec {
            name: self.advertised_name.clone(),
            description: self.advertised_description.clone(),
            parameters,
            // Conservative: MCP side effects are unknown, so treat as mutating + serialized.
            mutating: true,
            parallel_safe: false,
        }
    }

    fn metadata_redactions(&self) -> usize {
        self.metadata_redactions
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

fn redact_json_strings(value: &serde_json::Value) -> (serde_json::Value, usize) {
    match value {
        serde_json::Value::String(text) => {
            let redacted = Redactor::apply(text);
            (serde_json::Value::String(redacted.text), redacted.count)
        }
        serde_json::Value::Array(values) => {
            let mut count = 0_usize;
            let values = values
                .iter()
                .map(|value| {
                    let (value, redactions) = redact_json_strings(value);
                    count = count.saturating_add(redactions);
                    value
                })
                .collect();
            (serde_json::Value::Array(values), count)
        }
        serde_json::Value::Object(values) => {
            let mut count = 0_usize;
            let mut redacted = serde_json::Map::new();
            for (key, value) in values {
                let redacted_key = Redactor::apply(key);
                let (value, value_redactions) = redact_json_strings(value);
                count = count
                    .saturating_add(redacted_key.count)
                    .saturating_add(value_redactions);
                redacted.insert(redacted_key.text, value);
            }
            (serde_json::Value::Object(redacted), count)
        }
        scalar => (scalar.clone(), 0),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use grokforge_mcp::{McpConnection, McpError, McpTool};
    use serde_json::{Value, json};

    use super::McpToolAdapter;
    use crate::tools::Tool;

    #[derive(Debug)]
    struct NoopConnection;

    #[async_trait]
    impl McpConnection for NoopConnection {
        async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
            Ok(Vec::new())
        }

        async fn call_tool(&self, _name: &str, _args: Value) -> Result<String, McpError> {
            Ok(String::new())
        }
    }

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

    #[test]
    fn dynamic_metadata_is_recursively_redacted_before_advertising() {
        let bearer = "abcdefghijklmnopqrstuvwxyz123456";
        let assigned = "abcdefghijklmnopqrstuv";
        let tool: McpTool = serde_json::from_value(json!({
            "name": "search",
            "description": format!("Call with Bearer {bearer}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    format!("api_key={assigned}"): {
                        "type": "string",
                        "description": format!("Bearer {bearer}")
                    }
                }
            }
        }))
        .unwrap();
        let adapter = McpToolAdapter::new("docs", tool, Arc::new(NoopConnection));
        let spec = adapter.spec();
        let serialized = serde_json::to_string(&json!({
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.parameters,
        }))
        .unwrap();

        assert!(!serialized.contains(bearer));
        assert!(!serialized.contains(assigned));
        assert!(serialized.contains("[REDACTED:"));
        assert!(adapter.metadata_redactions() >= 3);
    }

    #[test]
    fn secret_bearing_names_that_redact_identically_are_detectable_collisions() {
        let first =
            McpToolAdapter::advertised_name("Bearer abcdefghijklmnopqrstuvwxyz123456", "search");
        let second =
            McpToolAdapter::advertised_name("Bearer zyxwvutsrqponmlkjihgfedcba654321", "search");
        assert_eq!(first, second);
    }
}
