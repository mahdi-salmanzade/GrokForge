//! `grokforge-git` — gix reads; git-CLI mutations from the trusted host process; auto-commit, undo, worktrees.
//!
//! Stub: implemented in M6. See docs/design/03-roadmap.md.

/// Crate version, surfaced in `grokforge doctor`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_has_version() {
        assert!(!super::VERSION.is_empty());
    }
}
