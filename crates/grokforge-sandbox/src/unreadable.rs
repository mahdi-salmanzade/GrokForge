//! Shared bounded discovery for policy unreadable globs.

use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSetBuilder};
use grokforge_protocol::SandboxPolicy;

use crate::ExecError;

const MAX_SCAN_ENTRIES: usize = 100_000;
const MAX_SECRET_MATCHES: usize = 512;

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Find currently existing unreadable-glob matches without descending through symlinked
/// directories. Both the lexical scanned name and its resolved physical target are matched so an
/// innocent-looking workspace symlink cannot expose an outside secret path.
#[allow(clippy::too_many_lines)] // Root selection, matching, and the bounded walk share one budget.
pub(crate) fn discover_unreadable_paths(
    policy: &SandboxPolicy,
    command_cwd: &Path,
) -> Result<Vec<PathBuf>, ExecError> {
    if policy.unreadable_globs.is_empty() {
        return Ok(Vec::new());
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in &policy.unreadable_globs {
        let glob = GlobBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .map_err(|error| {
                ExecError::UnsupportedPolicy(format!(
                    "invalid unreadable glob `{pattern}`: {error}"
                ))
            })?;
        builder.add(glob);
    }
    let matcher = builder.build().map_err(|error| {
        ExecError::UnsupportedPolicy(format!("could not compile unreadable globs: {error}"))
    })?;

    let mut roots: Vec<PathBuf> = policy
        .writable_roots
        .iter()
        .map(|path| canonical(path))
        // A full-filesystem write approval cannot trigger an unbounded scan of `/`.
        .filter(|path| path != Path::new("/"))
        .collect();
    if policy
        .writable_roots
        .iter()
        .map(|path| canonical(path))
        .any(|path| path == Path::new("/"))
    {
        roots.push(canonical(command_cwd));
    }
    let command_cwd = canonical(command_cwd);
    roots.push(command_cwd.clone());
    // In read-only mode the workspace's lexical `.git` path supplies the project root. External
    // protected parents must not broaden this scan into unrelated host directories.
    roots.extend(
        policy
            .protected_paths
            .iter()
            .filter(|path| path.file_name().is_some_and(|name| name == ".git"))
            .filter_map(|path| path.parent())
            .map(canonical)
            .filter(|path| command_cwd.starts_with(path)),
    );
    roots.sort();
    roots.dedup();
    let mut confined_roots: Vec<PathBuf> = Vec::new();
    for root in &roots {
        if confined_roots.iter().any(|parent| root.starts_with(parent)) {
            continue;
        }
        confined_roots.push(root.clone());
    }
    // Exact unreadable rules for external session/worktree stores are paired with protected
    // paths. Include those paths directly without walking their unrelated parents.
    roots.extend(policy.protected_paths.iter().map(|path| canonical(path)));
    roots.sort();
    roots.dedup();
    let protected: Vec<PathBuf> = policy
        .protected_paths
        .iter()
        .map(|path| canonical(path))
        .collect();
    let mut explicitly_matched_protected: Vec<PathBuf> = policy
        .protected_paths
        .iter()
        .filter_map(|path| {
            let physical = canonical(path);
            (matcher.is_match(path) || matcher.is_match(&physical)).then_some(physical)
        })
        .collect();
    explicitly_matched_protected.sort();
    explicitly_matched_protected.dedup();

    let mut matched_paths = Vec::new();
    let mut pending = roots;
    let mut scanned = 0usize;
    while let Some(path) = pending.pop() {
        scanned = scanned.saturating_add(1);
        if scanned > MAX_SCAN_ENTRIES {
            return Err(ExecError::UnsupportedPolicy(format!(
                "unreadable-glob scan exceeded {MAX_SCAN_ENTRIES} workspace entries"
            )));
        }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(ExecError::UnsupportedPolicy(format!(
                    "could not inspect unreadable-glob path {}: {error}",
                    path.display()
                )));
            }
        };
        let physical = std::fs::canonicalize(&path).ok();
        if metadata.file_type().is_symlink()
            && let Some(target) = &physical
        {
            let target_metadata = std::fs::metadata(target).map_err(|error| {
                ExecError::UnsupportedPolicy(format!(
                    "could not inspect unreadable-glob symlink target {}: {error}",
                    target.display()
                ))
            })?;
            if target_metadata.is_dir()
                && !confined_roots.iter().any(|root| target.starts_with(root))
            {
                return Err(ExecError::UnsupportedPolicy(format!(
                    "unreadable-glob scan found directory symlink {} targeting {} outside the confined roots",
                    path.display(),
                    target.display()
                )));
            }
        }
        #[cfg(unix)]
        if metadata.is_file() {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.nlink() > 1 {
                return Err(ExecError::UnsupportedPolicy(format!(
                    "unreadable-glob scan found hard-linked file {}",
                    path.display()
                )));
            }
        }
        let is_match = matcher.is_match(&path)
            || physical.as_ref().is_some_and(|path| matcher.is_match(path))
            || explicitly_matched_protected.binary_search(&path).is_ok();
        if is_match {
            matched_paths.push(path.clone());
            if matched_paths.len() > MAX_SECRET_MATCHES {
                return Err(ExecError::UnsupportedPolicy(format!(
                    "unreadable-glob scan found more than {MAX_SECRET_MATCHES} paths"
                )));
            }
            if metadata.is_dir() {
                continue;
            }
        }
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            if protected.contains(&path) {
                continue;
            }
            let entries = std::fs::read_dir(&path).map_err(|error| {
                ExecError::UnsupportedPolicy(format!(
                    "could not enumerate unreadable-glob scan root {}: {error}",
                    path.display()
                ))
            })?;
            for entry in entries {
                pending.push(
                    entry
                        .map_err(|error| {
                            ExecError::UnsupportedPolicy(format!(
                                "could not enumerate unreadable-glob scan root {}: {error}",
                                path.display()
                            ))
                        })?
                        .path(),
                );
            }
        }
    }
    matched_paths.sort();
    matched_paths.dedup();
    Ok(matched_paths)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[cfg(unix)]
    #[test]
    fn resolved_physical_secret_target_is_discovered_without_following_directory_links() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join(".env");
        std::fs::write(&target, "private").expect("secret");
        let alias = workspace.path().join("innocent");
        std::os::unix::fs::symlink(&target, &alias).expect("symlink");
        let policy = SandboxPolicy::read_only(workspace.path());

        let matches = discover_unreadable_paths(&policy, workspace.path()).expect("discovery");
        let scanned_alias = canonical(workspace.path()).join("innocent");
        assert!(matches.contains(&scanned_alias));
        let prepared = crate::privacy::prepare_privacy_candidates(matches, workspace.path(), &[])
            .expect("prepare physical denial");
        assert!(
            prepared
                .iter()
                .any(|private| private.path == canonical(&target))
        );
    }

    #[cfg(unix)]
    #[test]
    fn external_directory_symlink_fails_closed_in_read_only_mode() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join(".env"), "private").expect("secret");
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("innocent"))
            .expect("directory symlink");
        let policy = SandboxPolicy::read_only(workspace.path());

        assert!(matches!(
            discover_unreadable_paths(&policy, workspace.path()),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("outside the confined roots")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_hardlink_alias_fails_closed_in_read_only_mode() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join(".env");
        std::fs::write(&target, "private").expect("secret");
        std::fs::hard_link(&target, workspace.path().join("innocent")).expect("hardlink");
        let policy = SandboxPolicy::read_only(workspace.path());

        assert!(matches!(
            discover_unreadable_paths(&policy, workspace.path()),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("hard-linked file")
        ));
    }

    #[test]
    fn discovery_is_ascii_case_insensitive() {
        let workspace = tempfile::tempdir().expect("workspace");
        let secret = workspace.path().join(".ENV");
        std::fs::write(&secret, "private").expect("secret");
        let policy = SandboxPolicy::read_only(workspace.path());

        let matches = discover_unreadable_paths(&policy, workspace.path()).expect("discovery");
        let scanned_secret = canonical(workspace.path()).join(".ENV");
        assert!(matches.contains(&scanned_secret));
    }
}
