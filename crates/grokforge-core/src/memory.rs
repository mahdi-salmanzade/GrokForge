//! Agent-managed persistent memory. Notes the agent chooses to keep across sessions live under
//! `.grokforge/memory/` in the workspace:
//!
//! ```text
//! .grokforge/memory/
//!   MEMORY.md      # index, auto-loaded into context every session
//!   <topic>.md     # per-topic notes the agent writes with the `remember` tool
//! ```
//!
//! Only `MEMORY.md` (the index) is loaded automatically, mirroring the skills catalog: full topic
//! bodies stay local until the agent reads them through the ordinary ledgered `read_file` path.
//! Reads use the same descriptor-relative, no-follow safety as other automatic context; writes are
//! confined to the memory directory with a sanitized topic slug so a note can never escape it.

use std::path::{Path, PathBuf};

/// Workspace-relative memory directory.
pub const MEMORY_DIR: &str = ".grokforge/memory";
/// The auto-loaded index file within [`MEMORY_DIR`].
pub const MEMORY_INDEX: &str = "MEMORY.md";

const MAX_MEMORY_INDEX_BYTES: usize = 64 * 1024;
const MAX_MEMORY_FILE_BYTES: usize = 128 * 1024;
const MAX_NOTE_BYTES: usize = 16 * 1024;
const MAX_TOPIC_SLUG_LEN: usize = 48;

/// A discovered memory document (currently just the auto-loaded index).
#[derive(Debug, Clone)]
pub struct MemoryDoc {
    pub path: PathBuf,
    pub content: String,
}

/// Load the auto-context memory for `workspace_root`: the `MEMORY.md` index, if present and
/// readable. Returns an empty vector when there is no memory yet.
#[must_use]
pub fn discover(workspace_root: &Path) -> Vec<MemoryDoc> {
    let index = workspace_root.join(MEMORY_DIR).join(MEMORY_INDEX);
    let Ok((content, truncated)) = crate::path_safety::read_workspace_context_text(
        workspace_root,
        &index,
        MAX_MEMORY_INDEX_BYTES,
    ) else {
        // Missing is the common case; unreadable/linked/non-text is intentionally ignored.
        return Vec::new();
    };
    if truncated {
        tracing::warn!(path = %index.display(), "ignoring oversized MEMORY.md index");
        return Vec::new();
    }
    if content.trim().is_empty() {
        return Vec::new();
    }
    vec![MemoryDoc {
        path: index,
        content,
    }]
}

/// Turn an agent-supplied topic into a filesystem-safe slug so a note can never be written outside
/// the memory directory. Keeps ASCII alphanumerics and dashes; collapses everything else to `-`.
fn slug(topic: &str) -> Option<String> {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in topic.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= MAX_TOPIC_SLUG_LEN {
            break;
        }
    }
    let slug = out.trim_matches('-').to_string();
    if slug.is_empty() { None } else { Some(slug) }
}

/// Append `note` to the agent's memory. With no `topic`, the note goes into the `MEMORY.md` index;
/// with a `topic`, it goes into `<slug>.md` and an index link is added to `MEMORY.md` the first
/// time that topic appears. Returns a short human-readable confirmation.
///
/// # Errors
/// Returns an error string when the note is empty or too large, the topic is unusable, or the
/// write fails.
pub fn remember(workspace_root: &Path, note: &str, topic: Option<&str>) -> Result<String, String> {
    let note = note.trim();
    if note.is_empty() {
        return Err("remember requires a non-empty `note`".to_string());
    }
    if note.len() > MAX_NOTE_BYTES {
        return Err(format!(
            "note exceeds the {MAX_NOTE_BYTES}-byte memory limit; keep notes concise"
        ));
    }
    let dir = workspace_root.join(MEMORY_DIR);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("could not create memory dir: {error}"))?;

    match topic.map(str::trim).filter(|topic| !topic.is_empty()) {
        None => {
            append_section(&dir.join(MEMORY_INDEX), None, note)?;
            Ok("noted in MEMORY.md".to_string())
        }
        Some(topic) => {
            let slug = slug(topic)
                .ok_or_else(|| "topic must contain at least one letter or digit".to_string())?;
            let file = format!("{slug}.md");
            append_section(&dir.join(&file), Some(topic), note)?;
            ensure_index_link(&dir.join(MEMORY_INDEX), topic, &file)?;
            Ok(format!("noted in memory/{file}"))
        }
    }
}

/// Append a note to a memory file, creating it (with an `# <title>` header for topic files) when
/// missing. Bounded so a memory file cannot grow without limit.
fn append_section(path: &Path, title: Option<&str>, note: &str) -> Result<(), String> {
    let mut content = read_existing(path)?;
    if content.is_empty() {
        if let Some(title) = title {
            content.push_str("# ");
            content.push_str(title);
            content.push_str("\n\n");
        }
    } else if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str("- ");
    content.push_str(note);
    content.push('\n');
    write_bounded(path, &content)
}

/// Ensure `MEMORY.md` contains an index link to a topic file, adding it once under an
/// `## Index` section. Idempotent: an existing link for the file is left untouched.
fn ensure_index_link(index: &Path, topic: &str, file: &str) -> Result<(), String> {
    let mut content = read_existing(index)?;
    let link = format!("- [{topic}]({file})");
    if content.lines().any(|line| line.trim() == link.trim())
        || content.contains(&format!("]({file})"))
    {
        return Ok(());
    }
    if content.is_empty() {
        content.push_str("# Memory\n\n## Index\n");
    } else {
        if !content.ends_with('\n') {
            content.push('\n');
        }
        if !content.contains("## Index") {
            content.push_str("\n## Index\n");
        }
    }
    content.push_str(&link);
    content.push('\n');
    write_bounded(index, &content)
}

fn read_existing(path: &Path) -> Result<String, String> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes)
            .map_err(|_| "existing memory file is not valid UTF-8".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!("could not read memory file: {error}")),
    }
}

fn write_bounded(path: &Path, content: &str) -> Result<(), String> {
    if content.len() > MAX_MEMORY_FILE_BYTES {
        return Err(format!(
            "memory file would exceed the {MAX_MEMORY_FILE_BYTES}-byte limit; prune older notes"
        ));
    }
    // Refuse to follow a symlink at the memory path: memory writes stay inside the workspace tree.
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        return Err("refusing to write memory through a symlink".to_string());
    }
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, content).map_err(|error| format!("could not write memory: {error}"))?;
    std::fs::rename(&tmp, path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        format!("could not finalize memory write: {error}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_appends_to_index_and_is_discoverable() {
        let ws = tempfile::tempdir().unwrap();
        assert!(discover(ws.path()).is_empty());
        remember(ws.path(), "the build needs `source ~/.cargo/env`", None).unwrap();
        let docs = discover(ws.path());
        assert_eq!(docs.len(), 1);
        assert!(docs[0].content.contains("source ~/.cargo/env"));
    }

    #[test]
    fn topic_notes_go_to_a_slugged_file_and_are_linked_from_the_index() {
        let ws = tempfile::tempdir().unwrap();
        remember(
            ws.path(),
            "prefers concise commits",
            Some("User Preferences!"),
        )
        .unwrap();
        let topic = ws.path().join(MEMORY_DIR).join("user-preferences.md");
        assert!(topic.exists(), "topic file should be slugged");
        let index = std::fs::read_to_string(ws.path().join(MEMORY_DIR).join(MEMORY_INDEX)).unwrap();
        assert!(
            index.contains("(user-preferences.md)"),
            "index links the topic"
        );
    }

    #[test]
    fn topic_slug_cannot_escape_the_memory_directory() {
        let ws = tempfile::tempdir().unwrap();
        remember(ws.path(), "x", Some("../../etc/passwd")).unwrap();
        // The only file written is inside the memory dir; nothing escaped upward.
        assert!(!ws.path().join("etc").exists());
        let dir = ws.path().join(MEMORY_DIR);
        let escaped = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.path().to_string_lossy().contains(".."));
        assert!(!escaped);
    }

    #[test]
    fn empty_and_oversized_notes_are_rejected() {
        let ws = tempfile::tempdir().unwrap();
        assert!(remember(ws.path(), "   ", None).is_err());
        assert!(remember(ws.path(), &"x".repeat(MAX_NOTE_BYTES + 1), None).is_err());
    }
}
