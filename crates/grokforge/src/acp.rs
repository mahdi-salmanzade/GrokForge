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

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use grokforge_core::mcp_config::{
    EditorMcpError, EditorStdioServer, MAX_EDITOR_MCP_ARGS, MAX_EDITOR_MCP_ENV,
    MAX_EDITOR_MCP_SERVERS,
};
use grokforge_core::{
    Agent, Approver, RolloutWriter, Session, SessionConfig, ToolRegistry, TurnCancellation,
};
use grokforge_protocol::{ApprovalRequest, Decision, EventMsg, StopReason};
use grokforge_xai::{Effort, XaiClient, model_supports_effort};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};

const PROTOCOL_VERSION: i64 = 1;
const ACP_STDIO_SERVER_FIELDS: [&str; 5] = ["name", "command", "args", "env", "_meta"];
const ACP_STDIO_REQUIRED_FIELDS: [&str; 4] = ["name", "command", "args", "env"];
/// ACP clients can include editor buffers as embedded resources. Keep one prompt below the core's
/// conservative context budget and prevent an untrusted client from making persistence allocate
/// without bound before the model request is assembled.
const MAX_ACP_PROMPT_BYTES: usize = 512 * 1024;
/// Bound the complete newline-delimited JSON-RPC envelope before parsing. MCP declarations can be
/// larger than a prompt because their per-server environment caps are aggregate, but a client can
/// never make the ACP process buffer an unbounded line.
const MAX_ACP_JSON_LINE_BYTES: usize = 4 * 1024 * 1024;
/// A disconnected or broken editor must not leave a turn blocked on permission forever.
const PERMISSION_RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);

/// Run the ACP agent server over stdio until the client closes stdin.
pub async fn run(
    trust_project_mcp: bool,
    trust_project_config: bool,
    model_override: Option<String>,
    effort_override: Option<String>,
) -> ExitCode {
    // stdin is the JSON-RPC channel, so a password prompt is impossible: require `XAI_API_KEY`.
    let Some(api_key) = acp_api_key() else {
        eprintln!(
            "grokforge acp: set XAI_API_KEY in the agent's environment (stdin is the ACP channel, so interactive sign-in is unavailable)"
        );
        return ExitCode::from(3);
    };
    let startup_workspace = match std::env::current_dir().and_then(std::fs::canonicalize) {
        Ok(path) if path.is_dir() => path,
        Ok(path) => {
            eprintln!(
                "grokforge acp: current workspace is not a directory: {}",
                path.display()
            );
            return ExitCode::from(2);
        }
        Err(error) => {
            eprintln!("grokforge acp: cannot resolve current workspace: {error}");
            return ExitCode::from(2);
        }
    };
    let startup_settings = match grokforge_config::Config::load_with_project_config(
        &startup_workspace,
        trust_project_config,
    ) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!(
                "grokforge acp: configuration error: {}",
                crate::sanitize_terminal(&error.to_string())
            );
            return ExitCode::from(2);
        }
    };
    let base_url = std::env::var("XAI_BASE_URL").unwrap_or(startup_settings.provider.grok.base_url);
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
        client,
        model_override,
        effort_override,
        trust_project_mcp,
        trust_project_config,
    });

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    loop {
        let line = match read_bounded_json_line(&mut reader).await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                eprintln!("grokforge acp: invalid input: {error}");
                drop(connection);
                let _ = writer.await;
                return ExitCode::from(2);
            }
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
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

fn acp_api_key() -> Option<String> {
    let key = std::env::var("XAI_API_KEY").ok()?;
    if key.trim().is_empty() {
        return None;
    }
    Some(key)
}

/// Read one newline-delimited ACP message without ever retaining more than the configured cap.
/// Once a line overflows, the remainder is drained through its newline before returning the error
/// so callers that choose to recover cannot desynchronize the transport.
async fn read_bounded_json_line<R>(reader: &mut R) -> std::io::Result<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    let mut overflowed = false;
    loop {
        let (consumed, ended, eof) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                (0, false, true)
            } else if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
                if !overflowed {
                    if line
                        .len()
                        .checked_add(newline)
                        .is_none_or(|length| length > MAX_ACP_JSON_LINE_BYTES)
                    {
                        overflowed = true;
                    } else {
                        line.extend_from_slice(&available[..newline]);
                    }
                }
                (newline + 1, true, false)
            } else {
                if !overflowed {
                    if line
                        .len()
                        .checked_add(available.len())
                        .is_none_or(|length| length > MAX_ACP_JSON_LINE_BYTES)
                    {
                        overflowed = true;
                    } else {
                        line.extend_from_slice(available);
                    }
                }
                (available.len(), false, false)
            }
        };

        if consumed > 0 {
            reader.consume(consumed);
        }
        if ended || eof {
            if overflowed {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("ACP JSON-RPC line exceeds {MAX_ACP_JSON_LINE_BYTES} bytes"),
                ));
            }
            if eof && line.is_empty() {
                return Ok(None);
            }
            if line.ends_with(b"\r") {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
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
    /// closes or the client does not answer before the permission deadline).
    async fn request(self: &Arc<Self>, method: &str, params: Value) -> Option<Value> {
        self.request_with_timeout(method, params, PERMISSION_RESPONSE_TIMEOUT)
            .await
    }

    async fn request_with_timeout(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        response_timeout: Duration,
    ) -> Option<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if self
            .out_tx
            .send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .is_err()
        {
            self.pending.lock().await.remove(&id);
            return None;
        }
        // Keep expiry independent from this request future. The core deliberately drops an
        // in-flight approval future when a turn is cancelled; this task still removes its sender
        // at the deadline, so cancellation cannot leak a pending request forever.
        let expiry = {
            let shared = Arc::clone(self);
            tokio::spawn(async move {
                tokio::time::sleep(response_timeout).await;
                shared.pending.lock().await.remove(&id);
            })
        };
        let response = rx.await.ok();
        expiry.abort();
        // `resolve` normally removed the sender before delivering the response. This also closes
        // the race where expiry and a response become ready together.
        self.pending.lock().await.remove(&id);
        response
    }

    async fn resolve(&self, id: i64, message: Value) {
        if let Some(tx) = self.pending.lock().await.remove(&id) {
            let _ = tx.send(message);
        }
    }
}

struct Connection {
    shared: Arc<Shared>,
    sessions: Mutex<HashMap<String, Arc<SessionSlot>>>,
    client: XaiClient,
    model_override: Option<String>,
    effort_override: Option<String>,
    trust_project_mcp: bool,
    trust_project_config: bool,
}

/// Per-session prompt serialization plus separately lockable cancellation state. A queued prompt
/// must acquire `entry` before installing its token, so it cannot replace the token belonging to
/// the currently running prompt. `session/cancel` only needs `active_prompt`, so it never waits for
/// the long-lived turn guard.
struct SessionSlot {
    entry: Mutex<SessionEntry>,
    active_prompt: Mutex<ActivePrompt>,
}

struct SessionEntry {
    agent: Agent,
    session: Session,
    rollout: Option<RolloutWriter>,
    events_rx: mpsc::UnboundedReceiver<EventMsg>,
}

#[derive(Default)]
struct ActivePrompt {
    cancellation: Option<Arc<TurnCancellation>>,
}

impl ActivePrompt {
    fn install(&mut self, cancellation: Arc<TurnCancellation>) {
        self.cancellation = Some(cancellation);
    }

    fn cancel(&self) {
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
        }
    }

    /// Clear only the token owned by the finishing prompt. Pointer identity prevents stale cleanup
    /// from deleting a newer token if prompt scheduling changes in the future.
    fn clear_if_owner(&mut self, owner: &Arc<TurnCancellation>) {
        if self
            .cancellation
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, owner))
        {
            self.cancellation = None;
        }
    }
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
        let workspace = canonical_session_cwd(cwd).map_err(|message| (-32602, message))?;
        // Validate the complete editor process list before model discovery or either editor/project
        // MCP startup. Unsupported transports and malformed later entries therefore cannot arrive
        // after an earlier stdio command has already executed.
        let editor_mcp = parse_editor_mcp_servers(message).map_err(|message| (-32602, message))?;
        let settings = grokforge_config::Config::load_with_project_config(
            &workspace,
            self.trust_project_config,
        )
        .map_err(|error| (-32602, format!("invalid GrokForge configuration: {error}")))?;
        let model = self
            .model_override
            .clone()
            .unwrap_or_else(|| settings.agent.default_model.clone());
        let model_catalog = crate::model_catalog_startup(&self.client, &model)
            .await
            .map_err(|_| (-32002, format!("model `{model}` is unavailable")))?;
        let selected_model = model_catalog.iter().find(|candidate| {
            candidate.id == model || candidate.aliases.iter().any(|alias| alias == &model)
        });
        let active_model = selected_model.map_or(model, |candidate| candidate.id.clone());
        let context_window_tokens = selected_model.and_then(|candidate| candidate.context_window);
        let effort = match self.effort_override.as_deref() {
            Some("auto") => None,
            Some("low") => Some(Effort::Low),
            Some("medium") => Some(Effort::Medium),
            Some("high") => Some(Effort::High),
            Some("xhigh") => Some(Effort::Xhigh),
            Some(_) => return Err((-32602, "invalid reasoning effort".to_string())),
            None => settings.agent.effort.map(configured_effort),
        };
        if effort.is_some_and(|effort| !model_supports_effort(&active_model, effort)) {
            return Err((
                -32602,
                "reasoning effort `xhigh` requires an xAI multi-agent model".to_string(),
            ));
        }

        let session_id = grokforge_protocol::SessionId::new().to_string();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let approver = Arc::new(AcpApprover {
            shared: Arc::clone(&self.shared),
            session_id: session_id.clone(),
        });

        let mut registry = ToolRegistry::with_builtins();
        if let Err(error) = grokforge_core::mcp_config::connect_and_register_editor(
            &workspace,
            editor_mcp,
            &mut registry,
        )
        .await
        {
            let code = match &error {
                EditorMcpError::Invalid(_) => -32602,
                EditorMcpError::StartupTimeout => -32800,
                _ => -32603,
            };
            return Err((code, error.to_string()));
        }
        if self.trust_project_mcp {
            grokforge_core::mcp_config::connect_and_register_trusted(&workspace, &mut registry)
                .await;
        }
        let sandbox = grokforge_sandbox::default_runner();
        // `interactive()` routes approvals to our approver, which forwards to the ACP client via
        // `session/request_permission`, so the editor gates boundary-crossing actions.
        let agent =
            Agent::new(self.client.clone(), registry, sandbox, approver, events_tx).interactive();

        let mut config = SessionConfig::new(workspace, active_model);
        config.plan_model = settings.agent.plan_model;
        config.model_catalog = model_catalog;
        config.context_window_tokens = context_window_tokens;
        config.max_iterations = settings.agent.max_iterations;
        config.auto_compact = settings.agent.auto_compact;
        config.compaction_trigger_bytes = settings.agent.compaction_trigger_bytes;
        config.compaction_keep_tail = settings.agent.compaction_keep_tail;
        config.effort = effort;
        let session = Session::new(config);

        self.sessions.lock().await.insert(
            session_id.clone(),
            Arc::new(SessionSlot {
                entry: Mutex::new(SessionEntry {
                    agent,
                    session,
                    rollout: None,
                    events_rx,
                }),
                active_prompt: Mutex::new(ActivePrompt::default()),
            }),
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
        let Some(slot) = self.sessions.lock().await.get(&session_id).cloned() else {
            self.respond_error(id, -32602, "unknown sessionId");
            return;
        };
        let text = match extract_prompt_text(message.pointer("/params/prompt")) {
            Ok(text) => text,
            Err(PromptTextError::TooLarge) => {
                self.respond_error(
                    id,
                    -32602,
                    &format!(
                        "session/prompt text and embedded resources exceed the {MAX_ACP_PROMPT_BYTES}-byte limit"
                    ),
                );
                return;
            }
        };
        if text.trim().is_empty() {
            self.respond(id, json!({ "stopReason": "end_turn" }));
            return;
        }

        // Acquire prompt ownership before publishing the cancellation token. A second prompt for
        // this session queues here and therefore cannot steal cancellation from the running turn.
        let mut entry = slot.entry.lock().await;
        let cancel = Arc::new(TurnCancellation::new());
        slot.active_prompt.lock().await.install(Arc::clone(&cancel));
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
        slot.active_prompt.lock().await.clear_if_owner(&cancel);
        drop(entry);
        self.respond(id, json!({ "stopReason": stop_reason(&stop) }));
    }

    async fn cancel(&self, message: &Value) {
        let Some(session_id) = message.pointer("/params/sessionId").and_then(Value::as_str) else {
            return;
        };
        let slot = self.sessions.lock().await.get(session_id).cloned();
        if let Some(slot) = slot {
            slot.active_prompt.lock().await.cancel();
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

fn configured_effort(effort: grokforge_config::Effort) -> Effort {
    match effort {
        grokforge_config::Effort::Low => Effort::Low,
        grokforge_config::Effort::Medium => Effort::Medium,
        grokforge_config::Effort::High => Effort::High,
        grokforge_config::Effort::Xhigh => Effort::Xhigh,
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

/// Parse only the official ACP v1 stdio shape. The exact-key checks intentionally reject HTTP,
/// SSE, and extension transport fields rather than guessing how to execute them.
fn parse_editor_mcp_servers(message: &Value) -> Result<Vec<EditorStdioServer>, String> {
    let Some(raw_servers) = message.pointer("/params/mcpServers") else {
        return Ok(Vec::new());
    };
    let servers = raw_servers
        .as_array()
        .ok_or_else(|| "session/new `mcpServers` must be an array".to_string())?;
    if servers.len() > MAX_EDITOR_MCP_SERVERS {
        return Err(format!(
            "session/new accepts at most {MAX_EDITOR_MCP_SERVERS} MCP servers"
        ));
    }

    let mut parsed = Vec::with_capacity(servers.len());
    let mut names = BTreeSet::new();
    for (server_index, raw_server) in servers.iter().enumerate() {
        let server = raw_server
            .as_object()
            .ok_or_else(|| format!("mcpServers[{server_index}] must be a stdio server object"))?;
        if server
            .keys()
            .any(|key| !ACP_STDIO_SERVER_FIELDS.contains(&key.as_str()))
        {
            return Err(format!(
                "mcpServers[{server_index}] contains an unsupported field; only stdio `name`, `command`, `args`, and `env` are accepted"
            ));
        }
        if ACP_STDIO_REQUIRED_FIELDS
            .iter()
            .any(|field| !server.contains_key(*field))
        {
            return Err(format!(
                "mcpServers[{server_index}] requires `name`, `command`, `args`, and `env`"
            ));
        }
        let name = server["name"]
            .as_str()
            .ok_or_else(|| format!("mcpServers[{server_index}].name must be a string"))?;
        let command = server["command"]
            .as_str()
            .ok_or_else(|| format!("mcpServers[{server_index}].command must be a string"))?;
        let raw_args = server["args"].as_array().ok_or_else(|| {
            format!("mcpServers[{server_index}].args must be an array of strings")
        })?;
        if raw_args.len() > MAX_EDITOR_MCP_ARGS {
            return Err(format!(
                "mcpServers[{server_index}].args accepts at most {MAX_EDITOR_MCP_ARGS} entries"
            ));
        }
        let mut args = Vec::with_capacity(raw_args.len());
        for (arg_index, argument) in raw_args.iter().enumerate() {
            args.push(argument.as_str().ok_or_else(|| {
                format!("mcpServers[{server_index}].args[{arg_index}] must be a string")
            })?);
        }

        let raw_env = server["env"].as_array().ok_or_else(|| {
            format!("mcpServers[{server_index}].env must be an array of name/value objects")
        })?;
        if raw_env.len() > MAX_EDITOR_MCP_ENV {
            return Err(format!(
                "mcpServers[{server_index}].env accepts at most {MAX_EDITOR_MCP_ENV} entries"
            ));
        }
        let mut env = Vec::with_capacity(raw_env.len());
        for (env_index, raw_entry) in raw_env.iter().enumerate() {
            let entry = raw_entry.as_object().ok_or_else(|| {
                format!("mcpServers[{server_index}].env[{env_index}] must be a name/value object")
            })?;
            if entry
                .keys()
                .any(|key| !matches!(key.as_str(), "name" | "value" | "_meta"))
                || !entry.contains_key("name")
                || !entry.contains_key("value")
            {
                return Err(format!(
                    "mcpServers[{server_index}].env[{env_index}] accepts `name`, `value`, and optional `_meta`"
                ));
            }
            let key = entry["name"].as_str().ok_or_else(|| {
                format!("mcpServers[{server_index}].env[{env_index}].name must be a string")
            })?;
            let value = entry["value"].as_str().ok_or_else(|| {
                format!("mcpServers[{server_index}].env[{env_index}].value must be a string")
            })?;
            env.push((key, value));
        }

        let declaration = EditorStdioServer::try_new(name, command, &args, &env)
            .map_err(|error| format!("mcpServers[{server_index}]: {error}"))?;
        if !names.insert(name) {
            return Err("mcpServers names must be unique".to_string());
        }
        parsed.push(declaration);
    }
    Ok(parsed)
}

fn canonical_session_cwd(cwd: &str) -> Result<PathBuf, String> {
    let requested = PathBuf::from(cwd);
    if !requested.is_absolute() {
        return Err("session/new requires an absolute `cwd`".to_string());
    }
    let workspace =
        std::fs::canonicalize(&requested).map_err(|error| format!("invalid cwd: {error}"))?;
    if !workspace.is_absolute() || !workspace.is_dir() {
        return Err("cwd is not a canonical directory".to_string());
    }
    Ok(workspace)
}

/// Build the shared `agent_message_chunk` / `agent_thought_chunk` update body.
fn chunk(session_update: &str, text: &str) -> Value {
    json!({ "sessionUpdate": session_update, "content": { "type": "text", "text": text } })
}

/// Collect the text of an ACP prompt (an array of ContentBlock) into the plain string the core
/// turn loop expects. Text and embedded `resource` file bodies are inlined; `resource_link`s are
/// noted by URI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptTextError {
    TooLarge,
}

fn extract_prompt_text(prompt: Option<&Value>) -> Result<String, PromptTextError> {
    let Some(blocks) = prompt.and_then(Value::as_array) else {
        return Ok(String::new());
    };
    let mut out = String::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !out.is_empty() {
                        push_prompt_part(&mut out, "\n")?;
                    }
                    push_prompt_part(&mut out, text)?;
                }
            }
            Some("resource") => {
                if let Some(resource) = block.get("resource") {
                    let uri = resource.get("uri").and_then(Value::as_str).unwrap_or("");
                    if let Some(text) = resource.get("text").and_then(Value::as_str) {
                        push_prompt_part(&mut out, "\n\n<attachment path=\"")?;
                        push_prompt_attribute(&mut out, uri)?;
                        push_prompt_part(&mut out, "\">\n")?;
                        push_prompt_part(&mut out, text)?;
                        push_prompt_part(&mut out, "\n</attachment>\n")?;
                    }
                }
            }
            Some("resource_link") => {
                if let Some(uri) = block.get("uri").and_then(Value::as_str) {
                    push_prompt_part(&mut out, "\n[linked resource: ")?;
                    push_prompt_part(&mut out, uri)?;
                    push_prompt_part(&mut out, "]")?;
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn push_prompt_part(out: &mut String, value: &str) -> Result<(), PromptTextError> {
    let total = out
        .len()
        .checked_add(value.len())
        .ok_or(PromptTextError::TooLarge)?;
    if total > MAX_ACP_PROMPT_BYTES {
        return Err(PromptTextError::TooLarge);
    }
    out.try_reserve(value.len())
        .map_err(|_| PromptTextError::TooLarge)?;
    out.push_str(value);
    Ok(())
}

/// Keep the synthetic attachment header on one line. Replacement characters are never wider than
/// their source, but still flow through the same bounded append helper for a single size invariant.
fn push_prompt_attribute(out: &mut String, value: &str) -> Result<(), PromptTextError> {
    for character in value.chars() {
        if character == '"' || character.is_control() {
            push_prompt_part(out, "_")?;
        } else {
            let mut encoded = [0; 4];
            push_prompt_part(out, character.encode_utf8(&mut encoded))?;
        }
    }
    Ok(())
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
        let out = extract_prompt_text(Some(&prompt)).expect("bounded prompt");
        assert!(out.starts_with("explain this"));
        assert!(out.contains("<attachment path=\"file:///a/b.rs\">"));
        assert!(out.contains("fn x() {}"));
        assert!(out.contains("[linked resource: file:///c.md]"));
    }

    #[test]
    fn empty_or_missing_prompt_is_empty() {
        assert_eq!(extract_prompt_text(None).expect("missing prompt"), "");
        assert_eq!(
            extract_prompt_text(Some(&json!([]))).expect("empty prompt"),
            ""
        );
    }

    #[test]
    fn prompt_text_and_resources_share_one_hard_byte_cap() {
        let exact = json!([{ "type": "text", "text": "x".repeat(MAX_ACP_PROMPT_BYTES) }]);
        assert_eq!(
            extract_prompt_text(Some(&exact))
                .expect("exact limit")
                .len(),
            MAX_ACP_PROMPT_BYTES
        );

        let over = json!([
            { "type": "text", "text": "x".repeat(MAX_ACP_PROMPT_BYTES / 2) },
            {
                "type": "resource",
                "resource": {
                    "uri": "file:///large.rs",
                    "text": "y".repeat(MAX_ACP_PROMPT_BYTES / 2)
                }
            }
        ]);
        assert_eq!(
            extract_prompt_text(Some(&over)),
            Err(PromptTextError::TooLarge)
        );
    }

    #[test]
    fn embedded_resource_attribute_stays_on_one_line() {
        let prompt = json!([{
            "type": "resource",
            "resource": { "uri": "file:///bad\n\"name.rs", "text": "body" }
        }]);
        let out = extract_prompt_text(Some(&prompt)).expect("bounded prompt");
        assert!(out.contains("<attachment path=\"file:///bad__name.rs\">"));
    }

    #[test]
    fn session_cwd_must_be_absolute_and_resolves_to_a_directory() {
        assert!(canonical_session_cwd("relative/project").is_err());

        let workspace = tempfile::tempdir().expect("workspace");
        let canonical = std::fs::canonicalize(workspace.path()).expect("canonical workspace");
        assert_eq!(
            canonical_session_cwd(workspace.path().to_string_lossy().as_ref())
                .expect("absolute directory"),
            canonical
        );

        let file = workspace.path().join("not-a-directory");
        std::fs::write(&file, "content").expect("fixture file");
        assert!(canonical_session_cwd(file.to_string_lossy().as_ref()).is_err());
    }

    fn test_command_path() -> String {
        std::env::current_exe()
            .expect("current executable")
            .to_string_lossy()
            .into_owned()
    }

    fn session_new_with_mcp(servers: &Value) -> Value {
        json!({ "params": { "mcpServers": servers } })
    }

    #[test]
    fn parses_the_official_acp_stdio_mcp_shape() {
        let message = session_new_with_mcp(&json!([{
            "name": "Editor tools 🛠",
            "command": test_command_path(),
            "args": ["--stdio"],
            "env": [{
                "name": "DOCS-TOKEN",
                "value": "explicit",
                "_meta": { "editor": "test" }
            }],
            "_meta": { "editor": "test" }
        }]));
        assert_eq!(parse_editor_mcp_servers(&message).unwrap().len(), 1);
        assert!(
            parse_editor_mcp_servers(&json!({ "params": {} }))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_unsupported_mcp_transports_and_fields() {
        let command = test_command_path();
        let unsupported = [
            session_new_with_mcp(&json!({})),
            session_new_with_mcp(&json!([{
                "name": "remote",
                "url": "https://example.invalid/mcp"
            }])),
            session_new_with_mcp(&json!([{
                "name": "sse",
                "command": command,
                "args": [],
                "env": [],
                "transport": "sse"
            }])),
        ];
        for message in unsupported {
            assert!(parse_editor_mcp_servers(&message).is_err());
        }
    }

    #[test]
    fn rejects_malformed_or_oversized_mcp_entries() {
        let command = test_command_path();
        let malformed = [
            session_new_with_mcp(&json!([{
                "name": "relative",
                "command": "python3",
                "args": [],
                "env": []
            }])),
            session_new_with_mcp(&json!([{
                "name": "args",
                "command": command,
                "args": "--stdio",
                "env": []
            }])),
            session_new_with_mcp(&json!([{
                "name": "env",
                "command": command,
                "args": [],
                "env": [{ "name": "BAD=NAME", "value": "x" }]
            }])),
            session_new_with_mcp(&json!([{
                "name": "env",
                "command": command,
                "args": [],
                "env": [{ "name": "OK", "value": "x", "secret": true }]
            }])),
            session_new_with_mcp(&json!([
                { "name": "same", "command": command, "args": [], "env": [] },
                { "name": "same", "command": command, "args": [], "env": [] }
            ])),
        ];
        for message in malformed {
            assert!(parse_editor_mcp_servers(&message).is_err());
        }

        let too_many_servers = (0..=MAX_EDITOR_MCP_SERVERS)
            .map(|index| {
                json!({
                    "name": format!("server_{index}"),
                    "command": command,
                    "args": [],
                    "env": []
                })
            })
            .collect::<Vec<_>>();
        assert!(parse_editor_mcp_servers(&session_new_with_mcp(&json!(too_many_servers))).is_err());
        let too_many_args = vec!["x"; MAX_EDITOR_MCP_ARGS + 1];
        assert!(
            parse_editor_mcp_servers(&session_new_with_mcp(&json!([{
                "name": "args",
                "command": command,
                "args": too_many_args,
                "env": []
            }])))
            .is_err()
        );
        let too_many_env = (0..=MAX_EDITOR_MCP_ENV)
            .map(|index| json!({ "name": format!("KEY_{index}"), "value": "x" }))
            .collect::<Vec<_>>();
        assert!(
            parse_editor_mcp_servers(&session_new_with_mcp(&json!([{
                "name": "env",
                "command": command,
                "args": [],
                "env": too_many_env
            }])))
            .is_err()
        );
    }

    #[tokio::test]
    async fn raw_json_lines_are_bounded_and_overflow_is_fully_drained() {
        let mut input = vec![b'x'; MAX_ACP_JSON_LINE_BYTES + 1];
        input.extend_from_slice(b"\nnext\r\n");
        let mut reader = BufReader::new(input.as_slice());

        let error = read_bounded_json_line(&mut reader).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            read_bounded_json_line(&mut reader).await.unwrap(),
            Some(b"next".to_vec())
        );
        assert_eq!(read_bounded_json_line(&mut reader).await.unwrap(), None);
    }

    #[tokio::test]
    async fn raw_json_line_accepts_the_exact_byte_limit() {
        let mut input = vec![b'x'; MAX_ACP_JSON_LINE_BYTES];
        input.push(b'\n');
        let mut reader = BufReader::new(input.as_slice());

        assert_eq!(
            read_bounded_json_line(&mut reader)
                .await
                .unwrap()
                .unwrap()
                .len(),
            MAX_ACP_JSON_LINE_BYTES
        );
    }

    #[tokio::test]
    async fn outgoing_request_times_out_and_removes_pending_sender() {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            out_tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
        });
        let request = {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move {
                shared
                    .request_with_timeout(
                        "session/request_permission",
                        json!({}),
                        Duration::from_millis(10),
                    )
                    .await
            })
        };
        let sent = out_rx.recv().await.expect("outgoing request");
        let id = sent.get("id").and_then(Value::as_i64).expect("request id");
        assert!(request.await.expect("request task").is_none());
        assert!(!shared.pending.lock().await.contains_key(&id));
    }

    #[tokio::test]
    async fn cancelled_permission_future_still_expires_its_pending_sender() {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            out_tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
        });
        let request = {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move {
                shared
                    .request_with_timeout(
                        "session/request_permission",
                        json!({}),
                        Duration::from_millis(10),
                    )
                    .await
            })
        };
        let sent = out_rx.recv().await.expect("outgoing request");
        let id = sent.get("id").and_then(Value::as_i64).expect("request id");
        request.abort();
        let _ = request.await;

        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!shared.pending.lock().await.contains_key(&id));
    }

    #[test]
    fn stale_prompt_cannot_clear_or_cancel_the_current_owner() {
        let stale = Arc::new(TurnCancellation::new());
        let current = Arc::new(TurnCancellation::new());
        let mut active = ActivePrompt {
            cancellation: Some(Arc::clone(&current)),
        };

        active.clear_if_owner(&stale);
        active.cancel();
        assert!(!stale.is_cancelled());
        assert!(current.is_cancelled());

        active.clear_if_owner(&current);
        assert!(active.cancellation.is_none());
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
