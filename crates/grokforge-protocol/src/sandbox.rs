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
    /// Broad filesystem/network access for the `yolo` preset. A concrete policy may still carry
    /// protected paths (notably Git/session metadata) that require an enforcing wrapper.
    DangerFullAccess,
}

impl SandboxMode {
    /// Whether this mode requests the ordinary read/workspace confinement tier. This does not
    /// imply that a full-access policy has no residual protected-path requirements.
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

    /// Broad access while retaining the invariant that Git metadata is deny-write.
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
        let Some(path) = normalize_lexically(path) else {
            return false;
        };
        if self.protected_paths.iter().any(|protected| {
            normalize_lexically(protected).is_none_or(|protected| path.starts_with(protected))
        }) {
            return false;
        }
        match self.mode {
            SandboxMode::DangerFullAccess => true,
            SandboxMode::ReadOnly => false,
            SandboxMode::WorkspaceWrite => self
                .writable_roots
                .iter()
                .filter_map(|root| normalize_lexically(root))
                .any(|root| path.starts_with(root)),
        }
    }
}

/// Remove `.` and `..` components without touching the filesystem. Attempts to traverse above
/// the path's lexical root fail closed. Symlink resolution remains the responsibility of the
/// host-side file tool immediately before it performs I/O.
fn normalize_lexically(path: &Path) -> Option<PathBuf> {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    return None;
                }
                normalized.pop();
            }
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
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
        "**/id_ecdsa",
        "**/id_dsa",
        "**/*.pfx",
        "**/.netrc",
        "**/.npmrc",
        "**/.pypirc",
        "**/.git-credentials",
        "**/.grokforge/credentials.enc",
        "**/.docker/config.json",
        "**/.aws/credentials",
        "**/.aws/config",
        "**/.config/gcloud/application_default_credentials.json",
        "**/.kube/config",
        "**/credentials.json",
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
    fn workspace_write_rejects_parent_traversal_outside_workspace() {
        let policy = SandboxPolicy::workspace_write(Path::new("/home/u/proj"));
        assert!(!policy.allows_write(Path::new("/home/u/proj/../../../etc/passwd")));
    }

    #[test]
    fn parent_traversal_cannot_bypass_protected_git_path() {
        let policy = SandboxPolicy::workspace_write(Path::new("/home/u/proj"));
        assert!(!policy.allows_write(Path::new("/home/u/proj/src/../.git/config")));
    }

    #[test]
    fn empty_workspace_root_fails_closed() {
        let policy = SandboxPolicy::workspace_write(Path::new(""));
        assert!(!policy.allows_write(Path::new("relative.txt")));
    }

    #[test]
    fn default_secret_globs_cover_common_package_and_cloud_credentials() {
        let globs = default_secret_globs();
        for expected in [
            "**/.netrc",
            "**/.npmrc",
            "**/.pypirc",
            "**/.git-credentials",
            "**/.grokforge/credentials.enc",
            "**/.docker/config.json",
            "**/.aws/credentials",
            "**/.config/gcloud/application_default_credentials.json",
            "**/.kube/config",
            "**/credentials.json",
        ] {
            assert!(
                globs.iter().any(|glob| glob == expected),
                "missing {expected}"
            );
        }
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
