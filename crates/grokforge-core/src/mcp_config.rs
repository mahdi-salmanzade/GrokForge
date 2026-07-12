//! Loads MCP server declarations from `.grokforge/mcp.json`, connects them, and registers their
//! tools into a [`ToolRegistry`]. Format:
//!
//! ```json
//! { "servers": { "docs": { "command": "my-mcp-server", "args": ["--flag"] } } }
//! ```

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use grokforge_mcp::{McpConnection, StdioClient};
use serde::Deserialize;

use crate::tools::ToolRegistry;
use crate::tools::mcp::McpToolAdapter;

#[derive(Debug, Deserialize)]
struct McpConfig {
    #[serde(default)]
    servers: BTreeMap<String, ServerSpec>,
}

#[derive(Debug, Deserialize)]
struct ServerSpec {
    command: String,
    #[serde(default)]
    args: Vec<String>,
}

/// Connect the MCP servers declared under `workspace/.grokforge/mcp.json` and register their tools.
/// Returns the names of servers that connected successfully. Failures are logged, not fatal.
pub async fn connect_and_register(workspace: &Path, registry: &mut ToolRegistry) -> Vec<String> {
    let path = workspace.join(".grokforge/mcp.json");
    let Ok(text) = tokio::fs::read_to_string(&path).await else {
        return Vec::new();
    };
    let config: McpConfig = match serde_json::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("invalid .grokforge/mcp.json: {e}");
            return Vec::new();
        }
    };

    let mut connected = Vec::new();
    for (name, spec) in config.servers {
        match StdioClient::connect(&name, &spec.command, &spec.args).await {
            Ok(client) => {
                let conn: Arc<dyn McpConnection> = Arc::new(client);
                match conn.list_tools().await {
                    Ok(tools) => {
                        let count = tools.len();
                        for tool in tools {
                            registry.register(Arc::new(McpToolAdapter::new(
                                name.clone(),
                                tool,
                                Arc::clone(&conn),
                            )));
                        }
                        tracing::info!("mcp: connected `{name}` ({count} tools)");
                        connected.push(name);
                    }
                    Err(e) => tracing::warn!("mcp `{name}` tools/list failed: {e}"),
                }
            }
            Err(e) => tracing::warn!("mcp `{name}` failed to start: {e}"),
        }
    }
    connected
}
