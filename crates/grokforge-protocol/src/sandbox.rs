//! Platform-agnostic sandbox policy. The `grokforge-sandbox` crate compiles this into a
//! per-OS enforcement plan; the same struct describes the intended confinement everywhere.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How much a command is allowed to touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// Read anywhere permitted; no writes, no network.
    ReadOnly,
    /// Write inside the workspace (and temp); network denied by default.
    WorkspaceWrite,
    /// No confinement at all. The `yolo` preset.
    DangerFullAccess,
}

impl SandboxMode {
    /// Whether this mode confines anything at all.
    #[must_use]
    pub fn is_sandboxed(self) -> bool {
        !matches!(self, SandboxMode::DangerFullAccess)
    }
}

/// Network posture for sandboxed commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    /// No network access.
    Isolated,
    /// Routed through an allow-listing proxy. Reserved for a later milestone; backends
    /// treat it as unsupported in v0.1.
    ProxyRouted,
    /// Unrestricted network.
    Full,
}

/// The classification the denial classifier assigns to a blocked operation, so the UI can
/// distinguish "the sandbox stopped this" from a genuine command failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialClass {
    FsWrite,
    FsRead,
    Network,
    Signal,
}

/// A complete, platform-agnostic confinement policy for a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    /// Roots the command may write within.
    pub writable_roots: Vec<PathBuf>,
    /// Roots the command may read within (default: the whole filesystem).
    pub readable_roots: Vec<PathBuf>,
    /// Globs that must never be read, even inside a readable root (secrets).
    pub unreadable_globs: Vec<String>,
    /// Paths that are deny-write even inside a writable root (notably `.git`).
    pub protected_paths: Vec<PathBuf>,
    pub network: NetworkMode,
}

impl SandboxPolicy {
    /// A read-only policy rooted at `workspace`.
    #[must_use]
    pub fn read_only(workspace: &Path) -> Self {
        Self {
            mode: SandboxMode::ReadOnly,
            writable_roots: Vec::new(),
            readable_roots: vec![PathBuf::from("/")],
            unreadable_globs: default_secret_globs(),
            protected_paths: vec![workspace.join(".git")],
            network: NetworkMode::Isolated,
        }
    }

    /// The default policy: write inside the workspace, network off, `.git` protected.
    #[must_use]
    pub fn workspace_write(workspace: &Path) -> Self {
        Self {
            mode: SandboxMode::WorkspaceWrite,
            writable_roots: vec![workspace.to_path_buf()],
            readable_roots: vec![PathBuf::from("/")],
            unreadable_globs: default_secret_globs(),
            protected_paths: vec![workspace.join(".git")],
            network: NetworkMode::Isolated,
        }
    }

    /// No confinement.
    #[must_use]
    pub fn danger_full_access(workspace: &Path) -> Self {
        Self {
            mode: SandboxMode::DangerFullAccess,
            writable_roots: vec![PathBuf::from("/")],
            readable_roots: vec![PathBuf::from("/")],
            unreadable_globs: Vec::new(),
            protected_paths: vec![workspace.join(".git")],
            network: NetworkMode::Full,
        }
    }

    /// Whether `path` falls inside any writable root and is not a protected path.
    #[must_use]
    pub fn allows_write(&self, path: &Path) -> bool {
        if self.protected_paths.iter().any(|p| path.starts_with(p)) {
            return false;
        }
        match self.mode {
            SandboxMode::DangerFullAccess => true,
            SandboxMode::ReadOnly => false,
            SandboxMode::WorkspaceWrite => self.writable_roots.iter().any(|r| path.starts_with(r)),
        }
    }
}

/// Secret-bearing paths that are never read into context by default. Kept in one place so the
/// sandbox policy, the redactor, and the ledger's "blocked file" logic stay aligned.
#[must_use]
pub fn default_secret_globs() -> Vec<String> {
    [
        "**/.env",
        "**/.env.*",
        "**/*.pem",
        "**/*.key",
        "**/*.p12",
        "**/id_rsa",
        "**/id_ed25519",
        "**/*.pfx",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_write_confines_writes_to_workspace() {
        let ws = PathBuf::from("/home/u/proj");
        let policy = SandboxPolicy::workspace_write(&ws);
        assert!(policy.allows_write(&ws.join("src/main.rs")));
        assert!(!policy.allows_write(&PathBuf::from("/etc/passwd")));
        // .git is protected even though it is inside the workspace.
        assert!(!policy.allows_write(&ws.join(".git/config")));
    }

    #[test]
    fn read_only_forbids_all_writes() {
        let ws = PathBuf::from("/proj");
        let policy = SandboxPolicy::read_only(&ws);
        assert!(!policy.allows_write(&ws.join("a.txt")));
    }

    #[test]
    fn danger_allows_writes_but_still_protects_git() {
        let ws = PathBuf::from("/proj");
        let policy = SandboxPolicy::danger_full_access(&ws);
        assert!(policy.allows_write(&PathBuf::from("/tmp/x")));
        assert!(!policy.allows_write(&ws.join(".git/hooks/pre-commit")));
    }
}
