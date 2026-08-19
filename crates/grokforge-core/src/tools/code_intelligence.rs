//! Model-callable local code intelligence. Configuration is loaded once when the registry is
//! constructed, then every process runs through the active OS sandbox with exact argv (no shell).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::path::{Component, Path};
use std::sync::Arc;

use async_trait::async_trait;
use grokforge_context::{
    CodeIntelligenceConfig, LspLocation, LspPosition, LspQueryKind, LspQueryRequest,
    LspQueryResponse, LspRange, LspSymbol, PreparedFormatter, PreparedLanguageServer,
    build_diagnostic_session, build_lsp_query_session, parse_diagnostic_report,
    parse_lsp_query_report,
};
use grokforge_protocol::{ApprovalKind, DenialClass, NetworkMode, SandboxMode, SandboxPolicy};
use grokforge_sandbox::{CommandSpec, ExecError, ExecOutput};
use serde_json::json;

use super::builtins::{canonical_read_path, is_blocked, truncate_utf8_bytes};
use super::{Tool, ToolInvocation, ToolOutput, ToolSpec, TurnContext, arg_str};
use crate::approvals::ApprovalNeed;

const MAX_DIAGNOSTIC_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_LSP_QUERY_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_LSP_QUERY_TEXT_BYTES: usize = 32 * 1024;
const MAX_LSP_QUERY_STRING_BYTES: usize = 512;
const MAX_FORMAT_SOURCE_BYTES: usize = crate::path_safety::MAX_MUTATING_FILE_BYTES;
const MAX_PROCESS_DETAIL_BYTES: usize = 16 * 1024;

type SharedConfig = Arc<Result<CodeIntelligenceConfig, String>>;

/// Construct the tools with one immutable configuration snapshot. A malformed owner config is
/// surfaced by calls instead of silently falling back to different executables.
pub(crate) fn all() -> Vec<Arc<dyn Tool>> {
    let config = Arc::new(CodeIntelligenceConfig::load_global().map_err(|error| error.to_string()));
    vec![
        Arc::new(LspDiagnostics {
            config: Arc::clone(&config),
        }),
        Arc::new(LspQuery {
            config: Arc::clone(&config),
        }),
        Arc::new(FormatFile { config }),
    ]
}

#[derive(Debug)]
struct LspDiagnostics {
    config: SharedConfig,
}

#[derive(Debug)]
struct LspQuery {
    config: SharedConfig,
}

#[derive(Debug)]
struct FormatFile {
    config: SharedConfig,
}

impl LspDiagnostics {
    fn prepare(&self, ctx: &TurnContext, path: &Path) -> Result<PreparedLanguageServer, String> {
        self.config
            .as_ref()
            .as_ref()
            .map_err(Clone::clone)?
            .prepare_language_server(&ctx.workspace_root, path)
            .map_err(|error| error.to_string())
    }
}

impl LspQuery {
    fn prepare(&self, ctx: &TurnContext, path: &Path) -> Result<PreparedLanguageServer, String> {
        self.config
            .as_ref()
            .as_ref()
            .map_err(Clone::clone)?
            .prepare_language_server(&ctx.workspace_root, path)
            .map_err(|error| error.to_string())
    }
}

impl FormatFile {
    fn prepare(&self, ctx: &TurnContext, path: &Path) -> Result<PreparedFormatter, String> {
        self.config
            .as_ref()
            .as_ref()
            .map_err(Clone::clone)?
            .prepare_formatter(&ctx.workspace_root, path)
            .map_err(|error| error.to_string())
    }
}

#[async_trait]
impl Tool for LspDiagnostics {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp_diagnostics".to_string(),
            description: "Ask the configured local language server for diagnostics on one text file. The server runs with network off inside a read-only OS sandbox; output and runtime are bounded. Configure owner-only commands in ~/.grokforge/code-intelligence.toml."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Text file path relative to the workspace root."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            mutating: false,
            parallel_safe: true,
        }
    }

    fn approval(&self, args: &serde_json::Value, ctx: &TurnContext) -> ApprovalNeed {
        let raw = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let path = ctx.resolve(raw);
        let (command, cwd) = self.prepare(ctx, &path).map_or_else(
            |_| {
                (
                    vec!["configured-language-server".to_string()],
                    ctx.workspace_root.clone(),
                )
            },
            |prepared| (command_vec(&prepared.program, &prepared.args), prepared.cwd),
        );
        ApprovalNeed::Gated(ApprovalKind::ExecCommand {
            command,
            cwd,
            sandbox: SandboxMode::ReadOnly,
            escalation_of: None,
        })
    }

    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        let raw_path = match arg_str(&inv.args, "path") {
            Ok(path) => path,
            Err(error) => return error,
        };
        let resolved = inv.ctx.resolve(raw_path);
        if is_blocked(inv.ctx, &resolved) {
            return ToolOutput::failure(format!(
                "cannot diagnose `{raw_path}`: path matches a secrets.deny rule"
            ));
        }
        let canonical = match canonical_read_path(inv.ctx, &resolved) {
            Ok(path) => path,
            Err(error) => {
                return ToolOutput::failure(format!("cannot diagnose `{raw_path}`: {error}"));
            }
        };
        if is_blocked(inv.ctx, &canonical) {
            return ToolOutput::failure(format!(
                "cannot diagnose `{raw_path}`: resolved path is secret"
            ));
        }
        let workspace = inv.ctx.workspace_root.clone();
        let source_path = canonical.clone();
        let (source, truncated) = match tokio::task::spawn_blocking(move || {
            crate::path_safety::read_workspace_text(
                &workspace,
                &source_path,
                MAX_DIAGNOSTIC_SOURCE_BYTES,
            )
        })
        .await
        {
            Ok(Ok(source)) => source,
            Ok(Err(error)) => {
                return ToolOutput::failure(format!(
                    "cannot diagnose `{raw_path}` safely: {error}"
                ));
            }
            Err(error) => {
                return ToolOutput::failure(format!(
                    "cannot read `{raw_path}` for diagnostics: task failed: {error}"
                ));
            }
        };
        if truncated {
            return ToolOutput::failure(format!(
                "cannot diagnose `{raw_path}`: file exceeds the {MAX_DIAGNOSTIC_SOURCE_BYTES}-byte source limit"
            ));
        }
        let server = match self.prepare(inv.ctx, &canonical) {
            Ok(server) => server,
            Err(error) => return ToolOutput::failure(error),
        };
        let (stdin, target_uri) =
            match build_diagnostic_session(&server, &server.cwd, &canonical, &source) {
                Ok(session) => session,
                Err(error) => return ToolOutput::failure(error.to_string()),
            };
        if let Err(error) = server.reverify_executable() {
            return ToolOutput::failure(error.to_string());
        }
        let spec = CommandSpec {
            program: server.program.clone(),
            args: server.args.clone(),
            cwd: server.cwd,
            timeout: server.timeout,
            stdin: Some(stdin),
            stdin_close_delay: server.diagnostic_wait,
            env: Vec::new(),
            private_read_roots: Vec::new(),
            cancellation: Some(inv.ctx.cancellation.process_token()),
        };
        let policy = diagnostic_policy(inv.ctx);
        match inv.ctx.sandbox.run(&policy, &spec).await {
            Ok(output) => diagnostics_output(raw_path, &target_uri, &output),
            Err(ExecError::Cancelled) => {
                ToolOutput::failure("[turn interrupted by user; language server killed and reaped]")
            }
            Err(error) => ToolOutput::failure(format!(
                "failed to run language server `{}`: {error}",
                server.program
            )),
        }
    }
}

#[async_trait]
impl Tool for LspQuery {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp_query".to_string(),
            description: "Run one bounded, read-only query against the configured local language server. Supports hover, definition, references, document symbols, workspace symbols, and implementation. The server is reverified, runs once with network off in the read-only OS sandbox, and is killed on cancellation or timeout. GrokForge proactively sends only the requested document over LSP, but the local server receives the workspace root and can inspect read-only filesystem paths visible inside its sandbox. Returned locations are restricted to non-secret files inside this workspace."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "operation": {
                        "type": "string",
                        "enum": [
                            "hover",
                            "definition",
                            "references",
                            "document_symbols",
                            "workspace_symbols",
                            "implementation"
                        ]
                    },
                    "path": {
                        "type": "string",
                        "maxLength": 4096,
                        "description": "Workspace-relative text file used to select and initialize the language server."
                    },
                    "line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "One-based source line; required for hover, definition, references, and implementation."
                    },
                    "character": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "One-based UTF-16 column; required for positional operations."
                    },
                    "query": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_LSP_QUERY_STRING_BYTES,
                        "description": "Search text required only for workspace_symbols."
                    }
                },
                "required": ["operation", "path"],
                "additionalProperties": false
            }),
            mutating: false,
            parallel_safe: true,
        }
    }

    fn approval(&self, args: &serde_json::Value, ctx: &TurnContext) -> ApprovalNeed {
        let raw = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let path = ctx.resolve(raw);
        let (command, cwd) = self.prepare(ctx, &path).map_or_else(
            |_| {
                (
                    vec!["configured-language-server".to_string()],
                    ctx.workspace_root.clone(),
                )
            },
            |prepared| (command_vec(&prepared.program, &prepared.args), prepared.cwd),
        );
        ApprovalNeed::Gated(ApprovalKind::ExecCommand {
            command,
            cwd,
            sandbox: SandboxMode::ReadOnly,
            escalation_of: None,
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        let raw_path = match arg_str(&inv.args, "path") {
            Ok(path) => path,
            Err(error) => return error,
        };
        if raw_path.len() > 4_096 {
            return ToolOutput::failure("lsp_query `path` exceeds the 4096-byte limit");
        }
        let Some(kind) = arg_str(&inv.args, "operation")
            .ok()
            .and_then(LspQueryKind::from_name)
        else {
            return ToolOutput::failure(
                "lsp_query `operation` must be hover, definition, references, document_symbols, workspace_symbols, or implementation",
            );
        };
        let resolved = inv.ctx.resolve(raw_path);
        if is_blocked(inv.ctx, &resolved) {
            return ToolOutput::failure("cannot query language server: source path is secret");
        }
        let canonical = match canonical_read_path(inv.ctx, &resolved) {
            Ok(path) => path,
            Err(error) => {
                return ToolOutput::failure(format!(
                    "cannot query language server for source file: {error}"
                ));
            }
        };
        if is_blocked(inv.ctx, &canonical) {
            return ToolOutput::failure(
                "cannot query language server: resolved source path is secret",
            );
        }
        let display_path = safe_workspace_display(inv.ctx, &canonical)
            .unwrap_or_else(|| "[workspace file]".to_string());
        let workspace = inv.ctx.workspace_root.clone();
        let source_path = canonical.clone();
        let (source, truncated) = match tokio::task::spawn_blocking(move || {
            crate::path_safety::read_workspace_text(
                &workspace,
                &source_path,
                MAX_LSP_QUERY_SOURCE_BYTES,
            )
        })
        .await
        {
            Ok(Ok(source)) => source,
            Ok(Err(error)) => {
                return ToolOutput::failure(format!("cannot read LSP source file safely: {error}"));
            }
            Err(error) => {
                return ToolOutput::failure(format!(
                    "cannot read source file for LSP query: task failed: {error}"
                ));
            }
        };
        if truncated {
            return ToolOutput::failure(format!(
                "cannot run lsp_query: file exceeds the {MAX_LSP_QUERY_SOURCE_BYTES}-byte source limit"
            ));
        }
        let request = match parse_lsp_query_args(&inv.args, kind, &source) {
            Ok(request) => request,
            Err(error) => return error,
        };
        let server = match self.prepare(inv.ctx, &canonical) {
            Ok(server) => server,
            Err(error) => return ToolOutput::failure(error),
        };
        let session =
            match build_lsp_query_session(&server, &server.cwd, &canonical, &source, &request) {
                Ok(session) => session,
                Err(error) => return ToolOutput::failure(error.to_string()),
            };
        if let Err(error) = server.reverify_executable() {
            return ToolOutput::failure(error.to_string());
        }
        let spec = CommandSpec {
            program: server.program.clone(),
            args: server.args.clone(),
            cwd: server.cwd,
            timeout: server.timeout,
            stdin: Some(session.stdin),
            stdin_close_delay: server.diagnostic_wait,
            env: Vec::new(),
            private_read_roots: Vec::new(),
            cancellation: Some(inv.ctx.cancellation.process_token()),
        };
        let policy = diagnostic_policy(inv.ctx);
        match inv.ctx.sandbox.run(&policy, &spec).await {
            Ok(output) => lsp_query_output(
                inv.ctx,
                kind,
                &display_path,
                &session.target_uri,
                session.request_id,
                &output,
            ),
            Err(ExecError::Cancelled) => {
                ToolOutput::failure("[turn interrupted by user; language server killed and reaped]")
            }
            Err(error) => ToolOutput::failure(format!(
                "failed to run language server `{}`: {error}",
                server.program
            )),
        }
    }
}

#[async_trait]
impl Tool for FormatFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "format_file".to_string(),
            description: "Format one workspace text file safely with the configured local formatter. GrokForge formats a private copy in a networkless sandbox, then descriptor-safely replaces only the requested file. The executable is canonical, argv uses no shell, and runtime/input/output are bounded."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Text file path relative to the workspace root."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            mutating: true,
            parallel_safe: false,
        }
    }

    fn approval(&self, args: &serde_json::Value, ctx: &TurnContext) -> ApprovalNeed {
        let raw = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let path = ctx.resolve(raw);
        let (command, cwd) = self.prepare(ctx, &path).map_or_else(
            |_| {
                (
                    vec!["configured-formatter".to_string()],
                    ctx.workspace_root.clone(),
                )
            },
            |prepared| (command_vec(&prepared.program, &prepared.args), prepared.cwd),
        );
        ApprovalNeed::Gated(ApprovalKind::ExecCommand {
            command,
            cwd,
            sandbox: ctx.policy.mode,
            escalation_of: None,
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        let raw_path = match arg_str(&inv.args, "path") {
            Ok(path) => path,
            Err(error) => return error,
        };
        let resolved = inv.ctx.resolve(raw_path);
        if is_blocked(inv.ctx, &resolved) {
            return ToolOutput::failure(format!(
                "cannot format `{raw_path}`: path matches a secrets.deny rule"
            ));
        }
        let canonical = match canonical_read_path(inv.ctx, &resolved) {
            Ok(path) => path,
            Err(error) => {
                return ToolOutput::failure(format!("cannot format `{raw_path}`: {error}"));
            }
        };
        if is_blocked(inv.ctx, &canonical) {
            return ToolOutput::failure(format!(
                "cannot format `{raw_path}`: resolved path is secret"
            ));
        }
        let workspace = inv.ctx.workspace_root.clone();
        let source_path = canonical.clone();
        let (source, truncated) = match tokio::task::spawn_blocking(move || {
            crate::path_safety::read_workspace_text(
                &workspace,
                &source_path,
                MAX_FORMAT_SOURCE_BYTES,
            )
        })
        .await
        {
            Ok(Ok(source)) => source,
            Ok(Err(error)) => {
                return ToolOutput::failure(format!("cannot format `{raw_path}` safely: {error}"));
            }
            Err(error) => {
                return ToolOutput::failure(format!(
                    "cannot inspect `{raw_path}` before formatting: task failed: {error}"
                ));
            }
        };
        if truncated {
            return ToolOutput::failure(format!(
                "cannot format `{raw_path}`: file exceeds the {MAX_FORMAT_SOURCE_BYTES}-byte mutation limit"
            ));
        }
        let Some(extension) = canonical.extension().and_then(std::ffi::OsStr::to_str) else {
            return ToolOutput::failure(format!(
                "cannot format `{raw_path}`: file has no supported UTF-8 extension"
            ));
        };
        let scratch = match tempfile::Builder::new()
            .prefix("grokforge-format-")
            .tempdir()
        {
            Ok(directory) => directory,
            Err(error) => {
                return ToolOutput::failure(format!(
                    "cannot create private formatter workspace: {error}"
                ));
            }
        };
        let private_copy = scratch.path().join(format!("source.{extension}"));
        let copy_for_write = private_copy.clone();
        let source_for_copy = source.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || {
            write_private_source_copy(&copy_for_write, source_for_copy.as_bytes())
        })
        .await
        .map_err(|error| format!("private formatter copy task failed: {error}"))
        .and_then(|result| result.map_err(|error| format!("cannot write formatter copy: {error}")))
        {
            return ToolOutput::failure(error);
        }
        let prepared = match self
            .config
            .as_ref()
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|config| {
                config
                    .prepare_formatter_for_copy(&inv.ctx.workspace_root, &canonical, &private_copy)
                    .map_err(|error| error.to_string())
            }) {
            Ok(prepared) => prepared,
            Err(error) => return ToolOutput::failure(error),
        };
        if let Err(error) = prepared.reverify_executable() {
            return ToolOutput::failure(error.to_string());
        }
        let spec = CommandSpec {
            program: prepared.program.clone(),
            args: prepared.args.clone(),
            cwd: prepared.cwd,
            timeout: prepared.timeout,
            stdin: None,
            stdin_close_delay: std::time::Duration::ZERO,
            env: Vec::new(),
            private_read_roots: Vec::new(),
            cancellation: Some(inv.ctx.cancellation.process_token()),
        };
        let policy = formatter_policy(inv.ctx, &canonical, scratch.path());
        match inv.ctx.sandbox.run(&policy, &spec).await {
            Ok(output) if output.succeeded() => {
                let copy_for_read = private_copy.clone();
                let formatted_bytes = match tokio::task::spawn_blocking(move || {
                    read_private_formatted_copy(&copy_for_read)
                })
                .await
                {
                    Ok(Ok(formatted_bytes)) => formatted_bytes,
                    Ok(Err(error)) => return ToolOutput::failure(error),
                    Err(error) => {
                        return ToolOutput::failure(format!(
                            "formatted-copy read task failed: {error}"
                        ));
                    }
                };
                let changed = formatted_bytes != source.as_bytes();
                if changed {
                    let policy = inv.ctx.policy.clone();
                    let target = canonical.clone();
                    let approved_target = inv.ctx.bound_write_target(&canonical);
                    let expected = source.into_bytes();
                    match tokio::task::spawn_blocking(move || {
                        crate::path_safety::replace_file_if_unchanged_bound(
                            &policy,
                            &target,
                            approved_target.as_deref(),
                            &expected,
                            &formatted_bytes,
                        )
                    })
                    .await
                    {
                        Ok(Ok(())) => inv.ctx.record_touched(canonical),
                        Ok(Err(error)) => {
                            return format_write_failure(raw_path, &error);
                        }
                        Err(error) => {
                            return ToolOutput::failure(format!(
                                "cannot install formatted `{raw_path}`: task failed: {error}"
                            ));
                        }
                    }
                }
                let details = process_details(&output);
                let suffix = if details.is_empty() {
                    String::new()
                } else {
                    format!("\n{details}")
                };
                ToolOutput::success(format!(
                    "{} `{raw_path}` with `{}`{suffix}",
                    if changed {
                        "formatted"
                    } else {
                        "already formatted"
                    },
                    prepared.program
                ))
            }
            Ok(output) => process_failure("formatter", &prepared.program, &output),
            Err(ExecError::Cancelled) => {
                ToolOutput::failure("[turn interrupted by user; formatter killed and reaped]")
            }
            Err(error) => ToolOutput::failure(format!(
                "failed to run formatter `{}`: {error}",
                prepared.program
            )),
        }
    }
}

fn parse_lsp_query_args(
    args: &serde_json::Value,
    kind: LspQueryKind,
    source: &str,
) -> Result<LspQueryRequest, ToolOutput> {
    let position = if kind.requires_position() {
        let line = required_positive_integer(args, "line")?;
        let character = required_positive_integer(args, "character")?;
        let line_index = usize::try_from(line.saturating_sub(1))
            .map_err(|_| ToolOutput::failure("lsp_query `line` is too large"))?;
        let Some(source_line) = source.split('\n').nth(line_index) else {
            return Err(ToolOutput::failure(format!(
                "lsp_query `line` {line} is outside the source file"
            )));
        };
        let source_line = source_line.strip_suffix('\r').unwrap_or(source_line);
        let max_character = u64::try_from(source_line.encode_utf16().count())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        if character > max_character {
            return Err(ToolOutput::failure(format!(
                "lsp_query `character` {character} is outside line {line}; maximum one-based UTF-16 column is {max_character}"
            )));
        }
        Some(LspPosition {
            line: line.saturating_sub(1),
            character: character.saturating_sub(1),
        })
    } else {
        if args.get("line").is_some() || args.get("character").is_some() {
            return Err(ToolOutput::failure(format!(
                "lsp_query operation `{}` does not accept line or character",
                kind.as_str()
            )));
        }
        None
    };
    let query = if kind.requires_query() {
        let query = arg_str(args, "query")?;
        if query.is_empty()
            || query.len() > MAX_LSP_QUERY_STRING_BYTES
            || query.chars().any(char::is_control)
        {
            return Err(ToolOutput::failure(format!(
                "lsp_query workspace_symbols `query` must be 1-{MAX_LSP_QUERY_STRING_BYTES} bytes with no control characters"
            )));
        }
        Some(query.to_string())
    } else {
        if args.get("query").is_some() {
            return Err(ToolOutput::failure(format!(
                "lsp_query operation `{}` does not accept query",
                kind.as_str()
            )));
        }
        None
    };
    Ok(LspQueryRequest {
        kind,
        position,
        query,
    })
}

fn required_positive_integer(args: &serde_json::Value, field: &str) -> Result<u64, ToolOutput> {
    let value = args.get(field).and_then(serde_json::Value::as_u64);
    match value {
        Some(value) if value > 0 => Ok(value),
        _ => Err(ToolOutput::failure(format!(
            "lsp_query `{field}` must be a positive integer"
        ))),
    }
}

fn lsp_query_output(
    ctx: &TurnContext,
    kind: LspQueryKind,
    display_path: &str,
    target_uri: &str,
    request_id: u64,
    output: &ExecOutput,
) -> ToolOutput {
    if let Some(denial) = output.denial {
        return ToolOutput::Failure {
            error: format!(
                "language server blocked by read-only sandbox{}",
                lsp_error_suffix(output)
            ),
            denial: Some(denial),
        };
    }
    if output.truncated {
        return ToolOutput::failure(
            "language-server response exceeded the 64 KiB process cap; query results were discarded",
        );
    }
    let report = match parse_lsp_query_report(&output.stdout, kind, request_id, target_uri) {
        Ok(report) => report,
        Err(error) => {
            return ToolOutput::failure(format!(
                "could not parse {} language-server response: {error}{}",
                kind.as_str(),
                lsp_error_suffix(output)
            ));
        }
    };
    if report.server_error.is_some() {
        return ToolOutput::failure(format!(
            "language server rejected {} query; server message withheld{}",
            kind.as_str(),
            lsp_error_suffix(output)
        ));
    }
    let Some(response) = &report.response else {
        return ToolOutput::failure(format!(
            "language server returned no response for {} query{}",
            kind.as_str(),
            lsp_error_suffix(output)
        ));
    };
    let mut rendered = match response {
        LspQueryResponse::Hover(hover) => render_hover(display_path, hover.as_ref()),
        LspQueryResponse::Locations(locations) => {
            render_locations(ctx, kind, locations, report.omitted)
        }
        LspQueryResponse::Symbols(symbols) => render_symbols(ctx, kind, symbols, report.omitted),
    };
    if !output.succeeded() {
        rendered.push_str("\n[note: language server answered before exiting unsuccessfully]");
    }
    ToolOutput::success(truncate_utf8_bytes(
        rendered,
        MAX_LSP_QUERY_TEXT_BYTES,
        "\n… [LSP query output truncated] …",
    ))
}

fn render_hover(display_path: &str, hover: Option<&grokforge_context::LspHover>) -> String {
    let Some(hover) = hover.filter(|hover| !hover.contents.trim().is_empty()) else {
        return format!("no hover information for `{display_path}`");
    };
    let location = hover.range.map_or_else(
        || display_path.to_string(),
        |range| format!("{display_path}:{}:{}", range.line, range.column),
    );
    format!(
        "hover for `{location}`:\n{}",
        redact_absolute_paths(&redact_file_uris(&hover.contents))
    )
}

fn render_locations(
    ctx: &TurnContext,
    kind: LspQueryKind,
    locations: &[LspLocation],
    parser_omitted: usize,
) -> String {
    let mut seen = BTreeSet::new();
    let mut safe = Vec::new();
    let mut omitted = parser_omitted;
    for location in locations {
        let Some(location) = safe_location(ctx, location) else {
            omitted = omitted.saturating_add(1);
            continue;
        };
        let rendered = render_safe_location(&location.0, location.1);
        if seen.insert(rendered.clone()) {
            safe.push(rendered);
        } else {
            omitted = omitted.saturating_add(1);
        }
    }
    let mut output = if safe.is_empty() {
        format!("no safe {} results inside the workspace", kind.as_str())
    } else {
        format!(
            "{} {} result(s):\n{}",
            safe.len(),
            kind.as_str(),
            safe.join("\n")
        )
    };
    if omitted > 0 {
        let _ = write!(
            output,
            "\n… [{omitted} malformed, duplicate, secret, non-file, or outside-workspace result(s) omitted] …"
        );
    }
    output
}

fn render_symbols(
    ctx: &TurnContext,
    kind: LspQueryKind,
    symbols: &[LspSymbol],
    parser_omitted: usize,
) -> String {
    let mut seen = BTreeSet::new();
    let mut safe = Vec::new();
    let mut omitted = parser_omitted;
    for symbol in symbols {
        let Some(location) = safe_location(ctx, &symbol.location) else {
            omitted = omitted.saturating_add(1);
            continue;
        };
        let location = render_safe_location(&location.0, location.1);
        let indent = "  ".repeat(symbol.depth.min(16));
        let container = symbol
            .container
            .as_ref()
            .map_or_else(String::new, |container| format!(" in {container}"));
        let detail = symbol
            .detail
            .as_ref()
            .map_or_else(String::new, |detail| format!(" — {detail}"));
        let rendered = format!(
            "{indent}{location}: {} {}{container}{detail}",
            symbol.kind, symbol.name
        );
        if seen.insert(rendered.clone()) {
            safe.push(rendered);
        } else {
            omitted = omitted.saturating_add(1);
        }
    }
    let mut output = if safe.is_empty() {
        format!("no safe {} results inside the workspace", kind.as_str())
    } else {
        format!(
            "{} {} result(s):\n{}",
            safe.len(),
            kind.as_str(),
            safe.join("\n")
        )
    };
    if omitted > 0 {
        let _ = write!(
            output,
            "\n… [{omitted} malformed, duplicate, secret, non-file, or outside-workspace result(s) omitted] …"
        );
    }
    output
}

fn safe_location(ctx: &TurnContext, location: &LspLocation) -> Option<(String, Option<LspRange>)> {
    let path = location.file_path()?;
    if is_blocked(ctx, &path) {
        return None;
    }
    let canonical = canonical_read_path(ctx, &path).ok()?;
    if !canonical.is_file() || is_blocked(ctx, &canonical) {
        return None;
    }
    Some((safe_workspace_display(ctx, &canonical)?, location.range))
}

fn render_safe_location(path: &str, range: Option<LspRange>) -> String {
    range.map_or_else(
        || path.to_string(),
        |range| format!("{path}:{}:{}", range.line, range.column),
    )
}

fn safe_workspace_display(ctx: &TurnContext, path: &Path) -> Option<String> {
    let workspace = std::fs::canonicalize(&ctx.workspace_root).ok()?;
    let relative = path.strip_prefix(workspace).ok()?;
    let mut components = Vec::new();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return None;
        };
        let mut safe = String::new();
        for character in component.to_string_lossy().chars() {
            match character {
                '\\' => safe.push_str("\\\\"),
                character if character.is_control() => safe.extend(character.escape_default()),
                character => safe.push(character),
            }
        }
        components.push(safe);
    }
    (!components.is_empty()).then(|| components.join("/"))
}

fn redact_file_uris(input: &str) -> String {
    let mut remaining = input;
    let mut output = String::new();
    while let Some(start) = remaining.find("file://") {
        output.push_str(&remaining[..start]);
        let uri = &remaining[start..];
        let end = uri
            .char_indices()
            .find(|(_, character)| {
                character.is_whitespace()
                    || matches!(character, ')' | ']' | '}' | '>' | '"' | '\'' | '`')
            })
            .map_or(uri.len(), |(index, _)| index);
        output.push_str("[file URI omitted]");
        remaining = &uri[end..];
    }
    output.push_str(remaining);
    output
}

fn redact_absolute_paths(input: &str) -> String {
    let mut output = String::new();
    let mut cursor = 0;
    let indices: Vec<(usize, char)> = input.char_indices().collect();
    let mut position = 0;
    while position < indices.len() {
        let (start, character) = indices[position];
        let previous = position
            .checked_sub(1)
            .and_then(|index| indices.get(index).map(|(_, character)| *character));
        let at_boundary = previous.is_none_or(|previous| {
            previous.is_whitespace()
                || matches!(previous, '(' | '[' | '{' | '<' | '=' | '"' | '\'' | '`')
        });
        let unix_path = character == '/'
            && at_boundary
            && indices
                .get(position + 1)
                .is_some_and(|(_, next)| *next != '/');
        let windows_path = character.is_ascii_alphabetic()
            && at_boundary
            && indices
                .get(position + 1)
                .is_some_and(|(_, next)| *next == ':')
            && indices
                .get(position + 2)
                .is_some_and(|(_, next)| matches!(*next, '\\' | '/'));
        if !unix_path && !windows_path {
            position += 1;
            continue;
        }
        output.push_str(&input[cursor..start]);
        output.push_str("[absolute path omitted]");
        let mut end_position = position + 1;
        while end_position < indices.len()
            && !indices[end_position].1.is_whitespace()
            && !matches!(
                indices[end_position].1,
                ')' | ']' | '}' | '>' | '"' | '\'' | '`'
            )
        {
            end_position += 1;
        }
        cursor = indices
            .get(end_position)
            .map_or(input.len(), |(index, _)| *index);
        position = end_position;
    }
    output.push_str(&input[cursor..]);
    output
}

fn lsp_error_suffix(output: &ExecOutput) -> String {
    let emitted_stderr = !output.stderr.trim().is_empty();
    if !output.timed_out && !emitted_stderr {
        String::new()
    } else {
        format!(
            "\n[{}{}]",
            if output.timed_out {
                "language-server process timed out"
            } else {
                ""
            },
            if emitted_stderr {
                if output.timed_out {
                    "; stderr withheld"
                } else {
                    "language-server stderr withheld"
                }
            } else {
                ""
            }
        )
    }
}

fn diagnostic_policy(ctx: &TurnContext) -> SandboxPolicy {
    let mut policy = SandboxPolicy::read_only(&ctx.workspace_root);
    policy
        .unreadable_globs
        .clone_from(&ctx.policy.unreadable_globs);
    policy
        .protected_paths
        .extend(ctx.policy.protected_paths.iter().cloned());
    policy.protected_paths.sort();
    policy.protected_paths.dedup();
    policy.network = NetworkMode::Isolated;
    policy
}

fn formatter_policy(ctx: &TurnContext, file: &Path, scratch: &Path) -> SandboxPolicy {
    let mut policy = ctx.policy.clone();
    // Formatting is a local operation even in the broad preset. The real workspace stays
    // read-only; the process can mutate only a private scratch directory containing a copy.
    policy.network = NetworkMode::Isolated;
    if policy.mode != SandboxMode::ReadOnly && ctx.policy.allows_write(file) {
        policy.mode = SandboxMode::WorkspaceWrite;
        policy.writable_roots = vec![scratch.to_path_buf()];
    } else {
        // Never retain unrelated writable roots when this particular target is not writable.
        // A denied format attempt must fail inside the sandbox before the formatter can mutate
        // any other path that happened to be permitted by the session policy.
        policy.mode = SandboxMode::ReadOnly;
        policy.writable_roots.clear();
    }
    policy
}

fn read_private_formatted_copy(path: &Path) -> Result<Vec<u8>, String> {
    #[cfg(unix)]
    let mut file = {
        use rustix::fs::{Mode, OFlags, open};

        let descriptor = open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|error| format!("cannot safely open formatted private copy: {error}"))?;
        std::fs::File::from(descriptor)
    };
    #[cfg(not(unix))]
    let mut file = {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect formatted private copy: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err("formatter replaced its private copy with a symbolic link".to_string());
        }
        std::fs::File::open(path)
            .map_err(|error| format!("cannot open formatted private copy: {error}"))?
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect open formatted private copy: {error}"))?;
    if !metadata.is_file() {
        return Err("formatter replaced its private copy with a non-regular file".to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        if metadata.nlink() != 1 {
            return Err("formatter produced a multiply-linked private copy".to_string());
        }
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take((MAX_FORMAT_SOURCE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read formatted private copy: {error}"))?;
    if bytes.len() > MAX_FORMAT_SOURCE_BYTES {
        return Err(format!(
            "formatter output exceeds the {MAX_FORMAT_SOURCE_BYTES}-byte mutation limit"
        ));
    }
    std::str::from_utf8(&bytes).map_err(|_| "formatter output is not UTF-8 text".to_string())?;
    Ok(bytes)
}

fn write_private_source_copy(path: &Path, source: &[u8]) -> Result<(), std::io::Error> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(source)?;
    file.flush()
}

fn format_write_failure(path: &str, error: &crate::path_safety::PathSafetyError) -> ToolOutput {
    ToolOutput::Failure {
        error: format!("cannot install formatted `{path}`: {error}"),
        denial: matches!(error, crate::path_safety::PathSafetyError::Denied)
            .then_some(DenialClass::FsWrite),
    }
}

fn command_vec(program: &str, args: &[String]) -> Vec<String> {
    std::iter::once(program.to_string())
        .chain(args.iter().cloned())
        .collect()
}

fn diagnostics_output(path: &str, target_uri: &str, output: &ExecOutput) -> ToolOutput {
    if let Some(denial) = output.denial {
        return ToolOutput::Failure {
            error: format!(
                "language server blocked by read-only sandbox: {}",
                process_details(output)
            ),
            denial: Some(denial),
        };
    }
    if output.truncated {
        return ToolOutput::failure(
            "language-server output exceeded the 64 KiB process cap; narrow the server configuration",
        );
    }
    let report = match parse_diagnostic_report(&output.stdout, target_uri) {
        Ok(report) => report,
        Err(error) => {
            let details = process_details(output);
            return ToolOutput::failure(format!(
                "could not read language-server diagnostics: {error}{}",
                if details.is_empty() {
                    String::new()
                } else {
                    format!("\n{details}")
                }
            ));
        }
    };
    if !report.published {
        let mut details = process_details(output);
        if !report.server_errors.is_empty() {
            if !details.is_empty() {
                details.push('\n');
            }
            details.push_str(&report.server_errors.join("\n"));
        }
        return ToolOutput::failure(format!(
            "{}{}",
            report.render(path),
            if details.is_empty() {
                String::new()
            } else {
                format!("\n{details}")
            }
        ));
    }
    let mut rendered = report.render(path);
    if !output.succeeded() {
        let _ = std::fmt::Write::write_fmt(
            &mut rendered,
            format_args!(
                "\n[note: language server exited {} after publishing diagnostics]",
                output
                    .exit_code
                    .map_or_else(|| "without a status".to_string(), |code| code.to_string())
            ),
        );
    }
    ToolOutput::success(rendered)
}

fn process_failure(kind: &str, program: &str, output: &ExecOutput) -> ToolOutput {
    let denial = output.denial;
    ToolOutput::Failure {
        error: format!(
            "{kind} `{program}` failed{}",
            if process_details(output).is_empty() {
                String::new()
            } else {
                format!("\n{}", process_details(output))
            }
        ),
        denial,
    }
}

fn process_details(output: &ExecOutput) -> String {
    let mut details = String::new();
    if !output.stdout.trim().is_empty() {
        details.push_str(output.stdout.trim());
    }
    if !output.stderr.trim().is_empty() {
        if !details.is_empty() {
            details.push_str("\n[stderr]\n");
        }
        details.push_str(output.stderr.trim());
    }
    if output.timed_out && !details.contains("timed out") {
        if !details.is_empty() {
            details.push('\n');
        }
        details.push_str("process timed out");
    }
    truncate_utf8_bytes(
        details,
        MAX_PROCESS_DETAIL_BYTES,
        "\n… [process details truncated] …",
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use grokforge_protocol::{SandboxMode, ToolCallId};
    use grokforge_sandbox::{ExecError, SandboxCapability, SandboxRunner};
    use serde_json::Value;

    use super::*;
    use crate::TurnCancellation;

    #[derive(Debug)]
    struct RecordingRunner {
        output: ExecOutput,
        stdin: Mutex<Option<Vec<u8>>>,
        /// Test-only formatter output installed into the private copy named by the last argv.
        formatted: Option<Vec<u8>>,
    }

    #[async_trait]
    impl SandboxRunner for RecordingRunner {
        fn capability(&self) -> SandboxCapability {
            SandboxCapability {
                backend: "test".to_string(),
                enforced: true,
                notes: vec![],
            }
        }

        async fn run(
            &self,
            policy: &SandboxPolicy,
            command: &CommandSpec,
        ) -> Result<ExecOutput, ExecError> {
            assert_eq!(policy.network, NetworkMode::Isolated);
            if let Ok(mut stdin) = self.stdin.lock() {
                stdin.clone_from(&command.stdin);
            }
            if let Some(formatted) = &self.formatted
                && command.stdin.is_none()
            {
                let private_copy = command.args.last().ok_or_else(|| {
                    ExecError::Io(std::io::Error::other("missing formatter file argument"))
                })?;
                std::fs::write(private_copy, formatted).map_err(ExecError::Io)?;
            }
            Ok(self.output.clone())
        }
    }

    fn context(root: &Path, runner: Arc<dyn SandboxRunner>) -> TurnContext {
        TurnContext {
            workspace_root: std::fs::canonicalize(root).unwrap(),
            policy: SandboxPolicy::workspace_write(root),
            sandbox: runner,
            touched: Arc::new(Mutex::new(Vec::new())),
            bound_write_targets: Vec::new(),
            cancellation: TurnCancellation::new(),
        }
    }

    fn test_config() -> CodeIntelligenceConfig {
        CodeIntelligenceConfig::from_toml(
            r#"
                extend_defaults = false
                [[language]]
                name = "rust"
                extensions = ["rs"]
                language_id = "rust"

                [language.lsp]
                command = "/bin/echo"
                diagnostic_wait_ms = 100

                [language.formatter]
                command = "/bin/echo"
                args = ["{file}"]
            "#,
        )
        .unwrap()
    }

    fn lsp_frame(message: &Value) -> String {
        let body = serde_json::to_string(&message).unwrap();
        format!("Content-Length: {}\r\n\r\n{body}", body.len())
    }

    #[tokio::test]
    async fn lsp_tool_sends_a_framed_read_only_session() {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("main.rs");
        std::fs::write(&file, "fn main() { let x: u8 = \"no\"; }").unwrap();
        let uri = format!(
            "file://{}",
            std::fs::canonicalize(&file).unwrap().to_string_lossy()
        );
        let output = ExecOutput {
            exit_code: Some(0),
            stdout: lsp_frame(&json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {"uri": uri, "diagnostics": [{
                    "range": {"start": {"line": 0, "character": 16}, "end": {"line": 0, "character": 20}},
                    "severity": 1,
                    "message": "mismatched types"
                }]}
            })),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
            denial: None,
        };
        let runner = Arc::new(RecordingRunner {
            output,
            stdin: Mutex::new(None),
            formatted: None,
        });
        let ctx = context(workspace.path(), runner.clone());
        let tool = LspDiagnostics {
            config: Arc::new(Ok(test_config())),
        };
        let result = tool
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: json!({"path": "main.rs"}),
                ctx: &ctx,
            })
            .await;
        assert!(!result.is_error(), "{result:?}");
        assert!(result.content().contains("main.rs:1:17: error"));
        let stdin = runner.stdin.lock().unwrap().clone().unwrap();
        assert!(
            String::from_utf8(stdin)
                .unwrap()
                .contains("textDocument/didOpen")
        );
    }

    #[tokio::test]
    async fn lsp_query_filters_outside_and_secret_locations() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("main.rs");
        let safe = workspace.path().join("safe.rs");
        let secret = workspace.path().join("secret.rs");
        std::fs::write(&source, "fn main() {}\n").unwrap();
        std::fs::write(&safe, "fn safe() {}\n").unwrap();
        std::fs::write(&secret, "fn secret() {}\n").unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.rs");
        std::fs::write(&outside_file, "fn outside() {}\n").unwrap();
        let uri = |path: &Path| {
            format!(
                "file://{}",
                std::fs::canonicalize(path).unwrap().to_string_lossy()
            )
        };
        let output = ExecOutput {
            exit_code: Some(0),
            stdout: lsp_frame(&json!({
                "jsonrpc":"2.0", "id":2, "result":[
                    {"uri":uri(&safe), "range":{"start":{"line":0,"character":3},"end":{"line":0,"character":7}}},
                    {"uri":uri(&secret), "range":{"start":{"line":0,"character":3},"end":{"line":0,"character":9}}},
                    {"uri":uri(&outside_file), "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}}},
                    {"uri":"https://example.com/not-local", "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}
                ]
            })),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
            denial: None,
        };
        let runner = Arc::new(RecordingRunner {
            output,
            stdin: Mutex::new(None),
            formatted: None,
        });
        let mut ctx = context(workspace.path(), runner.clone());
        ctx.policy.unreadable_globs.push("**/secret.rs".to_string());
        let result = LspQuery {
            config: Arc::new(Ok(test_config())),
        }
        .invoke(ToolInvocation {
            call_id: ToolCallId::new(),
            args: json!({
                "operation":"definition", "path":"main.rs", "line":1, "character":1
            }),
            ctx: &ctx,
        })
        .await;
        assert!(!result.is_error(), "{result:?}");
        assert!(result.content().contains("safe.rs:1:4"));
        assert!(!result.content().contains("secret.rs"));
        assert!(!result.content().contains("outside.rs"));
        assert!(!result.content().contains("example.com"));
        assert!(result.content().contains("3 malformed"));
        let stdin = String::from_utf8(runner.stdin.lock().unwrap().clone().unwrap()).unwrap();
        for method in [
            "initialize",
            "initialized",
            "textDocument/didOpen",
            "textDocument/definition",
            "textDocument/didClose",
            "shutdown",
            "exit",
        ] {
            assert!(stdin.contains(&format!("\"method\":\"{method}\"")));
        }
    }

    #[tokio::test]
    async fn lsp_query_hover_is_terminal_safe_and_withholds_file_uris() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").unwrap();
        let output = ExecOutput {
            exit_code: Some(0),
            stdout: lsp_frame(&json!({
                "jsonrpc":"2.0", "id":2, "result":{
                    "contents":{"kind":"markdown", "value":"main\u{001b}[31m file:///tmp/outside-secret.rs (/private/secret.txt)"},
                    "range":{"start":{"line":0,"character":3},"end":{"line":0,"character":7}}
                }
            })),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
            denial: None,
        };
        let ctx = context(
            workspace.path(),
            Arc::new(RecordingRunner {
                output,
                stdin: Mutex::new(None),
                formatted: None,
            }),
        );
        let result = LspQuery {
            config: Arc::new(Ok(test_config())),
        }
        .invoke(ToolInvocation {
            call_id: ToolCallId::new(),
            args: json!({"operation":"hover", "path":"main.rs", "line":1, "character":1}),
            ctx: &ctx,
        })
        .await;
        assert!(!result.is_error(), "{result:?}");
        assert!(result.content().contains("main.rs:1:4"));
        assert!(result.content().contains("[file URI omitted]"));
        assert!(result.content().contains("[absolute path omitted]"));
        assert!(!result.content().contains('\u{1b}'));
        assert!(!result.content().contains("outside-secret"));
        assert!(!result.content().contains("private/secret"));
    }

    #[tokio::test]
    async fn lsp_query_rejects_invalid_positions_queries_and_responses() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").unwrap();
        let ctx = context(
            workspace.path(),
            Arc::new(RecordingRunner {
                output: ExecOutput {
                    exit_code: Some(0),
                    stdout: "not LSP framing".to_string(),
                    stderr: "/outside/private/path".to_string(),
                    truncated: false,
                    timed_out: false,
                    denial: None,
                },
                stdin: Mutex::new(None),
                formatted: None,
            }),
        );
        for args in [
            json!({"operation":"hover", "path":"main.rs", "line":1, "character":99}),
            json!({"operation":"workspace_symbols", "path":"main.rs", "query":""}),
        ] {
            let result = LspQuery {
                config: Arc::new(Ok(test_config())),
            }
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args,
                ctx: &ctx,
            })
            .await;
            assert!(result.is_error());
        }
        let malformed = LspQuery {
            config: Arc::new(Ok(test_config())),
        }
        .invoke(ToolInvocation {
            call_id: ToolCallId::new(),
            args: json!({"operation":"document_symbols", "path":"main.rs"}),
            ctx: &ctx,
        })
        .await;
        assert!(malformed.is_error());
        assert!(!malformed.content().contains("/outside/private/path"));
        assert!(malformed.content().contains("stderr withheld"));

        let truncated_ctx = context(
            workspace.path(),
            Arc::new(RecordingRunner {
                output: ExecOutput {
                    exit_code: None,
                    stdout: "x".repeat(64 * 1024),
                    stderr: String::new(),
                    truncated: true,
                    timed_out: false,
                    denial: None,
                },
                stdin: Mutex::new(None),
                formatted: None,
            }),
        );
        let truncated = LspQuery {
            config: Arc::new(Ok(test_config())),
        }
        .invoke(ToolInvocation {
            call_id: ToolCallId::new(),
            args: json!({"operation":"document_symbols", "path":"main.rs"}),
            ctx: &truncated_ctx,
        })
        .await;
        assert!(truncated.is_error());
        assert!(truncated.content().contains("64 KiB process cap"));
    }

    #[tokio::test]
    async fn formatter_tool_records_only_the_requested_file() {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("main.rs");
        std::fs::write(&file, "fn main(){}").unwrap();
        let runner = Arc::new(RecordingRunner {
            output: ExecOutput {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                denial: None,
            },
            stdin: Mutex::new(None),
            formatted: Some(b"fn main() {}\n".to_vec()),
        });
        let ctx = context(workspace.path(), runner);
        let tool = FormatFile {
            config: Arc::new(Ok(test_config())),
        };
        let result = tool
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: json!({"path": "main.rs"}),
                ctx: &ctx,
            })
            .await;
        assert!(!result.is_error(), "{result:?}");
        assert_eq!(
            ctx.touched_paths(),
            vec![std::fs::canonicalize(&file).unwrap()]
        );
        assert_eq!(std::fs::read_to_string(file).unwrap(), "fn main() {}\n");
    }

    #[test]
    fn registry_exposes_all_code_intelligence_tools() {
        let registry = crate::tools::ToolRegistry::with_builtins();
        assert!(registry.get("lsp_diagnostics").is_some());
        assert!(registry.get("lsp_query").is_some());
        assert!(registry.get("format_file").is_some());
        assert!(
            registry
                .readonly_tool_defs()
                .iter()
                .filter_map(grokforge_xai::ToolDef::function_name)
                .any(|name| name == "lsp_diagnostics")
        );
        assert!(
            registry
                .readonly_tool_defs()
                .iter()
                .filter_map(grokforge_xai::ToolDef::function_name)
                .any(|name| name == "lsp_query")
        );
        assert!(
            !registry
                .readonly_tool_defs()
                .iter()
                .filter_map(grokforge_xai::ToolDef::function_name)
                .any(|name| name == "format_file")
        );
    }

    #[test]
    fn formatter_approval_uses_exact_argv_without_a_shell() {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("has spaces.rs");
        std::fs::write(&file, "fn main(){}").unwrap();
        let runner = Arc::new(RecordingRunner {
            output: ExecOutput {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                denial: None,
            },
            stdin: Mutex::new(None),
            formatted: None,
        });
        let ctx = context(workspace.path(), runner);
        let tool = FormatFile {
            config: Arc::new(Ok(test_config())),
        };
        let approval = tool.approval(&json!({"path": "has spaces.rs"}), &ctx);
        let ApprovalNeed::Gated(ApprovalKind::ExecCommand { command, .. }) = approval else {
            panic!("unexpected approval")
        };
        assert!(command[0].ends_with("/echo"));
        assert_eq!(command.len(), 2);
        assert!(command[1].contains("has spaces.rs"));
    }

    #[test]
    fn diagnostics_always_force_read_only_policy() {
        let workspace = tempfile::tempdir().unwrap();
        let ctx = context(
            workspace.path(),
            Arc::new(RecordingRunner {
                output: ExecOutput {
                    exit_code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                    truncated: false,
                    timed_out: false,
                    denial: None,
                },
                stdin: Mutex::new(None),
                formatted: None,
            }),
        );
        let policy = diagnostic_policy(&ctx);
        assert_eq!(policy.mode, SandboxMode::ReadOnly);
        assert!(policy.writable_roots.is_empty());
        assert_eq!(policy.network, NetworkMode::Isolated);

        let mut ctx = ctx;
        ctx.policy
            .unreadable_globs
            .push("**/owner-secret".to_string());
        assert!(
            diagnostic_policy(&ctx)
                .unreadable_globs
                .contains(&"**/owner-secret".to_string())
        );
    }

    #[test]
    fn formatter_policy_is_networkless_and_private_copy_scoped() {
        let workspace = tempfile::tempdir().unwrap();
        let nested = workspace.path().join("src");
        std::fs::create_dir(&nested).unwrap();
        let file = nested.join("main.rs");
        std::fs::write(&file, "fn main(){}").unwrap();
        let mut ctx = context(
            workspace.path(),
            Arc::new(RecordingRunner {
                output: ExecOutput {
                    exit_code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                    truncated: false,
                    timed_out: false,
                    denial: None,
                },
                stdin: Mutex::new(None),
                formatted: None,
            }),
        );
        ctx.policy = SandboxPolicy::danger_full_access(workspace.path());
        let scratch = tempfile::tempdir().unwrap();
        let canonical_scratch = std::fs::canonicalize(scratch.path()).unwrap();
        let policy = formatter_policy(
            &ctx,
            &std::fs::canonicalize(file).unwrap(),
            &canonical_scratch,
        );
        assert_eq!(policy.mode, SandboxMode::WorkspaceWrite);
        assert_eq!(policy.network, NetworkMode::Isolated);
        assert_eq!(policy.writable_roots, vec![canonical_scratch.clone()]);
        assert!(
            policy
                .readable_roots
                .iter()
                .any(|root| canonical_scratch.starts_with(root)),
            "private formatter copy must remain readable under every constrained backend"
        );
        assert!(
            policy
                .writable_roots
                .iter()
                .all(|root| !root.starts_with(workspace.path())),
            "formatter process must never receive a writable workspace path"
        );

        ctx.policy = SandboxPolicy::read_only(workspace.path());
        let denied = formatter_policy(
            &ctx,
            &std::fs::canonicalize(nested.join("main.rs")).unwrap(),
            &canonical_scratch,
        );
        assert_eq!(denied.mode, SandboxMode::ReadOnly);
        assert!(denied.writable_roots.is_empty());
    }
}
