//! `@`-mention file/folder attachments. Typing `@path` in a prompt attaches that file (or the
//! files under that folder) to the message: [`expand`] inlines the content as bounded
//! `<attachment>` blocks that flow through the ordinary redaction, ledger, and context-budget
//! path. [`search_paths`] powers the interactive picker (`.gitignore`-aware, fuzzy-ranked).
//!
//! Attachment reads reuse the descriptor-relative, no-follow workspace reader, so an `@path` can
//! never follow a symlink out of the workspace, and common secret files are skipped by default —
//! redaction remains the backstop for anything inlined.

use std::collections::HashSet;
use std::path::Path;

use base64::Engine as _;
use grokforge_protocol::ImageAttachment;

/// Per-file attachment cap.
const MAX_ATTACH_FILE_BYTES: usize = 96 * 1024;
/// Total inlined bytes across every `@`-mention in one message.
const MAX_TOTAL_ATTACH_BYTES: usize = 384 * 1024;
/// Per-image binary cap. xAI accepts larger images, but this keeps the durable JSONL record and
/// stateless replay comfortably below GrokForge's own request/record safety limits.
const MAX_IMAGE_FILE_BYTES: usize = 4 * 1024 * 1024;
/// Aggregate image bytes in one prompt before base64 expansion.
const MAX_TOTAL_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// Bound the number of persisted image parts even though the provider itself currently has no
/// count limit.
const MAX_IMAGES: usize = 8;
/// Files listed in a single `@folder` manifest (further files are summarized as a count).
const MAX_MANIFEST_FILES: usize = 500;
/// Directory entries scanned before a walk gives up (bounds worst-case cost).
const MAX_WALK_ENTRIES: usize = 20_000;
/// Candidates returned to the picker before ranking.
const MAX_SEARCH_CANDIDATES: usize = 8_000;

/// A prompt after safe local attachment expansion. Text files/folder manifests are inlined in
/// `text`; native image parts remain separate so the Responses API receives `input_image` rather
/// than an enormous block of base64-looking text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedPrompt {
    pub text: String,
    pub images: Vec<ImageAttachment>,
}

/// Expand `@path` mentions in `text` by inlining the referenced file/folder content as bounded
/// `<attachment>` blocks appended to the message. Mentions that do not resolve to a workspace file
/// or folder are left untouched (so `user@host` and literal `@` usage pass through). The original
/// text is always preserved; attachments are added after it.
#[must_use]
pub fn expand(workspace_root: &Path, text: &str) -> String {
    expand_multimodal(workspace_root, text).text
}

/// Expand text, folder, and supported image mentions into a native multimodal prompt. Only PNG
/// and JPEG files are accepted (matching xAI's documented image-understanding input formats),
/// and their signatures are verified instead of trusting the filename extension.
#[must_use]
pub fn expand_multimodal(workspace_root: &Path, text: &str) -> ExpandedPrompt {
    let mentions = parse_mentions(text);
    if mentions.is_empty() {
        return ExpandedPrompt {
            text: text.to_string(),
            images: Vec::new(),
        };
    }
    let mut attachments = String::new();
    let mut images = Vec::new();
    let mut used_text = 0usize;
    let mut used_image_bytes = 0usize;
    let mut seen = HashSet::new();
    for mention in mentions {
        if !seen.insert(mention.clone()) {
            continue;
        }

        if let Some(image) = read_image_attachment(
            workspace_root,
            &mention,
            MAX_TOTAL_IMAGE_BYTES.saturating_sub(used_image_bytes),
            images.len(),
        ) {
            attachments.push_str(&image.marker);
            if let Some(attachment) = image.attachment {
                used_image_bytes = used_image_bytes.saturating_add(image.raw_bytes);
                images.push(attachment);
            }
            continue;
        }

        if used_text >= MAX_TOTAL_ATTACH_BYTES {
            continue;
        }
        if let Some(block) =
            read_attachment(workspace_root, &mention, MAX_TOTAL_ATTACH_BYTES - used_text)
        {
            used_text = used_text.saturating_add(block.len());
            attachments.push_str(&block);
        }
    }
    if attachments.is_empty() {
        return ExpandedPrompt {
            text: text.to_string(),
            images,
        };
    }
    ExpandedPrompt {
        text: format!("{text}\n\n[Attached from the message]\n{attachments}"),
        images,
    }
}

struct ImageRead {
    marker: String,
    attachment: Option<ImageAttachment>,
    raw_bytes: usize,
}

/// Return `None` when this mention is not image-shaped, so ordinary text/folder expansion can
/// continue. An image-shaped but invalid/oversized file returns a visible skipped marker and no
/// bytes; it is never retried as text and never becomes a remote URL.
fn read_image_attachment(
    workspace_root: &Path,
    mention: &str,
    remaining_bytes: usize,
    image_count: usize,
) -> Option<ImageRead> {
    let trimmed = mention.trim_end_matches('/');
    let extension = Path::new(trimmed)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase)?;
    if !matches!(extension.as_str(), "png" | "jpg" | "jpeg") {
        return None;
    }
    let marker = |note: &str| {
        format!(
            "<image path=\"{}\" note=\"{}\" />\n",
            sanitize_attr(trimmed),
            note
        )
    };
    if trimmed.is_empty() || is_probably_secret(trimmed) {
        return Some(ImageRead {
            marker: marker("skipped by attachment policy"),
            attachment: None,
            raw_bytes: 0,
        });
    }
    if image_count >= MAX_IMAGES || remaining_bytes == 0 {
        return Some(ImageRead {
            marker: marker("skipped: prompt image limit reached"),
            attachment: None,
            raw_bytes: 0,
        });
    }
    let cap = remaining_bytes.min(MAX_IMAGE_FILE_BYTES);
    let absolute = workspace_root.join(trimmed);
    let Ok((bytes, truncated)) =
        crate::path_safety::read_workspace_context_bytes(workspace_root, &absolute, cap)
    else {
        return Some(ImageRead {
            marker: marker("skipped: file is not a safe workspace image"),
            attachment: None,
            raw_bytes: 0,
        });
    };
    if truncated {
        return Some(ImageRead {
            marker: marker("skipped: image exceeds GrokForge's local size limit"),
            attachment: None,
            raw_bytes: 0,
        });
    }
    let mime_type = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else {
        return Some(ImageRead {
            marker: marker("skipped: extension and image signature do not match"),
            attachment: None,
            raw_bytes: 0,
        });
    };
    let raw_bytes = bytes.len();
    let attachment = ImageAttachment {
        mime_type: mime_type.to_string(),
        base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    };
    Some(ImageRead {
        marker: format!(
            "<image path=\"{}\" mime_type=\"{mime_type}\" bytes=\"{raw_bytes}\" />\n",
            sanitize_attr(trimmed)
        ),
        attachment: Some(attachment),
        raw_bytes,
    })
}

/// Fuzzy-ranked workspace path candidates for the `@` picker. Returns relative paths
/// (`.gitignore`-aware); folders carry a trailing `/`. An empty query returns the first shallow
/// entries. `limit` caps the result count.
#[must_use]
pub fn search_paths(workspace_root: &Path, query: &str, limit: usize) -> Vec<String> {
    let query = query.trim();
    let mut candidates: Vec<String> = Vec::new();
    for entry in ignore::WalkBuilder::new(workspace_root)
        .max_depth(if query.is_empty() { Some(6) } else { None })
        .build()
        .flatten()
        .take(MAX_SEARCH_CANDIDATES)
    {
        let Ok(relative) = entry.path().strip_prefix(workspace_root) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let mut display = relative.to_string_lossy().replace('\\', "/");
        // Control characters cannot be represented by the quoted mention grammar and would also
        // corrupt a terminal palette. Leave such unusual files accessible through explicit tools.
        if display.chars().any(char::is_control) {
            continue;
        }
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            display.push('/');
        }
        candidates.push(display);
    }

    if query.is_empty() {
        candidates.sort();
        candidates.truncate(limit);
        return candidates;
    }

    let mut scored: Vec<(i32, &String)> = candidates
        .iter()
        .filter_map(|candidate| fuzzy_score(query, candidate).map(|score| (score, candidate)))
        .collect();
    // Highest score first; break ties by shorter path, then lexically for determinism.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.len().cmp(&b.1.len()))
            .then_with(|| a.1.cmp(b.1))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, path)| path.clone())
        .collect()
}

/// Extract `@path` mentions. A mention starts at `@` that begins the text or follows whitespace
/// (so `user@host` is not a mention). Unquoted mentions run to the next whitespace; quoted forms
/// (`@"path with spaces"` and `@'path with spaces'`) may contain whitespace and escape their
/// matching quote or a backslash. Trailing sentence punctuation is trimmed from unquoted paths.
fn parse_mentions(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let preceded_by_boundary = i == 0 || bytes[i - 1].is_ascii_whitespace();
        if bytes[i] == b'@' && preceded_by_boundary {
            let start = i + 1;
            if matches!(bytes.get(start), Some(b'"' | b'\'')) {
                let quote = bytes[start] as char;
                if let Some((mention, end)) = parse_quoted_mention(text, start, quote) {
                    if !mention.is_empty() {
                        out.push(mention);
                    }
                    i = end;
                    continue;
                }
                // An unterminated or malformed quoted mention is literal user text. Advance past
                // the `@` only so a later, independent mention can still be discovered.
                i += 1;
                continue;
            }
            let mut j = start;
            while j < bytes.len() && !bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let raw = &text[start..j];
            let trimmed = raw.trim_end_matches(|c: char| {
                matches!(
                    c,
                    '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\''
                )
            });
            if !trimmed.is_empty() && !trimmed.contains('@') {
                out.push(trimmed.to_string());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Decode one quoted mention. Only the matching quote and `\\` are escape sequences; preserving
/// the backslash for every other character keeps ordinary path names lossless. A closing quote
/// must be followed by whitespace, end-of-input, or ordinary sentence punctuation so malformed
/// text such as `@"file"suffix` cannot attach an unintended partial path.
fn parse_quoted_mention(text: &str, quote_at: usize, quote: char) -> Option<(String, usize)> {
    let content_start = quote_at.checked_add(quote.len_utf8())?;
    let content = text.get(content_start..)?;
    let mut decoded = String::new();
    let mut escaped = false;

    for (relative, character) in content.char_indices() {
        let absolute = content_start.checked_add(relative)?;
        if escaped {
            if character == quote || character == '\\' {
                decoded.push(character);
            } else {
                decoded.push('\\');
                decoded.push(character);
            }
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if character == quote {
            let end = absolute.checked_add(character.len_utf8())?;
            if quoted_mention_boundary(text.get(end..).and_then(|tail| tail.chars().next())) {
                return Some((decoded, end));
            }
            return None;
        }
        // Multiline quoted paths are surprising in a prompt and cannot name a normal picker item.
        if matches!(character, '\n' | '\r') || character.is_control() {
            return None;
        }
        decoded.push(character);
    }
    None
}

fn quoted_mention_boundary(next: Option<char>) -> bool {
    next.is_none_or(|character| {
        character.is_whitespace()
            || matches!(
                character,
                '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}'
            )
    })
}

/// Read one attachment mention (a file, or the files under a folder) into `<attachment>` blocks,
/// bounded by `budget`. Returns `None` when the path does not resolve, is a symlink, or is empty.
fn read_attachment(workspace_root: &Path, mention: &str, budget: usize) -> Option<String> {
    let trimmed = mention.trim_end_matches('/');
    if trimmed.is_empty() || is_probably_secret(trimmed) {
        return None;
    }
    let absolute = workspace_root.join(trimmed);
    let meta = crate::path_safety::workspace_context_metadata(workspace_root, &absolute).ok()?;
    if meta.is_file() {
        return read_one_file(workspace_root, &absolute, trimmed, budget);
    }
    if !meta.is_dir() {
        return None;
    }
    // A folder becomes a *manifest* (paths + sizes), not inlined content: dumping a large folder
    // would blow the context budget. The agent gets a map of what's there and reads the files that
    // matter with `read_file` (cheap to revisit thanks to the provider's prompt cache).
    folder_manifest(workspace_root, &absolute, trimmed, budget)
}

/// Build a `<folder>` listing of the files under `absolute` (relative paths + sizes),
/// `.gitignore`-aware and skipping common secret files, bounded to [`MAX_MANIFEST_FILES`].
fn folder_manifest(
    workspace_root: &Path,
    absolute: &Path,
    rel: &str,
    budget: usize,
) -> Option<String> {
    use std::fmt::Write as _;
    let mut entries: Vec<(String, u64)> = Vec::new();
    let mut total_files = 0usize;
    let mut total_bytes = 0u64;
    let filter_root = workspace_root.to_path_buf();
    for entry in ignore::WalkBuilder::new(absolute)
        // Recheck every candidate before descent and before rendering it. The ignore walker
        // uses paths, so a directory replaced after the initial check must still fail closed.
        .filter_entry(move |entry| {
            crate::path_safety::workspace_context_metadata(&filter_root, entry.path()).is_ok()
        })
        .build()
        .flatten()
        .take(MAX_WALK_ENTRIES)
    {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(workspace_root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if is_probably_secret(&relative) || relative.chars().any(char::is_control) {
            continue;
        }
        let Ok(metadata) =
            crate::path_safety::workspace_context_metadata(workspace_root, entry.path())
        else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let size = metadata.len();
        total_files += 1;
        total_bytes = total_bytes.saturating_add(size);
        if entries.len() < MAX_MANIFEST_FILES {
            entries.push((relative, size));
        }
    }

    let mut out = String::new();
    out.push_str("<folder path=\"");
    out.push_str(&sanitize_attr(rel));
    out.push_str("\" note=\"Listing only — read the files you need with read_file.\">\n");
    let summary = format!(
        "[{total_files} file(s), {} total]\n</folder>\n",
        human_size(total_bytes)
    );
    // Reserving the largest possible omission notice lets every row fit without splitting a
    // UTF-8 filename or leaving an unterminated block when the aggregate budget is nearly full.
    let omitted = format!("  … {total_files} more file(s) not listed\n");
    let reserved = summary.len().saturating_add(omitted.len());
    if out.len().saturating_add(reserved) > budget {
        return None;
    }
    let mut listed = 0usize;
    for (path, size) in &entries {
        let row = format!("  {path} ({})\n", human_size(*size));
        if out.len().saturating_add(row.len()).saturating_add(reserved) > budget {
            break;
        }
        out.push_str(&row);
        listed += 1;
    }
    if total_files > listed {
        let _ = writeln!(out, "  … {} more file(s) not listed", total_files - listed);
    }
    out.push_str(&summary);
    Some(out)
}

/// Compact human-readable byte size (integer math, no float precision casts).
fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if bytes >= MB {
        format!("{}.{} MB", bytes / MB, (bytes % MB) * 10 / MB)
    } else if bytes >= KB {
        format!("{}.{} KB", bytes / KB, (bytes % KB) * 10 / KB)
    } else {
        format!("{bytes} B")
    }
}

fn read_one_file(
    workspace_root: &Path,
    absolute: &Path,
    relative: &str,
    budget: usize,
) -> Option<String> {
    const TRUNCATED: &str = "\n… [attachment truncated]";
    const FOOTER: &str = "\n</attachment>\n";
    let header = format!("<attachment path=\"{}\">\n", sanitize_attr(relative));
    let overhead = header
        .len()
        .saturating_add(FOOTER.len())
        .saturating_add(TRUNCATED.len());
    let cap = budget.saturating_sub(overhead).min(MAX_ATTACH_FILE_BYTES);
    if cap == 0 {
        return None;
    }
    let (content, truncated) =
        crate::path_safety::read_workspace_context_text(workspace_root, absolute, cap).ok()?;
    let mut block = String::with_capacity(content.len().saturating_add(overhead));
    block.push_str(&header);
    block.push_str(&content);
    if truncated {
        block.push_str(TRUNCATED);
    }
    block.push_str(FOOTER);
    Some(block)
}

/// Path attribute value made safe for the `<attachment path="…">` header: no quotes, no control
/// characters that could break out of the block or corrupt the terminal.
fn sanitize_attr(path: &str) -> String {
    path.chars()
        .map(|c| if c == '"' || c.is_control() { '_' } else { c })
        .collect()
}

/// Skip common credential files by default even when explicitly mentioned, so an `@`-mention does
/// not casually inline a secret. Redaction is the backstop for anything that does get inlined.
#[allow(clippy::case_sensitive_file_extension_comparisons)] // `name` is already lowercased.
fn is_probably_secret(relative: &str) -> bool {
    let name = relative
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(relative)
        .to_ascii_lowercase();
    name == ".env"
        || name.starts_with(".env.")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
        || name.starts_with("id_rsa")
        || name.starts_with("id_dsa")
        || name.starts_with("id_ecdsa")
        || name.starts_with("id_ed25519")
}

/// Case-insensitive subsequence fuzzy score, or `None` when `query` is not a subsequence of
/// `candidate`. Rewards contiguous runs and matches at the start or just after a path separator so
/// the picker surfaces intuitive results.
fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    let cand: Vec<char> = candidate.chars().flat_map(char::to_lowercase).collect();
    let mut score = 0i32;
    let mut ci = 0usize;
    let mut prev_match: Option<usize> = None;
    for qch in query.chars().flat_map(char::to_lowercase) {
        let mut found = None;
        while ci < cand.len() {
            if cand[ci] == qch {
                found = Some(ci);
                break;
            }
            ci += 1;
        }
        let idx = found?;
        score += 1;
        if idx == 0 || matches!(cand.get(idx - 1), Some('/' | '\\' | '_' | '-' | '.')) {
            score += 3; // boundary match
        }
        if prev_match == Some(idx.wrapping_sub(1)) {
            score += 2; // contiguous with the previous match
        }
        prev_match = Some(idx);
        ci = idx + 1;
    }
    // Prefer shorter candidates and exact basename hits.
    if candidate
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|base| base.eq_ignore_ascii_case(query))
    {
        score += 8;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hi there").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join(".env"), "SECRET=abc123def456ghi789").unwrap();
        dir
    }

    #[test]
    fn parses_only_boundary_mentions() {
        assert_eq!(parse_mentions("see @src/lib.rs please"), vec!["src/lib.rs"]);
        assert_eq!(parse_mentions("@hello.txt."), vec!["hello.txt"]);
        assert!(parse_mentions("email me at a@b.com").is_empty());
        assert!(parse_mentions("no mentions here").is_empty());
    }

    #[test]
    fn parses_quoted_mentions_with_spaces_and_escaped_quotes() {
        assert_eq!(
            parse_mentions(r#"compare @"docs/first draft.md" please"#),
            vec!["docs/first draft.md"]
        );
        assert_eq!(
            parse_mentions(r"open @'docs/it\'s ready.md' now"),
            vec!["docs/it's ready.md"]
        );
        assert_eq!(
            parse_mentions(r#"open @"docs/a\"quote.md" now"#),
            vec!["docs/a\"quote.md"]
        );
        assert_eq!(
            parse_mentions(r#"open @"docs/a\\b.md" now"#),
            vec![r"docs/a\b.md"]
        );
    }

    #[test]
    fn malformed_quoted_mentions_stay_literal() {
        assert!(parse_mentions(r#"open @"unfinished path.md"#).is_empty());
        assert!(parse_mentions(r#"open @"file.md"suffix"#).is_empty());
        assert!(parse_mentions("open @\"line\nfeed\"").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn expand_inlines_file_content_and_leaves_unknown_mentions() {
        let dir = ws();
        let out = expand(dir.path(), "explain @src/lib.rs and @nope.rs");
        assert!(out.contains("<attachment path=\"src/lib.rs\">"));
        assert!(out.contains("fn main() {}"));
        // The unresolved mention stays as literal text.
        assert!(out.contains("@nope.rs"));
        // Original text is preserved.
        assert!(out.starts_with("explain @src/lib.rs and @nope.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn expand_inlines_quoted_paths_with_spaces_and_quotes() {
        let dir = ws();
        std::fs::write(dir.path().join("notes with spaces.txt"), "spaced content").unwrap();
        std::fs::write(dir.path().join("quoted\"name.txt"), "quoted content").unwrap();

        let out = expand(
            dir.path(),
            r#"explain @"notes with spaces.txt" and @"quoted\"name.txt""#,
        );
        assert!(out.contains("spaced content"));
        assert!(out.contains("quoted content"));
        assert!(out.contains("<attachment path=\"notes with spaces.txt\">"));
        // Attribute sanitization remains the terminal/XML-like block boundary backstop.
        assert!(out.contains("<attachment path=\"quoted_name.txt\">"));
    }

    #[cfg(unix)]
    #[test]
    fn expand_skips_secret_files() {
        let dir = ws();
        let out = expand(dir.path(), "check @.env");
        assert!(!out.contains("SECRET=abc123"));
        assert!(!out.contains("<attachment"));
    }

    #[test]
    fn expand_without_mentions_is_identity() {
        let dir = ws();
        assert_eq!(
            expand(dir.path(), "just a normal message"),
            "just a normal message"
        );
    }

    #[cfg(unix)]
    #[test]
    fn png_and_jpeg_mentions_become_native_bounded_image_parts() {
        let dir = ws();
        std::fs::write(
            dir.path().join("screen.png"),
            b"\x89PNG\r\n\x1a\nsmall-test-image",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("photo.jpg"),
            b"\xff\xd8\xffsmall-test-image",
        )
        .unwrap();

        let expanded = expand_multimodal(dir.path(), "inspect @screen.png and @photo.jpg");
        assert_eq!(expanded.images.len(), 2);
        assert_eq!(expanded.images[0].mime_type, "image/png");
        assert_eq!(expanded.images[1].mime_type, "image/jpeg");
        assert!(expanded.text.contains("<image path=\"screen.png\""));
        assert!(expanded.text.contains("<image path=\"photo.jpg\""));
        assert!(!expanded.text.contains(&expanded.images[0].base64));
    }

    #[cfg(unix)]
    #[test]
    fn image_extension_is_not_trusted_without_a_matching_signature() {
        let dir = ws();
        std::fs::write(dir.path().join("fake.png"), b"not actually an image").unwrap();

        let expanded = expand_multimodal(dir.path(), "inspect @fake.png");
        assert!(expanded.images.is_empty());
        assert!(expanded.text.contains("signature do not match"));
        assert!(!expanded.text.contains("not actually an image"));
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_image_mentions_are_attached_once() {
        let dir = ws();
        std::fs::write(dir.path().join("same.png"), b"\x89PNG\r\n\x1a\nimage").unwrap();

        let expanded = expand_multimodal(dir.path(), "@same.png then @same.png");
        assert_eq!(expanded.images.len(), 1);
        assert_eq!(expanded.text.matches("<image path=").count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn folder_mention_lists_files_without_inlining_content() {
        let dir = ws();
        let out = expand(dir.path(), "look at @src/");
        // A folder becomes a manifest the agent can explore, not inlined file content.
        assert!(out.contains("<folder path=\"src\""), "{out}");
        assert!(out.contains("src/lib.rs"), "{out}");
        assert!(
            !out.contains("fn main() {}"),
            "folder content must not be inlined: {out}"
        );
    }

    #[test]
    fn folder_mention_cannot_inventory_a_parent_directory() {
        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        let outside = parent.path().join("private");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("private-file.txt"), "private data").unwrap();

        let prompt = "inspect @../private/";
        assert_eq!(expand(&workspace, prompt), prompt);
    }

    #[cfg(unix)]
    #[test]
    fn folder_mention_cannot_inventory_through_a_symlinked_parent() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(outside.path().join("nested")).unwrap();
        std::fs::write(
            outside.path().join("nested/private-file.txt"),
            "private data",
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("linked")).unwrap();

        let prompt = "inspect @linked/nested/";
        assert_eq!(expand(workspace.path(), prompt), prompt);
    }

    #[cfg(unix)]
    #[test]
    fn text_attachment_blocks_include_their_envelopes_in_the_byte_budget() {
        let workspace = ws();
        std::fs::write(workspace.path().join("large.txt"), "x".repeat(1_024)).unwrap();
        let budget = 256;
        let file = read_attachment(workspace.path(), "large.txt", budget).unwrap();
        assert!(file.len() <= budget, "{} bytes exceed {budget}", file.len());
        assert!(file.ends_with("</attachment>\n"));

        for index in 0..30 {
            std::fs::write(workspace.path().join(format!("src/file-{index}.txt")), "x").unwrap();
        }
        let folder = read_attachment(workspace.path(), "src", budget).unwrap();
        assert!(
            folder.len() <= budget,
            "{} bytes exceed {budget}",
            folder.len()
        );
        assert!(folder.ends_with("</folder>\n"));
        assert!(folder.contains("more file(s) not listed"));
    }

    #[cfg(unix)]
    #[test]
    fn folder_inventory_preserves_ignore_and_private_file_policy() {
        let workspace = ws();
        std::fs::create_dir(workspace.path().join(".git")).unwrap();
        std::fs::write(workspace.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir(workspace.path().join("ignored")).unwrap();
        std::fs::write(workspace.path().join("ignored/hidden.txt"), "ignored data").unwrap();
        std::fs::write(workspace.path().join("private.key"), "private key").unwrap();
        let outside = tempfile::tempdir().unwrap();
        let private_file = outside.path().join("private.txt");
        std::fs::write(&private_file, "private data").unwrap();
        std::fs::hard_link(&private_file, workspace.path().join("hardlinked.txt")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("linked")).unwrap();

        let folder = read_attachment(workspace.path(), ".", MAX_TOTAL_ATTACH_BYTES).unwrap();
        assert!(folder.contains("src/lib.rs"), "{folder}");
        for excluded in [
            ".git",
            ".env",
            "ignored/",
            "private.key",
            "hardlinked.txt",
            "linked/",
        ] {
            assert!(!folder.contains(excluded), "listed {excluded}: {folder}");
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn folder_mentions_fail_closed_without_descriptor_safe_metadata() {
        let workspace = ws();
        let prompt = "inspect @src/";
        assert_eq!(expand(workspace.path(), prompt), prompt);
    }

    #[test]
    fn search_ranks_fuzzy_and_finds_folders() {
        let dir = ws();
        let hits = search_paths(dir.path(), "librs", 10);
        assert!(hits.iter().any(|hit| hit == "src/lib.rs"), "got: {hits:?}");
        let folders = search_paths(dir.path(), "src", 10);
        assert!(folders.iter().any(|hit| hit == "src/"), "got: {folders:?}");
    }

    #[cfg(unix)]
    #[test]
    fn search_omits_paths_that_cannot_be_safely_rendered_or_quoted() {
        let dir = ws();
        std::fs::write(dir.path().join("bad\nname.txt"), "content").unwrap();
        let hits = search_paths(dir.path(), "", 100);
        assert!(!hits.iter().any(|hit| hit.contains('\n')));
    }
}
