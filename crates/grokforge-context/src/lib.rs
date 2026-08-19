//! Local code intelligence for GrokForge.
//!
//! This crate deliberately stops at process descriptions and the language-server wire format.
//! The core tool layer executes every resulting command through `grokforge-sandbox`, preserving
//! network isolation, secret masking, cancellation, output caps, and approval behavior.

mod config;
mod lsp;
mod lsp_query;
mod repo_map;

pub use config::{
    CodeIntelligenceConfig, CodeIntelligenceError, FormatterConfig, LanguageConfig, LspConfig,
    PreparedFormatter, PreparedLanguageServer, ProcessConfig,
};
pub use lsp::{
    Diagnostic, DiagnosticReport, DiagnosticSeverity, build_diagnostic_session,
    parse_diagnostic_report,
};
pub use lsp_query::{
    LspHover, LspLocation, LspPosition, LspQueryKind, LspQueryReport, LspQueryRequest,
    LspQueryResponse, LspQuerySession, LspRange, LspSymbol, build_lsp_query_session,
    parse_lsp_query_report,
};
pub use repo_map::{
    RepoMap, RepoMapError, RepoMapLimits, RepoMapOptions, RepoMapStats, build_repo_map,
};

/// Crate version, surfaced in `grokforge doctor`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_has_version() {
        assert!(!super::VERSION.is_empty());
    }
}
