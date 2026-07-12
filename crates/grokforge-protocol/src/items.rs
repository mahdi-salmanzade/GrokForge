//! The conversation record. `ResponseItem`s are the canonical, model-visible transcript,
//! persisted verbatim to the JSONL rollout (see ADR 0002) and replayed on resume.

use serde::{Deserialize, Serialize};

use crate::ids::ToolCallId;

/// One entry in the conversation history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponseItem {
    /// A message from the user.
    UserMessage { text: String },
    /// An assistant text message.
    AssistantMessage { text: String },
    /// Reasoning summary text (kept out of the resend prefix by default for cache-friendliness).
    Reasoning { text: String },
    /// A tool call the model requested.
    ToolCall {
        id: ToolCallId,
        name: String,
        arguments: String,
    },
    /// The result of a tool call, fed back to the model.
    ToolResult {
        id: ToolCallId,
        content: String,
        is_error: bool,
    },
    /// A compaction summary that stands in for a replaced range of items.
    CompactionSummary { text: String },
}

impl ResponseItem {
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        ResponseItem::UserMessage { text: text.into() }
    }

    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        ResponseItem::AssistantMessage { text: text.into() }
    }
}
