//! The context assembler: the single code path that turns session state into an outbound
//! request, and the ledger choke point (ADR 0003). Nothing else may build a request body, so
//! every byte that leaves the machine is accounted for and reconciles with the serialized size.

use grokforge_protocol::{LedgerEntry, RequestLedger, ResponseItem};
use grokforge_xai::{ContentPart, InputItem, ResponsesRequest, Role, XaiClient, XaiError};

use crate::agents_md::AgentsDoc;
use crate::redaction::Redactor;
use crate::session::Session;

/// A request plus its reconciled ledger.
#[derive(Debug)]
pub struct Assembled {
    pub request: ResponsesRequest,
    pub ledger: RequestLedger,
    /// Exact serialized body length; equals `ledger.total_bytes()`.
    pub body_len: usize,
}

/// Build the request for the current session state. `agents_md` content is redacted here;
/// history items were already redacted at ingress (tool output / user input).
pub fn assemble(
    session: &Session,
    agents_md: &[AgentsDoc],
    tool_defs: Vec<grokforge_xai::ToolDef>,
) -> Result<Assembled, XaiError> {
    let mut input: Vec<InputItem> = Vec::new();
    let mut ledger = RequestLedger::default();

    // System prompt (structural — accounted for in the envelope entry, not as user data).
    input.push(InputItem::text(
        Role::Developer,
        &session.config.system_prompt,
    ));

    // AGENTS.md, redacted, counted as auto-context.
    for doc in agents_md {
        let red = Redactor::apply(&doc.content);
        let block = format!(
            "<AGENTS.md path=\"{}\">\n{}\n</AGENTS.md>",
            doc.path.display(),
            red.text
        );
        ledger.push(
            LedgerEntry::new(
                format!("agents_md:{}", doc.path.display()),
                red.text.len(),
                "auto-context",
            )
            .with_redactions(red.count),
        );
        input.push(InputItem::text(Role::Developer, block));
    }

    // Conversation history. Reasoning is not resent (cache-friendly).
    let mut conversation_bytes = 0usize;
    for item in &session.history {
        match item {
            ResponseItem::UserMessage { text } => {
                conversation_bytes += text.len();
                input.push(InputItem::text(Role::User, text));
            }
            ResponseItem::AssistantMessage { text } => {
                conversation_bytes += text.len();
                input.push(InputItem::text(Role::Assistant, text));
            }
            ResponseItem::Reasoning { .. } => {}
            ResponseItem::ToolCall {
                id,
                name,
                arguments,
            } => {
                conversation_bytes += arguments.len();
                input.push(InputItem::FunctionCall {
                    call_id: id.to_string(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
            }
            ResponseItem::ToolResult { id, content, .. } => {
                conversation_bytes += content.len();
                input.push(InputItem::FunctionCallOutput {
                    call_id: id.to_string(),
                    output: content.clone(),
                });
            }
            ResponseItem::CompactionSummary { text } => {
                conversation_bytes += text.len();
                input.push(InputItem::Message {
                    role: Role::Developer,
                    content: vec![ContentPart::InputText {
                        text: format!("[summary of earlier conversation]\n{text}"),
                    }],
                });
            }
        }
    }
    if conversation_bytes > 0 {
        ledger.push(LedgerEntry::new(
            "conversation",
            conversation_bytes,
            "history",
        ));
    }

    let mut request =
        ResponsesRequest::new(session.config.model.clone(), input).with_tools(tool_defs);
    request.parallel_tool_calls = Some(true);
    if let Some(effort) = session.config.effort {
        request = request.with_reasoning(effort);
    }

    let (_body, body_len) = XaiClient::serialize_request(&request)?;

    // Reconcile: attribute the remaining bytes (system prompt, tool schemas, JSON envelope) to a
    // single honest entry so the ledger total equals the exact serialized size.
    let content_bytes = ledger.total_bytes();
    let envelope = body_len.saturating_sub(content_bytes);
    ledger.push(LedgerEntry::new(
        "system_prompt+tool_specs+envelope",
        envelope,
        "protocol",
    ));

    Ok(Assembled {
        request,
        ledger,
        body_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, SessionConfig};
    use std::path::PathBuf;

    #[test]
    fn ledger_reconciles_with_serialized_body() {
        let mut session = Session::new(SessionConfig::new(PathBuf::from("/tmp"), "grok-build-0.1"));
        session.history.push(ResponseItem::user("hello world"));
        let assembled = assemble(&session, &[], vec![]).unwrap();
        assert_eq!(assembled.ledger.total_bytes(), assembled.body_len);
    }

    #[test]
    fn agents_md_secret_is_redacted_and_counted() {
        let session = Session::new(SessionConfig::new(PathBuf::from("/tmp"), "m"));
        let docs = vec![AgentsDoc {
            path: PathBuf::from("/tmp/AGENTS.md"),
            content: "deploy key: xai-ABCDEF0123456789ZZZ".to_string(),
        }];
        let assembled = assemble(&session, &docs, vec![]).unwrap();
        let (body, _) = XaiClient::serialize_request(&assembled.request).unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(!body.contains("xai-ABCDEF0123456789ZZZ"));
        assert!(assembled.ledger.total_redactions() >= 1);
        assert_eq!(assembled.ledger.total_bytes(), assembled.body_len);
    }
}
