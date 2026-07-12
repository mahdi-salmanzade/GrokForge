//! Wire types for `POST /v1/responses` requests (OpenAI Responses API shape).
//!
//! Only the request side lives here; streamed response events are in [`crate::event`].
//! Fields the caller leaves unset are omitted from the JSON so we send a minimal, stable
//! prefix (important for xAI's automatic prompt caching).

use serde::Serialize;

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

    /// `response_format` for guaranteed-structure outputs (commit messages, plans). Passed
    /// through as raw JSON so any json_schema shape works.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<serde_json::Value>,

    /// Server-side conversation storage. Forced `false` whenever an image is in context
    /// (documented xAI failure mode with stored history + vision).
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
            store: None,
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
}

/// One item in the input list.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
}

/// A piece of message content.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    InputText { text: String },
    InputImage { image_url: String },
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
            ToolDef::web_search(),
        ]);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["name"], "read_file");
        assert_eq!(v["tools"][1]["type"], "web_search");
    }

    #[test]
    fn reasoning_effort_lowercases() {
        let req = ResponsesRequest::new("m", vec![]).with_reasoning(Effort::High);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["reasoning"]["effort"], "high");
    }
}
