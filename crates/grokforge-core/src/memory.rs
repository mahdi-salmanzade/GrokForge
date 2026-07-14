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
//! Reads and writes use the same descriptor-relative, no-follow safety as the built-in file tools.
//! Every parent component is opened relative to the workspace descriptor, existing symlinks and
//! hard links are rejected, and replacement uses an exclusively-created temporary file. Together
//! with the sanitized topic slug, a note can never escape the workspace.

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

    match topic.map(str::trim).filter(|topic| !topic.is_empty()) {
        None => {
            append_section(workspace_root, &dir.join(MEMORY_INDEX), None, note)?;
            Ok("noted in MEMORY.md".to_string())
        }
        Some(topic) => {
            let slug = slug(topic)
                .ok_or_else(|| "topic must contain at least one letter or digit".to_string())?;
            let file = format!("{slug}.md");
            append_section(workspace_root, &dir.join(&file), Some(topic), note)?;
            ensure_index_link(workspace_root, &dir.join(MEMORY_INDEX), topic, &file)?;
            Ok(format!("noted in memory/{file}"))
        }
    }
}

/// Append a note to a memory file, creating it (with an `# <title>` header for topic files) when
/// missing. Bounded so a memory file cannot grow without limit.
fn append_section(
    workspace_root: &Path,
    path: &Path,
    title: Option<&str>,
    note: &str,
) -> Result<(), String> {
    let mut content = read_existing(workspace_root, path)?;
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
    write_bounded(workspace_root, path, &content)
}

/// Ensure `MEMORY.md` contains an index link to a topic file, adding it once under an
/// `## Index` section. Idempotent: an existing link for the file is left untouched.
fn ensure_index_link(
    workspace_root: &Path,
    index: &Path,
    topic: &str,
    file: &str,
) -> Result<(), String> {
    let mut content = read_existing(workspace_root, index)?;
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
    write_bounded(workspace_root, index, &content)
}

fn read_existing(workspace_root: &Path, path: &Path) -> Result<String, String> {
    match crate::path_safety::read_workspace_context_text(
        workspace_root,
        path,
        MAX_MEMORY_FILE_BYTES,
    ) {
        Ok((content, false)) => Ok(content),
        Ok((_, true)) => Err(format!(
            "existing memory file exceeds the {MAX_MEMORY_FILE_BYTES}-byte limit; prune older notes"
        )),
        Err(crate::path_safety::PathSafetyError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(String::new())
        }
        Err(error) => Err(format!("could not safely read memory file: {error}")),
    }
}

fn write_bounded(workspace_root: &Path, path: &Path, content: &str) -> Result<(), String> {
    if content.len() > MAX_MEMORY_FILE_BYTES {
        return Err(format!(
            "memory file would exceed the {MAX_MEMORY_FILE_BYTES}-byte limit; prune older notes"
        ));
    }
    // `remember` deliberately needs no approval, so its host write must have the same confinement
    // guarantees as `write_file`: descriptor-relative parent traversal, no-follow opens, hard-link
    // rejection, exclusive temporary creation, identity re-check, atomic rename, and directory
    // fsync. Platforms without those guarantees fail closed in `write_file_bound`.
    let policy = grokforge_protocol::SandboxPolicy::workspace_write(workspace_root);
    crate::path_safety::write_file_bound(&policy, path, None, content.as_bytes())
        .map_err(|error| format!("could not safely write memory file: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // `discover` and secure memory writes use descriptor-relative filesystem operations on Unix;
    // other platforms deliberately fail closed rather than use a link-following fallback.
    #[cfg(unix)]
    #[test]
    fn remember_appends_to_index_and_is_discoverable() {
        let ws = tempfile::tempdir().unwrap();
        assert!(discover(ws.path()).is_empty());
        remember(ws.path(), "the build needs `source ~/.cargo/env`", None).unwrap();
        let docs = discover(ws.path());
        assert_eq!(docs.len(), 1);
        assert!(docs[0].content.contains("source ~/.cargo/env"));
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn symlinked_memory_parent_cannot_redirect_a_note_outside_workspace() {
        use std::os::unix::fs::symlink;

        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(ws.path().join(".grokforge")).unwrap();
        symlink(outside.path(), ws.path().join(MEMORY_DIR)).unwrap();

        assert!(remember(ws.path(), "do not escape", None).is_err());
        assert!(!outside.path().join(MEMORY_INDEX).exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_top_level_parent_cannot_redirect_a_note_outside_workspace() {
        use std::os::unix::fs::symlink;

        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), ws.path().join(".grokforge")).unwrap();

        assert!(remember(ws.path(), "do not escape", None).is_err());
        assert!(!outside.path().join("memory").join(MEMORY_INDEX).exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_and_hard_linked_memory_targets_are_rejected() {
        use std::os::unix::fs::symlink;

        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let dir = ws.path().join(MEMORY_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join(MEMORY_INDEX);
        let victim = outside.path().join("victim.txt");
        std::fs::write(&victim, "untouched").unwrap();

        symlink(&victim, &target).unwrap();
        assert!(remember(ws.path(), "overwrite", None).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");

        std::fs::remove_file(&target).unwrap();
        std::fs::hard_link(&victim, &target).unwrap();
        assert!(remember(ws.path(), "overwrite", None).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
    }

    #[cfg(unix)]
    #[test]
    fn attacker_controlled_legacy_temp_links_are_never_opened() {
        use std::os::unix::fs::symlink;

        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let dir = ws.path().join(MEMORY_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy_temp = dir.join("MEMORY.md.tmp");
        let victim = outside.path().join("victim.txt");
        std::fs::write(&victim, "untouched").unwrap();

        symlink(&victim, &legacy_temp).unwrap();
        remember(ws.path(), "safe through symlink trap", None).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
        assert!(
            std::fs::symlink_metadata(&legacy_temp)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        std::fs::remove_file(&legacy_temp).unwrap();
        std::fs::hard_link(&victim, &legacy_temp).unwrap();
        remember(ws.path(), "safe through hard-link trap", None).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
    }

    #[cfg(not(unix))]
    #[test]
    fn remember_fails_closed_without_descriptor_safe_host_writes() {
        let ws = tempfile::tempdir().unwrap();
        assert!(remember(ws.path(), "must not use an unsafe fallback", None).is_err());
        assert!(!ws.path().join(MEMORY_DIR).join(MEMORY_INDEX).exists());
    }
}
