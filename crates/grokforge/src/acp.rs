//! ACP (Agent Client Protocol) frontend: `grokforge acp`.
//!
//! Speaks JSON-RPC 2.0 over newline-delimited stdio so an editor (Zed and other ACP clients) can
//! embed GrokForge as an agent. This is an additive frontend over the same `Op`/`Event` seam the
//! headless frontend uses (ADR 0005): no core rearchitecting. Diagnostics go to stderr; stdout
//! carries only protocol messages.
//!
//! Implemented (protocol version 1): `initialize`, `session/new`, `session/prompt` (streaming
//! `session/update` notifications + a `stopReason` response), `session/cancel`, and
//! `session/request_permission` (agent→client) bridged from the core approval engine. Credentials
//! come from `XAI_API_KEY` because stdin is the protocol channel (no password prompt). Session
//! persistence, `session/load`, and client `fs`/`terminal` calls are intentionally deferred.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use async_trait::async_trait;
use grokforge_core::{
    Agent, Approver, RolloutWriter, Session, SessionConfig, ToolRegistry, TurnCancellation,
};
use grokforge_protocol::{ApprovalRequest, Decision, EventMsg, StopReason};
use grokforge_xai::XaiClient;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};

const PROTOCOL_VERSION: i64 = 1;

/// Run the ACP agent server over stdio until the client closes stdin.
pub async fn run(trust_project_mcp: bool) -> ExitCode {
    // stdin is the JSON-RPC channel, so a password prompt is impossible: require `XAI_API_KEY`.
    let api_key = match std::env::var("XAI_API_KEY") {
        Ok(key) if !key.trim().is_empty() => key,
        _ => {
            eprintln!(
                "grokforge acp: set XAI_API_KEY in the agent's environment (stdin is the ACP channel, so interactive sign-in is unavailable)"
            );
            return ExitCode::from(3);
        }
    };
    let base_url = std::env::var("XAI_BASE_URL").unwrap_or_else(|_| "https://api.x.ai".to_string());
    let client = match XaiClient::new(&base_url, api_key) {
        Ok(client) => client,
        Err(error) => {
            eprintln!(
                "grokforge acp: client error: {}",
                crate::sanitize_terminal(&error.to_string())
            );
            return ExitCode::from(2);
        }
    };
    let model = "grok-build-0.1".to_string();
    if let Err(code) = crate::validate_model_startup(&client, &model).await {
        return code;
    }
    let context_window_tokens = client.model_context_window(&model).await;

    // Writer task: every outgoing message is one JSON line on stdout.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(message) = out_rx.recv().await {
            let Ok(mut line) = serde_json::to_vec(&message) else {
                continue;
            };
            line.push(b'\n');
            if stdout.write_all(&line).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    let connection = Arc::new(Connection {
        shared: Arc::new(Shared {
            out_tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
        }),
        sessions: Mutex::new(HashMap::new()),
        cancels: Mutex::new(HashMap::new()),
        client,
        model,
        context_window_tokens,
        trust_project_mcp,
    });

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Handle each message on its own task so a long `session/prompt` never blocks reading the
        // `session/cancel` or `session/request_permission` responses that must interleave with it.
        let connection = Arc::clone(&connection);
        tokio::spawn(async move { connection.handle(message).await });
    }
    drop(connection);
    let _ = writer.await;
    ExitCode::SUCCESS
}

/// The outgoing side plus outstanding agent→client requests. Held by both the [`Connection`] and
/// every [`AcpApprover`] without a reference cycle back to the session map.
struct Shared {
    out_tx: mpsc::UnboundedSender<Value>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    next_id: AtomicI64,
}

impl Shared {
    fn send(&self, message: Value) {
        let _ = self.out_tx.send(message);
    }

    /// Send an agent→client request and await its response message (or `None` if the connection
    /// closes first).
    async fn request(&self, method: &str, params: Value) -> Option<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        rx.await.ok()
    }

    async fn resolve(&self, id: i64, message: Value) {
        if let Some(tx) = self.pending.lock().await.remove(&id) {
            let _ = tx.send(message);
        }
    }
}

struct Connection {
    shared: Arc<Shared>,
    sessions: Mutex<HashMap<String, Arc<Mutex<SessionEntry>>>>,
    cancels: Mutex<HashMap<String, TurnCancellation>>,
    client: XaiClient,
    model: String,
    context_window_tokens: Option<u64>,
    trust_project_mcp: bool,
}

struct SessionEntry {
    agent: Agent,
    session: Session,
    rollout: Option<RolloutWriter>,
    events_rx: mpsc::UnboundedReceiver<EventMsg>,
}

impl Connection {
    async fn handle(self: Arc<Self>, message: Value) {
        let id = message.get("id").cloned();
        match message.get("method").and_then(Value::as_str) {
            Some("initialize") => self.respond(id, initialize_result()),
            Some("authenticate") => self.respond(id, json!({})),
            Some("session/new") => match self.new_session(&message).await {
                Ok(result) => self.respond(id, result),
                Err((code, msg)) => self.respond_error(id, code, &msg),
            },
            Some("session/prompt") => self.prompt(id, &message).await,
            Some("session/cancel") => self.cancel(&message).await,
            Some(other) => self.respond_error(id, -32601, &format!("method not found: {other}")),
            // No method → a response to one of our outgoing requests (request_permission).
            None => {
                if let Some(response_id) = message.get("id").and_then(Value::as_i64) {
                    self.shared.resolve(response_id, message).await;
                }
            }
        }
    }

    #[allow(clippy::needless_pass_by_value)] // `id` and `result` are moved into the response.
    fn respond(&self, id: Option<Value>, result: Value) {
        if let Some(id) = id {
            self.shared
                .send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        }
    }

    fn respond_error(&self, id: Option<Value>, code: i64, message: &str) {
        if let Some(id) = id {
            self.shared.send(
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
            );
        }
    }

    async fn new_session(&self, message: &Value) -> Result<Value, (i64, String)> {
        let cwd = message
            .pointer("/params/cwd")
            .and_then(Value::as_str)
            .ok_or((-32602, "session/new requires an absolute `cwd`".to_string()))?;
        let workspace = std::fs::canonicalize(PathBuf::from(cwd))
            .map_err(|error| (-32602, format!("invalid cwd: {error}")))?;
        if !workspace.is_dir() {
            return Err((-32602, "cwd is not a directory".to_string()));
        }

        let session_id = grokforge_protocol::SessionId::new().to_string();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let approver = Arc::new(AcpApprover {
            shared: Arc::clone(&self.shared),
            session_id: session_id.clone(),
        });

        let mut registry = ToolRegistry::with_builtins();
        if self.trust_project_mcp {
            grokforge_core::mcp_config::connect_and_register_trusted(&workspace, &mut registry)
                .await;
        }
        let sandbox = grokforge_sandbox::default_runner();
        // `interactive()` routes approvals to our approver, which forwards to the ACP client via
        // `session/request_permission`, so the editor gates boundary-crossing actions.
        let agent =
            Agent::new(self.client.clone(), registry, sandbox, approver, events_tx).interactive();

        let mut config = SessionConfig::new(workspace, self.model.clone());
        config.context_window_tokens = self.context_window_tokens;
        let session = Session::new(config);

        self.sessions.lock().await.insert(
            session_id.clone(),
            Arc::new(Mutex::new(SessionEntry {
                agent,
                session,
                rollout: None,
                events_rx,
            })),
        );
        Ok(json!({ "sessionId": session_id }))
    }

    async fn prompt(&self, id: Option<Value>, message: &Value) {
        let Some(session_id) = message
            .pointer("/params/sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            self.respond_error(id, -32602, "session/prompt requires sessionId");
            return;
        };
        let Some(entry_arc) = self.sessions.lock().await.get(&session_id).cloned() else {
            self.respond_error(id, -32602, "unknown sessionId");
            return;
        };
        let text = extract_prompt_text(message.pointer("/params/prompt"));
        if text.trim().is_empty() {
            self.respond(id, json!({ "stopReason": "end_turn" }));
            return;
        }

        let cancel = TurnCancellation::new();
        self.cancels
            .lock()
            .await
            .insert(session_id.clone(), cancel.clone());

        let mut entry = entry_arc.lock().await;
        let SessionEntry {
            agent,
            session,
            rollout,
            events_rx,
        } = &mut *entry;
        let mut saw_delta = false;
        let stop = {
            let turn = agent.run_turn_cancellable(session, &text, rollout, &cancel);
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    stop = &mut turn => break stop,
                    Some(event) = events_rx.recv() => self.emit_update(&session_id, event, &mut saw_delta),
                }
            }
        };
        while let Ok(event) = events_rx.try_recv() {
            self.emit_update(&session_id, event, &mut saw_delta);
        }
        drop(entry);
        self.cancels.lock().await.remove(&session_id);
        self.respond(id, json!({ "stopReason": stop_reason(&stop) }));
    }

    async fn cancel(&self, message: &Value) {
        if let Some(session_id) = message.pointer("/params/sessionId").and_then(Value::as_str)
            && let Some(token) = self.cancels.lock().await.get(session_id)
        {
            token.cancel();
        }
    }

    /// Translate one core [`EventMsg`] into an ACP `session/update` notification (when it maps to a
    /// visible update). `saw_delta` suppresses the duplicate final message after streamed deltas.
    fn emit_update(&self, session_id: &str, event: EventMsg, saw_delta: &mut bool) {
        let update = match event {
            EventMsg::AgentMessageDelta { delta } => {
                *saw_delta = true;
                Some(chunk("agent_message_chunk", &delta))
            }
            EventMsg::AgentMessageDone { text } => {
                if *saw_delta {
                    None
                } else {
                    Some(chunk("agent_message_chunk", &text))
                }
            }
            EventMsg::ReasoningDelta { delta } => Some(chunk("agent_thought_chunk", &delta)),
            EventMsg::ToolCallBegin {
                call_id,
                name,
                args_preview,
                ..
            } => Some(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": call_id.to_string(),
                "title": tool_title(&name, &args_preview),
                "kind": tool_kind(&name),
                "status": "in_progress",
            })),
            EventMsg::ToolOutputDelta { call_id, chunk } => Some(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": call_id.to_string(),
                "content": [{ "type": "content", "content": { "type": "text", "text": chunk } }],
            })),
            EventMsg::ToolCallEnd {
                call_id,
                ok,
                summary,
                ..
            } => Some(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": call_id.to_string(),
                "status": if ok { "completed" } else { "failed" },
                "content": [{ "type": "content", "content": { "type": "text", "text": summary } }],
            })),
            EventMsg::Committed { sha, message } => Some(chunk(
                "agent_thought_chunk",
                &format!("committed {} {message}", &sha[..sha.len().min(8)]),
            )),
            EventMsg::StreamRetrying { attempt, .. } => Some(chunk(
                "agent_thought_chunk",
                &format!("retrying (attempt {attempt})"),
            )),
            EventMsg::Error { message, .. } => {
                Some(chunk("agent_message_chunk", &format!("⚠ {message}")))
            }
            EventMsg::SubagentStarted {
                label,
                index,
                total,
                ..
            } => Some(chunk(
                "agent_thought_chunk",
                &format!("subagent {}/{}: {label}", index + 1, total),
            )),
            EventMsg::SubagentFinished { ok, summary, .. } => Some(chunk(
                "agent_thought_chunk",
                &format!("subagent {}: {summary}", if ok { "done" } else { "failed" }),
            )),
            _ => None,
        };
        if let Some(update) = update {
            self.shared.send(json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": { "sessionId": session_id, "update": update },
            }));
        }
    }
}

/// An [`Approver`] that forwards each core approval request to the ACP client as a
/// `session/request_permission` request and maps the selected option back to a [`Decision`].
struct AcpApprover {
    shared: Arc<Shared>,
    session_id: String,
}

#[async_trait]
impl Approver for AcpApprover {
    async fn request(&self, req: ApprovalRequest) -> Decision {
        let params = json!({
            "sessionId": self.session_id,
            "toolCall": {
                "toolCallId": req.call_id.as_ref().map(ToString::to_string).unwrap_or_default(),
                "title": req.reason,
                "kind": "other",
            },
            "options": [
                { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "allow_always", "name": "Allow always", "kind": "allow_always" },
                { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" },
            ],
        });
        let Some(response) = self
            .shared
            .request("session/request_permission", params)
            .await
        else {
            return Decision::Deny;
        };
        let outcome = response.pointer("/result/outcome");
        match outcome
            .and_then(|outcome| outcome.get("outcome"))
            .and_then(Value::as_str)
        {
            Some("selected") => match outcome
                .and_then(|outcome| outcome.get("optionId"))
                .and_then(Value::as_str)
            {
                Some("allow_always") => Decision::ApproveForSession,
                Some("allow_once") => Decision::Approve,
                _ => Decision::Deny,
            },
            Some("cancelled") => Decision::Abort,
            _ => Decision::Deny,
        }
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": false,
            "promptCapabilities": { "image": false, "audio": false, "embeddedContext": true },
        },
        "agentInfo": { "name": "grokforge", "title": "GrokForge", "version": env!("CARGO_PKG_VERSION") },
        "authMethods": [],
    })
}

/// Build the shared `agent_message_chunk` / `agent_thought_chunk` update body.
fn chunk(session_update: &str, text: &str) -> Value {
    json!({ "sessionUpdate": session_update, "content": { "type": "text", "text": text } })
}

/// Collect the text of an ACP prompt (an array of ContentBlock) into the plain string the core
/// turn loop expects. Text and embedded `resource` file bodies are inlined; `resource_link`s are
/// noted by URI.
fn extract_prompt_text(prompt: Option<&Value>) -> String {
    let Some(blocks) = prompt.and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
            Some("resource") => {
                if let Some(resource) = block.get("resource") {
                    let uri = resource.get("uri").and_then(Value::as_str).unwrap_or("");
                    if let Some(text) = resource.get("text").and_then(Value::as_str) {
                        out.push_str("\n\n<attachment path=\"");
                        out.push_str(&uri.replace('"', "'"));
                        out.push_str("\">\n");
                        out.push_str(text);
                        out.push_str("\n</attachment>\n");
                    }
                }
            }
            Some("resource_link") => {
                if let Some(uri) = block.get("uri").and_then(Value::as_str) {
                    out.push_str("\n[linked resource: ");
                    out.push_str(uri);
                    out.push(']');
                }
            }
            _ => {}
        }
    }
    out
}

fn tool_kind(name: &str) -> &'static str {
    match name {
        "read_file" | "git_status" | "git_diff" => "read",
        "write_file" | "edit" => "edit",
        "shell" => "execute",
        "grep" | "glob" | "list" => "search",
        _ => "other",
    }
}

fn tool_title(name: &str, preview: &str) -> String {
    let title = if preview.trim().is_empty() {
        name.to_string()
    } else {
        format!("{name} {}", preview.trim())
    };
    let title: String = title.chars().take(120).collect();
    crate::sanitize_terminal_line(&title)
}

fn stop_reason(stop: &StopReason) -> &'static str {
    match stop {
        StopReason::EndTurn | StopReason::Error => "end_turn",
        StopReason::Interrupted => "cancelled",
        StopReason::MaxIterations => "max_turn_requests",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_text_and_embedded_resources_from_a_prompt() {
        let prompt = json!([
            { "type": "text", "text": "explain this" },
            { "type": "resource", "resource": { "uri": "file:///a/b.rs", "text": "fn x() {}" } },
            { "type": "resource_link", "uri": "file:///c.md" },
            { "type": "image", "data": "…", "mimeType": "image/png" }
        ]);
        let out = extract_prompt_text(Some(&prompt));
        assert!(out.starts_with("explain this"));
        assert!(out.contains("<attachment path=\"file:///a/b.rs\">"));
        assert!(out.contains("fn x() {}"));
        assert!(out.contains("[linked resource: file:///c.md]"));
    }

    #[test]
    fn empty_or_missing_prompt_is_empty() {
        assert_eq!(extract_prompt_text(None), "");
        assert_eq!(extract_prompt_text(Some(&json!([]))), "");
    }

    #[test]
    fn tool_kinds_map_to_acp_categories() {
        assert_eq!(tool_kind("read_file"), "read");
        assert_eq!(tool_kind("edit"), "edit");
        assert_eq!(tool_kind("shell"), "execute");
        assert_eq!(tool_kind("grep"), "search");
        assert_eq!(tool_kind("spawn_task"), "other");
    }

    #[test]
    fn stop_reasons_map_to_acp_values() {
        assert_eq!(stop_reason(&StopReason::EndTurn), "end_turn");
        assert_eq!(stop_reason(&StopReason::Interrupted), "cancelled");
        assert_eq!(stop_reason(&StopReason::MaxIterations), "max_turn_requests");
    }

    #[test]
    fn initialize_advertises_protocol_version_one() {
        let result = initialize_result();
        assert_eq!(result["protocolVersion"], json!(PROTOCOL_VERSION));
        assert_eq!(result["agentInfo"]["name"], json!("grokforge"));
    }
}
