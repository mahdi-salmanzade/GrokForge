//! Typed streaming events and the logic that maps raw SSE frames onto them.
//!
//! The public [`StreamEvent`] is what [`crate::stream::ResponseStream`] yields. Frame
//! parsing is deliberately tolerant: any event `type` we don't model is ignored rather
//! than fatal, so xAI adding event kinds never breaks the client (API-churn hedge).

use serde::Deserialize;

/// A high-level event from a streamed response.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// The response was created; carries the server response id when present.
    Created { response_id: Option<String> },
    /// Incremental assistant text.
    TextDelta(String),
    /// Incremental reasoning-summary text (drives the collapsible thinking pane).
    ReasoningDelta(String),
    /// A complete tool call. For xAI these arrive whole; the client also reassembles
    /// argument deltas defensively (partial-JSON hedge).
    ToolCall(ToolCall),
    /// Provider-encrypted reasoning output that must be replayed in stateless continuations.
    EncryptedReasoning(EncryptedReasoning),
    /// A complete provider output item preserved without schema projection for stateless replay.
    ProviderOutput {
        /// Canonical position of this item in the provider response's `output` array.
        output_index: usize,
        item: serde_json::Value,
    },
    /// Token accounting for the response.
    Usage(Usage),
    /// The response finished.
    Completed { stop: StopReason },
}

/// A fully-formed tool call requested by the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// Canonical position of this call in the provider response's `output` array.
    pub output_index: usize,
    pub call_id: String,
    pub name: String,
    /// Raw JSON arguments as a string (validated/parsed by the tool layer).
    pub arguments: String,
}

/// A reasoning output item whose encrypted content can be sent back without exposing the model's
/// private chain of thought.
#[derive(Debug, Clone, PartialEq)]
pub struct EncryptedReasoning {
    /// Canonical position of this item in the provider response's `output` array.
    pub output_index: usize,
    pub id: String,
    pub status: String,
    pub summary: Vec<serde_json::Value>,
    pub encrypted_content: String,
}

/// Token usage, including the cache-hit and reasoning breakdowns GrokForge surfaces in the
/// cost display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

/// Why the response ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolCalls,
    MaxTokens,
    Other(String),
}

/// Outcome of parsing a single SSE `data:` frame. Some frames (`response.completed`) yield
/// more than one [`StreamEvent`]; the stream layer expands them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Frame {
    /// The `[DONE]` sentinel — end of stream.
    Done,
    /// Nothing the client models; skip it.
    Ignore,
    Created {
        response_id: Option<String>,
    },
    TextDelta(String),
    ReasoningDelta(String),
    /// A streamed slice of a tool call's arguments (accumulated by the stream layer).
    ToolArgsDelta {
        item_id: String,
        delta: String,
    },
    /// A finished output item; a `function_call` item completes a tool call.
    ToolItemDone {
        output_index: usize,
        item: serde_json::Value,
        item_id: String,
        call_id: String,
        name: String,
        arguments: Option<String>,
    },
    EncryptedReasoningDone {
        output_index: usize,
        item: serde_json::Value,
        reasoning: EncryptedReasoning,
    },
    ProviderOutputDone {
        output_index: usize,
        item: serde_json::Value,
    },
    Completed {
        usage: Option<Usage>,
        stop: StopReason,
    },
    /// The API signalled an error inside the stream.
    Error(String),
    /// The server sent an SSE data frame that was not a valid event payload.
    Malformed(String),
}

pub(crate) fn parse_frame(data: &str) -> Frame {
    let data = data.trim();
    if data.is_empty() {
        return Frame::Ignore;
    }
    if data == "[DONE]" {
        return Frame::Done;
    }
    let value: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(error) => return Frame::Malformed(format!("invalid JSON: {error}")),
    };
    let Some(kind) = value.get("type").and_then(|t| t.as_str()) else {
        return Frame::Malformed("missing string `type` field".to_string());
    };

    match kind {
        "response.created" => Frame::Created {
            response_id: value
                .get("response")
                .and_then(|r| r.get("id"))
                .and_then(|i| i.as_str())
                .map(String::from),
        },
        "response.output_text.delta" => text_delta(&value).map_or_else(
            || Frame::Malformed("text delta is missing `delta`".to_string()),
            Frame::TextDelta,
        ),
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            text_delta(&value).map_or_else(
                || Frame::Malformed("reasoning delta is missing `delta`".to_string()),
                Frame::ReasoningDelta,
            )
        }
        "response.function_call_arguments.delta" => {
            let Some(item_id) = nonempty_str_field(&value, "item_id") else {
                return Frame::Malformed(
                    "function-call argument delta is missing `item_id`".to_string(),
                );
            };
            let Some(delta) = str_field(&value, "delta") else {
                return Frame::Malformed(
                    "function-call argument delta is missing `delta`".to_string(),
                );
            };
            if delta.is_empty() {
                Frame::Ignore
            } else {
                Frame::ToolArgsDelta { item_id, delta }
            }
        }
        "response.output_item.done" => parse_output_item_done(&value),
        "response.completed" => {
            let Some(response) = value.get("response") else {
                return Frame::Malformed("completed event is missing `response`".to_string());
            };
            let usage = match optional_usage(response) {
                Ok(usage) => usage,
                Err(message) => return Frame::Malformed(message),
            };
            let Some(status) = response.get("status").and_then(|s| s.as_str()) else {
                return Frame::Malformed(
                    "completed event is missing response `status`".to_string(),
                );
            };
            let stop = map_stop(status);
            Frame::Completed { usage, stop }
        }
        "response.incomplete" => {
            let Some(response) = value.get("response") else {
                return Frame::Malformed("incomplete event is missing `response`".to_string());
            };
            let usage = match optional_usage(response) {
                Ok(usage) => usage,
                Err(message) => return Frame::Malformed(message),
            };
            let stop = response
                .get("incomplete_details")
                .and_then(|details| details.get("reason"))
                .and_then(|reason| reason.as_str())
                .map_or(StopReason::MaxTokens, map_stop);
            Frame::Completed { usage, stop }
        }
        "error" | "response.failed" => {
            let msg = value
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .or_else(|| {
                    value
                        .get("response")
                        .and_then(|response| response.get("error"))
                        .and_then(|error| error.get("message"))
                        .and_then(|message| message.as_str())
                })
                .or_else(|| value.get("message").and_then(|m| m.as_str()))
                .unwrap_or("unknown stream error")
                .to_string();
            Frame::Error(msg)
        }
        _ => Frame::Ignore,
    }
}

fn text_delta(value: &serde_json::Value) -> Option<String> {
    value
        .get("delta")
        .and_then(|d| d.as_str())
        .map(String::from)
}

fn str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key).and_then(|v| v.as_str()).map(String::from)
}

fn nonempty_str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    str_field(value, key).filter(|value| !value.is_empty())
}

fn parse_output_item_done(value: &serde_json::Value) -> Frame {
    let Some(output_index) = value
        .get("output_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
    else {
        return Frame::Malformed(
            "output-item event is missing non-negative integer `output_index`".to_string(),
        );
    };
    let Some(item) = value.get("item") else {
        return Frame::Malformed("output-item event is missing `item`".to_string());
    };
    let Some(item_type) = item.get("type").and_then(|kind| kind.as_str()) else {
        return Frame::Malformed("output item is missing `type`".to_string());
    };
    if item_type == "reasoning" {
        return parse_encrypted_reasoning(item, output_index);
    }
    if item_type != "function_call" {
        return Frame::ProviderOutputDone {
            output_index,
            item: item.clone(),
        };
    }
    let Some(item_id) = nonempty_str_field(item, "id") else {
        return Frame::Malformed("function-call item is missing `id`".to_string());
    };
    let Some(call_id) = nonempty_str_field(item, "call_id") else {
        return Frame::Malformed("function-call item is missing `call_id`".to_string());
    };
    let Some(name) = nonempty_str_field(item, "name") else {
        return Frame::Malformed("function-call item is missing `name`".to_string());
    };
    let arguments = item
        .get("arguments")
        .and_then(|a| a.as_str())
        .map(String::from);
    Frame::ToolItemDone {
        output_index,
        item: item.clone(),
        item_id,
        call_id,
        name,
        arguments,
    }
}

fn parse_encrypted_reasoning(item: &serde_json::Value, output_index: usize) -> Frame {
    let Some(id) = nonempty_str_field(item, "id") else {
        return Frame::Malformed("reasoning item is missing `id`".to_string());
    };
    let Some(status) = nonempty_str_field(item, "status") else {
        return Frame::Malformed("reasoning item is missing `status`".to_string());
    };
    let Some(encrypted_content) = nonempty_str_field(item, "encrypted_content") else {
        return Frame::Malformed("reasoning item is missing `encrypted_content`".to_string());
    };
    let Some(summary) = item.get("summary").and_then(|summary| summary.as_array()) else {
        return Frame::Malformed("reasoning item is missing array `summary`".to_string());
    };
    Frame::EncryptedReasoningDone {
        output_index,
        item: item.clone(),
        reasoning: EncryptedReasoning {
            output_index,
            id,
            status,
            summary: summary.clone(),
            encrypted_content,
        },
    }
}

fn map_stop(status: &str) -> StopReason {
    match status {
        "completed" => StopReason::EndTurn,
        "requires_action" | "tool_calls" => StopReason::ToolCalls,
        "incomplete" | "max_output_tokens" => StopReason::MaxTokens,
        other => StopReason::Other(other.to_string()),
    }
}

fn parse_usage(v: &serde_json::Value) -> Option<Usage> {
    #[derive(Deserialize)]
    struct Details {
        #[serde(default)]
        cached_tokens: u64,
        #[serde(default)]
        reasoning_tokens: u64,
    }
    #[derive(Deserialize)]
    struct Wire {
        #[serde(default)]
        input_tokens: u64,
        #[serde(default)]
        output_tokens: u64,
        #[serde(default)]
        input_tokens_details: Option<Details>,
        #[serde(default)]
        output_tokens_details: Option<Details>,
    }
    let wire: Wire = serde_json::from_value(v.clone()).ok()?;
    Some(Usage {
        input_tokens: wire.input_tokens,
        output_tokens: wire.output_tokens,
        cached_tokens: wire.input_tokens_details.map_or(0, |d| d.cached_tokens),
        reasoning_tokens: wire.output_tokens_details.map_or(0, |d| d.reasoning_tokens),
    })
}

fn optional_usage(response: &serde_json::Value) -> Result<Option<Usage>, String> {
    match response.get("usage") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => parse_usage(value)
            .map(Some)
            .ok_or_else(|| "response contains malformed `usage`".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_delta() {
        let f = parse_frame(r#"{"type":"response.output_text.delta","delta":"hello"}"#);
        assert_eq!(f, Frame::TextDelta("hello".into()));
    }

    #[test]
    fn parses_reasoning_delta() {
        let f = parse_frame(r#"{"type":"response.reasoning_summary_text.delta","delta":"think"}"#);
        assert_eq!(f, Frame::ReasoningDelta("think".into()));
    }

    #[test]
    fn done_sentinel() {
        assert_eq!(parse_frame("[DONE]"), Frame::Done);
    }

    #[test]
    fn unknown_event_is_ignored_not_fatal() {
        assert_eq!(
            parse_frame(r#"{"type":"response.some_future_event"}"#),
            Frame::Ignore
        );
    }

    #[test]
    fn malformed_json_is_not_silently_ignored() {
        assert!(matches!(parse_frame("not json"), Frame::Malformed(_)));
    }

    #[test]
    fn function_call_item_done_extracts_call() {
        let frame = parse_frame(
            r#"{"type":"response.output_item.done","output_index":3,"item":{"type":"function_call","id":"fc_1","call_id":"call_9","name":"read_file","arguments":"{\"path\":\"a.rs\"}"}}"#,
        );
        match frame {
            Frame::ToolItemDone {
                output_index,
                call_id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(output_index, 3);
                assert_eq!(call_id, "call_9");
                assert_eq!(name, "read_file");
                assert_eq!(arguments.as_deref(), Some(r#"{"path":"a.rs"}"#));
            }
            other => panic!("expected ToolItemDone, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_item_done_preserves_encrypted_state() {
        let frame = parse_frame(
            r#"{"type":"response.output_item.done","output_index":4,"item":{"type":"reasoning","id":"rs_1","status":"completed","summary":[],"encrypted_content":"opaque=="}}"#,
        );
        match frame {
            Frame::EncryptedReasoningDone {
                output_index,
                item,
                reasoning,
            } => {
                assert_eq!(output_index, 4);
                assert_eq!(item["id"], "rs_1");
                assert_eq!(
                    reasoning,
                    EncryptedReasoning {
                        output_index: 4,
                        id: "rs_1".into(),
                        status: "completed".into(),
                        summary: vec![],
                        encrypted_content: "opaque==".into(),
                    }
                );
            }
            other => panic!("expected EncryptedReasoningDone, got {other:?}"),
        }
    }

    #[test]
    fn output_item_requires_a_non_negative_integer_index() {
        for event in [
            r#"{"type":"response.output_item.done","item":{"type":"message"}}"#,
            r#"{"type":"response.output_item.done","output_index":-1,"item":{"type":"message"}}"#,
            r#"{"type":"response.output_item.done","output_index":1.5,"item":{"type":"message"}}"#,
        ] {
            assert!(matches!(parse_frame(event), Frame::Malformed(_)));
        }
    }

    #[test]
    fn completed_extracts_usage_with_cache_breakdown() {
        let frame = parse_frame(
            r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":61},"output_tokens_details":{"reasoning_tokens":8}}}}"#,
        );
        match frame {
            Frame::Completed {
                usage: Some(u),
                stop,
            } => {
                assert_eq!(u.input_tokens, 100);
                assert_eq!(u.cached_tokens, 61);
                assert_eq!(u.output_tokens, 20);
                assert_eq!(u.reasoning_tokens, 8);
                assert_eq!(stop, StopReason::EndTurn);
            }
            other => panic!("expected Completed with usage, got {other:?}"),
        }
    }

    #[test]
    fn error_event_captured() {
        let frame = parse_frame(r#"{"type":"error","error":{"message":"boom"}}"#);
        assert_eq!(frame, Frame::Error("boom".into()));
    }

    #[test]
    fn failed_event_reads_nested_response_error() {
        let frame = parse_frame(
            r#"{"type":"response.failed","response":{"error":{"message":"nested boom"}}}"#,
        );
        assert_eq!(frame, Frame::Error("nested boom".into()));
    }

    #[test]
    fn incomplete_event_is_a_max_tokens_terminal_event() {
        let frame = parse_frame(
            r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}"#,
        );
        assert_eq!(
            frame,
            Frame::Completed {
                usage: None,
                stop: StopReason::MaxTokens,
            }
        );
    }

    #[test]
    fn malformed_usage_is_not_silently_dropped() {
        let frame = parse_frame(
            r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":"many"}}}"#,
        );
        assert!(matches!(frame, Frame::Malformed(_)));
    }
}
