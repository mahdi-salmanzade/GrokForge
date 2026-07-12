//! Conversation compaction. When history grows past a threshold, older items are replaced by a
//! summary so the model-visible window stays bounded (the full transcript remains in the
//! rollout). Per the design docs, file paths and error text are extracted **mechanically** from
//! tool items — never paraphrased by the model — so they survive verbatim.

use grokforge_protocol::ResponseItem;

/// Roughly estimate the token cost of history (no bundled tokenizer; ~4 bytes/token heuristic).
#[must_use]
pub fn estimate_bytes(history: &[ResponseItem]) -> usize {
    history.iter().map(item_bytes).sum()
}

fn item_bytes(item: &ResponseItem) -> usize {
    match item {
        ResponseItem::UserMessage { text }
        | ResponseItem::AssistantMessage { text }
        | ResponseItem::Reasoning { text }
        | ResponseItem::CompactionSummary { text } => text.len(),
        ResponseItem::ToolCall { arguments, .. } => arguments.len(),
        ResponseItem::ToolResult { content, .. } => content.len(),
    }
}

/// Whether history has grown past `trigger_bytes` and there is enough to compact.
#[must_use]
pub fn should_compact(history: &[ResponseItem], trigger_bytes: usize, keep_tail: usize) -> bool {
    history.len() > keep_tail + 1 && estimate_bytes(history) > trigger_bytes
}

/// Mechanically pull verbatim file paths (from write/edit tool calls) and error text (from
/// failed tool results) out of the items being summarized, so they are never lost or reworded.
#[must_use]
pub fn extract_verbatim(items: &[ResponseItem]) -> (Vec<String>, Vec<String>) {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    for item in items {
        match item {
            ResponseItem::ToolCall {
                name, arguments, ..
            } if matches!(name.as_str(), "write_file" | "edit") => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments) {
                    if let Some(p) = v.get("path").and_then(|p| p.as_str()) {
                        if !files.contains(&p.to_string()) {
                            files.push(p.to_string());
                        }
                    }
                }
            }
            ResponseItem::ToolResult {
                content,
                is_error: true,
                ..
            } => {
                let line = content.lines().next().unwrap_or(content).to_string();
                if !line.is_empty() {
                    errors.push(line);
                }
            }
            _ => {}
        }
    }
    (files, errors)
}

/// Build the summary item that replaces a compacted range. The model's narrative summary is
/// combined with the mechanically-extracted, verbatim path/error lists.
#[must_use]
pub fn build_summary_item(
    model_summary: &str,
    files: &[String],
    errors: &[String],
) -> ResponseItem {
    let mut text = String::from("Summary of earlier conversation:\n");
    text.push_str(model_summary.trim());
    if !files.is_empty() {
        text.push_str("\n\nFiles touched (verbatim):\n");
        for f in files {
            text.push_str("- ");
            text.push_str(f);
            text.push('\n');
        }
    }
    if !errors.is_empty() {
        text.push_str("\nErrors seen (verbatim):\n");
        for e in errors {
            text.push_str("- ");
            text.push_str(e);
            text.push('\n');
        }
    }
    ResponseItem::CompactionSummary { text }
}

/// Serialize the to-be-summarized items into a plain-text transcript for the summary request.
#[must_use]
pub fn transcript_text(items: &[ResponseItem]) -> String {
    let mut out = String::new();
    for item in items {
        match item {
            ResponseItem::UserMessage { text } => {
                out.push_str("USER: ");
                out.push_str(text);
            }
            ResponseItem::AssistantMessage { text } => {
                out.push_str("ASSISTANT: ");
                out.push_str(text);
            }
            ResponseItem::ToolCall {
                name, arguments, ..
            } => {
                out.push_str("TOOL_CALL ");
                out.push_str(name);
                out.push(' ');
                out.push_str(arguments);
            }
            ResponseItem::ToolResult { content, .. } => {
                out.push_str("TOOL_RESULT: ");
                out.push_str(&content.chars().take(500).collect::<String>());
            }
            ResponseItem::Reasoning { .. } => continue,
            ResponseItem::CompactionSummary { text } => out.push_str(text),
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use grokforge_protocol::ToolCallId;

    #[test]
    fn should_compact_respects_threshold_and_tail() {
        let history = vec![
            ResponseItem::user("a".repeat(100)),
            ResponseItem::assistant("b".repeat(100)),
            ResponseItem::user("c".repeat(100)),
        ];
        assert!(should_compact(&history, 50, 1));
        assert!(!should_compact(&history, 5000, 1));
        // Not enough beyond the tail to bother.
        assert!(!should_compact(&history[..1], 1, 1));
    }

    #[test]
    fn extracts_verbatim_paths_and_errors() {
        let items = vec![
            ResponseItem::ToolCall {
                id: ToolCallId::new(),
                name: "write_file".into(),
                arguments: r#"{"path":"src/net/backoff.rs","content":"x"}"#.into(),
            },
            ResponseItem::ToolResult {
                id: ToolCallId::new(),
                content: "error: cannot find value `foo` in this scope\n  --> a.rs:3".into(),
                is_error: true,
            },
        ];
        let (files, errors) = extract_verbatim(&items);
        assert_eq!(files, vec!["src/net/backoff.rs".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("cannot find value `foo`"));
    }

    #[test]
    fn summary_item_preserves_verbatim_sections() {
        let item = build_summary_item(
            "We fixed the retry test.",
            &["src/a.rs".to_string()],
            &["error: boom".to_string()],
        );
        let ResponseItem::CompactionSummary { text } = item else {
            panic!("expected summary");
        };
        assert!(text.contains("We fixed the retry test."));
        assert!(text.contains("src/a.rs"));
        assert!(text.contains("error: boom"));
    }
}
