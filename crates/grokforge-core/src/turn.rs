//! The agent loop. One [`Agent`] drives turns on a [`Session`]: assemble context → stream the
//! model → run tool calls (gated by the approval engine, redacted on the way back) → repeat
//! until the model stops or the iteration cap is hit. Every step emits an [`EventMsg`] and is
//! appended to the canonical JSONL rollout.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use futures::StreamExt;
use grokforge_protocol::{
    ApprovalId, ApprovalKind, ApprovalPolicy, ApprovalRequest, Decision, EventMsg, LedgerEntry,
    ResponseItem, SandboxMode, SandboxPolicy, StopReason, ToolCallId, TurnId, Usage,
};
use grokforge_sandbox::SandboxRunner;
use grokforge_xai::{ServerTool, StreamEvent, ToolDef, XaiClient};
use tokio::sync::mpsc::UnboundedSender;

use crate::agents_md;
use crate::approvals::{Approver, Gate, gate};
use crate::cancellation::TurnCancellation;
use crate::compaction;
use crate::context::{self, Assembled};
use crate::redaction::Redactor;
use crate::session::Session;
use crate::skills;
use crate::store::RolloutWriter;
use crate::tools::{MAX_TOOLS, ToolInvocation, ToolOutput, ToolRegistry, TurnContext};

/// Upper bound on subagents a single turn may spawn. `spawn_task` calls that the model emits in
/// one response run concurrently (each in its own git worktree); this caps the fan-out so a turn
/// cannot launch an unbounded number of parallel API loops and worktrees.
const MAX_SUBAGENTS_PER_TURN: usize = 32;
/// Bound both retained transcript text and the amount of delta state a frontend may queue for a
/// single response. This stays comfortably below the rollout line limit after JSON escaping.
const MAX_RESPONSE_TEXT_BYTES: usize = 8 * 1024 * 1024;
const MAX_USER_TEXT_BYTES: usize = 8 * 1024 * 1024;
const MAX_REASONING_DELTA_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_EVENTS: usize = 32_768;
const MAX_PROVIDER_OUTPUT_BYTES: usize = 15 * 1024 * 1024;
const MAX_PROVIDER_OUTPUT_AGGREGATE_BYTES: usize = 24 * 1024 * 1024;
const MAX_PROVIDER_OUTPUT_ITEMS: usize = 512;
const MAX_TOOL_CALLS_PER_RESPONSE: usize = 64;
// Compaction must remain a recovery path even when resumed history is near its in-memory cap.
// Four MiB stays below the 32 MiB request cap even under worst-case JSON escaping.
const MAX_COMPACTION_TRANSCRIPT_BYTES: usize = 4 * 1024 * 1024;
// Keep the summary itself bounded so a coherent recent tail still has room in the durable line.
const MAX_COMPACTION_SUMMARY_ITEM_BYTES: usize = 7 * 1024 * 1024;
const MAX_COMPACTION_TAIL_BYTES: usize = 7 * 1024 * 1024;
// Rollout records are capped at 16 MiB; reserve a full MiB for serialization/newline overhead.
const MAX_COMPACTION_CHECKPOINT_BYTES: usize = 15 * 1024 * 1024;
const MAX_COMPACTION_TAIL_CANDIDATES: usize = 1_024;

/// Add the explicitly enabled provider-executed tools while preserving the API's combined tool
/// limit. Client definitions are already sorted with built-ins first, so any required truncation
/// drops only the tail of the optional/MCP surface in practical registries.
fn append_server_tools(tool_defs: &mut Vec<ToolDef>, enabled_server_tools: &BTreeSet<ServerTool>) {
    let client_budget = MAX_TOOLS.saturating_sub(enabled_server_tools.len());
    tool_defs.truncate(client_budget);
    tool_defs.extend(
        enabled_server_tools
            .iter()
            .copied()
            .map(ServerTool::definition),
    );
}

fn advertised_tool_defs(
    registry: &ToolRegistry,
    plan: bool,
    allow_subagents: bool,
    enabled_server_tools: &BTreeSet<ServerTool>,
) -> Vec<ToolDef> {
    let mut tool_defs = if plan {
        registry.readonly_tool_defs()
    } else {
        registry.tool_defs()
    };
    // Subagents (and plan mode) do not offer the subagent-spawning tool.
    if !allow_subagents || plan {
        tool_defs.retain(|definition| {
            definition.function_name() != Some(crate::tools::builtins::SPAWN_TASK)
        });
    }
    // Server tools are separately metered and stay off unless session configuration opted in.
    // Plan mode deliberately remains local/read-only and never advertises provider tools.
    if !plan {
        append_server_tools(&mut tool_defs, enabled_server_tools);
    }
    tool_defs
}

#[derive(Debug)]
struct ConsumedResponse {
    assistant_text: String,
    tool_calls: Vec<(usize, ToolCallId, String, String)>,
    provider_outputs: Vec<(usize, serde_json::Value)>,
    encrypted_reasoning: Vec<(usize, ResponseItem)>,
    usage: Usage,
    terminal: grokforge_xai::StopReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolCallFlow {
    Continue,
    Abort,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsumeError {
    Cancelled,
    Failed,
}

/// Tokio detaches a task when its join handle is dropped. Subagents instead belong to their
/// parent turn: cancellation of the parent must promptly cancel the nested API loop too.
struct AbortOnDrop<T> {
    handle: Option<tokio::task::JoinHandle<T>>,
}

impl<T> AbortOnDrop<T> {
    fn new(handle: tokio::task::JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    async fn join(mut self) -> Result<T, String> {
        let Some(handle) = self.handle.take() else {
            return Err("subagent join handle was already consumed".to_string());
        };
        handle.await.map_err(|error| error.to_string())
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// A subagent prepared by [`Agent::setup_subagent`] and ready to run concurrently with its
/// siblings. It owns everything the run needs so the parent's `&Session` is not borrowed during
/// the parallel phase.
struct SubagentJob {
    /// The spawning tool call's id, used as the stable lane id in per-agent events.
    call_id: ToolCallId,
    /// Short preview of the subtask prompt, shown as the lane label.
    label: String,
    prompt: String,
    git: grokforge_git::Git,
    worktree: std::path::PathBuf,
    branch: String,
    base: String,
    sub_agent: Agent,
    sub_session: Session,
    sub_rollout: Option<RolloutWriter>,
    sub_rx: tokio::sync::mpsc::UnboundedReceiver<EventMsg>,
}

/// Outcome of admitting a single `spawn_task` call before the parallel run.
enum SpawnAdmit {
    /// Approved and announced; carries the parsed arguments for setup.
    Approved(serde_json::Value),
    /// Rejected with a failure output already recorded in the transcript.
    Rejected,
    /// The user aborted the turn during approval.
    Abort,
    /// A durable write failed; the turn must end with an error.
    Error,
}

/// Remove a subagent's worktree, ignoring errors (best-effort cleanup on a setup failure path).
async fn remove_worktree(git: grokforge_git::Git, worktree: std::path::PathBuf) {
    let _ = tokio::task::spawn_blocking(move || git.worktree_remove(&worktree)).await;
}

/// A compact one-line label for a subagent lane, derived from its prompt.
fn subagent_label(prompt: &str) -> String {
    const MAX_LABEL_CHARS: usize = 56;
    let collapsed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > MAX_LABEL_CHARS {
        let truncated: String = collapsed.chars().take(MAX_LABEL_CHARS - 1).collect();
        format!("{truncated}…")
    } else {
        collapsed
    }
}

/// A short status line for a finished subagent lane, derived from its tool output.
fn summarize_subagent(branch: &str, output: &ToolOutput) -> String {
    const MAX_CHARS: usize = 100;
    let first = output
        .content()
        .lines()
        .find(|line| !line.trim().is_empty());
    let base = match first {
        Some(line) => line.trim().to_string(),
        None if output.is_error() => "subagent failed".to_string(),
        None => format!("finished on {branch}"),
    };
    if base.chars().count() > MAX_CHARS {
        let truncated: String = base.chars().take(MAX_CHARS - 1).collect();
        format!("{truncated}…")
    } else {
        base
    }
}

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

    fn turn_context(
        &self,
        session: &Session,
        cancellation: &TurnCancellation,
    ) -> Result<TurnContext, String> {
        let root = session.config.workspace_root.clone();
        let policy = sandbox_policy(
            &root,
            session.config.sandbox_mode,
            session.config.network,
            session.config.isolated_worktree,
        )?;
        Ok(TurnContext {
            workspace_root: root,
            policy,
            sandbox: Arc::clone(&self.sandbox),
            touched: Arc::new(std::sync::Mutex::new(Vec::new())),
            bound_write_targets: Vec::new(),
            cancellation: cancellation.clone(),
        })
    }

    /// Like [`Self::turn_context`] but forces a read-only sandbox policy (plan mode).
    fn turn_context_readonly(
        &self,
        session: &Session,
        cancellation: &TurnCancellation,
    ) -> Result<TurnContext, String> {
        let root = session.config.workspace_root.clone();
        Ok(TurnContext {
            policy: sandbox_policy(
                &root,
                SandboxMode::ReadOnly,
                session.config.network,
                session.config.isolated_worktree,
            )?,
            workspace_root: root,
            sandbox: Arc::clone(&self.sandbox),
            touched: Arc::new(std::sync::Mutex::new(Vec::new())),
            bound_write_targets: Vec::new(),
            cancellation: cancellation.clone(),
        })
    }

    fn elevated_context(ctx: &TurnContext) -> TurnContext {
        // Filesystem/exec approval is not network approval. Keep an enforcing sandbox mode with
        // broad filesystem roots while preserving the original network, secret-read, and Git
        // metadata restrictions. Only `escalation_context(Network)` widens network access.
        let mut policy = ctx.policy.clone();
        policy.mode = SandboxMode::WorkspaceWrite;
        policy.writable_roots = vec![std::path::PathBuf::from("/")];
        policy.readable_roots = vec![std::path::PathBuf::from("/")];
        TurnContext {
            workspace_root: ctx.workspace_root.clone(),
            policy,
            sandbox: Arc::clone(&ctx.sandbox),
            touched: Arc::clone(&ctx.touched),
            bound_write_targets: Vec::new(),
            cancellation: ctx.cancellation.clone(),
        }
    }

    fn elevated_write_context(ctx: &TurnContext, targets: &[std::path::PathBuf]) -> TurnContext {
        let mut elevated = Self::elevated_context(ctx);
        elevated.bound_write_targets = targets.to_vec();
        elevated
    }

    fn escalation_context(
        ctx: &TurnContext,
        denial: grokforge_protocol::DenialClass,
    ) -> TurnContext {
        match denial {
            grokforge_protocol::DenialClass::Network => {
                let mut policy = ctx.policy.clone();
                policy.network = grokforge_protocol::NetworkMode::Full;
                TurnContext {
                    workspace_root: ctx.workspace_root.clone(),
                    policy,
                    sandbox: Arc::clone(&ctx.sandbox),
                    touched: Arc::clone(&ctx.touched),
                    bound_write_targets: Vec::new(),
                    cancellation: ctx.cancellation.clone(),
                }
            }
            grokforge_protocol::DenialClass::FsWrite => Self::elevated_context(ctx),
            // These capabilities cannot be widened narrowly with the current SandboxPolicy.
            // `escalation_kind` refuses them, and this conservative fallback preserves policy.
            grokforge_protocol::DenialClass::FsRead | grokforge_protocol::DenialClass::Signal => {
                TurnContext {
                    workspace_root: ctx.workspace_root.clone(),
                    policy: ctx.policy.clone(),
                    sandbox: Arc::clone(&ctx.sandbox),
                    touched: Arc::clone(&ctx.touched),
                    bound_write_targets: Vec::new(),
                    cancellation: ctx.cancellation.clone(),
                }
            }
        }
    }

    async fn open_stream_accounted(
        &self,
        request: &grokforge_xai::ResponsesRequest,
    ) -> Result<grokforge_xai::ResponseStream, grokforge_xai::XaiError> {
        let events = self.events.clone();
        self.client
            .stream_with_attempt_observer(request, move |attempt| {
                if attempt.number > 1 {
                    let _ = events.send(EventMsg::LedgerAppended(LedgerEntry::new(
                        format!("request_retry_{}", attempt.number),
                        attempt.request_bytes,
                        "transport retry",
                    )));
                }
            })
            .await
    }

    /// Run one turn to completion (execute mode).
    pub async fn run_turn(
        &self,
        session: &mut Session,
        user_text: &str,
        rollout: &mut Option<RolloutWriter>,
    ) -> StopReason {
        self.run_turn_cancellable(session, user_text, rollout, &TurnCancellation::new())
            .await
    }

    /// Run one execute-mode turn with cooperative cancellation controlled by the frontend.
    pub async fn run_turn_cancellable(
        &self,
        session: &mut Session,
        user_text: &str,
        rollout: &mut Option<RolloutWriter>,
        cancellation: &TurnCancellation,
    ) -> StopReason {
        self.run_inner(session, user_text, false, rollout, cancellation)
            .await
    }

    /// Run a plan-mode turn: read-only tools + read-only sandbox + a planning preamble, so the
    /// agent produces a plan without changing anything.
    pub async fn run_plan_turn(
        &self,
        session: &mut Session,
        user_text: &str,
        rollout: &mut Option<RolloutWriter>,
    ) -> StopReason {
        self.run_plan_turn_cancellable(session, user_text, rollout, &TurnCancellation::new())
            .await
    }

    /// Run one plan-mode turn with cooperative cancellation controlled by the frontend.
    pub async fn run_plan_turn_cancellable(
        &self,
        session: &mut Session,
        user_text: &str,
        rollout: &mut Option<RolloutWriter>,
        cancellation: &TurnCancellation,
    ) -> StopReason {
        self.run_inner(session, user_text, true, rollout, cancellation)
            .await
    }

    #[allow(clippy::too_many_lines)]
    async fn run_inner(
        &self,
        session: &mut Session,
        user_text: &str,
        plan: bool,
        rollout: &mut Option<RolloutWriter>,
        cancellation: &TurnCancellation,
    ) -> StopReason {
        let turn_id = grokforge_protocol::TurnId::new();
        self.emit(EventMsg::TurnStarted { turn_id });
        let clean_repo_at_start = session.config.isolated_worktree
            && grokforge_git::Git::discover(&session.config.workspace_root)
                .and_then(|git| git.is_dirty().ok())
                == Some(false);

        if cancellation.is_cancelled() {
            let stop = StopReason::Interrupted;
            self.emit(EventMsg::TurnComplete {
                turn_id,
                stop: stop.clone(),
            });
            return stop;
        }

        // In plan mode, instruct the model not to change anything.
        let effective_text = if plan {
            format!(
                "[PLAN MODE — do not modify files or run mutating commands; produce a concise, \
                 numbered plan for the following task]\n\n{user_text}"
            )
        } else {
            user_text.to_string()
        };
        if effective_text.len() > MAX_USER_TEXT_BYTES {
            let stop = StopReason::Error;
            self.emit(EventMsg::Error {
                message: format!("user input exceeded the {MAX_USER_TEXT_BYTES}-byte safety limit"),
                recoverable: false,
            });
            self.emit(EventMsg::TurnComplete {
                turn_id,
                stop: stop.clone(),
            });
            return stop;
        }

        // User input is redacted at ingress (a pasted secret must not enter the transcript).
        let user_red = Redactor::apply(&effective_text);
        if self
            .record(
                session,
                rollout,
                ResponseItem::user_redacted(user_red.text, user_red.count),
            )
            .await
            .is_err()
        {
            let stop = StopReason::Error;
            self.emit(EventMsg::TurnComplete {
                turn_id,
                stop: stop.clone(),
            });
            return stop;
        }

        // Plan mode enforces read-only tools and a read-only sandbox regardless of preset.
        let tool_defs = advertised_tool_defs(
            &self.registry,
            plan,
            self.allow_subagents,
            &session.config.enabled_server_tools,
        );
        let offered_tools: BTreeSet<String> = tool_defs
            .iter()
            .filter_map(|definition| definition.function_name().map(str::to_string))
            .collect();
        let ctx = match if plan {
            self.turn_context_readonly(session, cancellation)
        } else {
            self.turn_context(session, cancellation)
        } {
            Ok(ctx) => ctx,
            Err(message) => {
                let stop = StopReason::Error;
                self.emit(EventMsg::Error {
                    message: format!("could not construct a safe sandbox policy: {message}"),
                    recoverable: false,
                });
                self.emit(EventMsg::TurnComplete {
                    turn_id,
                    stop: stop.clone(),
                });
                return stop;
            }
        };
        let agents = agents_md::discover(&session.config.workspace_root);
        let skills = skills::discover(&session.config.workspace_root);
        let memory = crate::memory::discover(&session.config.workspace_root);

        let mut iteration = 0u32;
        let mut spawned = 0usize;
        let mut auto_commit_has_exclusive_ownership = true;
        let mut compacted_baseline = None;
        let mut compaction_failed = false;
        // The largest request body (in bytes) that stays under the model's prompt-token limit.
        let budget = session.config.input_budget_bytes();
        // Set while a single forced budget-recovery compaction is outstanding, so the guard cannot
        // loop forever when the oversize content cannot be compacted away.
        let mut budget_recovery_used = false;
        let mut stop = 'agent: loop {
            if cancellation.is_cancelled() {
                break StopReason::Interrupted;
            }
            // A single accepted provider/tool round can push replay over the next request's
            // 32 MiB cap. Compact before every request, including the first request of a resumed
            // oversized session, so compaction remains a recovery path rather than an end-turn
            // best effort.
            // Reserve headroom inside the budget for non-history context (system prompt,
            // AGENTS.md, skills, tool defs) so history alone never fills the whole window.
            let history_ceiling = budget.saturating_sub(budget / 4).max(1);
            let compaction_threshold = compacted_baseline
                .map_or(
                    session.config.compaction_trigger_bytes,
                    |baseline: usize| {
                        baseline.saturating_add(session.config.compaction_trigger_bytes)
                    },
                )
                // The baseline-relative threshold otherwise grows every compaction; cap it at the
                // model's absolute budget so a long session cannot creep past the token limit.
                .min(history_ceiling);
            if session.config.auto_compact
                && !compaction_failed
                && compaction::should_compact(
                    &session.history,
                    compaction_threshold,
                    session.config.compaction_keep_tail,
                )
            {
                if self
                    .compact_inner(session, rollout.as_mut(), cancellation)
                    .await
                {
                    compacted_baseline = Some(compaction::estimate_bytes(&session.history));
                } else {
                    // Do not hammer the summarization endpoint again in the same turn. The
                    // ordinary request still gets its exact size check and fails closed if the
                    // un-compacted history cannot be replayed.
                    compaction_failed = true;
                }
            }
            if cancellation.is_cancelled() {
                break StopReason::Interrupted;
            }
            if iteration >= session.config.max_iterations {
                break StopReason::MaxIterations;
            }
            iteration += 1;

            let assembled =
                match context::assemble(session, &agents, &memory, &skills, tool_defs.clone()) {
                    Ok(a) => a,
                    Err(e) => {
                        self.emit(EventMsg::Error {
                            message: format!("failed to assemble request: {e}"),
                            recoverable: false,
                        });
                        break StopReason::Error;
                    }
                };

            // Hard budget guard: never send a request that exceeds the model's input-token budget.
            // The provider rejects an oversize prompt with a 400 that aborts the whole turn, so
            // instead recover with one forced compaction; if it still does not fit, stop with an
            // actionable message rather than a raw provider error.
            if assembled.body_len > budget {
                if session.config.auto_compact
                    && !budget_recovery_used
                    && !compaction_failed
                    && session.history.len() > 1
                {
                    budget_recovery_used = true;
                    if self
                        .compact_inner(session, rollout.as_mut(), cancellation)
                        .await
                    {
                        compacted_baseline = Some(compaction::estimate_bytes(&session.history));
                    } else {
                        compaction_failed = true;
                    }
                    if cancellation.is_cancelled() {
                        break StopReason::Interrupted;
                    }
                    continue 'agent;
                }
                self.emit(EventMsg::Error {
                    message: format!(
                        "the request is about {} KB, over this model's ~{} KB input budget even after compaction — start a new session (/new) or remove large pasted or attached content",
                        assembled.body_len / 1024,
                        budget / 1024
                    ),
                    recoverable: false,
                });
                break StopReason::Error;
            }
            // The request fits; allow a fresh forced compaction if history grows again later.
            budget_recovery_used = false;

            let Assembled {
                request, ledger, ..
            } = assembled;
            for entry in ledger.entries {
                self.emit(EventMsg::LedgerAppended(entry));
            }

            let stream = match tokio::select! {
                result = self.open_stream_accounted(&request) => Some(result),
                () = cancellation.cancelled() => None,
            } {
                None => break StopReason::Interrupted,
                Some(result) => match result {
                    Ok(s) => s,
                    Err(e) => {
                        self.emit(EventMsg::Error {
                            message: format!("model request failed: {e}"),
                            recoverable: e.is_retriable(),
                        });
                        break StopReason::Error;
                    }
                },
            };

            let response = match self.consume_response(stream, cancellation).await {
                Ok(response) => response,
                Err(ConsumeError::Cancelled) => break StopReason::Interrupted,
                Err(ConsumeError::Failed) => break StopReason::Error,
            };
            if let Err(message) = validate_provider_response(&response, &session.history) {
                self.emit(EventMsg::Error {
                    message,
                    recoverable: false,
                });
                break StopReason::Error;
            }
            self.emit(EventMsg::TokenUsage {
                usage: response.usage,
            });

            let raw_call_ids: BTreeSet<String> = response
                .provider_outputs
                .iter()
                .filter(|(_, item)| {
                    item.get("type").and_then(serde_json::Value::as_str) == Some("function_call")
                })
                .filter_map(|(_, item)| {
                    item.get("call_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
                .collect();
            let raw_reasoning_ids: BTreeSet<String> = response
                .provider_outputs
                .iter()
                .filter(|(_, item)| {
                    item.get("type").and_then(serde_json::Value::as_str) == Some("reasoning")
                })
                .filter_map(|(_, item)| {
                    item.get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
                .collect();
            let raw_has_assistant_message = response.provider_outputs.iter().any(|(_, item)| {
                item.get("type").and_then(serde_json::Value::as_str) == Some("message")
            });
            for (_, item) in response.provider_outputs {
                if self
                    .record(session, rollout, ResponseItem::ProviderOutput { item })
                    .await
                    .is_err()
                {
                    break 'agent StopReason::Error;
                }
            }
            for (_, item) in response.encrypted_reasoning {
                let duplicate = matches!(
                    &item,
                    ResponseItem::EncryptedReasoning { id, .. } if raw_reasoning_ids.contains(id)
                );
                if !duplicate && self.record(session, rollout, item).await.is_err() {
                    break 'agent StopReason::Error;
                }
            }

            if !response.assistant_text.is_empty() {
                if !raw_has_assistant_message
                    && self
                        .record(
                            session,
                            rollout,
                            ResponseItem::assistant(response.assistant_text.clone()),
                        )
                        .await
                        .is_err()
                {
                    break StopReason::Error;
                }
                self.emit(EventMsg::AgentMessageDone {
                    text: response.assistant_text,
                });
            }

            match response.terminal {
                grokforge_xai::StopReason::MaxTokens => {
                    self.emit(EventMsg::Error {
                        message: "model response ended at the output-token limit".to_string(),
                        recoverable: true,
                    });
                    break StopReason::Error;
                }
                grokforge_xai::StopReason::Other(status) => {
                    self.emit(EventMsg::Error {
                        message: format!("model response ended with status `{status}`"),
                        recoverable: false,
                    });
                    break StopReason::Error;
                }
                grokforge_xai::StopReason::ToolCalls if response.tool_calls.is_empty() => {
                    self.emit(EventMsg::Error {
                        message: "model requested continuation without any tool calls".to_string(),
                        recoverable: false,
                    });
                    break StopReason::Error;
                }
                grokforge_xai::StopReason::EndTurn if response.tool_calls.is_empty() => {
                    break StopReason::EndTurn;
                }
                grokforge_xai::StopReason::EndTurn | grokforge_xai::StopReason::ToolCalls => {}
            }

            let calls = response.tool_calls;
            let mut cursor = 0;
            while cursor < calls.len() {
                // Subagent spawns that the model requested together run concurrently: gather the
                // maximal run of `spawn_task` calls and dispatch them as one parallel batch. Every
                // other tool runs sequentially in place (mutating tools must serialize).
                if calls[cursor].2 == crate::tools::builtins::SPAWN_TASK {
                    let start = cursor;
                    while cursor < calls.len()
                        && calls[cursor].2 == crate::tools::builtins::SPAWN_TASK
                    {
                        cursor += 1;
                    }
                    match self
                        .run_spawn_batch(
                            session,
                            rollout,
                            &ctx,
                            &calls[start..cursor],
                            &raw_call_ids,
                            offered_tools.contains(crate::tools::builtins::SPAWN_TASK),
                            &mut spawned,
                            &mut auto_commit_has_exclusive_ownership,
                            cancellation,
                        )
                        .await
                    {
                        ToolCallFlow::Continue => {}
                        ToolCallFlow::Abort => break 'agent StopReason::Interrupted,
                        ToolCallFlow::Error => break 'agent StopReason::Error,
                    }
                    continue;
                }

                let (_, call_id, name, arguments) = &calls[cursor];
                cursor += 1;
                auto_commit_has_exclusive_ownership &= tool_preserves_auto_commit_ownership(name);
                let record_call = !raw_call_ids.contains(call_id.as_str());
                match self
                    .run_tool_call(
                        session,
                        rollout,
                        &ctx,
                        call_id.clone(),
                        name,
                        arguments,
                        record_call,
                        offered_tools.contains(name),
                        true,
                        cancellation,
                    )
                    .await
                {
                    ToolCallFlow::Continue => {}
                    ToolCallFlow::Abort => break 'agent StopReason::Interrupted,
                    ToolCallFlow::Error => break 'agent StopReason::Error,
                }
            }
        };

        if cancellation.is_cancelled() {
            stop = StopReason::Interrupted;
        }

        // Raw provider calls are persisted before execution so stateless replay remains exact.
        // Any early terminal status, user abort, append failure, or cancellation between parallel
        // calls must therefore close every durable call with a deterministic failed output.
        for repair in crate::store::interrupted_tool_results(&session.history) {
            if self.record(session, rollout, repair).await.is_err() {
                stop = StopReason::Error;
                break;
            }
        }

        // Auto-commit the agent's edits from the trusted host process (never in the sandbox).
        if !plan
            && session.config.auto_commit
            && session.config.isolated_worktree
            && auto_commit_has_exclusive_ownership
            && stop == StopReason::EndTurn
        {
            self.auto_commit(session, turn_id, clean_repo_at_start, ctx.touched_paths())
                .await;
        }

        // Keep the model-visible window bounded on long sessions.
        let end_threshold = compacted_baseline
            .map_or(session.config.compaction_trigger_bytes, |baseline| {
                baseline.saturating_add(session.config.compaction_trigger_bytes)
            });
        if session.config.auto_compact
            && !compaction_failed
            && stop == StopReason::EndTurn
            && compaction::should_compact(
                &session.history,
                end_threshold,
                session.config.compaction_keep_tail,
            )
        {
            self.compact_inner(session, rollout.as_mut(), cancellation)
                .await;
        }

        if cancellation.is_cancelled() {
            stop = StopReason::Interrupted;
        }

        self.emit(EventMsg::TurnComplete {
            turn_id,
            stop: stop.clone(),
        });
        stop
    }

    /// Drain a model response stream: emit text/reasoning deltas and collect the final text,
    /// the requested tool calls, and usage.
    #[allow(clippy::too_many_lines)]
    async fn consume_response(
        &self,
        mut stream: grokforge_xai::ResponseStream,
        cancellation: &TurnCancellation,
    ) -> Result<ConsumedResponse, ConsumeError> {
        let mut assistant_text = String::new();
        // `output_item.done` events may arrive out of order. Key them by the provider's
        // canonical output index, then validate and materialize them in array order only after
        // the terminal event. BTreeMap keeps sparse/malicious indices from causing allocation.
        let mut tool_calls: BTreeMap<usize, (ToolCallId, String, String)> = BTreeMap::new();
        let mut provider_outputs: BTreeMap<usize, serde_json::Value> = BTreeMap::new();
        let mut encrypted_reasoning: BTreeMap<usize, ResponseItem> = BTreeMap::new();
        let mut usage = Usage::default();
        let mut terminal = None;
        let mut event_count = 0usize;
        let mut reasoning_bytes = 0usize;
        let mut provider_output_bytes = 0usize;
        loop {
            let event = tokio::select! {
                event = stream.next() => event,
                () = cancellation.cancelled() => return Err(ConsumeError::Cancelled),
            };
            let Some(event) = event else {
                break;
            };
            event_count = event_count.saturating_add(1);
            if event_count > MAX_RESPONSE_EVENTS {
                self.emit(EventMsg::Error {
                    message: format!(
                        "model response exceeded the {MAX_RESPONSE_EVENTS}-event safety limit"
                    ),
                    recoverable: false,
                });
                return Err(ConsumeError::Failed);
            }
            match event {
                Ok(StreamEvent::TextDelta(d)) => {
                    if assistant_text
                        .len()
                        .checked_add(d.len())
                        .is_none_or(|len| len > MAX_RESPONSE_TEXT_BYTES)
                    {
                        self.emit(EventMsg::Error {
                            message: format!(
                                "assistant text exceeded the {MAX_RESPONSE_TEXT_BYTES}-byte safety limit"
                            ),
                            recoverable: false,
                        });
                        return Err(ConsumeError::Failed);
                    }
                    self.emit(EventMsg::AgentMessageDelta { delta: d.clone() });
                    assistant_text.push_str(&d);
                }
                Ok(StreamEvent::ReasoningDelta(d)) => {
                    reasoning_bytes = reasoning_bytes.saturating_add(d.len());
                    if reasoning_bytes > MAX_REASONING_DELTA_BYTES {
                        self.emit(EventMsg::Error {
                            message: format!(
                                "reasoning deltas exceeded the {MAX_REASONING_DELTA_BYTES}-byte safety limit"
                            ),
                            recoverable: false,
                        });
                        return Err(ConsumeError::Failed);
                    }
                    self.emit(EventMsg::ReasoningDelta { delta: d });
                }
                Ok(StreamEvent::ToolCall(call)) => {
                    if tool_calls.len() >= MAX_TOOL_CALLS_PER_RESPONSE
                        || tool_calls
                            .insert(
                                call.output_index,
                                (
                                    ToolCallId::from_raw(call.call_id),
                                    call.name,
                                    call.arguments,
                                ),
                            )
                            .is_some()
                    {
                        self.emit(EventMsg::Error {
                            message: format!(
                                "provider returned duplicate or excessive typed output index {}",
                                call.output_index
                            ),
                            recoverable: false,
                        });
                        return Err(ConsumeError::Failed);
                    }
                }
                Ok(StreamEvent::EncryptedReasoning(reasoning)) => {
                    let output_index = reasoning.output_index;
                    let item = ResponseItem::EncryptedReasoning {
                        id: reasoning.id,
                        status: reasoning.status,
                        summary: reasoning.summary,
                        encrypted_content: reasoning.encrypted_content,
                    };
                    if encrypted_reasoning.len() >= MAX_PROVIDER_OUTPUT_ITEMS
                        || encrypted_reasoning.insert(output_index, item).is_some()
                    {
                        self.emit(EventMsg::Error {
                            message: format!(
                                "provider returned duplicate or excessive reasoning output index {output_index}"
                            ),
                            recoverable: false,
                        });
                        return Err(ConsumeError::Failed);
                    }
                }
                Ok(StreamEvent::ProviderOutput { output_index, item }) => {
                    let serialized_bytes = match serde_json::to_vec(&item) {
                        Ok(serialized) => serialized.len(),
                        Err(error) => {
                            self.emit(EventMsg::Error {
                                message: format!(
                                    "provider output could not be serialized: {error}"
                                ),
                                recoverable: false,
                            });
                            return Err(ConsumeError::Failed);
                        }
                    };
                    provider_output_bytes = provider_output_bytes.saturating_add(serialized_bytes);
                    if serialized_bytes > MAX_PROVIDER_OUTPUT_BYTES
                        || provider_output_bytes > MAX_PROVIDER_OUTPUT_AGGREGATE_BYTES
                    {
                        self.emit(EventMsg::Error {
                            message: format!(
                                "provider output exceeded the per-item or {MAX_PROVIDER_OUTPUT_AGGREGATE_BYTES}-byte aggregate safety limit"
                            ),
                            recoverable: false,
                        });
                        return Err(ConsumeError::Failed);
                    }
                    if provider_outputs.len() >= MAX_PROVIDER_OUTPUT_ITEMS
                        || provider_outputs.insert(output_index, item).is_some()
                    {
                        self.emit(EventMsg::Error {
                            message: format!(
                                "provider returned duplicate or excessive output index {output_index}"
                            ),
                            recoverable: false,
                        });
                        return Err(ConsumeError::Failed);
                    }
                }
                Ok(StreamEvent::Usage(u)) => {
                    usage = Usage {
                        input_tokens: u.input_tokens,
                        cached_tokens: u.cached_tokens,
                        output_tokens: u.output_tokens,
                        reasoning_tokens: u.reasoning_tokens,
                    };
                }
                Ok(StreamEvent::Completed { stop }) => terminal = Some(stop),
                Ok(StreamEvent::Created { .. }) => {}
                Err(e) => {
                    self.emit(EventMsg::Error {
                        message: format!("stream error: {e}"),
                        recoverable: e.is_retriable(),
                    });
                    return Err(ConsumeError::Failed);
                }
            }
        }
        let Some(terminal) = terminal else {
            self.emit(EventMsg::Error {
                message: "model stream ended before response.completed".to_string(),
                recoverable: true,
            });
            return Err(ConsumeError::Failed);
        };
        if let Some((&actual, _)) = provider_outputs
            .iter()
            .enumerate()
            .find(|(expected, (actual, _))| *expected != **actual)
            .map(|(_, entry)| entry)
        {
            self.emit(EventMsg::Error {
                message: format!(
                    "provider output indices contain a gap before index {actual}; refusing ambiguous order"
                ),
                recoverable: false,
            });
            return Err(ConsumeError::Failed);
        }
        Ok(ConsumedResponse {
            assistant_text,
            tool_calls: tool_calls
                .into_iter()
                .map(|(index, (id, name, arguments))| (index, id, name, arguments))
                .collect(),
            provider_outputs: provider_outputs.into_iter().collect(),
            encrypted_reasoning: encrypted_reasoning.into_iter().collect(),
            usage,
            terminal,
        })
    }

    /// Compact history if it has grown past the threshold, replacing older items with a
    /// model-written summary plus mechanically-extracted verbatim paths/errors. Returns whether
    /// compaction happened. Public so a `/compact` command can force it.
    pub async fn compact(
        &self,
        session: &mut Session,
        rollout: &mut Option<RolloutWriter>,
    ) -> bool {
        self.compact_inner(session, rollout.as_mut(), &TurnCancellation::new())
            .await
    }

    async fn compact_inner(
        &self,
        session: &mut Session,
        mut rollout: Option<&mut RolloutWriter>,
        cancellation: &TurnCancellation,
    ) -> bool {
        if cancellation.is_cancelled() {
            return false;
        }
        let trigger = session.config.compaction_trigger_bytes;
        let keep = session.config.compaction_keep_tail;
        if !compaction::should_compact(&session.history, trigger, keep) {
            return false;
        }
        let initial_split = coherent_compaction_split(&session.history, keep);
        // If preserving the configured tail would pull a call/result group all the way to the
        // beginning, summarize the complete group rather than making compaction impossible.
        let desired_split = if initial_split == 0 {
            session.history.len()
        } else {
            initial_split
        };
        // Decide the durable coherent tail before asking for a summary. If the desired tail is
        // too large, every item we drop from it must move into the summary input; choosing after
        // the model call would silently lose that range.
        let Some(split) = choose_compaction_tail_split(
            &session.history,
            desired_split,
            keep,
            MAX_COMPACTION_TAIL_BYTES,
        ) else {
            tracing::warn!("compaction could not select a coherent durable tail");
            return false;
        };
        let older = &session.history[..split];
        let (files, errors) = compaction::extract_verbatim(older);
        let prior_redactions = compaction::redaction_count(older);
        let transcript =
            compaction::transcript_text_bounded(older, MAX_COMPACTION_TRANSCRIPT_BYTES);

        let assembled = match context::assemble_auxiliary(
            session,
            "Summarize the following conversation so the assistant can continue the task. \
             Capture decisions, current state, and open work. Be concise.",
            &transcript,
            "compaction",
        ) {
            Ok(assembled) => assembled,
            Err(e) => {
                tracing::warn!("compaction request assembly failed: {e}");
                return false;
            }
        };
        for entry in assembled.ledger.entries {
            self.emit(EventMsg::LedgerAppended(entry));
        }
        let summary = match self.collect_text(&assembled.request, cancellation).await {
            Ok(summary) if !summary.trim().is_empty() => summary,
            Ok(_) => {
                tracing::warn!("compaction returned an empty summary; preserving history");
                return false;
            }
            Err(e) => {
                tracing::warn!("compaction request failed: {e}; preserving history");
                return false;
            }
        };
        let redacted = Redactor::apply(&summary);
        let summary_item = compaction::build_summary_item(
            &redacted.text,
            &files,
            &errors,
            prior_redactions.saturating_add(redacted.count),
        );
        let Some(summary_item) =
            bound_compaction_summary(summary_item, MAX_COMPACTION_SUMMARY_ITEM_BYTES)
        else {
            tracing::warn!("compaction summary could not fit its durable safety budget");
            return false;
        };
        if !checkpoint_fits(
            &summary_item,
            &session.history[split..],
            MAX_COMPACTION_CHECKPOINT_BYTES,
        ) {
            tracing::warn!("compaction checkpoint could not fit its durable safety budget");
            return false;
        }

        let mut new_history = Vec::with_capacity(1 + session.history.len().saturating_sub(split));
        new_history.push(summary_item);
        new_history.extend_from_slice(&session.history[split..]);
        if let Some(writer) = rollout.as_mut()
            && let Err(e) = writer
                .append(&ResponseItem::CompactionCheckpoint {
                    history: new_history.clone(),
                })
                .await
        {
            self.emit(EventMsg::Error {
                message: format!("could not persist compaction checkpoint: {e}"),
                recoverable: true,
            });
            return false;
        }
        session.history = new_history;
        tracing::info!("compacted {} items into a summary", split);
        true
    }

    /// Stream a request and collect its assistant text (used for summaries/commit messages).
    async fn collect_text(
        &self,
        req: &grokforge_xai::ResponsesRequest,
        cancellation: &TurnCancellation,
    ) -> Result<String, String> {
        let mut text = String::new();
        let mut usage = Usage::default();
        let mut stream = tokio::select! {
            result = self.open_stream_accounted(req) => result.map_err(|e| e.to_string())?,
            () = cancellation.cancelled() => return Err("turn interrupted".to_string()),
        };
        let mut terminal = None;
        let mut event_count = 0usize;
        loop {
            let event = tokio::select! {
                event = stream.next() => event,
                () = cancellation.cancelled() => return Err("turn interrupted".to_string()),
            };
            let Some(event) = event else {
                break;
            };
            event_count = event_count.saturating_add(1);
            if event_count > MAX_RESPONSE_EVENTS {
                return Err(format!(
                    "summary response exceeded the {MAX_RESPONSE_EVENTS}-event safety limit"
                ));
            }
            match event {
                Ok(StreamEvent::TextDelta(delta)) => {
                    if text
                        .len()
                        .checked_add(delta.len())
                        .is_none_or(|len| len > MAX_RESPONSE_TEXT_BYTES)
                    {
                        return Err(format!(
                            "summary response exceeded the {MAX_RESPONSE_TEXT_BYTES}-byte safety limit"
                        ));
                    }
                    text.push_str(&delta);
                }
                Ok(StreamEvent::Completed { stop }) => terminal = Some(stop),
                Ok(StreamEvent::Usage(provider_usage)) => {
                    usage = Usage {
                        input_tokens: provider_usage.input_tokens,
                        cached_tokens: provider_usage.cached_tokens,
                        output_tokens: provider_usage.output_tokens,
                        reasoning_tokens: provider_usage.reasoning_tokens,
                    };
                }
                Ok(StreamEvent::ToolCall(_)) => {
                    return Err("summary model unexpectedly requested a tool".to_string());
                }
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        match terminal {
            Some(grokforge_xai::StopReason::EndTurn) => {
                self.emit(EventMsg::TokenUsage { usage });
                Ok(text)
            }
            Some(stop) => Err(format!("summary response ended with {stop:?}")),
            None => Err("summary stream ended before response.completed".to_string()),
        }
    }

    /// Commit files the agent wrote this turn, staging only those paths, from the host process.
    async fn auto_commit(
        &self,
        session: &Session,
        turn_id: TurnId,
        clean_at_start: bool,
        touched: Vec<std::path::PathBuf>,
    ) {
        // A dirty start means some changes already belong to the user; never sweep them into an
        // agent commit. Even after a clean start, stage only paths recorded by descriptor-safe
        // write/edit tools: shell or concurrent user/other-session changes have no reliable
        // ownership identity and must remain uncommitted.
        if !clean_at_start || touched.is_empty() {
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

    /// Admit and then concurrently run a batch of `spawn_task` calls that the model requested in
    /// one response. Admission (recording the raw call, cap/offer checks, and the approval gate)
    /// runs sequentially so approvals stay ordered and the parent session is mutated one call at a
    /// time. Worktree setup also runs sequentially so concurrent `git worktree add` invocations
    /// cannot race on the repository's ref locks. The subagent turns then run in parallel — each in
    /// its own worktree with a fresh sibling agent (depth cap 1) — and their outputs are recorded
    /// back into the parent transcript in the original call order. Commits land on `gf/agent/<id>`
    /// branches for the parent/user to review or merge; we do not auto-merge.
    #[allow(clippy::too_many_arguments)]
    async fn run_spawn_batch(
        &self,
        session: &mut Session,
        rollout: &mut Option<RolloutWriter>,
        ctx: &TurnContext,
        batch: &[(usize, ToolCallId, String, String)],
        raw_call_ids: &BTreeSet<String>,
        offered: bool,
        spawned: &mut usize,
        auto_commit_has_exclusive_ownership: &mut bool,
        cancellation: &TurnCancellation,
    ) -> ToolCallFlow {
        // Phase 1 — admission (sequential). Rejections record their failure here; approved calls
        // carry their parsed arguments forward. Setup is deferred to phase 2 so that a user abort
        // mid-approval cannot leave already-created worktrees behind.
        let mut approved: Vec<(ToolCallId, serde_json::Value)> = Vec::new();
        for (_, call_id, _name, arguments) in batch {
            *auto_commit_has_exclusive_ownership &=
                tool_preserves_auto_commit_ownership(crate::tools::builtins::SPAWN_TASK);
            let record_call = !raw_call_ids.contains(call_id.as_str());
            *spawned += 1;
            let spawn_allowed = *spawned <= MAX_SUBAGENTS_PER_TURN;
            match self
                .admit_spawn_task(
                    session,
                    rollout,
                    ctx,
                    call_id,
                    arguments,
                    record_call,
                    offered,
                    spawn_allowed,
                    cancellation,
                )
                .await
            {
                SpawnAdmit::Approved(args) => approved.push((call_id.clone(), args)),
                SpawnAdmit::Rejected => {}
                SpawnAdmit::Abort => return ToolCallFlow::Abort,
                SpawnAdmit::Error => return ToolCallFlow::Error,
            }
        }
        if approved.is_empty() {
            return ToolCallFlow::Continue;
        }

        // Phase 2 — worktree setup (sequential; serializes the git ref-lock mutations).
        let mut jobs: Vec<SubagentJob> = Vec::new();
        for (call_id, args) in &approved {
            match self.setup_subagent(session, args, call_id.clone()).await {
                Ok(job) => jobs.push(job),
                Err(output) => {
                    if self
                        .finish_tool_call(session, rollout, call_id.clone(), output)
                        .await
                        .is_err()
                    {
                        return ToolCallFlow::Error;
                    }
                }
            }
        }
        if jobs.is_empty() {
            return ToolCallFlow::Continue;
        }

        // Phase 3 — run every admitted subagent concurrently. Each internally spawns its own API
        // loop task, so these run in true parallel; `join_all` drives their per-lane event drains.
        let call_ids: Vec<ToolCallId> = jobs.iter().map(|job| job.call_id.clone()).collect();
        let total = jobs.len();
        let outputs = futures::future::join_all(
            jobs.into_iter()
                .enumerate()
                .map(|(index, job)| self.run_subagent_job(job, index, total, cancellation)),
        )
        .await;

        // Phase 4 — record the outputs back into the parent transcript in original call order.
        for (call_id, output) in call_ids.into_iter().zip(outputs) {
            if self
                .finish_tool_call(session, rollout, call_id, output)
                .await
                .is_err()
            {
                return ToolCallFlow::Error;
            }
        }
        ToolCallFlow::Continue
    }

    /// Record the raw `spawn_task` call, run the cap/offer/depth checks and the approval gate, and
    /// (on approval) announce the tool call. Failures are recorded immediately; the return value
    /// tells [`Self::run_spawn_batch`] whether to continue, abort, or fail the turn.
    #[allow(clippy::too_many_arguments)]
    async fn admit_spawn_task(
        &self,
        session: &mut Session,
        rollout: &mut Option<RolloutWriter>,
        ctx: &TurnContext,
        call_id: &ToolCallId,
        arguments: &str,
        record_call: bool,
        offered: bool,
        spawn_allowed: bool,
        cancellation: &TurnCancellation,
    ) -> SpawnAdmit {
        let spawn = crate::tools::builtins::SPAWN_TASK;
        if record_call
            && self
                .record(
                    session,
                    rollout,
                    ResponseItem::ToolCall {
                        id: call_id.clone(),
                        name: spawn.to_string(),
                        arguments: arguments.to_string(),
                    },
                )
                .await
                .is_err()
        {
            return SpawnAdmit::Error;
        }
        if cancellation.is_cancelled() {
            return match self
                .finish_tool_call(
                    session,
                    rollout,
                    call_id.clone(),
                    ToolOutput::failure("[turn interrupted by user before tool execution]"),
                )
                .await
            {
                Ok(()) => SpawnAdmit::Abort,
                Err(()) => SpawnAdmit::Error,
            };
        }
        if !offered {
            return self
                .reject_spawn(
                    session,
                    rollout,
                    call_id,
                    format!("tool `{spawn}` is not available in this turn's advertised tool set"),
                )
                .await;
        }
        if !self.allow_subagents || !spawn_allowed {
            let message = if self.allow_subagents {
                format!("at most {MAX_SUBAGENTS_PER_TURN} subagents may be spawned in one turn")
            } else {
                "subagents cannot spawn further subagents".to_string()
            };
            return self.reject_spawn(session, rollout, call_id, message).await;
        }
        let args: serde_json::Value =
            serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null);
        let Some(tool) = self.registry.get(spawn) else {
            return self
                .reject_spawn(session, rollout, call_id, format!("unknown tool `{spawn}`"))
                .await;
        };
        let need = tool.approval(&args, ctx);
        if let Gate::Ask(kind) = gate(
            session.config.approval_policy,
            session.config.sandbox_mode,
            &need,
        ) {
            let decision = self
                .request_approval(call_id, kind, spawn, format!("run `{spawn}`"), cancellation)
                .await;
            if decision == Decision::Abort {
                return match self
                    .finish_tool_call(
                        session,
                        rollout,
                        call_id.clone(),
                        ToolOutput::failure("[turn aborted by user]"),
                    )
                    .await
                {
                    Ok(()) => SpawnAdmit::Abort,
                    Err(()) => SpawnAdmit::Error,
                };
            }
            if !decision.is_approved() {
                let feedback = match decision {
                    Decision::DenyWithFeedback(f) => f,
                    _ => "denied".to_string(),
                };
                return self
                    .reject_spawn(session, rollout, call_id, format!("[not run: {feedback}]"))
                    .await;
            }
        }
        self.emit(EventMsg::ToolCallBegin {
            call_id: call_id.clone(),
            name: spawn.to_string(),
            args_preview: preview(arguments),
            sandboxed: false,
        });
        SpawnAdmit::Approved(args)
    }

    async fn reject_spawn(
        &self,
        session: &mut Session,
        rollout: &mut Option<RolloutWriter>,
        call_id: &ToolCallId,
        message: String,
    ) -> SpawnAdmit {
        match self
            .finish_tool_call(
                session,
                rollout,
                call_id.clone(),
                ToolOutput::failure(message),
            )
            .await
        {
            Ok(()) => SpawnAdmit::Rejected,
            Err(()) => SpawnAdmit::Error,
        }
    }

    /// Create the isolated worktree, session, rollout, and metadata for one approved subagent and
    /// return a [`SubagentJob`] ready to run. On any failure the partially created worktree is
    /// removed and a failure output is returned for the caller to record.
    #[allow(clippy::too_many_lines)]
    async fn setup_subagent(
        &self,
        session: &Session,
        args: &serde_json::Value,
        call_id: ToolCallId,
    ) -> Result<SubagentJob, ToolOutput> {
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if prompt.is_empty() {
            return Err(ToolOutput::failure(
                "spawn_task requires a non-empty `prompt`",
            ));
        }
        let workspace = session.config.workspace_root.clone();
        let Some(git) = grokforge_git::Git::discover(&workspace) else {
            return Err(ToolOutput::failure("subagents require a git repository"));
        };
        let base = git.head_sha().unwrap_or_else(|_| "HEAD".to_string());
        let id = ToolCallId::new().to_string();
        let worktrees = match crate::store::prepare_worktrees_dir().await {
            Ok(worktrees) => worktrees,
            Err(error) => {
                tracing::warn!(%error, "secure subagent worktree storage is unavailable");
                return Err(ToolOutput::failure(
                    "secure private subagent worktree storage is unavailable",
                ));
            }
        };
        let worktree = worktrees.join(&id);
        let branch = format!("gf/agent/{id}");

        let (git_c, wt_c, br_c, base_c) =
            (git.clone(), worktree.clone(), branch.clone(), base.clone());
        match tokio::task::spawn_blocking(move || git_c.worktree_add(&wt_c, &br_c, &base_c)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %e, branch, "could not create private subagent worktree");
                return Err(ToolOutput::failure(
                    "could not create private subagent worktree",
                ));
            }
            Err(e) => {
                tracing::warn!(error = %e, branch, "subagent worktree task failed");
                return Err(ToolOutput::failure("subagent worktree task failed"));
            }
        }

        // Each subagent turn runs with its own event channel so its stream can be tagged per lane.
        let (sub_tx, sub_rx) = tokio::sync::mpsc::unbounded_channel();
        let sub_agent = self.for_subagent(sub_tx);
        let mut sub_config =
            crate::session::SessionConfig::new(worktree.clone(), session.config.model.clone())
                .with_policy(
                    session.config.approval_policy,
                    // Even a yolo parent does not broaden a child into sibling worktrees.
                    SandboxMode::WorkspaceWrite,
                );
        sub_config.effort = session.config.effort;
        sub_config
            .enabled_server_tools
            .clone_from(&session.config.enabled_server_tools);
        sub_config
            .system_prompt
            .clone_from(&session.config.system_prompt);
        sub_config.max_iterations = session.config.max_iterations;
        sub_config.compaction_trigger_bytes = session.config.compaction_trigger_bytes;
        sub_config.compaction_keep_tail = session.config.compaction_keep_tail;
        sub_config.auto_compact = session.config.auto_compact;
        sub_config.context_window_tokens = session.config.context_window_tokens;
        sub_config.network = session.config.network;
        // This dedicated worktree gives the subagent exclusive path ownership, which is the
        // precondition for race-safe auto-commit. Keep its default auto-commit enabled even
        // when the parent disabled auto-commit.
        sub_config.isolated_worktree = true;
        let sub_session = crate::session::Session::new(sub_config);
        let sessions = match crate::store::sessions_dir() {
            Ok(sessions) => sessions,
            Err(error) => {
                tracing::warn!(%error, branch, "secure subagent session storage is unavailable");
                remove_worktree(git, worktree).await;
                return Err(ToolOutput::failure(
                    "secure subagent session storage is unavailable; no model call was made",
                ));
            }
        };
        let sub_writer = match RolloutWriter::create(&sessions, sub_session.id).await {
            Ok(writer) => writer,
            Err(error) => {
                tracing::warn!(%error, branch, "subagent persistence is unavailable");
                remove_worktree(git, worktree).await;
                return Err(ToolOutput::failure(
                    "subagent persistence unavailable; no model call was made",
                ));
            }
        };
        let meta = crate::store::SessionMeta::new(
            sub_session.id,
            worktree.clone(),
            session.config.model.clone(),
            &prompt,
        );
        if let Err(error) = meta.write(&sessions, sub_session.id).await {
            tracing::warn!(%error, branch, "subagent metadata could not be persisted");
            drop(sub_writer);
            remove_worktree(git, worktree).await;
            return Err(ToolOutput::failure(
                "subagent metadata could not be persisted; no model call was made",
            ));
        }
        Ok(SubagentJob {
            call_id,
            label: subagent_label(&prompt),
            prompt,
            git,
            worktree,
            branch,
            base,
            sub_agent,
            sub_session,
            sub_rollout: Some(sub_writer),
            sub_rx,
        })
    }

    /// Run one prepared subagent to completion, streaming its events tagged with its lane id and
    /// returning the tool output to record in the parent transcript.
    ///
    /// Declared with a boxed `+ Send` return type (not `async fn`) to break the async-recursion
    /// auto-trait cycle `run_turn -> run_spawn_batch -> run_subagent_job -> run_turn`. Without the
    /// type-erased boundary the compiler cannot prove this future is `Send`.
    fn run_subagent_job<'a>(
        &'a self,
        job: SubagentJob,
        index: usize,
        total: usize,
        cancellation: &'a TurnCancellation,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let SubagentJob {
                call_id,
                label,
                prompt,
                git,
                worktree,
                branch,
                base,
                sub_agent,
                mut sub_session,
                mut sub_rollout,
                mut sub_rx,
            } = job;
            let agent_id = call_id.to_string();
            self.emit(EventMsg::SubagentStarted {
                agent_id: agent_id.clone(),
                label,
                index,
                total,
            });

            let sub_cancellation = cancellation.clone();
            let sub_fut: std::pin::Pin<Box<dyn std::future::Future<Output = StopReason> + Send>> =
                Box::pin(async move {
                    sub_agent
                        .run_turn_cancellable(
                            &mut sub_session,
                            &prompt,
                            &mut sub_rollout,
                            &sub_cancellation,
                        )
                        .await
                });
            let handle = AbortOnDrop::new(tokio::spawn(sub_fut));
            let mut final_text = String::new();
            while let Some(ev) = sub_rx.recv().await {
                if let EventMsg::AgentMessageDone { text } = &ev {
                    final_text.clone_from(text);
                }
                // Tag every subagent event with its lane so the frontend renders per-agent
                // progress rather than interleaving 32 streams into one transcript.
                // Accounting-relevant inner events (token usage, ledger) are still folded into
                // global totals by frontends.
                self.emit(EventMsg::SubagentUpdate {
                    agent_id: agent_id.clone(),
                    inner: Box::new(ev),
                });
            }
            let output = self
                .finalize_subagent(git, worktree, &branch, &base, handle, &final_text)
                .await;
            self.emit(EventMsg::SubagentFinished {
                agent_id,
                ok: !output.is_error(),
                summary: summarize_subagent(&branch, &output),
            });
            output
        })
    }

    /// Join the subagent task, inspect its worktree, and build the tool output. Never force-removes
    /// uncommitted edits: a failed auto-commit (for example a missing git identity) leaves
    /// recoverable work in place on the agent branch.
    async fn finalize_subagent(
        &self,
        git: grokforge_git::Git,
        worktree: std::path::PathBuf,
        branch: &str,
        base: &str,
        handle: AbortOnDrop<StopReason>,
        final_text: &str,
    ) -> ToolOutput {
        let sub_stop = match handle.join().await {
            Ok(stop) => stop,
            Err(e) => {
                tracing::warn!(error = %e, branch, "subagent task failed");
                return ToolOutput::failure(format!(
                    "subagent task failed; private recovery worktree for branch `{branch}` was preserved"
                ));
            }
        };

        let base_d = base.to_string();
        let inspection = tokio::task::spawn_blocking(move || {
            let worktree_git = grokforge_git::Git::discover(&worktree)
                .ok_or_else(|| "could not inspect subagent worktree".to_string())?;
            let dirty = worktree_git.is_dirty().map_err(|e| e.to_string())?;
            let diff = worktree_git
                .diff_stat(&format!("{base_d}..HEAD"))
                .map_err(|e| e.to_string())?;
            if !dirty {
                git.worktree_remove(&worktree).map_err(|e| e.to_string())?;
            }
            Ok::<_, String>((diff, dirty))
        })
        .await;
        let (diff, dirty) = match inspection {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, branch, "subagent inspection failed");
                return ToolOutput::failure(format!(
                    "subagent inspection failed; private recovery worktree for branch `{branch}` was preserved"
                ));
            }
            Err(e) => {
                tracing::warn!(error = %e, branch, "subagent inspection task failed");
                return ToolOutput::failure(format!(
                    "subagent inspection task failed; private recovery worktree for branch `{branch}` was preserved"
                ));
            }
        };

        if dirty {
            return ToolOutput::failure(format!(
                "subagent left uncommitted changes; its private recovery worktree was preserved on branch `{branch}` (locate it with `git worktree list`)"
            ));
        }
        if sub_stop != StopReason::EndTurn {
            return ToolOutput::failure(format!(
                "subagent stopped with {sub_stop:?}; committed changes remain on branch `{branch}`"
            ));
        }

        let changes = if diff.trim().is_empty() {
            "(no changes)".to_string()
        } else {
            diff.trim().to_string()
        };
        ToolOutput::success(format!(
            "Subagent finished on branch `{branch}` (review or merge it manually).\n\nResult:\n{final_text}\n\nChanges:\n{changes}"
        ))
    }

    async fn request_approval(
        &self,
        call_id: &ToolCallId,
        kind: ApprovalKind,
        name: &str,
        reason: String,
        cancellation: &TurnCancellation,
    ) -> Decision {
        let req = ApprovalRequest {
            id: ApprovalId::new(),
            call_id: Some(call_id.clone()),
            kind,
            reason,
        };
        self.emit(EventMsg::ApprovalRequested(req.clone()));
        let decision = tokio::select! {
            decision = self.approver.request(req) => decision,
            () = cancellation.cancelled() => Decision::Abort,
        };
        self.emit(EventMsg::ApprovalResolved {
            summary: format!("`{name}`"),
            decision: format!("{decision:?}"),
            auto: self.auto_approval,
        });
        decision
    }

    async fn finish_tool_call(
        &self,
        session: &mut Session,
        rollout: &mut Option<RolloutWriter>,
        call_id: ToolCallId,
        output: ToolOutput,
    ) -> Result<(), ()> {
        let red = Redactor::apply(output.content());
        let is_error = output.is_error();
        let denial = match &output {
            ToolOutput::Failure { denial, .. } => *denial,
            ToolOutput::Success { .. } => None,
        };
        self.emit(EventMsg::ToolCallEnd {
            call_id: call_id.clone(),
            ok: !is_error,
            summary: summarize(&red.text),
            denial,
        });
        self.record_tool_result(session, rollout, call_id, &red.text, is_error, red.count)
            .await
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn run_tool_call(
        &self,
        session: &mut Session,
        rollout: &mut Option<RolloutWriter>,
        ctx: &TurnContext,
        call_id: ToolCallId,
        name: &str,
        arguments: &str,
        record_call: bool,
        offered: bool,
        spawn_allowed: bool,
        cancellation: &TurnCancellation,
    ) -> ToolCallFlow {
        if record_call
            && self
                .record(
                    session,
                    rollout,
                    ResponseItem::ToolCall {
                        id: call_id.clone(),
                        name: name.to_string(),
                        arguments: arguments.to_string(),
                    },
                )
                .await
                .is_err()
        {
            return ToolCallFlow::Error;
        }

        if cancellation.is_cancelled() {
            return if self
                .finish_tool_call(
                    session,
                    rollout,
                    call_id,
                    ToolOutput::failure("[turn interrupted by user before tool execution]"),
                )
                .await
                .is_err()
            {
                ToolCallFlow::Error
            } else {
                ToolCallFlow::Abort
            };
        }

        if !offered {
            if self
                .finish_tool_call(
                    session,
                    rollout,
                    call_id,
                    ToolOutput::failure(format!(
                        "tool `{name}` is not available in this turn's advertised tool set"
                    )),
                )
                .await
                .is_err()
            {
                return ToolCallFlow::Error;
            }
            return ToolCallFlow::Continue;
        }

        if name == crate::tools::builtins::SPAWN_TASK && (!self.allow_subagents || !spawn_allowed) {
            let message = if self.allow_subagents {
                format!("at most {MAX_SUBAGENTS_PER_TURN} subagents may be spawned in one turn")
            } else {
                "subagents cannot spawn further subagents".to_string()
            };
            if self
                .finish_tool_call(session, rollout, call_id, ToolOutput::failure(message))
                .await
                .is_err()
            {
                return ToolCallFlow::Error;
            }
            return ToolCallFlow::Continue;
        }

        let Some(tool) = self.registry.get(name) else {
            let msg = format!("unknown tool `{name}`");
            if self
                .finish_tool_call(session, rollout, call_id, ToolOutput::failure(msg))
                .await
                .is_err()
            {
                return ToolCallFlow::Error;
            }
            return ToolCallFlow::Continue;
        };

        let args: serde_json::Value =
            serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null);
        let need = tool.approval(&args, ctx);

        let mut elevated = false;
        let mut approved_write_targets = Vec::new();
        if let Gate::Ask(kind) = gate(
            session.config.approval_policy,
            session.config.sandbox_mode,
            &need,
        ) {
            let exceeds = approval_exceeds_sandbox(&need, &kind, ctx);
            if session.config.isolated_worktree
                && exceeds
                && !matches!(kind, ApprovalKind::Network { .. })
            {
                if self
                    .finish_tool_call(
                        session,
                        rollout,
                        call_id,
                        ToolOutput::failure(
                            "isolated subagents cannot widen filesystem access beyond their private worktree",
                        ),
                    )
                    .await
                    .is_err()
                {
                    return ToolCallFlow::Error;
                }
                return ToolCallFlow::Continue;
            }
            let approval_kind = if exceeds {
                match physical_write_identity(&kind) {
                    Ok(Some((identity, targets))) => {
                        approved_write_targets = targets;
                        identity
                    }
                    Ok(None) => kind.clone(),
                    Err(message) => {
                        if self
                            .finish_tool_call(
                                session,
                                rollout,
                                call_id,
                                ToolOutput::failure(message),
                            )
                            .await
                            .is_err()
                        {
                            return ToolCallFlow::Error;
                        }
                        return ToolCallFlow::Continue;
                    }
                }
            } else {
                kind.clone()
            };
            let decision = self
                .request_approval(
                    &call_id,
                    approval_kind,
                    name,
                    format!("run `{name}`"),
                    cancellation,
                )
                .await;
            if decision == Decision::Abort {
                if self
                    .finish_tool_call(
                        session,
                        rollout,
                        call_id,
                        ToolOutput::failure("[turn aborted by user]"),
                    )
                    .await
                    .is_err()
                {
                    return ToolCallFlow::Error;
                }
                return ToolCallFlow::Abort;
            }
            if !decision.is_approved() {
                let feedback = match decision {
                    Decision::DenyWithFeedback(f) => f,
                    _ => "denied".to_string(),
                };
                let content = format!("[not run: {feedback}]");
                if self
                    .finish_tool_call(session, rollout, call_id, ToolOutput::failure(content))
                    .await
                    .is_err()
                {
                    return ToolCallFlow::Error;
                }
                return ToolCallFlow::Continue;
            }
            elevated = exceeds;
        }

        // `spawn_task` is dispatched separately (see `run_spawn_batch`) so a whole batch of
        // subagents runs concurrently; it does not reach this per-tool invocation path.

        if elevated
            && !approved_write_targets.is_empty()
            && !write_targets_still_resolve(&approved_write_targets)
        {
            if self
                .finish_tool_call(
                    session,
                    rollout,
                    call_id,
                    ToolOutput::failure(
                        "approved write target changed before invocation; request approval again",
                    ),
                )
                .await
                .is_err()
            {
                return ToolCallFlow::Error;
            }
            return ToolCallFlow::Continue;
        }
        let elevated_ctx;
        let invoke_ctx = if elevated {
            elevated_ctx = if approved_write_targets.is_empty() {
                Self::elevated_context(ctx)
            } else {
                Self::elevated_write_context(ctx, &approved_write_targets)
            };
            &elevated_ctx
        } else {
            ctx
        };
        let invoke_args = bind_approved_write_target(args.clone(), &approved_write_targets);
        self.emit(EventMsg::ToolCallBegin {
            call_id: call_id.clone(),
            name: name.to_string(),
            args_preview: preview(arguments),
            sandboxed: invoke_ctx.policy.mode.is_sandboxed(),
        });

        let mut output = tool
            .invoke(ToolInvocation {
                call_id: call_id.clone(),
                args: invoke_args,
                ctx: invoke_ctx,
            })
            .await;

        let denial = match &output {
            ToolOutput::Failure { denial, .. } => *denial,
            ToolOutput::Success { .. } => None,
        };
        let escalation = denial
            .filter(|denial| {
                !elevated
                    && session.config.approval_policy != ApprovalPolicy::Never
                    && !(session.config.isolated_worktree
                        && *denial == grokforge_protocol::DenialClass::FsWrite)
            })
            .and_then(|denial| escalation_kind(&need, denial).map(|kind| (denial, kind)));
        if let Some((denial, kind)) = escalation {
            let retry_targets = escalation_write_targets(&kind);
            let decision = self
                .request_approval(
                    &call_id,
                    kind,
                    name,
                    format!("retry `{name}` after sandbox denial {denial:?}"),
                    cancellation,
                )
                .await;
            if decision == Decision::Abort {
                if self
                    .finish_tool_call(
                        session,
                        rollout,
                        call_id,
                        ToolOutput::failure("[turn aborted by user]"),
                    )
                    .await
                    .is_err()
                {
                    return ToolCallFlow::Error;
                }
                return ToolCallFlow::Abort;
            }
            if decision.is_approved() {
                if !retry_targets.is_empty() && !write_targets_still_resolve(&retry_targets) {
                    output = ToolOutput::failure(
                        "approved write target changed before retry; request approval again",
                    );
                } else {
                    let retry_ctx = if retry_targets.is_empty() {
                        Self::escalation_context(ctx, denial)
                    } else {
                        Self::elevated_write_context(ctx, &retry_targets)
                    };
                    let retry_args = bind_approved_write_target(args, &retry_targets);
                    output = tool
                        .invoke(ToolInvocation {
                            call_id: call_id.clone(),
                            args: retry_args,
                            ctx: &retry_ctx,
                        })
                        .await;
                }
            }
        }

        let cancelled = cancellation.is_cancelled();
        if self
            .finish_tool_call(session, rollout, call_id, output)
            .await
            .is_err()
        {
            ToolCallFlow::Error
        } else if cancelled {
            ToolCallFlow::Abort
        } else {
            ToolCallFlow::Continue
        }
    }

    async fn record(
        &self,
        session: &mut Session,
        rollout: &mut Option<RolloutWriter>,
        item: ResponseItem,
    ) -> Result<(), ()> {
        if let Some(w) = rollout.as_mut()
            && let Err(e) = w.append(&item).await
        {
            let message =
                format!("rollout append failed; turn stopped before using unpersisted state: {e}");
            tracing::warn!("{message}");
            self.emit(EventMsg::Error {
                message,
                recoverable: false,
            });
            return Err(());
        }
        session.history.push(item);
        Ok(())
    }

    async fn record_tool_result(
        &self,
        session: &mut Session,
        rollout: &mut Option<RolloutWriter>,
        id: ToolCallId,
        content: &str,
        is_error: bool,
        redactions: usize,
    ) -> Result<(), ()> {
        self.record(
            session,
            rollout,
            ResponseItem::ToolResult {
                id,
                content: content.to_string(),
                is_error,
                redactions,
            },
        )
        .await
    }
}

#[allow(clippy::too_many_lines)]
fn validate_provider_response(
    response: &ConsumedResponse,
    history: &[ResponseItem],
) -> Result<(), String> {
    let mut prior_ids = BTreeSet::new();
    for item in history {
        match item {
            ResponseItem::ToolCall { id, .. } | ResponseItem::ToolResult { id, .. } => {
                prior_ids.insert(id.as_str());
            }
            ResponseItem::ProviderOutput { item }
                if item.get("type").and_then(serde_json::Value::as_str)
                    == Some("function_call") =>
            {
                if let Some(id) = item.get("call_id").and_then(serde_json::Value::as_str) {
                    prior_ids.insert(id);
                }
            }
            _ => {}
        }
    }
    let mut typed_ids = BTreeSet::new();
    for (output_index, id, name, arguments) in &response.tool_calls {
        if id.as_str().is_empty() || !typed_ids.insert(id.as_str()) {
            return Err(format!(
                "provider returned duplicate or empty tool call id `{}`",
                id.as_str()
            ));
        }
        if prior_ids.contains(id.as_str()) {
            return Err(format!(
                "provider reused tool call id `{}` from prior session history",
                id.as_str()
            ));
        }
        let Some((_, raw)) = response
            .provider_outputs
            .iter()
            .find(|(index, _)| index == output_index)
        else {
            return Err(format!(
                "typed tool call `{}` has no provider output at index {output_index}",
                id.as_str()
            ));
        };
        if raw.get("type").and_then(serde_json::Value::as_str) != Some("function_call")
            || raw.get("call_id").and_then(serde_json::Value::as_str) != Some(id.as_str())
            || raw.get("name").and_then(serde_json::Value::as_str) != Some(name.as_str())
            || raw.get("arguments").and_then(serde_json::Value::as_str) != Some(arguments.as_str())
        {
            return Err(format!(
                "typed tool call `{}` disagrees with provider output index {output_index}",
                id.as_str()
            ));
        }
    }

    let mut raw_ids = BTreeSet::new();
    for (output_index, item) in response.provider_outputs.iter().filter(|(_, item)| {
        item.get("type").and_then(serde_json::Value::as_str) == Some("function_call")
    }) {
        let id = item
            .get("call_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if id.is_empty() || !raw_ids.insert(id) {
            return Err(format!(
                "provider returned duplicate or empty raw tool call id `{id}`"
            ));
        }
        if !response
            .tool_calls
            .iter()
            .any(|(index, typed_id, _, _)| index == output_index && typed_id.as_str() == id)
        {
            return Err(format!(
                "provider function call `{id}` at index {output_index} has no matching typed call"
            ));
        }
    }

    for (output_index, reasoning) in &response.encrypted_reasoning {
        let ResponseItem::EncryptedReasoning { id, .. } = reasoning else {
            return Err("internal reasoning item lost its typed representation".to_string());
        };
        let Some((_, raw)) = response
            .provider_outputs
            .iter()
            .find(|(index, _)| index == output_index)
        else {
            return Err(format!(
                "typed reasoning `{id}` has no provider output at index {output_index}"
            ));
        };
        if raw.get("type").and_then(serde_json::Value::as_str) != Some("reasoning")
            || raw.get("id").and_then(serde_json::Value::as_str) != Some(id.as_str())
        {
            return Err(format!(
                "typed reasoning `{id}` disagrees with provider output index {output_index}"
            ));
        }
    }

    let messages: Vec<_> = response
        .provider_outputs
        .iter()
        .filter_map(|(_, item)| {
            (item.get("type").and_then(serde_json::Value::as_str) == Some("message"))
                .then_some(item)
        })
        .collect();
    if !messages.is_empty() {
        let mut finalized = String::new();
        for message in messages {
            let content = message
                .get("content")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    "provider finalized message omitted its content array".to_string()
                })?;
            for part in content {
                if part.get("type").and_then(serde_json::Value::as_str) == Some("output_text") {
                    let text = part
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| "provider finalized output_text omitted text".to_string())?;
                    finalized.push_str(text);
                }
            }
        }
        if finalized != response.assistant_text {
            return Err(
                "streamed assistant text did not match the provider's finalized message"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn sandbox_policy(
    root: &std::path::Path,
    mode: SandboxMode,
    network: grokforge_protocol::NetworkMode,
    isolated_worktree: bool,
) -> Result<SandboxPolicy, String> {
    let mut policy = match mode {
        SandboxMode::ReadOnly => SandboxPolicy::read_only(root),
        SandboxMode::WorkspaceWrite => SandboxPolicy::workspace_write(root),
        SandboxMode::DangerFullAccess => SandboxPolicy::danger_full_access(root),
    };
    policy.network = network;
    if let Some(git) = grokforge_git::Git::discover(root) {
        protect_git_metadata(&mut policy, &git)?;
    }
    let session_dir = crate::store::prepare_sessions_dir_blocking()
        .map_err(|error| format!("secure session storage is unavailable: {error}"))?;
    protect_private_store_path(&mut policy, &session_dir);
    let worktrees = crate::store::prepare_worktrees_dir_blocking()
        .map_err(|error| format!("secure subagent worktree storage is unavailable: {error}"))?;
    let canonical_root = std::fs::canonicalize(root).map_err(|error| {
        format!(
            "could not resolve workspace root `{}`: {error}",
            root.display()
        )
    })?;
    let canonical_worktrees = std::fs::canonicalize(&worktrees).map_err(|error| {
        format!(
            "could not resolve private worktree root `{}`: {error}",
            worktrees.display()
        )
    })?;
    if isolated_worktree {
        if canonical_root.parent() != Some(canonical_worktrees.as_path()) {
            return Err(
                "isolated-worktree session is not rooted in private worktree storage".to_string(),
            );
        }
        // The child must access its own checkout. Protect every existing sibling individually;
        // its forced WorkspaceWrite policy also confines writes to the exact owned root.
        let entries = std::fs::read_dir(&canonical_worktrees).map_err(|error| {
            format!("could not inspect private subagent worktree storage: {error}")
        })?;
        for (index, entry) in entries.enumerate() {
            if index >= MAX_SUBAGENTS_PER_TURN.saturating_mul(128) {
                return Err(
                    "private subagent worktree storage contains too many entries".to_string(),
                );
            }
            let entry = entry.map_err(|error| {
                format!("could not inspect private subagent worktree entry: {error}")
            })?;
            let metadata = std::fs::symlink_metadata(entry.path()).map_err(|error| {
                format!("could not inspect private subagent worktree entry: {error}")
            })?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(
                    "private subagent worktree storage contains an unsafe entry".to_string()
                );
            }
            let sibling = std::fs::canonicalize(entry.path()).map_err(|error| {
                format!("could not resolve private subagent worktree entry: {error}")
            })?;
            if sibling != canonical_root {
                protect_private_store_path(&mut policy, &sibling);
            }
        }
    } else {
        protect_private_store_path(&mut policy, &canonical_worktrees);
    }
    Ok(policy)
}

fn protect_private_store_path(policy: &mut SandboxPolicy, path: &std::path::Path) {
    if !policy.protected_paths.iter().any(|known| known == path) {
        policy.protected_paths.push(path.to_path_buf());
    }
    let literal = globset::escape(&path.to_string_lossy());
    for pattern in [literal.clone(), format!("{literal}/**")] {
        if !policy.unreadable_globs.contains(&pattern) {
            policy.unreadable_globs.push(pattern);
        }
    }
}

fn protect_git_metadata(
    policy: &mut SandboxPolicy,
    git: &grokforge_git::Git,
) -> Result<(), String> {
    let paths = git.metadata_paths().map_err(|error| {
        format!(
            "could not pin all Git metadata paths for `{}`: {error}",
            git.root().display()
        )
    })?;
    for path in paths {
        if !policy.protected_paths.contains(&path) {
            policy.protected_paths.push(path);
        }
    }
    Ok(())
}

fn coherent_compaction_split(history: &[ResponseItem], keep: usize) -> usize {
    let mut split = history.len().saturating_sub(keep);
    loop {
        let mut required = split;
        for result_id in history[split..].iter().filter_map(|item| match item {
            ResponseItem::ToolResult { id, .. } => Some(id.as_str()),
            _ => None,
        }) {
            if let Some(call_index) =
                history[..required]
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(index, item)| match item {
                        ResponseItem::ToolCall { id, .. } if id.as_str() == result_id => {
                            Some(index)
                        }
                        ResponseItem::ProviderOutput { item }
                            if item.get("type").and_then(serde_json::Value::as_str)
                                == Some("function_call")
                                && item.get("call_id").and_then(serde_json::Value::as_str)
                                    == Some(result_id) =>
                        {
                            Some(index)
                        }
                        _ => None,
                    })
            {
                required = required.min(call_index);
            }
        }
        while required > 0
            && matches!(
                history.get(required - 1),
                Some(ResponseItem::ProviderOutput { .. } | ResponseItem::EncryptedReasoning { .. })
            )
        {
            required -= 1;
        }
        if required == split {
            return split;
        }
        split = required;
    }
}

fn bound_compaction_summary(
    mut summary: ResponseItem,
    max_serialized_bytes: usize,
) -> Option<ResponseItem> {
    const TRUNCATED: &str = "\n[compaction summary truncated to durable storage limit]";
    loop {
        if serde_json::to_vec(&summary).ok()?.len() <= max_serialized_bytes {
            return Some(summary);
        }
        let ResponseItem::CompactionSummary { text, .. } = &mut summary else {
            return None;
        };
        if text.is_empty() {
            return None;
        }
        let mut end = text.len() / 2;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        if text.len().saturating_add(TRUNCATED.len()) <= max_serialized_bytes {
            text.push_str(TRUNCATED);
        }
    }
}

fn choose_compaction_tail_split(
    history: &[ResponseItem],
    initial_split: usize,
    keep: usize,
    max_tail_bytes: usize,
) -> Option<usize> {
    if tail_fits(&history[initial_split..], max_tail_bytes) {
        return Some(initial_split);
    }

    // A coherence expansion can reach arbitrarily far back in a malicious resumed transcript.
    // If that expanded tail is too large, consider only a bounded recent suffix and drop any
    // leading results whose calls no longer fit. Empty tail is always the final candidate.
    let candidates = keep.min(MAX_COMPACTION_TAIL_CANDIDATES);
    let base = history.len().saturating_sub(candidates);
    (base..=history.len()).find(|candidate| {
        let tail = &history[*candidate..];
        tail_is_coherent(tail) && tail_fits(tail, max_tail_bytes)
    })
}

fn tail_is_coherent(tail: &[ResponseItem]) -> bool {
    let mut calls = BTreeSet::new();
    for item in tail {
        match item {
            ResponseItem::ToolCall { id, .. } => {
                calls.insert(id.as_str());
            }
            ResponseItem::ProviderOutput { item }
                if item.get("type").and_then(serde_json::Value::as_str)
                    == Some("function_call") =>
            {
                if let Some(id) = item.get("call_id").and_then(serde_json::Value::as_str) {
                    calls.insert(id);
                }
            }
            ResponseItem::ToolResult { id, .. } if !calls.contains(id.as_str()) => return false,
            _ => {}
        }
    }
    true
}

fn checkpoint_fits(
    summary: &ResponseItem,
    tail: &[ResponseItem],
    max_serialized_bytes: usize,
) -> bool {
    use std::io::Write as _;

    struct CappedCounter {
        bytes: usize,
        max: usize,
    }

    impl std::io::Write for CappedCounter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            let next = self.bytes.saturating_add(buffer.len());
            if next > self.max {
                return Err(std::io::Error::other("checkpoint exceeds byte budget"));
            }
            self.bytes = next;
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut output = CappedCounter {
        bytes: 0,
        max: max_serialized_bytes,
    };
    let result = (|| {
        output.write_all(b"{\"kind\":\"compaction_checkpoint\",\"history\":[")?;
        serde_json::to_writer(&mut output, summary).map_err(std::io::Error::other)?;
        for item in tail {
            output.write_all(b",")?;
            serde_json::to_writer(&mut output, item).map_err(std::io::Error::other)?;
        }
        output.write_all(b"]}")
    })();
    result.is_ok()
}

fn tail_fits(tail: &[ResponseItem], max_serialized_bytes: usize) -> bool {
    use std::io::Write as _;

    struct CappedCounter {
        bytes: usize,
        max: usize,
    }

    impl std::io::Write for CappedCounter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            let next = self.bytes.saturating_add(buffer.len());
            if next > self.max {
                return Err(std::io::Error::other("tail exceeds byte budget"));
            }
            self.bytes = next;
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut output = CappedCounter {
        bytes: 0,
        max: max_serialized_bytes,
    };
    let result = (|| {
        output.write_all(b"[")?;
        for (index, item) in tail.iter().enumerate() {
            if index > 0 {
                output.write_all(b",")?;
            }
            serde_json::to_writer(&mut output, item).map_err(std::io::Error::other)?;
        }
        output.write_all(b"]")
    })();
    result.is_ok()
}

fn approval_exceeds_sandbox(
    need: &crate::approvals::ApprovalNeed,
    kind: &ApprovalKind,
    ctx: &TurnContext,
) -> bool {
    if matches!(need, crate::approvals::ApprovalNeed::OutsideSandbox(_)) {
        return true;
    }
    match kind {
        ApprovalKind::WriteFile { path } => !ctx.policy.allows_write(path),
        ApprovalKind::ApplyPatch { files } => {
            files.iter().any(|path| !ctx.policy.allows_write(path))
        }
        ApprovalKind::Network { .. } => ctx.policy.network != grokforge_protocol::NetworkMode::Full,
        // Approving command source never pre-grants filesystem access. It runs under the
        // original policy; only a later classified FsWrite denial can request a distinct
        // SandboxEscalation (which command-prefix grants do not approve).
        ApprovalKind::ExecCommand { .. }
        | ApprovalKind::GitMutation { .. }
        | ApprovalKind::McpToolCall { .. }
        | ApprovalKind::SandboxEscalation { .. } => false,
    }
}

fn physical_write_identity(
    kind: &ApprovalKind,
) -> Result<Option<(ApprovalKind, Vec<std::path::PathBuf>)>, String> {
    fn canonical_target(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
        let canonical = crate::path_safety::canonicalize_allow_missing(path)
            .map_err(|error| format!("cannot resolve write approval target: {error}"))?;
        let lexical = crate::path_safety::normalize(path);
        if !path.is_absolute() || lexical != canonical {
            return Err(format!(
                "refusing an elevated write through a symlink or non-canonical path `{}`; name the canonical absolute target `{}`",
                path.display(),
                canonical.display()
            ));
        }
        Ok(canonical)
    }

    match kind {
        ApprovalKind::WriteFile { path } => {
            let target = canonical_target(path)?;
            Ok(Some((
                ApprovalKind::WriteFile {
                    path: target.clone(),
                },
                vec![target],
            )))
        }
        ApprovalKind::ApplyPatch { files } => {
            let targets = files
                .iter()
                .map(|path| canonical_target(path))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some((
                ApprovalKind::ApplyPatch {
                    files: targets.clone(),
                },
                targets,
            )))
        }
        _ => Ok(None),
    }
}

fn escalation_write_targets(kind: &ApprovalKind) -> Vec<std::path::PathBuf> {
    let ApprovalKind::SandboxEscalation { original, .. } = kind else {
        return Vec::new();
    };
    match original.as_ref() {
        ApprovalKind::WriteFile { path } => vec![path.clone()],
        ApprovalKind::ApplyPatch { files } => files.clone(),
        _ => Vec::new(),
    }
}

fn write_targets_still_resolve(targets: &[std::path::PathBuf]) -> bool {
    targets.iter().all(|target| {
        crate::path_safety::canonicalize_allow_missing(target)
            .is_ok_and(|canonical| canonical == *target)
    })
}

fn bind_approved_write_target(
    mut args: serde_json::Value,
    targets: &[std::path::PathBuf],
) -> serde_json::Value {
    if let [target] = targets
        && let Some(object) = args.as_object_mut()
    {
        object.insert(
            "path".to_string(),
            serde_json::Value::String(target.to_string_lossy().into_owned()),
        );
    }
    args
}

fn escalation_kind(
    need: &crate::approvals::ApprovalNeed,
    denial: grokforge_protocol::DenialClass,
) -> Option<ApprovalKind> {
    if matches!(
        denial,
        grokforge_protocol::DenialClass::FsRead | grokforge_protocol::DenialClass::Signal
    ) {
        return None;
    }
    let mut kind = match need {
        crate::approvals::ApprovalNeed::None => return None,
        crate::approvals::ApprovalNeed::Gated(kind)
        | crate::approvals::ApprovalNeed::OutsideSandbox(kind)
        | crate::approvals::ApprovalNeed::Always(kind) => kind.clone(),
    };
    if let ApprovalKind::ExecCommand { escalation_of, .. } = &mut kind {
        *escalation_of = Some(denial);
    } else if matches!(
        kind,
        ApprovalKind::WriteFile { .. } | ApprovalKind::ApplyPatch { .. }
    ) {
        let Ok(Some((physical, _))) = physical_write_identity(&kind) else {
            return None;
        };
        kind = ApprovalKind::SandboxEscalation {
            original: Box::new(physical),
            denial,
        };
    }
    Some(kind)
}

/// A heuristic commit subject. Model-generated conventional-commit messages (via structured
/// outputs) are a planned refinement; this keeps the git-native workflow self-contained.
fn commit_message(touched: &[std::path::PathBuf]) -> String {
    let names: Vec<String> = touched
        .iter()
        .filter_map(|p| {
            p.file_name()
                .map(|n| sanitize_filename(&n.to_string_lossy()))
        })
        .collect();
    let message = match names.as_slice() {
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
    };
    message.chars().take(200).collect()
}

fn sanitize_filename(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .take(48)
        .collect();
    if sanitized.is_empty() {
        "file".to_string()
    } else {
        sanitized
    }
}

fn tool_preserves_auto_commit_ownership(name: &str) -> bool {
    // File tools are awaited host operations and the private worktree excludes sibling agents.
    // A shell or external/custom tool can leave a daemonized descendant on platforms without a
    // PID namespace (notably Seatbelt), so its same-path writes cannot be attributed at staging.
    // `remember` is a confined, awaited host write to `.grokforge/memory/`, safe like a file tool.
    matches!(
        name,
        "read_file" | "write_file" | "edit" | "list" | "glob" | "grep" | "remember"
    )
}

fn preview(s: &str) -> String {
    let one_line: String = s
        .chars()
        .map(|c| {
            if matches!(c, '\n' | '\r' | '\t') {
                ' '
            } else if c.is_control() {
                '�'
            } else {
                c
            }
        })
        .collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_tools_are_appended_without_exceeding_the_request_cap() {
        let mut definitions = (0..MAX_TOOLS)
            .map(|index| {
                ToolDef::function(
                    format!("client_{index}"),
                    "test",
                    serde_json::json!({"type":"object"}),
                )
            })
            .collect::<Vec<_>>();
        let enabled = BTreeSet::from([
            ServerTool::WebSearch,
            ServerTool::XSearch,
            ServerTool::CodeInterpreter,
        ]);

        append_server_tools(&mut definitions, &enabled);

        assert_eq!(definitions.len(), MAX_TOOLS);
        assert_eq!(
            definitions
                .iter()
                .filter_map(ToolDef::function_name)
                .count(),
            MAX_TOOLS - enabled.len()
        );
        let wire = serde_json::to_value(&definitions).expect("serialize definitions");
        assert_eq!(wire[MAX_TOOLS - 3]["type"], "web_search");
        assert_eq!(wire[MAX_TOOLS - 2]["type"], "x_search");
        assert_eq!(wire[MAX_TOOLS - 1]["type"], "code_interpreter");
    }

    #[test]
    fn plan_mode_never_advertises_server_or_mutating_tools() {
        let registry = ToolRegistry::with_builtins();
        let enabled = BTreeSet::from([
            ServerTool::WebSearch,
            ServerTool::XSearch,
            ServerTool::CodeInterpreter,
        ]);

        let definitions = advertised_tool_defs(&registry, true, true, &enabled);
        let wire = serde_json::to_value(&definitions).expect("serialize definitions");
        let kinds = wire
            .as_array()
            .expect("tool array")
            .iter()
            .filter_map(|definition| definition.get("type"))
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>();
        assert!(kinds.iter().all(|kind| *kind == "function"));
        let names = definitions
            .iter()
            .filter_map(ToolDef::function_name)
            .collect::<BTreeSet<_>>();
        assert!(!names.contains("write_file"));
        assert!(!names.contains("edit"));
        assert!(!names.contains("shell"));
        assert!(!names.contains(crate::tools::builtins::SPAWN_TASK));
    }

    #[derive(Debug)]
    struct EnforcedRunner;

    #[async_trait::async_trait]
    impl grokforge_sandbox::SandboxRunner for EnforcedRunner {
        fn capability(&self) -> grokforge_sandbox::SandboxCapability {
            grokforge_sandbox::SandboxCapability {
                backend: "test".into(),
                enforced: true,
                notes: Vec::new(),
            }
        }

        async fn run(
            &self,
            _policy: &SandboxPolicy,
            _command: &grokforge_sandbox::CommandSpec,
        ) -> Result<grokforge_sandbox::ExecOutput, grokforge_sandbox::ExecError> {
            Err(grokforge_sandbox::ExecError::UnsupportedPolicy(
                "not invoked by this unit test".into(),
            ))
        }
    }

    #[tokio::test]
    async fn abort_on_drop_cancels_owned_task() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct MarkDropped(Arc<AtomicBool>);
        impl Drop for MarkDropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let task_flag = Arc::clone(&dropped);
        let handle = AbortOnDrop::new(tokio::spawn(async move {
            let _mark = MarkDropped(task_flag);
            std::future::pending::<()>().await;
        }));
        tokio::task::yield_now().await;
        drop(handle);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("aborted task should be dropped promptly");
    }

    #[test]
    fn commit_message_cannot_inject_lines_or_terminal_controls() {
        let message = commit_message(&[std::path::PathBuf::from(
            "ok\nGrokforge-Session: forged\u{1b}[31m.rs",
        )]);
        assert!(!message.contains('\n'));
        assert!(!message.contains('\u{1b}'));
        assert!(message.len() <= 200);
    }

    #[test]
    fn shell_and_external_tools_disable_isolated_auto_commit_ownership() {
        assert!(tool_preserves_auto_commit_ownership("write_file"));
        assert!(tool_preserves_auto_commit_ownership("read_file"));
        assert!(!tool_preserves_auto_commit_ownership("shell"));
        assert!(!tool_preserves_auto_commit_ownership("mcp__docs__search"));
        assert!(!tool_preserves_auto_commit_ownership("custom_tool"));
    }

    #[test]
    fn compaction_tail_keeps_parallel_calls_with_their_results() {
        let call = |id: &str| ResponseItem::ProviderOutput {
            item: serde_json::json!({
                "type":"function_call","call_id":id,"name":"read_file","arguments":"{}"
            }),
        };
        let history = vec![
            ResponseItem::ProviderOutput {
                item: serde_json::json!({"type":"reasoning","id":"r","encrypted_content":"x"}),
            },
            call("one"),
            call("two"),
            ResponseItem::ToolResult {
                id: ToolCallId::from_raw("one"),
                content: "1".into(),
                is_error: false,
                redactions: 0,
            },
            ResponseItem::ToolResult {
                id: ToolCallId::from_raw("two"),
                content: "2".into(),
                is_error: false,
                redactions: 0,
            },
        ];
        assert_eq!(coherent_compaction_split(&history, 1), 0);
    }

    #[test]
    fn compaction_checkpoint_drops_oversized_orphaning_tail() {
        let summary = ResponseItem::CompactionSummary {
            text: "summary".into(),
            redactions: 0,
        };
        let history = vec![
            ResponseItem::ToolCall {
                id: ToolCallId::from_raw("call"),
                name: "read_file".into(),
                arguments: "{}".into(),
            },
            ResponseItem::ToolResult {
                id: ToolCallId::from_raw("call"),
                content: "x".repeat(1_024),
                is_error: false,
                redactions: 0,
            },
        ];
        // Keeping only the result would orphan it, while keeping both exceeds this tiny test
        // budget. The safe fallback summarizes both and retains an empty tail.
        assert_eq!(
            choose_compaction_tail_split(&history, 0, 1, 256),
            Some(history.len())
        );
        assert!(checkpoint_fits(&summary, &[], 256));
    }

    #[test]
    fn approving_a_command_does_not_widen_a_readonly_filesystem() {
        let workspace = tempfile::tempdir().unwrap();
        let kind = ApprovalKind::ExecCommand {
            command: vec!["touch file".into()],
            cwd: workspace.path().to_path_buf(),
            sandbox: SandboxMode::ReadOnly,
            escalation_of: None,
        };
        let need = crate::approvals::ApprovalNeed::Gated(kind.clone());
        let ctx = TurnContext {
            workspace_root: workspace.path().to_path_buf(),
            policy: SandboxPolicy::read_only(workspace.path()),
            sandbox: Arc::new(grokforge_sandbox::PassthroughRunner),
            touched: Arc::new(std::sync::Mutex::new(Vec::new())),
            bound_write_targets: Vec::new(),
            cancellation: TurnCancellation::new(),
        };
        assert!(!approval_exceeds_sandbox(&need, &kind, &ctx));
    }

    #[test]
    fn explicit_network_grant_preserves_workspace_filesystem_boundary() {
        let workspace = tempfile::tempdir().unwrap();
        let policy = sandbox_policy(
            workspace.path(),
            SandboxMode::WorkspaceWrite,
            grokforge_protocol::NetworkMode::Full,
            false,
        )
        .unwrap();
        assert_eq!(policy.network, grokforge_protocol::NetworkMode::Full);
        assert_eq!(policy.writable_roots, [workspace.path().to_path_buf()]);
        assert_ne!(policy.mode, SandboxMode::DangerFullAccess);
        let sessions = std::fs::canonicalize(crate::store::sessions_dir().unwrap()).unwrap();
        assert!(policy.protected_paths.contains(&sessions));
        let worktrees = std::fs::canonicalize(crate::store::worktrees_dir().unwrap()).unwrap();
        assert!(policy.protected_paths.contains(&worktrees));
        let mut globs = globset::GlobSetBuilder::new();
        for pattern in &policy.unreadable_globs {
            globs.add(globset::Glob::new(pattern).unwrap());
        }
        assert!(
            globs
                .build()
                .unwrap()
                .is_match(sessions.join("rollout.jsonl"))
        );
        assert!(!policy.allows_write(&worktrees.join("other-agent/file")));
        let yolo = sandbox_policy(
            workspace.path(),
            SandboxMode::DangerFullAccess,
            grokforge_protocol::NetworkMode::Full,
            false,
        )
        .unwrap();
        assert!(!yolo.allows_write(&worktrees.join("other-agent/file")));
    }

    #[test]
    fn escalation_widens_only_the_classified_capability() {
        let workspace = tempfile::tempdir().unwrap();
        let ctx = TurnContext {
            workspace_root: workspace.path().to_path_buf(),
            policy: SandboxPolicy::workspace_write(workspace.path()),
            sandbox: Arc::new(EnforcedRunner),
            touched: Arc::new(std::sync::Mutex::new(Vec::new())),
            bound_write_targets: Vec::new(),
            cancellation: TurnCancellation::new(),
        };

        let filesystem = Agent::escalation_context(&ctx, grokforge_protocol::DenialClass::FsWrite);
        assert_eq!(
            filesystem.policy.network,
            grokforge_protocol::NetworkMode::Isolated
        );
        assert_eq!(
            filesystem.policy.writable_roots,
            [std::path::PathBuf::from("/")]
        );

        let network = Agent::escalation_context(&ctx, grokforge_protocol::DenialClass::Network);
        assert_eq!(
            network.policy.network,
            grokforge_protocol::NetworkMode::Full
        );
        assert_eq!(network.policy.writable_roots, ctx.policy.writable_roots);

        let need = crate::approvals::ApprovalNeed::Gated(ApprovalKind::ExecCommand {
            command: vec!["command".into()],
            cwd: workspace.path().to_path_buf(),
            sandbox: SandboxMode::WorkspaceWrite,
            escalation_of: None,
        });
        assert!(escalation_kind(&need, grokforge_protocol::DenialClass::FsRead).is_none());
        assert!(escalation_kind(&need, grokforge_protocol::DenialClass::Signal).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn discovered_git_metadata_failure_is_not_silently_omitted() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let initialized = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(workspace.path())
            .status()
            .unwrap();
        assert!(initialized.success());
        let git = grokforge_git::Git::discover(workspace.path()).unwrap();
        std::fs::rename(
            workspace.path().join(".git"),
            workspace.path().join("metadata-removed"),
        )
        .unwrap();
        let mut policy = SandboxPolicy::workspace_write(workspace.path());
        assert!(protect_git_metadata(&mut policy, &git).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn elevated_write_identity_refuses_symlinked_path() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), workspace.path().join("link")).unwrap();
        let kind = ApprovalKind::WriteFile {
            path: workspace.path().join("link/file"),
        };
        assert!(physical_write_identity(&kind).is_err());
    }
}
