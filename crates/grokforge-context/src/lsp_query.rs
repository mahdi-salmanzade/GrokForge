//! Bounded one-shot Language Server Protocol queries.
//!
//! This module only constructs and parses JSON-RPC. Process execution remains in
//! `grokforge-core`, where the configured executable is reverified and run inside the active
//! networkless read-only sandbox.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::{CodeIntelligenceError, PreparedLanguageServer};

const MAX_SESSION_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_FRAMES: usize = 256;
const MAX_ITEMS: usize = 200;
const MAX_SYMBOL_DEPTH: usize = 16;
const MAX_HOVER_BYTES: usize = 12 * 1024;
const MAX_INLINE_BYTES: usize = 512;
const MAX_URI_BYTES: usize = 4 * 1024;
const QUERY_REQUEST_ID: u64 = 2;

/// Read-only LSP operations supported by the one-shot query tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspQueryKind {
    Hover,
    Definition,
    References,
    DocumentSymbols,
    WorkspaceSymbols,
    Implementation,
}

impl LspQueryKind {
    #[must_use]
    pub const fn from_name(name: &str) -> Option<Self> {
        match name.as_bytes() {
            b"hover" => Some(Self::Hover),
            b"definition" => Some(Self::Definition),
            b"references" => Some(Self::References),
            b"document_symbols" => Some(Self::DocumentSymbols),
            b"workspace_symbols" => Some(Self::WorkspaceSymbols),
            b"implementation" => Some(Self::Implementation),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hover => "hover",
            Self::Definition => "definition",
            Self::References => "references",
            Self::DocumentSymbols => "document_symbols",
            Self::WorkspaceSymbols => "workspace_symbols",
            Self::Implementation => "implementation",
        }
    }

    const fn method(self) -> &'static str {
        match self {
            Self::Hover => "textDocument/hover",
            Self::Definition => "textDocument/definition",
            Self::References => "textDocument/references",
            Self::DocumentSymbols => "textDocument/documentSymbol",
            Self::WorkspaceSymbols => "workspace/symbol",
            Self::Implementation => "textDocument/implementation",
        }
    }

    #[must_use]
    pub const fn requires_position(self) -> bool {
        matches!(
            self,
            Self::Hover | Self::Definition | Self::References | Self::Implementation
        )
    }

    #[must_use]
    pub const fn requires_query(self) -> bool {
        matches!(self, Self::WorkspaceSymbols)
    }
}

/// Zero-based UTF-16 LSP position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LspPosition {
    pub line: u64,
    pub character: u64,
}

/// Inputs needed to construct one LSP request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspQueryRequest {
    pub kind: LspQueryKind,
    pub position: Option<LspPosition>,
    pub query: Option<String>,
}

/// Fully framed stdin for one sandboxed language-server process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspQuerySession {
    pub stdin: Vec<u8>,
    pub target_uri: String,
    pub request_id: u64,
}

/// One-based display range normalized from an LSP range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LspRange {
    pub line: u64,
    pub column: u64,
    pub end_line: u64,
    pub end_column: u64,
}

/// Location or location-link target returned by an LSP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspLocation {
    pub uri: String,
    pub range: Option<LspRange>,
}

impl LspLocation {
    /// Convert a file URI to a local path. Non-file schemes and malformed URIs are rejected.
    #[must_use]
    pub fn file_path(&self) -> Option<PathBuf> {
        let uri = url::Url::parse(&self.uri).ok()?;
        (uri.scheme() == "file").then_some(())?;
        uri.to_file_path().ok()
    }
}

/// Terminal-safe hover text and optional source range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspHover {
    pub contents: String,
    pub range: Option<LspRange>,
}

/// A flattened document/workspace symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspSymbol {
    pub name: String,
    pub kind: &'static str,
    pub detail: Option<String>,
    pub container: Option<String>,
    pub location: LspLocation,
    pub depth: usize,
}

/// Normalized response shapes for supported operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspQueryResponse {
    Hover(Option<LspHover>),
    Locations(Vec<LspLocation>),
    Symbols(Vec<LspSymbol>),
}

/// The response for request id 2, plus bounded-parser omissions and a sanitized server error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspQueryReport {
    pub response: Option<LspQueryResponse>,
    pub omitted: usize,
    pub server_error: Option<String>,
}

/// Construct initialize → initialized → didOpen → query → didClose → shutdown → exit frames.
/// The one-shot sandbox runner writes these frames in order, leaves stdin open for a bounded grace
/// period, then closes stdin and reaps the server.
pub fn build_lsp_query_session(
    server: &PreparedLanguageServer,
    workspace: &Path,
    file: &Path,
    source: &str,
    request: &LspQueryRequest,
) -> Result<LspQuerySession, CodeIntelligenceError> {
    validate_request(request)?;
    let root_uri = url::Url::from_directory_path(workspace)
        .map_err(|()| CodeIntelligenceError::InvalidFileUri(workspace.to_path_buf()))?
        .to_string();
    let file_uri = url::Url::from_file_path(file)
        .map_err(|()| CodeIntelligenceError::InvalidFileUri(file.to_path_buf()))?
        .to_string();
    let query_params = query_params(request, &file_uri)?;
    let messages = vec![
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
                    "general": { "positionEncodings": ["utf-16"] },
                    "textDocument": {
                        "hover": { "contentFormat": ["markdown", "plaintext"] },
                        "definition": { "linkSupport": true },
                        "references": {},
                        "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                        "implementation": { "linkSupport": true }
                    },
                    "workspace": { "configuration": false, "symbol": {} }
                },
                "initializationOptions": server.initialization_options
            }
        }),
        json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
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
        json!({
            "jsonrpc": "2.0",
            "id": QUERY_REQUEST_ID,
            "method": request.kind.method(),
            "params": query_params
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didClose",
            "params": { "textDocument": { "uri": file_uri } }
        }),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": null }),
        json!({ "jsonrpc": "2.0", "method": "exit", "params": null }),
    ];
    let mut stdin = Vec::new();
    for message in messages {
        frame(&message, &mut stdin)?;
    }
    Ok(LspQuerySession {
        stdin,
        target_uri: file_uri,
        request_id: QUERY_REQUEST_ID,
    })
}

fn validate_request(request: &LspQueryRequest) -> Result<(), CodeIntelligenceError> {
    if request.kind.requires_position() != request.position.is_some() {
        return Err(CodeIntelligenceError::Validation(format!(
            "LSP operation `{}` {} a position",
            request.kind.as_str(),
            if request.kind.requires_position() {
                "requires"
            } else {
                "does not accept"
            }
        )));
    }
    if request.kind.requires_query() {
        let query = request.query.as_deref().unwrap_or_default();
        if query.is_empty() || query.len() > 512 {
            return Err(CodeIntelligenceError::Validation(
                "workspace_symbols requires a non-empty query of at most 512 bytes".to_string(),
            ));
        }
    } else if request.query.is_some() {
        return Err(CodeIntelligenceError::Validation(format!(
            "LSP operation `{}` does not accept a query",
            request.kind.as_str()
        )));
    }
    Ok(())
}

fn query_params(request: &LspQueryRequest, file_uri: &str) -> Result<Value, CodeIntelligenceError> {
    if request.kind.requires_position() {
        let position = request
            .position
            .ok_or_else(|| CodeIntelligenceError::Validation("missing LSP position".to_string()))?;
        let mut params = json!({
            "textDocument": { "uri": file_uri },
            "position": { "line": position.line, "character": position.character }
        });
        if request.kind == LspQueryKind::References {
            params["context"] = json!({ "includeDeclaration": true });
        }
        return Ok(params);
    }
    match request.kind {
        LspQueryKind::DocumentSymbols => Ok(json!({ "textDocument": { "uri": file_uri } })),
        LspQueryKind::WorkspaceSymbols => Ok(json!({
            "query": request.query.as_deref().unwrap_or_default()
        })),
        _ => Err(CodeIntelligenceError::Validation(
            "invalid LSP query parameters".to_string(),
        )),
    }
}

fn frame(message: &Value, session: &mut Vec<u8>) -> Result<(), CodeIntelligenceError> {
    let body = serde_json::to_vec(message).map_err(|error| {
        CodeIntelligenceError::Validation(format!("cannot encode language-server request: {error}"))
    })?;
    let next_len = session.len().saturating_add(body.len()).saturating_add(64);
    if next_len > MAX_SESSION_BYTES {
        return Err(CodeIntelligenceError::Validation(format!(
            "language-server session exceeds the {MAX_SESSION_BYTES}-byte input limit"
        )));
    }
    session.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    session.extend_from_slice(&body);
    Ok(())
}

/// Parse the response matching `request_id` and normalize its operation-specific shape.
pub fn parse_lsp_query_report(
    stdout: &str,
    kind: LspQueryKind,
    request_id: u64,
    target_uri: &str,
) -> Result<LspQueryReport, CodeIntelligenceError> {
    let frames = parse_frames(stdout)?;
    let mut matching = frames.into_iter().filter(|message| {
        message
            .get("id")
            .and_then(Value::as_u64)
            .is_some_and(|id| id == request_id)
    });
    let Some(message) = matching.next() else {
        return Ok(LspQueryReport {
            response: None,
            omitted: 0,
            server_error: None,
        });
    };
    if matching.next().is_some() {
        return Err(CodeIntelligenceError::InvalidLspResponse(
            "language server returned duplicate responses for the query".to_string(),
        ));
    }
    if let Some(error) = message.get("error") {
        let text = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("language server returned an unspecified error");
        return Ok(LspQueryReport {
            response: None,
            omitted: 0,
            server_error: Some(safe_inline(text, 1_000)),
        });
    }
    let result = message.get("result").ok_or_else(|| {
        CodeIntelligenceError::InvalidLspResponse(
            "query response contains neither result nor error".to_string(),
        )
    })?;
    let (response, omitted) = match kind {
        LspQueryKind::Hover => (LspQueryResponse::Hover(parse_hover(result)?), 0),
        LspQueryKind::Definition | LspQueryKind::References | LspQueryKind::Implementation => {
            let (locations, omitted) = parse_locations(result)?;
            (LspQueryResponse::Locations(locations), omitted)
        }
        LspQueryKind::DocumentSymbols => {
            let (symbols, omitted) = parse_document_symbols(result, target_uri)?;
            (LspQueryResponse::Symbols(symbols), omitted)
        }
        LspQueryKind::WorkspaceSymbols => {
            let (symbols, omitted) = parse_workspace_symbols(result)?;
            (LspQueryResponse::Symbols(symbols), omitted)
        }
    };
    Ok(LspQueryReport {
        response: Some(response),
        omitted,
        server_error: None,
    })
}

fn parse_frames(stdout: &str) -> Result<Vec<Value>, CodeIntelligenceError> {
    if stdout.len() > MAX_RESPONSE_BYTES {
        return Err(CodeIntelligenceError::InvalidLspResponse(format!(
            "language-server output exceeds the {MAX_RESPONSE_BYTES}-byte response limit"
        )));
    }
    let bytes = stdout.as_bytes();
    let mut cursor = 0;
    let mut frames = Vec::new();
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
        if length > MAX_RESPONSE_BYTES {
            return Err(CodeIntelligenceError::InvalidLspResponse(
                "language-server frame exceeds the response limit".to_string(),
            ));
        }
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
        let message = serde_json::from_slice(&bytes[body_start..body_end]).map_err(|error| {
            CodeIntelligenceError::InvalidLspResponse(format!("invalid JSON body: {error}"))
        })?;
        frames.push(message);
        if frames.len() > MAX_FRAMES {
            return Err(CodeIntelligenceError::InvalidLspResponse(format!(
                "language-server output exceeds the {MAX_FRAMES}-frame limit"
            )));
        }
        cursor = body_end;
    }
    if frames.is_empty() && !stdout.trim().is_empty() {
        return Err(CodeIntelligenceError::InvalidLspResponse(
            "stdout contained no LSP Content-Length frames".to_string(),
        ));
    }
    if bytes.len().saturating_sub(cursor) > MAX_HEADER_BYTES {
        return Err(CodeIntelligenceError::InvalidLspResponse(
            "too many trailing non-protocol bytes".to_string(),
        ));
    }
    Ok(frames)
}

fn parse_hover(result: &Value) -> Result<Option<LspHover>, CodeIntelligenceError> {
    if result.is_null() {
        return Ok(None);
    }
    let object = result.as_object().ok_or_else(|| {
        CodeIntelligenceError::InvalidLspResponse(
            "hover result is not an object or null".to_string(),
        )
    })?;
    let contents = object.get("contents").ok_or_else(|| {
        CodeIntelligenceError::InvalidLspResponse("hover result has no contents".to_string())
    })?;
    let contents = parse_hover_contents(contents);
    let range = object.get("range").and_then(parse_range);
    Ok(Some(LspHover { contents, range }))
}

fn parse_hover_contents(contents: &Value) -> String {
    let mut parts = Vec::new();
    match contents {
        Value::String(text) => parts.push(text.as_str()),
        Value::Object(object) => {
            if let Some(value) = object.get("value").and_then(Value::as_str) {
                parts.push(value);
            }
        }
        Value::Array(values) => {
            for value in values.iter().take(MAX_ITEMS) {
                match value {
                    Value::String(text) => parts.push(text),
                    Value::Object(object) => {
                        if let Some(value) = object.get("value").and_then(Value::as_str) {
                            parts.push(value);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    safe_multiline(&parts.join("\n\n"), MAX_HOVER_BYTES)
}

fn parse_locations(result: &Value) -> Result<(Vec<LspLocation>, usize), CodeIntelligenceError> {
    if result.is_null() {
        return Ok((Vec::new(), 0));
    }
    let values: Vec<&Value> = match result {
        Value::Array(values) => values.iter().collect(),
        Value::Object(_) => vec![result],
        _ => {
            return Err(CodeIntelligenceError::InvalidLspResponse(
                "location result is not an object, array, or null".to_string(),
            ));
        }
    };
    let mut omitted = values.len().saturating_sub(MAX_ITEMS);
    let mut locations = Vec::new();
    for value in values.into_iter().take(MAX_ITEMS) {
        if let Some(location) = parse_location(value, false) {
            locations.push(location);
        } else {
            omitted = omitted.saturating_add(1);
        }
    }
    Ok((locations, omitted))
}

fn parse_location(value: &Value, allow_missing_range: bool) -> Option<LspLocation> {
    let uri = value
        .get("uri")
        .and_then(Value::as_str)
        .or_else(|| value.get("targetUri").and_then(Value::as_str))?;
    if uri.len() > MAX_URI_BYTES || uri.chars().any(char::is_control) {
        return None;
    }
    let range_value = value
        .get("range")
        .or_else(|| value.get("targetSelectionRange"))
        .or_else(|| value.get("targetRange"));
    let range = match range_value {
        Some(range) => Some(parse_range(range)?),
        None if allow_missing_range => None,
        None => return None,
    };
    Some(LspLocation {
        uri: uri.to_string(),
        range,
    })
}

fn parse_document_symbols(
    result: &Value,
    target_uri: &str,
) -> Result<(Vec<LspSymbol>, usize), CodeIntelligenceError> {
    if result.is_null() {
        return Ok((Vec::new(), 0));
    }
    let values = result.as_array().ok_or_else(|| {
        CodeIntelligenceError::InvalidLspResponse(
            "document-symbol result is not an array or null".to_string(),
        )
    })?;
    let mut symbols = Vec::new();
    let mut omitted = 0;
    flatten_document_symbols(values, target_uri, 0, &mut symbols, &mut omitted);
    Ok((symbols, omitted))
}

fn flatten_document_symbols(
    values: &[Value],
    target_uri: &str,
    depth: usize,
    symbols: &mut Vec<LspSymbol>,
    omitted: &mut usize,
) {
    if depth > MAX_SYMBOL_DEPTH {
        *omitted = omitted.saturating_add(values.len());
        return;
    }
    for value in values {
        if symbols.len() >= MAX_ITEMS {
            *omitted = omitted.saturating_add(1);
            continue;
        }
        let Some(name) = value.get("name").and_then(Value::as_str) else {
            *omitted = omitted.saturating_add(1);
            continue;
        };
        let location = if let Some(location) = value
            .get("location")
            .and_then(|location| parse_location(location, false))
        {
            location
        } else {
            let Some(range) = value
                .get("selectionRange")
                .or_else(|| value.get("range"))
                .and_then(parse_range)
            else {
                *omitted = omitted.saturating_add(1);
                continue;
            };
            LspLocation {
                uri: target_uri.to_string(),
                range: Some(range),
            }
        };
        symbols.push(LspSymbol {
            name: safe_inline(name, MAX_INLINE_BYTES),
            kind: symbol_kind(value.get("kind").and_then(Value::as_u64)),
            detail: value
                .get("detail")
                .and_then(Value::as_str)
                .map(|detail| safe_inline(detail, MAX_INLINE_BYTES)),
            container: value
                .get("containerName")
                .and_then(Value::as_str)
                .map(|container| safe_inline(container, MAX_INLINE_BYTES)),
            location,
            depth,
        });
        if let Some(children) = value.get("children").and_then(Value::as_array) {
            flatten_document_symbols(
                children,
                target_uri,
                depth.saturating_add(1),
                symbols,
                omitted,
            );
        }
    }
}

fn parse_workspace_symbols(
    result: &Value,
) -> Result<(Vec<LspSymbol>, usize), CodeIntelligenceError> {
    if result.is_null() {
        return Ok((Vec::new(), 0));
    }
    let values = result.as_array().ok_or_else(|| {
        CodeIntelligenceError::InvalidLspResponse(
            "workspace-symbol result is not an array or null".to_string(),
        )
    })?;
    let mut omitted = values.len().saturating_sub(MAX_ITEMS);
    let mut symbols = Vec::new();
    for value in values.iter().take(MAX_ITEMS) {
        let Some(name) = value.get("name").and_then(Value::as_str) else {
            omitted = omitted.saturating_add(1);
            continue;
        };
        let Some(location) = value
            .get("location")
            .and_then(|location| parse_location(location, true))
        else {
            omitted = omitted.saturating_add(1);
            continue;
        };
        symbols.push(LspSymbol {
            name: safe_inline(name, MAX_INLINE_BYTES),
            kind: symbol_kind(value.get("kind").and_then(Value::as_u64)),
            detail: value
                .get("detail")
                .and_then(Value::as_str)
                .map(|detail| safe_inline(detail, MAX_INLINE_BYTES)),
            container: value
                .get("containerName")
                .and_then(Value::as_str)
                .map(|container| safe_inline(container, MAX_INLINE_BYTES)),
            location,
            depth: 0,
        });
    }
    Ok((symbols, omitted))
}

fn parse_range(value: &Value) -> Option<LspRange> {
    let start = value.get("start")?;
    let end = value.get("end")?;
    Some(LspRange {
        line: start.get("line")?.as_u64()?.saturating_add(1),
        column: start.get("character")?.as_u64()?.saturating_add(1),
        end_line: end.get("line")?.as_u64()?.saturating_add(1),
        end_column: end.get("character")?.as_u64()?.saturating_add(1),
    })
}

const fn symbol_kind(kind: Option<u64>) -> &'static str {
    match kind {
        Some(1) => "file",
        Some(2) => "module",
        Some(3) => "namespace",
        Some(4) => "package",
        Some(5) => "class",
        Some(6) => "method",
        Some(7) => "property",
        Some(8) => "field",
        Some(9) => "constructor",
        Some(10) => "enum",
        Some(11) => "interface",
        Some(12) => "function",
        Some(13) => "variable",
        Some(14) => "constant",
        Some(15) => "string",
        Some(16) => "number",
        Some(17) => "boolean",
        Some(18) => "array",
        Some(19) => "object",
        Some(20) => "key",
        Some(21) => "null",
        Some(22) => "enum-member",
        Some(23) => "struct",
        Some(24) => "event",
        Some(25) => "operator",
        Some(26) => "type-parameter",
        _ => "symbol",
    }
}

fn safe_inline(value: &str, max_bytes: usize) -> String {
    let value: String = value
        .chars()
        .map(|character| {
            if character.is_control() || character == '\u{7f}' {
                ' '
            } else {
                character
            }
        })
        .collect();
    truncate_utf8(
        value.split_whitespace().collect::<Vec<_>>().join(" "),
        max_bytes,
    )
}

fn safe_multiline(value: &str, max_bytes: usize) -> String {
    let mut output = String::new();
    for character in value.chars() {
        match character {
            '\n' | '\t' => output.push(character),
            '\r' => output.push('\n'),
            character if character.is_control() || character == '\u{7f}' => output.push(' '),
            character => output.push(character),
        }
    }
    truncate_utf8(output, max_bytes)
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let marker = "…";
    let mut boundary = max_bytes.saturating_sub(marker.len()).min(value.len());
    while !value.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    value.truncate(boundary);
    value.push_str(marker);
    value
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
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;

    fn server() -> PreparedLanguageServer {
        PreparedLanguageServer {
            program: "demo-lsp".to_string(),
            args: vec![],
            cwd: PathBuf::from("/tmp/work"),
            timeout: Duration::from_secs(1),
            language_id: "rust".to_string(),
            initialization_options: Some(json!({"check": true})),
            diagnostic_wait: Duration::from_secs(2),
            canonical_program: PathBuf::from("/tmp/demo-lsp"),
        }
    }

    fn response(result: &Value) -> String {
        let message = json!({"jsonrpc":"2.0", "id": QUERY_REQUEST_ID, "result": result});
        let body = serde_json::to_string(&message).unwrap();
        format!("Content-Length: {}\r\n\r\n{body}", body.len())
    }

    fn range(line: u64, character: u64) -> Value {
        json!({
            "start": {"line": line, "character": character},
            "end": {"line": line, "character": character + 1}
        })
    }

    #[test]
    fn query_session_has_all_framed_lifecycle_messages() {
        let request = LspQueryRequest {
            kind: LspQueryKind::References,
            position: Some(LspPosition {
                line: 2,
                character: 4,
            }),
            query: None,
        };
        let session = build_lsp_query_session(
            &server(),
            Path::new("/tmp/work"),
            Path::new("/tmp/work/src/lib.rs"),
            "fn café() {}",
            &request,
        )
        .unwrap();
        let text = String::from_utf8(session.stdin).unwrap();
        assert_eq!(text.matches("Content-Length:").count(), 7);
        for method in [
            "initialize",
            "initialized",
            "textDocument/didOpen",
            "textDocument/references",
            "textDocument/didClose",
            "shutdown",
            "exit",
        ] {
            assert!(text.contains(&format!("\"method\":\"{method}\"")));
        }
        assert!(text.contains("\"includeDeclaration\":true"));
        assert_eq!(session.request_id, QUERY_REQUEST_ID);
    }

    #[test]
    fn parses_hover_marked_strings_and_sanitizes_controls() {
        let report = parse_lsp_query_report(
            &response(&json!({
                "contents": [
                    {"language":"rust", "value":"fn forge()\u{001b}[31m"},
                    "documentation"
                ],
                "range": range(1, 2)
            })),
            LspQueryKind::Hover,
            QUERY_REQUEST_ID,
            "file:///workspace/main.rs",
        )
        .unwrap();
        let Some(LspQueryResponse::Hover(Some(hover))) = report.response else {
            panic!("unexpected response")
        };
        assert!(hover.contents.contains("fn forge() [31m"));
        assert!(!hover.contents.contains('\u{1b}'));
        assert_eq!(hover.range.unwrap().line, 2);
    }

    #[test]
    fn parses_location_and_location_link_shapes() {
        let report = parse_lsp_query_report(
            &response(&json!([
                {"uri":"file:///workspace/a.rs", "range": range(0, 1)},
                {
                    "originSelectionRange": range(2, 0),
                    "targetUri":"file:///workspace/b.rs",
                    "targetRange": range(3, 0),
                    "targetSelectionRange": range(3, 4)
                }
            ])),
            LspQueryKind::Definition,
            QUERY_REQUEST_ID,
            "file:///workspace/main.rs",
        )
        .unwrap();
        let Some(LspQueryResponse::Locations(locations)) = report.response else {
            panic!("unexpected response")
        };
        assert_eq!(locations.len(), 2);
        assert_eq!(locations[1].uri, "file:///workspace/b.rs");
        assert_eq!(locations[1].range.unwrap().column, 5);
    }

    #[test]
    fn parses_references_and_implementation_location_arrays() {
        for kind in [LspQueryKind::References, LspQueryKind::Implementation] {
            let report = parse_lsp_query_report(
                &response(&json!([
                    {"uri":"file:///workspace/a.rs", "range": range(0, 0)}
                ])),
                kind,
                QUERY_REQUEST_ID,
                "file:///workspace/main.rs",
            )
            .unwrap();
            assert!(matches!(
                report.response,
                Some(LspQueryResponse::Locations(ref locations)) if locations.len() == 1
            ));
        }
    }

    #[test]
    fn flattens_hierarchical_document_symbols() {
        let report = parse_lsp_query_report(
            &response(&json!([
                {
                    "name":"Forge", "kind":5, "range":range(0,0),
                    "selectionRange":range(0,4), "children":[{
                        "name":"cast", "kind":6, "range":range(1,0),
                        "selectionRange":range(1,7), "detail":"fn cast()"
                    }]
                },
                {
                    "name":"legacy", "kind":12, "containerName":"module",
                    "location":{"uri":"file:///workspace/legacy.rs", "range":range(3,1)}
                }
            ])),
            LspQueryKind::DocumentSymbols,
            QUERY_REQUEST_ID,
            "file:///workspace/main.rs",
        )
        .unwrap();
        let Some(LspQueryResponse::Symbols(symbols)) = report.response else {
            panic!("unexpected response")
        };
        assert_eq!(symbols.len(), 3);
        assert_eq!(symbols[0].kind, "class");
        assert_eq!(symbols[1].depth, 1);
        assert_eq!(symbols[1].name, "cast");
        assert_eq!(symbols[2].container.as_deref(), Some("module"));
    }

    #[test]
    fn parses_symbol_information_and_workspace_symbol_shapes() {
        let report = parse_lsp_query_report(
            &response(&json!([
                {
                    "name":"Forge", "kind":23, "containerName":"core",
                    "location":{"uri":"file:///workspace/a.rs", "range":range(2,1)}
                },
                {
                    "name":"unresolved", "kind":12, "detail":"fn unresolved",
                    "location":{"uri":"file:///workspace/b.rs"}
                }
            ])),
            LspQueryKind::WorkspaceSymbols,
            QUERY_REQUEST_ID,
            "file:///workspace/main.rs",
        )
        .unwrap();
        let Some(LspQueryResponse::Symbols(symbols)) = report.response else {
            panic!("unexpected response")
        };
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].container.as_deref(), Some("core"));
        assert!(symbols[1].location.range.is_none());
    }

    #[test]
    fn malformed_duplicate_and_oversized_responses_fail_closed() {
        assert!(
            parse_lsp_query_report(
                "Content-Length: 99\r\n\r\n{}",
                LspQueryKind::Hover,
                QUERY_REQUEST_ID,
                "file:///workspace/main.rs"
            )
            .is_err()
        );
        let duplicate = response(&Value::Null) + &response(&Value::Null);
        assert!(
            parse_lsp_query_report(
                &duplicate,
                LspQueryKind::Hover,
                QUERY_REQUEST_ID,
                "file:///workspace/main.rs"
            )
            .is_err()
        );
        let oversized = "x".repeat(MAX_RESPONSE_BYTES + 1);
        assert!(
            parse_lsp_query_report(
                &oversized,
                LspQueryKind::Hover,
                QUERY_REQUEST_ID,
                "file:///workspace/main.rs"
            )
            .is_err()
        );
    }

    #[test]
    fn validates_operation_specific_inputs() {
        assert!(
            build_lsp_query_session(
                &server(),
                Path::new("/tmp/work"),
                Path::new("/tmp/work/main.rs"),
                "",
                &LspQueryRequest {
                    kind: LspQueryKind::Hover,
                    position: None,
                    query: None
                }
            )
            .is_err()
        );
        assert!(
            build_lsp_query_session(
                &server(),
                Path::new("/tmp/work"),
                Path::new("/tmp/work/main.rs"),
                "",
                &LspQueryRequest {
                    kind: LspQueryKind::WorkspaceSymbols,
                    position: None,
                    query: Some(String::new())
                }
            )
            .is_err()
        );
    }
}
