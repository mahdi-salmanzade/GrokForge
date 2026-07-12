//! `grokforge-mcp` — a minimal Model Context Protocol client over the stdio transport, behind an
//! internal [`McpConnection`] trait so the rest of GrokForge never depends on the wire details.
//!
//! We hand-roll a small JSON-RPC 2.0 client (newline-delimited) rather than pin an unverified
//! `rmcp` version; the trait is the seam where a fuller SDK could slot in later. Requests are
//! serialized (one in flight at a time), which is plenty for tool discovery and calls.

use std::process::Stdio;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

/// Errors from talking to an MCP server.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("failed to spawn MCP server `{0}`")]
    Spawn(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("server returned error: {0}")]
    Rpc(String),
    #[error("decode error: {0}")]
    Decode(#[from] serde_json::Error),
}

/// A tool advertised by an MCP server.
#[derive(Debug, Clone, Deserialize)]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

/// A connection to an MCP server.
#[async_trait]
pub trait McpConnection: Send + Sync + std::fmt::Debug {
    /// List the tools the server offers.
    async fn list_tools(&self) -> Result<Vec<McpTool>, McpError>;
    /// Call a tool and return its textual result.
    async fn call_tool(&self, name: &str, args: Value) -> Result<String, McpError>;
}

struct Io {
    stdin: ChildStdin,
    reader: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

/// An MCP server reached over stdio.
pub struct StdioClient {
    io: Mutex<Io>,
    _child: Child,
    name: String,
}

impl std::fmt::Debug for StdioClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioClient")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl StdioClient {
    /// Spawn `command args...` as an MCP server and complete the initialize handshake.
    pub async fn connect(name: &str, command: &str, args: &[String]) -> Result<Self, McpError> {
        let mut child = tokio::process::Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| McpError::Spawn(command.to_string()))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Protocol("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Protocol("no stdout".into()))?;
        let reader = BufReader::new(stdout).lines();

        let client = Self {
            io: Mutex::new(Io {
                stdin,
                reader,
                next_id: 1,
            }),
            _child: child,
            name: name.to_string(),
        };
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&self) -> Result<(), McpError> {
        let params = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "grokforge", "version": env!("CARGO_PKG_VERSION") }
        });
        self.request("initialize", params).await?;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let mut io = self.io.lock().await;
        write_line(&mut io.stdin, &msg).await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let mut io = self.io.lock().await;
        let id = io.next_id;
        io.next_id += 1;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        write_line(&mut io.stdin, &msg).await?;

        // Read until the matching response; skip notifications / unrelated messages.
        loop {
            let Some(line) = io.reader.next_line().await? else {
                return Err(McpError::Protocol("server closed the stream".into()));
            };
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line)?;
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = value.get("error") {
                    return Err(McpError::Rpc(err.to_string()));
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }
}

async fn write_line(stdin: &mut ChildStdin, msg: &Value) -> Result<(), McpError> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

#[async_trait]
impl McpConnection for StdioClient {
    async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
        #[derive(Deserialize)]
        struct ToolsResult {
            #[serde(default)]
            tools: Vec<McpTool>,
        }
        let result = self.request("tools/list", json!({})).await?;
        let parsed: ToolsResult = serde_json::from_value(result)?;
        Ok(parsed.tools)
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<String, McpError> {
        let result = self
            .request("tools/call", json!({ "name": name, "arguments": args }))
            .await?;
        // result.content is an array of content blocks; concatenate the text ones.
        let mut out = String::new();
        if let Some(blocks) = result.get("content").and_then(Value::as_array) {
            for block in blocks {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                }
            }
        }
        if out.is_empty() {
            out = result.to_string();
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A tiny in-process MCP server script (python) used to exercise the client.
    const MOCK_SERVER: &str = r#"
import sys, json
def send(o): sys.stdout.write(json.dumps(o)+"\n"); sys.stdout.flush()
for line in sys.stdin:
    line=line.strip()
    if not line: continue
    msg=json.loads(line)
    mid=msg.get("id"); method=msg.get("method")
    if method=="initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"mock","version":"0"}}})
    elif method=="notifications/initialized":
        pass
    elif method=="tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"echo","description":"echo text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}})
    elif method=="tools/call":
        args=msg["params"]["arguments"]
        send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"echoed: "+args.get("text","")}]}})
    else:
        send({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"unknown"}})
"#;

    fn python_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[tokio::test]
    async fn lists_and_calls_tools_over_stdio() {
        if !python_available() {
            eprintln!("skipping: python3 unavailable");
            return;
        }
        let client = StdioClient::connect("python3", "python3", &["-c".into(), MOCK_SERVER.into()])
            .await
            .expect("connect");

        let tools = client.list_tools().await.expect("list");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        let out = client
            .call_tool("echo", json!({ "text": "hi there" }))
            .await
            .expect("call");
        assert_eq!(out, "echoed: hi there");
    }
}
