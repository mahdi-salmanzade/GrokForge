//! The conversation record. `ResponseItem`s are the canonical, model-visible transcript,
//! persisted verbatim to the JSONL rollout (see ADR 0002) and replayed on resume.

use serde::{Deserialize, Serialize};

use crate::ids::ToolCallId;

/// One entry in the conversation history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponseItem {
    /// A message from the user.
    UserMessage {
        text: String,
        /// Number of secrets replaced before this text entered durable history. Optional on the
        /// wire so transcripts written before this field was introduced remain readable.
        #[serde(default, skip_serializing_if = "is_zero")]
        redactions: usize,
    },
    /// An assistant text message.
    AssistantMessage { text: String },
    /// Reasoning summary text (kept out of the resend prefix by default for cache-friendliness).
    Reasoning { text: String },
    /// Provider-encrypted reasoning state. Replayed verbatim so reasoning models can continue a
    /// stateless tool loop without storing the response on the provider's servers.
    EncryptedReasoning {
        id: String,
        status: String,
        summary: Vec<serde_json::Value>,
        encrypted_content: String,
    },
    /// A provider output item preserved byte-for-byte at the JSON value level and replayed in
    /// order for stateless Responses API continuation. Typed legacy variants remain readable.
    ProviderOutput { item: serde_json::Value },
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
        /// Number of secrets replaced before this output entered durable history.
        #[serde(default, skip_serializing_if = "is_zero")]
        redactions: usize,
    },
    /// A compaction summary that stands in for a replaced range of items.
    CompactionSummary {
        text: String,
        /// Aggregate redactions represented by this replacement history item.
        #[serde(default, skip_serializing_if = "is_zero")]
        redactions: usize,
    },
    /// Append-only persistence checkpoint. Readers replace the previously replayed prefix with
    /// this exact model-visible history; the checkpoint itself is never sent to a provider.
    CompactionCheckpoint { history: Vec<ResponseItem> },
}

impl ResponseItem {
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        ResponseItem::UserMessage {
            text: text.into(),
            redactions: 0,
        }
    }

    #[must_use]
    pub fn user_redacted(text: impl Into<String>, redactions: usize) -> Self {
        ResponseItem::UserMessage {
            text: text.into(),
            redactions,
        }
    }

    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        ResponseItem::AssistantMessage { text: text.into() }
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip predicate receives `&T`.
fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_reasoning_round_trips_without_changing_provider_payload() {
        let item = ResponseItem::EncryptedReasoning {
            id: "rs_provider_1".into(),
            status: "completed".into(),
            summary: vec![serde_json::json!({"type":"summary_text","text":"brief"})],
            encrypted_content: "opaque+ciphertext==".into(),
        };
        let wire = serde_json::to_vec(&item).unwrap();
        let decoded: ResponseItem = serde_json::from_slice(&wire).unwrap();
        assert_eq!(decoded, item);
    }

    #[test]
    fn legacy_redacted_item_fields_default_to_zero() {
        let user: ResponseItem =
            serde_json::from_str(r#"{"kind":"user_message","text":"legacy"}"#).unwrap();
        assert!(matches!(
            user,
            ResponseItem::UserMessage { redactions: 0, .. }
        ));

        let result: ResponseItem = serde_json::from_str(
            r#"{"kind":"tool_result","id":"call_legacy","content":"ok","is_error":false}"#,
        )
        .unwrap();
        assert!(matches!(
            result,
            ResponseItem::ToolResult { redactions: 0, .. }
        ));

        let summary: ResponseItem =
            serde_json::from_str(r#"{"kind":"compaction_summary","text":"legacy summary"}"#)
                .unwrap();
        assert!(matches!(
            summary,
            ResponseItem::CompactionSummary { redactions: 0, .. }
        ));
    }
}
