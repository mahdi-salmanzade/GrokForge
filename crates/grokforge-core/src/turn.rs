//! The agent loop. One [`Agent`] drives turns on a [`Session`]: assemble context → stream the
//! model → run tool calls (gated by the approval engine, redacted on the way back) → repeat
//! until the model stops or the iteration cap is hit. Every step emits an [`EventMsg`] and is
//! appended to the canonical JSONL rollout.

use std::sync::Arc;

use futures::StreamExt;
use grokforge_protocol::{
    ApprovalId, ApprovalRequest, EventMsg, ResponseItem, SandboxMode, SandboxPolicy, StopReason,
    ToolCallId, TurnId, Usage,
};
use grokforge_sandbox::SandboxRunner;
use grokforge_xai::{InputItem, ResponsesRequest, Role, StreamEvent, XaiClient};
use tokio::sync::mpsc::UnboundedSender;

use crate::agents_md;
use crate::approvals::{Approver, Gate, gate};
use crate::compaction;
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
    /// Whether this agent may spawn subagents (false inside a subagent — depth cap 1).
    allow_subagents: bool,
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
            allow_subagents: true,
        }
    }

    #[must_use]
    pub fn interactive(mut self) -> Self {
        self.auto_approval = false;
        self
    }

    /// A sibling agent (shared client/registry/sandbox/approver) for running a subagent turn,
    /// with its own event channel and subagent-spawning disabled (depth cap 1).
    fn for_subagent(&self, events: UnboundedSender<EventMsg>) -> Self {
        Self {
            client: self.client.clone(),
            registry: self.registry.clone(),
            sandbox: Arc::clone(&self.sandbox),
            approver: Arc::clone(&self.approver),
            events,
            auto_approval: self.auto_approval,
            allow_subagents: false,
        }
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
            touched: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Like [`Self::turn_context`] but forces a read-only sandbox policy (plan mode).
    fn turn_context_readonly(&self, session: &Session) -> TurnContext {
        let root = session.config.workspace_root.clone();
        TurnContext {
            policy: SandboxPolicy::read_only(&root),
            workspace_root: root,
            sandbox: Arc::clone(&self.sandbox),
            touched: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Run one turn to completion (execute mode).
    pub async fn run_turn(
        &self,
        session: &mut Session,
        user_text: &str,
        rollout: &mut Option<RolloutWriter>,
    ) -> StopReason {
        self.run_inner(session, user_text, false, rollout).await
    }

    /// Run a plan-mode turn: read-only tools + read-only sandbox + a planning preamble, so the
    /// agent produces a plan without changing anything.
    pub async fn run_plan_turn(
        &self,
        session: &mut Session,
        user_text: &str,
        rollout: &mut Option<RolloutWriter>,
    ) -> StopReason {
        self.run_inner(session, user_text, true, rollout).await
    }

    async fn run_inner(
        &self,
        session: &mut Session,
        user_text: &str,
        plan: bool,
        rollout: &mut Option<RolloutWriter>,
    ) -> StopReason {
        let turn_id = grokforge_protocol::TurnId::new();
        self.emit(EventMsg::TurnStarted { turn_id });

        // In plan mode, instruct the model not to change anything.
        let effective_text = if plan {
            format!(
                "[PLAN MODE — do not modify files or run mutating commands; produce a concise, \
                 numbered plan for the following task]\n\n{user_text}"
            )
        } else {
            user_text.to_string()
        };

        // User input is redacted at ingress (a pasted secret must not enter the transcript).
        let user_red = Redactor::apply(&effective_text);
        self.record(session, rollout, ResponseItem::user(user_red.text))
            .await;

        // Plan mode enforces read-only tools and a read-only sandbox regardless of preset.
        let mut tool_defs = if plan {
            self.registry.readonly_tool_defs()
        } else {
            self.registry.tool_defs()
        };
        // Subagents (and plan mode) do not offer the subagent-spawning tool.
        if !self.allow_subagents || plan {
            tool_defs.retain(|d| d.function_name() != Some(crate::tools::builtins::SPAWN_TASK));
        }
        let ctx = if plan {
            self.turn_context_readonly(session)
        } else {
            self.turn_context(session)
        };
        let agents = agents_md::discover(&session.config.workspace_root);

        let mut iteration = 0u32;
        let stop = loop {
            if iteration >= session.config.max_iterations {
                break StopReason::MaxIterations;
            }
            iteration += 1;

            let Assembled {
                request, ledger, ..
            } = match context::assemble(session, &agents, tool_defs.clone()) {
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

            let stream = match self.client.stream(&request).await {
                Ok(s) => s,
                Err(e) => {
                    self.emit(EventMsg::Error {
                        message: format!("model request failed: {e}"),
                        recoverable: e.is_retriable(),
                    });
                    break StopReason::Error;
                }
            };

            let (assistant_text, tool_calls, usage) = self.consume_response(stream).await;
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

        // Auto-commit the agent's edits from the trusted host process (never in the sandbox).
        if session.config.auto_commit {
            self.auto_commit(session, turn_id, &ctx).await;
        }

        // Keep the model-visible window bounded on long sessions.
        if session.config.auto_compact {
            self.compact(session).await;
        }

        self.emit(EventMsg::TurnComplete {
            turn_id,
            stop: stop.clone(),
        });
        stop
    }

    /// Drain a model response stream: emit text/reasoning deltas and collect the final text,
    /// the requested tool calls, and usage.
    #[allow(clippy::type_complexity)]
    async fn consume_response(
        &self,
        mut stream: grokforge_xai::ResponseStream,
    ) -> (String, Vec<(ToolCallId, String, String)>, Usage) {
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
                    tool_calls.push((ToolCallId::new(), call.name, call.arguments));
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
        (assistant_text, tool_calls, usage)
    }

    /// Compact history if it has grown past the threshold, replacing older items with a
    /// model-written summary plus mechanically-extracted verbatim paths/errors. Returns whether
    /// compaction happened. Public so a `/compact` command can force it.
    pub async fn compact(&self, session: &mut Session) -> bool {
        let trigger = session.config.compaction_trigger_bytes;
        let keep = session.config.compaction_keep_tail;
        if !compaction::should_compact(&session.history, trigger, keep) {
            return false;
        }
        let split = session.history.len().saturating_sub(keep);
        let older = &session.history[..split];
        let (files, errors) = compaction::extract_verbatim(older);
        let transcript = compaction::transcript_text(older);

        let req = ResponsesRequest::new(
            session.config.model.clone(),
            vec![
                InputItem::text(
                    Role::Developer,
                    "Summarize the following conversation so the assistant can continue the task. \
                     Capture decisions, current state, and open work. Be concise.",
                ),
                InputItem::text(Role::User, transcript),
            ],
        );
        let summary = self.collect_text(&req).await;
        let summary_item = compaction::build_summary_item(&summary, &files, &errors);

        let tail = session.history.split_off(split);
        session.history.clear();
        session.history.push(summary_item);
        session.history.extend(tail);
        tracing::info!("compacted {} items into a summary", split);
        true
    }

    /// Stream a request and collect its assistant text (used for summaries/commit messages).
    async fn collect_text(&self, req: &ResponsesRequest) -> String {
        let mut text = String::new();
        match self.client.stream(req).await {
            Ok(mut stream) => {
                while let Some(ev) = stream.next().await {
                    if let Ok(StreamEvent::TextDelta(d)) = ev {
                        text.push_str(&d);
                    }
                }
            }
            Err(e) => tracing::warn!("summary request failed: {e}"),
        }
        text
    }

    /// Commit files the agent wrote this turn, staging only those paths, from the host process.
    async fn auto_commit(&self, session: &Session, turn_id: TurnId, ctx: &TurnContext) {
        let touched: Vec<std::path::PathBuf> = ctx
            .touched_paths()
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        if touched.is_empty() {
            return;
        }
        let Some(git) = grokforge_git::Git::discover(&session.config.workspace_root) else {
            return;
        };
        let message = commit_message(&touched);
        let session_id = session.id;
        let result = tokio::task::spawn_blocking(move || {
            git.agent_commit(&touched, &message, session_id, turn_id)
                .map(|sha| (sha, message))
        })
        .await;
        match result {
            Ok(Ok((Some(sha), message))) => {
                self.emit(EventMsg::Committed { sha, message });
            }
            Ok(Ok((None, _))) => {}
            Ok(Err(e)) => tracing::warn!("auto-commit failed: {e}"),
            Err(e) => tracing::warn!("auto-commit task failed: {e}"),
        }
    }

    /// Run a subagent in an isolated git worktree with a fresh sibling agent (depth cap 1). The
    /// subagent's commits land on a `gf/agent/<id>` branch for the parent/user to review or merge;
    /// we do not auto-merge (conflict resolution is a later enhancement).
    ///
    /// Declared with a boxed `+ Send` return type (not `async fn`) to break the async-recursion
    /// auto-trait cycle `run_turn -> run_tool_call -> spawn_subagent -> run_turn`.
    fn spawn_subagent<'a>(
        &'a self,
        session: &'a Session,
        args: &'a serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let prompt = args
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if prompt.is_empty() {
                return ToolOutput::failure("spawn_task requires a non-empty `prompt`");
            }
            let workspace = session.config.workspace_root.clone();
            let Some(git) = grokforge_git::Git::discover(&workspace) else {
                return ToolOutput::failure("subagents require a git repository");
            };
            let base = git.head_sha().unwrap_or_else(|_| "HEAD".to_string());
            let id = ToolCallId::new().to_string();
            let worktree = workspace.join(".grokforge/worktrees").join(&id);
            let branch = format!("gf/agent/{id}");

            let (git_c, wt_c, br_c) = (git.clone(), worktree.clone(), branch.clone());
            match tokio::task::spawn_blocking(move || git_c.worktree_add(&wt_c, &br_c, "HEAD"))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    return ToolOutput::failure(format!("could not create worktree: {e}"));
                }
                Err(e) => return ToolOutput::failure(format!("worktree task failed: {e}")),
            }

            // Run the subagent turn in the worktree with its own event channel.
            let (sub_tx, mut sub_rx) = tokio::sync::mpsc::unbounded_channel();
            let sub_agent = self.for_subagent(sub_tx);
            let sub_config =
                crate::session::SessionConfig::new(worktree.clone(), session.config.model.clone())
                    .with_policy(session.config.approval_policy, session.config.sandbox_mode);
            let mut sub_session = crate::session::Session::new(sub_config);
            // Box with an explicit `+ Send` trait object to break the async-recursion type cycle
            // (run_turn -> spawn_subagent -> run_turn).
            let sub_fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
                Box::pin(async move {
                    sub_agent
                        .run_turn(&mut sub_session, &prompt, &mut None)
                        .await;
                });
            let handle = tokio::spawn(sub_fut);
            let mut final_text = String::new();
            while let Some(ev) = sub_rx.recv().await {
                if let EventMsg::AgentMessageDone { text } = ev {
                    final_text = text;
                }
            }
            let _ = handle.await;

            // Capture the change summary, then remove the worktree (keeping the branch).
            let (git_d, wt_d, base_d) = (git.clone(), worktree.clone(), base.clone());
            let diff = tokio::task::spawn_blocking(move || {
                let d = grokforge_git::Git::discover(&wt_d)
                    .and_then(|g| g.diff_stat(&format!("{base_d}..HEAD")).ok())
                    .unwrap_or_default();
                let _ = git_d.worktree_remove(&wt_d);
                d
            })
            .await
            .unwrap_or_default();

            let changes = if diff.trim().is_empty() {
                "(no changes)".to_string()
            } else {
                diff.trim().to_string()
            };
            ToolOutput::success(format!(
                "Subagent finished on branch `{branch}` (review or merge it manually).\n\nResult:\n{final_text}\n\nChanges:\n{changes}"
            ))
        })
    }

    async fn handle_spawn_task(
        &self,
        session: &mut Session,
        rollout: &mut Option<RolloutWriter>,
        call_id: ToolCallId,
        name: &str,
        arguments: &str,
    ) {
        let args: serde_json::Value =
            serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null);
        self.emit(EventMsg::ToolCallBegin {
            call_id,
            name: name.to_string(),
            args_preview: preview(arguments),
            sandboxed: true,
        });
        let output = self.spawn_subagent(session, &args).await;
        let red = Redactor::apply(output.content());
        let is_error = output.is_error();
        self.emit(EventMsg::ToolCallEnd {
            call_id,
            ok: !is_error,
            summary: summarize(&red.text),
            denial: None,
        });
        self.record_tool_result(session, rollout, call_id, &red.text, is_error)
            .await;
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

        // The subagent tool is intercepted by the runtime (it needs the Agent itself).
        if name == crate::tools::builtins::SPAWN_TASK && self.allow_subagents {
            self.handle_spawn_task(session, rollout, call_id, name, arguments)
                .await;
            return;
        }

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

/// A heuristic commit subject. Model-generated conventional-commit messages (via structured
/// outputs) are a planned refinement; this keeps the git-native workflow self-contained.
fn commit_message(touched: &[std::path::PathBuf]) -> String {
    let names: Vec<String> = touched
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    match names.as_slice() {
        [] => "grokforge: update files".to_string(),
        [one] => format!("grokforge: update {one}"),
        many => {
            let shown: Vec<&str> = many.iter().take(3).map(String::as_str).collect();
            let more = if many.len() > 3 {
                format!(", +{}", many.len() - 3)
            } else {
                String::new()
            };
            format!(
                "grokforge: update {} files ({}{more})",
                many.len(),
                shown.join(", ")
            )
        }
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
