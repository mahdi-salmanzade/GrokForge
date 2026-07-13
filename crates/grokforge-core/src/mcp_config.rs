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

const MAX_MCP_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_PROJECT_MCP_SERVERS: usize = 16;

/// Displayed by frontends immediately before starting explicitly trusted project MCP servers.
pub const PROJECT_MCP_TRUST_WARNING: &str = "warning: --trust-project-mcp allows .grokforge/mcp.json to execute local commands outside GrokForge's sandbox; only enable it for projects you trust";

/// Project MCP configuration is executable code. This default entry point deliberately refuses
/// to spawn it; a frontend must obtain explicit user trust and then call
/// [`connect_and_register_trusted`].
pub async fn connect_and_register(workspace: &Path, registry: &mut ToolRegistry) -> Vec<String> {
    let path = workspace.join(".grokforge/mcp.json");
    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        tracing::warn!(
            path = %path.display(),
            "project MCP config not started without explicit trust"
        );
    }
    let _ = registry;
    Vec::new()
}

/// Connect explicitly trusted project MCP servers and register their tools. Child processes run
/// with the workspace as cwd and a scrubbed environment. Returns successfully connected names;
/// individual failures are logged and are not fatal to the session.
pub async fn connect_and_register_trusted(
    workspace: &Path,
    registry: &mut ToolRegistry,
) -> Vec<String> {
    let path = workspace.join(".grokforge/mcp.json");
    let workspace_owned = workspace.to_path_buf();
    let path_owned = path.clone();
    let Ok(Ok((text, truncated))) = tokio::task::spawn_blocking(move || {
        crate::path_safety::read_workspace_context_text(
            &workspace_owned,
            &path_owned,
            MAX_MCP_CONFIG_BYTES,
        )
    })
    .await
    else {
        return Vec::new();
    };
    if truncated {
        tracing::warn!(".grokforge/mcp.json is unreadable or exceeds 1 MiB");
        return Vec::new();
    }
    let config: McpConfig = match serde_json::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("invalid .grokforge/mcp.json: {e}");
            return Vec::new();
        }
    };

    let mut connected = Vec::new();
    for (name, spec) in config.servers.into_iter().take(MAX_PROJECT_MCP_SERVERS) {
        match StdioClient::connect_in(&name, &spec.command, &spec.args, workspace).await {
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

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const MOCK_SERVER: &str = r#"
import pathlib, sys, json
pathlib.Path("trusted-mcp-started").touch()
def send(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    message = json.loads(line)
    request_id = message.get("id")
    method = message.get("method")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"mock","version":"0"}}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":request_id,"result":{"tools":[{"name":"echo","description":"echo","inputSchema":{"type":"object"}}]}})
"#;

    fn python_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[tokio::test]
    async fn untrusted_project_config_is_not_executed() {
        let workspace = tempfile::tempdir().unwrap();
        let config_dir = workspace.path().join(".grokforge");
        std::fs::create_dir(&config_dir).unwrap();
        let marker = workspace.path().join("spawned");
        std::fs::write(
            config_dir.join("mcp.json"),
            serde_json::json!({
                "servers": {
                    "malicious": {
                        "command": "/bin/sh",
                        "args": ["-c", format!("touch '{}'", marker.display())]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let mut registry = ToolRegistry::with_builtins();
        assert!(
            connect_and_register(workspace.path(), &mut registry)
                .await
                .is_empty()
        );
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn explicitly_trusted_project_config_is_started_and_registered() {
        if !python_available() {
            eprintln!("skipping: python3 unavailable");
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let config_dir = workspace.path().join(".grokforge");
        std::fs::create_dir(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("mcp.json"),
            serde_json::json!({
                "servers": {
                    "trusted": {
                        "command": "python3",
                        "args": ["-c", MOCK_SERVER]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let mut registry = ToolRegistry::with_builtins();
        let connected = connect_and_register_trusted(workspace.path(), &mut registry).await;

        assert_eq!(connected, ["trusted"]);
        assert!(workspace.path().join("trusted-mcp-started").exists());
        assert!(registry.get("mcp__trusted__echo").is_some());
    }

    #[tokio::test]
    async fn trusted_config_still_refuses_a_symlink_alias() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let config_dir = workspace.path().join(".grokforge");
        std::fs::create_dir(&config_dir).unwrap();
        let marker = workspace.path().join("spawned");
        let target = workspace.path().join("actual-mcp.json");
        std::fs::write(
            &target,
            serde_json::json!({
                "servers": {
                    "malicious": {
                        "command": "/bin/sh",
                        "args": ["-c", format!("touch '{}'", marker.display())]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        symlink(&target, config_dir.join("mcp.json")).unwrap();
        let mut registry = ToolRegistry::with_builtins();
        assert!(
            connect_and_register_trusted(workspace.path(), &mut registry)
                .await
                .is_empty()
        );
        assert!(!marker.exists());
    }
}
