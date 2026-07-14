//! Loads MCP server declarations from `.grokforge/mcp.json`, connects them, and registers their
//! tools into a [`ToolRegistry`]. Format:
//!
//! ```json
//! { "servers": { "docs": { "command": "my-mcp-server", "args": ["--flag"] } } }
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use grokforge_mcp::{McpConnection, StdioClient};
use serde::Deserialize;

use crate::tools::mcp::McpToolAdapter;
use crate::tools::{MAX_TOOLS, ToolRegistry};

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

/// Hard limits for MCP process declarations supplied by an ACP editor. They are public so the
/// protocol frontend can reject oversized JSON arrays before allocating intermediate vectors.
pub const MAX_EDITOR_MCP_SERVERS: usize = 16;
pub const MAX_EDITOR_MCP_ARGS: usize = 64;
pub const MAX_EDITOR_MCP_ENV: usize = 64;
const MAX_EDITOR_MCP_NAME_BYTES: usize = 256;
const MAX_EDITOR_MCP_COMMAND_BYTES: usize = 4096;
const MAX_EDITOR_MCP_ARG_BYTES: usize = 8192;
const MAX_EDITOR_MCP_ARGS_BYTES: usize = 64 * 1024;
const MAX_EDITOR_MCP_ENV_NAME_BYTES: usize = 128;
const MAX_EDITOR_MCP_ENV_VALUE_BYTES: usize = 16 * 1024;
const MAX_EDITOR_MCP_ENV_BYTES: usize = 128 * 1024;
const MAX_EDITOR_MCP_TOOL_METADATA_BYTES: usize = 8 * 1024 * 1024;
const EDITOR_MCP_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// A validated stdio MCP declaration received from an editor through ACP. Fields stay private so
/// every instance must pass the same bounded, transport-specific validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorStdioServer {
    name: String,
    command: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

impl EditorStdioServer {
    /// Validate and copy one official ACP v1 stdio declaration.
    pub fn try_new(
        name: &str,
        command: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Result<Self, EditorMcpError> {
        if name.is_empty()
            || name.len() > MAX_EDITOR_MCP_NAME_BYTES
            || name.chars().any(char::is_control)
        {
            return Err(EditorMcpError::invalid(
                "server name must be a non-empty human-readable string of at most 256 bytes",
            ));
        }
        if command.is_empty()
            || command.len() > MAX_EDITOR_MCP_COMMAND_BYTES
            || command.contains('\0')
        {
            return Err(EditorMcpError::invalid(
                "command must be a non-empty absolute path of at most 4096 bytes",
            ));
        }
        let command = PathBuf::from(command);
        if !command.is_absolute() {
            return Err(EditorMcpError::invalid(
                "command must be an absolute executable path",
            ));
        }
        if args.len() > MAX_EDITOR_MCP_ARGS {
            return Err(EditorMcpError::invalid(format!(
                "a server may have at most {MAX_EDITOR_MCP_ARGS} arguments"
            )));
        }
        let mut args_bytes = 0_usize;
        for argument in args {
            if argument.len() > MAX_EDITOR_MCP_ARG_BYTES || argument.contains('\0') {
                return Err(EditorMcpError::invalid(format!(
                    "each argument must be at most {MAX_EDITOR_MCP_ARG_BYTES} bytes and contain no NUL"
                )));
            }
            args_bytes = args_bytes
                .checked_add(argument.len())
                .ok_or_else(|| EditorMcpError::invalid("argument data is too large"))?;
        }
        if args_bytes > MAX_EDITOR_MCP_ARGS_BYTES {
            return Err(EditorMcpError::invalid(format!(
                "argument data may total at most {MAX_EDITOR_MCP_ARGS_BYTES} bytes"
            )));
        }
        if env.len() > MAX_EDITOR_MCP_ENV {
            return Err(EditorMcpError::invalid(format!(
                "a server may have at most {MAX_EDITOR_MCP_ENV} environment entries"
            )));
        }
        let mut env_bytes = 0_usize;
        let mut env_names = BTreeSet::new();
        for (key, value) in env {
            if !valid_env_name(key) {
                return Err(EditorMcpError::invalid(
                    "environment names must be non-empty and contain neither NUL nor `=`",
                ));
            }
            if key.len() > MAX_EDITOR_MCP_ENV_NAME_BYTES
                || value.len() > MAX_EDITOR_MCP_ENV_VALUE_BYTES
                || value.contains('\0')
            {
                return Err(EditorMcpError::invalid(
                    "environment entry exceeds its byte limit or contains NUL",
                ));
            }
            if !env_names.insert(key.to_ascii_uppercase()) {
                return Err(EditorMcpError::invalid(
                    "environment names must be unique (case-insensitively)",
                ));
            }
            env_bytes = env_bytes
                .checked_add(key.len())
                .and_then(|total| total.checked_add(value.len()))
                .ok_or_else(|| EditorMcpError::invalid("environment data is too large"))?;
        }
        if env_bytes > MAX_EDITOR_MCP_ENV_BYTES {
            return Err(EditorMcpError::invalid(format!(
                "environment data may total at most {MAX_EDITOR_MCP_ENV_BYTES} bytes"
            )));
        }

        Ok(Self {
            name: name.to_string(),
            command,
            args: args.iter().map(|value| (*value).to_string()).collect(),
            env: env
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
        })
    }
}

fn valid_env_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['\0', '='])
}

/// Failures from the editor-supplied MCP trust boundary.
#[derive(Debug, thiserror::Error)]
pub enum EditorMcpError {
    #[error("invalid editor MCP configuration: {0}")]
    Invalid(String),
    #[error("editor MCP `{server}` failed to start: {source}")]
    Start {
        server: String,
        #[source]
        source: grokforge_mcp::McpError,
    },
    #[error("editor MCP `{server}` tool discovery failed: {source}")]
    Discovery {
        server: String,
        #[source]
        source: grokforge_mcp::McpError,
    },
    #[error("editor MCP tool registry rejected the discovered tools: {0}")]
    Registry(String),
    #[error("editor MCP startup exceeded the 60-second session deadline")]
    StartupTimeout,
}

impl EditorMcpError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    /// Whether the client request itself was malformed (JSON-RPC `invalid params`) rather than a
    /// valid local server failing at runtime.
    #[must_use]
    pub fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid(_))
    }
}

struct ResolvedEditorServer {
    name: String,
    command: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

struct PreparedEditorServer {
    name: String,
    connection: Arc<dyn McpConnection>,
    tools: Vec<grokforge_mcp::McpTool>,
}

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

/// Start editor-supplied ACP stdio MCP servers and atomically register all discovered tools.
///
/// The complete declaration set and every executable path are validated before any process is
/// spawned. Connections and tool lists are then prepared without mutating `registry`; if any
/// server fails, dropping the prepared connections kills their process groups and leaves the
/// registry unchanged.
pub async fn connect_and_register_editor(
    workspace: &Path,
    servers: Vec<EditorStdioServer>,
    registry: &mut ToolRegistry,
) -> Result<Vec<String>, EditorMcpError> {
    let (workspace, resolved) = resolve_editor_servers(workspace, servers)?;
    let available_tools = MAX_TOOLS.saturating_sub(registry.specs().len());
    let prepared = tokio::time::timeout(
        EDITOR_MCP_STARTUP_TIMEOUT,
        prepare_editor_servers(&workspace, resolved, available_tools),
    )
    .await
    .map_err(|_| EditorMcpError::StartupTimeout)??;
    preflight_editor_tools(&prepared, registry)?;
    Ok(register_editor_tools(prepared, registry))
}

fn resolve_editor_servers(
    workspace: &Path,
    servers: Vec<EditorStdioServer>,
) -> Result<(PathBuf, Vec<ResolvedEditorServer>), EditorMcpError> {
    if servers.len() > MAX_EDITOR_MCP_SERVERS {
        return Err(EditorMcpError::invalid(format!(
            "at most {MAX_EDITOR_MCP_SERVERS} editor MCP servers are allowed"
        )));
    }

    let workspace = std::fs::canonicalize(workspace)
        .map_err(|_| EditorMcpError::invalid("workspace cwd could not be canonicalized"))?;
    if !workspace.is_absolute() || !workspace.is_dir() {
        return Err(EditorMcpError::invalid(
            "workspace cwd must be a canonical directory",
        ));
    }

    let mut names = BTreeSet::new();
    let mut resolved = Vec::with_capacity(servers.len());
    for server in servers {
        if !names.insert(server.name.clone()) {
            return Err(EditorMcpError::invalid(
                "editor MCP server names must be unique",
            ));
        }
        let command = std::fs::canonicalize(&server.command).map_err(|_| {
            EditorMcpError::invalid(format!(
                "editor MCP `{}` command does not resolve to a local executable file",
                server.name
            ))
        })?;
        if !command.is_absolute() || !is_executable_file(&command) {
            return Err(EditorMcpError::invalid(format!(
                "editor MCP `{}` command does not resolve to a local executable file",
                server.name
            )));
        }
        resolved.push(ResolvedEditorServer {
            name: server.name,
            command,
            args: server.args,
            env: server.env,
        });
    }
    Ok((workspace, resolved))
}

async fn prepare_editor_servers(
    workspace: &Path,
    resolved: Vec<ResolvedEditorServer>,
    available_tools: usize,
) -> Result<Vec<PreparedEditorServer>, EditorMcpError> {
    let mut prepared = Vec::with_capacity(resolved.len());
    let mut discovered_tools = 0_usize;
    let mut metadata_bytes = 0_usize;
    for server in resolved {
        let client = StdioClient::connect_in_with_env(
            &server.name,
            &server.command,
            &server.args,
            workspace,
            &server.env,
        )
        .await
        .map_err(|source| EditorMcpError::Start {
            server: server.name.clone(),
            source,
        })?;
        let connection: Arc<dyn McpConnection> = Arc::new(client);
        let tools = connection
            .list_tools()
            .await
            .map_err(|source| EditorMcpError::Discovery {
                server: server.name.clone(),
                source,
            })?;
        discovered_tools = discovered_tools.saturating_add(tools.len());
        if discovered_tools > available_tools {
            return Err(EditorMcpError::Registry(format!(
                "editor MCP tools exceed the {available_tools}-tool capacity remaining in this session"
            )));
        }
        for tool in &tools {
            let schema_bytes = serde_json::to_vec(&tool.input_schema)
                .map_err(|error| EditorMcpError::Registry(error.to_string()))?
                .len();
            metadata_bytes = metadata_bytes
                .checked_add(tool.name.len())
                .and_then(|total| total.checked_add(tool.description.len()))
                .and_then(|total| total.checked_add(schema_bytes))
                .ok_or_else(|| {
                    EditorMcpError::Registry("editor MCP tool metadata is too large".to_string())
                })?;
            if metadata_bytes > MAX_EDITOR_MCP_TOOL_METADATA_BYTES {
                return Err(EditorMcpError::Registry(format!(
                    "editor MCP tool metadata exceeds {MAX_EDITOR_MCP_TOOL_METADATA_BYTES} bytes across the session"
                )));
            }
        }
        prepared.push(PreparedEditorServer {
            name: server.name,
            connection,
            tools,
        });
    }
    Ok(prepared)
}

fn preflight_editor_tools(
    prepared: &[PreparedEditorServer],
    registry: &ToolRegistry,
) -> Result<(), EditorMcpError> {
    // Preflight provider limits and all qualified-name collisions before the first registration.
    let mut tool_names = BTreeSet::new();
    for server in prepared {
        for tool in &server.tools {
            let qualified = McpToolAdapter::advertised_name(&server.name, &tool.name);
            if registry.get(&qualified).is_some() || !tool_names.insert(qualified) {
                return Err(EditorMcpError::Registry(
                    "tools contain a duplicate provider-facing name after redaction".to_string(),
                ));
            }
        }
    }
    if registry.specs().len().saturating_add(tool_names.len()) > MAX_TOOLS {
        return Err(EditorMcpError::Registry(format!(
            "tools exceed GrokForge's {MAX_TOOLS}-tool session limit"
        )));
    }
    Ok(())
}

fn register_editor_tools(
    prepared: Vec<PreparedEditorServer>,
    registry: &mut ToolRegistry,
) -> Vec<String> {
    let connected = prepared.iter().map(|server| server.name.clone()).collect();
    for server in prepared {
        for tool in server.tools {
            registry.register(Arc::new(McpToolAdapter::new(
                server.name.clone(),
                tool,
                Arc::clone(&server.connection),
            )));
        }
    }
    connected
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
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
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"0"}}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":request_id,"result":{"tools":[{"name":"echo","description":"echo","inputSchema":{"type":"object"}}]}})
"#;

    fn python_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn python_executable() -> Option<PathBuf> {
        let output = std::process::Command::new("python3")
            .args(["-c", "import sys; print(sys.executable)"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let path = String::from_utf8(output.stdout).ok()?;
        std::fs::canonicalize(path.trim()).ok()
    }

    #[test]
    fn editor_stdio_declarations_are_strictly_bounded() {
        assert!(EditorStdioServer::try_new("ok", "relative", &[], &[]).is_err());
        assert!(EditorStdioServer::try_new("Project tools 🛠", "/bin/sh", &[], &[]).is_ok());
        assert!(EditorStdioServer::try_new("bad\nname", "/bin/sh", &[], &[]).is_err());

        let too_many_args = vec!["x"; MAX_EDITOR_MCP_ARGS + 1];
        assert!(EditorStdioServer::try_new("args", "/bin/sh", &too_many_args, &[]).is_err());
        let oversized_arg = "x".repeat(MAX_EDITOR_MCP_ARG_BYTES + 1);
        assert!(EditorStdioServer::try_new("args", "/bin/sh", &[&oversized_arg], &[]).is_err());
        assert!(
            EditorStdioServer::try_new(
                "env",
                "/bin/sh",
                &[],
                &[("TOKEN", "one"), ("token", "two")],
            )
            .is_err()
        );
        assert!(
            EditorStdioServer::try_new(
                "env",
                "/bin/sh",
                &[],
                &[("BAD-NAME", "x"), ("name.with.dot", "y")],
            )
            .is_ok()
        );
        assert!(EditorStdioServer::try_new("env", "/bin/sh", &[], &[("", "x")]).is_err());
        assert!(EditorStdioServer::try_new("env", "/bin/sh", &[], &[("BAD=NAME", "x")]).is_err());
        let oversized_value = "x".repeat(MAX_EDITOR_MCP_ENV_VALUE_BYTES + 1);
        assert!(
            EditorStdioServer::try_new("env", "/bin/sh", &[], &[("VALUE", &oversized_value)])
                .is_err()
        );
    }

    #[tokio::test]
    async fn editor_bridge_validates_every_executable_before_spawning_any() {
        let workspace = tempfile::tempdir().unwrap();
        let marker = workspace.path().join("must-not-exist");
        let command = format!("touch '{}'", marker.display());
        let first = EditorStdioServer::try_new("first", "/bin/sh", &["-c", &command], &[]).unwrap();
        let second =
            EditorStdioServer::try_new("missing", "/definitely/not/a/real/grokforge-mcp", &[], &[])
                .unwrap();
        let mut registry = ToolRegistry::with_builtins();

        let error =
            connect_and_register_editor(workspace.path(), vec![first, second], &mut registry)
                .await
                .unwrap_err();

        assert!(error.is_invalid());
        assert!(!marker.exists(), "an earlier declaration was spawned");
    }

    #[tokio::test]
    async fn editor_bridge_does_not_partially_register_when_a_later_server_fails() {
        let Some(python) = python_executable() else {
            eprintln!("skipping: python3 unavailable");
            return;
        };
        let workspace = tempfile::tempdir().unwrap();
        let good_script = r#"
import json, sys
def send(value): sys.stdout.write(json.dumps(value) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    message=json.loads(line); request_id=message.get("id"); method=message.get("method")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"first","version":"0"}}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":request_id,"result":{"tools":[{"name":"ready","inputSchema":{"type":"object"}}]}})
"#;
        let python = python.to_string_lossy().into_owned();
        let first =
            EditorStdioServer::try_new("first", &python, &["-c", good_script], &[]).unwrap();
        let second =
            EditorStdioServer::try_new("broken", &python, &["-c", "import sys; sys.exit(0)"], &[])
                .unwrap();
        let mut registry = ToolRegistry::with_builtins();
        let initial_tool_count = registry.specs().len();

        let error =
            connect_and_register_editor(workspace.path(), vec![first, second], &mut registry)
                .await
                .unwrap_err();

        assert!(matches!(error, EditorMcpError::Start { .. }));
        assert_eq!(registry.specs().len(), initial_tool_count);
        assert!(registry.get("mcp__first__ready").is_none());
    }

    #[tokio::test]
    async fn editor_bridge_uses_canonical_cwd_explicit_env_and_registers_tools() {
        let Some(python) = python_executable() else {
            eprintln!("skipping: python3 unavailable");
            return;
        };
        let workspace = tempfile::tempdir().unwrap();
        let script = r#"
import json, os, pathlib, sys
pathlib.Path("editor-mcp-cwd").write_text(os.getcwd())
def send(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    message = json.loads(line)
    request_id = message.get("id")
    method = message.get("method")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"editor","version":"0"}}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":request_id,"result":{"tools":[{"name":os.environ["GF_EDITOR_TOOL"],"description":"editor env","inputSchema":{"type":"object"}}]}})
"#;
        let python = python.to_string_lossy().into_owned();
        let server = EditorStdioServer::try_new(
            "editor",
            &python,
            &["-c", script],
            &[("GF_EDITOR_TOOL", "from_editor")],
        )
        .unwrap();
        let mut registry = ToolRegistry::with_builtins();

        let connected = connect_and_register_editor(workspace.path(), vec![server], &mut registry)
            .await
            .unwrap();

        assert_eq!(connected, ["editor"]);
        assert!(registry.get("mcp__editor__from_editor").is_some());
        let observed_cwd =
            std::fs::read_to_string(workspace.path().join("editor-mcp-cwd")).unwrap();
        assert_eq!(
            PathBuf::from(observed_cwd),
            std::fs::canonicalize(workspace.path()).unwrap()
        );
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
