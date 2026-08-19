//! Structured questions from the agent to an interactive frontend.
//!
//! These types deliberately contain only bounded, serializable data. Frontends decide how to
//! collect the answer; the core validates model-produced requests before constructing them.

use serde::{Deserialize, Serialize};

use crate::ids::{QuestionId, ToolCallId};

/// One selectable answer displayed for a question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    /// Short primary label.
    pub label: String,
    /// Optional tradeoff or consequence shown beside the label.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// One question in a request. Every question has two to four mutually-exclusive choices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestion {
    /// Compact section label, such as `Database` or `Scope`.
    pub header: String,
    /// The complete question shown to the user.
    pub prompt: String,
    /// Mutually-exclusive choices, in display order.
    pub options: Vec<QuestionOption>,
    /// Whether the frontend may collect a free-form answer instead of a listed choice.
    #[serde(default)]
    pub allow_custom: bool,
}

/// A validated batch of one to three questions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionRequest {
    pub id: QuestionId,
    pub call_id: ToolCallId,
    pub questions: Vec<UserQuestion>,
}

/// The answer to one question. Exactly one of `selected` and `custom` is populated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionAnswer {
    /// Zero-based question index in [`QuestionRequest::questions`].
    pub question: usize,
    /// Zero-based selected option, when the user picked a listed choice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<usize>,
    /// A free-form answer, when the question allowed one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,
}

/// Result returned by a frontend's question handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum QuestionResponse {
    /// All questions were answered in request order.
    Answered { answers: Vec<QuestionAnswer> },
    /// The user dismissed the request.
    Cancelled,
    /// The active frontend cannot collect an answer (for example, a headless CI run).
    Unavailable { reason: String },
}

impl QuestionResponse {
    /// Number of structured answers contained in this response.
    #[must_use]
    pub fn answer_count(&self) -> usize {
        match self {
            Self::Answered { answers } => answers.len(),
            Self::Cancelled | Self::Unavailable { .. } => 0,
        }
    }

    /// Whether the user dismissed the request.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_round_trips_without_empty_optional_fields() {
        let response = QuestionResponse::Answered {
            answers: vec![QuestionAnswer {
                question: 0,
                selected: Some(1),
                custom: None,
            }],
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(!json.contains("custom"));
        assert_eq!(
            serde_json::from_str::<QuestionResponse>(&json).unwrap(),
            response
        );
    }
}
