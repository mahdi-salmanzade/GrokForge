//! `grokforge-test-support` — Mock xAI SSE server, fixture repositories, and a PTY test harness.
//!
//! Stub: implemented in M1. See docs/design/03-roadmap.md.

/// Crate version, surfaced in `grokforge doctor`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_has_version() {
        assert!(!super::VERSION.is_empty());
    }
}
