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

/// Per-file attachment cap.
const MAX_ATTACH_FILE_BYTES: usize = 96 * 1024;
/// Total inlined bytes across every `@`-mention in one message.
const MAX_TOTAL_ATTACH_BYTES: usize = 384 * 1024;
/// Files read from a single `@folder` mention.
const MAX_DIR_FILES: usize = 60;
/// Directory entries scanned before a walk gives up (bounds worst-case cost).
const MAX_WALK_ENTRIES: usize = 20_000;
/// Candidates returned to the picker before ranking.
const MAX_SEARCH_CANDIDATES: usize = 8_000;

/// Expand `@path` mentions in `text` by inlining the referenced file/folder content as bounded
/// `<attachment>` blocks appended to the message. Mentions that do not resolve to a workspace file
/// or folder are left untouched (so `user@host` and literal `@` usage pass through). The original
/// text is always preserved; attachments are added after it.
#[must_use]
pub fn expand(workspace_root: &Path, text: &str) -> String {
    let mentions = parse_mentions(text);
    if mentions.is_empty() {
        return text.to_string();
    }
    let mut attachments = String::new();
    let mut used = 0usize;
    let mut seen = HashSet::new();
    for mention in mentions {
        if used >= MAX_TOTAL_ATTACH_BYTES {
            break;
        }
        if !seen.insert(mention.clone()) {
            continue;
        }
        if let Some(block) =
            read_attachment(workspace_root, &mention, MAX_TOTAL_ATTACH_BYTES - used)
        {
            used = used.saturating_add(block.len());
            attachments.push_str(&block);
        }
    }
    if attachments.is_empty() {
        return text.to_string();
    }
    format!("{text}\n\n[Attached from the message]\n{attachments}")
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
    let meta = std::fs::symlink_metadata(&absolute).ok()?;
    // Never follow a symlink mention out of the workspace; the reader is also no-follow.
    if meta.file_type().is_symlink() {
        return None;
    }
    if meta.is_file() {
        return read_one_file(workspace_root, &absolute, trimmed, budget);
    }
    if !meta.is_dir() {
        return None;
    }
    let mut out = String::new();
    let mut used = 0usize;
    let mut files = 0usize;
    for entry in ignore::WalkBuilder::new(&absolute)
        .build()
        .flatten()
        .take(MAX_WALK_ENTRIES)
    {
        if used >= budget || files >= MAX_DIR_FILES {
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(workspace_root) else {
            continue;
        };
        let relative = relative.to_string_lossy();
        if is_probably_secret(&relative) {
            continue;
        }
        if let Some(block) = read_one_file(workspace_root, entry.path(), &relative, budget - used) {
            used = used.saturating_add(block.len());
            files += 1;
            out.push_str(&block);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn read_one_file(
    workspace_root: &Path,
    absolute: &Path,
    relative: &str,
    budget: usize,
) -> Option<String> {
    let cap = budget.min(MAX_ATTACH_FILE_BYTES);
    if cap == 0 {
        return None;
    }
    let (content, truncated) =
        crate::path_safety::read_workspace_context_text(workspace_root, absolute, cap).ok()?;
    let mut block = String::with_capacity(content.len() + relative.len() + 48);
    block.push_str("<attachment path=\"");
    block.push_str(&sanitize_attr(relative));
    block.push_str("\">\n");
    block.push_str(&content);
    if truncated {
        block.push_str("\n… [attachment truncated]");
    }
    block.push_str("\n</attachment>\n");
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
