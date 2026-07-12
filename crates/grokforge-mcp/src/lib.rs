//! `grokforge-mcp` — a minimal Model Context Protocol client over the stdio transport, behind an
//! internal [`McpConnection`] trait so the rest of GrokForge never depends on the wire details.
//!
//! We hand-roll a small JSON-RPC 2.0 client (newline-delimited) rather than pin an unverified
//! `rmcp` version; the trait is the seam where a fuller SDK could slot in later. Requests are
//! serialized (one in flight at a time), which is plenty for tool discovery and calls.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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
    #[error("tool reported an error: {0}")]
    Tool(String),
    #[error("MCP request `{method}` timed out after {timeout:?}")]
    Timeout { method: String, timeout: Duration },
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
    reader: BufReader<ChildStdout>,
    next_id: u64,
}

const MAX_JSON_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOOL_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_TOOL_LIST_PAGES: usize = 64;
const MAX_DISCOVERED_TOOLS: usize = 256;

/// An MCP server reached over stdio.
pub struct StdioClient {
    // Declared before `Child` so Rust drops the group guard first; descendants are killed while
    // the leader PID is still owned and cannot be recycled for an unrelated process group.
    process_group: ProcessGroupGuard,
    io: Mutex<Io>,
    _child: Child,
    name: String,
    request_timeout: Duration,
}

/// Default bound for initialization, discovery, and tool calls. A broken local server must not
/// freeze GrokForge startup or an agent turn indefinitely.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

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
        Self::connect_with_options(name, command, args, None, DEFAULT_REQUEST_TIMEOUT).await
    }

    /// Connect with an explicit working directory. The child receives a minimal, scrubbed
    /// environment rather than API keys and other credentials from the agent process.
    pub async fn connect_in(
        name: &str,
        command: &str,
        args: &[String],
        cwd: &Path,
    ) -> Result<Self, McpError> {
        Self::connect_with_options(name, command, args, Some(cwd), DEFAULT_REQUEST_TIMEOUT).await
    }

    /// Connect with explicit process options. Public primarily for deterministic embedding and
    /// timeout tests; normal callers should use [`Self::connect`] or [`Self::connect_in`].
    pub async fn connect_with_options(
        name: &str,
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
        request_timeout: Duration,
    ) -> Result<Self, McpError> {
        if command.trim().is_empty() {
            return Err(McpError::Spawn("<empty command>".to_string()));
        }
        let mut process = tokio::process::Command::new(command);
        process
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .env_clear();
        // Retain only variables needed to locate/runtime-link ordinary executables. Credentials,
        // cloud profiles, and the xAI key are deliberately absent.
        for key in [
            "PATH",
            "PATHEXT",
            "SystemRoot",
            "WINDIR",
            "TMPDIR",
            "TEMP",
            "TMP",
            "LANG",
            "LC_ALL",
        ] {
            if let Some(value) = std::env::var_os(key) {
                process.env(key, value);
            }
        }
        if let Some(cwd) = cwd {
            process.current_dir(cwd);
        }
        #[cfg(unix)]
        process.process_group(0);
        let mut child = process
            .spawn()
            .map_err(|_| McpError::Spawn(command.to_string()))?;
        let process_group = ProcessGroupGuard::new(child.id());

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Protocol("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Protocol("no stdout".into()))?;
        let reader = BufReader::new(stdout);

        let client = Self {
            process_group,
            io: Mutex::new(Io {
                stdin,
                reader,
                next_id: 1,
            }),
            _child: child,
            name: name.to_string(),
            request_timeout,
        };
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&self) -> Result<(), McpError> {
        let params = json!({
            "protocolVersion": LATEST_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "grokforge", "version": env!("CARGO_PKG_VERSION") }
        });
        let result = self.request("initialize", params).await?;
        let protocol = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| McpError::Protocol("initialize omitted protocolVersion".into()))?;
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&protocol) {
            return Err(McpError::Protocol(format!(
                "server negotiated unsupported protocol version `{protocol}`"
            )));
        }
        if !result.get("capabilities").is_some_and(Value::is_object)
            || !result.get("serverInfo").is_some_and(Value::is_object)
        {
            return Err(McpError::Protocol(
                "initialize omitted required capabilities or serverInfo".into(),
            ));
        }
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let mut guard = RequestDropGuard::new(&self.process_group);
        let result = tokio::time::timeout(self.request_timeout, async {
            let mut io = self.io.lock().await;
            write_line(&mut io.stdin, &msg).await
        })
        .await;
        if let Ok(result) = result {
            guard.disarm();
            result
        } else {
            self.process_group.kill();
            guard.disarm();
            Err(McpError::Timeout {
                method: method.to_string(),
                timeout: self.request_timeout,
            })
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let mut guard = RequestDropGuard::new(&self.process_group);
        let result =
            tokio::time::timeout(self.request_timeout, self.request_inner(method, params)).await;
        if let Ok(result) = result {
            guard.disarm();
            result
        } else {
            self.process_group.kill();
            guard.disarm();
            Err(McpError::Timeout {
                method: method.to_string(),
                timeout: self.request_timeout,
            })
        }
    }

    async fn request_inner(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let mut io = self.io.lock().await;
        let id = io.next_id;
        io.next_id += 1;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        write_line(&mut io.stdin, &msg).await?;

        // Read until the matching response; skip notifications / unrelated messages.
        loop {
            let Some(line) = read_json_line(&mut io.reader).await? else {
                return Err(McpError::Protocol("server closed the stream".into()));
            };
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line)?;
            if value.get("method").is_some() && value.get("id").is_some() {
                // Client and server request ids occupy independent namespaces. A server request
                // may legitimately reuse our in-flight numeric id; capabilities are empty, so
                // answer it explicitly instead of mistaking it for our response.
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": value.get("id").cloned().unwrap_or(Value::Null),
                    "error": { "code": -32601, "message": "client method not supported" }
                });
                write_line(&mut io.stdin, &response).await?;
                continue;
            }
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = value.get("error") {
                    return Err(McpError::Rpc(err.to_string()));
                }
                if let Some(result) = value.get("result") {
                    return Ok(result.clone());
                }
                return Err(McpError::Protocol(
                    "matching JSON-RPC response omitted both result and error".into(),
                ));
            }
        }
    }
}

async fn read_json_line(reader: &mut BufReader<ChildStdout>) -> Result<Option<String>, McpError> {
    let mut bytes = Vec::new();
    let read = reader
        .take((MAX_JSON_LINE_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .await?;
    if read == 0 {
        return Ok(None);
    }
    if bytes.len() > MAX_JSON_LINE_BYTES || !bytes.ends_with(b"\n") {
        return Err(McpError::Protocol(format!(
            "MCP message exceeded {MAX_JSON_LINE_BYTES} bytes or was not newline-delimited"
        )));
    }
    bytes.pop();
    if bytes.ends_with(b"\r") {
        bytes.pop();
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| McpError::Protocol("MCP message was not valid UTF-8".into()))
}

async fn write_line(stdin: &mut ChildStdin, msg: &Value) -> Result<(), McpError> {
    let line = serialize_line(msg)?;
    stdin.write_all(&line).await?;
    stdin.flush().await?;
    Ok(())
}

fn serialize_line(msg: &Value) -> Result<Vec<u8>, McpError> {
    struct CappedLine {
        bytes: Vec<u8>,
        exceeded: bool,
    }

    impl std::io::Write for CappedLine {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self
                .bytes
                .len()
                .checked_add(bytes.len())
                .is_none_or(|length| length >= MAX_JSON_LINE_BYTES)
            {
                self.exceeded = true;
                return Err(std::io::Error::other("MCP request exceeds byte cap"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut line = CappedLine {
        bytes: Vec::new(),
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut line, msg) {
        if line.exceeded {
            return Err(McpError::Protocol(format!(
                "MCP request exceeded {MAX_JSON_LINE_BYTES} bytes"
            )));
        }
        return Err(McpError::Decode(error));
    }
    line.bytes.push(b'\n');
    Ok(line.bytes)
}

#[async_trait]
impl McpConnection for StdioClient {
    async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
        #[derive(Deserialize)]
        struct ToolsResult {
            #[serde(default)]
            tools: Vec<McpTool>,
            #[serde(default, rename = "nextCursor")]
            next_cursor: Option<String>,
        }
        let operation = async {
            let mut tools = Vec::new();
            let mut cursor: Option<String> = None;
            for _page in 0..MAX_TOOL_LIST_PAGES {
                let params = cursor
                    .as_ref()
                    .map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
                let result = self.request("tools/list", params).await?;
                let parsed: ToolsResult = serde_json::from_value(result)?;
                if tools.len().saturating_add(parsed.tools.len()) > MAX_DISCOVERED_TOOLS {
                    return Err(McpError::Protocol(format!(
                        "tools/list exceeded the {MAX_DISCOVERED_TOOLS}-tool discovery cap"
                    )));
                }
                tools.extend(parsed.tools);
                match parsed.next_cursor {
                    Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                    Some(_) => {
                        return Err(McpError::Protocol(
                            "tools/list returned the same pagination cursor repeatedly".into(),
                        ));
                    }
                    None => return Ok(tools),
                }
            }
            Err(McpError::Protocol(format!(
                "tools/list exceeded the {MAX_TOOL_LIST_PAGES}-page discovery cap"
            )))
        };
        tokio::time::timeout(self.request_timeout, operation)
            .await
            .map_err(|_| McpError::Timeout {
                method: "tools/list pagination".to_string(),
                timeout: self.request_timeout,
            })?
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<String, McpError> {
        let result = self
            .request("tools/call", json!({ "name": name, "arguments": args }))
            .await?;
        let mut out = content_text(&result);
        if out.is_empty() {
            out = bounded_text(&result.to_string(), MAX_TOOL_OUTPUT_BYTES);
        }
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            Err(McpError::Tool(out))
        } else {
            Ok(out)
        }
    }
}

fn content_text(result: &Value) -> String {
    let mut out = String::new();
    let mut truncated = false;
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        for block in blocks {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                if !out.is_empty() {
                    truncated |= !push_bounded(&mut out, "\n", MAX_TOOL_OUTPUT_BYTES);
                }
                truncated |= !push_bounded(&mut out, text, MAX_TOOL_OUTPUT_BYTES);
                if truncated {
                    break;
                }
            }
        }
    }
    if truncated {
        append_truncation_marker(&mut out, MAX_TOOL_OUTPUT_BYTES);
    }
    out
}

fn bounded_text(text: &str, max_bytes: usize) -> String {
    let mut out = String::new();
    if !push_bounded(&mut out, text, max_bytes) {
        append_truncation_marker(&mut out, max_bytes);
    }
    out
}

fn append_truncation_marker(out: &mut String, max_bytes: usize) {
    const MARKER: &str = "\n… [MCP tool output truncated] …";
    let keep = max_bytes.saturating_sub(MARKER.len());
    while out.len() > keep {
        out.pop();
    }
    if max_bytes >= MARKER.len() {
        out.push_str(MARKER);
    }
}

fn push_bounded(out: &mut String, text: &str, max_bytes: usize) -> bool {
    let remaining = max_bytes.saturating_sub(out.len());
    if text.len() <= remaining {
        out.push_str(text);
        return true;
    }
    let mut end = remaining;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    out.push_str(&text[..end]);
    false
}

#[cfg(unix)]
fn kill_process_group(id: u32) {
    let _ = std::process::Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{id}")])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

struct ProcessGroupGuard {
    #[cfg(unix)]
    id: Option<u32>,
}

impl ProcessGroupGuard {
    fn new(id: Option<u32>) -> Self {
        #[cfg(unix)]
        {
            Self { id }
        }
        #[cfg(not(unix))]
        {
            let _ = id;
            Self {}
        }
    }

    fn kill(&self) {
        #[cfg(unix)]
        if let Some(id) = self.id {
            kill_process_group(id);
        }
        #[cfg(not(unix))]
        let _ = self;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

struct RequestDropGuard<'a> {
    group: &'a ProcessGroupGuard,
    armed: bool,
}

impl<'a> RequestDropGuard<'a> {
    fn new(group: &'a ProcessGroupGuard) -> Self {
        Self { group, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RequestDropGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.group.kill();
        }
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
        if args.get("text") == "fail":
            send({"jsonrpc":"2.0","id":mid,"result":{"isError":True,"content":[{"type":"text","text":"permission denied"}]}})
        else:
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

        let error = client
            .call_tool("echo", json!({ "text": "fail" }))
            .await
            .unwrap_err();
        assert!(matches!(error, McpError::Tool(message) if message == "permission denied"));
    }

    #[tokio::test]
    async fn initialization_timeout_is_bounded() {
        if !python_available() {
            eprintln!("skipping: python3 unavailable");
            return;
        }
        let script = "import time; time.sleep(60)";
        let error = StdioClient::connect_with_options(
            "hung",
            "python3",
            &["-c".into(), script.into()],
            None,
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, McpError::Timeout { .. }));
    }

    #[tokio::test]
    async fn same_id_server_request_is_not_mistaken_for_initialize_response() {
        if !python_available() {
            eprintln!("skipping: python3 unavailable");
            return;
        }
        let script = r#"
import sys, json
def send(o): sys.stdout.write(json.dumps(o)+"\n"); sys.stdout.flush()
for line in sys.stdin:
    msg=json.loads(line); mid=msg.get("id"); method=msg.get("method")
    if method=="initialize":
        send({"jsonrpc":"2.0","id":mid,"method":"roots/list","params":{}})
        client_reply=json.loads(sys.stdin.readline())
        if client_reply.get("error",{}).get("code") != -32601: sys.exit(3)
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"interleave","version":"0"}}})
    elif method=="notifications/initialized": pass
"#;
        let client = StdioClient::connect("interleave", "python3", &["-c".into(), script.into()])
            .await
            .unwrap();
        drop(client);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn initialization_timeout_kills_server_process_group() {
        if !python_available() {
            eprintln!("skipping: python3 unavailable");
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("grandchild-survived");
        let script = r#"
import subprocess, sys, time
marker=sys.argv[1]
child="import sys,time; time.sleep(0.7); open(sys.argv[1],'w').write('survived')"
subprocess.Popen([sys.executable, "-c", child, marker])
time.sleep(60)
"#;
        let error = StdioClient::connect_with_options(
            "group",
            "python3",
            &[
                "-c".into(),
                script.into(),
                marker.to_string_lossy().into_owned(),
            ],
            None,
            Duration::from_millis(250),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, McpError::Timeout { .. }));
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(!marker.exists(), "MCP grandchild escaped client drop");
    }

    #[test]
    fn tool_error_result_is_not_success_text() {
        let value = json!({
            "isError": true,
            "content": [{"type":"text", "text":"permission denied"}]
        });
        assert_eq!(content_text(&value), "permission denied");
    }

    #[test]
    fn outgoing_requests_and_rendered_tool_output_are_bounded() {
        let request = json!({"arguments": "x".repeat(MAX_JSON_LINE_BYTES)});
        assert!(matches!(
            serialize_line(&request),
            Err(McpError::Protocol(_))
        ));

        let value = json!({
            "content": [{"type":"text", "text":"x".repeat(MAX_TOOL_OUTPUT_BYTES + 100)}]
        });
        let output = content_text(&value);
        assert!(output.len() <= MAX_TOOL_OUTPUT_BYTES);
        assert!(output.contains("MCP tool output truncated"));
    }

    #[tokio::test]
    async fn tool_pagination_has_a_hard_page_cap() {
        if !python_available() {
            eprintln!("skipping: python3 unavailable");
            return;
        }
        let server = r#"
import sys, json
def send(o): sys.stdout.write(json.dumps(o)+"\n"); sys.stdout.flush()
for line in sys.stdin:
    msg=json.loads(line); mid=msg.get("id"); method=msg.get("method")
    if method=="initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"pages","version":"0"}}})
    elif method=="notifications/initialized": pass
    elif method=="tools/list":
        cursor=int(msg.get("params",{}).get("cursor","0"))
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[],"nextCursor":str(cursor+1)}})
"#;
        let client = StdioClient::connect("pages", "python3", &["-c".into(), server.into()])
            .await
            .unwrap();
        let error = client.list_tools().await.unwrap_err();
        assert!(matches!(error, McpError::Protocol(message) if message.contains("page")));
    }
}
