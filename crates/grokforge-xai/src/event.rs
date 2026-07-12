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
    /// Token accounting for the response.
    Usage(Usage),
    /// The response finished.
    Completed { stop: StopReason },
}

/// A fully-formed tool call requested by the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    /// Raw JSON arguments as a string (validated/parsed by the tool layer).
    pub arguments: String,
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
        item_id: String,
        call_id: String,
        name: String,
        arguments: Option<String>,
    },
    Completed {
        usage: Option<Usage>,
        stop: StopReason,
    },
    /// The API signalled an error inside the stream.
    Error(String),
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
        Err(_) => return Frame::Ignore,
    };
    let kind = value.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match kind {
        "response.created" => Frame::Created {
            response_id: value
                .get("response")
                .and_then(|r| r.get("id"))
                .and_then(|i| i.as_str())
                .map(String::from),
        },
        "response.output_text.delta" => text_delta(&value).map_or(Frame::Ignore, Frame::TextDelta),
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            text_delta(&value).map_or(Frame::Ignore, Frame::ReasoningDelta)
        }
        "response.function_call_arguments.delta" => {
            let item_id = str_field(&value, "item_id").unwrap_or_default();
            let delta = str_field(&value, "delta").unwrap_or_default();
            if delta.is_empty() {
                Frame::Ignore
            } else {
                Frame::ToolArgsDelta { item_id, delta }
            }
        }
        "response.output_item.done" => parse_output_item_done(&value),
        "response.completed" => {
            let usage = value
                .get("response")
                .and_then(|r| r.get("usage"))
                .and_then(parse_usage);
            let stop = value
                .get("response")
                .and_then(|r| r.get("status"))
                .and_then(|s| s.as_str())
                .map_or(StopReason::EndTurn, map_stop);
            Frame::Completed { usage, stop }
        }
        "error" | "response.failed" => {
            let msg = value
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
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

fn parse_output_item_done(value: &serde_json::Value) -> Frame {
    let Some(item) = value.get("item") else {
        return Frame::Ignore;
    };
    let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if item_type != "function_call" {
        return Frame::Ignore;
    }
    let item_id = item
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or_default()
        .to_string();
    let call_id = item
        .get("call_id")
        .and_then(|i| i.as_str())
        .map_or_else(|| item_id.clone(), String::from);
    let name = item
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or_default()
        .to_string();
    let arguments = item
        .get("arguments")
        .and_then(|a| a.as_str())
        .map(String::from);
    Frame::ToolItemDone {
        item_id,
        call_id,
        name,
        arguments,
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
        assert_eq!(parse_frame("not json"), Frame::Ignore);
    }

    #[test]
    fn function_call_item_done_extracts_call() {
        let frame = parse_frame(
            r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_9","name":"read_file","arguments":"{\"path\":\"a.rs\"}"}}"#,
        );
        match frame {
            Frame::ToolItemDone {
                call_id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(call_id, "call_9");
                assert_eq!(name, "read_file");
                assert_eq!(arguments.as_deref(), Some(r#"{"path":"a.rs"}"#));
            }
            other => panic!("expected ToolItemDone, got {other:?}"),
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
}
