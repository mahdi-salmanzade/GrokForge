//! The agent loop. One [`Agent`] drives turns on a [`Session`]: assemble context → stream the
//! model → run tool calls (gated by the approval engine, redacted on the way back) → repeat
//! until the model stops or the iteration cap is hit. Every step emits an [`EventMsg`] and is
//! appended to the canonical JSONL rollout.

use std::sync::Arc;

use futures::StreamExt;
use grokforge_protocol::{
    ApprovalId, ApprovalRequest, EventMsg, ResponseItem, SandboxMode, SandboxPolicy, StopReason,
    ToolCallId, Usage,
};
use grokforge_sandbox::SandboxRunner;
use grokforge_xai::{StreamEvent, XaiClient};
use tokio::sync::mpsc::UnboundedSender;

use crate::agents_md;
use crate::approvals::{Approver, Gate, gate};
use crate::context::{self, Assembled};
use crate::redaction::Redactor;
use crate::session::Session;
use crate::store::RolloutWriter;
use crate::tools::{ToolInvocation, ToolOutput, ToolRegistry, TurnContext};

/// Drives turns for a session. Shared, cheap to hold; the mutable per-run state is the session
/// and the rollout writer passed to [`Agent::run_turn`].
pub struct Agent {
    client: XaiClient,
    registry: ToolRegistry,
    sandbox: Arc<dyn SandboxRunner>,
    approver: Arc<dyn Approver>,
    events: UnboundedSender<EventMsg>,
    /// Whether approvals are resolved without a human (headless); recorded on `ApprovalResolved`.
    auto_approval: bool,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("registry", &self.registry)
            .field("auto_approval", &self.auto_approval)
            .finish_non_exhaustive()
    }
}

impl Agent {
    #[must_use]
    pub fn new(
        client: XaiClient,
        registry: ToolRegistry,
        sandbox: Arc<dyn SandboxRunner>,
        approver: Arc<dyn Approver>,
        events: UnboundedSender<EventMsg>,
    ) -> Self {
        Self {
            client,
            registry,
            sandbox,
            approver,
            events,
            auto_approval: true,
        }
    }

    #[must_use]
    pub fn interactive(mut self) -> Self {
        self.auto_approval = false;
        self
    }

    fn emit(&self, msg: EventMsg) {
        // A dropped receiver just means the frontend is gone; the turn can still finish.
        let _ = self.events.send(msg);
    }

    fn turn_context(&self, session: &Session) -> TurnContext {
        let root = session.config.workspace_root.clone();
        let policy = match session.config.sandbox_mode {
            SandboxMode::ReadOnly => SandboxPolicy::read_only(&root),
            SandboxMode::WorkspaceWrite => SandboxPolicy::workspace_write(&root),
            SandboxMode::DangerFullAccess => SandboxPolicy::danger_full_access(&root),
        };
        TurnContext {
            workspace_root: root,
            policy,
            sandbox: Arc::clone(&self.sandbox),
        }
    }

    /// Run one turn to completion, mutating the session history and appending to `rollout`.
    pub async fn run_turn(
        &self,
        session: &mut Session,
        user_text: &str,
        rollout: &mut Option<RolloutWriter>,
    ) -> StopReason {
        let turn_id = grokforge_protocol::TurnId::new();
        self.emit(EventMsg::TurnStarted { turn_id });

        // User input is redacted at ingress (a pasted secret must not enter the transcript).
        let user_red = Redactor::apply(user_text);
        self.record(session, rollout, ResponseItem::user(user_red.text))
            .await;

        let ctx = self.turn_context(session);
        let agents = agents_md::discover(&session.config.workspace_root);

        let mut iteration = 0u32;
        let stop = loop {
            if iteration >= session.config.max_iterations {
                break StopReason::MaxIterations;
            }
            iteration += 1;

            let Assembled {
                request, ledger, ..
            } = match context::assemble(session, &agents, self.registry.tool_defs()) {
                Ok(a) => a,
                Err(e) => {
                    self.emit(EventMsg::Error {
                        message: format!("failed to assemble request: {e}"),
                        recoverable: false,
                    });
                    break StopReason::Error;
                }
            };
            for entry in ledger.entries {
                self.emit(EventMsg::LedgerAppended(entry));
            }

            let mut stream = match self.client.stream(&request).await {
                Ok(s) => s,
                Err(e) => {
                    self.emit(EventMsg::Error {
                        message: format!("model request failed: {e}"),
                        recoverable: e.is_retriable(),
                    });
                    break StopReason::Error;
                }
            };

            let mut assistant_text = String::new();
            let mut tool_calls: Vec<(ToolCallId, String, String)> = Vec::new();
            let mut usage = Usage::default();

            while let Some(event) = stream.next().await {
                match event {
                    Ok(StreamEvent::TextDelta(d)) => {
                        assistant_text.push_str(&d);
                        self.emit(EventMsg::AgentMessageDelta { delta: d });
                    }
                    Ok(StreamEvent::ReasoningDelta(d)) => {
                        self.emit(EventMsg::ReasoningDelta { delta: d });
                    }
                    Ok(StreamEvent::ToolCall(call)) => {
                        let id = ToolCallId::new();
                        tool_calls.push((id, call.name, call.arguments));
                    }
                    Ok(StreamEvent::Usage(u)) => {
                        usage = Usage {
                            input_tokens: u.input_tokens,
                            cached_tokens: u.cached_tokens,
                            output_tokens: u.output_tokens,
                            reasoning_tokens: u.reasoning_tokens,
                        };
                    }
                    Ok(StreamEvent::Completed { .. } | StreamEvent::Created { .. }) => {}
                    Err(e) => {
                        self.emit(EventMsg::Error {
                            message: format!("stream error: {e}"),
                            recoverable: e.is_retriable(),
                        });
                        break;
                    }
                }
            }

            self.emit(EventMsg::TokenUsage { usage });

            if !assistant_text.is_empty() {
                self.emit(EventMsg::AgentMessageDone {
                    text: assistant_text.clone(),
                });
                self.record(session, rollout, ResponseItem::assistant(assistant_text))
                    .await;
            }

            if tool_calls.is_empty() {
                break StopReason::EndTurn;
            }

            for (call_id, name, arguments) in tool_calls {
                self.run_tool_call(session, rollout, &ctx, call_id, &name, &arguments)
                    .await;
            }
        };

        self.emit(EventMsg::TurnComplete {
            turn_id,
            stop: stop.clone(),
        });
        stop
    }

    async fn run_tool_call(
        &self,
        session: &mut Session,
        rollout: &mut Option<RolloutWriter>,
        ctx: &TurnContext,
        call_id: ToolCallId,
        name: &str,
        arguments: &str,
    ) {
        self.record(
            session,
            rollout,
            ResponseItem::ToolCall {
                id: call_id,
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
        )
        .await;

        let Some(tool) = self.registry.get(name) else {
            let msg = format!("unknown tool `{name}`");
            self.emit(EventMsg::ToolCallEnd {
                call_id,
                ok: false,
                summary: msg.clone(),
                denial: None,
            });
            self.record_tool_result(session, rollout, call_id, &msg, true)
                .await;
            return;
        };

        let args: serde_json::Value =
            serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null);
        let need = tool.approval(&args, ctx);

        // Consult the decision table; ask the approver if required.
        if let Gate::Ask(kind) = gate(
            session.config.approval_policy,
            session.config.sandbox_mode,
            &need,
        ) {
            let req = ApprovalRequest {
                id: ApprovalId::new(),
                call_id: Some(call_id),
                kind,
                reason: format!("run `{name}`"),
            };
            self.emit(EventMsg::ApprovalRequested(req.clone()));
            let decision = self.approver.request(req).await;
            self.emit(EventMsg::ApprovalResolved {
                summary: format!("`{name}`"),
                decision: format!("{decision:?}"),
                auto: self.auto_approval,
            });
            if !decision.is_approved() {
                let feedback = match decision {
                    grokforge_protocol::Decision::DenyWithFeedback(f) => f,
                    _ => "denied".to_string(),
                };
                let content = format!("[not run: {feedback}]");
                self.emit(EventMsg::ToolCallEnd {
                    call_id,
                    ok: false,
                    summary: "denied".to_string(),
                    denial: None,
                });
                self.record_tool_result(session, rollout, call_id, &content, true)
                    .await;
                return;
            }
        }

        self.emit(EventMsg::ToolCallBegin {
            call_id,
            name: name.to_string(),
            args_preview: preview(arguments),
            sandboxed: session.config.sandbox_mode.is_sandboxed(),
        });

        let output = tool.invoke(ToolInvocation { call_id, args, ctx }).await;

        // Redact tool output before it enters the transcript / the next request.
        let red = Redactor::apply(output.content());
        let is_error = output.is_error();
        let denial = match &output {
            ToolOutput::Failure { denial, .. } => *denial,
            ToolOutput::Success { .. } => None,
        };
        self.emit(EventMsg::ToolCallEnd {
            call_id,
            ok: !is_error,
            summary: summarize(&red.text),
            denial,
        });
        self.record_tool_result(session, rollout, call_id, &red.text, is_error)
            .await;
    }

    async fn record(
        &self,
        session: &mut Session,
        rollout: &mut Option<RolloutWriter>,
        item: ResponseItem,
    ) {
        if let Some(w) = rollout.as_mut() {
            if let Err(e) = w.append(&item).await {
                tracing::warn!("rollout append failed: {e}");
            }
        }
        session.history.push(item);
    }

    async fn record_tool_result(
        &self,
        session: &mut Session,
        rollout: &mut Option<RolloutWriter>,
        id: ToolCallId,
        content: &str,
        is_error: bool,
    ) {
        self.record(
            session,
            rollout,
            ResponseItem::ToolResult {
                id,
                content: content.to_string(),
                is_error,
            },
        )
        .await;
    }
}

fn preview(s: &str) -> String {
    let one_line = s.replace('\n', " ");
    // Truncate on a char boundary to avoid slicing through a multibyte codepoint.
    if one_line.chars().count() > 80 {
        let truncated: String = one_line.chars().take(80).collect();
        format!("{truncated}…")
    } else {
        one_line
    }
}

fn summarize(s: &str) -> String {
    let first = s.lines().next().unwrap_or("");
    preview(first)
}
