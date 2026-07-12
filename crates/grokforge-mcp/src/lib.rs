//! `grokforge-mcp` — MCP client (rmcp) behind an internal trait.
//!
//! Stub: implemented in M9. See docs/design/03-roadmap.md.

/// Crate version, surfaced in `grokforge doctor`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_has_version() {
        assert!(!super::VERSION.is_empty());
    }
}
