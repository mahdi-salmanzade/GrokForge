//! Conversation compaction. When history grows past a threshold, older items are replaced by a
//! summary so the model-visible window stays bounded (the full transcript remains in the
//! rollout). Per the design docs, file paths and error text are extracted **mechanically** from
//! tool items — never paraphrased by the model — so they survive verbatim.

use grokforge_protocol::ResponseItem;

const MAX_VERBATIM_ENTRIES: usize = 1_024;
const MAX_VERBATIM_ENTRY_BYTES: usize = 4 * 1024;
const MAX_VERBATIM_TOTAL_BYTES: usize = 1024 * 1024;

/// Roughly estimate the token cost of history (no bundled tokenizer; ~4 bytes/token heuristic).
#[must_use]
pub fn estimate_bytes(history: &[ResponseItem]) -> usize {
    history
        .iter()
        .fold(0usize, |total, item| total.saturating_add(item_bytes(item)))
}

fn item_bytes(item: &ResponseItem) -> usize {
    match item {
        ResponseItem::UserMessage { text, .. }
        | ResponseItem::AssistantMessage { text }
        | ResponseItem::Reasoning { text }
        | ResponseItem::CompactionSummary { text, .. } => text.len(),
        ResponseItem::EncryptedReasoning {
            id,
            status,
            summary,
            encrypted_content,
        } => id
            .len()
            .saturating_add(status.len())
            .saturating_add(encrypted_content.len())
            .saturating_add(serde_json::to_vec(summary).map_or(0, |bytes| bytes.len())),
        ResponseItem::ProviderOutput { item } => {
            serde_json::to_vec(item).map_or(0, |bytes| bytes.len())
        }
        // Persistence control records are never part of live history; do not recurse through an
        // untrusted manually-constructed checkpoint.
        ResponseItem::CompactionCheckpoint { .. } => 0,
        ResponseItem::ToolCall { arguments, .. } => arguments.len(),
        ResponseItem::ToolResult { content, .. } => content.len(),
    }
}

/// Whether history has grown past `trigger_bytes` and there is enough to compact.
#[must_use]
pub fn should_compact(history: &[ResponseItem], trigger_bytes: usize, _keep_tail: usize) -> bool {
    // A few provider items can each be near the 15 MiB item cap. Waiting until the item count
    // exceeds the preferred tail length would make the next 32 MiB request unrecoverable.
    history.len() > 1 && estimate_bytes(history) > trigger_bytes
}

/// Mechanically pull verbatim file paths (from write/edit tool calls) and error text (from
/// failed tool results) out of the items being summarized, so they are never lost or reworded.
#[must_use]
pub fn extract_verbatim(items: &[ResponseItem]) -> (Vec<String>, Vec<String>) {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    let mut known_files = std::collections::HashSet::new();
    let mut total_bytes = 0usize;
    for item in items {
        if files.len().saturating_add(errors.len()) >= MAX_VERBATIM_ENTRIES
            || total_bytes >= MAX_VERBATIM_TOTAL_BYTES
        {
            break;
        }
        match item {
            ResponseItem::ToolCall {
                name, arguments, ..
            } if matches!(name.as_str(), "write_file" | "edit") => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments)
                    && let Some(p) = v.get("path").and_then(|p| p.as_str())
                {
                    let path = bounded_fragment(p, MAX_VERBATIM_ENTRY_BYTES);
                    if known_files.insert(path.clone()) {
                        total_bytes = total_bytes.saturating_add(path.len());
                        files.push(path);
                    }
                }
            }
            ResponseItem::ToolResult {
                content,
                is_error: true,
                ..
            } => {
                let line = bounded_fragment(
                    content.lines().next().unwrap_or(content),
                    MAX_VERBATIM_ENTRY_BYTES,
                );
                if !line.is_empty() {
                    total_bytes = total_bytes.saturating_add(line.len());
                    errors.push(line);
                }
            }
            ResponseItem::ProviderOutput { item }
                if item.get("type").and_then(serde_json::Value::as_str)
                    == Some("function_call") =>
            {
                let name = item
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if matches!(name, "write_file" | "edit") {
                    let arguments = item
                        .get("arguments")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments)
                        && let Some(path) = value.get("path").and_then(serde_json::Value::as_str)
                    {
                        let path = bounded_fragment(path, MAX_VERBATIM_ENTRY_BYTES);
                        if known_files.insert(path.clone()) {
                            total_bytes = total_bytes.saturating_add(path.len());
                            files.push(path);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    (files, errors)
}

fn bounded_fragment(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

/// Build the summary item that replaces a compacted range. The model's narrative summary is
/// combined with the mechanically-extracted, verbatim path/error lists.
#[must_use]
pub fn build_summary_item(
    model_summary: &str,
    files: &[String],
    errors: &[String],
    redactions: usize,
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
    ResponseItem::CompactionSummary { text, redactions }
}

/// Aggregate persisted redaction provenance for items replaced by a compaction summary.
#[must_use]
pub fn redaction_count(items: &[ResponseItem]) -> usize {
    items.iter().fold(0usize, |count, item| {
        count.saturating_add(match item {
            ResponseItem::UserMessage { redactions, .. }
            | ResponseItem::ToolResult { redactions, .. }
            | ResponseItem::CompactionSummary { redactions, .. } => *redactions,
            _ => 0,
        })
    })
}

/// Serialize the to-be-summarized items into a plain-text transcript for the summary request.
#[must_use]
pub fn transcript_text(items: &[ResponseItem]) -> String {
    transcript_prefix(items, usize::MAX)
}

/// Serialize bounded chronological excerpts from the history. Space is reserved for the latest
/// user request and the most recent post-user items, so one huge early item cannot erase the task
/// being compacted. Every segment is bounded while writing; no oversized intermediate is built.
#[must_use]
pub fn transcript_text_bounded(items: &[ResponseItem], max_bytes: usize) -> String {
    const HEAD: &str = "[earliest transcript excerpt]\n";
    const USER: &str = "\n[latest user request excerpt]\n";
    const TAIL: &str = "\n[most recent transcript excerpt]\n";
    let labels = HEAD
        .len()
        .saturating_add(USER.len())
        .saturating_add(TAIL.len());
    if max_bytes <= labels.saturating_add(32) {
        return transcript_prefix(items, max_bytes);
    }

    let latest_user = items
        .iter()
        .rposition(|item| matches!(item, ResponseItem::UserMessage { .. }));
    let available = max_bytes - labels;
    let head_budget = available / 4;
    let user_budget = if latest_user.is_some() {
        available / 4
    } else {
        0
    };
    let tail_budget = available
        .saturating_sub(head_budget)
        .saturating_sub(user_budget);

    let head = transcript_prefix(items, head_budget);
    let user = latest_user.map_or_else(String::new, |index| {
        transcript_prefix(&items[index..=index], user_budget)
    });
    let tail_floor = latest_user.map_or(0, |index| index.saturating_add(1));
    let tail_start = recent_transcript_start(items, tail_floor, tail_budget);
    let tail = transcript_prefix(&items[tail_start..], tail_budget);

    let mut out = String::with_capacity(max_bytes.min(64 * 1024));
    let _ = push_bounded(&mut out, HEAD, max_bytes);
    let _ = push_bounded(&mut out, &head, max_bytes);
    if !user.is_empty() {
        let _ = push_bounded(&mut out, USER, max_bytes);
        let _ = push_bounded(&mut out, &user, max_bytes);
    }
    if !tail.is_empty() {
        let _ = push_bounded(&mut out, TAIL, max_bytes);
        let _ = push_bounded(&mut out, &tail, max_bytes);
    }
    out
}

fn recent_transcript_start(items: &[ResponseItem], floor: usize, budget: usize) -> usize {
    let mut start = items.len();
    let mut bytes = 0usize;
    for index in (floor..items.len()).rev() {
        let cost = transcript_item_estimate(&items[index]).min(budget);
        if cost > 0 && start < items.len() && bytes.saturating_add(cost) > budget {
            break;
        }
        start = index;
        bytes = bytes.saturating_add(cost);
        if bytes >= budget {
            break;
        }
    }
    start
}

fn transcript_item_estimate(item: &ResponseItem) -> usize {
    match item {
        ResponseItem::UserMessage { text, .. } | ResponseItem::AssistantMessage { text } => {
            text.len().saturating_add(16)
        }
        ResponseItem::ToolCall {
            name, arguments, ..
        } => name
            .len()
            .saturating_add(arguments.len())
            .saturating_add(16),
        ResponseItem::ToolResult { content, .. } => content.len().min(2_000).saturating_add(16),
        ResponseItem::ProviderOutput { .. } => 1_024,
        ResponseItem::CompactionSummary { text, .. } => text.len(),
        ResponseItem::Reasoning { .. }
        | ResponseItem::EncryptedReasoning { .. }
        | ResponseItem::CompactionCheckpoint { .. } => 0,
    }
}

fn transcript_prefix(items: &[ResponseItem], max_bytes: usize) -> String {
    const TRUNCATED: &str = "\n[earlier transcript truncated at the compaction input limit]\n";
    let content_limit = max_bytes.saturating_sub(TRUNCATED.len());
    let mut out = String::new();
    let mut truncated = false;
    'items: for item in items {
        macro_rules! push {
            ($text:expr $(,)?) => {
                if !push_bounded(&mut out, $text, content_limit) {
                    truncated = true;
                    break 'items;
                }
            };
        }
        match item {
            ResponseItem::UserMessage { text, .. } => {
                push!("USER: ");
                push!(text);
            }
            ResponseItem::AssistantMessage { text } => {
                push!("ASSISTANT: ");
                push!(text);
            }
            ResponseItem::ToolCall {
                name, arguments, ..
            } => {
                push!("TOOL_CALL ");
                push!(name);
                push!(" ");
                push!(arguments);
            }
            ResponseItem::ToolResult { content, .. } => {
                push!("TOOL_RESULT: ");
                push!(&content.chars().take(500).collect::<String>());
            }
            ResponseItem::Reasoning { .. }
            | ResponseItem::EncryptedReasoning { .. }
            | ResponseItem::CompactionCheckpoint { .. } => continue,
            ResponseItem::ProviderOutput { item } => {
                match item.get("type").and_then(serde_json::Value::as_str) {
                    // Never send provider ciphertext to the summary request.
                    Some("message") => {
                        push!("ASSISTANT_OUTPUT: ");
                        let mut remaining = 500usize;
                        if let Some(parts) =
                            item.get("content").and_then(serde_json::Value::as_array)
                        {
                            for text in parts.iter().filter_map(|part| {
                                part.get("text").and_then(serde_json::Value::as_str)
                            }) {
                                let piece: String = text.chars().take(remaining).collect();
                                remaining = remaining.saturating_sub(piece.chars().count());
                                push!(&piece);
                                if remaining == 0 {
                                    break;
                                }
                            }
                        }
                    }
                    Some("function_call") => {
                        push!("TOOL_CALL ");
                        push!(
                            item.get("name")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("<unknown>"),
                        );
                        push!(" ");
                        if let Some(arguments) =
                            item.get("arguments").and_then(serde_json::Value::as_str)
                        {
                            push!(&arguments.chars().take(500).collect::<String>());
                        }
                    }
                    _ => continue,
                }
            }
            ResponseItem::CompactionSummary { text, .. } => push!(text),
        }
        push!("\n");
    }
    if truncated && max_bytes >= TRUNCATED.len() {
        out.push_str(TRUNCATED);
    }
    out
}

fn push_bounded(out: &mut String, value: &str, limit: usize) -> bool {
    let remaining = limit.saturating_sub(out.len());
    if value.len() <= remaining {
        out.push_str(value);
        return true;
    }
    let mut end = remaining;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    out.push_str(&value[..end]);
    false
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
        assert!(
            should_compact(&history, 50, 8),
            "a few huge items must compact even when the preferred tail is longer"
        );
        // A single item cannot be replaced by summary + tail usefully.
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
                redactions: 0,
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
            3,
        );
        let ResponseItem::CompactionSummary { text, redactions } = item else {
            panic!("expected summary");
        };
        assert!(text.contains("We fixed the retry test."));
        assert!(text.contains("src/a.rs"));
        assert!(text.contains("error: boom"));
        assert_eq!(redactions, 3);
    }

    #[test]
    fn transcript_is_bounded_while_rendering_oversized_history() {
        let history = vec![ResponseItem::user("x".repeat(2 * 1024 * 1024))];
        let transcript = transcript_text_bounded(&history, 4 * 1024);
        assert!(transcript.len() <= 4 * 1024);
        assert!(transcript.contains("transcript truncated"));
    }

    #[test]
    fn bounded_transcript_preserves_latest_user_after_huge_early_item() {
        let history = vec![
            ResponseItem::assistant("old".repeat(1024 * 1024)),
            ResponseItem::user("LATEST USER REQUEST: fix the durable bug"),
            ResponseItem::assistant("recent state"),
        ];
        let transcript = transcript_text_bounded(&history, 4 * 1024);
        assert!(transcript.len() <= 4 * 1024);
        assert!(transcript.contains("LATEST USER REQUEST: fix the durable bug"));
        assert!(transcript.contains("recent state"));
    }

    #[test]
    fn redaction_provenance_saturates_across_compacted_items() {
        let items = vec![
            ResponseItem::user_redacted("u", usize::MAX),
            ResponseItem::ToolResult {
                id: ToolCallId::from_raw("c"),
                content: "r".into(),
                is_error: false,
                redactions: 1,
            },
        ];
        assert_eq!(redaction_count(&items), usize::MAX);
    }
}
