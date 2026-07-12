//! Bounded validation for trees exposed to a confined command.

use std::path::{Path, PathBuf};

use grokforge_protocol::SandboxPolicy;

use crate::ExecError;

const MAX_CONFINED_SCAN_ENTRIES: usize = 100_000;

/// Validate every workspace tree exposed to a confined command, including read-only policies.
/// Hard-linked regular files can expose or mutate an inode through another name, while special
/// entries preserve host IPC or device capabilities regardless of filesystem write rules.
/// Symlink targets are resolved: regular files remain governed by backend path rules, while
/// directory targets must be inside a tree this same bounded walk scans.
#[allow(clippy::too_many_lines)] // One bounded walk keeps entry accounting and fail-closed errors together.
pub(crate) fn validate_confined_trees(
    policy: &SandboxPolicy,
    command_cwd: &Path,
) -> Result<(), ExecError> {
    let command_cwd = std::fs::canonicalize(command_cwd).map_err(|error| {
        ExecError::UnsupportedPolicy(format!(
            "could not resolve command cwd {}: {error}",
            command_cwd.display()
        ))
    })?;
    let mut roots = vec![command_cwd.clone()];
    for root in &policy.writable_roots {
        let root = std::fs::canonicalize(root).map_err(|error| {
            ExecError::UnsupportedPolicy(format!(
                "could not resolve writable root {}: {error}",
                root.display()
            ))
        })?;
        // A filesystem-wide grant cannot be walked within a useful bound. The active cwd and
        // lexical project root below are the confined command's intended workspace surface.
        if root != Path::new("/") {
            roots.push(root);
        }
    }
    // Read-only policies have no writable root. Recover the intended project root from the
    // lexical `.git` protection entry when cwd is a nested directory; never use external Git or
    // private-store protected parents to broaden this scan.
    roots.extend(
        policy
            .protected_paths
            .iter()
            .filter(|path| path.file_name().is_some_and(|name| name == ".git"))
            .filter_map(|path| path.parent())
            .filter_map(|path| std::fs::canonicalize(path).ok())
            .filter(|path| command_cwd.starts_with(path)),
    );
    roots.sort();
    roots.dedup();
    let mut reduced: Vec<PathBuf> = Vec::new();
    for root in roots {
        if reduced.iter().any(|parent| root.starts_with(parent)) {
            continue;
        }
        reduced.push(root);
    }

    let confined_roots = reduced.clone();
    let mut pending = reduced;
    let mut scanned = 0usize;
    while let Some(path) = pending.pop() {
        scanned = scanned.saturating_add(1);
        if scanned > MAX_CONFINED_SCAN_ENTRIES {
            return Err(ExecError::UnsupportedPolicy(format!(
                "confined-workspace scan exceeded {MAX_CONFINED_SCAN_ENTRIES} entries"
            )));
        }
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            ExecError::UnsupportedPolicy(format!(
                "could not inspect confined path {}: {error}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                if metadata.nlink() > 1 {
                    return Err(ExecError::UnsupportedPolicy(format!(
                        "confined symlink {} has multiple hard links and cannot be confined by a path rule",
                        path.display()
                    )));
                }
            }
            let target = std::fs::canonicalize(&path).map_err(|error| {
                ExecError::UnsupportedPolicy(format!(
                    "could not resolve confined symlink {}: {error}",
                    path.display()
                ))
            })?;
            let target_metadata = std::fs::metadata(&target).map_err(|error| {
                ExecError::UnsupportedPolicy(format!(
                    "could not inspect confined symlink target {}: {error}",
                    target.display()
                ))
            })?;
            if !target_metadata.is_file() && !target_metadata.is_dir() {
                return Err(ExecError::UnsupportedPolicy(format!(
                    "confined symlink {} targets non-regular entry {}",
                    path.display(),
                    target.display()
                )));
            }
            if target_metadata.is_dir()
                && !confined_roots.iter().any(|root| target.starts_with(root))
            {
                return Err(ExecError::UnsupportedPolicy(format!(
                    "confined symlink {} targets directory {} outside the scanned confined roots",
                    path.display(),
                    target.display()
                )));
            }
            continue;
        }
        if metadata.is_dir() {
            let entries = std::fs::read_dir(&path).map_err(|error| {
                ExecError::UnsupportedPolicy(format!(
                    "could not enumerate confined directory {}: {error}",
                    path.display()
                ))
            })?;
            for entry in entries {
                pending.push(
                    entry
                        .map_err(|error| {
                            ExecError::UnsupportedPolicy(format!(
                                "could not enumerate confined directory {}: {error}",
                                path.display()
                            ))
                        })?
                        .path(),
                );
            }
            continue;
        }
        if !metadata.is_file() {
            return Err(ExecError::UnsupportedPolicy(format!(
                "confined tree contains non-regular entry {}",
                path.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.nlink() > 1 {
                return Err(ExecError::UnsupportedPolicy(format!(
                    "confined file {} has multiple hard links and cannot be confined by a path rule",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[cfg(unix)]
    #[test]
    fn read_only_empty_globs_reject_a_workspace_unix_socket() {
        use std::os::unix::net::UnixListener;

        let workspace = tempfile::tempdir().expect("workspace");
        let _listener = UnixListener::bind(workspace.path().join("control.sock")).expect("socket");
        let mut policy = SandboxPolicy::read_only(workspace.path());
        policy.unreadable_globs.clear();

        assert!(matches!(
            validate_confined_trees(&policy, workspace.path()),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("non-regular entry")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_empty_globs_reject_a_symlink_to_an_outside_socket() {
        use std::os::unix::net::UnixListener;

        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let socket = outside.path().join("control.sock");
        let _listener = UnixListener::bind(&socket).expect("socket");
        std::os::unix::fs::symlink(&socket, workspace.path().join("innocent"))
            .expect("socket symlink");
        let mut policy = SandboxPolicy::read_only(workspace.path());
        policy.unreadable_globs.clear();

        assert!(matches!(
            validate_confined_trees(&policy, workspace.path()),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("targets non-regular")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_empty_globs_reject_a_workspace_fifo() {
        let workspace = tempfile::tempdir().expect("workspace");
        let fifo = workspace.path().join("control.fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("run mkfifo");
        assert!(status.success(), "mkfifo failed: {status}");
        let mut policy = SandboxPolicy::read_only(workspace.path());
        policy.unreadable_globs.clear();

        assert!(matches!(
            validate_confined_trees(&policy, workspace.path()),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("non-regular entry")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_empty_globs_reject_a_hardlink_alias() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("target");
        std::fs::write(&target, "private").expect("target");
        std::fs::hard_link(&target, workspace.path().join("ordinary")).expect("hardlink");
        let mut policy = SandboxPolicy::read_only(workspace.path());
        policy.unreadable_globs.clear();

        assert!(matches!(
            validate_confined_trees(&policy, workspace.path()),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("multiple hard links")
        ));
    }

    #[test]
    fn ordinary_read_only_workspace_with_empty_globs_passes() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("ordinary"), "safe").expect("file");
        let mut policy = SandboxPolicy::read_only(workspace.path());
        policy.unreadable_globs.clear();

        validate_confined_trees(&policy, workspace.path()).expect("ordinary workspace");
    }
}
