//! End-to-end agent-loop tests against the mock xAI server. These exercise the M2 exit
//! criteria: a tool-using turn that creates a file, secrets never leaving the machine, and
//! ledger byte-reconciliation.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value
)]

use std::sync::Arc;

use async_trait::async_trait;
use grokforge_core::{
    Agent, ApprovalNeed, Approver, AutoApprover, Session, SessionConfig, Tool, ToolInvocation,
    ToolOutput, ToolRegistry, ToolSpec, TurnCancellation,
};
use grokforge_protocol::{ApprovalRequest, Decision, EventMsg, SandboxMode, StopReason};
use grokforge_sandbox::{
    CommandSpec, ExecError, ExecOutput, PassthroughRunner, SandboxCapability, SandboxRunner,
};
use grokforge_test_support::{MockXai, Reply};
use grokforge_xai::{RetryConfig, XaiClient};
use serde_json::json;
use tokio::sync::mpsc;

#[derive(Debug)]
struct DelayedHostMutation {
    started: Arc<tokio::sync::Notify>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl Tool for DelayedHostMutation {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "delayed_host_mutation".to_string(),
            description: "test-only delayed host mutation".to_string(),
            parameters: json!({"type":"object","properties":{}}),
            mutating: true,
            parallel_safe: false,
        }
    }

    fn approval(
        &self,
        _args: &serde_json::Value,
        _ctx: &grokforge_core::TurnContext,
    ) -> ApprovalNeed {
        ApprovalNeed::None
    }

    async fn invoke(&self, _inv: ToolInvocation<'_>) -> ToolOutput {
        let started = Arc::clone(&self.started);
        let finished = Arc::clone(&self.finished);
        match tokio::task::spawn_blocking(move || {
            started.notify_one();
            std::thread::sleep(std::time::Duration::from_millis(150));
            finished.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .await
        {
            Ok(()) => ToolOutput::success("host mutation finished"),
            Err(error) => ToolOutput::failure(format!("host mutation task failed: {error}")),
        }
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct EnforcedTestRunner;

#[cfg(unix)]
#[async_trait]
impl SandboxRunner for EnforcedTestRunner {
    fn capability(&self) -> SandboxCapability {
        SandboxCapability {
            backend: "test".into(),
            enforced: true,
            notes: vec![],
        }
    }

    async fn run(
        &self,
        _policy: &grokforge_protocol::SandboxPolicy,
        command: &CommandSpec,
    ) -> Result<ExecOutput, ExecError> {
        grokforge_sandbox::run_capture(command).await
    }
}

/// Build an SSE response that requests a single tool call, then finishes.
fn tool_call_then_done(name: &str, args: serde_json::Value) -> Reply {
    Reply::sse_events(&[
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": name,
                "arguments": args.to_string()
            }
        }),
        json!({"type":"response.completed","response":{"status":"requires_action"}}),
    ])
}

fn parallel_tool_calls_then_done() -> Reply {
    Reply::sse_events(&[
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "function_call", "id": "fc_1", "call_id": "call_1",
                "name": "write_file", "arguments": json!({"path":"one","content":"1"}).to_string()
            }
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": {
                "type": "function_call", "id": "fc_2", "call_id": "call_2",
                "name": "write_file", "arguments": json!({"path":"two","content":"2"}).to_string()
            }
        }),
        json!({"type":"response.completed","response":{"status":"requires_action"}}),
    ])
}

/// A single response that asks to spawn two subagents at once (exercises the parallel batch path).
fn two_spawn_tasks() -> Reply {
    Reply::sse_events(&[
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "function_call", "id": "fc_1", "call_id": "call_1",
                "name": "spawn_task", "arguments": json!({"prompt":"task one"}).to_string()
            }
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": {
                "type": "function_call", "id": "fc_2", "call_id": "call_2",
                "name": "spawn_task", "arguments": json!({"prompt":"task two"}).to_string()
            }
        }),
        json!({"type":"response.completed","response":{"status":"requires_action"}}),
    ])
}

/// A response that just says something and ends the turn.
fn final_text(text: &str) -> Reply {
    Reply::sse_events(&[
        json!({"type":"response.output_text.delta","delta": text}),
        json!({"type":"response.completed","response":{"status":"completed",
            "usage":{"input_tokens":50,"output_tokens":10,
                "input_tokens_details":{"cached_tokens":20}}}}),
    ])
}

fn agent_for(mock: &MockXai, events: mpsc::UnboundedSender<EventMsg>) -> Agent {
    let client = XaiClient::new(&mock.base_url(), "test-key")
        .unwrap()
        .with_retry(RetryConfig {
            max_attempts: 2,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
        });
    Agent::new(
        client,
        ToolRegistry::with_builtins(),
        Arc::new(PassthroughRunner),
        Arc::new(AutoApprover::yolo()),
        events,
    )
}

#[cfg(unix)]
#[tokio::test]
async fn creates_a_file_via_write_tool() {
    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        // Turn iteration 1: ask to write hello.py. Iteration 2: final message.
        .route(
            "/v1/responses",
            tool_call_then_done(
                "write_file",
                json!({ "path": "hello.py", "content": "print('hi')\n" }),
            ),
        )
        .route("/v1/responses", final_text("Done — created hello.py."))
        .start()
        .await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(
        SessionConfig::new(workspace.path().to_path_buf(), "grok-build-0.1").with_policy(
            grokforge_protocol::ApprovalPolicy::Never,
            SandboxMode::DangerFullAccess,
        ),
    );
    let mut rollout = None;

    let stop = agent
        .run_turn(&mut session, "create hello.py that prints hi", &mut rollout)
        .await;

    assert_eq!(stop, StopReason::EndTurn);
    let created = workspace.path().join("hello.py");
    assert!(created.exists(), "hello.py should have been created");
    assert_eq!(std::fs::read_to_string(created).unwrap(), "print('hi')\n");

    // The transcript should contain a tool call and its result.
    let mut saw_tool_end = false;
    while let Ok(ev) = rx.try_recv() {
        if let EventMsg::ToolCallEnd { ok, .. } = ev {
            assert!(ok);
            saw_tool_end = true;
        }
    }
    assert!(saw_tool_end);
}

#[tokio::test]
async fn cancellation_awaits_an_already_running_host_mutation_and_records_its_result() {
    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            tool_call_then_done("delayed_host_mutation", json!({})),
        )
        .start()
        .await;
    let started = Arc::new(tokio::sync::Notify::new());
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut registry = ToolRegistry::with_builtins();
    registry.register(Arc::new(DelayedHostMutation {
        started: Arc::clone(&started),
        finished: Arc::clone(&finished),
    }));
    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = Agent::new(
        XaiClient::new(&mock.base_url(), "k").unwrap(),
        registry,
        Arc::new(PassthroughRunner),
        Arc::new(AutoApprover::yolo()),
        tx,
    );
    let cancellation = TurnCancellation::new();
    let task_cancellation = cancellation.clone();
    let mut session = Session::new(
        SessionConfig::new(workspace.path().to_path_buf(), "m").with_policy(
            grokforge_protocol::ApprovalPolicy::Never,
            SandboxMode::DangerFullAccess,
        ),
    );
    let handle = tokio::spawn(async move {
        let stop = agent
            .run_turn_cancellable(&mut session, "mutate", &mut None, &task_cancellation)
            .await;
        (stop, session)
    });

    started.notified().await;
    cancellation.cancel();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(
        !handle.is_finished(),
        "host mutation was detached on cancellation"
    );
    let (stop, session) = handle.await.unwrap();
    assert_eq!(stop, StopReason::Interrupted);
    assert!(finished.load(std::sync::atomic::Ordering::SeqCst));
    assert!(session.history.iter().any(|item| matches!(
        item,
        grokforge_protocol::ResponseItem::ToolResult { content, is_error: false, .. }
            if content.contains("host mutation finished")
    )));
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_reaches_the_sandbox_and_persists_an_interrupted_result() {
    let workspace = tempfile::tempdir().unwrap();
    let marker = workspace.path().join("sandbox-grandchild-survived");
    let command = format!("(sleep 1; touch '{}') & wait", marker.display());
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            tool_call_then_done("shell", json!({"command": command})),
        )
        .start()
        .await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let agent = Agent::new(
        XaiClient::new(&mock.base_url(), "k").unwrap(),
        ToolRegistry::with_builtins(),
        Arc::new(EnforcedTestRunner),
        Arc::new(AutoApprover::yolo()),
        tx,
    );
    let cancellation = TurnCancellation::new();
    let task_cancellation = cancellation.clone();
    let mut session = Session::new(
        SessionConfig::new(workspace.path().to_path_buf(), "m").with_policy(
            grokforge_protocol::ApprovalPolicy::Never,
            SandboxMode::DangerFullAccess,
        ),
    );
    let handle = tokio::spawn(async move {
        let stop = agent
            .run_turn_cancellable(&mut session, "run", &mut None, &task_cancellation)
            .await;
        (stop, session)
    });
    while let Some(event) = rx.recv().await {
        if matches!(event, EventMsg::ToolCallBegin { .. }) {
            break;
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cancellation.cancel();
    let (stop, session) = handle.await.unwrap();
    assert_eq!(stop, StopReason::Interrupted);
    assert!(session.history.iter().any(|item| matches!(
        item,
        grokforge_protocol::ResponseItem::ToolResult { content, is_error: true, .. }
            if content.contains("command killed and reaped")
    )));
    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
    assert!(
        !marker.exists(),
        "sandboxed grandchild survived cancellation"
    );
}

#[tokio::test]
async fn planted_env_secret_never_reaches_the_wire() {
    let workspace = tempfile::tempdir().unwrap();
    // Plant a .env with a secret. The model will try to read it.
    std::fs::write(
        workspace.path().join(".env"),
        "XAI_API_KEY=xai-SUPERSECRETVALUE0123456789\n",
    )
    .unwrap();

    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            tool_call_then_done("read_file", json!({ "path": ".env" })),
        )
        .route("/v1/responses", final_text("I could not read the secret."))
        .start()
        .await;

    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(
        SessionConfig::new(workspace.path().to_path_buf(), "grok-build-0.1").with_policy(
            grokforge_protocol::ApprovalPolicy::Never,
            SandboxMode::DangerFullAccess,
        ),
    );
    let mut rollout = None;
    agent
        .run_turn(&mut session, "read the .env file", &mut rollout)
        .await;

    // No request body the server received may contain the secret value.
    for req in mock.received() {
        let body = String::from_utf8_lossy(&req.body);
        assert!(
            !body.contains("SUPERSECRETVALUE"),
            "secret leaked into a request body"
        );
    }
}

#[tokio::test]
async fn inline_secret_in_tool_output_is_redacted_before_resend() {
    let workspace = tempfile::tempdir().unwrap();
    // A non-blocked file that nonetheless contains a secret inline.
    std::fs::write(
        workspace.path().join("notes.txt"),
        "deploy token: xai-INLINELEAK0123456789ABC end\n",
    )
    .unwrap();

    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            tool_call_then_done("read_file", json!({ "path": "notes.txt" })),
        )
        .route("/v1/responses", final_text("noted"))
        .start()
        .await;

    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(
        SessionConfig::new(workspace.path().to_path_buf(), "grok-build-0.1").with_policy(
            grokforge_protocol::ApprovalPolicy::Never,
            SandboxMode::DangerFullAccess,
        ),
    );
    let mut rollout = None;
    agent
        .run_turn(&mut session, "read notes.txt", &mut rollout)
        .await;

    // The tool result is fed back on the second request; the inline secret must be redacted.
    for req in mock.received() {
        let body = String::from_utf8_lossy(&req.body);
        assert!(!body.contains("INLINELEAK"), "inline secret leaked");
    }
}

#[tokio::test]
async fn ledger_reconciles_with_every_received_request() {
    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route("/v1/responses", final_text("hi"))
        .start()
        .await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(SessionConfig::new(
        workspace.path().to_path_buf(),
        "grok-build-0.1",
    ));
    let mut rollout = None;
    agent
        .run_turn(&mut session, "hello there", &mut rollout)
        .await;

    // Sum the ledger entries the core emitted; it must equal the bytes the server received.
    let mut ledger_total = 0usize;
    while let Ok(ev) = rx.try_recv() {
        if let EventMsg::LedgerAppended(entry) = ev {
            ledger_total += entry.bytes;
        }
    }
    let received = mock.last_request().expect("a request");
    assert_eq!(
        ledger_total,
        received.body_len(),
        "ledger must reconcile byte-for-byte with the request body"
    );
}

#[tokio::test]
async fn rollout_persists_the_transcript() {
    let workspace = tempfile::tempdir().unwrap();
    let rollout_dir = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route("/v1/responses", final_text("all done"))
        .start()
        .await;

    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(SessionConfig::new(
        workspace.path().to_path_buf(),
        "grok-build-0.1",
    ));
    let mut rollout = Some(
        grokforge_core::RolloutWriter::create(rollout_dir.path(), session.id)
            .await
            .unwrap(),
    );
    agent
        .run_turn(&mut session, "do the thing", &mut rollout)
        .await;

    let path = rollout.as_ref().unwrap().path().to_path_buf();
    let items = grokforge_core::RolloutWriter::read_all(&path)
        .await
        .unwrap();
    // At least the user message and the assistant reply were recorded.
    assert!(items.len() >= 2);
}

#[tokio::test]
async fn long_history_is_compacted_at_turn_end() {
    use grokforge_protocol::ResponseItem;

    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        // Oversized prior history is compacted before the next turn request.
        .route(
            "/v1/responses",
            final_text("Earlier: set up the project and fixed two tests."),
        )
        .route("/v1/responses", final_text("done"))
        .start()
        .await;

    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);

    let mut config = SessionConfig::new(workspace.path().to_path_buf(), "grok-build-0.1");
    config.auto_commit = false;
    config.compaction_trigger_bytes = 100; // force compaction
    config.compaction_keep_tail = 2;
    let mut session = Session::new(config);
    // Seed a long prior history.
    for i in 0..10 {
        session.history.push(ResponseItem::assistant(
            format!("old message {i} ").repeat(10),
        ));
    }
    let before = session.history.len();

    agent.run_turn(&mut session, "continue", &mut None).await;

    // History was compacted: it now starts with a summary and is much shorter.
    assert!(session.history.len() < before, "history should shrink");
    assert!(
        matches!(
            session.history.first(),
            Some(ResponseItem::CompactionSummary { .. })
        ),
        "first item should be the compaction summary"
    );
    if let Some(ResponseItem::CompactionSummary { text, .. }) = session.history.first() {
        assert!(text.contains("Earlier: set up the project"));
    }
}

#[tokio::test]
async fn write_file_outside_workspace_is_refused_in_workspace_write() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = std::env::temp_dir().join("grokforge_wf_escape_probe.txt");
    let _ = std::fs::remove_file(&outside);

    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            tool_call_then_done(
                "write_file",
                json!({ "path": outside.to_string_lossy(), "content": "escaped" }),
            ),
        )
        .route("/v1/responses", final_text("blocked"))
        .start()
        .await;

    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    // workspace-write (not yolo): the file tool must refuse an out-of-workspace path.
    let mut session = Session::new(
        SessionConfig::new(workspace.path().to_path_buf(), "grok-build-0.1").with_policy(
            grokforge_protocol::ApprovalPolicy::Never,
            SandboxMode::WorkspaceWrite,
        ),
    );
    agent
        .run_turn(&mut session, "write outside", &mut None)
        .await;

    assert!(
        !outside.exists(),
        "write_file must not escape the workspace"
    );
}

#[tokio::test]
async fn plan_mode_refuses_writes() {
    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            tool_call_then_done(
                "write_file",
                json!({ "path": "in_plan.txt", "content": "no" }),
            ),
        )
        .route("/v1/responses", final_text("here is the plan"))
        .start()
        .await;

    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    // Even yolo config: plan mode forces read-only, so the write is refused.
    let mut session = Session::new(
        SessionConfig::new(workspace.path().to_path_buf(), "grok-build-0.1").with_policy(
            grokforge_protocol::ApprovalPolicy::Never,
            SandboxMode::DangerFullAccess,
        ),
    );
    agent
        .run_plan_turn(&mut session, "plan a change", &mut None)
        .await;

    assert!(
        !workspace.path().join("in_plan.txt").exists(),
        "plan mode must not write files"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn subagent_runs_in_an_isolated_worktree_branch() {
    // Requires git.
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: git unavailable");
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    let ws = workspace.path();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(ws)
            .output()
            .unwrap()
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t.dev"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(ws.join("README"), "x\n").unwrap();
    git(&["add", "README"]);
    git(&["commit", "-qm", "init"]);

    let mock = MockXai::builder()
        // 1: parent asks to spawn a subtask
        .route(
            "/v1/responses",
            tool_call_then_done("spawn_task", json!({ "prompt": "create sub.txt" })),
        )
        // 2: subagent writes a file
        .route(
            "/v1/responses",
            tool_call_then_done(
                "write_file",
                json!({ "path": "sub.txt", "content": "from subagent\n" }),
            ),
        )
        // 3: subagent final message
        .route("/v1/responses", final_text("wrote sub.txt"))
        // 4: parent final message
        .route("/v1/responses", final_text("subagent completed"))
        .start()
        .await;

    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(
        SessionConfig::new(ws.to_path_buf(), "grok-build-0.1").with_policy(
            grokforge_protocol::ApprovalPolicy::Never,
            SandboxMode::DangerFullAccess,
        ),
    );
    agent
        .run_turn(&mut session, "delegate a subtask", &mut None)
        .await;

    // A subagent branch was created and carries the file it wrote.
    let branches = String::from_utf8(git(&["branch", "--list", "gf/agent/*"]).stdout).unwrap();
    assert!(
        !branches.trim().is_empty(),
        "a gf/agent/* branch should exist"
    );
    let branch = branches.trim().trim_start_matches('*').trim().to_string();
    let show = git(&["show", &format!("{branch}:sub.txt")]);
    assert!(
        show.status.success(),
        "sub.txt should exist on the subagent branch"
    );
    assert_eq!(String::from_utf8(show.stdout).unwrap(), "from subagent\n");
    assert!(
        !ws.join(".grokforge/worktrees").exists(),
        "subagent worktrees must not be placed inside the parent workspace"
    );

    // The spawn_task tool result was recorded in the parent transcript.
    let has_subagent_result = session.history.iter().any(|i| {
        matches!(i, grokforge_protocol::ResponseItem::ToolResult { content, .. }
            if content.contains("Subagent finished on branch"))
    });
    assert!(has_subagent_result, "parent should see the subagent result");
}

#[tokio::test]
async fn oversized_request_fails_with_a_budget_error_not_a_provider_400() {
    let workspace = tempfile::tempdir().unwrap();
    // No routes are registered: the budget guard must stop the turn before any request is sent,
    // so the mock is never contacted.
    let mock = MockXai::builder().start().await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut config = SessionConfig::new(workspace.path().to_path_buf(), "grok-build-0.1")
        .with_policy(
            grokforge_protocol::ApprovalPolicy::Never,
            SandboxMode::DangerFullAccess,
        );
    // One token over the output reserve → a ~0-byte input budget, so even the base request (system
    // prompt + the user message) exceeds it and the guard fires immediately.
    config.context_window_tokens = Some(16_385);
    let mut session = Session::new(config);

    let stop = agent.run_turn(&mut session, "hello", &mut None).await;

    assert_eq!(stop, StopReason::Error);
    let mut saw_budget_error = false;
    while let Ok(ev) = rx.try_recv() {
        if let EventMsg::Error {
            message,
            recoverable,
        } = ev
            && message.contains("input budget")
        {
            assert!(!recoverable, "the budget error is terminal for this turn");
            saw_budget_error = true;
        }
    }
    assert!(
        saw_budget_error,
        "an over-budget request should surface an actionable budget error, not be sent and 400"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn spawns_multiple_subagents_in_parallel() {
    // Requires git.
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: git unavailable");
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    let ws = workspace.path();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(ws)
            .output()
            .unwrap()
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t.dev"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(ws.join("README"), "x\n").unwrap();
    git(&["add", "README"]);
    git(&["commit", "-qm", "init"]);

    // Parent asks to spawn two subagents in one response; each subagent turn is a single
    // final message (both scripts are equivalent, so the concurrent request order is irrelevant).
    let mock = MockXai::builder()
        .route("/v1/responses", two_spawn_tasks())
        .route("/v1/responses", final_text("subtask one done"))
        .route("/v1/responses", final_text("subtask two done"))
        .route("/v1/responses", final_text("both subtasks delegated"))
        .start()
        .await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(
        SessionConfig::new(ws.to_path_buf(), "grok-build-0.1").with_policy(
            grokforge_protocol::ApprovalPolicy::Never,
            SandboxMode::DangerFullAccess,
        ),
    );
    let stop = agent
        .run_turn(&mut session, "delegate two subtasks", &mut None)
        .await;
    assert_eq!(stop, StopReason::EndTurn);

    // Each subagent got its own gf/agent/* branch.
    let branches = String::from_utf8(git(&["branch", "--list", "gf/agent/*"]).stdout).unwrap();
    let branch_count = branches
        .lines()
        .filter(|line| line.contains("gf/agent/"))
        .count();
    assert_eq!(
        branch_count, 2,
        "expected two subagent branches: {branches}"
    );

    // Both subagent results were recorded back into the parent transcript.
    let result_count = session
        .history
        .iter()
        .filter(|item| {
            matches!(item, grokforge_protocol::ResponseItem::ToolResult { content, .. }
                if content.contains("Subagent finished on branch"))
        })
        .count();
    assert_eq!(
        result_count, 2,
        "parent should record both subagent results"
    );

    // Both lanes emitted a start and a finish, each tagged with the batch total.
    let mut started = 0;
    let mut finished = 0;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            EventMsg::SubagentStarted { total, .. } => {
                assert_eq!(total, 2);
                started += 1;
            }
            EventMsg::SubagentFinished { ok, .. } => {
                assert!(ok, "subagents completed cleanly");
                finished += 1;
            }
            _ => {}
        }
    }
    assert_eq!(started, 2, "two SubagentStarted events");
    assert_eq!(finished, 2, "two SubagentFinished events");
}

#[tokio::test]
async fn headless_denies_and_continues_without_yolo() {
    // With the strict-ish default (OnRequest + read-only) and the default AutoApprover (no
    // allow rules), a write is auto-denied and the model is told — the turn still completes.
    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            tool_call_then_done("write_file", json!({ "path": "x.txt", "content": "nope" })),
        )
        .route("/v1/responses", final_text("understood, not writing"))
        .start()
        .await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = XaiClient::new(&mock.base_url(), "k").unwrap();
    let agent = Agent::new(
        client,
        ToolRegistry::with_builtins(),
        Arc::new(PassthroughRunner),
        Arc::new(AutoApprover::default()), // no allow rules -> deny
        tx,
    );
    let mut session = Session::new(
        SessionConfig::new(workspace.path().to_path_buf(), "grok-build-0.1").with_policy(
            grokforge_protocol::ApprovalPolicy::OnRequest,
            SandboxMode::ReadOnly,
        ),
    );
    let mut rollout = None;
    let stop = agent
        .run_turn(&mut session, "write x.txt", &mut rollout)
        .await;

    assert_eq!(stop, StopReason::EndTurn);
    assert!(
        !workspace.path().join("x.txt").exists(),
        "denied write must not create the file"
    );
    let mut saw_denied = false;
    while let Ok(ev) = rx.try_recv() {
        if let EventMsg::ToolCallEnd { ok: false, .. } = ev {
            saw_denied = true;
        }
    }
    assert!(saw_denied, "the denied tool call should be reported");
}

#[tokio::test]
async fn provider_tool_call_id_is_replayed_verbatim() {
    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[
                json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":{"type":"function_call","id":"fc_9","call_id":"call_9",
                        "name":"read_file","arguments":"{\"path\":\"missing.txt\"}"}
                }),
                json!({"type":"response.completed","response":{"status":"requires_action"}}),
            ]),
        )
        .route("/v1/responses", final_text("done"))
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(SessionConfig::new(
        workspace.path().to_path_buf(),
        "grok-build-0.1",
    ));
    agent.run_turn(&mut session, "read it", &mut None).await;

    let requests = mock.received();
    assert_eq!(requests.len(), 2);
    let input = requests[1].json()["input"].as_array().unwrap().clone();
    let replayed: Vec<&str> = input
        .iter()
        .filter_map(|item| item.get("call_id").and_then(|id| id.as_str()))
        .collect();
    assert_eq!(replayed, vec!["call_9", "call_9"]);
}

#[tokio::test]
async fn abrupt_stream_eof_is_an_error_and_partial_output_is_not_committed() {
    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[json!({
                "type":"response.output_text.delta","delta":"partial answer"
            })]),
        )
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(SessionConfig::new(
        workspace.path().to_path_buf(),
        "grok-build-0.1",
    ));
    let stop = agent.run_turn(&mut session, "hello", &mut None).await;
    assert_eq!(stop, StopReason::Error);
    assert_eq!(
        session.history.len(),
        1,
        "partial assistant output is not canonical"
    );
    assert_eq!(mock.received().len(), 1);
}

#[tokio::test]
async fn encrypted_reasoning_is_persisted_and_replayed_verbatim() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("a.txt"), "a").unwrap();
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[
                json!({"type":"response.output_item.done","output_index":0,"item":{
                    "type":"reasoning","id":"rs_provider_1","status":"completed",
                    "summary":[{"type":"summary_text","text":"brief"}],
                    "encrypted_content":"opaque+ciphertext=="
                }}),
                json!({"type":"response.output_item.done","output_index":1,"item":{
                    "type":"function_call","id":"fc_1","call_id":"provider_call_1",
                    "name":"read_file","arguments":"{\"path\":\"a.txt\"}"
                }}),
                json!({"type":"response.completed","response":{"status":"requires_action"}}),
            ]),
        )
        .route("/v1/responses", final_text("done"))
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(SessionConfig::new(
        workspace.path().to_path_buf(),
        "grok-build-0.1",
    ));
    agent.run_turn(&mut session, "read", &mut None).await;
    assert!(session.history.iter().any(|item| matches!(
        item,
        grokforge_protocol::ResponseItem::ProviderOutput { item }
            if item["id"] == "rs_provider_1"
                && item["encrypted_content"] == "opaque+ciphertext=="
    )));
    let second = mock.received()[1].json();
    let reasoning = second["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "reasoning")
        .unwrap();
    assert_eq!(reasoning["id"], "rs_provider_1");
    assert_eq!(reasoning["encrypted_content"], "opaque+ciphertext==");
}

#[tokio::test]
async fn mixed_provider_output_items_round_trip_in_order_without_duplicates() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("a.txt"), "a").unwrap();
    let reasoning = json!({
        "type":"reasoning","id":"rs_ordered","status":"completed","summary":[],
        "encrypted_content":"ciphertext"
    });
    let message = json!({
        "type":"message","id":"msg_ordered","status":"completed","role":"assistant",
        "content":[{"type":"output_text","text":"I will read it."}]
    });
    let function = json!({
        "type":"function_call","id":"fc_ordered","call_id":"call_ordered",
        "name":"read_file","arguments":"{\"path\":\"a.txt\"}"
    });
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[
                // Done events are deliberately delivered out of order. Persistence must follow
                // the provider's canonical output indices, not network arrival order.
                json!({"type":"response.output_item.done","output_index":2,"item":function.clone()}),
                json!({"type":"response.output_text.delta","delta":"I will read it."}),
                json!({"type":"response.output_item.done","output_index":0,"item":reasoning.clone()}),
                json!({"type":"response.output_item.done","output_index":1,"item":message.clone()}),
                json!({"type":"response.completed","response":{"status":"requires_action"}}),
            ]),
        )
        .route("/v1/responses", final_text("done"))
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(SessionConfig::new(
        workspace.path().to_path_buf(),
        "grok-build-0.1",
    ));
    agent.run_turn(&mut session, "read", &mut None).await;

    let second = mock.received()[1].json();
    let replay: Vec<&serde_json::Value> = second["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| {
            item.get("id").is_some_and(|id| {
                matches!(
                    id.as_str(),
                    Some("rs_ordered" | "msg_ordered" | "fc_ordered")
                )
            }) || item.get("type").and_then(serde_json::Value::as_str)
                == Some("function_call_output")
        })
        .collect();
    // Initial developer/user messages are filtered out by the IDs below; provider output stays
    // in exact order and the function call is not projected into a duplicate typed item.
    let ordered_types: Vec<&str> = replay
        .iter()
        .filter_map(|item| item["type"].as_str())
        .collect();
    assert_eq!(
        ordered_types,
        vec![
            "reasoning",
            "message",
            "function_call",
            "function_call_output"
        ]
    );
    assert_eq!(replay[0], &reasoning);
    assert_eq!(replay[1], &message);
    assert_eq!(replay[2], &function);
    assert_eq!(
        session
            .history
            .iter()
            .filter(|item| matches!(
                item,
                grokforge_protocol::ResponseItem::ProviderOutput { .. }
            ))
            .count(),
        3
    );
    assert!(!session.history.iter().any(|item| matches!(
        item,
        grokforge_protocol::ResponseItem::ToolCall { id, .. } if id.as_str() == "call_ordered"
    )));
}

#[tokio::test]
async fn retry_attempt_bytes_are_included_in_the_ledger() {
    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::status_with_retry_after(500, Some(0)),
        )
        .route("/v1/responses", final_text("done"))
        .start()
        .await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(SessionConfig::new(
        workspace.path().to_path_buf(),
        "grok-build-0.1",
    ));
    assert_eq!(
        agent.run_turn(&mut session, "hello", &mut None).await,
        StopReason::EndTurn
    );
    let ledger_bytes: usize = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            EventMsg::LedgerAppended(entry) => Some(entry.bytes),
            _ => None,
        })
        .sum();
    let wire_bytes: usize = mock
        .received()
        .iter()
        .map(grokforge_test_support::Received::body_len)
        .sum();
    assert_eq!(ledger_bytes, wire_bytes);
}

#[tokio::test]
async fn failed_compaction_preserves_full_history() {
    use grokforge_protocol::ResponseItem;

    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[json!({
                "type":"response.output_text.delta","delta":"partial summary"
            })]),
        )
        .route("/v1/responses", final_text("turn done"))
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut config = SessionConfig::new(workspace.path().to_path_buf(), "grok-build-0.1");
    config.auto_commit = false;
    config.compaction_trigger_bytes = 1;
    config.compaction_keep_tail = 2;
    let mut session = Session::new(config);
    for index in 0..10 {
        session
            .history
            .push(ResponseItem::assistant(format!("old {index}")));
    }
    agent.run_turn(&mut session, "continue", &mut None).await;
    assert_eq!(session.history.len(), 12);
    assert!(
        !session
            .history
            .iter()
            .any(|item| matches!(item, ResponseItem::CompactionSummary { .. }))
    );
}

#[tokio::test]
async fn persisted_compaction_resumes_the_same_visible_history() {
    use grokforge_protocol::ResponseItem;

    let workspace = tempfile::tempdir().unwrap();
    let rollout_dir = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route("/v1/responses", final_text("compact summary"))
        .route("/v1/responses", final_text("turn done"))
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut config = SessionConfig::new(workspace.path().to_path_buf(), "grok-build-0.1");
    config.auto_commit = false;
    config.compaction_trigger_bytes = 1;
    config.compaction_keep_tail = 2;
    let mut session = Session::new(config);
    for index in 0..10 {
        session
            .history
            .push(ResponseItem::assistant(format!("old {index}")));
    }
    let mut rollout = Some(
        grokforge_core::RolloutWriter::create(rollout_dir.path(), session.id)
            .await
            .unwrap(),
    );
    agent.run_turn(&mut session, "continue", &mut rollout).await;
    let resumed = grokforge_core::RolloutWriter::read_all(rollout.as_ref().unwrap().path())
        .await
        .unwrap();
    assert_eq!(resumed, session.history);
}

#[derive(Debug)]
struct AbortApprover;

#[async_trait]
impl Approver for AbortApprover {
    async fn request(&self, _request: ApprovalRequest) -> Decision {
        Decision::Abort
    }
}

#[tokio::test]
async fn approval_abort_stops_the_whole_turn() {
    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            tool_call_then_done("write_file", json!({"path":"x.txt","content":"no"})),
        )
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let client = XaiClient::new(&mock.base_url(), "k").unwrap();
    let agent = Agent::new(
        client,
        ToolRegistry::with_builtins(),
        Arc::new(PassthroughRunner),
        Arc::new(AbortApprover),
        tx,
    );
    let mut config = SessionConfig::new(workspace.path().to_path_buf(), "grok-build-0.1")
        .with_policy(
            grokforge_protocol::ApprovalPolicy::Untrusted,
            SandboxMode::DangerFullAccess,
        );
    config.compaction_trigger_bytes = 1;
    config.compaction_keep_tail = 1;
    // This test isolates approval abort semantics; compaction has separate recovery coverage.
    config.auto_compact = false;
    let mut session = Session::new(config);
    session
        .history
        .push(grokforge_protocol::ResponseItem::assistant(
            "old history that would otherwise compact",
        ));
    let stop = agent.run_turn(&mut session, "write", &mut None).await;
    assert_eq!(stop, StopReason::Interrupted);
    assert!(!workspace.path().join("x.txt").exists());
    assert_eq!(mock.received().len(), 1);
}

#[tokio::test]
async fn parallel_calls_after_an_abort_receive_interrupted_results() {
    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route("/v1/responses", parallel_tool_calls_then_done())
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let client = XaiClient::new(&mock.base_url(), "k").unwrap();
    let agent = Agent::new(
        client,
        ToolRegistry::with_builtins(),
        Arc::new(PassthroughRunner),
        Arc::new(AbortApprover),
        tx,
    );
    let mut session = Session::new(
        SessionConfig::new(workspace.path().to_path_buf(), "m").with_policy(
            grokforge_protocol::ApprovalPolicy::Untrusted,
            SandboxMode::DangerFullAccess,
        ),
    );
    assert_eq!(
        agent.run_turn(&mut session, "write", &mut None).await,
        StopReason::Interrupted
    );
    for id in ["call_1", "call_2"] {
        assert!(session.history.iter().any(|item| matches!(
            item,
            grokforge_protocol::ResponseItem::ToolResult { id: result_id, .. }
                if result_id.as_str() == id
        )));
    }
}

#[tokio::test]
async fn max_tokens_with_a_persisted_call_closes_the_call() {
    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[
                json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item": {"type":"function_call","id":"fc_limit","call_id":"call_limit",
                        "name":"read_file","arguments":"{}"}
                }),
                json!({"type":"response.incomplete","response":{"status":"incomplete",
                    "incomplete_details":{"reason":"max_output_tokens"}}}),
            ]),
        )
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(SessionConfig::new(workspace.path().to_path_buf(), "m"));
    assert_eq!(
        agent.run_turn(&mut session, "read", &mut None).await,
        StopReason::Error
    );
    assert!(session.history.iter().any(|item| matches!(
        item,
        grokforge_protocol::ResponseItem::ToolResult { id, content, is_error: true, .. }
            if id.as_str() == "call_limit" && content.contains("interrupted before result")
    )));
}

#[tokio::test]
async fn excessive_tiny_response_events_are_bounded() {
    let workspace = tempfile::tempdir().unwrap();
    let mut body = Vec::new();
    for _ in 0..=32_768 {
        body.extend_from_slice(
            b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
        );
    }
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::Stream {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                chunks: vec![body],
            },
        )
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(SessionConfig::new(workspace.path().to_path_buf(), "m"));
    assert_eq!(
        agent.run_turn(&mut session, "hello", &mut None).await,
        StopReason::Error
    );
}

#[tokio::test]
async fn provider_cannot_reuse_a_call_id_from_visible_history() {
    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[
                json!({"type":"response.output_item.done","output_index":0,"item":{
                    "type":"function_call","id":"fc_reused","call_id":"call_reused",
                    "name":"write_file","arguments":"{\"path\":\"bad.txt\",\"content\":\"bad\"}"
                }}),
                json!({"type":"response.completed","response":{"status":"requires_action"}}),
            ]),
        )
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(SessionConfig::new(workspace.path().to_path_buf(), "m"));
    session
        .history
        .push(grokforge_protocol::ResponseItem::ProviderOutput {
            item: json!({
                "type":"function_call","id":"fc_old","call_id":"call_reused",
                "name":"read_file","arguments":"{\"path\":\"old.txt\"}"
            }),
        });
    session
        .history
        .push(grokforge_protocol::ResponseItem::ToolResult {
            id: grokforge_protocol::ToolCallId::from_raw("call_reused"),
            content: "old result".into(),
            is_error: false,
            redactions: 0,
        });

    assert_eq!(
        agent.run_turn(&mut session, "try again", &mut None).await,
        StopReason::Error
    );
    assert!(!workspace.path().join("bad.txt").exists());
    assert_eq!(
        session
            .history
            .iter()
            .filter(
                |item| matches!(item, grokforge_protocol::ResponseItem::ProviderOutput { item }
                if item.get("call_id").and_then(serde_json::Value::as_str) == Some("call_reused"))
            )
            .count(),
        1,
        "the reused provider call must not be persisted"
    );
}

#[tokio::test]
async fn duplicate_provider_call_ids_are_rejected_before_invocation() {
    let workspace = tempfile::tempdir().unwrap();
    let duplicate = |output_index: usize, item_id: &str, path: &str| {
        json!({
            "type":"response.output_item.done",
            "output_index":output_index,
            "item":{"type":"function_call","id":item_id,"call_id":"duplicate_call",
                "name":"write_file","arguments":json!({"path":path,"content":"bad"}).to_string()}
        })
    };
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[
                duplicate(0, "fc_one", "one.txt"),
                duplicate(1, "fc_two", "two.txt"),
                json!({"type":"response.completed","response":{"status":"requires_action"}}),
            ]),
        )
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(SessionConfig::new(workspace.path().to_path_buf(), "m"));
    assert_eq!(
        agent.run_turn(&mut session, "write", &mut None).await,
        StopReason::Error
    );
    assert!(!workspace.path().join("one.txt").exists());
    assert!(!workspace.path().join("two.txt").exists());
    assert_eq!(session.history.len(), 1);
}

#[tokio::test]
async fn out_of_order_tool_items_are_executed_in_canonical_output_order() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("first.txt"), "first").unwrap();
    std::fs::write(workspace.path().join("second.txt"), "second").unwrap();
    let call = |output_index: usize, id: &str, path: &str| {
        json!({
            "type":"response.output_item.done",
            "output_index":output_index,
            "item":{"type":"function_call","id":format!("fc_{id}"),"call_id":id,
                "name":"read_file","arguments":json!({"path":path}).to_string()}
        })
    };
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[
                call(1, "call_second", "second.txt"),
                call(0, "call_first", "first.txt"),
                json!({"type":"response.completed","response":{"status":"requires_action"}}),
            ]),
        )
        .route("/v1/responses", final_text("done"))
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(SessionConfig::new(workspace.path().to_path_buf(), "m"));
    assert_eq!(
        agent.run_turn(&mut session, "read both", &mut None).await,
        StopReason::EndTurn
    );

    let provider_ids: Vec<&str> = session
        .history
        .iter()
        .filter_map(|item| match item {
            grokforge_protocol::ResponseItem::ProviderOutput { item }
                if item["type"] == "function_call" =>
            {
                item["call_id"].as_str()
            }
            _ => None,
        })
        .collect();
    let result_ids: Vec<&str> = session
        .history
        .iter()
        .filter_map(|item| match item {
            grokforge_protocol::ResponseItem::ToolResult { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(provider_ids, ["call_first", "call_second"]);
    assert_eq!(result_ids, ["call_first", "call_second"]);
}

#[tokio::test]
async fn duplicate_or_gapped_output_indices_are_rejected_before_invocation() {
    for indices in [[0, 0], [0, 2]] {
        let workspace = tempfile::tempdir().unwrap();
        let events = [
            json!({"type":"response.output_item.done","output_index":indices[0],"item":{
                "type":"function_call","id":"fc_one","call_id":"call_one",
                "name":"write_file","arguments":"{\"path\":\"one.txt\",\"content\":\"bad\"}"
            }}),
            json!({"type":"response.output_item.done","output_index":indices[1],"item":{
                "type":"function_call","id":"fc_two","call_id":"call_two",
                "name":"write_file","arguments":"{\"path\":\"two.txt\",\"content\":\"bad\"}"
            }}),
            json!({"type":"response.completed","response":{"status":"requires_action"}}),
        ];
        let mock = MockXai::builder()
            .route("/v1/responses", Reply::sse_events(&events))
            .start()
            .await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let agent = agent_for(&mock, tx);
        let mut session = Session::new(SessionConfig::new(workspace.path().to_path_buf(), "m"));
        assert_eq!(
            agent.run_turn(&mut session, "write", &mut None).await,
            StopReason::Error
        );
        assert_eq!(session.history.len(), 1);
        assert!(!workspace.path().join("one.txt").exists());
        assert!(!workspace.path().join("two.txt").exists());
    }
}

#[tokio::test]
async fn finalized_message_mismatch_streams_but_is_not_persisted_or_executed() {
    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[
                json!({"type":"response.output_text.delta","delta":"streamed text"}),
                json!({"type":"response.output_item.done","output_index":0,"item":{
                    "type":"message","id":"msg","status":"completed","role":"assistant",
                    "content":[{"type":"output_text","text":"different finalized text"}]
                }}),
                json!({"type":"response.completed","response":{"status":"completed"}}),
            ]),
        )
        .start()
        .await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(SessionConfig::new(workspace.path().to_path_buf(), "m"));
    assert_eq!(
        agent.run_turn(&mut session, "hello", &mut None).await,
        StopReason::Error
    );
    assert_eq!(session.history.len(), 1);
    let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(events.iter().any(|event| matches!(
        event,
        EventMsg::AgentMessageDelta { delta } if delta == "streamed text"
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EventMsg::AgentMessageDone { .. }))
    );
}

#[tokio::test]
async fn assistant_text_deltas_are_forwarded_as_they_arrive() {
    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[
                json!({"type":"response.output_text.delta","delta":"one"}),
                json!({"type":"response.output_text.delta","delta":"two"}),
                json!({"type":"response.completed","response":{"status":"completed"}}),
            ]),
        )
        .start()
        .await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(SessionConfig::new(workspace.path().to_path_buf(), "m"));
    assert_eq!(
        agent.run_turn(&mut session, "hello", &mut None).await,
        StopReason::EndTurn
    );
    let deltas: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            EventMsg::AgentMessageDelta { delta } => Some(delta),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["one", "two"]);
}

#[cfg(unix)]
#[tokio::test]
async fn approved_outside_write_uses_a_one_call_elevated_context() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = std::fs::canonicalize(outside.path())
        .unwrap()
        .join("approved.txt");
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            tool_call_then_done(
                "write_file",
                json!({"path":target.to_string_lossy(),"content":"approved"}),
            ),
        )
        .route("/v1/responses", final_text("done"))
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(
        SessionConfig::new(workspace.path().to_path_buf(), "grok-build-0.1").with_policy(
            grokforge_protocol::ApprovalPolicy::OnRequest,
            SandboxMode::WorkspaceWrite,
        ),
    );
    agent
        .run_turn(&mut session, "write outside", &mut None)
        .await;
    assert_eq!(std::fs::read_to_string(target).unwrap(), "approved");
}

#[tokio::test]
async fn plan_mode_rejects_a_hallucinated_unadvertised_mutating_tool() {
    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            tool_call_then_done(
                "write_file",
                json!({"path":"plan-hole.txt","content":"bad"}),
            ),
        )
        .route("/v1/responses", final_text("plan"))
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(
        SessionConfig::new(workspace.path().to_path_buf(), "grok-build-0.1").with_policy(
            grokforge_protocol::ApprovalPolicy::Never,
            SandboxMode::DangerFullAccess,
        ),
    );
    agent.run_plan_turn(&mut session, "plan", &mut None).await;
    assert!(!workspace.path().join("plan-hole.txt").exists());
    assert!(session.history.iter().any(|item| matches!(
        item,
        grokforge_protocol::ResponseItem::ToolResult { content, .. }
            if content.contains("not available")
    )));
}

#[cfg(unix)]
#[tokio::test]
async fn auto_commit_does_not_sweep_shell_created_files() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(workspace.path())
            .output()
            .unwrap()
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.test"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(workspace.path().join("README"), "initial\n").unwrap();
    git(&["add", "README"]);
    git(&["commit", "-qm", "initial"]);

    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            tool_call_then_done(
                "shell",
                json!({"command":"printf 'from shell\\n' > shell-created.txt"}),
            ),
        )
        .route("/v1/responses", final_text("done"))
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let client = XaiClient::new(&mock.base_url(), "k").unwrap();
    let agent = Agent::new(
        client,
        ToolRegistry::with_builtins(),
        Arc::new(EnforcedTestRunner),
        Arc::new(AutoApprover::yolo()),
        tx,
    );
    let mut session = Session::new(
        SessionConfig::new(workspace.path().to_path_buf(), "grok-build-0.1").with_policy(
            grokforge_protocol::ApprovalPolicy::Never,
            SandboxMode::DangerFullAccess,
        ),
    );
    agent.run_turn(&mut session, "create it", &mut None).await;
    let shown = git(&["show", "HEAD:shell-created.txt"]);
    assert!(!shown.status.success());
    assert_eq!(
        String::from_utf8(git(&["status", "--porcelain"]).stdout)
            .unwrap()
            .trim(),
        "?? shell-created.txt"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn foreground_file_tool_changes_remain_uncommitted_without_isolation() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(workspace.path())
            .output()
            .unwrap()
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.test"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(workspace.path().join("README"), "initial\n").unwrap();
    git(&["add", "README"]);
    git(&["commit", "-qm", "initial"]);

    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            tool_call_then_done(
                "write_file",
                json!({"path":"direct.txt","content":"direct\n"}),
            ),
        )
        .route("/v1/responses", final_text("done"))
        .start()
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let agent = agent_for(&mock, tx);
    let mut session = Session::new(
        SessionConfig::new(workspace.path().to_path_buf(), "grok-build-0.1").with_policy(
            grokforge_protocol::ApprovalPolicy::Never,
            SandboxMode::DangerFullAccess,
        ),
    );
    agent.run_turn(&mut session, "create it", &mut None).await;
    let shown = git(&["show", "HEAD:direct.txt"]);
    assert!(!shown.status.success());
    assert_eq!(
        String::from_utf8(git(&["status", "--porcelain"]).stdout)
            .unwrap()
            .trim(),
        "?? direct.txt"
    );
}

#[derive(Debug, Default)]
struct NetworkProbeRunner {
    policies: std::sync::Mutex<Vec<grokforge_protocol::SandboxPolicy>>,
}

#[async_trait]
impl SandboxRunner for NetworkProbeRunner {
    fn capability(&self) -> SandboxCapability {
        SandboxCapability {
            backend: "probe".into(),
            enforced: true,
            notes: vec![],
        }
    }

    async fn run(
        &self,
        policy: &grokforge_protocol::SandboxPolicy,
        _command: &CommandSpec,
    ) -> Result<ExecOutput, ExecError> {
        self.policies.lock().unwrap().push(policy.clone());
        let denied = policy.network != grokforge_protocol::NetworkMode::Full;
        Ok(ExecOutput {
            exit_code: Some(i32::from(denied)),
            stdout: if denied { String::new() } else { "ok".into() },
            stderr: if denied {
                "network denied".into()
            } else {
                String::new()
            },
            truncated: false,
            timed_out: false,
            denial: denied.then_some(grokforge_protocol::DenialClass::Network),
        })
    }
}

#[tokio::test]
async fn network_escalation_does_not_widen_filesystem_access() {
    let workspace = tempfile::tempdir().unwrap();
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            tool_call_then_done("shell", json!({"command":"fetch example.test"})),
        )
        .route("/v1/responses", final_text("done"))
        .start()
        .await;
    let runner = Arc::new(NetworkProbeRunner::default());
    let (tx, _rx) = mpsc::unbounded_channel();
    let client = XaiClient::new(&mock.base_url(), "k").unwrap();
    let agent = Agent::new(
        client,
        ToolRegistry::with_builtins(),
        runner.clone(),
        Arc::new(AutoApprover::yolo()),
        tx,
    );
    let mut session = Session::new(
        SessionConfig::new(workspace.path().to_path_buf(), "grok-build-0.1").with_policy(
            grokforge_protocol::ApprovalPolicy::OnFailure,
            SandboxMode::WorkspaceWrite,
        ),
    );
    agent.run_turn(&mut session, "fetch", &mut None).await;
    let policies = runner.policies.lock().unwrap();
    assert_eq!(policies.len(), 2);
    assert_eq!(policies[1].network, grokforge_protocol::NetworkMode::Full);
    assert_eq!(policies[1].mode, SandboxMode::WorkspaceWrite);
    assert_eq!(policies[1].writable_roots, policies[0].writable_roots);
    assert_eq!(policies[1].protected_paths, policies[0].protected_paths);
    assert_eq!(policies[1].unreadable_globs, policies[0].unreadable_globs);
}
