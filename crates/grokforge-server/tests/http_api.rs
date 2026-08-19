#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use grokforge_core::{SessionMeta, TurnCancellation};
use grokforge_protocol::{EventMsg, SessionId, StopReason, TurnId};
use grokforge_server::{
    ApiLimits, ProductionConfig, ProductionRunner, PromptRequest, PromptRunner, RunError,
    ServerAuth, StartedPrompt, bind, router, serve_until,
};
use grokforge_test_support::{MockXai, Reply};
use grokforge_xai::XaiClient;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

const TOKEN: &str = "63hNf7kPq4Ws8Ty2Za5Vc9Bm1Dx6Lu0R";

async fn launch(
    runner: Arc<dyn PromptRunner>,
    sessions_dir: std::path::PathBuf,
    limits: ApiLimits,
) -> (String, oneshot::Sender<()>) {
    let auth = ServerAuth::new(TOKEN).expect("auth");
    let app = router(runner, auth, sessions_dir, limits).expect("router");
    let listener = bind("127.0.0.1:0".parse().expect("address"))
        .await
        .expect("listener");
    let address = listener.local_addr().expect("local address");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        let _ = serve_until(listener, app, async move {
            let _ = shutdown_rx.await;
        })
        .await;
    });
    (format!("http://{address}"), shutdown_tx)
}

fn authenticated(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
) -> reqwest::RequestBuilder {
    client.request(method, url).bearer_auth(TOKEN)
}

#[tokio::test]
async fn real_agent_prompt_streams_protocol_events_and_persists_metadata() {
    let workspace = tempfile::tempdir().expect("workspace");
    let storage = tempfile::tempdir().expect("storage");
    let sessions = storage.path().join("sessions");
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[
                json!({"type":"response.output_text.delta","delta":"hello from Grok"}),
                json!({
                    "type":"response.completed",
                    "response": {
                        "status":"completed",
                        "usage":{"input_tokens":4,"output_tokens":3}
                    }
                }),
            ]),
        )
        .start()
        .await;
    let client = XaiClient::new(&mock.base_url(), "test-key").expect("xAI client");
    let config = ProductionConfig::new(
        std::fs::canonicalize(workspace.path()).expect("canonical workspace"),
        sessions.clone(),
        client,
        "grok-build-0.1".to_string(),
    );
    let runner = Arc::new(ProductionRunner::new(config).expect("runner"));
    let (base, shutdown) = launch(runner, sessions.clone(), ApiLimits::default()).await;
    let http = reqwest::Client::new();

    let unauthenticated = http
        .get(format!("{base}/v1/sessions"))
        .send()
        .await
        .expect("unauthenticated request");
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(
        unauthenticated
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer realm=\"grokforge\"")
    );

    let response = authenticated(&http, reqwest::Method::POST, format!("{base}/v1/prompts"))
        .json(&json!({"prompt":"Say hello without using tools"}))
        .send()
        .await
        .expect("prompt response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );
    let session_id = response
        .headers()
        .get("x-grokforge-session-id")
        .and_then(|value| value.to_str().ok())
        .expect("session header")
        .to_string();
    let stream = response.text().await.expect("SSE body");
    assert!(stream.contains("agent_message_delta"));
    assert!(stream.contains("hello from Grok"));
    assert!(stream.contains("turn_complete"));

    let detail = authenticated(
        &http,
        reqwest::Method::GET,
        format!("{base}/v1/sessions/{session_id}"),
    )
    .send()
    .await
    .expect("session detail");
    assert_eq!(detail.status(), reqwest::StatusCode::OK);
    let detail: serde_json::Value = detail.json().await.expect("detail JSON");
    assert_eq!(detail["session_id"], session_id);
    assert_eq!(detail["model"], "grok-build-0.1");
    assert_eq!(detail["first_prompt"], "Say hello without using tools");

    let recorded = mock.received();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].path, "/v1/responses");
    let _ = shutdown.send(());
}

#[tokio::test]
async fn plan_prompt_uses_the_validated_plan_model_and_high_effort() {
    let workspace = tempfile::tempdir().expect("workspace");
    let storage = tempfile::tempdir().expect("storage");
    let sessions = storage.path().join("sessions");
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[
                json!({"type":"response.output_text.delta","delta":"plan"}),
                json!({
                    "type":"response.completed",
                    "response": {
                        "status":"completed",
                        "usage":{"input_tokens":2,"output_tokens":1}
                    }
                }),
            ]),
        )
        .start()
        .await;
    let client = XaiClient::new(&mock.base_url(), "test-key").expect("xAI client");
    let mut config = ProductionConfig::new(
        std::fs::canonicalize(workspace.path()).expect("canonical workspace"),
        sessions.clone(),
        client,
        "execute-model".to_string(),
    );
    config.effort = Some(grokforge_xai::Effort::Low);
    config.context_window_tokens = Some(64_000);
    config.plan_model = "plan-model".to_string();
    config.plan_context_window_tokens = Some(500_000);
    let runner = Arc::new(ProductionRunner::new(config).expect("runner"));
    let (base, shutdown) = launch(runner, sessions.clone(), ApiLimits::default()).await;
    let http = reqwest::Client::new();

    let response = authenticated(&http, reqwest::Method::POST, format!("{base}/v1/prompts"))
        .json(&json!({"prompt":"Design the change", "plan":true}))
        .send()
        .await
        .expect("plan response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let stream = response.text().await.expect("SSE body");
    assert!(stream.contains("turn_complete"));

    let request = mock.last_request().expect("model request").json();
    assert_eq!(request["model"], "plan-model");
    assert_eq!(request["reasoning"]["effort"], "high");
    let metadata = SessionMeta::list(&sessions)
        .await
        .into_iter()
        .next()
        .expect("session metadata");
    assert_eq!(metadata.model, "plan-model");
    assert_eq!(metadata.effort, Some(grokforge_core::PersistedEffort::High));

    let _ = shutdown.send(());
}

#[derive(Debug)]
struct SlowRunner;

#[async_trait]
impl PromptRunner for SlowRunner {
    async fn start(&self, _request: PromptRequest) -> Result<StartedPrompt, RunError> {
        let (sender, receiver) = mpsc::channel(1);
        let (completion_tx, completion) = oneshot::channel();
        let cancellation = TurnCancellation::new();
        tokio::spawn(async move {
            let _completion_tx = completion_tx;
            tokio::time::sleep(Duration::from_millis(400)).await;
            let _ = sender
                .send(EventMsg::TurnComplete {
                    turn_id: TurnId::new(),
                    stop: StopReason::EndTurn,
                })
                .await;
        });
        Ok(StartedPrompt {
            session_id: SessionId::new(),
            events: receiver,
            cancellation,
            event_queue_status: None,
            completion,
        })
    }
}

#[tokio::test]
async fn prompt_concurrency_and_body_size_are_bounded() {
    let storage = tempfile::tempdir().expect("storage");
    let sessions = storage.path().join("sessions");
    let limits = ApiLimits {
        max_request_body_bytes: 128,
        max_concurrent_prompts: 1,
        ..ApiLimits::default()
    };
    let (base, shutdown) = launch(Arc::new(SlowRunner), sessions, limits).await;
    let http = reqwest::Client::new();

    let first = authenticated(&http, reqwest::Method::POST, format!("{base}/v1/prompts"))
        .json(&json!({"prompt":"first"}))
        .send()
        .await
        .expect("first response");
    assert_eq!(first.status(), reqwest::StatusCode::OK);

    let second = authenticated(&http, reqwest::Method::POST, format!("{base}/v1/prompts"))
        .json(&json!({"prompt":"second"}))
        .send()
        .await
        .expect("second response");
    assert_eq!(second.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);

    drop(first);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let still_running = authenticated(&http, reqwest::Method::POST, format!("{base}/v1/prompts"))
        .json(&json!({"prompt":"third"}))
        .send()
        .await
        .expect("running-turn response");
    assert_eq!(
        still_running.status(),
        reqwest::StatusCode::TOO_MANY_REQUESTS
    );
    let oversized = authenticated(&http, reqwest::Method::POST, format!("{base}/v1/prompts"))
        .header("content-type", "application/json")
        .body(format!("{{\"prompt\":\"{}\"}}", "x".repeat(256)))
        .send()
        .await
        .expect("oversized response");
    assert_eq!(oversized.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);

    tokio::time::sleep(Duration::from_millis(420)).await;
    let after_completion =
        authenticated(&http, reqwest::Method::POST, format!("{base}/v1/prompts"))
            .json(&json!({"prompt":"after completion"}))
            .send()
            .await
            .expect("post-completion response");
    assert_eq!(after_completion.status(), reqwest::StatusCode::OK);

    let _ = shutdown.send(());
}

#[tokio::test]
async fn health_and_openapi_are_public_but_contain_no_private_session_data() {
    let storage = tempfile::tempdir().expect("storage");
    let sessions = storage.path().join("sessions");
    let id = SessionId::new();
    SessionMeta::new(
        id,
        storage.path().to_path_buf(),
        "model".to_string(),
        "private prompt",
    )
    .write(&sessions, id)
    .await
    .expect("metadata");
    let (base, shutdown) = launch(Arc::new(SlowRunner), sessions, ApiLimits::default()).await;
    let http = reqwest::Client::new();

    let health = http
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("health");
    assert_eq!(health.status(), reqwest::StatusCode::OK);
    let health_text = health.text().await.expect("health body");
    assert!(!health_text.contains("private prompt"));

    let openapi = http
        .get(format!("{base}/openapi.json"))
        .send()
        .await
        .expect("OpenAPI");
    assert_eq!(openapi.status(), reqwest::StatusCode::OK);
    let document: serde_json::Value = openapi.json().await.expect("OpenAPI JSON");
    assert_eq!(document["openapi"], "3.1.0");
    assert!(document["paths"]["/v1/prompts"].is_object());

    let _ = shutdown.send(());
}
