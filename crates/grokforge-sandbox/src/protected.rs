//! Physical-tree validation shared by OS sandbox backends.
//!
//! A path-based deny rule protects only the name that was mounted or placed in a profile.
//! Symlinks and hard links inside a protected tree would leave another name for the same
//! content, so both Linux and macOS reject those aliases before spawning a command.

use std::path::{Path, PathBuf};

use crate::ExecError;

const MAX_PROTECTED_SCAN_ENTRIES: usize = 100_000;

/// Validate each protected path that currently exists. Missing paths are left to the backend:
/// Seatbelt can deny a future lexical path, while bubblewrap must additionally reject a missing
/// path when it sits below a writable bind mount.
pub(crate) fn validate_existing_protected_trees(
    protected_paths: &[PathBuf],
) -> Result<(), ExecError> {
    for protected in protected_paths {
        match std::fs::symlink_metadata(protected) {
            Ok(_) => validate_protected_tree(protected)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ExecError::Io(error)),
        }
    }
    Ok(())
}

pub(crate) fn validate_protected_tree(root: &Path) -> Result<(), ExecError> {
    let mut pending = vec![root.to_path_buf()];
    let mut scanned = 0usize;
    while let Some(path) = pending.pop() {
        scanned = scanned.saturating_add(1);
        if scanned > MAX_PROTECTED_SCAN_ENTRIES {
            return Err(ExecError::UnsupportedPolicy(format!(
                "protected-path scan exceeded {MAX_PROTECTED_SCAN_ENTRIES} entries"
            )));
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(ExecError::UnsupportedPolicy(format!(
                "protected tree contains symlink {} and cannot be pinned safely",
                path.display()
            )));
        }
        if metadata.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                // Do not silently omit an entry that raced with the scan or could not be read.
                // A partially validated protected tree is not safe to expose to the command.
                pending.push(entry?.path());
            }
            continue;
        }
        if !metadata.is_file() {
            return Err(ExecError::UnsupportedPolicy(format!(
                "protected tree contains non-regular entry {}",
                path.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.nlink() > 1 {
                return Err(ExecError::UnsupportedPolicy(format!(
                    "protected file {} has multiple hard links and cannot be isolated by a path rule",
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

    #[test]
    fn missing_protected_tree_is_ignored_by_shared_validation() {
        let dir = tempfile::tempdir().expect("tempdir");
        validate_existing_protected_trees(&[dir.path().join("missing")])
            .expect("backend handles missing path semantics");
    }

    #[cfg(unix)]
    #[test]
    fn aliases_in_an_external_protected_tree_fail_closed() {
        let workspace = tempfile::tempdir().expect("workspace");
        let external = tempfile::tempdir().expect("external metadata");
        let target = workspace.path().join("target");
        std::fs::write(&target, "metadata").expect("target");
        std::os::unix::fs::symlink(&target, external.path().join("config"))
            .expect("protected symlink");
        assert!(matches!(
            validate_existing_protected_trees(&[external.path().to_path_buf()]),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("symlink")
        ));

        std::fs::remove_file(external.path().join("config")).expect("remove symlink");
        std::fs::hard_link(&target, external.path().join("config")).expect("hard link");
        assert!(matches!(
            validate_existing_protected_trees(&[external.path().to_path_buf()]),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("hard links")
        ));
    }
}
