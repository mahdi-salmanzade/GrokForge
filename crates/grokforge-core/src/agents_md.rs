//! AGENTS.md discovery. For M2 we read the workspace-root `AGENTS.md`; nested and
//! user-global docs join the chain in a later milestone.

use std::path::{Path, PathBuf};

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
    if let Ok(content) = std::fs::read_to_string(&candidate) {
        docs.push(AgentsDoc {
            path: candidate,
            content,
        });
    }
    docs
}
