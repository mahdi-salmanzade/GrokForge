//! End-to-end agent-loop tests against the mock xAI server. These exercise the M2 exit
//! criteria: a tool-using turn that creates a file, secrets never leaving the machine, and
//! ledger byte-reconciliation.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value
)]

use std::sync::Arc;

use grokforge_core::{Agent, AutoApprover, Session, SessionConfig, ToolRegistry};
use grokforge_protocol::{EventMsg, SandboxMode, StopReason};
use grokforge_sandbox::PassthroughRunner;
use grokforge_test_support::{MockXai, Reply};
use grokforge_xai::{RetryConfig, XaiClient};
use serde_json::json;
use tokio::sync::mpsc;

/// Build an SSE response that requests a single tool call, then finishes.
fn tool_call_then_done(name: &str, args: serde_json::Value) -> Reply {
    Reply::sse_events(&[
        json!({
            "type": "response.output_item.done",
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
