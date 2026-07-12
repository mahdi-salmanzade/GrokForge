//! `grokforge-sandbox` — SandboxPolicy compilation, per-OS backends (Landlock/seccomp, Seatbelt), denial classifier, and sandboxed process exec.
//!
//! Stub: implemented in M5. See docs/design/03-roadmap.md.

/// Crate version, surfaced in `grokforge doctor`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_has_version() {
        assert!(!super::VERSION.is_empty());
    }
}
