//! The context assembler: the single code path that turns session state into an outbound
//! request, and the ledger choke point (ADR 0003). Nothing else may build a request body, so
//! every byte that leaves the machine is accounted for and reconciles with the serialized size.

use grokforge_protocol::{LedgerEntry, RequestLedger, ResponseItem};
use grokforge_xai::{ContentPart, InputItem, ResponsesRequest, Role, XaiClient, XaiError};

use crate::agents_md::AgentsDoc;
use crate::redaction::Redactor;
use crate::session::Session;
use crate::skills::SkillDoc;

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
#[allow(clippy::too_many_lines)]
pub fn assemble(
    session: &Session,
    agents_md: &[AgentsDoc],
    memory: &[crate::memory::MemoryDoc],
    skills: &[SkillDoc],
    tool_defs: Vec<grokforge_xai::ToolDef>,
) -> Result<Assembled, XaiError> {
    let mut input: Vec<InputItem> = Vec::new();
    let mut ledger = RequestLedger::default();

    // A configured system prompt is still user-controlled input and can contain pasted secrets.
    let system_prompt = Redactor::apply(&session.config.system_prompt);
    input.push(InputItem::text(Role::Developer, &system_prompt.text));
    ledger.push(
        LedgerEntry::new("system_prompt", system_prompt.text.len(), "configuration")
            .with_redactions(system_prompt.count),
    );

    // AGENTS.md, redacted, counted as auto-context.
    for doc in agents_md {
        let red = Redactor::apply(&doc.content);
        // Absolute workspace paths disclose local usernames/home layouts without helping the
        // model. AGENTS discovery is workspace-confined; retain only that relative identity.
        let relative = doc
            .path
            .strip_prefix(&session.config.workspace_root)
            .unwrap_or_else(|_| std::path::Path::new("AGENTS.md"));
        let display_path = Redactor::apply(&relative.to_string_lossy());
        let path_json = serde_json::to_string(&display_path.text)
            .unwrap_or_else(|_| "\"<invalid path>\"".to_string());
        let block = format!("<AGENTS.md>\nPath: {path_json}\n{}\n</AGENTS.md>", red.text);
        ledger.push(
            LedgerEntry::new(
                format!("agents_md:{}", display_path.text),
                block.len(),
                "auto-context",
            )
            .with_redactions(red.count.saturating_add(display_path.count)),
        );
        input.push(InputItem::text(Role::Developer, block));
    }

    // Persistent memory (the auto-loaded MEMORY.md index), redacted and counted as auto-context.
    for doc in memory {
        let red = Redactor::apply(&doc.content);
        let relative = doc
            .path
            .strip_prefix(&session.config.workspace_root)
            .unwrap_or_else(|_| std::path::Path::new(".grokforge/memory/MEMORY.md"));
        let display_path = Redactor::apply(&relative.to_string_lossy());
        let path_json = serde_json::to_string(&display_path.text)
            .unwrap_or_else(|_| "\"<invalid path>\"".to_string());
        let block = format!(
            "<memory>\nYour persistent memory from earlier sessions (source: {path_json}). Trust and use it, and call the `remember` tool to save durable new facts.\n{}\n</memory>",
            red.text
        );
        ledger.push(
            LedgerEntry::new(
                format!("memory:{}", display_path.text),
                block.len(),
                "auto-context",
            )
            .with_redactions(red.count.saturating_add(display_path.count)),
        );
        input.push(InputItem::text(Role::Developer, block));
    }

    // Advertise a compact skill catalog. Full SKILL.md bodies remain local until the model
    // chooses a relevant workflow and reads it through the ordinary ledgered file tool.
    for skill in skills {
        let red = Redactor::apply(&skill.description);
        let red_name = Redactor::apply(&skill.name);
        let relative = skill
            .path
            .strip_prefix(&session.config.workspace_root)
            .unwrap_or_else(|_| std::path::Path::new(".grokforge/skills/SKILL.md"));
        let display_path = Redactor::apply(&relative.to_string_lossy());
        let path_json = serde_json::to_string(&display_path.text)
            .unwrap_or_else(|_| "\"<invalid path>\"".to_string());
        let name_json = serde_json::to_string(&red_name.text)
            .unwrap_or_else(|_| "\"<invalid name>\"".to_string());
        let block = format!(
            "<available_skill name={name_json}>\nPath: {path_json}\nDescription: {}\nRead this SKILL.md with read_file before applying it.\n</available_skill>",
            red.text
        );
        ledger.push(
            LedgerEntry::new(
                format!("skill:{}", safe_label(&red_name.text)),
                block.len(),
                "auto-context",
            )
            .with_redactions(
                red.count
                    .saturating_add(red_name.count)
                    .saturating_add(display_path.count),
            ),
        );
        input.push(InputItem::text(Role::Developer, block));
    }

    // Conversation history. Emit per-item provenance rather than collapsing tool/file bytes into
    // one opaque bucket. Reasoning summaries are not resent (cache-friendly).
    let mut calls = std::collections::BTreeMap::<String, String>::new();
    for item in &session.history {
        match item {
            ResponseItem::UserMessage { text, redactions } => {
                ledger.push(
                    LedgerEntry::new("history:user", text.len(), "history")
                        .with_redactions(*redactions),
                );
                input.push(InputItem::text(Role::User, text));
            }
            ResponseItem::AssistantMessage { text } => {
                ledger.push(LedgerEntry::new("history:assistant", text.len(), "history"));
                input.push(InputItem::assistant_output(text));
            }
            ResponseItem::Reasoning { .. } | ResponseItem::CompactionCheckpoint { .. } => {}
            ResponseItem::EncryptedReasoning {
                id,
                status,
                summary,
                encrypted_content,
            } => {
                let bytes = 0usize
                    .saturating_add(id.len())
                    .saturating_add(status.len())
                    .saturating_add(encrypted_content.len())
                    .saturating_add(
                        serde_json::to_vec(summary).map_or(0, |serialized| serialized.len()),
                    );
                ledger.push(LedgerEntry::new(
                    "provider_output:reasoning",
                    bytes,
                    "stateless continuation",
                ));
                input.push(InputItem::Reasoning {
                    id: id.clone(),
                    status: status.clone(),
                    summary: summary.clone(),
                    encrypted_content: encrypted_content.clone(),
                });
            }
            ResponseItem::ProviderOutput { item } => {
                // Provider-native function calls are the ordinary stateless-continuation path.
                // They carry the same model-generated arguments as `ResponseItem::ToolCall`, but
                // nested inside the raw provider item, so redact that field before replay too.
                // Keep every other provider field byte-for-byte intact (notably opaque reasoning
                // state and provider ids).
                let mut replay_item = item.clone();
                let item_type = replay_item
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let mut redactions = 0usize;
                let source = if item_type == "function_call" {
                    let call_id = replay_item
                        .get("call_id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let name = replay_item
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let redacted_arguments = replay_item
                        .get("arguments")
                        .and_then(serde_json::Value::as_str)
                        .map(Redactor::apply);
                    let arguments = redacted_arguments
                        .as_ref()
                        .map_or("", |redacted| redacted.text.as_str());
                    let label = tool_label(&name, arguments);
                    calls.insert(call_id, label.clone());
                    if let Some(redacted) = redacted_arguments {
                        redactions = redacted.count;
                        if let Some(arguments) = replay_item.get_mut("arguments") {
                            *arguments = serde_json::Value::String(redacted.text);
                        }
                    }
                    format!("provider_output:{label}")
                } else {
                    format!("provider_output:{}", safe_label(&item_type))
                };
                let bytes =
                    serde_json::to_vec(&replay_item).map_or(0, |serialized| serialized.len());
                ledger.push(
                    LedgerEntry::new(source, bytes, "stateless continuation")
                        .with_redactions(redactions),
                );
                input.push(InputItem::Raw(replay_item));
            }
            ResponseItem::ToolCall {
                id,
                name,
                arguments,
            } => {
                // Tool-call arguments can carry secrets (for example a shell command containing a
                // token). Redact them before they re-enter the request body, and account the
                // redactions in the ledger so byte reconciliation against the serialized body
                // stays exact.
                let red = Redactor::apply(arguments);
                let label = tool_label(name, &red.text);
                calls.insert(id.to_string(), label.clone());
                ledger.push(
                    LedgerEntry::new(format!("tool_call:{label}"), red.text.len(), "history")
                        .with_redactions(red.count),
                );
                input.push(InputItem::FunctionCall {
                    call_id: id.to_string(),
                    name: name.clone(),
                    arguments: red.text,
                });
            }
            ResponseItem::ToolResult {
                id,
                content,
                redactions,
                ..
            } => {
                let label = calls
                    .get(id.as_str())
                    .cloned()
                    .unwrap_or_else(|| format!("unknown:{}", safe_label(id.as_str())));
                ledger.push(
                    LedgerEntry::new(format!("tool_result:{label}"), content.len(), "tool output")
                        .with_redactions(*redactions),
                );
                input.push(InputItem::FunctionCallOutput {
                    call_id: id.to_string(),
                    output: content.clone(),
                });
            }
            ResponseItem::CompactionSummary { text, redactions } => {
                ledger.push(
                    LedgerEntry::new("history:compaction_summary", text.len(), "history")
                        .with_redactions(*redactions),
                );
                input.push(InputItem::Message {
                    role: Role::Developer,
                    content: vec![ContentPart::InputText {
                        text: format!("[summary of earlier conversation]\n{text}"),
                    }],
                });
            }
        }
    }

    let mut request = ResponsesRequest::new(session.config.model.clone(), input)
        .with_tools(tool_defs)
        .with_prompt_cache_key(session.id.as_uuid().to_string());
    request.parallel_tool_calls = Some(true);
    if let Some(effort) = session.config.effort {
        request = request.with_reasoning(effort);
    }

    reconcile(request, ledger)
}

fn tool_label(name: &str, arguments: &str) -> String {
    let name = safe_label(name);
    let path = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(safe_label)
        });
    path.map_or(name.clone(), |path| format!("{name}:{path}"))
}

fn safe_label(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                '_'
            } else {
                character
            }
        })
        .take(200)
        .collect()
}

/// Build a ledgered auxiliary text request (currently conversation compaction). Keeping this in
/// the assembler preserves the rule that no network request body bypasses provenance accounting.
pub(crate) fn assemble_auxiliary(
    session: &Session,
    developer_text: &str,
    user_text: &str,
    source: &str,
) -> Result<Assembled, XaiError> {
    let developer = Redactor::apply(developer_text);
    let user = Redactor::apply(user_text);
    let input = vec![
        InputItem::text(Role::Developer, &developer.text),
        InputItem::text(Role::User, &user.text),
    ];
    let mut ledger = RequestLedger::default();
    ledger.push(
        LedgerEntry::new("auxiliary_instruction", developer.text.len(), source)
            .with_redactions(developer.count),
    );
    ledger.push(
        LedgerEntry::new("auxiliary_input", user.text.len(), source).with_redactions(user.count),
    );
    let request = ResponsesRequest::new(session.config.model.clone(), input)
        .with_prompt_cache_key(session.id.as_uuid().to_string());
    reconcile(request, ledger)
}

fn reconcile(request: ResponsesRequest, mut ledger: RequestLedger) -> Result<Assembled, XaiError> {
    let (_body, body_len) = XaiClient::serialize_request(&request)?;
    let content_bytes = ledger.total_bytes();
    let envelope = body_len.saturating_sub(content_bytes);
    ledger.push(LedgerEntry::new(
        "tool_specs+envelope",
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
        let assembled = assemble(&session, &[], &[], &[], vec![]).unwrap();
        assert_eq!(assembled.ledger.total_bytes(), assembled.body_len);
    }

    #[test]
    fn agents_md_secret_is_redacted_and_counted() {
        let workspace = PathBuf::from("/home/private-user/project");
        let session = Session::new(SessionConfig::new(workspace.clone(), "m"));
        let docs = vec![AgentsDoc {
            path: workspace.join("AGENTS.md"),
            content: "deploy key: xai-ABCDEF0123456789ZZZ".to_string(),
        }];
        let assembled = assemble(&session, &docs, &[], &[], vec![]).unwrap();
        let (body, _) = XaiClient::serialize_request(&assembled.request).unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(!body.contains("xai-ABCDEF0123456789ZZZ"));
        assert!(!body.contains("/home/private-user/project"));
        assert!(body.contains("Path: \\\"AGENTS.md\\\""));
        assert!(assembled.ledger.total_redactions() >= 1);
        let entry = assembled
            .ledger
            .entries
            .iter()
            .find(|entry| entry.source == "agents_md:AGENTS.md")
            .unwrap();
        assert!(entry.bytes > "deploy key: [REDACTED:xai-key]".len());
        assert_eq!(assembled.ledger.total_bytes(), assembled.body_len);
    }

    #[test]
    fn skill_is_redacted_ledgered_and_uses_a_relative_path() {
        let workspace = PathBuf::from("/home/private-user/project");
        let session = Session::new(SessionConfig::new(workspace.clone(), "m"));
        let skills = vec![SkillDoc {
            name: "release".to_string(),
            path: workspace.join(".grokforge/skills/release/SKILL.md"),
            description: "publish with xai-ABCDEF0123456789ZZZ".to_string(),
        }];

        let assembled = assemble(&session, &[], &[], &skills, vec![]).unwrap();
        let (body, _) = XaiClient::serialize_request(&assembled.request).unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("<available_skill name=\\\"release\\\">"));
        assert!(body.contains(".grokforge/skills/release/SKILL.md"));
        assert!(body.contains("Read this SKILL.md with read_file"));
        assert!(!body.contains("xai-ABCDEF0123456789ZZZ"));
        assert!(!body.contains("/home/private-user/project"));
        assert!(
            assembled
                .ledger
                .entries
                .iter()
                .any(|entry| { entry.source == "skill:release" && entry.redactions == 1 })
        );
        assert_eq!(assembled.ledger.total_bytes(), assembled.body_len);
    }

    #[test]
    fn system_prompt_is_redacted_and_session_cache_key_is_stable() {
        let mut config = SessionConfig::new(PathBuf::from("/tmp"), "m");
        config.system_prompt = "PASSWORD=very-secret-password-value".to_string();
        let session = Session::new(config);
        let assembled = assemble(&session, &[], &[], &[], vec![]).unwrap();
        let (body, _) = XaiClient::serialize_request(&assembled.request).unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(!body.contains("very-secret-password-value"));
        assert_eq!(
            assembled.request.prompt_cache_key.as_deref(),
            Some(session.id.as_uuid().to_string().as_str())
        );
        assert_eq!(assembled.ledger.total_bytes(), assembled.body_len);
    }

    #[test]
    fn ledger_attributes_tool_results_to_their_tool_and_path() {
        let mut session = Session::new(SessionConfig::new(PathBuf::from("/tmp"), "m"));
        session
            .history
            .push(ResponseItem::user_redacted("question", 2));
        let id = grokforge_protocol::ToolCallId::from_raw("call_read");
        session.history.push(ResponseItem::ToolCall {
            id: id.clone(),
            name: "read_file".into(),
            arguments: r#"{"path":"src/private.rs"}"#.into(),
        });
        session.history.push(ResponseItem::ToolResult {
            id,
            content: "contents".into(),
            is_error: false,
            redactions: 3,
        });
        session.history.push(ResponseItem::CompactionSummary {
            text: "prior summary".into(),
            redactions: 5,
        });
        let assembled = assemble(&session, &[], &[], &[], vec![]).unwrap();
        assert!(assembled.ledger.entries.iter().any(|entry| {
            entry.source == "tool_result:read_file:src/private.rs"
                && entry.bytes == 8
                && entry.redactions == 3
        }));
        assert!(
            assembled
                .ledger
                .entries
                .iter()
                .any(|entry| { entry.source == "history:user" && entry.redactions == 2 })
        );
        assert!(assembled.ledger.entries.iter().any(|entry| {
            entry.source == "history:compaction_summary" && entry.redactions == 5
        }));
        assert_eq!(assembled.ledger.total_bytes(), assembled.body_len);
    }

    #[test]
    fn provider_function_call_arguments_are_redacted_and_counted() {
        let mut session = Session::new(SessionConfig::new(PathBuf::from("/tmp"), "m"));
        let call_id = grokforge_protocol::ToolCallId::from_raw("call_secret");
        let secret = "abcdefghijklmnopqrstuvwxyz123456";
        let arguments = format!(
            r#"{{"command":"curl -H 'Authorization: Bearer {secret}' https://example.test"}}"#
        );
        session.history.push(ResponseItem::ProviderOutput {
            item: serde_json::json!({
                "type": "function_call",
                "id": "fc_secret",
                "call_id": call_id.as_str(),
                "name": "shell",
                "arguments": arguments,
            }),
        });
        session.history.push(ResponseItem::ToolResult {
            id: call_id,
            content: "done".to_string(),
            is_error: false,
            redactions: 0,
        });

        let assembled = assemble(&session, &[], &[], &[], vec![]).unwrap();
        let (body, _) = XaiClient::serialize_request(&assembled.request).unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(!body.contains(secret));
        assert!(body.contains("[REDACTED:bearer-token]"));
        assert!(
            assembled
                .ledger
                .entries
                .iter()
                .any(|entry| { entry.source == "provider_output:shell" && entry.redactions == 1 })
        );
        assert!(
            assembled
                .ledger
                .entries
                .iter()
                .any(|entry| entry.source == "tool_result:shell")
        );
        assert_eq!(assembled.ledger.total_bytes(), assembled.body_len);
    }

    #[test]
    fn oversized_resumed_history_is_rejected_before_egress() {
        let mut session = Session::new(SessionConfig::new(PathBuf::from("/tmp"), "m"));
        session
            .history
            .push(ResponseItem::assistant("x".repeat(33 * 1024 * 1024)));
        assert!(matches!(
            assemble(&session, &[], &[], &[], vec![]),
            Err(XaiError::RequestTooLarge { .. })
        ));
    }
}
