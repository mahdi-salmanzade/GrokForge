//! Integration tests for the xAI client against the byte-controllable mock server.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use futures::StreamExt;
use grokforge_test_support::{MockXai, Reply};
use grokforge_xai::{
    Effort, InputItem, ResponsesRequest, RetryConfig, Role, StopReason, StreamEvent, XaiClient,
    XaiError,
};
use serde_json::json;

fn text_delta(s: &str) -> serde_json::Value {
    json!({ "type": "response.output_text.delta", "delta": s })
}

fn completed_with_usage() -> serde_json::Value {
    json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "input_tokens_details": { "cached_tokens": 61 },
                "output_tokens_details": { "reasoning_tokens": 8 }
            }
        }
    })
}

async fn collect(stream: grokforge_xai::ResponseStream) -> Vec<StreamEvent> {
    stream
        .filter_map(|r| async move { r.ok() })
        .collect::<Vec<_>>()
        .await
}

fn fast_client(mock: &MockXai) -> XaiClient {
    XaiClient::new(&mock.base_url(), "test-key")
        .expect("client")
        .with_retry(RetryConfig {
            max_attempts: 4,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        })
}

fn simple_req() -> ResponsesRequest {
    ResponsesRequest::new("grok-build-0.1", vec![InputItem::text(Role::User, "hi")])
}

#[tokio::test]
async fn happy_path_streams_text_usage_and_completion() {
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[
                json!({"type":"response.created","response":{"id":"resp_1"}}),
                text_delta("Hel"),
                text_delta("lo"),
                completed_with_usage(),
            ]),
        )
        .start()
        .await;

    let client = fast_client(&mock);
    let events = collect(client.stream(&simple_req()).await.expect("stream")).await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello");
    assert!(events.iter().any(|e| matches!(e, StreamEvent::Usage(_))));
    assert!(matches!(
        events.last(),
        Some(StreamEvent::Completed {
            stop: StopReason::EndTurn
        })
    ));
}

#[tokio::test]
async fn reassembles_events_split_across_tcp_chunks() {
    // The same SSE body, but re-split at awkward byte offsets that fall in the middle of
    // frames — the client must still produce whole events.
    let events = [
        text_delta("alpha"),
        text_delta("beta"),
        completed_with_usage(),
    ];
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events_split_at(&events, &[3, 10, 12, 25, 40, 55]),
        )
        .start()
        .await;

    let client = fast_client(&mock);
    let got = collect(client.stream(&simple_req()).await.expect("stream")).await;
    let text: String = got
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "alphabeta");
}

#[tokio::test]
async fn ignores_keepalive_comments_and_unknown_events() {
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_frames(&[
                ": keepalive ping\n\n",
                "data: {\"type\":\"response.future_thing\"}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
            ]),
        )
        .start()
        .await;

    let client = fast_client(&mock);
    let got = collect(client.stream(&simple_req()).await.expect("stream")).await;
    let text: String = got
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "ok");
}

#[tokio::test]
async fn malformed_json_frame_is_reported_as_retriable_stream_error() {
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_frames(&["data: {not-json}\n\n"]),
        )
        .start()
        .await;

    let client = fast_client(&mock);
    let mut stream = client.stream(&simple_req()).await.expect("stream");
    let error = stream
        .next()
        .await
        .expect("error event")
        .expect_err("malformed JSON must fail the stream");
    assert!(matches!(error, XaiError::Stream(_)));
    assert!(error.is_retriable());
}

#[tokio::test]
async fn eof_before_completed_is_reported_as_stream_error() {
    let mock = MockXai::builder()
        .route("/v1/responses", Reply::sse_events(&[text_delta("partial")]))
        .start()
        .await;

    let client = fast_client(&mock);
    let mut stream = client.stream(&simple_req()).await.expect("stream");
    assert!(matches!(
        stream.next().await,
        Some(Ok(StreamEvent::TextDelta(text))) if text == "partial"
    ));
    assert!(matches!(
        stream.next().await,
        Some(Err(XaiError::Stream(_)))
    ));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn done_sentinel_before_completed_is_reported_as_stream_error() {
    let mock = MockXai::builder()
        .route("/v1/responses", Reply::sse_frames(&["data: [DONE]\n\n"]))
        .start()
        .await;

    let client = fast_client(&mock);
    let mut stream = client.stream(&simple_req()).await.expect("stream");
    assert!(matches!(
        stream.next().await,
        Some(Err(XaiError::Stream(_)))
    ));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn incomplete_event_finishes_with_max_tokens_reason() {
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[json!({
                "type": "response.incomplete",
                "response": {
                    "status": "incomplete",
                    "incomplete_details": {"reason": "max_output_tokens"}
                }
            })]),
        )
        .start()
        .await;

    let client = fast_client(&mock);
    let got = collect(client.stream(&simple_req()).await.expect("stream")).await;
    assert_eq!(
        got,
        vec![StreamEvent::Completed {
            stop: StopReason::MaxTokens
        }]
    );
}

#[tokio::test]
async fn failed_event_surfaces_nested_api_message() {
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[json!({
                "type": "response.failed",
                "response": {"error": {"message": "provider exploded"}}
            })]),
        )
        .start()
        .await;

    let client = fast_client(&mock);
    let mut stream = client.stream(&simple_req()).await.expect("stream");
    assert!(matches!(
        stream.next().await,
        Some(Err(XaiError::ApiStreamError(message))) if message == "provider exploded"
    ));
}

#[tokio::test]
async fn whole_chunk_tool_call_is_surfaced() {
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "type": "function_call",
                        "id": "fc_1",
                        "call_id": "call_42",
                        "name": "read_file",
                        "arguments": "{\"path\":\"src/main.rs\"}"
                    }
                }),
                json!({"type":"response.completed","response":{"status":"requires_action"}}),
            ]),
        )
        .start()
        .await;

    let client = fast_client(&mock);
    let got = collect(client.stream(&simple_req()).await.expect("stream")).await;
    let call = got
        .iter()
        .find_map(|e| match e {
            StreamEvent::ToolCall(c) => Some(c),
            _ => None,
        })
        .expect("tool call present");
    assert_eq!(call.call_id, "call_42");
    assert_eq!(call.output_index, 0);
    assert_eq!(call.name, "read_file");
    assert_eq!(call.arguments, r#"{"path":"src/main.rs"}"#);
    assert!(matches!(
        got.last(),
        Some(StreamEvent::Completed {
            stop: StopReason::ToolCalls
        })
    ));
}

#[tokio::test]
async fn tool_call_arguments_reassembled_from_deltas() {
    // The partial-JSON hedge: arguments arrive across delta frames, then a done frame with
    // empty arguments. The client must reconstruct the full argument string.
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_9","delta":"{\"q\":"}),
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_9","delta":"\"hi\"}"}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_9","call_id":"c9","name":"search","arguments":""}}),
                json!({"type":"response.completed","response":{"status":"requires_action"}}),
            ]),
        )
        .start()
        .await;

    let client = fast_client(&mock);
    let got = collect(client.stream(&simple_req()).await.expect("stream")).await;
    let call = got
        .iter()
        .find_map(|e| match e {
            StreamEvent::ToolCall(c) => Some(c),
            _ => None,
        })
        .expect("tool call present");
    assert_eq!(call.arguments, r#"{"q":"hi"}"#);
}

#[tokio::test]
async fn encrypted_reasoning_output_is_preserved_for_stateless_replay() {
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "type": "reasoning",
                        "id": "rs_provider_1",
                        "status": "completed",
                        "summary": [{"type":"summary_text","text":"brief"}],
                        "encrypted_content": "opaque+ciphertext=="
                    }
                }),
                completed_with_usage(),
            ]),
        )
        .start()
        .await;

    let client = fast_client(&mock);
    let got = collect(client.stream(&simple_req()).await.expect("stream")).await;
    let reasoning = got
        .iter()
        .find_map(|event| match event {
            StreamEvent::EncryptedReasoning(reasoning) => Some(reasoning),
            _ => None,
        })
        .expect("encrypted reasoning event");
    assert_eq!(reasoning.id, "rs_provider_1");
    assert_eq!(reasoning.output_index, 0);
    assert_eq!(reasoning.status, "completed");
    assert_eq!(reasoning.summary[0]["text"], "brief");
    assert_eq!(reasoning.encrypted_content, "opaque+ciphertext==");
}

#[tokio::test]
async fn usage_reports_cached_and_reasoning_tokens() {
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[completed_with_usage()]),
        )
        .start()
        .await;

    let client = fast_client(&mock);
    let got = collect(client.stream(&simple_req()).await.expect("stream")).await;
    let usage = got
        .iter()
        .find_map(|e| match e {
            StreamEvent::Usage(u) => Some(*u),
            _ => None,
        })
        .expect("usage present");
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.cached_tokens, 61);
    assert_eq!(usage.reasoning_tokens, 8);
}

#[tokio::test]
async fn retries_429_then_succeeds() {
    let mock = MockXai::builder()
        // First call: 429 with Retry-After: 0. Second call: success. (Queue drains in order.)
        .route(
            "/v1/responses",
            Reply::status_with_retry_after(429, Some(0)),
        )
        .route(
            "/v1/responses",
            Reply::sse_events(&[text_delta("recovered"), completed_with_usage()]),
        )
        .start()
        .await;

    let client = fast_client(&mock);
    let mut attempts = Vec::new();
    let stream = client
        .stream_with_attempt_observer(&simple_req(), |attempt| attempts.push(attempt))
        .await
        .expect("stream after retry");
    assert_eq!(stream.request_attempts(), 2);
    let got = collect(stream).await;
    let text: String = got
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "recovered");
    let received = mock.received();
    assert_eq!(received.len(), 2, "should have retried exactly once");
    assert_eq!(attempts.len(), 2, "every send must be observable");
    for (attempt, request) in attempts.iter().zip(&received) {
        assert_eq!(attempt.request_bytes, request.body_len());
    }
}

#[tokio::test]
async fn auth_error_is_not_retried() {
    let mock = MockXai::builder()
        .route("/v1/responses", Reply::status_with_retry_after(401, None))
        .start()
        .await;
    let client = fast_client(&mock);
    let err = client.stream(&simple_req()).await.expect_err("should fail");
    assert!(matches!(err, XaiError::Auth { status: 401, .. }));
    assert_eq!(mock.received().len(), 1, "auth failures must not retry");
}

#[tokio::test]
async fn every_failed_retry_attempt_is_observable_for_egress_accounting() {
    let mock = MockXai::builder()
        .route("/v1/responses", Reply::status_with_retry_after(503, None))
        .start()
        .await;
    let client = fast_client(&mock);
    let mut attempts = Vec::new();
    let error = client
        .stream_with_attempt_observer(&simple_req(), |attempt| attempts.push(attempt))
        .await
        .expect_err("repeated 503 must eventually fail");
    assert!(matches!(error, XaiError::Server { status: 503, .. }));

    let received = mock.received();
    assert_eq!(received.len(), 4);
    assert_eq!(attempts.len(), received.len());
    for (attempt, request) in attempts.iter().zip(&received) {
        assert_eq!(attempt.request_bytes, request.body_len());
    }
}

#[tokio::test]
async fn redirects_cannot_replay_context_to_another_origin() {
    let sink = MockXai::builder()
        .route("/capture", Reply::sse_events(&[completed_with_usage()]))
        .start()
        .await;
    let source = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::Http {
                status: 307,
                headers: vec![("location".into(), format!("{}/capture", sink.base_url()))],
                body: Vec::new(),
            },
        )
        .start()
        .await;

    let client = fast_client(&source);
    let error = client
        .stream(&simple_req())
        .await
        .expect_err("redirect must be surfaced, not followed");
    assert!(matches!(error, XaiError::Api { status: 307, .. }));
    assert_eq!(source.received().len(), 1);
    assert!(
        sink.received().is_empty(),
        "request body must not reach redirect target"
    );
}

#[tokio::test]
async fn request_bytes_reconcile_with_received_body() {
    // The ledger invariant in miniature: the size the client reports for the request equals
    // the number of bytes the server actually received.
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::sse_events(&[completed_with_usage()]),
        )
        .start()
        .await;

    let client = fast_client(&mock);
    let req = ResponsesRequest::new(
        "grok-build-0.1",
        vec![InputItem::text(Role::User, "reconcile me")],
    )
    .with_reasoning(Effort::Low);
    let stream = client.stream(&req).await.expect("stream");
    let reported = stream.request_bytes();
    let _ = collect(stream).await;

    let received = mock.last_request().expect("a request");
    assert_eq!(reported, received.body_len());
    // And the received body is exactly our serialized request.
    let (serialized, _) = XaiClient::serialize_request(&req).unwrap();
    assert_eq!(received.body, serialized);
}

#[tokio::test]
async fn list_and_validate_models() {
    let mock = MockXai::builder()
        .route(
            "/v1/models",
            Reply::json(
                200,
                &json!({"object":"list","data":[
                    {"id":"grok-build-0.1","owned_by":"xai","aliases":["grok-build-latest"],"context_length":256_000},
                    {"id":"grok-4.5","owned_by":"xai"}
                ]}),
            ),
        )
        .start()
        .await;
    let client = fast_client(&mock);

    let models = client.list_models().await.expect("models");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].context_window, Some(256_000));

    client
        .validate_model("grok-build-0.1")
        .await
        .expect("known model ok");

    client
        .validate_model("grok-build-latest")
        .await
        .expect("advertised alias is valid");

    let err = client
        .validate_model("grok-code-fast-1")
        .await
        .expect_err("retired slug should error");
    match err {
        XaiError::UnknownModel { model, available } => {
            assert_eq!(model, "grok-code-fast-1");
            assert_eq!(available.len(), 2);
        }
        other => panic!("expected UnknownModel, got {other:?}"),
    }
}

#[tokio::test]
async fn oversized_models_response_is_rejected_before_json_decode() {
    let mock = MockXai::builder()
        .route(
            "/v1/models",
            Reply::Http {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body: vec![b' '; 2 * 1024 * 1024 + 1],
            },
        )
        .start()
        .await;
    let client = fast_client(&mock);
    let error = client
        .list_models()
        .await
        .expect_err("oversized model list must fail");
    assert!(matches!(error, XaiError::Stream(message) if message.contains("models response body")));
}

#[tokio::test]
async fn oversized_error_body_is_reduced_to_a_bounded_message() {
    let mock = MockXai::builder()
        .route(
            "/v1/responses",
            Reply::Http {
                status: 400,
                headers: vec![("content-type".into(), "text/plain".into())],
                body: vec![b'x'; 256 * 1024],
            },
        )
        .start()
        .await;
    let client = fast_client(&mock);
    let error = client
        .stream(&simple_req())
        .await
        .expect_err("400 must fail");
    let XaiError::Api { message, .. } = error else {
        panic!("expected API error");
    };
    assert_eq!(message.len(), 200);
}
