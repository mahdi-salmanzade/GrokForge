//! AGENTS.md discovery. For M2 we read the workspace-root `AGENTS.md`; nested and
//! user-global docs join the chain in a later milestone.

use std::path::{Path, PathBuf};

const MAX_AGENTS_MD_BYTES: usize = 256 * 1024;

/// A discovered project-context document.
#[derive(Debug, Clone)]
pub struct AgentsDoc {
    pub path: PathBuf,
    pub content: String,
}

/// Find AGENTS.md files that apply to `workspace_root`.
#[must_use]
pub fn discover(workspace_root: &Path) -> Vec<AgentsDoc> {
    let mut docs = Vec::new();
    let candidate = workspace_root.join("AGENTS.md");
    // Automatic outbound context uses a descriptor-relative, no-follow read. A background
    // sandbox process cannot swap this file to a symlink between validation and open, and a
    // hard-link alias cannot smuggle an outside inode under the expected filename.
    let Ok((content, truncated)) = crate::path_safety::read_workspace_context_text(
        workspace_root,
        &candidate,
        MAX_AGENTS_MD_BYTES,
    ) else {
        tracing::warn!(
            path = %candidate.display(),
            "ignoring unreadable, linked, or non-text AGENTS.md"
        );
        return docs;
    };
    if truncated {
        tracing::warn!(path = %candidate.display(), "ignoring oversized AGENTS.md");
        return docs;
    }
    docs.push(AgentsDoc {
        path: candidate,
        content,
    });
    docs
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[cfg(unix)]
    #[test]
    fn discovers_workspace_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "project rules").unwrap();
        let docs = discover(dir.path());
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content, "project rules");
    }

    #[cfg(unix)]
    #[test]
    fn ignores_agents_md_symlink_outside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "private outside data").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("AGENTS.md")).unwrap();
        assert!(discover(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn ignores_agents_md_hard_link_alias() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "private outside data").unwrap();
        std::fs::hard_link(outside.path(), dir.path().join("AGENTS.md")).unwrap();
        assert!(discover(dir.path()).is_empty());
    }

    #[test]
    fn ignores_oversized_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("AGENTS.md"),
            vec![b'a'; MAX_AGENTS_MD_BYTES + 1],
        )
        .unwrap();
        assert!(discover(dir.path()).is_empty());
    }
}
