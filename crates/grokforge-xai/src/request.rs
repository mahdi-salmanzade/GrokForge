//! Wire types for `POST /v1/responses` requests (OpenAI Responses API shape).
//!
//! Only the request side lives here; streamed response events are in [`crate::event`].
//! Fields the caller leaves unset are omitted from the JSON so we send a minimal, stable
//! prefix (important for xAI's automatic prompt caching).

use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

/// A request to the Responses API.
#[derive(Debug, Clone, Serialize)]
pub struct ResponsesRequest {
    /// Model slug, e.g. `grok-build-0.1`. Never a constant — comes from config.
    pub model: String,

    /// Ordered input items (messages, tool outputs).
    pub input: Vec<InputItem>,

    /// Client and server-side tool definitions. Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,

    /// Stream the response as SSE. GrokForge always streams.
    pub stream: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,

    /// Guaranteed-structure output format (commit messages, plans). The public convenience
    /// value is the `format` object; on the Responses API wire it is nested under `text.format`.
    #[serde(
        rename = "text",
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_text_format"
    )]
    pub response_format: Option<serde_json::Value>,

    /// Stable routing key for automatic prompt-cache reuse within one conversation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,

    /// Additional provider output fields to return. Encrypted reasoning is requested by default
    /// because `store: false` tool loops must replay it on their next stateless request.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,

    /// Server-side conversation storage. GrokForge defaults this to `false`: it replays its
    /// locally persisted transcript and does not need the API's 30-day response retention.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
}

impl ResponsesRequest {
    /// A streaming request with just a model and input; everything else defaulted off.
    #[must_use]
    pub fn new(model: impl Into<String>, input: Vec<InputItem>) -> Self {
        Self {
            model: model.into(),
            input,
            tools: Vec::new(),
            stream: true,
            parallel_tool_calls: None,
            reasoning: None,
            response_format: None,
            prompt_cache_key: None,
            include: vec!["reasoning.encrypted_content".to_string()],
            store: Some(false),
        }
    }

    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ToolDef>) -> Self {
        self.tools = tools;
        self
    }

    #[must_use]
    pub fn with_reasoning(mut self, effort: Effort) -> Self {
        self.reasoning = Some(Reasoning { effort });
        self
    }

    /// Request a structured text format. The value is the Responses API `text.format` object.
    #[must_use]
    pub fn with_response_format(mut self, format: serde_json::Value) -> Self {
        self.response_format = Some(format);
        self
    }

    /// Route requests from the same conversation to a cache-compatible backend.
    #[must_use]
    pub fn with_prompt_cache_key(mut self, key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(key.into());
        self
    }
}

#[allow(clippy::ref_option)] // serde's `serialize_with` contract passes `&FieldType`.
fn serialize_text_format<S>(
    value: &Option<serde_json::Value>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    #[derive(Serialize)]
    struct TextFormat<'a> {
        format: &'a serde_json::Value,
    }

    match value {
        Some(format) => TextFormat { format }.serialize(serializer),
        None => serializer.serialize_none(),
    }
}

/// One item in the input list.
#[derive(Debug, Clone)]
pub enum InputItem {
    /// A chat message.
    Message {
        role: Role,
        content: Vec<ContentPart>,
    },
    /// A prior assistant function call, replayed so the model sees a coherent transcript.
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// The result of a client-side tool/function call, fed back to the model.
    FunctionCallOutput { call_id: String, output: String },
    /// A provider-issued encrypted reasoning output item replayed unchanged for stateless
    /// reasoning continuity.
    Reasoning {
        id: String,
        status: String,
        summary: Vec<serde_json::Value>,
        encrypted_content: String,
    },
    /// An output item received from the provider and replayed without schema projection.
    Raw(serde_json::Value),
}

impl Serialize for InputItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            InputItem::Message { role, content } => {
                let mut state = serializer.serialize_struct("InputItem", 3)?;
                state.serialize_field("type", "message")?;
                state.serialize_field("role", role)?;
                state.serialize_field("content", content)?;
                state.end()
            }
            InputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                let mut state = serializer.serialize_struct("InputItem", 4)?;
                state.serialize_field("type", "function_call")?;
                state.serialize_field("call_id", call_id)?;
                state.serialize_field("name", name)?;
                state.serialize_field("arguments", arguments)?;
                state.end()
            }
            InputItem::FunctionCallOutput { call_id, output } => {
                let mut state = serializer.serialize_struct("InputItem", 3)?;
                state.serialize_field("type", "function_call_output")?;
                state.serialize_field("call_id", call_id)?;
                state.serialize_field("output", output)?;
                state.end()
            }
            InputItem::Reasoning {
                id,
                status,
                summary,
                encrypted_content,
            } => {
                let mut state = serializer.serialize_struct("InputItem", 5)?;
                state.serialize_field("type", "reasoning")?;
                state.serialize_field("id", id)?;
                state.serialize_field("status", status)?;
                state.serialize_field("summary", summary)?;
                state.serialize_field("encrypted_content", encrypted_content)?;
                state.end()
            }
            InputItem::Raw(item) => item.serialize(serializer),
        }
    }
}

impl InputItem {
    /// Convenience: a text-only message.
    #[must_use]
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        InputItem::Message {
            role,
            content: vec![ContentPart::InputText { text: text.into() }],
        }
    }

    /// Reconstruct a legacy assistant message using the Responses output-content wire type.
    /// New responses should prefer replaying their preserved provider output item verbatim.
    #[must_use]
    pub fn assistant_output(text: impl Into<String>) -> Self {
        InputItem::Message {
            role: Role::Assistant,
            content: vec![ContentPart::OutputText { text: text.into() }],
        }
    }
}

/// A piece of message content.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    InputText { text: String },
    InputImage { image_url: String },
    OutputText { text: String },
}

/// Message author role.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
}

/// A tool the model may call. Function tools are client-executed; server variants (web/X
/// search, code execution) run inside xAI and are metered separately.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ToolDef {
    /// A client-side function tool.
    Function(FunctionTool),
    /// A server-side tool, passed through verbatim (e.g. `{"type":"web_search"}`).
    Server(serde_json::Value),
}

/// Grok-native tools executed by xAI rather than by the local agent.
///
/// This closed enum is intentionally separate from [`ToolDef::Server`]: session and CLI
/// configuration can opt into known, separately metered capabilities without accepting an
/// arbitrary provider-tool JSON object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ServerTool {
    WebSearch,
    XSearch,
    CodeInterpreter,
}

impl ServerTool {
    /// Convert this opt-in capability to its Responses API tool definition.
    #[must_use]
    pub fn definition(self) -> ToolDef {
        match self {
            Self::WebSearch => ToolDef::web_search(),
            Self::XSearch => ToolDef::x_search(),
            Self::CodeInterpreter => ToolDef::code_interpreter(),
        }
    }
}

impl ToolDef {
    #[must_use]
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        ToolDef::Function(FunctionTool {
            kind: "function",
            name: name.into(),
            description: description.into(),
            parameters,
        })
    }

    /// The function name, for client-side function tools.
    #[must_use]
    pub fn function_name(&self) -> Option<&str> {
        match self {
            ToolDef::Function(f) => Some(&f.name),
            ToolDef::Server(_) => None,
        }
    }

    #[must_use]
    pub fn web_search() -> Self {
        ToolDef::Server(serde_json::json!({ "type": "web_search" }))
    }

    #[must_use]
    pub fn x_search() -> Self {
        ToolDef::Server(serde_json::json!({ "type": "x_search" }))
    }

    #[must_use]
    pub fn code_interpreter() -> Self {
        ToolDef::Server(serde_json::json!({ "type": "code_interpreter" }))
    }
}

/// A client-side function tool definition.
#[derive(Debug, Clone, Serialize)]
pub struct FunctionTool {
    #[serde(rename = "type")]
    kind: &'static str,
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Reasoning controls. `effort` is the primary cost/latency knob.
#[derive(Debug, Clone, Serialize)]
pub struct Reasoning {
    pub effort: Effort,
}

/// Reasoning effort level. `Xhigh` is only accepted by the multi-agent model.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_request_omits_unset_fields() {
        let req = ResponsesRequest::new("grok-build-0.1", vec![InputItem::text(Role::User, "hi")]);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["model"], "grok-build-0.1");
        assert_eq!(v["stream"], true);
        // Unset optionals are absent, keeping the serialized prefix stable for prompt caching.
        assert!(v.get("tools").is_none());
        assert!(v.get("reasoning").is_none());
        assert!(v.get("response_format").is_none());
        assert!(v.get("text").is_none());
        assert!(v.get("prompt_cache_key").is_none());
        assert_eq!(v["store"], false);
        assert_eq!(v["include"][0], "reasoning.encrypted_content");
        assert_eq!(v["input"][0]["type"], "message");
        assert_eq!(v["input"][0]["role"], "user");
        assert_eq!(v["input"][0]["content"][0]["type"], "input_text");
    }

    #[test]
    fn function_and_server_tools_serialize() {
        let req = ResponsesRequest::new("m", vec![]).with_tools(vec![
            ToolDef::function(
                "read_file",
                "read a file",
                serde_json::json!({"type":"object"}),
            ),
            ServerTool::WebSearch.definition(),
            ServerTool::XSearch.definition(),
            ServerTool::CodeInterpreter.definition(),
        ]);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["name"], "read_file");
        assert_eq!(v["tools"][1]["type"], "web_search");
        assert_eq!(v["tools"][2]["type"], "x_search");
        assert_eq!(v["tools"][3]["type"], "code_interpreter");
    }

    #[test]
    fn reasoning_effort_lowercases() {
        let req = ResponsesRequest::new("m", vec![]).with_reasoning(Effort::High);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["reasoning"]["effort"], "high");
    }

    #[test]
    fn response_format_uses_responses_api_text_envelope() {
        let req = ResponsesRequest::new("m", vec![]).with_response_format(
            serde_json::json!({"type":"json_schema","name":"plan","schema":{"type":"object"}}),
        );
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("response_format").is_none());
        assert_eq!(v["text"]["format"]["type"], "json_schema");
        assert_eq!(v["text"]["format"]["name"], "plan");
    }

    #[test]
    fn prompt_cache_key_serializes_at_top_level() {
        let req = ResponsesRequest::new("m", vec![]).with_prompt_cache_key("session-42");
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["prompt_cache_key"], "session-42");
    }

    #[test]
    fn image_requests_keep_server_storage_off() {
        let req = ResponsesRequest::new(
            "m",
            vec![InputItem::Message {
                role: Role::User,
                content: vec![ContentPart::InputImage {
                    image_url: "https://example.test/image.png".into(),
                }],
            }],
        );
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["store"], false);
    }

    #[test]
    fn encrypted_reasoning_item_replays_exact_wire_fields() {
        let req = ResponsesRequest::new(
            "m",
            vec![InputItem::Reasoning {
                id: "rs_1".into(),
                status: "completed".into(),
                summary: vec![],
                encrypted_content: "opaque==".into(),
            }],
        );
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["input"][0]["type"], "reasoning");
        assert_eq!(v["input"][0]["id"], "rs_1");
        assert_eq!(v["input"][0]["status"], "completed");
        assert_eq!(v["input"][0]["summary"], serde_json::json!([]));
        assert_eq!(v["input"][0]["encrypted_content"], "opaque==");
    }

    #[test]
    fn legacy_assistant_replay_uses_output_text_content() {
        let request = ResponsesRequest::new("m", vec![InputItem::assistant_output("answer")]);
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["input"][0]["role"], "assistant");
        assert_eq!(value["input"][0]["content"][0]["type"], "output_text");
        assert_eq!(value["input"][0]["content"][0]["text"], "answer");
    }
}
