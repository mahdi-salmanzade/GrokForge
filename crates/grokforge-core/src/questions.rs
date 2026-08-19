//! Model-callable structured questions and the frontend broker used to answer them.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use grokforge_protocol::{
    QuestionId, QuestionOption, QuestionRequest, QuestionResponse, ToolCallId, UserQuestion,
};
use serde::Deserialize;
use serde_json::json;

use crate::approvals::ApprovalNeed;
use crate::tools::{Tool, ToolInvocation, ToolOutput, ToolSpec};

/// Provider-visible name of the structured question tool.
pub const ASK_USER: &str = "ask_user";

const MAX_ARGUMENT_BYTES: usize = 16 * 1024;
const MAX_HEADER_BYTES: usize = 48;
const MAX_PROMPT_BYTES: usize = 1_024;
const MAX_LABEL_BYTES: usize = 96;
const MAX_DESCRIPTION_BYTES: usize = 512;
const MAX_UNAVAILABLE_REASON_BYTES: usize = 512;
/// Free-form answers are intentionally much smaller than the normal prompt composer. They are a
/// choice clarification, not a second hidden conversation channel.
pub const MAX_CUSTOM_ANSWER_BYTES: usize = 4 * 1024;

/// A frontend capable of collecting structured user input.
#[async_trait]
pub trait Questioner: Send + Sync {
    /// Ask a validated batch of questions and return a structured response.
    async fn ask(&self, request: QuestionRequest) -> QuestionResponse;
}

/// Safe default for non-interactive runs. It never guesses a user's intent.
#[derive(Debug, Clone, Default)]
pub struct AutoQuestioner;

#[async_trait]
impl Questioner for AutoQuestioner {
    async fn ask(&self, _request: QuestionRequest) -> QuestionResponse {
        QuestionResponse::Unavailable {
            reason: "interactive questions are unavailable in this headless run; state the needed choice in the final response instead"
                .to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequest {
    questions: Vec<RawQuestion>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQuestion {
    header: String,
    question: String,
    options: Vec<RawOption>,
    #[serde(default)]
    allow_custom: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOption {
    label: String,
    #[serde(default)]
    description: String,
}

/// Validate untrusted model arguments before anything reaches an interactive frontend.
pub(crate) fn parse_request(
    args: &serde_json::Value,
    call_id: ToolCallId,
) -> Result<QuestionRequest, String> {
    let argument_bytes =
        serde_json::to_vec(args).map_err(|error| format!("invalid question arguments: {error}"))?;
    if argument_bytes.len() > MAX_ARGUMENT_BYTES {
        return Err(format!(
            "question arguments exceed the {MAX_ARGUMENT_BYTES}-byte limit"
        ));
    }
    let raw: RawRequest = serde_json::from_value(args.clone())
        .map_err(|error| format!("invalid question arguments: {error}"))?;
    if !(1..=3).contains(&raw.questions.len()) {
        return Err("ask_user requires between 1 and 3 questions".to_string());
    }

    let mut questions = Vec::with_capacity(raw.questions.len());
    for (question_index, raw_question) in raw.questions.into_iter().enumerate() {
        let header = validate_text(
            &raw_question.header,
            MAX_HEADER_BYTES,
            false,
            &format!("questions[{question_index}].header"),
        )?;
        let prompt = validate_text(
            &raw_question.question,
            MAX_PROMPT_BYTES,
            true,
            &format!("questions[{question_index}].question"),
        )?;
        if !(2..=4).contains(&raw_question.options.len()) {
            return Err(format!(
                "questions[{question_index}].options must contain between 2 and 4 choices"
            ));
        }
        let mut labels = BTreeSet::new();
        let mut options = Vec::with_capacity(raw_question.options.len());
        for (option_index, raw_option) in raw_question.options.into_iter().enumerate() {
            let label = validate_text(
                &raw_option.label,
                MAX_LABEL_BYTES,
                false,
                &format!("questions[{question_index}].options[{option_index}].label"),
            )?;
            if !labels.insert(label.to_lowercase()) {
                return Err(format!(
                    "questions[{question_index}] contains duplicate option label `{label}`"
                ));
            }
            let description = if raw_option.description.trim().is_empty() {
                String::new()
            } else {
                validate_text(
                    &raw_option.description,
                    MAX_DESCRIPTION_BYTES,
                    true,
                    &format!("questions[{question_index}].options[{option_index}].description"),
                )?
            };
            options.push(QuestionOption { label, description });
        }
        questions.push(UserQuestion {
            header,
            prompt,
            options,
            allow_custom: raw_question.allow_custom,
        });
    }

    Ok(QuestionRequest {
        id: QuestionId::new(),
        call_id,
        questions,
    })
}

fn validate_text(
    value: &str,
    max_bytes: usize,
    multiline: bool,
    field: &str,
) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.len() > max_bytes {
        return Err(format!("{field} exceeds the {max_bytes}-byte limit"));
    }
    if value.chars().any(|character| {
        character.is_control() && !(multiline && matches!(character, '\n' | '\r' | '\t'))
    }) {
        return Err(format!("{field} contains terminal control characters"));
    }
    Ok(value.to_string())
}

/// Treat a frontend response as untrusted input and enforce the request's exact shape.
pub(crate) fn validate_response(
    request: &QuestionRequest,
    mut response: QuestionResponse,
) -> Result<QuestionResponse, String> {
    let answers = match &mut response {
        QuestionResponse::Cancelled => return Ok(response),
        QuestionResponse::Unavailable { reason } => {
            *reason = validate_text(
                reason,
                MAX_UNAVAILABLE_REASON_BYTES,
                true,
                "unavailable.reason",
            )?;
            return Ok(response);
        }
        QuestionResponse::Answered { answers } => answers,
    };
    if answers.len() != request.questions.len() {
        return Err(format!(
            "question frontend returned {} answers for {} questions",
            answers.len(),
            request.questions.len()
        ));
    }
    for (expected, answer) in answers.iter_mut().enumerate() {
        if answer.question != expected {
            return Err("question frontend returned answers out of order".to_string());
        }
        let question = &request.questions[expected];
        match (answer.selected, answer.custom.as_deref()) {
            (Some(selected), None) if selected < question.options.len() => {}
            (Some(_), None) => {
                return Err(format!(
                    "question frontend selected an option outside questions[{expected}]"
                ));
            }
            (None, Some(custom)) if question.allow_custom => {
                answer.custom = Some(validate_text(
                    custom,
                    MAX_CUSTOM_ANSWER_BYTES,
                    true,
                    &format!("answers[{expected}].custom"),
                )?);
            }
            (None, Some(_)) => {
                return Err(format!(
                    "question frontend returned a custom answer for questions[{expected}], which does not allow one"
                ));
            }
            _ => {
                return Err(format!(
                    "answers[{expected}] must contain exactly one selected option or custom answer"
                ));
            }
        }
    }
    Ok(response)
}

/// Render a validated response as compact, explicit JSON for the model-visible tool result.
pub(crate) fn response_output(
    request: &QuestionRequest,
    response: &QuestionResponse,
) -> ToolOutput {
    match response {
        QuestionResponse::Answered { answers } => {
            let answers = answers
                .iter()
                .map(|answer| {
                    let question = &request.questions[answer.question];
                    match (answer.selected, answer.custom.as_deref()) {
                        (Some(selected), None) => json!({
                            "question": answer.question,
                            "header": question.header,
                            "selected": selected,
                            "label": question.options[selected].label,
                        }),
                        (None, Some(custom)) => json!({
                            "question": answer.question,
                            "header": question.header,
                            "custom": custom,
                        }),
                        _ => json!({"question": answer.question, "invalid": true}),
                    }
                })
                .collect::<Vec<_>>();
            ToolOutput::success(json!({ "answers": answers }).to_string())
        }
        QuestionResponse::Cancelled => ToolOutput::failure("[question dismissed by user]"),
        QuestionResponse::Unavailable { reason } => {
            ToolOutput::failure(format!("[question unavailable: {reason}]"))
        }
    }
}

/// Marker tool. The turn runner intercepts it so the frontend broker stays on [`crate::Agent`]
/// instead of leaking into every [`crate::tools::TurnContext`] constructor.
#[derive(Debug)]
struct AskUserTool;

pub(crate) fn tool() -> Arc<dyn Tool> {
    Arc::new(AskUserTool)
}

#[async_trait]
impl Tool for AskUserTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ASK_USER.to_string(),
            description: "Ask the user 1-3 concise, blocking questions with 2-4 mutually exclusive choices each. Use only when their decision materially changes the work; do not use for safety approvals."
                .to_string(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["questions"],
                "properties": {
                    "questions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 3,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["header", "question", "options"],
                            "properties": {
                                "header": { "type": "string", "minLength": 1, "maxLength": 48 },
                                "question": { "type": "string", "minLength": 1, "maxLength": 1024 },
                                "allow_custom": { "type": "boolean", "default": false },
                                "options": {
                                    "type": "array",
                                    "minItems": 2,
                                    "maxItems": 4,
                                    "items": {
                                        "type": "object",
                                        "additionalProperties": false,
                                        "required": ["label"],
                                        "properties": {
                                            "label": { "type": "string", "minLength": 1, "maxLength": 96 },
                                            "description": { "type": "string", "maxLength": 512 }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }),
            mutating: false,
            parallel_safe: false,
        }
    }

    fn approval(
        &self,
        _args: &serde_json::Value,
        _ctx: &crate::tools::TurnContext,
    ) -> ApprovalNeed {
        ApprovalNeed::None
    }

    async fn invoke(&self, _inv: ToolInvocation<'_>) -> ToolOutput {
        ToolOutput::failure("ask_user must be dispatched by the agent question broker")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grokforge_protocol::QuestionAnswer;

    fn valid_args() -> serde_json::Value {
        json!({
            "questions": [{
                "header": "Database",
                "question": "Which database should back the service?",
                "options": [
                    {"label": "Postgres", "description": "Relational and durable"},
                    {"label": "SQLite", "description": "Simple local deployment"}
                ],
                "allow_custom": true
            }]
        })
    }

    #[test]
    fn validates_and_normalizes_a_question_request() {
        let request = parse_request(&valid_args(), ToolCallId::new()).unwrap();
        assert_eq!(request.questions.len(), 1);
        assert_eq!(request.questions[0].options.len(), 2);
        assert!(request.questions[0].allow_custom);
    }

    #[test]
    fn rejects_terminal_controls_and_duplicate_choices() {
        let mut args = valid_args();
        args["questions"][0]["header"] = json!("bad\u{1b}[2J");
        assert!(parse_request(&args, ToolCallId::new()).is_err());

        let mut args = valid_args();
        args["questions"][0]["options"][1]["label"] = json!("postgres");
        assert!(parse_request(&args, ToolCallId::new()).is_err());
    }

    #[test]
    fn validates_frontend_answers_against_the_request() {
        let request = parse_request(&valid_args(), ToolCallId::new()).unwrap();
        let valid = QuestionResponse::Answered {
            answers: vec![QuestionAnswer {
                question: 0,
                selected: Some(1),
                custom: None,
            }],
        };
        assert_eq!(validate_response(&request, valid.clone()).unwrap(), valid);

        let invalid = QuestionResponse::Answered {
            answers: vec![QuestionAnswer {
                question: 0,
                selected: Some(9),
                custom: None,
            }],
        };
        assert!(validate_response(&request, invalid).is_err());
    }

    #[tokio::test]
    async fn headless_questioner_never_guesses() {
        let request = parse_request(&valid_args(), ToolCallId::new()).unwrap();
        assert!(matches!(
            AutoQuestioner.ask(request).await,
            QuestionResponse::Unavailable { .. }
        ));
    }
}
