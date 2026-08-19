use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Value, json};

use crate::{CodeIntelligenceError, PreparedLanguageServer};

const MAX_SESSION_BYTES: usize = 2 * 1024 * 1024;
const MAX_DIAGNOSTICS: usize = 200;
const MAX_DIAGNOSTIC_MESSAGE_CHARS: usize = 2_000;
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// Normalized LSP diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
    Unknown,
}

impl DiagnosticSeverity {
    fn from_lsp(value: Option<u64>) -> Self {
        match value {
            Some(1) => Self::Error,
            Some(2) => Self::Warning,
            Some(3) => Self::Information,
            Some(4) => Self::Hint,
            _ => Self::Unknown,
        }
    }

    /// Stable lowercase label suitable for model context and terminal rendering.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "information",
            Self::Hint => "hint",
            Self::Unknown => "diagnostic",
        }
    }
}

/// A bounded, provider-neutral subset of an LSP `Diagnostic`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// One-based line number for display.
    pub line: u64,
    /// One-based UTF-16 column for display (matching LSP's default position encoding).
    pub column: u64,
    pub end_line: u64,
    pub end_column: u64,
    pub severity: DiagnosticSeverity,
    pub code: Option<String>,
    pub source: Option<String>,
    pub message: String,
}

/// The latest `publishDiagnostics` notification for the requested document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticReport {
    pub published: bool,
    pub diagnostics: Vec<Diagnostic>,
    pub omitted: usize,
    pub server_errors: Vec<String>,
}

impl DiagnosticReport {
    /// Render a compact, byte-bounded report for the model transcript.
    #[must_use]
    pub fn render(&self, display_path: &str) -> String {
        if !self.published {
            return format!("language server did not publish diagnostics for `{display_path}`");
        }
        if self.diagnostics.is_empty() {
            return format!("no diagnostics for `{display_path}`");
        }
        let mut rendered = format!(
            "{} diagnostic(s) for `{display_path}`:\n",
            self.diagnostics.len().saturating_add(self.omitted)
        );
        for diagnostic in &self.diagnostics {
            let _ = write!(
                rendered,
                "{display_path}:{}:{}: {}",
                diagnostic.line,
                diagnostic.column,
                diagnostic.severity.as_str()
            );
            if let Some(source) = &diagnostic.source {
                let _ = write!(rendered, " [{source}");
                if let Some(code) = &diagnostic.code {
                    let _ = write!(rendered, " {code}");
                }
                rendered.push(']');
            } else if let Some(code) = &diagnostic.code {
                let _ = write!(rendered, " [{code}]");
            }
            let _ = writeln!(rendered, " {}", diagnostic.message);
        }
        if self.omitted > 0 {
            let _ = writeln!(
                rendered,
                "… [{} additional diagnostic(s) omitted] …",
                self.omitted
            );
        }
        rendered.trim_end().to_string()
    }
}

/// Build a complete, one-document stdio LSP session. Messages are framed exactly as required by
/// the protocol. The server receives initialize → initialized → didOpen. The sandbox leaves stdin
/// open for the configured bounded grace period, then closes it and reaps the one-shot server.
pub fn build_diagnostic_session(
    server: &PreparedLanguageServer,
    workspace: &Path,
    file: &Path,
    source: &str,
) -> Result<(Vec<u8>, String), CodeIntelligenceError> {
    let root_uri = url::Url::from_directory_path(workspace)
        .map_err(|()| CodeIntelligenceError::InvalidFileUri(workspace.to_path_buf()))?
        .to_string();
    let file_uri = url::Url::from_file_path(file)
        .map_err(|()| CodeIntelligenceError::InvalidFileUri(file.to_path_buf()))?
        .to_string();
    let messages = [
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": null,
                "clientInfo": { "name": "GrokForge", "version": crate::VERSION },
                "rootUri": root_uri,
                "workspaceFolders": [{ "uri": root_uri, "name": "workspace" }],
                "capabilities": {
                    "textDocument": {
                        "publishDiagnostics": { "relatedInformation": true }
                    },
                    "workspace": { "configuration": false }
                },
                "initializationOptions": server.initialization_options
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri,
                    "languageId": server.language_id,
                    "version": 1,
                    "text": source
                }
            }
        }),
    ];
    let mut session = Vec::new();
    for message in messages {
        let body = serde_json::to_vec(&message).map_err(|error| {
            CodeIntelligenceError::Validation(format!(
                "cannot encode language-server request: {error}"
            ))
        })?;
        session.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        session.extend_from_slice(&body);
        if session.len() > MAX_SESSION_BYTES {
            return Err(CodeIntelligenceError::Validation(format!(
                "language-server session exceeds the {MAX_SESSION_BYTES}-byte input limit"
            )));
        }
    }
    Ok((session, file_uri))
}

/// Parse stdio LSP frames and retain the latest diagnostics notification for `target_uri`.
pub fn parse_diagnostic_report(
    stdout: &str,
    target_uri: &str,
) -> Result<DiagnosticReport, CodeIntelligenceError> {
    let bytes = stdout.as_bytes();
    let mut cursor = 0;
    let mut found_frame = false;
    let mut report = DiagnosticReport {
        published: false,
        diagnostics: Vec::new(),
        omitted: 0,
        server_errors: Vec::new(),
    };
    while let Some(start) = find_ascii_case_insensitive(bytes, cursor, b"content-length:") {
        if start.saturating_sub(cursor) > MAX_HEADER_BYTES {
            return Err(CodeIntelligenceError::InvalidLspResponse(
                "too many non-protocol bytes before Content-Length".to_string(),
            ));
        }
        let separator = find_bytes(bytes, start, b"\r\n\r\n").ok_or_else(|| {
            CodeIntelligenceError::InvalidLspResponse(
                "Content-Length header has no CRLF terminator".to_string(),
            )
        })?;
        if separator.saturating_sub(start) > MAX_HEADER_BYTES {
            return Err(CodeIntelligenceError::InvalidLspResponse(
                "language-server header exceeds 8192 bytes".to_string(),
            ));
        }
        let headers = std::str::from_utf8(&bytes[start..separator]).map_err(|error| {
            CodeIntelligenceError::InvalidLspResponse(format!("header is not UTF-8: {error}"))
        })?;
        let length = content_length(headers)?;
        let body_start = separator.saturating_add(4);
        let body_end = body_start.checked_add(length).ok_or_else(|| {
            CodeIntelligenceError::InvalidLspResponse("Content-Length overflow".to_string())
        })?;
        if body_end > bytes.len() {
            return Err(CodeIntelligenceError::InvalidLspResponse(format!(
                "truncated frame: expected {length} body bytes, received {}",
                bytes.len().saturating_sub(body_start)
            )));
        }
        let message: Value =
            serde_json::from_slice(&bytes[body_start..body_end]).map_err(|error| {
                CodeIntelligenceError::InvalidLspResponse(format!("invalid JSON body: {error}"))
            })?;
        found_frame = true;
        consume_message(&message, target_uri, &mut report);
        cursor = body_end;
    }
    if !found_frame && !stdout.trim().is_empty() {
        return Err(CodeIntelligenceError::InvalidLspResponse(
            "stdout contained no LSP Content-Length frames".to_string(),
        ));
    }
    Ok(report)
}

fn consume_message(message: &Value, target_uri: &str, report: &mut DiagnosticReport) {
    if let Some(error) = message
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
    {
        report.server_errors.push(truncate_chars(error, 1_000));
    }
    if message.get("method").and_then(Value::as_str) != Some("textDocument/publishDiagnostics") {
        return;
    }
    let Some(params) = message.get("params") else {
        return;
    };
    if params.get("uri").and_then(Value::as_str) != Some(target_uri) {
        return;
    }
    let Some(diagnostics) = params.get("diagnostics").and_then(Value::as_array) else {
        return;
    };
    report.published = true;
    report.diagnostics.clear();
    report.omitted = diagnostics.len().saturating_sub(MAX_DIAGNOSTICS);
    report.diagnostics.extend(
        diagnostics
            .iter()
            .take(MAX_DIAGNOSTICS)
            .filter_map(parse_diagnostic),
    );
}

fn parse_diagnostic(value: &Value) -> Option<Diagnostic> {
    let start = value.get("range")?.get("start")?;
    let end = value.get("range")?.get("end")?;
    let message = value.get("message")?.as_str()?;
    Some(Diagnostic {
        line: start.get("line")?.as_u64()?.saturating_add(1),
        column: start.get("character")?.as_u64()?.saturating_add(1),
        end_line: end.get("line")?.as_u64()?.saturating_add(1),
        end_column: end.get("character")?.as_u64()?.saturating_add(1),
        severity: DiagnosticSeverity::from_lsp(value.get("severity").and_then(Value::as_u64)),
        code: value.get("code").and_then(scalar_string),
        source: value
            .get("source")
            .and_then(Value::as_str)
            .map(|source| truncate_chars(source, 128)),
        message: truncate_chars(
            &message.replace(['\r', '\n'], " "),
            MAX_DIAGNOSTIC_MESSAGE_CHARS,
        ),
    })
}

fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(truncate_chars(value, 128)),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        let mut value: String = value.chars().take(max).collect();
        value.push('…');
        value
    }
}

fn content_length(headers: &str) -> Result<usize, CodeIntelligenceError> {
    for line in headers.lines() {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            return value.trim().parse::<usize>().map_err(|error| {
                CodeIntelligenceError::InvalidLspResponse(format!(
                    "invalid Content-Length: {error}"
                ))
            });
        }
    }
    Err(CodeIntelligenceError::InvalidLspResponse(
        "missing Content-Length".to_string(),
    ))
}

fn find_bytes(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| from.saturating_add(position))
}

fn find_ascii_case_insensitive(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
        .map(|position| from.saturating_add(position))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PreparedLanguageServer;
    use std::path::PathBuf;
    use std::time::Duration;

    fn frame(message: &Value) -> String {
        let body = serde_json::to_string(&message).unwrap();
        format!("Content-Length: {}\r\n\r\n{body}", body.len())
    }

    #[test]
    fn diagnostic_session_is_framed_and_bounded() {
        let server = PreparedLanguageServer {
            program: "demo-lsp".to_string(),
            args: vec![],
            cwd: PathBuf::from("/tmp/work"),
            timeout: Duration::from_secs(1),
            language_id: "rust".to_string(),
            initialization_options: Some(json!({"check": true})),
            diagnostic_wait: Duration::from_secs(2),
            canonical_program: PathBuf::from("/tmp/demo-lsp"),
        };
        let (session, uri) = build_diagnostic_session(
            &server,
            Path::new("/tmp/work"),
            Path::new("/tmp/work/src/lib.rs"),
            "fn café() {}",
        )
        .unwrap();
        let text = String::from_utf8(session).unwrap();
        assert_eq!(text.matches("Content-Length:").count(), 3);
        assert!(text.contains("textDocument/didOpen"));
        assert!(text.contains("fn café() {}"));
        assert_eq!(uri, "file:///tmp/work/src/lib.rs");
    }

    #[test]
    fn latest_matching_diagnostics_are_normalized() {
        let uri = "file:///workspace/main.rs";
        let first = frame(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": uri, "diagnostics": []}
        }));
        let unrelated = frame(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": "file:///other.rs", "diagnostics": []}
        }));
        let latest = frame(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": uri, "diagnostics": [{
                "range": {
                    "start": {"line": 2, "character": 4},
                    "end": {"line": 2, "character": 7}
                },
                "severity": 1,
                "code": "E0308",
                "source": "rust-analyzer",
                "message": "mismatched\ntypes"
            }]}
        }));
        let report = parse_diagnostic_report(&(first + &unrelated + &latest), uri).unwrap();
        assert!(report.published);
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(report.diagnostics[0].line, 3);
        assert_eq!(report.diagnostics[0].column, 5);
        assert_eq!(report.diagnostics[0].severity, DiagnosticSeverity::Error);
        assert_eq!(report.diagnostics[0].message, "mismatched types");
        assert!(report.render("main.rs").contains("main.rs:3:5: error"));
    }

    #[test]
    fn malformed_or_truncated_frames_fail_closed() {
        let malformed = "Content-Length: 99\r\n\r\n{}";
        assert!(parse_diagnostic_report(malformed, "file:///a").is_err());
        assert!(parse_diagnostic_report("plain process output", "file:///a").is_err());
    }
}
